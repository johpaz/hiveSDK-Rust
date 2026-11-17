//! Reading scanned documents and pictures of text (OCR) for the coordinator: the `ocr_read` tool.
//!
//! It is **off** until an administrator turns it on (the OCR tab of the desktop app, or
//! `PUT /api/ocr`), reads only files **inside one folder** chosen in the settings, and is offered only
//! to the coordinator. What it does, in this order, and what it says it did:
//!
//! 1. A PDF that already has text is read from its text layer: free, and nothing leaves the machine.
//! 2. Otherwise the file is sent to the provider and model chosen in the settings:
//!    - a model with the `ocr` capability and a dedicated endpoint (Mistral OCR: `POST /ocr`, pages as
//!      Markdown), or
//!    - any model that can see (`vision`): the PDF as a document (Anthropic, Gemini, OpenAI-style
//!      providers that take files) or the picture as an image, with an instruction to transcribe it
//!      literally and to mark what it cannot read.
//!
//! Sending a document out is a decision, so by default a person approves it once per conversation
//! (`ask`), and the result always names the method and the provider that read it. Nothing is guessed:
//! pages that could not be read are listed, and a scanned PDF with no provider configured is an error.
use crate::config::{self, App};
use hive_agent_core::{
    ModelRequest,
    provider::{CancellationToken, ModelEvent},
    tools::NativeTools,
};
use hivedb_core::PutOptions;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

pub(crate) const TOOL: &str = "ocr_read";
const COLLECTION: &str = "ocr";
const ID: &str = "settings";
const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;
const MAX_RESULT_CHARS: usize = 120_000;
const EXTENSIONS: [&str; 6] = ["pdf", "png", "jpg", "jpeg", "webp", "gif"];

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Settings {
    pub enabled: bool,
    /// Absolute folder the tool may read from.
    pub folder: String,
    pub provider_id: String,
    pub model_id: String,
    /// Approve once per conversation before a document is sent out.
    pub ask: bool,
    pub session_minutes: u64,
    /// Most pages read in one call.
    pub max_pages: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            folder: String::new(),
            provider_id: String::new(),
            model_id: String::new(),
            ask: true,
            session_minutes: 10,
            max_pages: 50,
        }
    }
}

impl Settings {
    pub(crate) fn parse(doc: &Value) -> Result<Self, String> {
        let object = doc.as_object().ok_or("los ajustes deben ser un objeto")?;
        const KNOWN: [&str; 8] = [
            "id",
            "enabled",
            "folder",
            "provider_id",
            "model_id",
            "ask",
            "session_minutes",
            "max_pages",
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
        flag("ask", &mut settings.ask)?;
        let text = |key: &str, max: usize| -> Result<String, String> {
            match object.get(key) {
                None | Some(Value::Null) => Ok(String::new()),
                Some(Value::String(text))
                    if text.len() <= max && !text.chars().any(char::is_control) =>
                {
                    Ok(text.trim().to_owned())
                }
                Some(_) => Err(format!("{key} no es un texto válido")),
            }
        };
        settings.folder = text("folder", 1024)?;
        settings.provider_id = text("provider_id", 128)?;
        settings.model_id = text("model_id", 256)?;
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
        number("session_minutes", &mut settings.session_minutes, 1..=120)?;
        number("max_pages", &mut settings.max_pages, 1..=200)?;
        if !settings.folder.is_empty() && !Path::new(&settings.folder).is_absolute() {
            return Err("folder debe ser una ruta absoluta".into());
        }
        if settings.enabled {
            if settings.folder.is_empty() || !Path::new(&settings.folder).is_dir() {
                return Err("para activar el OCR elige una carpeta que exista".into());
            }
            if settings.provider_id.is_empty() || settings.model_id.is_empty() {
                return Err("para activar el OCR elige un proveedor y un modelo".into());
            }
        }
        Ok(settings)
    }

    fn to_json(&self) -> Value {
        json!({"enabled":self.enabled,"folder":self.folder,"provider_id":self.provider_id,"model_id":self.model_id,
            "ask":self.ask,"session_minutes":self.session_minutes,"max_pages":self.max_pages})
    }
}

/// A provider and model that could read documents, as the settings screen offers them.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Candidate {
    pub provider_id: String,
    pub model_id: String,
    pub name: String,
    /// `ocr` (a dedicated endpoint) or `vision` (a chat model that can see).
    pub kind: &'static str,
    pub supported: bool,
    /// Whether it can read a PDF as such. Otherwise it reads pictures only (there is no page renderer here).
    pub documents: bool,
    pub note: Option<&'static str>,
}

