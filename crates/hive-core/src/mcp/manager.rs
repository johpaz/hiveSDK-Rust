//! Several MCP servers behind one handle: a named set that can be reloaded while the
//! application runs.
//!
//! `apply` takes the whole desired set and reconciles it: servers whose config is
//! unchanged and still connected are left alone, new or changed ones are connected (in
//! parallel), removed ones are closed, and the result is swapped in as one step so
//! readers never see a half-applied set. A bad config rejects the whole `apply` before
//! anything changes; a server that fails to connect is reported as `failed` and the
//! others are unaffected. Nothing reconnects on its own and nothing is replayed: a call
//! that ended uncertain closes its client (the existing single-request policy), the
//! server shows as `closed`, and it is brought back only by `reconnect` or the next
//! `apply`, which start a new session for *future* calls.
use super::{McpClient, McpRequest};
use crate::{Error, Result};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

/// Run futures concurrently and return their outputs in the order given.
async fn join_all<T: Send + 'static, F: Future<Output = T> + Send + 'static>(
    futures: impl IntoIterator<Item = F>,
) -> Vec<T> {
    let mut set = tokio::task::JoinSet::new();
    let mut count = 0;
    for (index, future) in futures.into_iter().enumerate() {
        set.spawn(async move { (index, future.await) });
        count += 1;
    }
    let mut out: Vec<Option<T>> = (0..count).map(|_| None).collect();
    while let Some(Ok((index, value))) = set.join_next().await {
        out[index] = Some(value);
    }
    out.into_iter().flatten().collect()
}

const MAX_SERVERS: usize = 64;

