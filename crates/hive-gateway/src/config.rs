//! Hive's collection contracts; no provider environment overrides or reseeding.
use crate::Result;
use hive_agent_core::{
    durable_handle::DurableEngineHandle,
    objective::ObjectivePolicy,
    provider::{CancellationToken, NativeProvider},
    sessions::SessionStore,
};
use hivedb_core::{HiveDB, ScanOptions};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use subtle::ConstantTimeEq;

pub struct App {
    pub piper: crate::piper::Runtime,
    pub live_sessions: Mutex<HashMap<String, CancellationToken>>,
    pub dashboard: crate::dashboard::Runtime,
    pub office_events: tokio::sync::broadcast::Sender<Value>,
    /// Configuration reads share it; every change to configuration, the panel, the scheduler and the dashboard
    /// takes it exclusively, so a read never sees a change half made.
    pub activity_lock: std::sync::RwLock<()>,
    pub db: Arc<HiveDB>,
    pub home: PathBuf,
    pub db_path: PathBuf,
    pub token: String,
    pub shutdown: CancellationToken,
    pub sessions: Mutex<HashMap<String, Arc<DurableEngineHandle>>>,
    pub active: Mutex<HashMap<String, CancellationToken>>,
    pub store: Arc<Mutex<Option<SessionStore>>>,
    /// Stores of the other users (the owner's is `store`), opened on first use.
    pub user_stores: Mutex<HashMap<String, Arc<Mutex<Option<SessionStore>>>>>,
    /// Tool calls waiting for a person to approve them, by approval id.
    pub approvals: Mutex<HashMap<String, PendingApproval>>,
    /// The approval a chat of a channel was asked, by `kind:peer`: that chat's next "sí" or "no"
    /// answers it (see `channels::ask_agent`).
    pub chat_approvals: Mutex<HashMap<String, String>>,
    /// The MCP servers of HiveDB, connected by `mcp_servers::run`.
    pub mcp: Arc<hive_agent_core::mcp::McpManager>,
    /// Why an `mcpServers` row is not connected (`id → reason`; `*` for the whole set).
    pub mcp_notes: Mutex<HashMap<String, String>>,
    /// Pictures a channel received for a conversation's next turn, by thread: set just before the
    /// turn starts and taken by it. Only one turn runs per thread, so there is no ambiguity.
    pub turn_attachments: Mutex<HashMap<String, Vec<Value>>>,
    /// Conversations whose documents a person approved sending to the OCR provider, until when.
    pub ocr_grants: Mutex<HashMap<String, std::time::Instant>>,
    /// The decision oracle for the current `oracle` settings: `(signature, oracle)`.
    pub oracle: Mutex<Option<(String, Arc<hive_agent_core::oracle::Oracle>)>>,
    /// The SDK desktop tools for the current `computerUse` settings: `(settings signature, tools)`.
    pub computer: Mutex<Option<(String, hive_agent_core::tools::NativeTools)>>,
    /// Conversations whose desktop control a person approved, until when (`user\0thread`).
    pub computer_grants: Mutex<HashMap<String, std::time::Instant>>,
    /// Live state of channel adapters, by channel id: `{status, error}`.
    pub channel_status: Mutex<HashMap<String, Value>>,
    /// Wakes the channel supervisor when a channel's configuration changes.
    pub channel_wakeup: tokio::sync::Notify,
    /// Writes to the tool audit log, to prune it now and then.
    pub audit_writes: std::sync::atomic::AtomicU64,
    /// Token buckets and the cached rate-limit configuration.
    pub limiter: Mutex<crate::ratelimit::Limiter>,
    pub rate_config: Mutex<Option<(std::time::Instant, crate::ratelimit::Config)>>,
}

pub struct PendingApproval {
    pub user: String,
    pub answer: tokio::sync::oneshot::Sender<bool>,
}

/// The administrator's tool policy, kept in HiveDB (`toolPolicy/default`).
pub struct StoredToolPolicy {
    pub policy: hive_agent_core::policy::ToolPolicy,
    /// How long a person has to answer an `ask` before it is denied.
    pub approval_timeout_ms: u64,
}
pub const DEFAULT_APPROVAL_TIMEOUT_MS: u64 = 120_000;
/// Newest tool-audit entries kept.
pub const AUDIT_KEEP: usize = 5_000;

/// One tool decision, as written to the audit log.
pub struct AuditEntry<'a> {
    pub user: &'a str,
    pub thread: &'a str,
    pub tool: &'a str,
    pub arguments: &'a Value,
    pub decision: &'a str,
    pub rule: Option<usize>,
    pub reason: &'a str,
    pub source: &'a str,
}

/// Who a request acts as. The shared `HIVE_AUTH_TOKEN` is the administrator (and the
/// first user, as before); a per-user token acts only as that user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    pub user: String,
    pub admin: bool,
}

pub struct Selection {
    pub provider_id: String,
    pub model_id: String,
    pub user: String,
    pub agent: Value,
    pub provider: NativeProvider,
    pub engine_config: String,
}

pub const MAX_OUTPUT_TOKENS: u64 = 32768;

