//! The MCP servers of HiveDB (`mcpServers` rows), connected through the SDK's `McpManager`.
//!
//! A row is `{id, name, enabled, transport, url, headers?}`. `sse` (the classic HTTP+SSE transport,
//! which is what n8n's MCP Server Trigger speaks) and `http` (Streamable HTTP) are connected; a
//! `stdio` row is reported as unsupported here on purpose, because connecting it would run a
//! command chosen by whoever can write the row. Their tools are given to the **coordinator** as
//! `mcp_<server>__<tool>` and every call goes through the same tool policy (and approvals) as any
//! other tool. A call whose outcome is unknown (the connection dropped while it ran) is reported as
//! such and never retried: it may have already sent the email.
use crate::config::{self, App};
use hive_agent_core::{Error, mcp::McpManager};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};

/// Wire names of MCP tools start with this, so they cannot collide with the gateway's own tools.
pub(crate) const TOOL_PREFIX: &str = "mcp_";
/// How often the rows are compared with what is connected, and how long a failed or closed server
/// waits before the next automatic reconnection (shortened by the tests).
static CYCLE_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(3_000);
static RETRY_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(30_000);
#[cfg(test)]
pub(crate) fn speed_up(cycle_ms: u64, retry_ms: u64) {
    CYCLE_MS.store(cycle_ms, std::sync::atomic::Ordering::SeqCst);
    RETRY_MS.store(retry_ms, std::sync::atomic::Ordering::SeqCst);
}
/// Most characters of a tool result handed to the model.
const MAX_RESULT_CHARS: usize = 64 * 1024;

/// `[A-Za-z0-9_-]{1,64}` without `__` (the manager's own rule for server names).
fn valid_server(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && !name.contains("__")
}

pub(crate) fn new_manager() -> Arc<McpManager> {
    Arc::new(McpManager::new())
}

/// The set to connect, and why some rows are not (`id → reason`).
fn desired(app: &App) -> (Value, HashMap<String, String>) {
    let mut set = serde_json::Map::new();
    let mut notes = HashMap::new();
    for row in config::docs(&app.db, "mcpServers").unwrap_or_default() {
        let Some(id) = row["id"].as_str() else {
            continue;
        };
        if !valid_server(id) {
            notes.insert(
                id.to_owned(),
                "el id debe tener 1 a 64 letras, dígitos, `_` o `-`, sin `__`".into(),
            );
            continue;
        }
        match row["transport"].as_str().unwrap_or_default() {
            transport @ ("sse" | "http") => {
                let Some(url) = row["url"].as_str().filter(|u| !u.is_empty()) else {
                    notes.insert(id.to_owned(), "falta la URL".into());
                    continue;
                };
                let headers: BTreeMap<String, String> = row["headers"]
                    .as_object()
                    .map(|h| {
                        h.iter()
                            .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                            .collect()
                    })
                    .unwrap_or_default();
                set.insert(
                    id.to_owned(),
                    json!({"transport":transport,"url":url,"headers":headers,"timeout_ms":20_000,
                        "enabled":row["enabled"]==true}),
                );
            }
            "stdio" => {
                notes.insert(
                    id.to_owned(),
                    "los servidores stdio no se ejecutan desde el gateway".into(),
                );
            }
            other => {
                notes.insert(id.to_owned(), format!("transporte no soportado: «{other}»"));
            }
        }
    }
    (Value::Object(set), notes)
}

/// Keeps the manager in step with the rows: applies a changed set, and tries a failed or closed server
/// again now and then (the manager itself never reconnects on its own).
pub(crate) async fn run(app: Arc<App>) {
    let mut applied = String::new();
    let mut attempts: HashMap<String, Instant> = HashMap::new();
    loop {
        let (set, notes) = desired(&app);
        if let Ok(mut all) = app.mcp_notes.lock() {
            *all = notes;
        }
        let key = config::App::hash_token(&set.to_string());
        if key != applied {
            let result =
                tokio::time::timeout(Duration::from_secs(90), app.mcp.apply(set.clone(), 2_000))
                    .await;
            // A set the manager refuses as a whole is retried when the rows change again.
            if matches!(result, Ok(Ok(_))) || result.is_err() {
                applied = key;
            } else {
                applied = key;
                if let Ok(mut all) = app.mcp_notes.lock() {
                    all.insert("*".into(), "no se pudo aplicar la configuración MCP".into());
                }
            }
            attempts.clear();
        }
        if let Some(rows) = app.mcp.status().as_array() {
            for row in rows {
                let (Some(name), Some(state)) = (row["name"].as_str(), row["state"].as_str())
                else {
                    continue;
                };
                if !matches!(state, "failed" | "closed") {
                    attempts.remove(name);
                    continue;
                }
                let retry =
                    Duration::from_millis(RETRY_MS.load(std::sync::atomic::Ordering::SeqCst));
                let due = attempts.get(name).is_none_or(|at| at.elapsed() >= retry);
                if due {
                    attempts.insert(name.to_owned(), Instant::now());
                    let _ = tokio::time::timeout(Duration::from_secs(60), app.mcp.reconnect(name))
                        .await;
                }
            }
        }
        tokio::select! {
            _ = app.shutdown.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_millis(CYCLE_MS.load(std::sync::atomic::Ordering::SeqCst))) => {}
        }
    }
    app.mcp.close_all().await;
}

