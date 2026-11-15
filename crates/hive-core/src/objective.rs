//! Shared objective budget. Durable session accounting is coordinated in `durable`.
use crate::{Error, Result, Usage, budget::now_ms};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};
mod durable;
mod summary;
mod triggers;
mod verification;
pub use summary::{SummaryClaim, SummaryCost, SummaryInput, SummaryResult};
pub use triggers::{TriggerClaim, TriggerReceipt, TriggerSchedule};
pub use verification::{
    ReviewEvidence, VerificationClaim, VerificationResult, VerificationVerdict,
};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ObjectivePolicy {
    pub max_model_calls: u32,
    pub max_tool_calls: u32,
    pub max_depth: u32,
    pub max_output_tokens: u64,
    pub max_total_output_tokens: u64,
    pub max_run_ms: Option<u64>,
}
impl Default for ObjectivePolicy {
    fn default() -> Self {
        Self {
            max_model_calls: 8,
            max_tool_calls: 16,
            max_depth: 2,
            max_output_tokens: 2048,
            max_total_output_tokens: 8192,
            max_run_ms: None,
        }
    }
}
#[derive(Clone, Default, Deserialize, Serialize)]
struct Ledger {
    model_calls: u32,
    tool_calls: u32,
    output_tokens: u64,
    input_tokens: u64,
    reservations: BTreeMap<u32, u64>,
    settled: BTreeSet<u32>,
    stopped: Option<String>,
    #[serde(default)]
    verifications: BTreeMap<String, verification::VerificationEntry>,
    #[serde(default)]
    triggers: BTreeMap<String, triggers::TriggerEntry>,
    #[serde(default)]
    summaries: BTreeMap<String, summary::SummaryEntry>,
}
pub struct Objective {
    policy: ObjectivePolicy,
    deadline: Option<u64>,
    ledger: Mutex<Ledger>,
}
#[derive(Debug, Deserialize, Serialize)]
pub struct ModelPermit {
    pub id: u32,
    pub max_output_tokens: u64,
    pub deadline_unix_ms: Option<u64>,
}
impl Objective {
    pub fn new(policy: ObjectivePolicy) -> Result<Self> {
        if policy.max_model_calls == 0
            || policy.max_tool_calls == 0
            || policy.max_output_tokens == 0
            || policy.max_total_output_tokens == 0
            || policy.max_run_ms == Some(0)
            || policy.max_total_output_tokens > 9_007_199_254_740_991
            || policy.max_output_tokens > 9_007_199_254_740_991
        {
            return Err(Error::Input(
                "objective limits must be positive and portable".into(),
            ));
        }
        let deadline = policy
            .max_run_ms
            .map(|ms| {
                now_ms()?
                    .checked_add(ms)
                    .filter(|n| *n <= 9_007_199_254_740_991)
                    .ok_or_else(|| Error::Input("objective deadline exceeds portable range".into()))
            })
            .transpose()?;
        Ok(Self {
            policy,
            deadline,
            ledger: Mutex::new(Ledger::default()),
        })
    }
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Ledger>> {
        self.ledger
            .lock()
            .map_err(|_| Error::State("poisoned objective".into()))
    }
    fn check_locked(&self, ledger: &mut Ledger, depth: u32) -> Result<()> {
        if let Some(deadline) = self.deadline
            && now_ms()? >= deadline
        {
            ledger.stopped.get_or_insert("deadline_exceeded".into());
        }
        if let Some(reason) = &ledger.stopped {
            return Err(Error::ObjectiveLimit(reason.clone()));
        }
        if depth > self.policy.max_depth {
            return Err(Error::ObjectiveLimit("depth_limit".into()));
        }
        Ok(())
    }
    pub fn check(&self, depth: u32) -> Result<()> {
        let mut ledger = self.lock()?;
        self.check_locked(&mut ledger, depth)
    }
    pub fn reserve_model(&self, depth: u32, cap: Option<u64>) -> Result<ModelPermit> {
        self.reserve_model_scoped(depth, cap, None)
    }
    fn reserve_model_scoped(
        &self,
        depth: u32,
        cap: Option<u64>,
        scope: Option<&str>,
    ) -> Result<ModelPermit> {
        let mut ledger = self.lock()?;
        self.check_locked(&mut ledger, depth)?;
        if let Some(key) = scope {
            summary::check_scope(&ledger, key, depth)?;
        }
        if ledger.model_calls >= self.policy.max_model_calls {
            return Err(Error::ObjectiveLimit("model_call_limit".into()));
        }
        let reserved: u64 = ledger.reservations.values().sum();
        let remaining = self
            .policy
            .max_total_output_tokens
            .saturating_sub(ledger.output_tokens)
            .saturating_sub(reserved);
        let cap = cap
            .unwrap_or(self.policy.max_output_tokens)
            .min(self.policy.max_output_tokens)
            .min(remaining);
        if cap == 0 {
            return Err(Error::ObjectiveLimit("output_limit".into()));
        }
        ledger.model_calls += 1;
        let id = ledger.model_calls;
        ledger.reservations.insert(id, cap);
        if let Some(key) = scope {
            ledger
                .summaries
                .get_mut(key)
                .unwrap()
                .models
                .insert(id, None);
        }
        Ok(ModelPermit {
            id,
            max_output_tokens: cap,
            deadline_unix_ms: self.deadline,
        })
    }
    /// Confirmed replies settle even after cancellation/deadline. Missing usage retains the reservation.
    pub fn settle_model(&self, id: u32, usage: Option<Usage>) -> Result<()> {
        let mut ledger = self.lock()?;
        if ledger.settled.contains(&id) {
            return Err(Error::State("already settled objective permit".into()));
        }
        let cap = *ledger
            .reservations
            .get(&id)
            .ok_or_else(|| Error::State("unknown or already settled objective permit".into()))?;
        ledger.settled.insert(id);
        for entry in ledger.summaries.values_mut() {
            if let Some(known) = entry.models.get_mut(&id) {
                *known = usage.clone();
                break;
            }
        }
        if let Some(usage) = usage {
            ledger.reservations.remove(&id);
            ledger.input_tokens = ledger.input_tokens.saturating_add(usage.input_tokens);
            ledger.output_tokens = ledger.output_tokens.saturating_add(usage.output_tokens);
            if usage.output_tokens > cap {
                ledger.stopped.get_or_insert("output_limit".into());
            }
        } else {
            // A missing measurement can never create more spendable capacity.
            ledger.stopped.get_or_insert("usage_unavailable".into());
        }
        Ok(())
    }
    pub fn count_tool(&self, depth: u32) -> Result<()> {
        self.count_tool_scoped(depth, None)
    }
    fn count_tool_scoped(&self, depth: u32, scope: Option<&str>) -> Result<()> {
        let mut ledger = self.lock()?;
        self.check_locked(&mut ledger, depth)?;
        if let Some(key) = scope {
            summary::check_scope(&ledger, key, depth)?;
        }
        if ledger.tool_calls >= self.policy.max_tool_calls {
            return Err(Error::ObjectiveLimit("tool_call_limit".into()));
        }
        ledger.tool_calls += 1;
        if let Some(key) = scope {
            ledger.summaries.get_mut(key).unwrap().tool_calls += 1;
        }
        Ok(())
    }
    pub fn cancel(&self) -> Result<()> {
        self.lock()?.stopped.get_or_insert("cancelled".into());
        Ok(())
    }
    pub fn snapshot(&self) -> Result<Value> {
        let ledger = self.lock()?;
        let summary_costs: BTreeMap<_, _> = ledger
            .summaries
            .iter()
            .map(|(key, entry)| (key.clone(), entry.cost(&ledger)))
            .collect();
        Ok(
            json!({"policy": self.policy, "deadline_unix_ms": self.deadline,
            "model_calls": ledger.model_calls, "tool_calls": ledger.tool_calls,
            "input_tokens": ledger.input_tokens, "output_tokens": ledger.output_tokens,
            "reservations": ledger.reservations, "stopped": ledger.stopped,
            "verifications": ledger.verifications, "triggers": ledger.triggers,"summaries":ledger.summaries,
            "summary_costs":summary_costs}),
        )
    }
    pub fn dispatch(&self, command: &str) -> Result<String> {
        #[derive(Deserialize)]
        #[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
        enum Command {
            SummaryCheck {
                key: String,
                depth: u32,
            },
            SummaryBegin {
                key: String,
                input: SummaryInput,
            },
            SummaryFinish {
                key: String,
                execution_id: String,
                content: String,
            },
            SummaryModel {
                key: String,
                depth: u32,
                cap: Option<u64>,
            },
            SummaryTool {
                key: String,
                depth: u32,
            },
            TriggerRegister {
                id: String,
                schedule: TriggerSchedule,
            },
            TriggerEnable {
                id: String,
                enabled: bool,
            },
            TriggerInspect {
                id: String,
            },
            TriggerEmit {
                id: String,
                key: String,
                input: Value,
            },
            TriggerTick {
                id: String,
            },
            TriggerFinish {
                id: String,
                key: String,
                execution_id: String,
                result: Value,
            },
            VerifyBegin {
                key: String,
                input: Value,
                #[serde(default)]
                model_review: Option<ReviewEvidence>,
            },
            VerifyFinish {
                key: String,
                execution_id: String,
                verdict: VerificationVerdict,
            },
            Check {
                depth: u32,
            },
            Model {
                depth: u32,
                cap: Option<u64>,
            },
            Settle {
                id: u32,
                usage: Option<Usage>,
            },
            Tool {
                depth: u32,
            },
            Cancel,
            Inspect,
        }
        let result = match serde_json::from_str(command)? {
            Command::SummaryCheck { key, depth } => {
                self.check_summary(&key, depth)?;
                Value::Null
            }
            Command::SummaryBegin { key, input } => {
                serde_json::to_value(self.begin_summary(key, input)?)?
            }
            Command::SummaryFinish {
                key,
                execution_id,
                content,
            } => serde_json::to_value(self.finish_summary(&key, &execution_id, content)?)?,
            Command::SummaryModel { key, depth, cap } => {
                serde_json::to_value(self.reserve_summary_model(&key, depth, cap)?)?
            }
            Command::SummaryTool { key, depth } => {
                self.count_summary_tool(&key, depth)?;
                Value::Null
            }
            Command::TriggerRegister { id, schedule } => self.register_trigger(id, schedule)?,
            Command::TriggerEnable { id, enabled } => {
                self.enable_trigger(&id, enabled)?;
                Value::Null
            }
            Command::TriggerInspect { id } => self.inspect_trigger(&id)?,
            Command::TriggerEmit { id, key, input } => {
                serde_json::to_value(self.emit_trigger(&id, key, input)?)?
            }
            Command::TriggerTick { id } => {
                serde_json::to_value(self.poll_trigger_at(&id, now_ms()?)?)?
            }
            Command::TriggerFinish {
                id,
                key,
                execution_id,
                result,
            } => serde_json::to_value(self.finish_trigger(&id, &key, &execution_id, result)?)?,
            Command::VerifyBegin {
                key,
                input,
                model_review,
            } => serde_json::to_value(self.begin_verification(key, input, model_review)?)?,
            Command::VerifyFinish {
                key,
                execution_id,
                verdict,
            } => serde_json::to_value(self.finish_verification(&key, &execution_id, verdict)?)?,
            Command::Check { depth } => {
                self.check(depth)?;
                Value::Null
            }
            Command::Model { depth, cap } => serde_json::to_value(self.reserve_model(depth, cap)?)?,
            Command::Settle { id, usage } => {
                self.settle_model(id, usage)?;
                Value::Null
            }
            Command::Tool { depth } => {
                self.count_tool(depth)?;
                Value::Null
            }
            Command::Cancel => {
                self.cancel()?;
                Value::Null
            }
            Command::Inspect => self.snapshot()?,
        };
        Ok(serde_json::to_string(&result)?)
    }
}