/// Providers whose adapters take a PDF as a document attachment.
fn takes_documents(provider_id: &str) -> bool {
    matches!(provider_id, "gemini" | "anthropic" | "openai" | "mistral")
}

/// Enabled models of enabled providers (with a key, unless local) that can read pictures of text.
pub(crate) fn candidates(app: &App) -> Vec<Candidate> {
    let providers = config::docs(&app.db, "providers").unwrap_or_default();
    let models = config::docs(&app.db, "models").unwrap_or_default();
    let mut out = vec![];
    for model in &models {
        let (Some(provider_id), Some(model_id)) =
            (model["provider_id"].as_str(), model["id"].as_str())
        else {
            continue;
        };
        if model["enabled"] != true {
            continue;
        }
        let Some(provider) = providers
            .iter()
            .find(|p| p["id"] == provider_id && p["enabled"] == true && p["active"] == true)
        else {
            continue;
        };
        // The same test the providers page uses: a stored key, without decrypting it just to list.
        let has_key = app
            .db
            .col_get("secrets", &format!("provider:{provider_id}:api_key"))
            .ok()
            .flatten()
            .is_some();
        if !has_key && provider_id != "ollama" {
            continue;
        }
        let _ = provider;
        let capabilities = crate::model_catalog::capabilities(model);
        let documents = takes_documents(provider_id);
        let (kind, supported, note) = if capabilities.iter().any(|c| c == "ocr") {
            if provider_id == "mistral" {
                ("ocr", true, None)
            } else {
                (
                    "ocr",
                    false,
                    Some("Este proveedor de OCR aún no tiene adaptador"),
                )
            }
        } else if crate::model_catalog::supports(model, "vision") {
            (
                "vision",
                true,
                (!documents).then_some(
                    "Lee imágenes: un PDF escaneado se lee página a página (una llamada por página). Gemini, Anthropic u OpenAI leen el PDF entero",
                ),
            )
        } else {
            continue;
        };
        out.push(Candidate {
            provider_id: provider_id.to_owned(),
            model_id: model_id.to_owned(),
            name: model["name"].as_str().unwrap_or(model_id).to_owned(),
            kind,
            supported,
            documents,
            note,
        });
    }
    out.sort_by(|a, b| {
        (a.kind != "ocr", &a.provider_id, &a.model_id).cmp(&(
            b.kind != "ocr",
            &b.provider_id,
            &b.model_id,
        ))
    });
    out
}

impl App {
    pub(crate) fn ocr_settings(&self) -> crate::Result<Settings> {
        match self.db.col_get(COLLECTION, ID)? {
            None => Ok(Settings::default()),
            // An unreadable document must not switch anything on.
            Some(row) => Ok(Settings::parse(&row.doc).unwrap_or_default()),
        }
    }

    pub(crate) fn ocr_grant_live(&self, user: &str, thread: &str) -> bool {
        let Ok(mut grants) = self.ocr_grants.lock() else {
            return false;
        };
        let now = Instant::now();
        grants.retain(|_, until| *until > now);
        grants.contains_key(&format!("{user}\u{0}{thread}"))
    }

    pub(crate) fn ocr_grant(&self, user: &str, thread: &str) {
        let minutes = self.ocr_settings().map_or(10, |s| s.session_minutes);
        if let Ok(mut grants) = self.ocr_grants.lock() {
            grants.insert(
                format!("{user}\u{0}{thread}"),
                Instant::now() + Duration::from_secs(minutes * 60),
            );
        }
    }
}

/// «Mistral (mistral-ocr-4-1)», for the question a person is asked.
pub(crate) fn provider_label(app: &App) -> String {
    match app.ocr_settings() {
        Ok(s) if !s.provider_id.is_empty() => format!("{} ({})", s.provider_id, s.model_id),
        _ => "el proveedor configurado".into(),
    }
}