pub fn home() -> Result<PathBuf> {
    let path = std::env::var_os("HIVE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".hive")))
        .ok_or("No se pudo resolver HIVE_HOME")?;
    Ok(fs::canonicalize(path)?)
}

/// The outcome of [`App::resolve`].
struct Resolved {
    user: String,
    agents: Vec<Value>,
    agent: Value,
    /// Provider id and model id.
    pair: (String, String),
    provider: Value,
    model: Value,
}

impl App {
    pub fn open(home: PathBuf) -> Result<Self> {
        let db_path = std::env::var_os("HIVE_DB_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("data/hivedb"));
        if !db_path.join("collections.redb").is_file() {
            return Err("No existe una HiveDB de Hive configurada en HIVE_HOME/data/hivedb".into());
        }
        let db_path = fs::canonicalize(db_path)?;
        let db = Arc::new(HiveDB::open(&db_path)?);
        crate::activity::reconcile(&db)?;
        let users = docs(&db, "users")?;
        let user = users
            .first()
            .ok_or("HiveDB no tiene usuarios; configura Hive primero")?;
        crate::realtime::ensure_default(&db)?;
        crate::model_catalog::ensure(&db)?;
        let token = std::env::var("HIVE_AUTH_TOKEN")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                fs::read_to_string(home.join(".auth_token"))
                    .ok()
                    .map(|s| s.trim().to_owned())
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let store = SessionStore::from_db(db.clone(), string(user, "id")?.to_owned())?;
        Ok(Self {
            piper: crate::piper::Runtime::default(),
            live_sessions: Mutex::new(HashMap::new()),
            dashboard: crate::dashboard::Runtime::default(),
            office_events: tokio::sync::broadcast::channel(256).0,
            activity_lock: std::sync::RwLock::new(()),
            db,
            home,
            db_path,
            token,
            shutdown: CancellationToken::new(),
            sessions: Mutex::new(HashMap::new()),
            active: Mutex::new(HashMap::new()),
            store: Arc::new(Mutex::new(Some(store))),
            user_stores: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            chat_approvals: Mutex::new(HashMap::new()),
            mcp: crate::mcp_servers::new_manager(),
            mcp_notes: Mutex::new(HashMap::new()),
            turn_attachments: Mutex::new(HashMap::new()),
            ocr_grants: Mutex::new(HashMap::new()),
            oracle: Mutex::new(None),
            computer: Mutex::new(None),
            computer_grants: Mutex::new(HashMap::new()),
            channel_status: Mutex::new(HashMap::new()),
            channel_wakeup: tokio::sync::Notify::new(),
            audit_writes: std::sync::atomic::AtomicU64::new(0),
            limiter: Mutex::new(Default::default()),
            rate_config: Mutex::new(None),
        })
    }

    pub fn has_credentials(&self) -> Result<bool> {
        Ok(docs(&self.db, "users")?.iter().any(|u| {
            u["email"].as_str().is_some_and(|s| !s.is_empty())
                && u["password_hash"].as_str().is_some_and(|s| !s.is_empty())
        }))
    }

    /// The configured rate limits, read from HiveDB at most every 3 seconds.
    fn rate_limits(&self) -> crate::ratelimit::Config {
        let now = std::time::Instant::now();
        if let Ok(cache) = self.rate_config.lock()
            && let Some((at, config)) = *cache
            && now.duration_since(at) < std::time::Duration::from_secs(3)
        {
            return config;
        }
        let config = self
            .db
            .col_get("rateLimits", "default")
            .ok()
            .flatten()
            .and_then(|row| crate::ratelimit::Config::from_json(&row.doc["limits"]).ok())
            .unwrap_or_default();
        if let Ok(mut cache) = self.rate_config.lock() {
            *cache = Some((now, config));
        }
        config
    }

    /// Forget the cached configuration, so a change applies on the next request.
    pub fn reload_rate_limits(&self) {
        if let Ok(mut cache) = self.rate_config.lock() {
            *cache = None;
        }
    }

    /// Count one event of `kind` for `key`. `Err(ms)` means it is over the limit and says how
    /// long to wait. With no limit configured for the kind, always `Ok`.
    pub fn limit(&self, kind: crate::ratelimit::Kind, key: &str) -> std::result::Result<(), u64> {
        let Some(per_minute) = self.rate_limits().limit(kind) else {
            return Ok(());
        };
        let key = format!("{kind:?}:{key}");
        self.limiter.lock().map_err(|_| 1_000u64)?.check(
            &key,
            per_minute,
            std::time::Instant::now(),
        )
    }

    /// Like [`Self::limit`] for `kind`, but only asks: nothing is taken. Used to refuse before
    /// doing the work a guesser is after, while a caller that succeeds costs nothing.
    pub fn limit_open(
        &self,
        kind: crate::ratelimit::Kind,
        key: &str,
    ) -> std::result::Result<(), u64> {
        let Some(per_minute) = self.rate_limits().limit(kind) else {
            return Ok(());
        };
        let key = format!("{kind:?}:{key}");
        self.limiter.lock().map_err(|_| 1_000u64)?.available(
            &key,
            per_minute,
            std::time::Instant::now(),
        )
    }

    /// The first user: the owner and the identity of the shared token.
    pub fn owner_id(&self) -> Result<String> {
        let users = docs(&self.db, "users")?;
        Ok(string(users.first().ok_or("HiveDB no tiene usuarios")?, "id")?.to_owned())
    }

    pub fn hash_token(token: &str) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(token.as_bytes()))
    }

    /// Resolve a bearer token. With no credentials configured the gateway is open
    /// and acts as the owner. Every user token is compared, in constant time and
    /// without stopping at the first match, so timing does not reveal which exists.
    pub fn authenticate(&self, token: Option<&str>) -> Result<Option<Principal>> {
        let owner = self.owner_id()?;
        if !self.has_credentials()? {
            return Ok(Some(Principal {
                user: owner,
                admin: true,
            }));
        }
        let Some(token) = token else { return Ok(None) };
        let mut found = None;
        let hashed = Self::hash_token(token);
        for user in docs(&self.db, "users")? {
            // The user's issued token and every session token a password login gave them.
            let sessions = user["session_token_hashes"]
                .as_array()
                .into_iter()
                .flatten();
            let stored = std::iter::once(&user["api_token_hash"]).chain(sessions);
            for stored in stored
                .filter_map(Value::as_str)
                .filter(|hash| !hash.is_empty())
            {
                if bool::from(hashed.as_bytes().ct_eq(stored.as_bytes())) {
                    found = user["id"].as_str().map(str::to_owned);
                }
            }
        }
        let global = bool::from(token.as_bytes().ct_eq(self.token.as_bytes()));
        Ok(match (global, found) {
            (true, _) => Some(Principal {
                user: owner,
                admin: true,
            }),
            (false, Some(user)) => Some(Principal { user, admin: false }),
            (false, None) => None,
        })
    }

    /// The policy that applies to a user: their own if the administrator set one (it replaces
    /// the global one for them), else the global one. The second value says which.
    pub fn tool_policy_for(&self, user: &str) -> Result<Option<(StoredToolPolicy, &'static str)>> {
        for (key, source) in [
            (format!("user:{user}"), "user"),
            ("default".to_owned(), "default"),
        ] {
            if let Some(row) = self.db.col_get("toolPolicy", &key)? {
                return Ok(Some((Self::stored_policy(&row.doc)?, source)));
            }
        }
        Ok(None)
    }

    fn stored_policy(doc: &Value) -> Result<StoredToolPolicy> {
        Ok(StoredToolPolicy {
            policy: hive_agent_core::policy::ToolPolicy::from_value(doc["policy"].clone())?,
            approval_timeout_ms: doc["approval_timeout_ms"]
                .as_u64()
                .unwrap_or(DEFAULT_APPROVAL_TIMEOUT_MS),
        })
    }

    /// Append a decision to the tool audit log. Arguments are recorded only as a hash, never
    /// in the clear. The log keeps the newest `AUDIT_KEEP` entries (checked every 200 writes).
    pub fn audit_tool(&self, entry: &AuditEntry) -> Result<()> {
        let AuditEntry {
            user,
            thread,
            tool,
            arguments,
            decision,
            rule,
            reason,
            source,
        } = entry;
        let now = crate::server::now();
        // Time, then a per-process sequence, so entries written in the same millisecond keep
        // their order; the random tail only keeps ids unique across processes.
        let sequence = self
            .audit_writes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let id = format!(
            "{now:020}-{:010}-{}",
            sequence,
            &uuid::Uuid::new_v4().simple().to_string()[..6]
        );
        let hash = Self::hash_token(&arguments.to_string());
        let reason: String = reason.chars().take(300).collect();
        self.db.col_put(
            "toolAudit",
            &id,
            &json!({"id":id,"at":now,"user":user,"thread":thread,"tool":tool,"decision":decision,
            "rule":rule,"reason":reason,"policy":source,"arguments_sha256":hash}),
            hivedb_core::PutOptions {
                expected_version: Some(0),
            },
        )?;
        if sequence % 200 == 199 {
            self.prune_audit(AUDIT_KEEP)?;
        }
        Ok(())
    }

    /// Delete all but the newest `keep` audit entries.
    pub fn prune_audit(&self, keep: usize) -> Result<usize> {
        let rows = self.db.col_scan(
            "toolAudit",
            &hivedb_core::ScanOptions {
                limit: keep + 5_000,
                ..Default::default()
            },
        )?;
        let excess = rows.len().saturating_sub(keep);
        for batch in rows[..excess].chunks(500) {
            let ops: Vec<_> = batch
                .iter()
                .map(|row| hivedb_core::ColOp::Delete {
                    collection: "toolAudit".into(),
                    id: row.id.clone(),
                })
                .collect();
            self.db.col_batch(&ops)?;
        }
        Ok(excess)
    }

    /// Newest-first audit entries, optionally for one user and only those before `before`.
    pub fn list_audit(
        &self,
        user: Option<&str>,
        limit: usize,
        before: Option<&str>,
    ) -> Result<Vec<Value>> {
        let rows = self.db.col_scan(
            "toolAudit",
            &hivedb_core::ScanOptions {
                reverse: true,
                limit: 5_000,
                ..Default::default()
            },
        )?;
        Ok(rows
            .into_iter()
            .filter(|row| before.is_none_or(|cursor| row.id.as_str() < cursor))
            .filter(|row| user.is_none_or(|user| row.doc["user"] == user))
            .take(limit)
            .map(|row| row.doc)
            .collect())
    }

    #[cfg(test)]
    pub fn tool_policy(&self) -> Result<Option<StoredToolPolicy>> {
        let Some(row) = self.db.col_get("toolPolicy", "default")? else {
            return Ok(None);
        };
        Ok(Some(StoredToolPolicy {
            policy: hive_agent_core::policy::ToolPolicy::from_value(row.doc["policy"].clone())?,
            approval_timeout_ms: row.doc["approval_timeout_ms"]
                .as_u64()
                .unwrap_or(DEFAULT_APPROVAL_TIMEOUT_MS),
        }))
    }

    /// Answer a pending approval. Only the user whose turn asked (or the administrator)
    /// can; anyone else gets `false`, indistinguishable from an unknown id.
    pub fn resolve_approval(
        &self,
        id: &str,
        principal: &Principal,
        approved: bool,
    ) -> Result<bool> {
        let mut pending = self
            .approvals
            .lock()
            .map_err(|_| "Aprobaciones bloqueadas")?;
        if !pending
            .get(id)
            .is_some_and(|entry| principal.admin || entry.user == principal.user)
        {
            return Ok(false);
        }
        let entry = pending.remove(id).expect("checked above");
        // The turn may have gone away already; that is not an error.
        let _ = entry.answer.send(approved);
        Ok(true)
    }

    /// The session store of a user: the owner's is `store`, others open on first use.
    pub fn store_for(&self, user: &str) -> Result<Arc<Mutex<Option<SessionStore>>>> {
        if user == self.owner_id()? {
            return Ok(self.store.clone());
        }
        if !docs(&self.db, "users")?.iter().any(|row| row["id"] == user) {
            return Err("Usuario desconocido".into());
        }
        let mut stores = self.user_stores.lock().map_err(|_| "Stores bloqueados")?;
        if let Some(slot) = stores.get(user) {
            return Ok(slot.clone());
        }
        let slot = Arc::new(Mutex::new(Some(SessionStore::from_db(
            self.db.clone(),
            user.to_owned(),
        )?)));
        stores.insert(user.to_owned(), slot.clone());
        Ok(slot)
    }

    /// A cloned handle to the user's store.
    pub fn store_handle(&self, user: &str) -> Result<SessionStore> {
        self.store_for(user)?
            .lock()
            .map_err(|_| "Store bloqueado")?
            .as_ref()
            .cloned()
            .ok_or_else(|| "Store cerrado".into())
    }

    pub fn selection(&self) -> Result<Selection> {
        self.select(None, None)
    }

    #[cfg(test)]
    pub fn selection_for_agent(&self, agent_id: &str) -> Result<Selection> {
        self.select(None, Some(agent_id))
    }

    /// The coordinator (or one of the user's workers) of a specific user.
    pub fn selection_for_user(&self, user: &str) -> Result<Selection> {
        self.select(Some(user), None)
    }

    pub fn selection_for_user_agent(&self, user: &str, agent_id: &str) -> Result<Selection> {
        self.select(Some(user), Some(agent_id))
    }

    pub fn delegation_enabled(&self) -> Result<bool> {
        Ok(self
            .db
            .col_get("tools", "task_delegate")?
            .is_none_or(|tool| tool.doc["enabled"] != false && tool.doc["active"] != false))
    }

    /// Who answers and with which model, checked against HiveDB: the user, the agent (a coordinator, or one of the
    /// user's workers), its provider and its model. Nothing is built from them here.
    fn resolve(&self, for_user: Option<&str>, requested: Option<&str>) -> Result<Resolved> {
        let users = docs(&self.db, "users")?;
        let user = match for_user {
            Some(user) => users
                .iter()
                .find(|row| row["id"] == user)
                .ok_or("Usuario desconocido")?,
            None => users.first().ok_or("HiveDB no tiene usuarios")?,
        };
        let user = string(user, "id")?.to_owned();
        let agents = docs(&self.db, "agents")?;
        let coordinator = agents
            .iter()
            .find(|a| a["role"] == "coordinator" && a["user_id"] == user)
            .ok_or("HiveDB no tiene un coordinador para el usuario")?;
        let agent = match requested {
            Some(id) => agents
                .iter()
                .find(|a| a["id"] == id && agent_visible(a, &user) && a["role"] == "worker")
                .ok_or("Especialista inexistente o no accesible")?,
            None => coordinator,
        }
        .clone();
        if agent["enabled"] != true || agent["status"] == "archived" {
            return Err("El agente está deshabilitado o archivado en HiveDB".into());
        }
        let assignment = if requested.is_some()
            && (reference(&agent, "provider_id").is_none()
                || reference(&agent, "model_id").is_none())
        {
            coordinator
        } else {
            &agent
        };
        let models = docs(&self.db, "models")?;
        let providers = docs(&self.db, "providers")?;
        let pair = match (
            reference(assignment, "provider_id"),
            reference(assignment, "model_id"),
        ) {
            (Some(p), Some(m)) => (p.to_owned(), m.to_owned()),
            _ => models
                .iter()
                .filter(|m| m["model_type"] == "llm" && m["active"] == true && m["enabled"] == true)
                .find_map(|m| {
                    providers
                        .iter()
                        .find(|p| {
                            p["id"] == m["provider_id"]
                                && p["active"] == true
                                && p["enabled"] == true
                        })
                        .map(|p| {
                            (
                                p["id"].as_str().unwrap_or_default().to_owned(),
                                m["id"].as_str().unwrap_or_default().to_owned(),
                            )
                        })
                })
                .ok_or("No hay un modelo LLM activo en HiveDB")?,
        };
        let provider = providers
            .iter()
            .find(|p| p["id"] == pair.0 && p["enabled"] == true && p["active"] == true)
            .ok_or("El proveedor del coordinador no está activo en HiveDB")?;
        // Like Hive's getDefaultLLM: an explicit coordinator assignment wins over
        // the active-model default. `active` is required only in automatic selection above.
        let model = models
            .iter()
            .find(|m| {
                m["id"] == pair.1
                    && m["provider_id"] == pair.0
                    && m["enabled"] == true
            })
            .ok_or_else(|| format!("Modelo del coordinador no disponible en HiveDB: proveedor={}, modelo={}; registro={}",
                pair.0, pair.1, models.iter().find(|m| m["id"] == pair.1).map(|m|
                    json!({"provider_id":m["provider_id"],"enabled":m["enabled"],"active":m["active"]}).to_string())
                    .unwrap_or_else(|| "ausente".into())))?;
        Ok(Resolved {
            user,
            agents,
            agent,
            pair,
            provider: provider.clone(),
            model: model.clone(),
        })
    }

    /// Whether a conversation could start: everything `select` checks about who answers and with which model,
    /// plus that the provider's address is valid and its key and headers can be read. It does not build the
    /// provider's HTTP client, the tool list or the engine configuration, which is most of what `select` costs.
    pub fn configured(&self) -> Result<()> {
        let resolved = self.resolve(None, None)?;
        provider_endpoint(&resolved.pair.0, resolved.provider["base_url"].as_str())?;
        self.provider_secrets(&resolved.pair.0)?;
        Ok(())
    }

    /// The provider's API key and extra headers, decrypted.
    fn provider_secrets(
        &self,
        provider_id: &str,
    ) -> Result<(Option<String>, HashMap<String, String>)> {
        let key = secret(
            &self.db,
            &self.home,
            &format!("provider:{provider_id}:api_key"),
        )?;
        let headers = secret(
            &self.db,
            &self.home,
            &format!("provider:{provider_id}:headers"),
        )?
        .map(|s| serde_json::from_str::<HashMap<String, String>>(&s))
        .transpose()?
        .unwrap_or_default();
        Ok((key, headers))
    }

    fn select(&self, for_user: Option<&str>, requested: Option<&str>) -> Result<Selection> {
        let Resolved {
            user,
            agents,
            agent,
            pair,
            provider,
            model,
        } = self.resolve(for_user, requested)?;
        let (provider, model) = (&provider, &model);
        let native = self.build_provider(&pair.0, provider, model)?;
        let workers: Vec<Value> = agents
            .iter()
            .filter(|a| enabled_worker(a, &user))
            .map(|a| json!({"id":a["id"],"name":a["name"],"description":a["description"]}))
            .collect();
        let mut system = agent["system_prompt"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let mut tools = Vec::new();
        let mut computer = false;
        if requested.is_none() && !workers.is_empty() && self.delegation_enabled()? {
            tools.push(json!({"name":"task_delegate","description":"Consulta de texto a un especialista habilitado. Espera su resultado antes de responder al usuario. No ejecuta herramientas externas.",
                "parameters":{"type":"object","additionalProperties":false,"required":["worker_id","task_description"],
                    "properties":{"worker_id":{"type":"string","enum":workers.iter().map(|w|w["id"].clone()).collect::<Vec<_>>()},
                        "task_description":{"type":"string","minLength":1,"maxLength":16000},"mode":{"type":"string","enum":["sync"]}}}}));
            if self
                .db
                .col_get("tools", "task_delegate_group")?
                .is_none_or(|row| row.doc["enabled"] != false && row.doc["active"] != false)
            {
                tools.push(json!({"name":"task_delegate_group","description":"Consulta en paralelo a especialistas habilitados. Reúne todos los resultados antes de continuar; conserva fallos y evidencia sin verificar.",
                    "parameters":{"type":"object","additionalProperties":false,"required":["tasks"],
                        "properties":{"tasks":{"type":"array","minItems":1,"maxItems":workers.len().min(64),"items":{
                            "type":"object","additionalProperties":false,"required":["task_id","worker_id","task_description"],
                            "properties":{"task_id":{"type":"string","minLength":1,"maxLength":512},
                                "worker_id":{"type":"string","enum":workers.iter().map(|w|w["id"].clone()).collect::<Vec<_>>()},
                                "task_description":{"type":"string","minLength":1,"maxLength":16000}}}}}}}));
            }
            system.push_str(&format!("\nEspecialistas disponibles en HiveDB: {}. Para consultarles usa task_delegate con worker_id y task_description. La delegación espera resultados durables. Usa task_delegate_group para consultas independientes en paralelo. Los especialistas responden consultas de texto; sin comprobaciones, sus resultados no están verificados.", serde_json::to_string(&workers)?));
        } else if requested.is_some() {
            system.push_str("\nResponde la consulta como especialista de texto. En esta integración no tienes herramientas externas. Explica los límites de lo que puedes comprobar.");
        }
        if requested.is_none() {
            for tool in crate::panel::specs() {
                let name = string(&tool, "name")?;
                if self
                    .db
                    .col_get("tools", name)?
                    .is_none_or(|row| row.doc["enabled"] != false && row.doc["active"] != false)
                {
                    tools.push(tool);
                }
            }
            let mcp_tools = crate::mcp_servers::tool_specs(self);
            if !mcp_tools.is_empty() {
                system.push_str(&format!("\nTienes {} herramientas de servidores MCP externos (prefijo mcp_). Úsalas cuando la petición lo requiera. Las que envían, crean, modifican o cancelan algo actúan sobre cuentas reales: antes de usarlas confirma con la persona los datos importantes (destinatario, fechas, contenido) y no repitas una acción cuyo resultado sea incierto.", mcp_tools.len()));
                tools.extend(mcp_tools);
            }
            if let Some(offer) = crate::ocr::offer(self) {
                system.push_str(&offer.guidance);
                tools.push(offer.tool);
            }
            if let Some(offer) = crate::computer::offer(self, model)? {
                system.push_str(&offer.guidance);
                tools.extend(offer.tools);
                computer = true;
            }
            system.push_str("\nPuedes crear formularios y tarjetas en el Panel interactivo con las herramientas a2ui. Crea la superficie, envía componentes completos con raíz root y datos enlazados. Las acciones de los botones llegarán como un nuevo turno. No afirmes que una acción externa se ejecutó por mostrar un panel.");
        }
        let policy = turn_policy(&agent, workers.len())?;
        let mut engine_policy = json!({"max_output_tokens":output_cap(provider, model),"max_tool_calls":policy.max_tool_calls});
        let mut max_turns = agent_iterations(&agent)?;
        if computer {
            // Each action is a tool call and a model call; screenshots reach the model as images.
            let extra = self.computer_extra_calls();
            engine_policy["max_tool_calls"] = json!(policy.max_tool_calls.saturating_add(extra));
            engine_policy["image_tools"] = json!(["computer_screenshot"]);
            engine_policy["max_tool_images"] = json!(2);
            max_turns = max_turns.saturating_add(extra);
        }
        let engine_config =
            json!({"op":"create", "model":wire_model(&pair.0, string(model, "id")?),
            "system":system, "tools":tools, "max_turns":max_turns,
            "policy":engine_policy})
            .to_string();
        Ok(Selection {
            provider_id: pair.0,
            model_id: pair.1,
            user,
            agent,
            provider: native,
            engine_config,
        })
    }

    /// The SDK provider for a HiveDB provider row and one of its model rows: endpoint, key and
    /// headers from the secrets, and the model's context window.
    pub(crate) fn build_provider(
        &self,
        provider_id: &str,
        provider: &Value,
        model: &Value,
    ) -> Result<NativeProvider> {
        let mut base = provider_endpoint(provider_id, provider["base_url"].as_str())?;
        if provider_id == "ollama" {
            base = ollama_api(&base);
        }
        let kind = match provider_id {
            "anthropic" => "anthropic",
            "gemini" => "gemini",
            "hiveagents" => "hiveagents",
            // Ollama's own API, not its OpenAI-compatible one: it is the one that takes the context size.
            "ollama" => "ollama",
            _ => "openai_compatible",
        };
        let (key, mut headers) = self.provider_secrets(provider_id)?;
        // Hive's Anthropic adapter uses API-key authentication rather than OAuth bearer tokens.
        let transport_key = if kind == "anthropic" {
            if let Some(key) = &key {
                headers.insert("x-api-key".into(), key.clone());
            }
            None
        } else {
            key
        };
        // Match Hive's agent-loop LLM_CALL_TIMEOUT_MS: every model operation
        // gets three minutes, including managed backend preparation.
        let mut transport = json!({"kind":kind, "base_url":base, "api_key":transport_key,
            "timeout_ms":180000});
        if kind == "hiveagents" {
            transport["context_window"] = json!(hiveagents_context(provider, model));
        } else if let Some(context) = model_context(provider, model) {
            // The model's own context window from HiveDB reaches the provider, which derives its
            // output limit from it whenever a request names none.
            transport["context_window"] = json!(context);
        }
        if kind == "ollama" {
            // Without num_ctx Ollama silently cuts the prompt to its default window.
            if let Some(context) = ollama_num_ctx(provider, model) {
                transport["num_ctx"] = json!(context);
            }
            // Local Ollama needs no key, and a null one means "none".
            if transport["api_key"].is_null() {
                transport.as_object_mut().map(|t| t.remove("api_key"));
            }
        }
        if kind == "openai_compatible" {
            transport["output_token_parameter"] = json!(if provider_id == "openai" {
                "max_completion_tokens"
            } else {
                "max_tokens"
            });
        }
        Ok(NativeProvider::from_json(&transport.to_string())?.with_headers(headers)?)
    }

    pub fn session(&self, thread: &str, selection: &Selection) -> Result<Arc<DurableEngineHandle>> {
        let row = self
            .db
            .col_get("conversationThreads", thread)?
            .ok_or("Conversación inexistente")?;
        if row.doc["user_id"] != selection.user || row.doc["archived"] == true {
            return Err("Conversación no accesible".into());
        }
        let mut sessions = self.sessions.lock().map_err(|_| "Sesiones bloqueadas")?;
        if let Some(handle) = sessions.get(thread) {
            return Ok(handle.clone());
        }
        let mut initial: Value = serde_json::from_str(&selection.engine_config)?;
        let mut history = crate::conversations::visible_messages(self, thread, 40)?;
        let mut bytes: usize = history
            .iter()
            .filter_map(|row| row["content"].as_str())
            .map(str::len)
            .sum();
        while bytes > 128 * 1024 && !history.is_empty() {
            bytes = bytes.saturating_sub(history.remove(0)["content"].as_str().map_or(0, str::len));
        }
        initial["initial_history"] = json!(
            history
                .iter()
                .filter_map(|row| Some(
                    json!({"role":row["role"],"content":row["content"].as_str()?})
                ))
                .collect::<Vec<_>>()
        );
        let handle = Arc::new(DurableEngineHandle::open(
            self.store_for(&selection.user)?,
            string(&selection.agent, "id")?.to_owned(),
            thread.to_owned(),
            &initial.to_string(),
            false,
        )?);
        sessions.insert(thread.to_owned(), handle.clone());
        Ok(handle)
    }
}

/// Hive seeds catalog workers with an empty owner; named owners remain isolated.
pub fn agent_visible(agent: &Value, user: &str) -> bool {
    agent["user_id"] == user || (agent["role"] == "worker" && agent["user_id"] == "")
}

pub fn enabled_worker(agent: &Value, user: &str) -> bool {
    agent_visible(agent, user)
        && agent["role"] == "worker"
        && agent["enabled"] == true
        && agent["status"] != "archived"
}

fn agent_iterations(agent: &Value) -> Result<u32> {
    match agent.get("max_iterations").filter(|value| !value.is_null()) {
        None => Ok(10), // Hive's agent-loop default.
        Some(value) => value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                "max_iterations debe ser un entero positivo de 32 bits en HiveDB".into()
            }),
    }
}