/// The rows as `/api/mcp/servers` shows them, with the live state of each connection.
pub(crate) fn status_rows(app: &App) -> Vec<Value> {
    let live: HashMap<String, Value> = app
        .mcp
        .status()
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| Some((row["name"].as_str()?.to_owned(), row.clone())))
        .collect();
    let catalog = app.mcp.catalog();
    let notes = app.mcp_notes.lock().map(|n| n.clone()).unwrap_or_default();
    config::docs(&app.db, "mcpServers")
        .unwrap_or_default()
        .into_iter()
        .map(|row| {
            let id = row["id"].as_str().unwrap_or_default().to_owned();
            let enabled = row["enabled"] == true;
            let tools: Vec<String> = catalog
                .as_array()
                .into_iter()
                .flatten()
                .filter(|tool| tool["server"] == id.as_str())
                .filter_map(|tool| tool["tool"].as_str().map(str::to_owned))
                .collect();
            let (status, error) = if let Some(note) = notes.get(&id) {
                ("error", Some(note.clone()))
            } else {
                match live.get(&id).and_then(|row| row["state"].as_str()) {
                    Some("connected") => ("connected", None),
                    Some("failed" | "closed") => ("error", live[&id]["error"].as_str().map(str::to_owned).or_else(|| Some("desconectado".into()))),
                    Some(_) => ("disconnected", None),
                    None if enabled => ("connecting", None),
                    None => ("disconnected", None),
                }
            };
            json!({"id":row["id"],"name":row["name"],"enabled":enabled,"status":status,"error":error,
                "tools_count":tools.len(),"tools":tools,
                "config":{"transport":row["transport"],"url":row["url"].as_str().map(crate::integrations::safe_url)}})
        })
        .collect()
}

fn wire(server: &str, tool: &str) -> Option<String> {
    let name = format!("{TOOL_PREFIX}{server}__{tool}");
    (name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'))
    .then_some(name)
}

/// The tools of every connected server, for the coordinator. A tool whose name cannot be a safe wire
/// name, or that would repeat one already offered, is left out.
pub(crate) fn tool_specs(app: &App) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut specs = vec![];
    for tool in app.mcp.catalog().as_array().into_iter().flatten() {
        let (Some(server), Some(name)) = (tool["server"].as_str(), tool["tool"].as_str()) else {
            continue;
        };
        let Some(wire) = wire(server, name) else {
            continue;
        };
        if !seen.insert(wire.clone()) || !tool["inputSchema"].is_object() {
            continue;
        }
        let mut parameters = tool["inputSchema"].clone();
        // The engine compiles each schema itself; the draft marker adds nothing.
        parameters
            .as_object_mut()
            .map(|schema| schema.remove("$schema"));
        specs.push(json!({"name":wire,
            "description":format!("[Servidor MCP «{server}»] {}", tool["description"].as_str().unwrap_or_default()),
            "parameters":parameters}));
    }
    specs
}

/// `(server, tool)` of a wire name the coordinator was offered.
pub(crate) fn resolve(app: &App, wire_name: &str) -> Option<(String, String)> {
    app.mcp.catalog().as_array()?.iter().find_map(|tool| {
        let (server, name) = (tool["server"].as_str()?, tool["tool"].as_str()?);
        (wire(server, name).as_deref() == Some(wire_name))
            .then(|| (server.to_owned(), name.to_owned()))
    })
}

/// Run one call and say what happened in terms the model can act on. Never retried.
pub(crate) async fn execute(app: &Arc<App>, server: &str, tool: &str, arguments: Value) -> Value {
    let request =
        match app
            .mcp
            .start_call(server, tool, arguments, json!({"source":"hive-gateway"}))
        {
            Ok(request) => request,
            Err(error) => return json!({"ok":false,"error":error.to_string()}),
        };
    match request.result().await {
        Ok(value) => {
            let failed = value["isError"] == true;
            let mut text = serde_json::to_string(&value).unwrap_or_default();
            if text.chars().count() > MAX_RESULT_CHARS {
                text = text.chars().take(MAX_RESULT_CHARS).collect::<String>() + "…(recortado)";
                return json!({"ok":!failed,"result_text":text,"truncated":true});
            }
            json!({"ok":!failed,"result":value})
        }
        Err(Error::McpOperationUncertain) => json!({"ok":false,
            "error":"MCP_OPERATION_UNCERTAIN: se perdió la conexión con el servidor mientras la acción se ejecutaba y no se sabe si llegó a hacerse. No la repitas sin comprobarlo (por ejemplo, buscando el resultado) o sin que la persona lo confirme."}),
        Err(error) => json!({"ok":false,"error":error.to_string()}),
    }
}
