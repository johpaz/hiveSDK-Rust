//! Embedded catalog services. Opt-in, tenant-scoped, and independent of HTTP.
//!
//! Catalog records describe capabilities; registering a tool or an MCP server
//! here does not install an executor or establish an external connection.
use crate::{Error, Result, sessions::SessionStore};
use hivedb_core::{HiveDB, PutOptions, ScanOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};

pub mod secrets;
pub mod skills;
mod tools;

const COLLECTION: &str = "hive_sdk_rust_catalog_v1";
const MAX_DOCUMENT_BYTES: usize = 262_144;
const MAX_PAGE: usize = 200;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    Agents,
    Skills,
    Tools,
    Memory,
    Providers,
    Models,
    Ethics,
    McpServers,
    Endpoints,
    Cron,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CatalogRecord {
    pub id: String,
    pub kind: EntityKind,
    pub revision: u64,
    pub data: Value,
}

#[derive(Debug, Serialize)]
pub struct CatalogPage {
    pub items: Vec<CatalogRecord>,
    pub next_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    ToolDefinitions,
    ExecuteTool {
        name: String,
        arguments: Value,
    },
    ImportSkill {
        path: String,
    },
    Create {
        kind: EntityKind,
        id: String,
        data: Value,
    },
    Get {
        kind: EntityKind,
        id: String,
    },
    List {
        kind: EntityKind,
        #[serde(default = "page_size")]
        limit: usize,
        cursor: Option<String>,
    },
    Update {
        kind: EntityKind,
        id: String,
        revision: u64,
        patch: Value,
    },
    Delete {
        kind: EntityKind,
        id: String,
        revision: u64,
    },
    SetEnabled {
        kind: EntityKind,
        id: String,
        revision: u64,
        enabled: bool,
    },
    Search {
        kind: EntityKind,
        query: String,
        #[serde(default = "page_size")]
        limit: usize,
    },
}

fn page_size() -> usize {
    50
}
fn invalid(message: &str) -> Error {
    Error::Input(message.into())
}
fn storage(error: impl std::fmt::Display) -> Error {
    Error::Storage(error.to_string())
}

fn identifier(value: &str) -> Result<()> {
    if value.trim().is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
        return Err(invalid(
            "catalog identifier must contain 1..512 bytes without control characters",
        ));
    }
    Ok(())
}

fn kind_key(kind: EntityKind) -> &'static str {
    match kind {
        EntityKind::Agents => "agents",
        EntityKind::Skills => "skills",
        EntityKind::Tools => "tools",
        EntityKind::Memory => "memory",
        EntityKind::Providers => "providers",
        EntityKind::Models => "models",
        EntityKind::Ethics => "ethics",
        EntityKind::McpServers => "mcp_servers",
        EntityKind::Endpoints => "endpoints",
        EntityKind::Cron => "cron",
    }
}

/// A scoped capability, not an authorization check for untrusted application code.
/// The host must authorize a tenant before handing its scoped store to a caller.
#[derive(Clone)]
pub struct CatalogServices {
    db: Arc<HiveDB>,
    tenant: String,
}

impl SessionStore {
    pub fn services(&self) -> CatalogServices {
        CatalogServices {
            db: self.db.clone(),
            tenant: self.tenant_id.clone(),
        }
    }
}

impl CatalogServices {
    pub fn from_db(db: Arc<HiveDB>, tenant: String) -> Result<Self> {
        identifier(&tenant)?;
        Ok(Self { db, tenant })
    }

    fn prefix(&self, kind: EntityKind) -> String {
        // JSON tuple encoding makes arbitrary delimiters and tenant names safe.
        let tuple = json!([self.tenant, kind_key(kind)]).to_string();
        format!("{},", &tuple[..tuple.len() - 1])
    }

    fn key(&self, kind: EntityKind, id: &str) -> Result<String> {
        identifier(id)?;
        Ok(json!([self.tenant, kind_key(kind), id]).to_string())
    }

    fn read(&self, kind: EntityKind, id: &str) -> Result<Option<(u64, Value)>> {
        let Some(row) = self
            .db
            .col_get(COLLECTION, &self.key(kind, id)?)
            .map_err(storage)?
        else {
            return Ok(None);
        };
        if row.doc["tenant_id"] != self.tenant
            || row.doc["kind"] != kind_key(kind)
            || row.doc["id"] != id
        {
            return Err(Error::State("catalog scope mismatch".into()));
        }
        Ok(Some((row.version, row.doc)))
    }