pub fn turn_policy(agent: &Value, worker_count: usize) -> Result<ObjectivePolicy> {
    let iterations = agent_iterations(agent)?;
    // A model iteration may delegate to the entire visible roster in one batch.
    // Workers currently have no tools, so each consultation needs one model call.
    let slots = u32::try_from(worker_count.max(1))?;
    let tools = iterations
        .checked_mul(slots)
        .ok_or("El presupuesto de delegación excede 32 bits")?;
    let models = iterations
        .checked_add(tools)
        .ok_or("El presupuesto de modelos excede 32 bits")?;
    // Each queued consultation charges both the parent tool and its worker claim.
    let tools = if worker_count > 0 {
        tools
            .checked_mul(2)
            .ok_or("El presupuesto de trabajos excede 32 bits")?
    } else {
        tools
    };
    Ok(ObjectivePolicy {
        max_model_calls: models,
        max_tool_calls: tools,
        max_depth: 1,
        max_output_tokens: MAX_OUTPUT_TOKENS,
        max_total_output_tokens: MAX_OUTPUT_TOKENS * u64::from(models),
        max_run_ms: None,
    })
}

/// A turn that may control the desktop needs a tool call and a model call per action on top of its usual budget.
pub fn with_computer_budget(mut policy: ObjectivePolicy, extra: u32) -> ObjectivePolicy {
    policy.max_tool_calls = policy.max_tool_calls.saturating_add(extra);
    policy.max_model_calls = policy.max_model_calls.saturating_add(extra);
    policy.max_total_output_tokens =
        MAX_OUTPUT_TOKENS.saturating_mul(u64::from(policy.max_model_calls));
    policy
}

