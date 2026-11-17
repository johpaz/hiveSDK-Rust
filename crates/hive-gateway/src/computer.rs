//! Desktop control for the coordinator (`computer_*` tools of the SDK).
//!
//! Controlling the desktop acts on every open application, so it is **off** until an administrator
//! turns it on, and screenshots and input are separate switches. Only the coordinator (depth 0) is
//! offered the tools, and only when its model can see images, because the loop is "look, act,
//! look again". Pointer and keyboard actions need a person's approval **once per conversation**,
//! which then holds for `session_minutes`, so nobody approves every click; the tool policy's
//! explicit `deny` rules still win. An administrator can stop everything at any moment
//! (`POST /api/computer/stop`): the action in progress is interrupted and no later one runs.
use crate::config::{self, App};
use hive_agent_core::tools::NativeTools;
use hivedb_core::PutOptions;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub(crate) const PREFIX: &str = "computer_";
const COLLECTION: &str = "computerUse";
const ID: &str = "settings";

/// The settings document: `{enabled, allow_input, backend, max_dimension, max_actions, session_minutes}`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Settings {
    pub enabled: bool,
    pub allow_input: bool,
    pub backend: String,
    pub max_dimension: u64,
    pub max_actions: u64,
    pub session_minutes: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_input: false,
            backend: "auto".into(),
            max_dimension: 1280,
            max_actions: 200,
            session_minutes: 10,
        }
    }
}

impl Settings {
    pub(crate) fn parse(doc: &Value) -> Result<Self, String> {
        let object = doc.as_object().ok_or("los ajustes deben ser un objeto")?;
        const KNOWN: [&str; 7] = [
            "id",
            "enabled",
            "allow_input",
            "backend",
            "max_dimension",
            "max_actions",
            "session_minutes",
        ];
        if let Some(extra) = object.keys().find(|k| !KNOWN.contains(&k.as_str())) {
            return Err(format!("ajuste desconocido: {extra}"));
        }
        let mut settings = Self::default();
        let flag = |key: &str, into: &mut bool| -> Result<(), String> {
            match object.get(key) {
                None => Ok(()),
                Some(Value::Bool(value)) => {
                    *into = *value;
                    Ok(())
                }
                Some(_) => Err(format!("{key} debe ser verdadero o falso")),
            }
        };
        flag("enabled", &mut settings.enabled)?;
        flag("allow_input", &mut settings.allow_input)?;
        let number = |key: &str,
                      into: &mut u64,
                      range: std::ops::RangeInclusive<u64>|
         -> Result<(), String> {
            match object.get(key) {
                None => Ok(()),
                Some(value) => match value.as_u64().filter(|n| range.contains(n)) {
                    Some(n) => {
                        *into = n;
                        Ok(())
                    }
                    None => Err(format!(
                        "{key} debe ser un entero entre {} y {}",
                        range.start(),
                        range.end()
                    )),
                },
            }
        };
        number("max_dimension", &mut settings.max_dimension, 320..=4096)?;
        number("max_actions", &mut settings.max_actions, 1..=10_000)?;
        number("session_minutes", &mut settings.session_minutes, 1..=120)?;
        if let Some(backend) = object.get("backend") {
            match backend.as_str() {
                Some(b @ ("auto" | "x11" | "portal")) => settings.backend = b.into(),
                _ => return Err("backend debe ser auto, x11 o portal".into()),
            }
        }
        Ok(settings)
    }

    fn to_json(&self) -> Value {
        json!({"enabled":self.enabled,"allow_input":self.allow_input,"backend":self.backend,
            "max_dimension":self.max_dimension,"max_actions":self.max_actions,"session_minutes":self.session_minutes})
    }
}

/// An input tool (pointer or keyboard); `computer_screenshot` and `computer_wait` are not.
pub(crate) fn is_input(tool: &str) -> bool {
    tool.starts_with(PREFIX) && !matches!(tool, "computer_screenshot" | "computer_wait")
}

