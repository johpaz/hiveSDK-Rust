use super::{CatalogServices, EntityKind as K};
use crate::{Error, Result, Tool};
use serde_json::{Value, json};

impl CatalogServices {
    /// Only tools with native executors are advertised here.
    pub fn tool_definitions(&self) -> Vec<Tool> {
        let title = json!({"type":"string","minLength":1,"maxLength":512,"pattern":"\\S"});
        let query = json!({"type":"string","maxLength":4096});
        let limit = json!({"type":"integer","minimum":1,"maximum":200});
        [
            ("memory_write", "Store or update a memory in the current tenant.", json!({"title":title,"content":{"type":"string","maxLength":65536}}), vec!["title","content"]),
            ("memory_read", "Read a tenant memory by title.", json!({"title":title}), vec!["title"]),
            ("memory_delete", "Delete a tenant memory by title.", json!({"title":title}), vec!["title"]),
            ("memory_list", "List tenant memories with an explicit continuation cursor.", json!({"limit":limit,"cursor":{"type":"string"}}), vec![]),
            ("memory_search", "Search tenant memories with local BM25.", json!({"query":query,"limit":limit}), vec!["query"]),
            ("skill_search", "Discover enabled skills with local BM25.", json!({"query":query,"limit":limit}), vec!["query"]),
            ("tool_search", "Discover enabled tool metadata; availability depends on the application's executors.", json!({"query":query,"limit":limit}), vec!["query"]),
            ("agent_find", "Find enabled agents by their name or description.", json!({"query":query,"limit":limit}), vec!["query"]),
        ].into_iter().map(|(name, description, properties, required)| Tool {
            name: name.into(), description: description.into(),
            parameters: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
            allow_repeated_results: false,
        }).collect()
    }

    /// Reuses the same services as application APIs. The Agent loop remains the
    /// authority for tool claims, budgets and uncertainty when used as a tool.
    pub fn execute_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        let tool = self
            .tool_definitions()
            .into_iter()
            .find(|tool| tool.name == name)
            .ok_or_else(|| Error::Input("unknown native catalog tool".into()))?;
        if !crate::validation::compile(&tool.parameters)?.is_valid(&arguments) {
            return Err(Error::Input(
                "native catalog tool arguments do not match schema".into(),
            ));
        }
        let title = arguments["title"].as_str().unwrap_or_default();
        let limit = arguments["limit"].as_u64().unwrap_or(50) as usize;
        Ok(match name {
            "memory_write" => {
                let data = json!({"title":title,"content":arguments["content"]});
                let record = match self.get(K::Memory, title)? {
                    Some(row) => self.update(K::Memory, title, row.revision, data)?,
                    None => self.create(K::Memory, title, data)?,
                };
                serde_json::to_value(record)?
            }
            "memory_read" => serde_json::to_value(self.get(K::Memory, title)?)?,
            "memory_delete" => {
                if let Some(row) = self.get(K::Memory, title)? {
                    self.delete(K::Memory, title, row.revision)?;
                    json!({"deleted":true})
                } else {
                    json!({"deleted":false})
                }
            }
            "memory_list" => {
                serde_json::to_value(self.list(K::Memory, limit, arguments["cursor"].as_str())?)?
            }
            _ => {
                let kind = match name {
                    "memory_search" => K::Memory,
                    "skill_search" => K::Skills,
                    "tool_search" => K::Tools,
                    "agent_find" => K::Agents,
                    _ => unreachable!(),
                };
                serde_json::to_value(self.search(
                    kind,
                    arguments["query"].as_str().unwrap(),
                    limit,
                )?)?
            }
        })
    }
}