fn hiveagents_context(provider: &Value, model: &Value) -> u64 {
    model_context(provider, model).unwrap_or(50_000)
}

fn output_cap(provider: &Value, model: &Value) -> u64 {
    // Hive's resolveMaxTokens reserves 15% of the effective HiveDB context.
    // Missing context uses an explicit bounded fallback rather than guessing a model.
    model_context(provider, model)
        .map(|context| (context.saturating_mul(15) / 100).clamp(1, MAX_OUTPUT_TOKENS))
        .unwrap_or(4096)
}

/// The largest `num_ctx` asked of Ollama on the strength of a model's context window alone. A model
/// may support 262144 tokens, but asking for all of it reserves memory for a cache that size, which
/// a local machine often does not have; an operator who wants more sets `num_ctx` on the provider.
const OLLAMA_DEFAULT_CTX_CEILING: u64 = 32_768;

/// The window to ask Ollama for: the provider's own `num_ctx` (an explicit operator choice, never
/// above what the model supports), else the model's context window from HiveDB up to a ceiling.
fn ollama_num_ctx(provider: &Value, model: &Value) -> Option<u64> {
    let explicit = provider["num_ctx"].as_u64().filter(|n| *n > 0);
    let supported = model["context_window"].as_u64().filter(|n| *n > 0);
    match (explicit, supported) {
        (Some(wanted), Some(max)) => Some(wanted.min(max)),
        (Some(wanted), None) => Some(wanted),
        (None, Some(max)) => Some(max.min(OLLAMA_DEFAULT_CTX_CEILING)),
        (None, None) => None,
    }
}