/// The configured candidate, if it is still one (its key, provider and model are still in place).
fn chosen(app: &App, settings: &Settings) -> Option<Candidate> {
    candidates(app).into_iter().find(|c| {
        c.provider_id == settings.provider_id && c.model_id == settings.model_id && c.supported
    })
}

/// The tool and the guidance for the coordinator, when OCR is on and usable.
pub(crate) struct Offer {
    pub tool: Value,
    pub guidance: String,
}

pub(crate) fn offer(app: &App) -> Option<Offer> {
    let settings = app.ocr_settings().ok().filter(|s| s.enabled)?;
    if !Path::new(&settings.folder).is_dir() || chosen(app, &settings).is_none() {
        return None;
    }
    Some(Offer {
        tool: json!({"name":TOOL,"description":format!(
            "Lee el texto de un documento escaneado o de una imagen con texto (OCR). `path` es relativo a la carpeta de documentos; acepta PDF, png, jpg, webp y gif. Un PDF que ya tiene texto se lee sin enviarlo a ningún sitio; si no, se envía al proveedor de OCR configurado y el resultado dice qué método usó. Hasta {} páginas por llamada (`first_page`, `last_page`, desde 1).", settings.max_pages),
            "parameters":{"type":"object","additionalProperties":false,"required":["path"],"properties":{
                "path":{"type":"string","minLength":1,"maxLength":1024},
                "first_page":{"type":"integer","minimum":1,"maximum":100000},
                "last_page":{"type":"integer","minimum":1,"maximum":100000}}},
            "allow_repeated_results":false}),
        guidance: "\nPuedes leer documentos escaneados e imágenes con texto con la herramienta ocr_read (solo archivos de la carpeta de documentos). Cita de qué método y proveedor salió el texto cuando importe, y si una página no se pudo leer, dilo en lugar de suponerla.".into(),
    })
}

fn clip(text: &str) -> (String, bool) {
    match text.char_indices().nth(MAX_RESULT_CHARS) {
        Some((at, _)) => (format!("{}…", &text[..at]), true),
        None => (text.to_owned(), false),
    }
}

/// Run one call. Never fails the turn: a problem is a result the model can read.
pub(crate) async fn execute(app: &Arc<App>, arguments: Value, cancel: &CancellationToken) -> Value {
    match tokio::select! {
        biased;
        _ = cancel.cancelled() => Err("cancelado".to_owned()),
        result = run(app, &arguments) => result,
    } {
        Ok(value) => value,
        Err(error) => json!({"ok":false,"error":error}),
    }
}

/// The file inside the folder, or why it cannot be read: never outside it, never a link out of it.
fn resolve(folder: &str, relative: &str) -> Result<(PathBuf, PathBuf), String> {
    if relative.is_empty() || relative.contains('\0') {
        return Err("path no es válido".into());
    }
    let root = std::fs::canonicalize(folder)
        .map_err(|_| "la carpeta de documentos no existe".to_owned())?;
    let target = std::fs::canonicalize(root.join(relative.trim_start_matches('/')))
        .map_err(|_| "el archivo no existe en la carpeta de documentos".to_owned())?;
    if !target.starts_with(&root) {
        return Err("el archivo está fuera de la carpeta de documentos".into());
    }
    let meta = std::fs::metadata(&target).map_err(|_| "no se pudo leer el archivo".to_owned())?;
    if !meta.is_file() {
        return Err("path no es un archivo".into());
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "el archivo pesa más de {} MB",
            MAX_FILE_BYTES / 1024 / 1024
        ));
    }
    let extension = target
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default();
    if !EXTENSIONS.contains(&extension.as_str()) {
        return Err("solo se leen PDF, png, jpg, webp y gif".into());
    }
    Ok((root, target))
}