impl App {
    pub(crate) fn computer_settings(&self) -> crate::Result<Settings> {
        match self.db.col_get(COLLECTION, ID)? {
            None => Ok(Settings::default()),
            // An unreadable document must not switch anything on.
            Some(row) => Ok(Settings::parse(&row.doc).unwrap_or_default()),
        }
    }

    /// The SDK tools for the current settings (built once per settings change), or none when off.
    pub(crate) fn computer_tools(&self) -> crate::Result<Option<NativeTools>> {
        let settings = self.computer_settings()?;
        let mut slot = self
            .computer
            .lock()
            .map_err(|_| "Estado de escritorio bloqueado")?;
        if !settings.enabled {
            if let Some((_, old)) = slot.take() {
                close_later(old);
            }
            return Ok(None);
        }
        let signature = settings.to_json().to_string();
        if let Some((current, tools)) = slot.as_ref()
            && *current == signature
        {
            return Ok(Some(tools.clone()));
        }
        let root = self.home.join("computer");
        std::fs::create_dir_all(&root)?;
        let tools = NativeTools::from_json(json!({"root":root,"computer":{
            "backend":settings.backend,"allow_screenshot":true,"allow_input":settings.allow_input,
            "max_dimension":settings.max_dimension,"max_actions":settings.max_actions,
            // The consent the desktop asked for is remembered, so a restart does not ask again.
            "restore_token_path":self.home.join("computer-portal-token")}}))
        .map_err(|e| format!("Ajustes de escritorio inválidos: {e}"))?;
        if let Some((_, old)) = slot.replace((signature, tools.clone())) {
            close_later(old);
        }
        Ok(Some(tools))
    }

    /// Tool calls a turn may need on top of its usual budget: one per allowed action.
    pub(crate) fn computer_extra_calls(&self) -> u32 {
        match self.computer_settings() {
            Ok(s) if s.enabled => u32::try_from(s.max_actions).unwrap_or(10_000),
            _ => 0,
        }
    }

    pub(crate) fn computer_grant_live(&self, user: &str, thread: &str) -> bool {
        let Ok(mut grants) = self.computer_grants.lock() else {
            return false;
        };
        let now = Instant::now();
        grants.retain(|_, until| *until > now);
        grants.contains_key(&format!("{user}\u{0}{thread}"))
    }

    pub(crate) fn computer_grant(&self, user: &str, thread: &str) {
        let minutes = self.computer_settings().map_or(10, |s| s.session_minutes);
        if let Ok(mut grants) = self.computer_grants.lock() {
            grants.insert(
                format!("{user}\u{0}{thread}"),
                Instant::now() + Duration::from_secs(minutes * 60),
            );
        }
    }
}

fn close_later(tools: NativeTools) {
    if let Ok(runtime) = tokio::runtime::Handle::try_current() {
        runtime.spawn(async move { tools.close().await });
    }
}

/// What to add to a coordinator's turn when it may control the desktop: the tool definitions, the
/// guidance for the model and the engine settings that turn screenshots into images.
pub(crate) struct Offer {
    pub tools: Vec<Value>,
    pub guidance: String,
}

pub(crate) fn offer(app: &App, model_row: &Value) -> crate::Result<Option<Offer>> {
    if !crate::model_catalog::supports(model_row, "vision") {
        return Ok(None);
    }
    let Some(tools) = app.computer_tools()? else {
        return Ok(None);
    };
    let definitions: Vec<Value> = tools
        .tool_definitions()
        .into_iter()
        .filter(|tool| tool.name.starts_with(PREFIX))
        .map(|tool| json!({"name":tool.name,"description":tool.description,"parameters":tool.parameters,"allow_repeated_results":true}))
        .collect();
    if definitions.is_empty() {
        return Ok(None);
    }
    Ok(Some(Offer {
        tools: definitions,
        guidance: "\nPuedes ver y manejar el escritorio de la persona con las herramientas computer_*. Empieza con computer_screenshot: las coordenadas de todas las demás herramientas son píxeles de esa imagen, y tras cada acción vuelve a capturar para comprobar que hizo lo esperado. Antes de algo difícil de deshacer (enviar, pagar, borrar, publicar) confirma con la persona. Cada acción de puntero o teclado necesita su aprobación; si te la niegan, no insistas. Si una acción devuelve COMPUTER_ACTION_UNCERTAIN, pudo haberse hecho a medias: mira la pantalla antes de continuar y no la repitas a ciegas.".into(),
    }))
}