fn error(message: impl Into<String>) -> Error {
    Error::Mcp(message.into())
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.contains("__")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn fingerprint(config: &Value) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in config.to_string().bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// One configured server and how it is doing. Immutable once published.
struct Entry {
    config: Value,
    hash: String,
    enabled: bool,
    client: Option<Arc<McpClient>>,
    error: Option<String>,
}

#[derive(Default)]
pub struct McpManager {
    servers: RwLock<BTreeMap<String, Arc<Entry>>>,
    /// Serializes `apply`, `reconnect` and `close_all`; readers never wait on it.
    writer: tokio::sync::Mutex<()>,
}

/// What `apply` did, by server name.
#[derive(Default)]
struct Changes {
    added: Vec<String>,
    removed: Vec<String>,
    restarted: Vec<String>,
    unchanged: Vec<String>,
    disabled: Vec<String>,
    failed: BTreeMap<String, String>,
}
impl Changes {
    fn report(self) -> Value {
        json!({"added":self.added,"removed":self.removed,"restarted":self.restarted,"unchanged":self.unchanged,"disabled":self.disabled,"failed":self.failed})
    }
}

/// Wait for the call a client is running to finish, up to `drain`, then close it.
async fn retire(client: Arc<McpClient>, drain: Duration) {
    let deadline = Instant::now() + drain;
    while client.is_busy() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    client.close().await;
}

impl McpManager {
    pub fn new() -> Self {
        Self::default()
    }

    fn snapshot(&self) -> BTreeMap<String, Arc<Entry>> {
        self.servers
            .read()
            .map(|servers| servers.clone())
            .unwrap_or_default()
    }

    /// `{name: config}` where each config is what `McpClient::connect_json` takes, plus an
    /// optional `"enabled": false` to keep a server in the set without connecting it.
    fn parse(desired: &Value) -> Result<BTreeMap<String, (Value, bool)>> {
        let object = desired
            .as_object()
            .ok_or_else(|| error("MCP servers must be an object of name to config"))?;
        if object.len() > MAX_SERVERS {
            return Err(error(format!("at most {MAX_SERVERS} MCP servers")));
        }
        let mut out = BTreeMap::new();
        for (name, config) in object {
            if !valid_name(name) {
                return Err(error(format!(
                    "server name `{name}` must be 1..64 letters, digits, `_` or `-`, without `__`"
                )));
            }
            let mut config = config.clone();
            let map = config
                .as_object_mut()
                .ok_or_else(|| error(format!("config of `{name}` must be an object")))?;
            let enabled = match map.remove("enabled") {
                None => true,
                Some(Value::Bool(flag)) => flag,
                Some(_) => return Err(error(format!("`enabled` of `{name}` must be a boolean"))),
            };
            out.insert(name.clone(), (config, enabled));
        }
        Ok(out)
    }

    async fn connect(config: Value) -> std::result::Result<Arc<McpClient>, String> {
        McpClient::connect_json(&config.to_string())
            .await
            .map_err(|error| error.to_string())
    }

    /// Reconcile the running set with `desired`. Old clients are given `drain_ms` to
    /// finish the call they are running before they are closed (a call cut off then is
    /// uncertain, like any closed connection). Returns what changed.
    pub async fn apply(&self, desired: Value, drain_ms: u64) -> Result<Value> {
        if drain_ms > 600_000 {
            return Err(error("drain_ms is at most 600000"));
        }
        let wanted = Self::parse(&desired)?;
        let _writer = self.writer.lock().await;
        let old = self.snapshot();
        let mut changes = Changes::default();
        let mut next: BTreeMap<String, Arc<Entry>> = BTreeMap::new();
        let mut pending = vec![];
        for (name, (config, enabled)) in &wanted {
            let hash = fingerprint(config);
            match old.get(name) {
                Some(entry)
                    if entry.hash == hash
                        && entry.enabled == *enabled
                        && (!enabled
                            || entry
                                .client
                                .as_ref()
                                .is_some_and(|client| !client.is_closed())) =>
                {
                    next.insert(name.clone(), entry.clone());
                    changes.unchanged.push(name.clone());
                }
                _ if !enabled => {
                    next.insert(
                        name.clone(),
                        Arc::new(Entry {
                            config: config.clone(),
                            hash,
                            enabled: false,
                            client: None,
                            error: None,
                        }),
                    );
                    changes.disabled.push(name.clone());
                }
                previous => {
                    let known = previous
                        .is_some_and(|entry| entry.client.is_some() || entry.error.is_some());
                    pending.push((name.clone(), config.clone(), hash, known));
                }
            }
        }
        // Connect everything new or changed at once; one slow server does not hold the others up.
        let connected = join_all(
            pending
                .iter()
                .map(|(_, config, _, _)| Self::connect(config.clone())),
        )
        .await;
        for ((name, config, hash, known), outcome) in pending.into_iter().zip(connected) {
            let (client, failure) = match outcome {
                Ok(client) => (Some(client), None),
                Err(message) => {
                    changes.failed.insert(name.clone(), message.clone());
                    (None, Some(message))
                }
            };
            if client.is_some() {
                if known {
                    changes.restarted.push(name.clone())
                } else {
                    changes.added.push(name.clone())
                }
            }
            next.insert(
                name,
                Arc::new(Entry {
                    config,
                    hash,
                    enabled: true,
                    client,
                    error: failure,
                }),
            );
        }
        // Publish the new set in one step, then retire what it no longer uses.
        let kept: Vec<usize> = next
            .values()
            .map(|entry| Arc::as_ptr(entry) as usize)
            .collect();
        let retired: Vec<(String, Arc<McpClient>)> = old
            .iter()
            .filter(|(_, entry)| !kept.contains(&(Arc::as_ptr(entry) as usize)))
            .filter_map(|(name, entry)| entry.client.clone().map(|client| (name.clone(), client)))
            .collect();
        changes.removed = old
            .keys()
            .filter(|name| !wanted.contains_key(*name))
            .cloned()
            .collect();
        *self
            .servers
            .write()
            .map_err(|_| error("poisoned MCP manager"))? = next;
        let drain = Duration::from_millis(drain_ms);
        join_all(retired.into_iter().map(|(_, client)| retire(client, drain))).await;
        Ok(changes.report())
    }

    /// Start a new session for one server with its current config (for a `failed` or
    /// `closed` server). The old client, if any, is closed first.
    pub async fn reconnect(&self, name: &str) -> Result<Value> {
        let _writer = self.writer.lock().await;
        let entry = self
            .snapshot()
            .get(name)
            .cloned()
            .ok_or_else(|| error(format!("unknown MCP server `{name}`")))?;
        if !entry.enabled {
            return Err(error(format!("MCP server `{name}` is disabled")));
        }
        if let Some(client) = &entry.client {
            client.close().await;
        }
        let (client, failure) = match Self::connect(entry.config.clone()).await {
            Ok(client) => (Some(client), None),
            Err(message) => (None, Some(message)),
        };
        let fresh = Arc::new(Entry {
            config: entry.config.clone(),
            hash: entry.hash.clone(),
            enabled: true,
            client,
            error: failure.clone(),
        });
        self.servers
            .write()
            .map_err(|_| error("poisoned MCP manager"))?
            .insert(name.to_owned(), fresh);
        match failure {
            None => Ok(json!({"server":name,"connected":true})),
            Some(message) => Err(error(format!("could not reconnect `{name}`: {message}"))),
        }
    }

    /// List the tools of one connected server again (see `McpClient::refresh_tools`): `{server,
    /// added, removed, changed}`. The new catalog is what `catalog()` returns from now on.
    pub async fn refresh_tools(&self, name: &str) -> Result<Value> {
        let entry = self
            .snapshot()
            .get(name)
            .cloned()
            .ok_or_else(|| error(format!("unknown MCP server `{name}`")))?;
        let client = entry
            .client
            .as_ref()
            .filter(|client| entry.enabled && !client.is_closed())
            .ok_or_else(|| error(format!("MCP server `{name}` is not connected")))?;
        let mut diff = client.refresh_tools().await?;
        diff["server"] = json!(name);
        Ok(diff)
    }

    pub fn status(&self) -> Value {
        Value::Array(
            self.snapshot()
                .iter()
                .map(|(name, entry)| {
                    let (state, tools) = match (&entry.client, entry.enabled) {
                        (_, false) => ("disabled", 0),
                        (Some(client), _) if client.is_closed() => ("closed", 0),
                        (Some(client), _) => ("connected", client.tools().len()),
                        (None, _) => ("failed", 0),
                    };
                    let changed = entry.client.as_ref().is_some_and(|client| client.tools_changed());
                    json!({"name":name,"state":state,"tools":tools,"tools_changed":changed,"error":entry.error,"config_hash":entry.hash,
                        "server":entry.client.as_ref().map(|client| client.info()["serverInfo"].clone())})
                })
                .collect(),
        )
    }

    /// The tools of every connected server, as `{server, tool, name, description, inputSchema}`
    /// with `name` qualified as `<server>__<tool>`.
    pub fn catalog(&self) -> Value {
        let mut rows = vec![];
        for (server, entry) in self.snapshot() {
            let Some(client) = entry.client.as_ref().filter(|client| !client.is_closed()) else {
                continue;
            };
            for tool in client.tools().iter() {
                let mut row = serde_json::to_value(tool).unwrap_or(Value::Null);
                row["server"] = json!(server);
                row["tool"] = json!(tool.name);
                row["name"] = json!(format!("{server}__{}", tool.name));
                rows.push(row);
            }
        }
        Value::Array(rows)
    }

    /// Start a tool call on a named server. Fails at once, without sending anything, if the
    /// server is unknown, disabled, failed or closed.
    pub fn start_call(
        &self,
        server: &str,
        tool: &str,
        arguments: Value,
        context: Value,
    ) -> Result<McpRequest> {
        let entry =
            self.snapshot().get(server).cloned().ok_or_else(|| {
                error(format!("MCP_SERVER_UNAVAILABLE: unknown server `{server}`"))
            })?;
        let client = match (&entry.client, entry.enabled) {
            (_, false) => {
                return Err(error(format!(
                    "MCP_SERVER_UNAVAILABLE: `{server}` is disabled"
                )));
            }
            (None, _) => {
                return Err(error(format!(
                    "MCP_SERVER_UNAVAILABLE: `{server}` failed to connect ({}); reconnect it",
                    entry.error.as_deref().unwrap_or("unknown error")
                )));
            }
            (Some(client), _) if client.is_closed() => {
                return Err(error(format!(
                    "MCP_SERVER_UNAVAILABLE: `{server}` is closed; reconnect it"
                )));
            }
            (Some(client), _) => client.clone(),
        };
        client.start_call(tool.to_owned(), arguments, context)
    }

    /// Stop every client without waiting, for `Drop`-time cleanup.
    pub fn cancel_all(&self) {
        for entry in self.snapshot().values() {
            if let Some(client) = &entry.client {
                client.cancel();
            }
        }
    }

    /// Close every server and empty the set.
    pub async fn close_all(&self) {
        let _writer = self.writer.lock().await;
        let old = std::mem::take(&mut *self.servers.write().expect("MCP manager lock"));
        join_all(
            old.into_values()
                .filter_map(|entry| entry.client.clone())
                .map(|client| async move { client.close().await }),
        )
        .await;
    }
}
