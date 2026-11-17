//! Audited, additive catalog. Never changes activation, credentials or agent bindings.
use crate::{
    Result,
    config::{self, App},
    integrations,
};
use hivedb_core::{ColOp, HiveDB};
use serde_json::{Value, json};

pub(crate) fn snapshot() -> Value {
    serde_json::from_str(include_str!("../catalog-2026-11.json")).expect("bundled model catalog")
}
fn key(provider: &str, wire: &str) -> String {
    if ["nvidia", "openrouter", "opencode-go", "groq", "modelscope"].contains(&provider) {
        format!("{provider}/{wire}")
    } else {
        wire.into()
    }
}
pub(crate) fn capabilities(row: &Value) -> Vec<String> {
    let value = if let Some(s) = row["capabilities"].as_str() {
        serde_json::from_str(s).unwrap_or(Value::Null)
    } else {
        row["capabilities"].clone()
    };
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_lowercase)
        .collect()
}
pub(crate) fn supports(row: &Value, purpose: &str) -> bool {
    let caps = capabilities(row);
    let has = |c: &str| caps.iter().any(|s| s == c);
    match purpose {
        // An OCR-only endpoint is not a general vision chat endpoint.
        "vision" => {
            (row["model_type"] == "llm" && has("vision"))
                || (row["model_type"] == "vision" && !has("ocr"))
        }
        "stt" => row["model_type"] == "stt" && !has("realtime"),
        "tts" => row["model_type"] == "tts",
        _ => false,
    }
}
fn merge(
    db: &HiveDB,
    provider: &str,
    entries: &[Value],
    source: &str,
    reviewed: &str,
) -> Result<usize> {
    let mut ops = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for entry in entries {
        let Some(wire) = entry["wire_id"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 240 && !s.chars().any(char::is_control))
        else {
            continue;
        };
        let id = key(provider, wire);
        if !seen.insert(id.clone()) {
            continue;
        }
        let old = db.col_get("models", &id)?;
        // A global collection key must never transfer ownership between providers.
        if old
            .as_ref()
            .is_some_and(|r| r.doc["provider_id"] != provider)
        {
            continue;
        }
        // A model from the reviewed catalog is `catalog`; one the provider's own list gave us is `discovered`. The
        // TypeScript Hive wipes and recreates every `catalog` row on start and keeps the rest, so a synced model
        // must not be marked `catalog` or it would vanish the next time that app starts.
        let mut doc = old.as_ref().map(|r|r.doc.clone()).unwrap_or_else(||json!({"id":id,"provider_id":provider,"wire_id":wire,"enabled":false,"active":false,"source":if reviewed == "2026-11-13" { "catalog" } else { "discovered" },"name":entry["name"].as_str().unwrap_or(wire)}));
        // Only repair absent metadata. Preserve custom names, prices, capabilities and context.
        // The one exception is an entry the review marks `refresh_capabilities`: its capabilities
        // were verified against the provider and supersede what an earlier sync stored.
        for field in ["model_type", "capabilities", "context_window"] {
            let superseded = field == "capabilities" && entry["refresh_capabilities"] == true;
            if (superseded
                || doc[field].is_null()
                || doc[field] == ""
                || doc[field] == "[]"
                || doc[field] == "unknown"
                // A window of 0 is "unknown" (the TypeScript Hive writes it for models it knows nothing about).
                || (field == "context_window" && doc[field].as_u64() == Some(0)))
                && !entry[field].is_null()
            {
                doc[field] = if field == "capabilities" {
                    json!(entry[field].to_string())
                } else {
                    entry[field].clone()
                };
            }
        }
        // The owner's price is never replaced; an absent one is filled with what the provider publishes.
        for field in ["input_per_1m", "output_per_1m"] {
            if doc[field].is_null() && entry[field].is_number() {
                doc[field] = entry[field].clone();
            }
        }
        if let Some(object) = doc.as_object_mut() {
            // The provider lists it again: no longer "unlisted".
            object.remove("unlisted");
        }
        if reviewed != "2026-11-13" {
            doc["synced_at"] = json!(chrono::Utc::now().timestamp_millis());
        }
        if reviewed == "2026-11-13" {
            doc["catalog_source_url"] = json!(entry["source_url"].as_str().unwrap_or(source));
            doc["catalog_reviewed_at"] = json!(reviewed);
        } else {
            doc["catalog_sync_source_url"] = json!(source);
            doc["catalog_synced_at"] = json!(reviewed);
        }
        if let Some(note) = entry["availability_note"].as_str() {
            doc["availability_note"] = json!(note);
        }
        if let Some(date) = entry["catalog_listed_at"].as_str() {
            doc["catalog_listed_at"] = json!(date);
        }
        if old.as_ref().is_some_and(|r| r.doc == doc) {
            continue;
        }
        ops.push(ColOp::Put {
            collection: "models".into(),
            id,
            doc,
            expected_version: Some(old.map_or(0, |r| r.version)),
        });
    }
    let changed = ops.len();
    if !ops.is_empty() {
        db.col_batch(&ops)?;
    }
    Ok(changed)
}
pub(crate) fn ensure(db: &HiveDB) -> Result<()> {
    let snapshot = snapshot();
    for provider in snapshot["providers"].as_array().into_iter().flatten() {
        let id = config::string(provider, "id")?;
        if db.col_get("providers", id)?.is_none() {
            continue;
        }
        merge(
            db,
            id,
            provider["models"].as_array().unwrap(),
            config::string(provider, "source_url")?,
            "2026-11-13",
        )?;
    }
    Ok(())
}

