//! Opt-in decision oracle (the Jev/Kev "System One" protocol).
//!
//! An oracle answers closed questions about a state the host describes: pick one
//! of named choices, or give a probability for a yes/no question. It is advisory
//! and never part of the Agent loop on its own: the host builds the questions,
//! decides what to do with the answers, and always has the classic path to fall
//! back on. Everything here is off unless the host constructs an [`Oracle`].
//!
//! Failures never raise: an unreachable server, an HTTP error or an answer that
//! breaks the contract produce [`OracleReply::Fallback`] and feed a cooldown, so
//! a broken oracle cannot stall a run. Every call is an auxiliary model call:
//! [`Oracle::ask_charged`] reserves it on the root [`Objective`] first and
//! settles the reported usage afterwards, like any other model call.
use crate::{Error, Result, Usage, objective::Objective, provider::CancellationToken};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Mutex, time::Duration};

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_QUESTIONS: usize = 32;

fn default_timeout() -> u64 {
    3_000
}
fn default_output() -> u64 {
    64
}
fn default_cooldown() -> u64 {
    60_000
}
fn default_strikes() -> u32 {
    3
}
fn default_distrust() -> u64 {
    5 * 60_000
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OracleConfig {
    /// Full URL of the decisions endpoint (OpenRouter's, a lab's `/v1/systemone`,
    /// or a self-hosted decision model).
    pub endpoint: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    /// A server without a per-call tariff; its calls are reported as free.
    #[serde(default)]
    pub self_hosted: bool,
    /// Output tokens reserved on the Objective for each call.
    #[serde(default = "default_output")]
    pub max_output_tokens: u64,
    /// Input tokens to charge when the server reports no usage. Without it, a
    /// response with no usage settles as unknown and halts the Objective, because
    /// an unmeasured call can never create spendable capacity.
    #[serde(default)]
    pub assumed_input_tokens: Option<u64>,
    /// How long the oracle is skipped after an authorization failure or repeated errors.
    #[serde(default = "default_cooldown")]
    pub cooldown_ms: u64,
    /// Consecutive overruled decisions after which the oracle is set aside.
    #[serde(default = "default_strikes")]
    pub distrust_strikes: u32,
    #[serde(default = "default_distrust")]
    pub distrust_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NoulCriteria {
    pub r#true: String,
    pub r#false: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Question {
    /// Pick exactly one key of `criteria`.
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
    /// Probability in [0, 1] that the statement in `instructions` is true.
    Noul {
        instructions: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    Choice {
        choice: String,
        confidence: f64,
        probabilities: BTreeMap<String, f64>,
    },
    Noul {
        noul: f64,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Decision {
    pub answers: BTreeMap<String, Answer>,
    /// Usage to settle on the Objective: reported, assumed, or absent.
    pub charge: Option<Usage>,
    /// Reported or zero for self-hosted servers; `None` when unknown.
    pub cost_usd: Option<f64>,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum OracleReply {
    Decision(Decision),
    /// No decision this time; run the classic path. `sent` tells whether a
    /// request went out (its reservation then stays on the Objective).
    Fallback {
        reason: String,
        sent: bool,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OracleStatus {
    /// `ready`, or `fallback` while in cooldown or after an unrecovered error.
    pub state: &'static str,
    /// True while calls are skipped; asking then sends nothing and reserves nothing.
    pub cooling_down: bool,
    pub last_error: Option<String>,
    pub last_success_at: Option<u64>,
    pub decisions: u64,
}

#[derive(Default)]
struct State {
    failures: u32,
    cooldown_until: u64,
    last_error: Option<String>,
    last_success_at: Option<u64>,
    strikes: u32,
    decisions: u64,
}

pub struct Oracle {
    config: OracleConfig,
    client: reqwest::Client,
    state: Mutex<State>,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Input(message.into())
}

impl Oracle {
    pub fn new(config: OracleConfig) -> Result<Self> {
        let url = reqwest::Url::parse(&config.endpoint)
            .map_err(|_| invalid("invalid oracle endpoint"))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(invalid(
                "oracle endpoint must be http(s) without credentials or fragment",
            ));
        }
        if config.model.is_empty() {
            return Err(invalid("oracle model must not be empty"));
        }
        if config.timeout_ms == 0 || config.timeout_ms > 60_000 {
            return Err(invalid("oracle timeout_ms must be 1..=60000"));
        }
        if config.max_output_tokens == 0 || config.distrust_strikes == 0 {
            return Err(invalid(
                "oracle max_output_tokens and distrust_strikes must be positive",
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::State("cannot initialize oracle HTTP client".into()))?;
        Ok(Self {
            config,
            client,
            state: Mutex::new(State::default()),
        })
    }

    pub fn from_json(config: Value) -> Result<Self> {
        Self::new(serde_json::from_value(config)?)
    }

    pub fn config(&self) -> &OracleConfig {
        &self.config
    }

    fn state(&self) -> Result<std::sync::MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| Error::State("poisoned oracle state".into()))
    }

    pub fn status(&self) -> Result<OracleStatus> {
        let now = crate::budget::now_ms()?;
        let state = self.state()?;
        let cooling_down = now < state.cooldown_until;
        Ok(OracleStatus {
            state: if cooling_down || state.last_error.is_some() {
                "fallback"
            } else {
                "ready"
            },
            cooling_down,
            last_error: state.last_error.clone(),
            last_success_at: state.last_success_at,
            decisions: state.decisions,
        })
    }

    /// The runtime found the oracle wrong (it said "done" and the evidence
    /// disagreed, it demanded a retry that changed nothing). Enough of them in a
    /// row set it aside for `distrust_ms`. Returns true when this one did.
    pub fn record_overruled(&self, reason: &str) -> Result<bool> {
        let now = crate::budget::now_ms()?;
        let mut state = self.state()?;
        state.strikes += 1;
        if state.strikes < self.config.distrust_strikes {
            return Ok(false);
        }
        state.strikes = 0;
        state.cooldown_until = now.saturating_add(self.config.distrust_ms);
        state.last_error = Some(format!(
            "oracle set aside after {} contradictions in a row (last: {reason})",
            self.config.distrust_strikes
        ));
        Ok(true)
    }

    /// A verdict nothing contradicted: the streak of contradictions ends.
    pub fn record_agreement(&self) -> Result<()> {
        self.state()?.strikes = 0;
        Ok(())
    }

    /// Clear failures, cooldown and strikes (for example after the key changed).
    pub fn reset(&self) -> Result<()> {
        let mut state = self.state()?;
        *state = State {
            decisions: state.decisions,
            ..State::default()
        };
        Ok(())
    }

    fn fail(&self, reason: String, now: u64, authorization: bool) -> Result<OracleReply> {
        let mut state = self.state()?;
        state.failures += 1;
        state.last_error = Some(reason.clone());
        if authorization || state.failures >= 3 {
            state.cooldown_until = now.saturating_add(self.config.cooldown_ms);
        }
        Ok(OracleReply::Fallback { reason, sent: true })
    }

    /// Ask without touching any Objective; the caller must charge `Decision::charge`.
    pub async fn ask(
        &self,
        state: &Value,
        questions: &BTreeMap<String, Question>,
        cancel: &CancellationToken,
    ) -> Result<OracleReply> {
        if questions.is_empty() {
            return Ok(OracleReply::Fallback {
                reason: "no questions".into(),
                sent: false,
            });
        }
        if questions.len() > MAX_QUESTIONS {
            return Err(invalid(format!(
                "at most {MAX_QUESTIONS} questions per call"
            )));
        }
        for (name, question) in questions {
            if name.is_empty() {
                return Err(invalid("question names must not be empty"));
            }
            if let Question::Choice { criteria, .. } = question
                && criteria.len() < 2
            {
                return Err(invalid(format!(
                    "choice question `{name}` needs at least two criteria"
                )));
            }
        }
        let now = crate::budget::now_ms()?;
        if now < self.state()?.cooldown_until {
            return Ok(OracleReply::Fallback {
                reason: "oracle in cooldown".into(),
                sent: false,
            });
        }
        let started = std::time::Instant::now();
        let mut call = self
            .client
            .post(&self.config.endpoint)
            .json(&json!({"model":self.config.model,"state":state,"questions":questions}));
        if let Some(key) = &self.config.api_key {
            call = call.bearer_auth(key);
        }
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            outcome = Self::exchange(call) => outcome,
        };
        let (status, body) = match outcome {
            Ok(pair) => pair,
            Err(reason) => return self.fail(reason, now, false),
        };
        if !(200..300).contains(&status) {
            return self.fail(
                format!("oracle HTTP {status}"),
                now,
                matches!(status, 401 | 403),
            );
        }
        let parsed = match serde_json::from_slice::<Value>(&body)
            .map_err(|_| "oracle returned invalid JSON".to_owned())
            .and_then(|data| validate(&data, questions).map(|answers| (data, answers)))
        {
            Ok(pair) => pair,
            Err(reason) => return self.fail(reason, now, false),
        };
        let (data, answers) = parsed;
        let usage = &data["usage"];
        let reported = usage["input_tokens"].as_u64().map(|input| Usage {
            input_tokens: input,
            output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
        });
        let charge = reported.or_else(|| {
            self.config.assumed_input_tokens.map(|input| Usage {
                input_tokens: input,
                output_tokens: self.config.max_output_tokens,
            })
        });
        let cost_usd = if self.config.self_hosted {
            Some(0.0)
        } else {
            usage["cost"].as_f64()
        };
        let mut guard = self.state()?;
        guard.failures = 0;
        guard.last_error = None;
        guard.last_success_at = Some(now);
        guard.decisions += 1;
        Ok(OracleReply::Decision(Decision {
            answers,
            charge,
            cost_usd,
            latency_ms: started.elapsed().as_millis() as u64,
        }))
    }

    /// JSON form for bindings: `{state, questions}` in, the serialized reply out.
    pub async fn ask_json(&self, input: Value, cancel: &CancellationToken) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Input {
            state: Value,
            questions: BTreeMap<String, Question>,
        }
        let input: Input = serde_json::from_value(input)?;
        Ok(serde_json::to_value(
            self.ask(&input.state, &input.questions, cancel).await?,
        )?)
    }

    async fn exchange(
        call: reqwest::RequestBuilder,
    ) -> std::result::Result<(u16, Vec<u8>), String> {
        let mut response = call.send().await.map_err(|error| {
            if error.is_timeout() {
                "oracle timed out".to_owned()
            } else {
                "oracle request failed".to_owned()
            }
        })?;
        let status = response.status().as_u16();
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "oracle response failed".to_owned())?
        {
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err("oracle response exceeds the size limit".into());
            }
            body.extend_from_slice(&chunk);
        }
        Ok((status, body))
    }

    /// Ask as an auxiliary model call on the root Objective. The call is reserved
    /// before anything is sent; limits surface as `Err(ObjectiveLimit)`, not as a
    /// fallback, because a spent budget is not an oracle failure. A request that
    /// was sent but produced no decision keeps its reservation, like any model
    /// call whose outcome is not confirmed.
    pub async fn ask_charged(
        &self,
        objective: &Objective,
        depth: u32,
        state: &Value,
        questions: &BTreeMap<String, Question>,
        cancel: &CancellationToken,
    ) -> Result<OracleReply> {
        if questions.is_empty() || crate::budget::now_ms()? < self.state()?.cooldown_until {
            return self.ask(state, questions, cancel).await;
        }
        let permit = objective.reserve_model(depth, Some(self.config.max_output_tokens))?;
        let reply = self.ask(state, questions, cancel).await?;
        if let OracleReply::Decision(decision) = &reply {
            objective.settle_model(permit.id, decision.charge.clone())?;
        }
        Ok(reply)
    }
}

/// Servers spell answers differently: OpenRouter returns `{choice, confidence}` /
/// `{noul}`, llama.cpp's System One returns `{choice, probabilities}` (the
/// choice's confidence is its own probability) and `{probability}`. Both are
/// accepted, then validated strictly.
fn validate(
    data: &Value,
    questions: &BTreeMap<String, Question>,
) -> std::result::Result<BTreeMap<String, Answer>, String> {
    let mut answers = BTreeMap::new();
    for (name, question) in questions {
        let raw = data["answers"].get(name).filter(|raw| raw.is_object());
        let unit = |value: Option<f64>| value.filter(|v| v.is_finite() && (0.0..=1.0).contains(v));
        let answer = match question {
            Question::Choice { criteria, .. } => {
                let choice = raw
                    .and_then(|raw| raw["choice"].as_str())
                    .filter(|choice| criteria.contains_key(*choice));
                let probabilities: BTreeMap<String, f64> = raw
                    .and_then(|raw| raw["probabilities"].as_object())
                    .map(|map| {
                        map.iter()
                            .filter_map(|(k, v)| v.as_f64().map(|v| (k.clone(), v)))
                            .collect()
                    })
                    .unwrap_or_default();
                let confidence = raw
                    .and_then(|raw| raw["confidence"].as_f64())
                    .or_else(|| choice.and_then(|c| probabilities.get(c).copied()));
                match (choice, unit(confidence)) {
                    (Some(choice), Some(confidence)) => Answer::Choice {
                        choice: choice.to_owned(),
                        confidence,
                        probabilities,
                    },
                    _ => return Err(format!("invalid choice answer: {name}")),
                }
            }
            Question::Noul { .. } => {
                let value = raw
                    .and_then(|raw| raw["noul"].as_f64().or_else(|| raw["probability"].as_f64()));
                match unit(value) {
                    Some(noul) => Answer::Noul { noul },
                    None => return Err(format!("invalid noul answer: {name}")),
                }
            }
        };
        answers.insert(name.clone(), answer);
    }
    Ok(answers)
}
