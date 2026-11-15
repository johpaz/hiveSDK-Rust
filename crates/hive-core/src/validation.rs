//! Offline JSON Schema compilation, shared by all language bindings.
use crate::{Error, Result};
use jsonschema::{Draft, Validator};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

const CACHE_CAPACITY: usize = 64;
const MAX_ISSUES: usize = 8;
type Cache = VecDeque<(String, Arc<Validator>)>;
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

fn bounded(message: impl AsRef<str>) -> String {
    message.as_ref().chars().take(512).collect()
}

pub fn compile(schema: &Value) -> Result<Arc<Validator>> {
    let key = serde_json::to_string(schema)?;
    let cache = CACHE.get_or_init(|| Mutex::new(VecDeque::new()));
    {
        let guard = cache
            .lock()
            .map_err(|_| Error::State("schema cache unavailable".into()))?;
        if let Some((_, validator)) = guard.iter().find(|(cached, _)| cached == &key) {
            return Ok(validator.clone());
        }
    }
    let mut options = jsonschema::options()
        .offline()
        .should_validate_formats(true);
    if schema.get("$schema").is_none() {
        options = options.with_draft(Draft::Draft202012);
    }
    let validator = Arc::new(
        options
            .build(schema)
            .map_err(|error| Error::InvalidSchema(bounded(error.masked().to_string())))?,
    );
    let mut guard = cache
        .lock()
        .map_err(|_| Error::State("schema cache unavailable".into()))?;
    if let Some((_, compiled)) = guard.iter().find(|(cached, _)| cached == &key) {
        return Ok(compiled.clone());
    }
    if guard.len() == CACHE_CAPACITY {
        guard.pop_front();
    }
    guard.push_back((key, validator.clone()));
    Ok(validator)
}

pub fn rejection(tool: &str, schema: &Value, arguments: &Value) -> Result<Option<Value>> {
    let validator = compile(schema)?;
    let mut issues: Vec<Value> = validator
        .iter_errors(arguments)
        .take(MAX_ISSUES + 1)
        .map(|error| {
            json!({
                "instance_path": error.instance_path().to_string(),
                "schema_path": error.schema_path().to_string(),
                "message": bounded(error.masked().to_string()),
            })
        })
        .collect();
    if issues.is_empty() {
        return Ok(None);
    }
    let truncated = issues.len() > MAX_ISSUES;
    issues.truncate(MAX_ISSUES);
    Ok(Some(json!({"error": {
        "code": "INVALID_TOOL_ARGUMENTS", "tool": tool,
        "message": "Arguments do not match the tool JSON Schema",
        "issues": issues, "truncated": truncated,
    }})))
}
