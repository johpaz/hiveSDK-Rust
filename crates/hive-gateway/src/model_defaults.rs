//! Purpose-specific choices; independent of coordinator and Live model activation.
use crate::{
    Result,
    config::{self, App},
    management, model_catalog,
};
use hivedb_core::PutOptions;
use serde_json::{Value, json};
fn config_id(app: &App) -> Result<String> {
    Ok(format!("model_defaults:{}", management::owner(app)?))
}
pub(crate) fn get(app: &App) -> Result<Value> {
    Ok(app
        .db
        .col_get("config", &config_id(app)?)?
        .map(|r| r.doc)
        .unwrap_or_else(|| json!({})))
}
pub(crate) fn set(app: &App, id: &str, purpose: &str) -> Result<()> {
    let row = app.db.col_get("models", id)?.ok_or("Modelo desconocido")?;
    if !model_catalog::supports(&row.doc, purpose) {
        return Err(
            "El modelo no admite este uso (visión, transcripción o síntesis de voz)".into(),
        );
    }
    if row.doc["enabled"] != true {
        return Err("Habilita el modelo primero".into());
    }
    let provider = config::string(&row.doc, "provider_id")?;
    let p = app
        .db
        .col_get("providers", provider)?
        .ok_or("Proveedor desconocido")?;
    if p.doc["active"] != true || p.doc["enabled"] != true {
        return Err("Activa el proveedor primero".into());
    }
    let cid = config_id(app)?;
    let old = app.db.col_get("config", &cid)?;
    let mut doc = old
        .as_ref()
        .map(|r| r.doc.clone())
        .unwrap_or_else(|| json!({"id":cid}));
    doc[format!("{purpose}_model_id")] = json!(id);
    app.db.col_put(
        "config",
        &cid,
        &doc,
        PutOptions {
            expected_version: Some(old.map_or(0, |r| r.version)),
        },
    )?;
    // Canonical per-channel keys used by Hive's multimodal adapters. Preserve other settings.
    let user = management::owner(app)?;
    for channel in config::docs(&app.db, "channels")?
        .into_iter()
        .filter(|c| c["type"] == "webchat" && management::owned(c, &user))
    {
        let cid = config::string(&channel, "id")?;
        let secret_id = format!("channel:{cid}:config");
        let mut cfg = if let Some(raw) = config::secret(&app.db, &app.home, &secret_id)? {
            serde_json::from_str::<Value>(&raw)?
        } else {
            channel["config"]
                .as_str()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or_else(|| channel["config"].clone())
        };
        if !cfg.is_object() {
            cfg = json!({});
        }
        if purpose == "vision" {
            cfg["vision_provider"] = json!(provider);
            cfg["vision_model_id"] = json!(id);
            cfg["vision_enabled"] = json!(true);
        } else {
            cfg[format!("{purpose}_provider")] = json!(provider);
            cfg[format!("{purpose}_model_id")] = json!(id);
        }
        config::store_secret(app, &secret_id, &cfg.to_string())?;
    }
    Ok(())
}
pub(crate) fn annotate(app: &App, models: &mut [Value]) -> Result<()> {
    let defaults = get(app)?;
    for model in models {
        model["default_for"] = json!(
            ["vision", "stt", "tts"]
                .into_iter()
                .filter(|p| defaults[format!("{p}_model_id")] == model["id"])
                .collect::<Vec<_>>()
        );
    }
    Ok(())
}