fn model_context(provider: &Value, model: &Value) -> Option<u64> {
    match (
        provider["num_ctx"].as_u64().filter(|n| *n > 0),
        model["context_window"].as_u64().filter(|n| *n > 0),
    ) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(n), None) | (None, Some(n)) => Some(n),
        (None, None) => None,
    }
}

pub fn docs(db: &HiveDB, collection: &str) -> Result<Vec<Value>> {
    Ok(db
        .col_scan(collection, &ScanOptions::default())?
        .into_iter()
        .map(|e| e.doc)
        .collect())
}
pub fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("Falta {field} en HiveDB").into())
}
fn reference<'a>(v: &'a Value, field: &str) -> Option<&'a str> {
    v[field]
        .as_str()
        .filter(|s| !s.is_empty() && *s != "__none__")
}
pub fn wire_model<'a>(provider: &str, model: &'a str) -> &'a str {
    if ["nvidia", "openrouter", "opencode-go", "groq", "modelscope"].contains(&provider) {
        model.strip_prefix(&format!("{provider}/")).unwrap_or(model)
    } else {
        model
    }
}

/// Hive's Gemini adapter defaults its null base URL; native transports need a
/// versioned endpoint. Anthropic's catalog stores the API root without /v1.
/// `http://host:11434`, `.../v1` and `.../api` all name the same server; the native API lives under `/api`.
fn ollama_api(base: &str) -> String {
    let base = base.trim_end_matches('/');
    let base = base
        .strip_suffix("/v1")
        .or_else(|| base.strip_suffix("/api"))
        .unwrap_or(base);
    format!("{}/api", base.trim_end_matches('/'))
}

