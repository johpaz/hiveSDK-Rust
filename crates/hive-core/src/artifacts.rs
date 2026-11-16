//! Bounded artifact store: bytes on disk, metadata in HiveDB, scoped per tenant.
//!
//! An artifact is an opaque blob an agent produced or received (an image, a
//! generated document, an oversized tool result). Files live under
//! `<root>/artifacts/<UTC day>/<id>.<ext>` where the id is generated and the
//! extension comes from a fixed list, so no caller-supplied text ever reaches a
//! path. Metadata records the SHA-256, which is re-checked on every read. Expiry
//! is explicit: nothing is deleted until the host calls [`ArtifactStore::expire`].
use crate::{Error, Result, Tool, image, media, sessions::SessionStore};
use hivedb_core::{HiveDB, PutOptions, ScanOptions};
use ring::digest;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

const COLLECTION: &str = "hive_sdk_rust_artifacts_v1";
/// Seven days, like the reference; `expires_in_ms: null` keeps an artifact forever.
pub const DEFAULT_RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;
pub const MAX_ARTIFACT_BYTES: usize = 25 * 1024 * 1024;
const MAX_PAGE: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub id: String,
    pub kind: String,
    pub mime_type: String,
    pub size: u64,
    pub sha256: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub run_id: Option<String>,
    pub task_id: Option<String>,
    /// `active`, `expired` or `deleted`.
    pub status: String,
    pub created_at: u64,
    pub expires_at: Option<u64>,
    pub expired_at: Option<u64>,
    /// Relative to the store root.
    path: String,
}

#[derive(Debug, Default)]
pub struct NewArtifact {
    pub kind: String,
    pub mime_type: String,
    pub run_id: Option<String>,
    pub task_id: Option<String>,
    /// Retention from creation; `None` means the default, `Some(None)` forever.
    pub expires_in_ms: Option<Option<u64>>,
}

#[derive(Debug, Serialize)]
pub struct ArtifactPage {
    pub items: Vec<Artifact>,
    pub next_cursor: Option<String>,
}

#[derive(Clone)]
pub struct ArtifactStore {
    db: Arc<HiveDB>,
    tenant: String,
    root: PathBuf,
}

impl SessionStore {
    /// Artifacts for this store's tenant, with files under `root`.
    pub fn artifacts(&self, root: impl Into<PathBuf>) -> Result<ArtifactStore> {
        ArtifactStore::open(self.db.clone(), self.tenant_id.clone(), root.into())
    }
}