async fn run(app: &Arc<App>, arguments: &Value) -> Result<Value, String> {
    let settings = app.ocr_settings().map_err(|e| e.to_string())?;
    if !settings.enabled {
        return Err("La lectura de documentos (OCR) está desactivada".into());
    }
    let relative = arguments["path"].as_str().ok_or("falta path")?.to_owned();
    let folder = settings.folder.clone();
    let (root, target) = tokio::task::spawn_blocking({
        let relative = relative.clone();
        move || resolve(&folder, &relative)
    })
    .await
    .map_err(|_| "tarea interrumpida".to_owned())??;
    let first = arguments["first_page"].as_u64();
    let last = arguments["last_page"].as_u64();
    if let (Some(first), Some(last)) = (first, last)
        && last < first
    {
        return Err("last_page no puede ser menor que first_page".into());
    }
    if let (Some(first), Some(last)) = (first, last)
        && last - first + 1 > settings.max_pages
    {
        return Err(format!(
            "se pueden leer como mucho {} páginas por llamada",
            settings.max_pages
        ));
    }
    let bytes = tokio::task::spawn_blocking({
        let target = target.clone();
        move || std::fs::read(target)
    })
    .await
    .map_err(|_| "tarea interrumpida".to_owned())?
    .map_err(|_| "no se pudo leer el archivo".to_owned())?;
    let is_pdf = bytes.starts_with(b"%PDF");
    let image_type =
        hive_agent_core::image::detect_mime(&bytes).filter(|m| m.starts_with("image/"));
    if !is_pdf && image_type.is_none() {
        return Err("el contenido no es un PDF ni una imagen reconocible".into());
    }

    // 1. A PDF with a text layer: read in place, nothing is sent anywhere.
    let mut warnings: Vec<String> = vec![];
    if is_pdf {
        let relative_in_root = target
            .strip_prefix(&root)
            .map_err(|_| "ruta inválida".to_owned())?
            .to_string_lossy()
            .into_owned();
        let tools =
            NativeTools::from_json(json!({"root":root,"office":{}})).map_err(|e| e.to_string())?;
        let mut read_arguments = json!({"path":relative_in_root});
        if let Some(first) = first {
            read_arguments["start_page"] = json!(first);
        }
        if let Some(last) = last.or(first.map(|f| f + settings.max_pages - 1)) {
            read_arguments["end_page"] = json!(last);
        } else {
            read_arguments["end_page"] = json!(settings.max_pages);
        }
        match tools
            .execute_tool("office_read_pdf", read_arguments, &CancellationToken::new())
            .await
        {
            Ok(read) => {
                let pages: Vec<&Value> = read["pages"]
                    .as_array()
                    .map(|p| p.iter().collect())
                    .unwrap_or_default();
                let blank = pages
                    .iter()
                    .filter(|p| p["text"].as_str().is_none_or(|t| t.trim().is_empty()))
                    .count();
                if !pages.is_empty() && blank == 0 {
                    let text: String = pages
                        .iter()
                        .map(|p| p["text"].as_str().unwrap_or_default())
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    let (text, truncated) = clip(&text);
                    return Ok(
                        json!({"ok":true,"method":"text_layer","sent_out":false,"total_pages":read["total_pages"],
                        "pages":pages.iter().map(|p| json!({"page":p["number"],"text":p["text"]})).collect::<Vec<_>>(),"text":text,"truncated":truncated,
                        "warnings":read["warnings"]}),
                    );
                }
                warnings.push(format!(
                    "{blank} de {} páginas no tienen texto: parece un escaneo",
                    pages.len()
                ));
            }
            Err(error) => warnings.push(format!("no se pudo leer la capa de texto ({error})")),
        }
    }

    // 2. A provider reads it.
    let chosen = chosen(app, &settings).ok_or_else(|| {
        "Este documento necesita OCR y el proveedor elegido en la pestaña OCR ya no está disponible (clave, proveedor o modelo)".to_owned()
    })?;
    let (kind, provider_id, model_id) = (
        chosen.kind,
        chosen.provider_id.clone(),
        chosen.model_id.clone(),
    );
    let mut result = if is_pdf && kind != "ocr" && !chosen.documents {
        // The provider reads pictures, not PDF: the scan's own page pictures are shown to it one by one.
        read_scanned_pages(
            app,
            &provider_id,
            &model_id,
            &bytes,
            first,
            last,
            settings.max_pages,
        )
        .await?
    } else if kind == "ocr" {
        read_with_ocr_endpoint(
            app,
            &provider_id,
            &model_id,
            &bytes,
            is_pdf,
            image_type,
            first,
            last,
            settings.max_pages,
        )
        .await?
    } else {
        if first.is_some() || last.is_some() {
            warnings.push(
                "este método lee el documento entero: first_page y last_page no se aplicaron"
                    .into(),
            );
        }
        read_with_vision(app, &provider_id, &model_id, &bytes, is_pdf, image_type).await?
    };
    result["ok"] = json!(true);
    result["sent_out"] = json!(true);
    result["provider"] = json!(provider_id);
    result["model"] = json!(model_id);
    if !warnings.is_empty() {
        result["warnings"] = json!(warnings);
    }
    Ok(result)
}

