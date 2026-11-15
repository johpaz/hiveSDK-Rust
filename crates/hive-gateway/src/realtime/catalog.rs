use super::config::{models, selected};
use crate::{
    Result,
    config::{self, App},
};
use hivedb_core::ColOp;
use serde_json::{Value, json};

pub(crate) fn route(
    app: &App,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<Option<(u16, Value)>> {
    if path != "/api/realtime/models" {
        return Ok(None);
    }
    let _guard = app
        .activity_lock
        .lock()
        .map_err(|_| "Configuración Live bloqueada")?;
    if !matches!(method, "GET" | "POST") {
        return Ok(Some((405, json!({"error":"Método no permitido"}))));
    }
    let mut rows = models(app)?;
    if method == "POST" {
        let Ok(body) = serde_json::from_slice::<Value>(body) else {
            return Ok(Some((400, json!({"error":"Selección de modelo inválida"}))));
        };
        let Some(id) = body["modelId"]
            .as_str()
            .filter(|id| rows.iter().any(|row| row["id"] == *id))
        else {
            return Ok(Some((
                400,
                json!({"error":"Selecciona un modelo realtime habilitado de Gemini"}),
            )));
        };
        let mut ops = Vec::new();
        for row in config::docs(&app.db, "models")?
            .into_iter()
            .filter(|row| row["provider_id"] == "gemini" && row["model_type"] == "realtime")
        {
            let key = config::string(&row, "id")?;
            if let Some(mut stored) = app.db.col_get("models", key)? {
                stored.doc["active"] = json!(key == id);
                ops.push(ColOp::Put {
                    collection: "models".into(),
                    id: key.into(),
                    doc: stored.doc,
                    expected_version: Some(stored.version),
                });
            }
        }
        app.db.col_batch(&ops)?;
        rows = models(app)?;
    }
    Ok(Some((
        200,
        json!({"selectedId":selected(&rows).map(|row|&row["id"]),"models":rows.iter().map(|row|json!({"id":row["id"],"name":row["name"].as_str().unwrap_or_else(||row["id"].as_str().unwrap_or_default())})).collect::<Vec<_>>()}),
    )))
}

/// One-time migration requested for existing desktop profiles. Later explicit
/// selections remain authoritative, including across restarts.
pub(crate) fn ensure_default(db: &hivedb_core::HiveDB) -> Result<()> {
    const MARKER: &str = "hive_rust_gemini_38_live_default_v1";
    if db.col_get("config", MARKER)?.is_some() {
        return Ok(());
    }
    let mut ops = Vec::new();
    for row in db.col_scan("models", &Default::default())? {
        if row.doc["provider_id"] == "gemini"
            && row.doc["model_type"] == "realtime"
            && row.doc["id"] != "gemini-3.8-live"
        {
            let mut doc = row.doc;
            doc["active"] = json!(false);
            ops.push(ColOp::Put {
                collection: "models".into(),
                id: config::string(&doc, "id")?.into(),
                doc,
                expected_version: Some(row.version),
            });
        }
    }
    let old = db.col_get("models", "gemini-3.8-live")?;
    let mut doc=old.as_ref().map(|row|row.doc.clone()).unwrap_or_else(||json!({"id":"gemini-3.8-live","provider_id":"gemini","name":"Gemini 3.8 Live","model_type":"realtime","context_window":128000,"input_per_1m":3,"output_per_1m":12,"capabilities":"[\"realtime\",\"audio_in\",\"audio_out\",\"function_calling\",\"transcription\"]"}));
    doc["id"] = json!("gemini-3.8-live");
    doc["provider_id"] = json!("gemini");
    doc["model_type"] = json!("realtime");
    doc["name"] = json!("Gemini 3.8 Live");
    doc["enabled"] = json!(true);
    doc["active"] = json!(true);
    ops.push(ColOp::Put {
        collection: "models".into(),
        id: "gemini-3.8-live".into(),
        doc,
        expected_version: Some(old.map_or(0, |row| row.version)),
    });
    ops.push(ColOp::Put {
        collection: "config".into(),
        id: MARKER.into(),
        doc: json!({"id":MARKER,"model_id":"gemini-3.8-live","applied":true}),
        expected_version: Some(0),
    });
    db.col_batch(&ops)?;
    eprintln!("[HiveLive] Modelo predeterminado guardado en HiveDB: gemini-3.8-live");
    Ok(())
}
