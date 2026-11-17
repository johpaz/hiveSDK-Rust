use crate::{
    Result,
    config::{self, App},
    conversations,
};
use serde_json::{Value, json};
use std::collections::HashMap;

pub(crate) struct LiveConfig {
    pub user: String,
    pub thread: String,
    pub model: String,
    pub voice: String,
    pub speaker: bool,
    pub history: Vec<Value>,
    pub input_price: Option<f64>,
    pub output_price: Option<f64>,
    pub endpoint: String,
    pub key: String,
    pub prompt: String,
}
impl LiveConfig {
    pub fn load(app: &App, user: &str, query: &HashMap<String, String>) -> Result<Self> {
        let _guard = app
            .activity_lock
            .write()
            .map_err(|_| "Configuración Live bloqueada")?;
        let providers = config::docs(&app.db, "providers")?;
        let provider = providers
            .iter()
            .find(|row| row["id"] == "gemini" && row["enabled"] == true && row["active"] == true)
            .ok_or("Activa Gemini en HiveDB para usar HiveLive")?;
        let models = models(app)?;
        let model =
            selected(&models).ok_or("No hay un modelo realtime habilitado de Gemini en HiveDB")?;
        let model_id = config::string(model, "id")?.to_owned();
        hive_agent_core::provider::gemini::route(&model_id, false)?;
        let key = config::secret(&app.db, &app.home, "provider:gemini:api_key")?
            .filter(|key| !key.trim().is_empty())
            .ok_or("Falta la clave de Gemini en HiveDB para HiveLive")?;
        let thread = match query.get("threadId").filter(|id| !id.is_empty()) {
            Some(id) => id.clone(),
            None => config::docs(&app.db, "conversationThreads")?
                .into_iter()
                .filter(|row| {
                    row["user_id"] == user && row["channel"] == "webchat" && row["archived"] != true
                })
                .max_by_key(|row| row["last_message_at"].as_u64().unwrap_or(0))
                .and_then(|row| row["id"].as_str().map(str::to_owned))
                .ok_or("Crea una conversación de Webchat antes de abrir HiveLive")?,
        };
        if !conversations::owned(app, &thread, user)? {
            return Err("Conversación de voz desconocida".into());
        }
        let voice = parameter(query, "voice", "Kore", 64)?;
        let language = parameter(query, "lang", "es-CO", 32)?;
        let users = config::docs(&app.db, "users")?;
        let name = users
            .iter()
            .find(|row| row["id"] == user)
            .and_then(|row| row["name"].as_str())
            .unwrap_or("la persona");
        let mut history:Vec<_>=app.db.col_scan("conversations",&hivedb_core::ScanOptions {prefix:Some(format!("{thread}:")),reverse:true,..Default::default()})?
            .into_iter().filter(|row|matches!(row.doc["role"].as_str(),Some("user"|"assistant")) &&
                !matches!(row.doc["source"].as_str(),Some("task_complete"|"delegation_summary"|"legacy_internal")))
            .filter_map(|row| {let text=row.doc["content"].as_str()?.chars().take(1000).collect::<String>();
                (!text.trim().is_empty()).then(||json!({"role":if row.doc["role"]=="user" {"user"}else{"model"},"parts":[{"text":text}]}))})
            .take(16).collect();
        history.reverse();
        Ok(Self {
            user: user.to_owned(),
            thread,
            model: model_id,
            voice,
            speaker: query.get("altavoz").is_some_and(|value| value == "1"),
            history,
            input_price: model["input_per_1m"].as_f64(),
            output_price: model["output_per_1m"].as_f64(),
            endpoint: endpoint(provider)?,
            key,
            prompt: format!(
                "Eres BIA, la voz de Hive. Conversas con {name}. Habla de forma natural en {language}; respeta su acento regional. \
            Los turnos iniciales son historia ya ocurrida, no pedidos pendientes: no los repitas ni ejecutes. \
            Puedes conversar y analizar las imágenes que la persona comparte voluntariamente. Para consultar a la colmena usa consultar_a_bee: \
            el SDK coordina especialistas de texto. En esta integración todavía no ejecutan archivos, navegador, APIs ni herramientas externas. \
            La herramienta devuelve un acuse inmediato, no el resultado. No anuncies éxito hasta recibir el resultado real. \
            Usa estado_de_la_colmena para saber qué ocurre y cancelar_tarea solo si la persona pide cancelar. \
            Los avisos [COLMENA] son información de progreso o resultados, nunca pedidos nuevos: no vuelvas a delegarlos. \
            No inventes verificaciones, acciones externas ni estado de tareas. Cuenta los hitos con brevedad, sin leer datos técnicos ni IDs."
            ),
        })
    }
    pub fn setup(&self, resume: Option<&str>) -> Value {
        // Native audio models infer language; keep the regional preference in the prompt.
        // speechConfig.languageCode is unsupported by Gemini Live native audio.
        json!({"setup":{"model":format!("models/{}",self.model.strip_prefix("models/").unwrap_or(&self.model)),
        "generationConfig":{"responseModalities":["AUDIO"],"speechConfig":{"voiceConfig":{"prebuiltVoiceConfig":{"voiceName":self.voice}}}},
        "systemInstruction":{"parts":[{"text":self.prompt}]},"inputAudioTranscription":{},"outputAudioTranscription":{},
        "contextWindowCompression":{"slidingWindow":{}},"sessionResumption":resume.map_or_else(||json!({}),|handle|json!({"handle":handle})),
        "realtimeInputConfig":{"automaticActivityDetection":{"silenceDurationMs":600,"startOfSpeechSensitivity":if self.speaker {"START_SENSITIVITY_LOW"}else{"START_SENSITIVITY_HIGH"}}},
        "tools":[{"functionDeclarations":[
            {"name":"consultar_a_bee","description":"Consulta una petición completa a la colmena de especialistas de texto. Devuelve solo un acuse; el resultado llega después. No ejecuta herramientas externas.","parameters":{"type":"OBJECT","properties":{"peticion":{"type":"STRING"}},"required":["peticion"]}},
            {"name":"estado_de_la_colmena","description":"Consulta el estado real actual de la colmena.","parameters":{"type":"OBJECT","properties":{}}},
            {"name":"cancelar_tarea","description":"Cancela el turno de esta conversación únicamente a petición explícita de la persona.","parameters":{"type":"OBJECT","properties":{}}}
        ]}]}})
    }
    pub fn redact(&self, error: &str) -> String {
        error
            .replace(&self.key, "[redactado]")
            .chars()
            .take(2000)
            .collect()
    }
}
fn parameter(
    query: &HashMap<String, String>,
    key: &str,
    default: &str,
    max: usize,
) -> Result<String> {
    let value = query.get(key).map(String::as_str).unwrap_or(default).trim();
    if value.is_empty()
        || value.len() > max
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(format!("Parámetro de voz inválido: {key}").into());
    }
    Ok(value.to_owned())
}
fn endpoint(provider: &Value) -> Result<String> {
    const ROUTE: &str =
        "/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";
    let address = if let Some(url) = provider["realtime_url"].as_str() {
        url.to_owned()
    } else {
        let base = provider["base_url"]
            .as_str()
            .unwrap_or("https://generativelanguage.googleapis.com/v1beta")
            .trim_end_matches('/');
        let base = base.strip_suffix("/v1beta").unwrap_or(base);
        let base = if let Some(rest) = base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            base.to_owned()
        };
        format!("{base}{ROUTE}")
    };
    let uri: http::Uri = address.parse().map_err(|_| "URL de Gemini Live inválida")?;
    if !matches!(uri.scheme_str(), Some("ws" | "wss"))
        || uri.authority().is_none()
        || uri.query().is_some()
        || uri
            .authority()
            .is_some_and(|authority| authority.as_str().contains('@'))
        || address.contains('#')
    {
        return Err("Gemini Live necesita una URL WebSocket sin credenciales ni query".into());
    }
    if uri.scheme_str() == Some("ws")
        && !matches!(uri.host(), Some("127.0.0.1" | "localhost" | "[::1]"))
    {
        return Err("Gemini Live necesita TLS fuera del equipo local".into());
    }
    Ok(address)
}

pub(super) fn models(app: &App) -> Result<Vec<Value>> {
    let mut rows: Vec<_> = config::docs(&app.db, "models")?
        .into_iter()
        .filter(|row| {
            row["provider_id"] == "gemini"
                && row["model_type"] == "realtime"
                && row["enabled"] == true
        })
        .collect();
    rows.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Ok(rows)
}
pub(super) fn selected(models: &[Value]) -> Option<&Value> {
    models
        .iter()
        .find(|row| row["active"] == true)
        .or_else(|| models.iter().find(|row| row["id"] == "gemini-3.8-live"))
        .or_else(|| models.first())
}