pub fn provider_endpoint(provider: &str, base: Option<&str>) -> Result<String> {
    let base = base.filter(|s| !s.trim().is_empty()).map(str::trim);
    let (base, version) = match provider {
        "gemini" => (
            base.unwrap_or("https://generativelanguage.googleapis.com"),
            Some("v1beta"),
        ),
        "anthropic" => (base.unwrap_or("https://api.anthropic.com"), Some("v1")),
        _ => (
            base.ok_or("El proveedor necesita base_url en HiveDB")?,
            None,
        ),
    };
    let base = base.trim_end_matches('/');
    Ok(match version {
        Some(version) if !base.ends_with("/v1") && !base.ends_with("/v1beta") => {
            format!("{base}/{version}")
        }
        _ => base.to_owned(),
    })
}

/// The profile's master key. Finding it can mean a round trip to the system keyring (D-Bus and a key
/// exchange: milliseconds), and every secret read needs it, so a key that was found is kept for the life of the
/// process, per profile. A key that was not found is looked for again next time.
fn master_key(home: &Path) -> Result<Option<Vec<u8>>> {
    if let Ok(key) = std::env::var("HIVE_MASTER_KEY")
        && !key.is_empty()
    {
        // crypto.ts uses UTF-8 of slice(0,32).padEnd(32,"0"), not hexadecimal.
        return Ok(Some(environment_master_key(&key)?));
    }
    static KEYS: std::sync::OnceLock<Mutex<HashMap<PathBuf, Vec<u8>>>> = std::sync::OnceLock::new();
    let cache = KEYS.get_or_init(Default::default);
    let profile = fs::canonicalize(home)?;
    if let Some(key) = cache
        .lock()
        .ok()
        .and_then(|keys| keys.get(&profile).cloned())
    {
        return Ok(Some(key));
    }
    let found = read_master_key(home)?;
    if let (Some(key), Ok(mut keys)) = (&found, cache.lock()) {
        keys.insert(profile, key.clone());
    }
    Ok(found)
}

