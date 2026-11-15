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
    pub activity_lock: Mutex<()>,
    pub db: Arc<HiveDB>,
    pub home: PathBuf,
    pub db_path: PathBuf,
    pub token: String,
    pub shutdown: CancellationToken,
    pub sessions: Mutex<HashMap<String, Arc<DurableEngineHandle>>>,
    pub active: Mutex<HashMap<String, CancellationToken>>,
    pub store: Arc<Mutex<Option<SessionStore>>>,
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
            activity_lock: Mutex::new(()),
            db,
            home,
            db_path,
            token,
            shutdown: CancellationToken::new(),
            sessions: Mutex::new(HashMap::new()),
            active: Mutex::new(HashMap::new()),
            store: Arc::new(Mutex::new(Some(store))),
        })
    }

    pub fn has_credentials(&self) -> Result<bool> {
        Ok(docs(&self.db, "users")?.iter().any(|u| {
            u["email"].as_str().is_some_and(|s| !s.is_empty())
                && u["password_hash"].as_str().is_some_and(|s| !s.is_empty())
        }))
    }

    pub fn authorized(&self, token: Option<&str>) -> Result<bool> {
        if !self.has_credentials()? {
            return Ok(true);
        }
        Ok(token.is_some_and(|t| bool::from(t.as_bytes().ct_eq(self.token.as_bytes()))))
    }

    pub fn selection(&self) -> Result<Selection> {
        self.select(None)
    }

    pub fn selection_for_agent(&self, agent_id: &str) -> Result<Selection> {
        self.select(Some(agent_id))
    }

    pub fn delegation_enabled(&self) -> Result<bool> {
        Ok(self
            .db
            .col_get("tools", "task_delegate")?
            .is_none_or(|tool| tool.doc["enabled"] != false && tool.doc["active"] != false))
    }

    fn select(&self, requested: Option<&str>) -> Result<Selection> {
        let users = docs(&self.db, "users")?;
        let user = string(users.first().ok_or("HiveDB no tiene usuarios")?, "id")?.to_owned();
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
        let base = provider_endpoint(&pair.0, provider["base_url"].as_str())?;
        let kind = match pair.0.as_str() {
            "anthropic" => "anthropic",
            "gemini" => "gemini",
            "hiveagents" => "hiveagents",
            "ollama" => {
                return Err("Ollama nativo todavía no está adaptado; configura un proveedor OpenAI compatible en HiveDB".into());
            }
            _ => "openai_compatible",
        };
        let key = secret(
            &self.db,
            &self.home,
            &format!("provider:{}:api_key", pair.0),
        )?;
        let mut headers = secret(
            &self.db,
            &self.home,
            &format!("provider:{}:headers", pair.0),
        )?
        .map(|s| serde_json::from_str::<HashMap<String, String>>(&s))
        .transpose()?
        .unwrap_or_default();
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
        }
        if kind == "openai_compatible" {
            transport["output_token_parameter"] = json!(if pair.0 == "openai" {
                "max_completion_tokens"
            } else {
                "max_tokens"
            });
        }
        let native = NativeProvider::from_json(&transport.to_string())?.with_headers(headers)?;
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
            system.push_str("\nPuedes crear formularios y tarjetas en el Panel interactivo con las herramientas a2ui. Crea la superficie, envía componentes completos con raíz root y datos enlazados. Las acciones de los botones llegarán como un nuevo turno. No afirmes que una acción externa se ejecutó por mostrar un panel.");
        }
        let policy = turn_policy(&agent, workers.len())?;
        let engine_config = json!({"op":"create", "model":wire_model(&pair.0, string(model, "id")?),
            "system":system, "tools":tools, "max_turns":agent_iterations(&agent)?,
            "policy":{"max_output_tokens":output_cap(provider, model),"max_tool_calls":policy.max_tool_calls}}).to_string();
        Ok(Selection {
            provider_id: pair.0,
            model_id: pair.1,
            user,
            agent,
            provider: native,
            engine_config,
        })
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
            self.store.clone(),
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

fn master_key(home: &Path) -> Result<Option<Vec<u8>>> {
    if let Ok(key) = std::env::var("HIVE_MASTER_KEY")
        && !key.is_empty()
    {
        // crypto.ts uses UTF-8 of slice(0,32).padEnd(32,"0"), not hexadecimal.
        return Ok(Some(environment_master_key(&key)?));
    }
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
