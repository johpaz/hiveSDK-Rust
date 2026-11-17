//! The oracle's review of the coordinator's final answer.
//!
//! **Off** until an administrator turns it on (`PUT /api/oracle`). When the coordinator is about to
//! give its answer, the SDK's reviewer asks the oracle two closed questions about it ("should it stop
//! here?", "is it backed by what the tools returned?"). A confident "no" sends the model back to
//! correct itself, at most `max_corrections` times, before the person sees anything; otherwise the
//! answer goes out unchanged.
//!
//! The oracle is advisory and fails open: unreachable, cooling down, over budget or answering badly
//! all mean "accept". Every call is an auxiliary model call charged to the turn's own Objective, so
//! it can neither be free nor unbounded, and a spent budget skips the review rather than the answer.
//! The oracle's API key is a secret (`oracle:api_key`): it is never returned, only whether it exists.
use crate::config::{self, App};
use hive_agent_core::oracle::{
    Oracle, OracleReply,
    review::{ReviewConfig, ReviewInput, Verdict, judge, questions},
};
use hivedb_core::PutOptions;
use serde_json::{Value, json};
use std::sync::Arc;

const COLLECTION: &str = "oracle";
const ID: &str = "settings";
const KEY_SECRET: &str = "oracle:api_key";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Settings {
    pub enabled: bool,
    pub endpoint: String,
    pub model: String,
    pub self_hosted: bool,
    pub timeout_ms: u64,
    pub review: ReviewConfig,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: String::new(),
            model: String::new(),
            self_hosted: false,
            timeout_ms: 5_000,
            review: ReviewConfig::default(),
        }
    }
}

impl Settings {
    pub(crate) fn parse(doc: &Value) -> Result<Self, String> {
        let object = doc.as_object().ok_or("los ajustes deben ser un objeto")?;
        const KNOWN: [&str; 10] = [
            "id",
            "enabled",
            "endpoint",
            "model",
            "self_hosted",
            "timeout_ms",
            "max_corrections",
            "min_confidence",
            "support_threshold",
            "api_key",
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
        flag("self_hosted", &mut settings.self_hosted)?;
        let text = |key: &str, max: usize| -> Result<Option<String>, String> {
            match object.get(key) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::String(text))
                    if text.len() <= max && !text.chars().any(char::is_control) =>
                {
                    Ok(Some(text.clone()))
                }
                Some(_) => Err(format!("{key} no es un texto válido")),
            }
        };
        settings.endpoint = text("endpoint", 2048)?.unwrap_or_default();
        settings.model = text("model", 256)?.unwrap_or_default();
        if let Some(value) = object.get("timeout_ms") {
            settings.timeout_ms = value
                .as_u64()
                .filter(|n| (500..=30_000).contains(n))
                .ok_or("timeout_ms debe estar entre 500 y 30000")?;
        }
        let review = json!({
            "max_corrections": object.get("max_corrections").cloned().unwrap_or(json!(settings.review.max_corrections)),
            "min_confidence": object.get("min_confidence").cloned().unwrap_or(json!(settings.review.min_confidence)),
            "support_threshold": object.get("support_threshold").cloned().unwrap_or(json!(settings.review.support_threshold)),
        });
        settings.review = serde_json::from_value(review)
            .map_err(|_| "max_corrections, min_confidence y support_threshold deben ser números")?;
        settings.review.validate().map_err(|e| e.to_string())?;
        if settings.enabled && (settings.endpoint.is_empty() || settings.model.is_empty()) {
            return Err("para activar el oráculo hacen falta endpoint y model".into());
        }
        if !settings.endpoint.is_empty() {
            // The same checks the SDK makes, so a bad address is refused when it is saved.
            Oracle::from_json(json!({"endpoint":settings.endpoint,"model":if settings.model.is_empty() { "m" } else { &settings.model },"timeout_ms":settings.timeout_ms}))
                .map_err(|e| e.to_string())?;
        }
        Ok(settings)
    }

    fn to_json(&self) -> Value {
        json!({"enabled":self.enabled,"endpoint":self.endpoint,"model":self.model,"self_hosted":self.self_hosted,
            "timeout_ms":self.timeout_ms,"max_corrections":self.review.max_corrections,
            "min_confidence":self.review.min_confidence,"support_threshold":self.review.support_threshold})
    }
}

impl App {
    pub(crate) fn oracle_settings(&self) -> crate::Result<Settings> {
        match self.db.col_get(COLLECTION, ID)? {
            None => Ok(Settings::default()),
            // An unreadable document must not switch anything on.
            Some(row) => Ok(Settings::parse(&row.doc).unwrap_or_default()),
        }
    }

    /// The oracle for the current settings (built once per change, so its cooldown survives between
    /// turns), or none when off.
    pub(crate) fn oracle_client(&self) -> crate::Result<Option<(Arc<Oracle>, ReviewConfig)>> {
        let settings = self.oracle_settings()?;
        let mut slot = self
            .oracle
            .lock()
            .map_err(|_| "Estado del oráculo bloqueado")?;
        if !settings.enabled {
            slot.take();
            return Ok(None);
        }
        let key = config::secret(&self.db, &self.home, KEY_SECRET)?;
        let signature = config::App::hash_token(&format!(
            "{}\\n{}",
            settings.to_json(),
            key.as_deref().unwrap_or_default()
        ));
        if let Some((current, oracle)) = slot.as_ref()
            && *current == signature
        {
            return Ok(Some((oracle.clone(), settings.review)));
        }
        let oracle = Arc::new(
            Oracle::from_json(
                json!({"endpoint":settings.endpoint,"model":settings.model,"api_key":key,
                "timeout_ms":settings.timeout_ms,"self_hosted":settings.self_hosted,
                // A server that reports no usage is charged this much rather than halting the turn.
                "assumed_input_tokens":2000}),
            )
            .map_err(|e| format!("Ajustes del oráculo inválidos: {e}"))?,
        );
        *slot = Some((signature, oracle.clone()));
        Ok(Some((oracle, settings.review)))
    }