    pub fn get(&self, kind: EntityKind, id: &str) -> Result<Option<CatalogRecord>> {
        Ok(self
            .read(kind, id)?
            .filter(|(_, row)| row["deleted"] != true)
            .map(|(revision, row)| CatalogRecord {
                id: id.into(),
                kind,
                revision,
                data: row["data"].clone(),
            }))
    }

    pub fn create(&self, kind: EntityKind, id: &str, mut data: Value) -> Result<CatalogRecord> {
        identifier(id)?;
        // Defaults are applied before schema/reference validation, in Rust only.
        if let Some(object) = data.as_object_mut() {
            object.entry("enabled").or_insert(json!(true));
        }
        self.validate(kind, &data)?;
        let revision = match self.read(kind, id)? {
            Some((revision, row)) if row["deleted"] == true => revision,
            _ => 0,
        };
        self.write(kind, id, revision, data, false)
    }

    pub fn update(
        &self,
        kind: EntityKind,
        id: &str,
        revision: u64,
        patch: Value,
    ) -> Result<CatalogRecord> {
        let current = self
            .get(kind, id)?
            .ok_or_else(|| Error::State("CATALOG_NOT_FOUND".into()))?;
        if revision == 0 || revision != current.revision {
            return Err(Error::State("CATALOG_CONFLICT".into()));
        }
        let fields = patch
            .as_object()
            .ok_or_else(|| invalid("catalog patch must be an object"))?;
        let mut data = current.data;
        let object = data
            .as_object_mut()
            .ok_or_else(|| Error::State("invalid catalog record".into()))?;
        for (key, value) in fields {
            object.insert(key.clone(), value.clone());
        }
        self.validate(kind, &data)?;
        self.write(kind, id, revision, data, false)
    }

    pub fn delete(&self, kind: EntityKind, id: &str, revision: u64) -> Result<()> {
        let current = self
            .get(kind, id)?
            .ok_or_else(|| Error::State("CATALOG_NOT_FOUND".into()))?;
        if revision == 0 || revision != current.revision {
            return Err(Error::State("CATALOG_CONFLICT".into()));
        }
        // A versioned tombstone avoids an unchecked delete and ABA on recreation.
        self.write(kind, id, revision, json!({}), true)?;
        Ok(())
    }

    fn write(
        &self,
        kind: EntityKind,
        id: &str,
        revision: u64,
        data: Value,
        deleted: bool,
    ) -> Result<CatalogRecord> {
        let row = json!({"tenant_id":self.tenant,"kind":kind_key(kind),"id":id,"data":data,"deleted":deleted});
        let revision = self
            .db
            .col_put(
                COLLECTION,
                &self.key(kind, id)?,
                &row,
                PutOptions {
                    expected_version: Some(revision),
                },
            )
            .map_err(|error| {
                if error.to_string().contains("version conflict on") {
                    Error::State("CATALOG_CONFLICT".into())
                } else {
                    storage(error)
                }
            })?;
        Ok(CatalogRecord {
            id: id.into(),
            kind,
            revision,
            data,
        })
    }

    pub fn list(
        &self,
        kind: EntityKind,
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<CatalogPage> {
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err(invalid("catalog limit must be between 1 and 200"));
        }
        let prefix = self.prefix(kind);
        if cursor.is_some_and(|cursor| !cursor.starts_with(&prefix)) {
            return Err(invalid("cursor belongs to a different catalog scope"));
        }
        let mut start = cursor.map(str::to_owned);
        let mut items = Vec::new();
        // Walk bounded batches rather than loading all tenants into memory.
        loop {
            let rows = self
                .db
                .col_scan(
                    COLLECTION,
                    &ScanOptions {
                        prefix: Some(prefix.clone()),
                        start: start.clone(),
                        limit: MAX_PAGE,
                        ..Default::default()
                    },
                )
                .map_err(storage)?;
            if rows.is_empty() {
                break;
            }
            let last = rows.last().unwrap().id.clone();
            let full = rows.len() == MAX_PAGE;
            for row in rows {
                if start.as_ref() == Some(&row.id) || row.doc["deleted"] == true {
                    continue;
                }
                if row.doc["tenant_id"] != self.tenant || row.doc["kind"] != kind_key(kind) {
                    return Err(Error::State("catalog scope mismatch".into()));
                }
                if items.len() == limit {
                    return Ok(CatalogPage {
                        items,
                        next_cursor: start,
                    });
                }
                let id = row.doc["id"]
                    .as_str()
                    .ok_or_else(|| Error::State("invalid catalog id".into()))?;
                items.push(CatalogRecord {
                    id: id.into(),
                    kind,
                    revision: row.version,
                    data: row.doc["data"].clone(),
                });
                start = Some(row.id);
            }
            if !full {
                break;
            }
            start = Some(last);
        }
        Ok(CatalogPage {
            items,
            next_cursor: None,
        })
    }