fn data_uri(mime: &str, bytes: &[u8]) -> String {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    format!("data:{mime};base64,{}", STANDARD.encode(bytes))
}

fn provider_row(app: &App, provider_id: &str, model_id: &str) -> Result<(Value, Value), String> {
    let provider = config::docs(&app.db, "providers")
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|p| p["id"] == provider_id)
        .ok_or("proveedor desconocido")?;
    let model = config::docs(&app.db, "models")
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|m| m["id"] == model_id)
        .ok_or("modelo desconocido")?;
    Ok((provider, model))
}

/// Mistral OCR: `POST {base}/ocr`, the document as a data URI, pages back as Markdown.
#[allow(clippy::too_many_arguments)]
async fn read_with_ocr_endpoint(
    app: &Arc<App>,
    provider_id: &str,
    model_id: &str,
    bytes: &[u8],
    is_pdf: bool,
    image_type: Option<&'static str>,
    first: Option<u64>,
    last: Option<u64>,
    max_pages: u64,
) -> Result<Value, String> {
    let (provider, model) = provider_row(app, provider_id, model_id)?;
    let base = config::provider_endpoint(
        provider_id,
        provider["base_url"]
            .as_str()
            .or(Some("https://api.mistral.ai/v1")),
    )
    .map_err(|e| e.to_string())?;
    let key = config::secret(
        &app.db,
        &app.home,
        &format!("provider:{provider_id}:api_key"),
    )
    .map_err(|e| e.to_string())?
    .ok_or("falta la clave del proveedor")?;
    let wire = model["wire_id"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| model_id.rsplit('/').next().unwrap_or(model_id).to_owned());
    let document = if is_pdf {
        json!({"type":"document_url","document_url":data_uri("application/pdf", bytes)})
    } else {
        json!({"type":"image_url","image_url":data_uri(image_type.unwrap_or("image/png"), bytes)})
    };
    let mut body = json!({"model":wire,"document":document,"include_image_base64":false});
    if is_pdf {
        // The endpoint counts pages from zero.
        let start = first.unwrap_or(1).saturating_sub(1);
        let end = last
            .map_or(start + max_pages - 1, |l| l - 1)
            .min(start + max_pages - 1);
        body["pages"] = json!((start..=end).collect::<Vec<u64>>());
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(180))
        .build()
        .map_err(|_| "no se pudo crear el cliente HTTP")?;
    let response = client
        .post(format!("{}/ocr", base.trim_end_matches('/')))
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                "el proveedor de OCR tardó demasiado".to_owned()
            } else {
                "no se pudo contactar con el proveedor de OCR".to_owned()
            }
        })?;
    let status = response.status();
    if !status.is_success() {
        // The provider's text is not repeated: it can carry anything.
        return Err(format!(
            "el proveedor de OCR respondió HTTP {}",
            status.as_u16()
        ));
    }
    let bytes = crate::integrations::limited(response, 16 * 1024 * 1024)
        .await
        .map_err(|e| e.to_string())?;
    let reply: Value =
        serde_json::from_slice(&bytes).map_err(|_| "respuesta del proveedor de OCR ilegible")?;
    let pages = reply["pages"]
        .as_array()
        .ok_or("el proveedor de OCR no devolvió páginas")?;
    let numbered: Vec<Value> = pages
        .iter()
        .map(|p| json!({"page":p["index"].as_u64().map_or(0, |i| i + 1),"text":p["markdown"]}))
        .collect();
    let joined: String = pages
        .iter()
        .map(|p| p["markdown"].as_str().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n\n");
    let blank: Vec<u64> = pages
        .iter()
        .filter(|p| p["markdown"].as_str().is_none_or(|t| t.trim().is_empty()))
        .filter_map(|p| p["index"].as_u64().map(|i| i + 1))
        .collect();
    let (text, truncated) = clip(&joined);
    let mut out = json!({"method":"ocr_endpoint","format":"markdown","pages":numbered,"text":text,"truncated":truncated,"pages_processed":reply["usage_info"]["pages_processed"]});
    if !blank.is_empty() {
        out["unread_pages"] = json!(blank);
    }
    Ok(out)
}

