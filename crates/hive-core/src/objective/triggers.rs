//! Explicit event/calendar dispatch. This module never starts a timer or model loop.
use super::Objective;
use crate::{Error, Result, budget::now_ms};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

const MAX_TIME: u64 = 9_007_199_254_740_991;
const MAX_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TriggerSchedule {
    Event,
    At {
        unix_ms: u64,
    },
    Interval {
        every_ms: u64,
        #[serde(default)]
        anchor_unix_ms: Option<u64>,
    },
}
impl TriggerSchedule {
    fn validate(&self) -> Result<()> {
        let valid = match self {
            Self::Event => true,
            Self::At { unix_ms } => *unix_ms <= MAX_TIME,
            Self::Interval {
                every_ms,
                anchor_unix_ms,
            } => {
                *every_ms > 0
                    && *every_ms <= MAX_TIME
                    && anchor_unix_ms.is_none_or(|anchor| {
                        anchor.checked_add(*every_ms).is_some_and(|n| n <= MAX_TIME)
                    })
            }
        };
        if valid {
            Ok(())
        } else {
            Err(Error::Input(
                "trigger schedule exceeds portable timestamp limits".into(),
            ))
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TriggerReceipt {
    pub trigger_id: String,
    pub occurrence_key: String,
    pub execution_id: String,
    pub result: Value,
}
#[derive(Clone, Debug, Serialize)]
pub struct TriggerClaim {
    pub occurrence_key: String,
    pub execution_id: String,
    pub input: Value,
    pub result: Option<TriggerReceipt>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Outcome {
    value: Value,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Execution {
    execution_id: String,
    input: Value,
    // The wrapper distinguishes a confirmed null result from an uncertain result.
    outcome: Option<Outcome>,
}
impl Execution {
    fn receipt(&self, id: &str, key: &str) -> Option<TriggerReceipt> {
        self.outcome.as_ref().map(|outcome| TriggerReceipt {
            trigger_id: id.into(),
            occurrence_key: key.into(),
            execution_id: self.execution_id.clone(),
            result: outcome.value.clone(),
        })
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TriggerEntry {
    schedule: TriggerSchedule,
    enabled: bool,
    anchor_unix_ms: Option<u64>,
    next_due_unix_ms: Option<u64>,
    active_occurrence: Option<String>,
    executions: BTreeMap<String, Execution>,
}
fn validate_id(id: &str) -> Result<()> {
    if id.trim().is_empty() || id.len() > 256 {
        return Err(Error::Input(
            "trigger and occurrence ids must contain 1..256 bytes".into(),
        ));
    }
    Ok(())
}
fn validate_value(value: &Value) -> Result<()> {
    if serde_json::to_vec(value)?.len() > MAX_BYTES {
        return Err(Error::Input("trigger input/result exceeds 64 KiB".into()));
    }
    Ok(())
}
impl TriggerEntry {
    pub(crate) fn execution_count(&self) -> usize {
        self.executions.len()
    }
    pub(crate) fn validate(&self, id: &str) -> Result<()> {
        validate_id(id)?;
        self.schedule.validate()?;
        if self.next_due_unix_ms.is_some_and(|time| time > MAX_TIME) {
            return Err(Error::Storage("invalid trigger next timestamp".into()));
        }
        match &self.schedule {
            TriggerSchedule::Interval {
                every_ms,
                anchor_unix_ms,
            } => {
                if self
                    .anchor_unix_ms
                    .is_none_or(|anchor| anchor.checked_add(*every_ms).is_none_or(|n| n > MAX_TIME))
                    || anchor_unix_ms.is_some_and(|anchor| Some(anchor) != self.anchor_unix_ms)
                    || self.next_due_unix_ms.is_none()
                {
                    return Err(Error::Storage("invalid interval trigger state".into()));
                }
            }
            TriggerSchedule::Event => {
                if self.anchor_unix_ms.is_some() || self.next_due_unix_ms.is_some() {
                    return Err(Error::Storage("event trigger has calendar state".into()));
                }
            }
            TriggerSchedule::At { unix_ms } => {
                if self.anchor_unix_ms.is_some()
                    || self.next_due_unix_ms.is_some_and(|next| next != *unix_ms)
                {
                    return Err(Error::Storage("invalid date trigger state".into()));
                }
            }
        }
        let mut pending = vec![];
        for (key, execution) in &self.executions {
            validate_id(key)?;
            validate_value(&execution.input)?;
            if execution.execution_id.is_empty() {
                return Err(Error::Storage("trigger has no execution id".into()));
            }
            if let Some(outcome) = &execution.outcome {
                validate_value(&outcome.value)?;
            } else {
                pending.push(key.as_str());
            }
        }
        let valid = match self.active_occurrence.as_deref() {
            Some(active) => pending == [active],
            None => pending.is_empty(),
        };
        if !valid {
            return Err(Error::Storage(
                "trigger active occurrence does not match its ledger".into(),
            ));
        }
        Ok(())
    }
}

impl Objective {
    pub fn register_trigger(&self, id: String, schedule: TriggerSchedule) -> Result<Value> {
        validate_id(&id)?;
        schedule.validate()?;
        let mut ledger = self.lock()?;
        if let Some(entry) = ledger.triggers.get(&id) {
            if entry.schedule != schedule {
                return Err(Error::Input(
                    "trigger id is already bound to another schedule".into(),
                ));
            }
            return Ok(serde_json::to_value(entry)?);
        }
        self.check_locked(&mut ledger, 0)?;
        if ledger.triggers.len() >= 64 {
            return Err(Error::Input(
                "objective supports at most 64 triggers".into(),
            ));
        }
        let (anchor, next) = match &schedule {
            TriggerSchedule::Event => (None, None),
            TriggerSchedule::At { unix_ms } => (None, Some(*unix_ms)),
            TriggerSchedule::Interval {
                every_ms,
                anchor_unix_ms,
            } => {
                let anchor = match anchor_unix_ms {
                    Some(anchor) => *anchor,
                    None => now_ms()?
                        .checked_add(*every_ms)
                        .filter(|n| *n <= MAX_TIME)
                        .ok_or_else(|| {
                            Error::Input("trigger anchor exceeds portable range".into())
                        })?,
                };
                if anchor.checked_add(*every_ms).is_none_or(|n| n > MAX_TIME) {
                    return Err(Error::Input(
                        "trigger interval exceeds portable range".into(),
                    ));
                }
                (Some(anchor), Some(anchor))
            }
        };
        let entry = TriggerEntry {
            schedule,
            enabled: false,
            anchor_unix_ms: anchor,
            next_due_unix_ms: next,
            active_occurrence: None,
            executions: BTreeMap::new(),
        };
        let snapshot = serde_json::to_value(&entry)?;
        ledger.triggers.insert(id, entry);
        Ok(snapshot)
    }

    pub fn enable_trigger(&self, id: &str, enabled: bool) -> Result<()> {
        let mut ledger = self.lock()?;
        if enabled {
            self.check_locked(&mut ledger, 0)?;
        }
        ledger
            .triggers
            .get_mut(id)
            .ok_or_else(|| Error::Input("trigger does not exist".into()))?
            .enabled = enabled;
        Ok(())
    }

    pub fn inspect_trigger(&self, id: &str) -> Result<Value> {
        let ledger = self.lock()?;
        let entry = ledger
            .triggers
            .get(id)
            .ok_or_else(|| Error::Input("trigger does not exist".into()))?;
        Ok(serde_json::to_value(entry)?)
    }

    pub fn emit_trigger(
        &self,
        id: &str,
        key: String,
        input: Value,
    ) -> Result<Option<TriggerClaim>> {
        validate_id(&key)?;
        validate_value(&input)?;
        self.begin_trigger(id, Some((key, input)), now_ms()?)
    }

    /// Explicit clock injection for Rust integrations/tests; the root deadline uses the real clock.
    pub fn poll_trigger_at(&self, id: &str, unix_ms: u64) -> Result<Option<TriggerClaim>> {
        if unix_ms > MAX_TIME {
            return Err(Error::Input(
                "trigger observation exceeds portable range".into(),
            ));
        }
        self.begin_trigger(id, None, unix_ms)
    }

    fn begin_trigger(
        &self,
        id: &str,
        event: Option<(String, Value)>,
        now: u64,
    ) -> Result<Option<TriggerClaim>> {
        let mut ledger = self.lock()?;
        let entry = ledger
            .triggers
            .get(id)
            .ok_or_else(|| Error::Input("trigger does not exist".into()))?;
        if !entry.enabled {
            return Ok(None);
        }
        if event.is_some() != matches!(entry.schedule, TriggerSchedule::Event) {
            return Err(Error::Input(
                "emit requires an event trigger; tick requires a calendar trigger".into(),
            ));
        }
        if let Some((key, input)) = &event
            && let Some(execution) = entry.executions.get(key)
        {
            if execution.input != *input {
                return Err(Error::Input(
                    "trigger occurrence is already bound to another input".into(),
                ));
            }
            let result = execution.receipt(id, key).ok_or(Error::TriggerUncertain)?;
            return Ok(Some(TriggerClaim {
                occurrence_key: key.clone(),
                execution_id: execution.execution_id.clone(),
                input: input.clone(),
                result: Some(result),
            }));
        }
        if entry.active_occurrence.is_some() {
            return Err(Error::TriggerUncertain);
        }
        self.check_locked(&mut ledger, 0)?;
        if ledger.tool_calls >= self.policy.max_tool_calls {
            return Err(Error::ObjectiveLimit("tool_call_limit".into()));
        }
        let entry = ledger.triggers.get(id).unwrap();
        let (key, input, next) = if let Some((key, input)) = event {
            (key, input, None)
        } else {
            let Some(due) = entry.next_due_unix_ms.filter(|due| *due <= now) else {
                return Ok(None);
            };
            let (scheduled, skipped, next) = match entry.schedule {
                TriggerSchedule::Interval { every_ms, .. } => {
                    let skipped = (now - due) / every_ms;
                    let scheduled = due + skipped * every_ms;
                    let next = scheduled
                        .checked_add(every_ms)
                        .filter(|n| *n <= MAX_TIME)
                        .ok_or_else(|| {
                            Error::Input("trigger next timestamp exceeds portable range".into())
                        })?;
                    (scheduled, skipped, Some(next))
                }
                TriggerSchedule::At { .. } => (due, 0, None),
                TriggerSchedule::Event => unreachable!(),
            };
            (
                scheduled.to_string(),
                json!({"scheduled_for_unix_ms":scheduled,"observed_unix_ms":now,"skipped_occurrences":skipped}),
                next,
            )
        };
        let execution_id = uuid::Uuid::new_v4().to_string();
        ledger.tool_calls += 1;
        let entry = ledger.triggers.get_mut(id).unwrap();
        entry.next_due_unix_ms = next;
        entry.active_occurrence = Some(key.clone());
        entry.executions.insert(
            key.clone(),
            Execution {
                execution_id: execution_id.clone(),
                input: input.clone(),
                outcome: None,
            },
        );
        Ok(Some(TriggerClaim {
            occurrence_key: key,
            execution_id,
            input,
            result: None,
        }))
    }

    pub fn finish_trigger(
        &self,
        id: &str,
        key: &str,
        execution_id: &str,
        result: Value,
    ) -> Result<TriggerReceipt> {
        validate_value(&result)?;
        let mut ledger = self.lock()?;
        let entry = ledger
            .triggers
            .get_mut(id)
            .ok_or_else(|| Error::Input("trigger does not exist".into()))?;
        if entry.active_occurrence.as_deref() != Some(key) {
            return Err(Error::SessionConflict);
        }
        let execution = entry
            .executions
            .get_mut(key)
            .ok_or_else(|| Error::Storage("missing trigger claim".into()))?;
        if execution.execution_id != execution_id || execution.outcome.is_some() {
            return Err(Error::SessionConflict);
        }
        execution.outcome = Some(Outcome {
            value: result.clone(),
        });
        entry.active_occurrence = None;
        let _ = self.check_locked(&mut ledger, 0);
        Ok(TriggerReceipt {
            trigger_id: id.into(),
            occurrence_key: key.into(),
            execution_id: execution_id.into(),
            result,
        })
    }
}