    fn validate(&self, kind: EntityKind, data: &Value) -> Result<()> {
        if serde_json::to_vec(data)?.len() > MAX_DOCUMENT_BYTES {
            return Err(invalid("catalog document exceeds 256 KiB"));
        }
        let schema = schema(kind);
        if !crate::validation::compile(&schema)?.is_valid(data) {
            return Err(invalid(
                "catalog document does not match its schema; unknown fields and inline credentials are not allowed",
            ));
        }
        if let Some(reference) = data.get("credential_ref") {
            let reference: secrets::SecretReference = serde_json::from_value(reference.clone())
                .map_err(|_| invalid("invalid credential reference"))?;
            reference.validate()?;
        }
        if kind == EntityKind::Tools {
            crate::validation::compile(&data["parameters"])?;
        }
        match kind {
            EntityKind::Agents => {
                for (field, target) in [
                    ("tools", EntityKind::Tools),
                    ("skills", EntityKind::Skills),
                    ("mcp_servers", EntityKind::McpServers),
                ] {
                    if let Some(ids) = data[field].as_array() {
                        for id in ids {
                            self.reference(target, id.as_str().unwrap())?;
                        }
                    }
                }
                let provider = data["provider_id"].as_str();
                let model = data["model_id"].as_str();
                if provider.is_some() != model.is_some() {
                    return Err(invalid(
                        "agent provider_id and model_id must be set together",
                    ));
                }
                if let (Some(provider), Some(model)) = (provider, model) {
                    self.reference(EntityKind::Providers, provider)?;
                    let record = self.reference(EntityKind::Models, model)?;
                    if record.data["provider_id"] != provider {
                        return Err(invalid("agent model belongs to another provider"));
                    }
                }
            }
            EntityKind::Skills => {
                if let Some(tools) = data["tools"].as_array() {
                    for tool in tools {
                        self.reference(EntityKind::Tools, tool.as_str().unwrap())?;
                    }
                }
            }
            EntityKind::Models => {
                self.reference(EntityKind::Providers, data["provider_id"].as_str().unwrap())?;
            }
            EntityKind::Providers | EntityKind::Endpoints => {
                if let Some(url) = data["base_url"].as_str().or_else(|| data["url"].as_str()) {
                    validate_url(url)?;
                }
            }
            EntityKind::McpServers => {
                let transport = data["transport"].as_str().unwrap();
                if transport == "stdio" {
                    if data["command"].as_str().is_none_or(|s| s.trim().is_empty())
                        || data.get("url").is_some()
                    {
                        return Err(invalid("stdio requires command and forbids url"));
                    }
                } else {
                    validate_url(
                        data["url"]
                            .as_str()
                            .ok_or_else(|| invalid("MCP HTTP transport requires url"))?,
                    )?;
                    if data.get("command").is_some() || data.get("args").is_some() {
                        return Err(invalid("HTTP transport forbids command and args"));
                    }
                }
            }
            EntityKind::Cron => {
                self.reference(EntityKind::Agents, data["agent_id"].as_str().unwrap())?;
            }
            _ => {}
        }
        Ok(())
    }

    fn reference(&self, kind: EntityKind, id: &str) -> Result<CatalogRecord> {
        let record = self
            .get(kind, id)?
            .ok_or_else(|| invalid("catalog reference not found in tenant"))?;
        if record.data["enabled"] == false {
            return Err(invalid("catalog reference is disabled"));
        }
        Ok(record)
    }