    /// A turn that may be corrected can run its model and tool budget again per correction, plus the
    /// oracle's own call each time.
    pub(crate) fn oracle_corrections(&self) -> u32 {
        match self.oracle_settings() {
            Ok(s) if s.enabled => s.review.max_corrections,
            _ => 0,
        }
    }
}

/// More budget for a turn whose answer may be corrected: its model and tool calls once more per
/// correction, and one oracle call per review.
pub(crate) fn with_review_budget(
    mut policy: hive_agent_core::objective::ObjectivePolicy,
    corrections: u32,
) -> hive_agent_core::objective::ObjectivePolicy {
    if corrections == 0 {
        return policy;
    }
    let rounds = corrections.saturating_add(1);
    policy.max_model_calls = policy
        .max_model_calls
        .saturating_mul(rounds)
        .saturating_add(rounds);
    policy.max_tool_calls = policy.max_tool_calls.saturating_mul(rounds);
    policy.max_total_output_tokens = policy
        .max_total_output_tokens
        .saturating_mul(u64::from(rounds))
        .saturating_add(64 * u64::from(rounds));
    policy
}

/// A short, secret-free note of what a tool returned, for the oracle: `{tool, ok, excerpt}`.
pub(crate) fn evidence(tool: &str, result: &Value) -> Value {
    let mut shown = result.clone();
    // A screenshot's pixels are not evidence the oracle can read.
    if let Some(object) = shown.as_object_mut() {
        object.remove("data");
    }
    let text = shown.to_string();
    let excerpt: String = text.chars().take(400).collect();
    json!({"tool":tool,"ok":result["ok"] != false && result.get("error").is_none_or(Value::is_null),"excerpt":excerpt})
}

/// Ask the oracle about a finished answer and say what to do. The call is reserved on the turn's
/// Objective first and settled with the reported usage; any problem means `Accept`.
pub(crate) async fn review(
    app: &Arc<App>,
    user: &str,
    objective_id: &str,
    input: ReviewInput,
    cancel: &hive_agent_core::provider::CancellationToken,
) -> Verdict {
    let Ok(Some((oracle, config))) = app.oracle_client() else {
        return Verdict::Accept;
    };
    if oracle.status().is_ok_and(|status| status.cooling_down) {
        return Verdict::Accept; // nothing sent, nothing reserved
    }
    let max_output = oracle.config().max_output_tokens;
    let command = |value: Value| value.to_string();
    let reserve = {
        let (app, user, objective) = (app.clone(), user.to_owned(), objective_id.to_owned());
        let body = command(json!({"op":"model","depth":0,"cap":max_output}));
        tokio::task::spawn_blocking(move || -> crate::Result<Value> {
            Ok(app
                .store_handle(&user)?
                .objective_command(&objective, &body)?)
        })
        .await
    };
    // A turn that has spent its budget skips the review, not the answer.
    let Ok(Ok(permit)) = reserve else {
        return Verdict::Accept;
    };
    let (state, asked) = questions(&input);
    let reply = match oracle.ask(&state, &asked, cancel).await {
        Ok(reply) => reply,
        Err(_) => return Verdict::Accept, // cancelled: the reservation stays, like any unconfirmed call
    };
    if let OracleReply::Decision(decision) = &reply {
        let (app, user, objective) = (app.clone(), user.to_owned(), objective_id.to_owned());
        let body = command(json!({"op":"settle","id":permit["id"],"usage":decision.charge}));
        let _ = tokio::task::spawn_blocking(move || -> crate::Result<Value> {
            Ok(app
                .store_handle(&user)?
                .objective_command(&objective, &body)?)
        })
        .await;
    }
    judge(&reply, &config)
}

/// `GET/PUT /api/oracle` (administrators only). The body of PUT is the settings, plus an optional
/// `api_key` that is stored as a secret and never shown again.
pub(crate) fn route(
    app: &App,
    method: &str,
    path: &str,
    body: &[u8],
) -> crate::Result<Option<(u16, Value)>> {
    if path != "/api/oracle" {
        return Ok(None);
    }
    let status = |app: &App| -> crate::Result<Value> {
        let settings = app.oracle_settings()?;
        let live = app
            .oracle_client()
            .ok()
            .flatten()
            .and_then(|(oracle, _)| oracle.status().ok());
        Ok(
            json!({"settings":settings.to_json(),"has_api_key":config::secret(&app.db, &app.home, KEY_SECRET)?.is_some(),
            "status":live.map(|s| json!({"state":s.state,"cooling_down":s.cooling_down,"last_error":s.last_error,"decisions":s.decisions}))}),
        )
    };
    Ok(Some(match method {
        "GET" => (200, status(app)?),
        "PUT" => {
            let doc: Value = serde_json::from_slice(body)?;
            let settings = match Settings::parse(&doc) {
                Ok(settings) => settings,
                Err(error) => return Ok(Some((400, json!({"error":error})))),
            };
            if let Some(key) = doc.get("api_key") {
                match key.as_str() {
                    Some(key)
                        if !key.is_empty()
                            && key.len() <= 512
                            && !key.chars().any(char::is_whitespace) =>
                    {
                        config::store_secret(app, KEY_SECRET, key)?
                    }
                    _ => return Ok(Some((400, json!({"error":"api_key no es válida"})))),
                }
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
            (200, status(app)?)
        }
        _ => (405, json!({"error":"Método no permitido"})),
    }))
}