/// A model that can see: the document or picture as an attachment, with a literal-transcription instruction.
/// One request to a vision model with one attachment; the text it answers, or a plain error.
async fn ask_vision(
    app: &Arc<App>,
    provider_id: &str,
    model_id: &str,
    attachment: Value,
    prompt: &str,
) -> Result<String, String> {
    let (provider_row, model_row) = provider_row(app, provider_id, model_id)?;
    let native = app
        .build_provider(provider_id, &provider_row, &model_row)
        .map_err(|e| e.to_string())?;
    let request: ModelRequest = serde_json::from_value(json!({
        "model":config::wire_model(provider_id, model_id),
        "messages":[{"role":"user","content":prompt,"attachments":[attachment]}],
        "tools":[]}))
    .map_err(|e| e.to_string())?;
    // Streamed: a long transcription arrives as it is written, and every provider supports it.
    let events = native.start(request, true);
    let mut content = None;
    loop {
        match events.next().await {
            Ok(Some(ModelEvent::Reply { reply })) => {
                content = reply.content;
                break;
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            // The provider's own words are not repeated.
            Err(error) => {
                return Err(format!(
                    "el modelo de visión no pudo leer el documento ({})",
                    error.to_string().chars().take(60).collect::<String>()
                ));
            }
        }
    }
    content
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| "el modelo no devolvió texto".to_owned())
}

const TRANSCRIBE: &str = "Transcribe literalmente todo el texto del documento, en orden de lectura, sin resumir, traducir ni corregir. Conserva las tablas como tablas de Markdown. Separa cada página con una línea `--- página N ---`. Si algo es ilegible, escribe [ilegible] en su lugar: no lo adivines. Responde solo con la transcripción.";

async fn read_with_vision(
    app: &Arc<App>,
    provider_id: &str,
    model_id: &str,
    bytes: &[u8],
    is_pdf: bool,
    image_type: Option<&'static str>,
) -> Result<Value, String> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let attachment = if is_pdf {
        json!({"kind":"document","media_type":"application/pdf","data":STANDARD.encode(bytes),"name":"documento.pdf"})
    } else {
        json!({"kind":"image","media_type":image_type.unwrap_or("image/png"),"data":STANDARD.encode(bytes)})
    };
    let text = ask_vision(app, provider_id, model_id, attachment, TRANSCRIBE).await?;
    let (text, truncated) = clip(&text);
    Ok(
        json!({"method":if is_pdf { "vision_document" } else { "vision_image" },"format":"text","text":text,"truncated":truncated}),
    )
}