/// Run one `computer_*` call and shape the result for the model. A screenshot keeps its base64
/// `data` here on purpose: the engine takes it out and shows it as an image.
pub(crate) async fn execute(
    app: &App,
    name: &str,
    arguments: Value,
    cancel: &hive_agent_core::provider::CancellationToken,
) -> Value {
    let tools = match app.computer_tools() {
        Ok(Some(tools)) => tools,
        Ok(None) => return json!({"ok":false,"error":"El control del escritorio está desactivado"}),
        Err(error) => return json!({"ok":false,"error":error.to_string()}),
    };
    match tools.execute_tool(name, arguments, cancel).await {
        Ok(mut value) => {
            if let Some(object) = value.as_object_mut() {
                object.entry("ok").or_insert(json!(true));
            }
            value
        }
        Err(error) => json!({"ok":false,"error":error.to_string()}),
    }
}

/// `GET/PUT /api/computer` and `POST /api/computer/{stop,resume,reset}` (administrators only).
pub(crate) fn route(
    app: &App,
    method: &str,
    path: &str,
    body: &[u8],
) -> crate::Result<Option<(u16, Value)>> {
    if !matches!(
        path,
        "/api/computer" | "/api/computer/stop" | "/api/computer/resume" | "/api/computer/reset"
    ) {
        return Ok(None);
    }
    let status = |app: &App| -> crate::Result<Value> {
        let settings = app.computer_settings()?;
        let tools = app.computer_tools()?;
        let live = match &tools {
            Some(tools) => tools.computer_status(),
            None => json!({"stopped":false,"actions":0}),
        };
        let vision = app
            .selection()
            .ok()
            .and_then(|s| {
                config::docs(&app.db, "models")
                    .ok()
                    .and_then(|models| models.into_iter().find(|m| m["id"] == s.model_id.as_str()))
            })
            .is_some_and(|row| crate::model_catalog::supports(&row, "vision"));
        Ok(
            json!({"settings":settings.to_json(),"stopped":live["stopped"],"actions":live["actions"],"model_has_vision":vision}),
        )
    };
    Ok(Some(match (method, path) {
        ("GET", "/api/computer") => (200, status(app)?),
        ("PUT", "/api/computer") => {
            let doc: Value = serde_json::from_slice(body)?;
            let settings = match Settings::parse(&doc) {
                Ok(settings) => settings,
                Err(error) => return Ok(Some((400, json!({"error":error})))),
            };
            let mut stored = settings.to_json();
            stored["id"] = json!(ID);
            let version = app.db.col_get(COLLECTION, ID)?.map_or(0, |row| row.version);
            app.db.col_put(
                COLLECTION,
                ID,
                &stored,
                PutOptions {
                    expected_version: Some(version),
                },
            )?;
            // Turning it off also ends every approval given so far.
            if !settings.enabled
                && let Ok(mut grants) = app.computer_grants.lock()
            {
                grants.clear();
            }
            (200, status(app)?)
        }
        ("POST", "/api/computer/stop") => {
            if let Ok(mut grants) = app.computer_grants.lock() {
                grants.clear();
            }
            if let Some(tools) = app.computer_tools()? {
                tools.computer_stop();
            }
            (200, status(app)?)
        }
        ("POST", "/api/computer/resume") => {
            if let Some(tools) = app.computer_tools()? {
                tools.computer_resume();
            }
            (200, status(app)?)
        }
        ("POST", "/api/computer/reset") => {
            if let Some(tools) = app.computer_tools()? {
                tools.computer_reset();
            }
            (200, status(app)?)
        }
        _ => (405, json!({"error":"Método no permitido"})),
    }))
}