    pub fn dispatch(&self, command: Value) -> Result<Value> {
        Ok(match serde_json::from_value::<Command>(command)? {
            Command::ToolDefinitions => serde_json::to_value(self.tool_definitions())?,
            Command::ExecuteTool { name, arguments } => self.execute_tool(&name, arguments)?,
            Command::ImportSkill { path } => {
                let skill = skills::load(std::path::Path::new(&path))?;
                serde_json::to_value(self.create(EntityKind::Skills, &skill.name, skill.data)?)?
            }
            Command::Create { kind, id, data } => {
                serde_json::to_value(self.create(kind, &id, data)?)?
            }
            Command::Get { kind, id } => serde_json::to_value(self.get(kind, &id)?)?,
            Command::List {
                kind,
                limit,
                cursor,
            } => serde_json::to_value(self.list(kind, limit, cursor.as_deref())?)?,
            Command::Update {
                kind,
                id,
                revision,
                patch,
            } => serde_json::to_value(self.update(kind, &id, revision, patch)?)?,
            Command::SetEnabled {
                kind,
                id,
                revision,
                enabled,
            } => serde_json::to_value(self.update(
                kind,
                &id,
                revision,
                json!({"enabled":enabled}),
            )?)?,
            Command::Delete { kind, id, revision } => {
                self.delete(kind, &id, revision)?;
                Value::Null
            }
            Command::Search { kind, query, limit } => {
                serde_json::to_value(self.search(kind, &query, limit)?)?
            }
        })
    }
}

pub fn validate_url(value: &str) -> Result<()> {
    let url = reqwest::Url::parse(value).map_err(|_| invalid("invalid service URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid(
            "service URLs require HTTP(S) without credentials, query, or fragment",
        ));
    }
    Ok(())
}

/// Shared by the legacy desktop adapter. Does not change its storage layout.
/// Authorization and field mapping remain the responsibility of that adapter.
pub fn patch_fields(
    db: &HiveDB,
    collection: &str,
    id: &str,
    body: &Value,
    mapping: &[(&str, &str)],
) -> Result<bool> {
    let Some(mut row) = db.col_get(collection, id).map_err(storage)? else {
        return Ok(false);
    };
    for (input, field) in mapping {
        if let Some(value) = body.get(*input) {
            row.doc[*field] = value.clone();
        }
    }
    db.col_put(
        collection,
        id,
        &row.doc,
        PutOptions {
            expected_version: Some(row.version),
        },
    )
    .map_err(storage)?;
    Ok(true)
}

pub fn schema(kind: EntityKind) -> Value {
    let text = json!({"type":"string","maxLength":65536});
    let name = json!({"type":"string","minLength":1,"maxLength":512,"pattern":"\\S"});
    let list = json!({"type":"array","maxItems":200,"uniqueItems":true,"items":{"type":"string","minLength":1,"maxLength":512}});
    let mut props = json!({"enabled":{"type":"boolean"}});
    let (required, fields) = match kind {
        EntityKind::Memory => (
            vec!["title", "content"],
            json!({"title":name,"content":text,"tags":list}),
        ),
        EntityKind::Agents => (
            vec!["name"],
            json!({"name":name,"description":text,"system_prompt":text,"role":{"enum":["coordinator","worker"]},"provider_id":name,"model_id":name,"tools":list,"skills":list,"mcp_servers":list,"max_iterations":{"type":"integer","minimum":1,"maximum":200}}),
        ),
        EntityKind::Skills => (
            vec!["name", "description", "content"],
            json!({"name":name,"description":text,"content":text,"category":name,"version":name,"tools":list,"triggers":list,"author":name,"icon":name,"permissions":list,"dependencies":list,"preferred_agents":list,"rules":{"type":"array","maxItems":200,"items":text},"steps":{"type":"array","maxItems":200,"items":{"type":"object","properties":{"step":{"type":"integer","minimum":1},"action":name,"instruction":text,"output":text,"params":{"type":"object"}},"required":["action","instruction"],"additionalProperties":false}},"output_format":{"type":"object"},"examples":{"type":"array","maxItems":100,"items":{"type":"object"}},"requirements":{"type":"object"},"metadata":{"type":"object"}}),
        ),
        EntityKind::Tools => (
            vec!["name", "description", "parameters"],
            json!({"name":name,"description":text,"category":name,"parameters":{"type":"object"}}),
        ),
        EntityKind::Providers => (
            vec!["name", "protocol", "base_url"],
            json!({"name":name,"protocol":{"enum":["openai_compatible","anthropic","gemini","hiveagents"]},"base_url":text}),
        ),
        EntityKind::Models => (
            vec!["name", "provider_id", "wire_id"],
            json!({"name":name,"provider_id":name,"wire_id":name,"context_window":{"type":"integer","minimum":1},"input_per_million":{"type":"number","minimum":0},"output_per_million":{"type":"number","minimum":0},"capabilities":list}),
        ),
        EntityKind::Ethics => (
            vec!["name", "content"],
            json!({"name":name,"content":text,"priority":{"type":"integer","minimum":0}}),
        ),
        EntityKind::McpServers => (
            vec!["name", "transport"],
            json!({"name":name,"transport":{"enum":["stdio","http"]},"url":text,"command":name,"args":{"type":"array","maxItems":200,"items":text}}),
        ),
        EntityKind::Endpoints => (
            vec!["name", "url", "method"],
            json!({"name":name,"url":text,"description":text,"method":{"enum":["GET","POST","PUT","PATCH","DELETE","HEAD","OPTIONS"]}}),
        ),
        EntityKind::Cron => (
            vec!["name", "agent_id", "schedule", "prompt"],
            json!({"name":name,"agent_id":name,"schedule":name,"timezone":name,"prompt":text}),
        ),
    };
    props
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    if matches!(
        kind,
        EntityKind::Providers | EntityKind::Endpoints | EntityKind::McpServers
    ) {
        props["credential_ref"] = json!({"type":"object","properties":{"vault_id":name,"name":name},
            "required":["vault_id","name"],"additionalProperties":false});
    }

    json!({"type":"object","properties":props,"required":required,"additionalProperties":false})
}