fn read_master_key(home: &Path) -> Result<Option<Vec<u8>>> {
    let digest = hex::encode(Sha256::digest(
        fs::canonicalize(home)?.to_string_lossy().as_bytes(),
    ));
    let name = format!("master-key:{}", &digest[..16]);
    #[cfg(target_os = "linux")]
    if let Some(key) = bun_keychain_key(&name)? {
        return Ok(Some(key));
    }
    if let Ok(entry) = keyring::Entry::new("hive", &name)
        && let Ok(value) = entry.get_password()
    {
        let key = hex::decode(value.trim()).map_err(|_| "Clave maestra inválida en llavero")?;
        if key.len() != 32 {
            return Err("Clave maestra inválida en llavero".into());
        }
        return Ok(Some(key));
    }
    match fs::read_to_string(home.join(".master.key")) {
        Ok(value) => {
            let key = hex::decode(value.trim()).map_err(|_| "Archivo .master.key inválido")?;
            if key.len() != 32 {
                return Err("Archivo .master.key inválido".into());
            }
            Ok(Some(key))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn environment_master_key(value: &str) -> Result<Vec<u8>> {
    let units: Vec<u16> = value.encode_utf16().take(32).collect();
    let mut text = String::from_utf16_lossy(&units);
    text.extend(std::iter::repeat_n('0', 32 - units.len()));
    let bytes = text.into_bytes();
    if bytes.len() != 32 {
        return Err("HIVE_MASTER_KEY no produce una clave AES de 32 bytes según crypto.ts".into());
    }
    Ok(bytes)
}

#[cfg(target_os = "linux")]
fn bun_keychain_key(name: &str) -> Result<Option<Vec<u8>>> {
    use dbus_secret_service::{EncryptionType, SecretService};
    // Search the exact service/account identity without rust-keyring's extra target attribute.
    // Different libsecret clients use username, account or name for the account field.
    let Ok(service) = SecretService::connect_with_max_prompt_timeout(EncryptionType::Dh, 10) else {
        return Ok(None);
    };
    let found = match service.search_items(HashMap::from([("service", "hive")])) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let mut keys = Vec::new();
    for item in found.unlocked.into_iter().chain(found.locked) {
        let attributes = item.get_attributes()?;
        if !["username", "account", "name"]
            .iter()
            .any(|field| attributes.get(*field).is_some_and(|v| v == name))
        {
            continue;
        }
        item.unlock()?;
        let key = hex::decode(String::from_utf8(item.get_secret()?)?.trim())
            .map_err(|_| "Clave maestra inválida en llavero de Hive")?;
        if key.len() != 32 {
            return Err("Clave maestra inválida en llavero de Hive".into());
        }
        if !keys.contains(&key) {
            keys.push(key);
        }
    }
    if keys.len() > 1 {
        return Err("Hay claves maestras distintas para este perfil en el llavero; resuelve el conflicto antes de iniciar".into());
    }
    Ok(keys.pop())
}

pub fn decrypt(key: &[u8], ciphertext: &str, iv: &str) -> Result<String> {
    use hive_agent_core::services::secrets::{SealedSecret, open_bytes};
    let (data, tag) = ciphertext
        .split_once(':')
        .ok_or("Secreto cifrado inválido")?;
    let plain = open_bytes(
        key,
        &[],
        &SealedSecret {
            nonce: iv.into(),
            ciphertext: format!("{data}{tag}"),
        },
    )?;
    Ok(std::str::from_utf8(&plain)
        .map_err(|_| "Secreto no UTF-8")?
        .to_owned())
}

pub(crate) fn secret(db: &HiveDB, home: &Path, name: &str) -> Result<Option<String>> {
    let Some(row) = db.col_get("secrets", name)? else {
        return Ok(None);
    };
    let key =
        master_key(home)?.ok_or("No se encontró la clave maestra para los secretos de HiveDB")?;
    Ok(Some(decrypt(
        &key,
        string(&row.doc, "ciphertext")?,
        string(&row.doc, "iv")?,
    ).map_err(|error| format!("No se pudo descifrar {name} en HiveDB: {error}; verifica la clave maestra y prepara la copia desde una sesión con acceso al llavero de Hive"))?))
}

pub(crate) fn store_secret(app: &App, name: &str, value: &str) -> Result<()> {
    let key = master_key(&app.home)?.ok_or("No se encontró la clave maestra del perfil")?;
    let sealed = hive_agent_core::services::secrets::seal_bytes(&key, &[], value.as_bytes())?;
    let split = sealed
        .ciphertext
        .len()
        .checked_sub(32)
        .ok_or("Secreto cifrado inválido")?;
    let ciphertext = format!(
        "{}:{}",
        &sealed.ciphertext[..split],
        &sealed.ciphertext[split..]
    );
    app.db.col_put(
        "secrets",
        name,
        &json!({"ciphertext":ciphertext,"iv":sealed.nonce}),
        Default::default(),
    )?;
    Ok(())
}

/// Linux password verification uses the same bcrypt / Argon2 formats as Bun.
/// Libraries are loaded only for login; a missing backend fails closed.
pub fn verify_password(password: &str, hash: &str) -> Result<bool> {
    use std::ffi::{CStr, CString, c_char, c_int, c_void};
    if password.len() > 4096 || hash.len() > 1024 {
        return Ok(false);
    }
    let password_c = CString::new(password)?;
    let hash_c = CString::new(hash)?;
    if hash.starts_with("$argon2") {
        let kind = if hash.starts_with("$argon2id$") {
            2
        } else if hash.starts_with("$argon2i$") {
            1
        } else {
            return Ok(false);
        };
        // SAFETY: fixed system ABI, valid C strings and bounded password slice; library outlives call.
        unsafe {
            let lib = libloading::Library::new("libargon2.so.1")
                .map_err(|_| "Login requiere libargon2.so.1")?;
            let verify: libloading::Symbol<
                unsafe extern "C" fn(*const c_char, *const c_void, usize, c_int) -> c_int,
            > = lib.get(b"argon2_verify")?;
            return Ok(verify(
                hash_c.as_ptr(),
                password.as_ptr().cast(),
                password.len(),
                kind,
            ) == 0);
        }
    }
    if !hash.starts_with("$2") {
        return Err("Formato de contraseña no soportado; se requiere bcrypt o Argon2".into());
    }
    static CRYPT: Mutex<()> = Mutex::new(());
    let _guard = CRYPT.lock().map_err(|_| "Login bloqueado")?;
    // SAFETY: crypt's static output is read while holding the global mutex; inputs are C strings.
    unsafe {
        let lib = libloading::Library::new("libcrypt.so.2")
            .or_else(|_| libloading::Library::new("libcrypt.so.1"))
            .map_err(|_| "Login requiere libcrypt (bcrypt)")?;
        let crypt: libloading::Symbol<
            unsafe extern "C" fn(*const c_char, *const c_char) -> *const c_char,
        > = lib.get(b"crypt")?;
        let value = crypt(password_c.as_ptr(), hash_c.as_ptr());
        Ok(!value.is_null() && bool::from(CStr::from_ptr(value).to_bytes().ct_eq(hash.as_bytes())))
    }
}

/// For a copy made to test or measure: nothing in it may reach a real service on its own. MCP servers are turned
/// off, scheduled tasks paused and every channel except the built-in web chat turned off (the copy still holds
/// their tokens, and a gateway started on it would otherwise connect to the real bots). Returns what was changed.
pub fn neutralize_home(target: &Path) -> Result<Vec<String>> {
    let db = HiveDB::open(target.join("data/hivedb"))?;
    let mut changed = vec![];
    for row in docs(&db, "mcpServers")? {
        let id = string(&row, "id")?.to_owned();
        if row["enabled"] == true {
            let mut next = row.clone();
            next["enabled"] = json!(false);
            db.col_put("mcpServers", &id, &next, Default::default())?;
            changed.push(format!("servidor MCP {id}: apagado"));
        }
    }
    for row in docs(&db, "cronJobs")? {
        let id = string(&row, "id")?.to_owned();
        if row["status"] == "active" {
            let mut next = row.clone();
            next["status"] = json!("paused");
            db.col_put("cronJobs", &id, &next, Default::default())?;
            changed.push(format!("tarea programada {id}: en pausa"));
        }
    }
    for row in docs(&db, "channels")? {
        let id = string(&row, "id")?.to_owned();
        if id != "webchat" && (row["enabled"] == true || row["active"] == true) {
            let mut next = row.clone();
            next["enabled"] = json!(false);
            next["active"] = json!(false);
            db.col_put("channels", &id, &next, Default::default())?;
            changed.push(format!("canal {id}: apagado"));
        }
    }
    Ok(changed)
}

pub fn copy_home(source: &Path, target: &Path) -> Result<()> {
    let source = fs::canonicalize(source)?;
    if target.exists() {
        return Err("El destino debe ser nuevo; no se sobrescriben perfiles".into());
    }
    let parent = fs::canonicalize(
        target
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    if parent.starts_with(&source) {
        return Err("La copia debe estar fuera del perfil original".into());
    }
    let database = source.join("data/hivedb");
    if !database.join("collections.redb").is_file() {
        return Err("El origen no contiene HiveDB".into());
    }
    let mut files = Vec::new();
    collect_files(&database, &mut files)?;
    // redb uses flock/LockFileEx. Keep every database locked until copying completes.
    let mut locks = Vec::new();
    for file in &files {
        if file.extension().is_some_and(|e| e == "redb") {
            let handle = fs::File::open(file)?;
            fs4::FileExt::try_lock_exclusive(&handle)
                .map_err(|_| "HiveDB está en uso; detén Hive antes de copiarla")?;
            locks.push(handle);
        }
    }
    let key = master_key(&source)?;
    let mut directory = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(target)?;
    for file in files {
        let destination = target
            .join("data/hivedb")
            .join(file.strip_prefix(&database)?);
        fs::create_dir_all(destination.parent().ok_or("Destino inválido")?)?;
        fs::copy(&file, destination)?;
    }
    if source.join(".auth_token").is_file() {
        fs::copy(source.join(".auth_token"), target.join(".auth_token"))?;
    }
    if let Some(key) = key {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        use std::io::Write;
        options
            .open(target.join(".master.key"))?
            .write_all(hex::encode(key).as_bytes())?;
    }
    println!("Copia aislada preparada en {}", target.display());
    Ok(())
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err("No se copian enlaces simbólicos dentro de HiveDB".into());
        }
        if kind.is_dir() {
            collect_files(&entry.path(), files)?;
        } else if kind.is_file() {
            files.push(entry.path());
        }
    }
    Ok(())
}

#[cfg(test)]
mod key_tests {
    #[test]
    fn delegation_budget_uses_iterations_and_roster_and_remains_bounded() {
        use hive_agent_core::objective::Objective;
        use serde_json::json;
        let policy = super::turn_policy(&json!({"max_iterations":2}), 17).unwrap();
        assert_eq!(policy.max_tool_calls, 68);
        assert_eq!(policy.max_model_calls, 36);
        assert_eq!(
            policy.max_total_output_tokens,
            36 * super::MAX_OUTPUT_TOKENS
        );
        let objective = Objective::new(policy).unwrap();
        for _ in 0..68 {
            objective.count_tool(0).unwrap();
        }
        assert!(
            objective
                .count_tool(0)
                .unwrap_err()
                .to_string()
                .contains("tool_call_limit")
        );
        for _ in 0..36 {
            objective.reserve_model(0, None).unwrap();
        }
        assert!(
            objective
                .reserve_model(0, None)
                .unwrap_err()
                .to_string()
                .contains("model_call_limit")
        );
        let default = super::turn_policy(&json!({}), 0).unwrap();
        assert_eq!(default.max_tool_calls, 10);
        assert_eq!(default.max_model_calls, 20);
    }

    #[test]
    fn invalid_or_overflowing_iteration_budgets_fail_before_execution() {
        use serde_json::json;
        for iterations in [
            json!(0),
            json!(-1),
            json!(1.5),
            json!("10"),
            json!(u64::MAX),
        ] {
            assert!(super::turn_policy(&json!({"max_iterations":iterations}), 17).is_err());
        }
        assert!(super::turn_policy(&json!({"max_iterations":u32::MAX}), 2).is_err());
        assert!(super::turn_policy(&json!({"max_iterations":2}), usize::MAX).is_err());
    }

    #[test]
    fn output_reserve_respects_context_limits_and_portable_bounds() {
        use serde_json::json;
        assert_eq!(
            super::output_cap(&json!({}), &json!({"context_window":50000})),
            7500
        );
        assert_eq!(
            super::output_cap(&json!({"num_ctx":32768}), &json!({"context_window":50000})),
            4915
        );
        assert_eq!(
            super::output_cap(&json!({}), &json!({"context_window":1050000})),
            32768
        );
        assert_eq!(
            super::output_cap(&json!({}), &json!({"context_window":u64::MAX})),
            32768
        );
        assert_eq!(
            super::output_cap(&json!({}), &json!({"context_window":1})),
            1
        );
        assert_eq!(
            super::output_cap(&json!({}), &json!({"context_window":0})),
            4096
        );
    }
    #[test]
    fn hiveagents_loading_context_comes_from_hivedb_model_and_provider_limits() {
        use serde_json::json;
        assert_eq!(
            super::hiveagents_context(&json!({}), &json!({"context_window":50000})),
            50000
        );
        assert_eq!(
            super::hiveagents_context(&json!({"num_ctx":32768}), &json!({"context_window":50000})),
            32768
        );
        assert_eq!(
            super::hiveagents_context(&json!({"num_ctx":100000}), &json!({})),
            100000
        );
        assert_eq!(super::hiveagents_context(&json!({}), &json!({})), 50000);
    }
    #[test]
    fn environment_master_key_matches_hive_utf8_padding_and_truncation() {
        assert_eq!(
            super::environment_master_key("abc").unwrap(),
            b"abc00000000000000000000000000000"
        );
        assert_eq!(
            super::environment_master_key("abcdefghijklmnopqrstuvwxyz0123456789").unwrap(),
            b"abcdefghijklmnopqrstuvwxyz012345"
        );
    }
}

#[cfg(test)]
mod ollama_tests {
    #[test]
    fn every_way_of_naming_an_ollama_server_ends_at_its_native_api() {
        for base in [
            "http://localhost:11434",
            "http://localhost:11434/",
            "http://localhost:11434/v1",
            "http://localhost:11434/v1/",
            "http://localhost:11434/api",
            "http://localhost:11434/api/",
        ] {
            assert_eq!(
                super::ollama_api(base),
                "http://localhost:11434/api",
                "{base}"
            );
        }
        assert_eq!(
            super::ollama_api("https://host.example/ollama"),
            "https://host.example/ollama/api"
        );
    }
}

#[cfg(test)]
mod ollama_ctx_tests {
    use serde_json::json;
    #[test]
    fn ollama_is_asked_for_the_models_window_up_to_a_ceiling_unless_the_operator_chose_one() {
        let model = |ctx: u64| json!({"context_window":ctx});
        // The model's window from HiveDB, but never an enormous one by default.
        assert_eq!(
            super::ollama_num_ctx(&json!({}), &model(8_192)),
            Some(8_192)
        );
        assert_eq!(
            super::ollama_num_ctx(&json!({}), &model(262_144)),
            Some(32_768)
        );
        // The operator's explicit choice wins over the ceiling, but not over what the model supports.
        assert_eq!(
            super::ollama_num_ctx(&json!({"num_ctx":65_536}), &model(262_144)),
            Some(65_536)
        );
        assert_eq!(
            super::ollama_num_ctx(&json!({"num_ctx":65_536}), &model(16_384)),
            Some(16_384)
        );
        assert_eq!(
            super::ollama_num_ctx(&json!({"num_ctx":8_192}), &model(262_144)),
            Some(8_192)
        );
        // A model row without a window: the operator's, or none.
        assert_eq!(
            super::ollama_num_ctx(&json!({"num_ctx":8_192}), &json!({})),
            Some(8_192)
        );
        assert_eq!(super::ollama_num_ctx(&json!({}), &json!({})), None);
        assert_eq!(super::ollama_num_ctx(&json!({}), &model(0)), None);
    }
}