fn storage(error: impl std::fmt::Display) -> Error {
    Error::Storage(error.to_string())
}
fn invalid(message: impl Into<String>) -> Error {
    Error::Input(message.into())
}
fn io(error: std::io::Error) -> Error {
    Error::State(format!("artifact file error: {}", error.kind()))
}
fn json_tool(name: &str, description: &str, properties: Value, required: Vec<&str>) -> Tool {
    Tool {
        name: name.into(),
        description: description.into(),
        parameters: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
        allow_repeated_results: false,
    }
}
fn sha256(bytes: &[u8]) -> String {
    digest::digest(&digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `YYYY-MM-DD` in UTC (days-from-civil inverse; no calendar dependency).
fn utc_day(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let year_of_era = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let day_of_year = doe - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_part = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_part + 2) / 5 + 1;
    let month = if month_part < 10 {
        month_part + 3
    } else {
        month_part - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn extension(mime_type: &str) -> &'static str {
    match mime_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        "text/markdown" => "md",
        "text/csv" => "csv",
        "application/json" => "json",
        _ => "bin",
    }
}

fn looks_binary(mime_type: &str, bytes: &[u8]) -> bool {
    mime_type.starts_with("image/")
        || mime_type.starts_with("audio/")
        || mime_type.starts_with("video/")
        || mime_type == "application/octet-stream"
        || mime_type == "application/pdf"
        || bytes.iter().take(1024).any(|byte| *byte == 0)
}

impl ArtifactStore {
    pub fn open(db: Arc<HiveDB>, tenant: String, root: PathBuf) -> Result<Self> {
        if tenant.trim().is_empty() {
            return Err(invalid("artifact tenant must not be empty"));
        }
        fs::create_dir_all(&root).map_err(io)?;
        let root = fs::canonicalize(&root).map_err(io)?;
        Ok(Self { db, tenant, root })
    }

    fn key(&self, id: &str) -> String {
        json!([self.tenant, id]).to_string()
    }

    fn read_row(&self, id: &str) -> Result<Option<(u64, Artifact)>> {
        // Ids are UUIDs we generated; anything else cannot exist, so reject early.
        if uuid::Uuid::parse_str(id).is_err() {
            return Err(invalid("invalid artifact id"));
        }
        let Some(row) = self
            .db
            .col_get(COLLECTION, &self.key(id))
            .map_err(storage)?
        else {
            return Ok(None);
        };
        if row.doc["tenant_id"] != self.tenant {
            return Err(Error::State("artifact scope mismatch".into()));
        }
        let artifact: Artifact = serde_json::from_value(row.doc["artifact"].clone())?;
        Ok(Some((row.version, artifact)))
    }

    fn write_row(&self, artifact: &Artifact, expected: u64) -> Result<()> {
        let row = json!({"tenant_id":self.tenant,"artifact":artifact});
        self.db
            .col_put(
                COLLECTION,
                &self.key(&artifact.id),
                &row,
                PutOptions {
                    expected_version: Some(expected),
                },
            )
            .map_err(|error| {
                if error.to_string().contains("version conflict on") {
                    Error::State("ARTIFACT_CONFLICT".into())
                } else {
                    storage(error)
                }
            })?;
        Ok(())
    }

    fn file(&self, artifact: &Artifact) -> Result<PathBuf> {
        let relative = Path::new(&artifact.path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(Error::State(
                "artifact path is not relative to the store".into(),
            ));
        }
        Ok(self.root.join(relative))
    }

    /// Store `bytes`. The declared media type is replaced by the one the magic
    /// bytes show (PNG, JPEG, GIF, WebP, PDF); image dimensions are read from the header.
    pub fn create(&self, bytes: &[u8], input: NewArtifact) -> Result<Artifact> {
        self.create_at(bytes, input, crate::budget::now_ms()?)
    }

    pub fn create_at(&self, bytes: &[u8], input: NewArtifact, now: u64) -> Result<Artifact> {
        if bytes.is_empty() || bytes.len() > MAX_ARTIFACT_BYTES {
            return Err(invalid(format!(
                "artifact must be 1..={MAX_ARTIFACT_BYTES} bytes"
            )));
        }
        if input.kind.is_empty()
            || input.kind.len() > 64
            || input.kind.chars().any(char::is_control)
        {
            return Err(invalid(
                "artifact kind must be 1..64 characters without control characters",
            ));
        }
        if !media::valid_media_type(&input.mime_type) {
            return Err(invalid("artifact mime_type is not a valid media type"));
        }
        for label in [&input.run_id, &input.task_id].into_iter().flatten() {
            if label.is_empty() || label.len() > 256 || label.chars().any(char::is_control) {
                return Err(invalid(
                    "run_id and task_id must be 1..256 characters without control characters",
                ));
            }
        }
        let size = image::info(bytes);
        let mime_type = image::detect_mime(bytes).map_or(input.mime_type.clone(), str::to_owned);
        let expires_at = match input.expires_in_ms {
            None => Some(now.saturating_add(DEFAULT_RETENTION_MS)),
            Some(None) => None,
            Some(Some(ms)) => Some(now.saturating_add(ms)),
        };
        let id = uuid::Uuid::new_v4().to_string();
        let relative = format!("artifacts/{}/{id}.{}", utc_day(now), extension(&mime_type));
        let artifact = Artifact {
            id: id.clone(),
            kind: input.kind,
            mime_type,
            size: bytes.len() as u64,
            sha256: sha256(bytes),
            width: size.map(|info| info.width),
            height: size.map(|info| info.height),
            run_id: input.run_id,
            task_id: input.task_id,
            status: "active".into(),
            created_at: now,
            expires_at,
            expired_at: None,
            path: relative,
        };
        let path = self.file(&artifact)?;
        fs::create_dir_all(
            path.parent()
                .ok_or_else(|| invalid("invalid artifact path"))?,
        )
        .map_err(io)?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(io)?;
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        if let Err(error) = written
            .map_err(io)
            .and_then(|()| self.write_row(&artifact, 0))
        {
            let _ = fs::remove_file(&path);
            return Err(error);
        }
        Ok(artifact)
    }

    /// Metadata only. `None` when the id does not exist for this tenant.
    pub fn get(&self, id: &str) -> Result<Option<Artifact>> {
        Ok(self.read_row(id)?.map(|(_, artifact)| artifact))
    }

    fn active(&self, id: &str) -> Result<Artifact> {
        let artifact = self
            .get(id)?
            .ok_or_else(|| Error::State("ARTIFACT_NOT_FOUND".into()))?;
        if artifact.status != "active" {
            return Err(Error::State(format!(
                "ARTIFACT_{}",
                artifact.status.to_uppercase()
            )));
        }
        Ok(artifact)
    }

    /// Raw bytes, verified against the recorded size and SHA-256.
    pub fn read_bytes(&self, id: &str) -> Result<Vec<u8>> {
        let artifact = self.active(id)?;
        let bytes = fs::read(self.file(&artifact)?).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                Error::State("ARTIFACT_MISSING".into())
            } else {
                io(error)
            }
        })?;
        if bytes.len() as u64 != artifact.size || sha256(&bytes) != artifact.sha256 {
            return Err(Error::State(
                "ARTIFACT_CORRUPT: content does not match its recorded SHA-256".into(),
            ));
        }
        Ok(bytes)
    }

    /// UTF-8 text of a non-binary artifact.
    pub fn read_text(&self, id: &str) -> Result<String> {
        let artifact = self.active(id)?;
        let bytes = self.read_bytes(id)?;
        if looks_binary(&artifact.mime_type, &bytes) {
            return Err(invalid(format!(
                "artifact is binary ({}); inspect it or read it as base64",
                artifact.mime_type
            )));
        }
        String::from_utf8(bytes).map_err(|_| invalid("artifact is not valid UTF-8 text"))
    }

    pub fn list(&self, limit: usize, cursor: Option<&str>) -> Result<ArtifactPage> {
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err(invalid("artifact limit must be between 1 and 200"));
        }
        let prefix = {
            let tuple = json!([self.tenant]).to_string();
            format!("{},", &tuple[..tuple.len() - 1])
        };
        if cursor.is_some_and(|cursor| !cursor.starts_with(&prefix)) {
            return Err(invalid("cursor belongs to a different artifact scope"));
        }
        let mut items = Vec::new();
        let mut start = cursor.map(str::to_owned);
        loop {
            let rows = self
                .db
                .col_scan(
                    COLLECTION,
                    &ScanOptions {
                        prefix: Some(prefix.clone()),
                        start: start.clone(),
                        limit: MAX_PAGE,
                        ..Default::default()
                    },
                )
                .map_err(storage)?;
            if rows.is_empty() {
                break;
            }
            let full = rows.len() == MAX_PAGE;
            let last = rows.last().unwrap().id.clone();
            for row in rows {
                if start.as_ref() == Some(&row.id) {
                    continue;
                }
                if row.doc["tenant_id"] != self.tenant {
                    return Err(Error::State("artifact scope mismatch".into()));
                }
                if items.len() == limit {
                    return Ok(ArtifactPage {
                        items,
                        next_cursor: start,
                    });
                }
                items.push(serde_json::from_value(row.doc["artifact"].clone())?);
                start = Some(row.id);
            }
            if !full {
                break;
            }
            start = Some(last);
        }
        Ok(ArtifactPage {
            items,
            next_cursor: None,
        })
    }

    fn retire(&self, id: &str, status: &str, now: u64) -> Result<Option<Artifact>> {
        let Some((version, mut artifact)) = self.read_row(id)? else {
            return Ok(None);
        };
        if artifact.status != "active" {
            return Ok(None);
        }
        // Record the new state first: a crash then leaves an orphan file, never a
        // live record pointing at deleted bytes.
        artifact.status = status.into();
        artifact.expired_at = Some(now);
        self.write_row(&artifact, version)?;
        match fs::remove_file(self.file(&artifact)?) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(error)),
        }
        Ok(Some(artifact))
    }

    /// Remove the file and mark the record `deleted`. `false` if it was not active.
    pub fn delete(&self, id: &str) -> Result<bool> {
        Ok(self
            .retire(id, "deleted", crate::budget::now_ms()?)?
            .is_some())
    }

    /// Retire every active artifact whose retention has elapsed. Returns their ids.
    pub fn expire(&self, now: u64) -> Result<Vec<String>> {
        let mut retired = Vec::new();
        let mut cursor = None;
        loop {
            let page = self.list(MAX_PAGE, cursor.as_deref())?;
            for artifact in &page.items {
                if artifact.status == "active"
                    && artifact.expires_at.is_some_and(|at| at <= now)
                    && self.retire(&artifact.id, "expired", now)?.is_some()
                {
                    retired.push(artifact.id.clone());
                }
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(retired)
    }

    /// Tool definitions this store can execute (`artifact_*` and `image_info`).
    pub fn tool_definitions(&self) -> Vec<Tool> {
        let id = json!({"type":"string","minLength":36,"maxLength":36});
        let kind = json!({"type":"string","minLength":1,"maxLength":64});
        let mime = json!({"type":"string","minLength":3,"maxLength":255});
        [
            ("artifact_save", "Store text or base64 content as an artifact and return its id and metadata.",
             json!({"kind":kind,"mime_type":mime,"text":{"type":"string"},"data":{"type":"string"},
                    "expires_in_days":{"type":["number","null"],"exclusiveMinimum":0,"maximum":3650}}),
             vec!["kind", "mime_type"]),
            ("artifact_inspect", "Metadata of an artifact, including its status.", json!({"id":id}), vec!["id"]),
            ("artifact_read", "Read a text artifact, or any artifact as base64 when encoding is base64.",
             json!({"id":id,"encoding":{"type":"string","enum":["text","base64"]}}), vec!["id"]),
            ("artifact_list", "List artifacts of this tenant.", json!({"limit":{"type":"integer","minimum":1,"maximum":200},"cursor":{"type":"string"}}), vec![]),
            ("artifact_delete", "Delete an artifact's content and mark it deleted.", json!({"id":id}), vec!["id"]),
            ("image_info", "Format and pixel size of a PNG, JPEG, GIF or WebP artifact, read from its header.", json!({"id":id}), vec!["id"]),
        ]
        .into_iter()
        .map(|(name, description, properties, required)| json_tool(name, description, properties, required))
        .collect()
    }

    pub fn execute_tool(&self, name: &str, arguments: Value) -> Result<Value> {
        let tool = self
            .tool_definitions()
            .into_iter()
            .find(|tool| tool.name == name)
            .ok_or_else(|| invalid("unknown artifact tool"))?;
        if !crate::validation::compile(&tool.parameters)?.is_valid(&arguments) {
            return Err(invalid("artifact tool arguments do not match schema"));
        }
        let id = arguments["id"].as_str().unwrap_or_default();
        Ok(match name {
            "artifact_save" => {
                let bytes = match (arguments["text"].as_str(), arguments["data"].as_str()) {
                    (Some(text), None) => text.as_bytes().to_vec(),
                    (None, Some(data)) => media::decode_base64(data)
                        .ok_or_else(|| invalid("data is not valid base64"))?,
                    _ => return Err(invalid("provide exactly one of text and data")),
                };
                let expires_in_ms = match arguments.get("expires_in_days") {
                    None => None,
                    Some(Value::Null) => Some(None),
                    Some(days) => Some(Some((days.as_f64().unwrap_or(0.0) * 86_400_000.0) as u64)),
                };
                let artifact = self.create(
                    &bytes,
                    NewArtifact {
                        kind: arguments["kind"].as_str().unwrap_or_default().into(),
                        mime_type: arguments["mime_type"].as_str().unwrap_or_default().into(),
                        expires_in_ms,
                        ..Default::default()
                    },
                )?;
                serde_json::to_value(artifact)?
            }
            "artifact_inspect" => serde_json::to_value(self.get(id)?)?,
            "artifact_read" => {
                if arguments["encoding"] == "base64" {
                    json!({"encoding":"base64","data":media::encode_base64(&self.read_bytes(id)?)})
                } else {
                    json!({"encoding":"text","text":self.read_text(id)?})
                }
            }
            "artifact_list" => serde_json::to_value(self.list(
                arguments["limit"].as_u64().unwrap_or(50) as usize,
                arguments["cursor"].as_str(),
            )?)?,
            "artifact_delete" => json!({"deleted":self.delete(id)?}),
            "image_info" => {
                let bytes = self.read_bytes(id)?;
                serde_json::to_value(
                    image::info(&bytes)
                        .ok_or_else(|| invalid("artifact is not a recognised image"))?,
                )?
            }
            _ => unreachable!(),
        })
    }

    pub fn dispatch(&self, command: Value) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
        enum Command {
            ToolDefinitions,
            ExecuteTool { name: String, arguments: Value },
            Expire { now: Option<u64> },
        }
        Ok(match serde_json::from_value::<Command>(command)? {
            Command::ToolDefinitions => serde_json::to_value(self.tool_definitions())?,
            Command::ExecuteTool { name, arguments } => self.execute_tool(&name, arguments)?,
            Command::Expire { now } => {
                json!({"expired":self.expire(now.map_or_else(crate::budget::now_ms, Ok)?)?})
            }
        })
    }
}