#[derive(Debug, Serialize)]
pub struct CatalogHit {
    pub record: CatalogRecord,
    pub score: f64,
}

fn terms(value: &str) -> Vec<String> {
    value
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect()
}

impl CatalogServices {
    /// BM25 over a tenant-local catalog. No remote models or hidden LLM calls.
    pub fn search(&self, kind: EntityKind, query: &str, limit: usize) -> Result<Vec<CatalogHit>> {
        if !(1..=MAX_PAGE).contains(&limit) || query.len() > 4096 {
            return Err(invalid("invalid search limit or query length"));
        }
        let mut query = terms(query);
        query.sort();
        query.dedup();
        if query.is_empty() {
            return Ok(vec![]);
        }
        if query.len() > 64 {
            return Err(invalid("catalog query exceeds 64 distinct terms"));
        }
        let query_index: HashMap<_, _> = query
            .iter()
            .enumerate()
            .map(|(index, term)| (term.as_str(), index))
            .collect();
        let mut frequencies = vec![0.0; query.len()];
        let mut records = Vec::new();
        let mut total_bytes = 0;
        let mut cursor = None;
        loop {
            let page = self.list(kind, MAX_PAGE, cursor.as_deref())?;
            for row in &page.items {
                total_bytes += serde_json::to_vec(&row.data)?.len();
            }
            if total_bytes > 16 * 1024 * 1024 {
                return Err(invalid("catalog search exceeds 16 MiB"));
            }
            records.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
            if records.len() > 10_000 {
                return Err(invalid("catalog search exceeds 10000 records"));
            }
        }
        if records.len() > 10_000 {
            return Err(invalid("catalog search exceeds 10000 records"));
        }
        let docs: Vec<_> = records
            .iter()
            .map(|record| {
                let text = ["name", "description", "content", "title", "triggers"]
                    .iter()
                    .filter_map(|field| {
                        let value = &record.data[*field];
                        value.as_str().map(str::to_owned).or_else(|| {
                            value.as_array().map(|items| {
                                items
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            })
                        })
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let terms = terms(&text);
                let mut counts = vec![0u32; query.len()];
                for term in &terms {
                    if let Some(index) = query_index.get(term.as_str()) {
                        counts[*index] += 1;
                    }
                }
                for (index, count) in counts.iter().enumerate() {
                    if *count > 0 {
                        frequencies[index] += 1.0;
                    }
                }
                (terms.len(), counts)
            })
            .collect();
        if docs.is_empty() {
            return Ok(vec![]);
        }
        let average = (docs.iter().map(|(length, _)| *length).sum::<usize>() as f64
            / docs.len() as f64)
            .max(1.0);
        let n = docs.len() as f64;
        let mut hits = Vec::new();
        for (record, doc) in records.into_iter().zip(docs) {
            if record.data["enabled"] == false {
                continue;
            }
            let mut score = 0.0;
            for (count, df) in doc.1.iter().zip(&frequencies) {
                let tf = *count as f64;
                let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
                score += idf * tf * 2.2 / (tf + 1.2 * (0.25 + 0.75 * doc.0 as f64 / average));
            }
            if score > 0.0 {
                hits.push(CatalogHit { record, score });
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.record.id.cmp(&b.record.id))
        });
        hits.truncate(limit);
        Ok(hits)
    }
}