/// Normalizes only declared metadata. Bare identifiers remain unclassified until reviewed.
fn discovered(provider: &str, value: &Value) -> Result<Vec<Value>> {
    let rows = value
        .as_array()
        .or_else(|| value["data"].as_array())
        .or_else(|| value["models"].as_array())
        .ok_or("El proveedor no devolvió un catálogo de modelos válido")?;
    if rows.len() > 10000 {
        return Err("Catálogo demasiado grande".into());
    }
    let snapshot = snapshot();
    let audited = snapshot["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == provider);
    let mut result = Vec::new();
    for row in rows {
        let Some(wire) = row["id"]
            .as_str()
            .or_else(|| row["model_id"].as_str())
            .or_else(|| row["name"].as_str())
            .map(|id| id.strip_prefix("models/").unwrap_or(id))
        else {
            continue;
        };
        let known = audited
            .and_then(|p| p["models"].as_array())
            .and_then(|ms| ms.iter().find(|m| m["wire_id"] == wire));
        let mut entry = known
            .cloned()
            .unwrap_or_else(|| json!({"wire_id":wire,"model_type":"unknown"}));
        if provider == "elevenlabs" && row["can_do_text_to_speech"] == true {
            entry["model_type"] = json!("tts");
            entry["capabilities"] = json!(["tts", "speech"]);
        }
        if let Some(name) = row["displayName"].as_str().or_else(|| row["name"].as_str()) {
            entry["name"] = json!(name);
        }
        let inputs = &row["architecture"]["input_modalities"];
        let inputs = inputs
            .as_array()
            .or_else(|| row["input_modalities"].as_array());
        let outputs = row["architecture"]["output_modalities"]
            .as_array()
            .or_else(|| row["output_modalities"].as_array());
        if let (Some(inputs), Some(outputs)) = (inputs, outputs) {
            let mut caps = vec![];
            if inputs.contains(&json!("image")) {
                caps.push("vision");
            }
            let typ = if outputs.contains(&json!("image")) {
                caps.push("image_generation");
                "image"
            } else if outputs.contains(&json!("audio")) {
                caps.push("tts");
                "tts"
            } else if inputs.contains(&json!("audio")) && !inputs.contains(&json!("text")) {
                caps.push("transcription");
                "stt"
            } else if outputs.contains(&json!("text")) {
                caps.push("chat");
                "llm"
            } else {
                "unknown"
            };
            entry["model_type"] = json!(typ);
            entry["capabilities"] = json!(caps);
        }
        // Anthropic declares what each model accepts.
        if provider == "anthropic" {
            let mut caps = vec!["chat"];
            if row["capabilities"]["image_input"]["supported"] == true {
                caps.push("vision");
            }
            if row["capabilities"]["structured_outputs"]["supported"] == true {
                caps.push("json_mode");
            }
            if row["capabilities"]["thinking"]["supported"] == true {
                caps.push("reasoning");
            }
            entry["model_type"] = json!("llm");
            entry["capabilities"] = json!(caps);
        }
        // Gemini: the generation methods a model supports say what kind of model it is.
        if provider == "gemini"
            && let Some(methods) = row["supportedGenerationMethods"].as_array()
        {
            let has = |m: &str| methods.iter().any(|v| v == m);
            if has("bidiGenerateContent") {
                entry["model_type"] = json!("realtime");
                entry["capabilities"] = json!(["realtime"]);
            } else if wire.contains("tts") {
                entry["model_type"] = json!("tts");
                entry["capabilities"] = json!(["tts", "speech"]);
            } else if has("generateContent") || has("streamGenerateContent") {
                if ["image", "imagen", "veo"].iter().any(|k| wire.contains(k)) {
                    entry["model_type"] = json!("image");
                    entry["capabilities"] = json!(["image_generation"]);
                } else {
                    entry["model_type"] = json!("llm");
                    entry["capabilities"] = json!(["chat", "streaming"]);
                }
            } else if has("embedContent") || has("batchEmbedContents") {
                entry["model_type"] = json!("embedding");
            } else if has("predict") || has("predictLongRunning") {
                entry["model_type"] = json!("image");
                entry["capabilities"] = json!(["image_generation"]);
            }
        }
        // Mistral declares capabilities as an object; OpenRouter lists the parameters a model supports. Both are
        // added to whatever the entry already declares: never replacing, never dropping one.
        let mut caps: Vec<String> = entry["capabilities"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let before = caps.len();
        let mut add = |cap: &str| {
            if !caps.iter().any(|c| c == cap) {
                caps.push(cap.to_owned());
            }
        };
        if let Some(declared) = row["capabilities"].as_object()
            && provider != "anthropic"
        {
            for (flag, cap) in [
                ("completion_chat", "chat"),
                ("function_calling", "function_calling"),
                ("vision", "vision"),
            ] {
                if declared.get(flag).is_some_and(|v| *v == true) {
                    add(cap);
                }
            }
        }
        if let Some(params) = row["supported_parameters"].as_array() {
            let has = |p: &str| params.iter().any(|v| v == p);
            if has("tools") {
                add("function_calling");
            }
            if has("response_format") || has("structured_outputs") {
                add("json_mode");
            }
            if has("reasoning") || has("include_reasoning") {
                add("reasoning");
            }
        }
        if caps.len() != before {
            entry["capabilities"] = json!(caps);
        }
        for field in [
            "context_window",
            "context_length",
            "inputTokenLimit",
            "max_input_tokens",
            "max_context_length",
            "max_model_len",
        ] {
            if let Some(n) = row[field].as_u64().filter(|n| *n > 0) {
                entry["context_window"] = json!(n);
                break;
            }
        }
        if entry["context_window"].is_null()
            && let Some(n) = row["top_provider"]["context_length"]
                .as_u64()
                .filter(|n| *n > 0)
        {
            entry["context_window"] = json!(n);
        }
        // A published price (OpenRouter: USD per token, as text; a negative one means "varies").
        let per_token = |v: &Value| {
            v.as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .or_else(|| v.as_f64())
                .filter(|n| *n >= 0.0)
        };
        if let (Some(prompt), Some(completion)) = (
            per_token(&row["pricing"]["prompt"]),
            per_token(&row["pricing"]["completion"]),
        ) {
            entry["input_per_1m"] = json!((prompt * 1e6 * 1e4).round() / 1e4);
            entry["output_per_1m"] = json!((completion * 1e6 * 1e4).round() / 1e4);
        }
        result.push(entry);
    }
    Ok(result)
}
/// Providers whose list is not complete by design (models are mounted on demand): nothing is ever marked there.
const PARTIAL_LISTING: [&str; 2] = ["hiveagents", "piper"];

/// After a sync that listed `entries`, mark the provider's other models `unlisted`: the provider no longer offers them.
/// They are not deleted or switched off (an agent may point at one); the mark is what lets a screen warn about it.
/// An empty list, or a provider with a partial one, marks nothing. Returns how many were marked.
fn mark_unlisted(db: &HiveDB, provider: &str, entries: &[Value]) -> Result<usize> {
    if entries.is_empty() || PARTIAL_LISTING.contains(&provider) {
        return Ok(0);
    }
    let listed: std::collections::HashSet<String> = entries
        .iter()
        .filter_map(|e| e["wire_id"].as_str().map(|wire| key(provider, wire)))
        .collect();
    let mut ops = vec![];
    for entry in db.col_scan("models", &hivedb_core::ScanOptions::default())? {
        let (id, version, mut doc) = (entry.id, entry.version, entry.doc);
        if doc["provider_id"] != provider || listed.contains(&id) || doc["unlisted"] == true {
            continue;
        }
        doc["unlisted"] = json!(true);
        doc["synced_at"] = json!(chrono::Utc::now().timestamp_millis());
        ops.push(ColOp::Put {
            collection: "models".into(),
            id,
            doc,
            expected_version: Some(version),
        });
    }
    let marked = ops.len();
    if marked > 0 {
        db.col_batch(&ops)?;
    }
    Ok(marked)
}

/// Fill `context_window` of each listed Ollama model from `/api/show` (`model_info.<family>.context_length`).
/// A model that cannot be shown is left as it is; nothing here fails the sync.
async fn ollama_context(client: &reqwest::Client, show_url: &str, entries: &mut [Value]) {
    for entry in entries.iter_mut().take(200) {
        if entry["context_window"].as_u64().is_some() {
            continue;
        }
        let Some(name) = entry["wire_id"].as_str().map(str::to_owned) else {
            continue;
        };
        let Ok(response) = client
            .post(show_url)
            .json(&json!({"model":name}))
            .send()
            .await
        else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        let Ok(bytes) = integrations::limited(response, 2 * 1024 * 1024).await else {
            continue;
        };
        let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if let Some(window) = context_length(&body) {
            entry["context_window"] = json!(window);
        }
    }
}

/// The context length an Ollama `/api/show` reply declares: the first `*.context_length` of `model_info`.
fn context_length(show: &Value) -> Option<u64> {
    show["model_info"]
        .as_object()?
        .iter()
        .filter(|(key, _)| key.ends_with(".context_length"))
        .filter_map(|(_, value)| value.as_u64().filter(|n| *n > 0))
        .next()
}

pub(crate) fn sync(app: &App, provider: &str) -> Result<Value> {
    if provider == "piper" {
        return Err("Las voces locales se administran en la configuración de Piper".into());
    }
    let row = app
        .db
        .col_get("providers", provider)?
        .ok_or("Proveedor desconocido")?;
    if row.doc["enabled"] != true || row.doc["active"] != true {
        return Err("Activa el proveedor antes de sincronizar su catálogo".into());
    }
    let base = config::provider_endpoint(provider, row.doc["base_url"].as_str())?;
    let url = if provider == "ollama" {
        format!("{}/api/tags", base.trim_end_matches("/v1"))
    } else {
        format!("{base}/models")
    };
    let key = config::secret(&app.db, &app.home, &format!("provider:{provider}:api_key"))?;
    if key.is_none() && !["ollama", "openrouter"].contains(&provider) {
        return Err("Guarda la clave del proveedor antes de sincronizar".into());
    }
    let p = provider.to_owned();
    let source = integrations::safe_url(&url);
    let entries = integrations::blocking(async move {
        let client = integrations::client()?;
        let mut all = Vec::new();
        let mut cursor = None::<(String, String)>;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..50 {
            let mut page_url = reqwest::Url::parse(&url)?;
            if let Some((name, value)) = &cursor {
                page_url.query_pairs_mut().append_pair(name, value);
            }
            let mut req = client.get(page_url);
            if let Some(key) = &key {
                req = if p == "gemini" {
                    req.header("x-goog-api-key", key)
                } else if p == "elevenlabs" {
                    req.header("xi-api-key", key)
                } else {
                    req.bearer_auth(key)
                };
            }
            if p == "anthropic" {
                req = req.header("anthropic-version", "2023-06-01");
            }
            let response = req
                .send()
                .await
                .map_err(|_| "No se pudo contactar con el catálogo del proveedor")?;
            if !response.status().is_success() {
                return Err(format!(
                    "El catálogo respondió HTTP {}; revisa el endpoint y los permisos de tu cuenta",
                    response.status().as_u16()
                )
                .into());
            }
            let value: Value =
                serde_json::from_slice(&integrations::limited(response, 8 * 1024 * 1024).await?)?;
            all.extend(discovered(&p, &value)?);
            if all.len() > 10000 {
                return Err("Catálogo demasiado grande".into());
            }
            cursor = if p == "gemini" {
                value["nextPageToken"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(|s| ("pageToken".into(), s.to_owned()))
            } else if p == "anthropic" && value["has_more"] == true {
                Some((
                    "after_id".into(),
                    config::string(&value, "last_id")?.to_owned(),
                ))
            } else {
                None
            };
            let Some((_, token)) = &cursor else {
                if p == "ollama" {
                    // `/api/tags` lists models without their context length; `/api/show` has it.
                    ollama_context(&client, &url.replace("/api/tags", "/api/show"), &mut all).await;
                }
                return Ok(all);
            };
            if !seen.insert(token.clone()) {
                return Err("El proveedor repitió la página del catálogo".into());
            }
        }
        Err("El catálogo excede el límite de páginas; no se aplicaron cambios".into())
    })?;
    let _guard = app
        .activity_lock
        .write()
        .map_err(|_| "Catálogo bloqueado")?;
    let updated = merge(
        &app.db,
        provider,
        &entries,
        &source,
        &chrono::Utc::now().to_rfc3339(),
    )?;
    let unlisted = mark_unlisted(&app.db, provider, &entries)?;
    Ok(json!({"ok":true,"discovered":entries.len(),"updated":updated,"unlisted":unlisted}))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_shipped_catalog_gives_a_window_to_the_nvidia_models_that_have_none_and_never_overrides_a_choice()
     {
        let snapshot = snapshot();
        let nvidia = snapshot["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "nvidia")
            .unwrap();
        let window = |wire: &str| {
            nvidia["models"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["wire_id"] == wire)
                .unwrap()["context_window"]
                .as_u64()
        };
        assert_eq!(
            window("nvidia/nemotron-3-nano-omni-30b-a3b-reasoning"),
            Some(262_144)
        );
        assert_eq!(
            window("nvidia/nemotron-3.5-super-vl-120b-a12b"),
            Some(1_000_000)
        );
        assert_eq!(window("nvidia/nemotron-3-ultra-550b-a55b"), Some(1_000_000));
        assert_eq!(window("meta/muse-glimmer-30b"), Some(131_072));
        // Only models that were seen calling tools stay in the NVIDIA list.
        for model in nvidia["models"].as_array().unwrap() {
            assert!(
                model["capabilities"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("function_calling")),
                "{} does not declare function_calling",
                model["wire_id"]
            );
        }
        assert_eq!(
            window("nvidia/nemotron-3.5-bolt-e4b"),
            Some(65_536),
            "the conservative end of disagreeing sources"
        );
        // Merged into a database: a row without a window gets it; a row that has one keeps its own.
        let dir = tempfile::tempdir().unwrap();
        let db = HiveDB::open(dir.path()).unwrap();
        db.col_put(
            "providers",
            "nvidia",
            &json!({"id":"nvidia"}),
            Default::default(),
        )
        .unwrap();
        for (id, doc) in [
            (
                "nvidia/nvidia/nemotron-3-ultra-550b-a55b",
                json!({"id":"nvidia/nvidia/nemotron-3-ultra-550b-a55b","provider_id":"nvidia","wire_id":"nvidia/nemotron-3-ultra-550b-a55b","name":"ultra","model_type":"llm","enabled":true,"active":true}),
            ),
            (
                "nvidia/nvidia/nemotron-3.5-bolt-e4b",
                json!({"id":"nvidia/nvidia/nemotron-3.5-bolt-e4b","provider_id":"nvidia","wire_id":"nvidia/nemotron-3.5-bolt-e4b","name":"bolt","model_type":"llm","context_window":131_072,"enabled":true,"active":true}),
            ),
        ] {
            db.col_put("models", id, &doc, Default::default()).unwrap();
        }
        ensure(&db).unwrap();
        assert_eq!(
            db.col_get("models", "nvidia/nvidia/nemotron-3-ultra-550b-a55b")
                .unwrap()
                .unwrap()
                .doc["context_window"],
            1_000_000
        );
        assert_eq!(
            db.col_get("models", "nvidia/nvidia/nemotron-3.5-bolt-e4b")
                .unwrap()
                .unwrap()
                .doc["context_window"],
            131_072,
            "what is already chosen is kept"
        );
    }

    #[test]
    fn an_ollama_show_reply_declares_its_context_length_in_model_info() {
        assert_eq!(
            context_length(
                &json!({"model_info":{"general.architecture":"qwen3","qwen3.context_length":262144,"qwen3.block_count":36}})
            ),
            Some(262_144)
        );
        assert_eq!(
            context_length(&json!({"model_info":{"llama.context_length":8192}})),
            Some(8_192)
        );
        assert_eq!(
            context_length(&json!({"model_info":{"llama.context_length":0}})),
            None
        );
        assert_eq!(
            context_length(&json!({"model_info":{"general.architecture":"x"}})),
            None
        );
        assert_eq!(context_length(&json!({"details":{}})), None);
    }
    #[test]
    fn additive_catalog_preserves_choices_and_reseller_namespaces() {
        let dir = tempfile::tempdir().unwrap();
        let db = HiveDB::open(dir.path()).unwrap();
        db.col_put(
            "providers",
            "gemini",
            &json!({"id":"gemini"}),
            Default::default(),
        )
        .unwrap();
        db.col_put(
            "providers",
            "nvidia",
            &json!({"id":"nvidia"}),
            Default::default(),
        )
        .unwrap();
        db.col_put("models","gemini-3.8-live",&json!({"id":"gemini-3.8-live","provider_id":"gemini","enabled":true,"active":true,"name":"Mi Live","input_per_1m":7}),Default::default()).unwrap();
        ensure(&db).unwrap();
        let first = db.col_get("models", "gemini-3.8-live").unwrap().unwrap();
        assert_eq!(first.doc["name"], "Mi Live");
        assert_eq!(first.doc["active"], true);
        assert_eq!(first.doc["input_per_1m"], 7);
        let new = db.col_get("models", "gemini-4-carbon").unwrap().unwrap();
        assert_eq!(new.doc["enabled"], false);
        assert_eq!(new.doc["active"], false);
        assert!(
            db.col_get("models", "nvidia/nvidia/nemotron-3.5-super-vl-120b-a12b")
                .unwrap()
                .is_some()
        );
        ensure(&db).unwrap();
        assert_eq!(
            first.version,
            db.col_get("models", "gemini-3.8-live")
                .unwrap()
                .unwrap()
                .version
        );
    }
    #[test]
    fn a_reviewed_entry_marked_refresh_replaces_stored_capabilities_and_nothing_else_does() {
        let dir = tempfile::tempdir().unwrap();
        let db = HiveDB::open(dir.path()).unwrap();
        db.col_put(
            "providers",
            "nvidia",
            &json!({"id":"nvidia"}),
            Default::default(),
        )
        .unwrap();
        let stored = json!({"id":"nvidia/a/one","provider_id":"nvidia","wire_id":"a/one","enabled":true,"active":true,"name":"Mi nombre","input_per_1m":3,
            "model_type":"llm","capabilities":"[\"chat\"]"});
        db.col_put("models", "nvidia/a/one", &stored, Default::default())
            .unwrap();
        let mut other = stored.clone();
        other["id"] = json!("nvidia/a/two");
        other["wire_id"] = json!("a/two");
        db.col_put("models", "nvidia/a/two", &other, Default::default())
            .unwrap();
        let entries = [
            json!({"wire_id":"a/one","model_type":"llm","capabilities":["chat","function_calling"],"refresh_capabilities":true,"availability_note":"verificado"}),
            json!({"wire_id":"a/two","model_type":"llm","capabilities":["chat","function_calling"]}),
        ];
        assert_eq!(
            merge(&db, "nvidia", &entries, "src", "2026-11-13").unwrap(),
            2
        );
        let one = db.col_get("models", "nvidia/a/one").unwrap().unwrap().doc;
        assert_eq!(
            one["capabilities"], "[\"chat\",\"function_calling\"]",
            "the reviewed capabilities replace the stored ones"
        );
        assert_eq!(
            (
                one["name"].clone(),
                one["input_per_1m"].clone(),
                one["active"].clone()
            ),
            (json!("Mi nombre"), json!(3), json!(true)),
            "name, price and activation are the owner's"
        );
        assert_eq!(one["availability_note"], "verificado");
        let two = db.col_get("models", "nvidia/a/two").unwrap().unwrap().doc;
        assert_eq!(
            two["capabilities"], "[\"chat\"]",
            "without the mark a stored value is kept, as always"
        );
        // Idempotent: the same review changes nothing the second time.
        assert_eq!(
            merge(&db, "nvidia", &entries, "src", "2026-11-13").unwrap(),
            0
        );
    }
    #[test]
    fn each_providers_own_list_declares_window_capabilities_kind_and_price() {
        let by = |rows: &[Value], wire: &str| {
            rows.iter().find(|e| e["wire_id"] == wire).cloned().unwrap()
        };
        // OpenRouter: window, modalities, supported parameters and a published price.
        let openrouter = discovered(
            "openrouter",
            &json!({"data":[{"id":"v/nuevo","name":"Nuevo","context_length":262144,
            "architecture":{"input_modalities":["text","image"],"output_modalities":["text"]},
            "supported_parameters":["tools","reasoning","response_format"],
            "pricing":{"prompt":"0.000003","completion":"0.000015"}},
            {"id":"v/variable","pricing":{"prompt":"-1","completion":"-1"}}]}),
        )
        .unwrap();
        let nuevo = by(&openrouter, "v/nuevo");
        assert_eq!(
            (nuevo["model_type"].clone(), nuevo["context_window"].clone()),
            (json!("llm"), json!(262144))
        );
        for cap in [
            "chat",
            "vision",
            "function_calling",
            "json_mode",
            "reasoning",
        ] {
            assert!(
                nuevo["capabilities"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(cap)),
                "{cap}: {nuevo}"
            );
        }
        assert_eq!(
            (
                nuevo["input_per_1m"].clone(),
                nuevo["output_per_1m"].clone()
            ),
            (json!(3.0), json!(15.0))
        );
        assert!(
            by(&openrouter, "v/variable")["input_per_1m"].is_null(),
            "a variable price is not a price"
        );
        // Mistral: capabilities declared as an object, and its own window field.
        let mistral = discovered(
            "mistral",
            &json!({"data":[{"id":"m-medio","max_context_length":128000,
            "capabilities":{"completion_chat":true,"function_calling":true,"vision":false}}]}),
        )
        .unwrap();
        let medio = by(&mistral, "m-medio");
        assert_eq!(medio["context_window"], 128000);
        assert_eq!(medio["capabilities"], json!(["chat", "function_calling"]));
        // Anthropic: declared input window and capabilities.
        let anthropic = discovered("anthropic", &json!({"data":[{"id":"claude-uno","display_name":"Claude Uno","max_input_tokens":1000000,
            "capabilities":{"image_input":{"supported":true},"thinking":{"supported":true}}}]})).unwrap();
        let uno = by(&anthropic, "claude-uno");
        assert_eq!(
            (uno["context_window"].clone(), uno["model_type"].clone()),
            (json!(1000000), json!("llm"))
        );
        assert_eq!(uno["capabilities"], json!(["chat", "vision", "reasoning"]));
        // Gemini: the generation methods decide the kind.
        let gemini = discovered("gemini", &json!({"models":[
            {"name":"models/g-flash","inputTokenLimit":1048576,"supportedGenerationMethods":["generateContent","streamGenerateContent"]},
            {"name":"models/g-live","supportedGenerationMethods":["bidiGenerateContent"]},
            {"name":"models/g-flash-tts","supportedGenerationMethods":["generateContent"]},
            {"name":"models/text-embedding-9","supportedGenerationMethods":["embedContent"]},
            {"name":"models/imagen-9","supportedGenerationMethods":["predict"]}]})).unwrap();
        let kind = |w: &str| by(&gemini, w)["model_type"].clone();
        assert_eq!(
            (
                kind("g-flash"),
                kind("g-live"),
                kind("g-flash-tts"),
                kind("text-embedding-9"),
                kind("imagen-9")
            ),
            (
                json!("llm"),
                json!("realtime"),
                json!("tts"),
                json!("embedding"),
                json!("image")
            )
        );
        assert_eq!(by(&gemini, "g-flash")["context_window"], 1048576);
    }

    #[test]
    fn a_window_of_zero_is_unknown_a_price_is_only_filled_when_absent_and_unlisted_models_are_marked()
     {
        let dir = tempfile::tempdir().unwrap();
        let db = HiveDB::open(dir.path().join("hivedb")).unwrap();
        // Written the way the TypeScript Hive writes a model it knows nothing about: window 0, no price.
        db.col_put("models", "nvidia/a/zero", &json!({"id":"nvidia/a/zero","provider_id":"nvidia","wire_id":"a/zero","enabled":true,"active":false,
            "source":"discovered","model_type":"llm","capabilities":"[\"chat\"]","context_window":0,"input_per_1m":null}), Default::default()).unwrap();
        db.col_put("models", "nvidia/a/priced", &json!({"id":"nvidia/a/priced","provider_id":"nvidia","wire_id":"a/priced","enabled":true,"active":true,
            "source":"discovered","model_type":"llm","capabilities":"[\"chat\"]","context_window":4096,"input_per_1m":9,"output_per_1m":9}), Default::default()).unwrap();
        db.col_put("models", "nvidia/a/gone", &json!({"id":"nvidia/a/gone","provider_id":"nvidia","wire_id":"a/gone","enabled":true,"active":true,
            "source":"catalog","model_type":"llm"}), Default::default()).unwrap();
        db.col_put(
            "models",
            "groq/other",
            &json!({"id":"groq/other","provider_id":"groq","enabled":true,"source":"catalog"}),
            Default::default(),
        )
        .unwrap();
        let entries = vec![
            json!({"wire_id":"a/zero","model_type":"llm","context_window":131072,"input_per_1m":1.5,"output_per_1m":6.0}),
            json!({"wire_id":"a/priced","model_type":"llm","context_window":999999,"input_per_1m":1.0,"output_per_1m":1.0}),
        ];
        merge(
            &db,
            "nvidia",
            &entries,
            "https://api.example/models",
            "2026-11-17T00:00:00Z",
        )
        .unwrap();
        let zero = db.col_get("models", "nvidia/a/zero").unwrap().unwrap().doc;
        assert_eq!(
            (
                zero["context_window"].clone(),
                zero["input_per_1m"].clone(),
                zero["output_per_1m"].clone()
            ),
            (json!(131072), json!(1.5), json!(6.0))
        );
        assert!(
            zero["synced_at"].as_i64().is_some(),
            "the sync time is written for the TypeScript Hive too"
        );
        let priced = db
            .col_get("models", "nvidia/a/priced")
            .unwrap()
            .unwrap()
            .doc;
        assert_eq!(
            (
                priced["context_window"].clone(),
                priced["input_per_1m"].clone()
            ),
            (json!(4096), json!(9)),
            "what the owner has stays"
        );
        // The model the provider no longer lists is marked, not deleted or switched off; other providers are untouched.
        assert_eq!(mark_unlisted(&db, "nvidia", &entries).unwrap(), 1);
        let gone = db.col_get("models", "nvidia/a/gone").unwrap().unwrap().doc;
        assert_eq!(
            (
                gone["unlisted"].clone(),
                gone["enabled"].clone(),
                gone["active"].clone()
            ),
            (json!(true), json!(true), json!(true))
        );
        assert!(db.col_get("models", "groq/other").unwrap().unwrap().doc["unlisted"].is_null());
        assert_eq!(
            mark_unlisted(&db, "nvidia", &entries).unwrap(),
            0,
            "already marked: nothing to do"
        );
        // It comes back: merge clears the mark.
        let back = vec![json!({"wire_id":"a/gone","model_type":"llm"})];
        merge(
            &db,
            "nvidia",
            &back,
            "https://api.example/models",
            "2026-11-18T00:00:00Z",
        )
        .unwrap();
        assert!(db.col_get("models", "nvidia/a/gone").unwrap().unwrap().doc["unlisted"].is_null());
        // An empty list, or a provider whose list is partial by design, marks nothing.
        assert_eq!(mark_unlisted(&db, "nvidia", &[]).unwrap(), 0);
        assert_eq!(mark_unlisted(&db, "hiveagents", &entries).unwrap(), 0);
    }

    #[test]
    fn a_model_from_a_provider_sync_is_discovered_so_the_typescript_hive_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = HiveDB::open(dir.path().join("hivedb")).unwrap();
        let entries = [json!({"wire_id":"a/new","model_type":"llm","capabilities":["chat"]})];
        // The reviewed catalog: `catalog` rows, which the TypeScript Hive recreates from its own seed.
        merge(&db, "nvidia", &entries, "src", "2026-11-13").unwrap();
        assert_eq!(
            db.col_get("models", "nvidia/a/new").unwrap().unwrap().doc["source"],
            "catalog"
        );
        // A provider's own list: `discovered`, which that app never wipes.
        let synced = [json!({"wire_id":"a/found","model_type":"llm","capabilities":["chat"]})];
        merge(
            &db,
            "nvidia",
            &synced,
            "https://api.example/models",
            "2026-11-16",
        )
        .unwrap();
        assert_eq!(
            db.col_get("models", "nvidia/a/found").unwrap().unwrap().doc["source"],
            "discovered"
        );
        // A row that already exists keeps the source it has.
        merge(
            &db,
            "nvidia",
            &synced,
            "https://api.example/models",
            "2026-11-17",
        )
        .unwrap();
        assert_eq!(
            db.col_get("models", "nvidia/a/found").unwrap().unwrap().doc["source"],
            "discovered"
        );
    }
    #[test]
    fn image_generation_is_never_a_vision_default_and_unknown_ids_are_not_guessed() {
        assert!(!supports(
            &json!({"model_type":"image","capabilities":["image_generation"]}),
            "vision"
        ));
        assert!(supports(
            &json!({"model_type":"llm","capabilities":"[\"vision\"]"}),
            "vision"
        ));
        let rows=discovered("openrouter",&json!({"data":[{"id":"new-tts-name"},{"id":"vendor/vision","architecture":{"input_modalities":["text","image"],"output_modalities":["text"]}}]})).unwrap();
        assert_eq!(rows[0]["model_type"], "unknown");
        assert!(supports(&rows[1], "vision"));
        assert_eq!(key("groq", "groq/compound"), "groq/groq/compound");
    }
}