/// A scanned PDF for a model that reads pictures only: the picture each page holds is extracted from the
/// PDF (nothing is rendered) and read on its own, one request per page. Pages with no extractable picture
/// or that the model could not read are listed in `unread_pages`, never filled in.
async fn read_scanned_pages(
    app: &Arc<App>,
    provider_id: &str,
    model_id: &str,
    bytes: &[u8],
    first: Option<u64>,
    last: Option<u64>,
    max_pages: u64,
) -> Result<Value, String> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    const MAX_PICTURE: usize = 15 * 1024 * 1024;
    let from = first.unwrap_or(1).max(1);
    let to = last
        .unwrap_or(from + max_pages - 1)
        .min(from + max_pages - 1);
    let owned = bytes.to_vec();
    let found = tokio::task::spawn_blocking(move || {
        hive_agent_core::tools::pdf_page_images(&owned, from as usize, to as usize, None, 30_000)
    })
    .await
    .map_err(|_| "tarea interrumpida".to_owned())?
    .map_err(|e| format!("no se pudieron extraer las páginas del PDF ({e})"))?;
    let mut warnings = found.warnings.clone();
    if found.images.is_empty() {
        return Err(format!(
            "este PDF no tiene texto y {provider_id} solo lee imágenes, pero no se pudo extraer ninguna página como imagen ({}): usa Gemini, Anthropic, OpenAI u un OCR dedicado",
            if warnings.is_empty() {
                "sin detalle".to_owned()
            } else {
                warnings.join("; ")
            }
        ));
    }
    let mut pages = vec![];
    let mut unread: Vec<u64> = vec![];
    let requested: Vec<u64> = (from..=to.min(found.total_pages as u64)).collect();
    for image in &found.images {
        let page = image.page as u64;
        if image.data.len() > MAX_PICTURE {
            warnings.push(format!("página {page}: la imagen pesa demasiado"));
            continue;
        }
        let attachment = json!({"kind":"image","media_type":image.media_type,"data":STANDARD.encode(&image.data)});
        let prompt = format!(
            "Transcribe literalmente todo el texto de esta página ({page}), en orden de lectura, sin resumir, traducir ni corregir. Conserva las tablas como tablas de Markdown. Si algo es ilegible, escribe [ilegible] en su lugar: no lo adivines. Responde solo con la transcripción."
        );
        match ask_vision(app, provider_id, model_id, attachment, &prompt).await {
            Ok(text) => pages.push((page, text)),
            Err(error) => warnings.push(format!("página {page}: {error}")),
        }
    }
    for page in requested {
        if !pages.iter().any(|(p, _)| *p == page) {
            unread.push(page);
        }
    }
    if pages.is_empty() {
        return Err(format!(
            "ninguna página se pudo leer: {}",
            warnings.join("; ")
        ));
    }
    let joined = pages
        .iter()
        .map(|(page, text)| format!("--- página {page} ---\n{}", text.trim()))
        .collect::<Vec<_>>()
        .join("\n\n");
    let (text, truncated) = clip(&joined);
    let mut result = json!({"method":"vision_page_images","format":"text","text":text,"truncated":truncated,
        "pages":pages.iter().map(|(page, text)| json!({"page":page,"text":text.trim()})).collect::<Vec<_>>(),
        "total_pages":found.total_pages});
    if !unread.is_empty() {
        result["unread_pages"] = json!(unread);
    }
    if !warnings.is_empty() {
        result["warnings"] = json!(warnings);
    }
    Ok(result)
}

/// `GET/PUT /api/ocr` (administrators only): the settings and the providers that could read documents.
pub(crate) fn route(
    app: &App,
    method: &str,
    path: &str,
    body: &[u8],
) -> crate::Result<Option<(u16, Value)>> {
    if path != "/api/ocr" {
        return Ok(None);
    }
    let status = |app: &App| -> crate::Result<Value> {
        let settings = app.ocr_settings()?;
        let list = candidates(app);
        let ready = settings.enabled
            && Path::new(&settings.folder).is_dir()
            && chosen(app, &settings).is_some();
        Ok(json!({"settings":settings.to_json(),"ready":ready,
            "candidates":list.iter().map(|c| json!({"provider_id":c.provider_id,"model_id":c.model_id,"name":c.name,"kind":c.kind,"supported":c.supported,"documents":c.documents,"note":c.note})).collect::<Vec<_>>()}))
    };
    Ok(Some(match method {
        "GET" => (200, status(app)?),
        "PUT" => {
            let doc: Value = serde_json::from_slice(body)?;
            let settings = match Settings::parse(&doc) {
                Ok(settings) => settings,
                Err(error) => return Ok(Some((400, json!({"error":error})))),
            };
            // The chosen provider and model must be ones that can do the job.
            if settings.enabled && chosen(app, &settings).is_none() {
                return Ok(Some((
                    400,
                    json!({"error":"el proveedor y el modelo elegidos no sirven para OCR (necesitan clave, estar activos y poder leer imágenes)"}),
                )));
            }
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
            if !settings.enabled
                && let Ok(mut grants) = app.ocr_grants.lock()
            {
                grants.clear();
            }
            (200, status(app)?)
        }
        _ => (405, json!({"error":"Método no permitido"})),
    }))
}
