//! Explicit summary claims and attributable auxiliary cost under the root budget.
use super::{Ledger, Objective};
use crate::memory::{ContextSource, ContextSummary};
use crate::{Error, Message, Result, Usage};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryInput {
    pub source: ContextSource,
    pub messages: Vec<Message>,
}
impl SummaryInput {
    fn validate(&self) -> Result<()> {
        if crate::context::serialized_bytes(self)? > 1024 * 1024 {
            return Err(Error::Input("summary input exceeds 1 MiB JSON".into()));
        }
        crate::memory::validate_summary_messages(&self.source, &self.messages)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryCost {
    pub model_calls: u32,
    pub tool_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub unmetered_model_calls: u32,
    pub pending_model_calls: u32,
    pub reserved_output_tokens: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryResult {
    pub key: String,
    pub execution_id: String,
    pub summary: ContextSummary,
    pub cost: SummaryCost,
}
#[derive(Debug, Serialize)]
pub struct SummaryClaim {
    pub execution_id: String,
    pub result: Option<SummaryResult>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SummaryEntry {
    input: SummaryInput,
    execution_id: String,
    content: Option<String>,
    pub(super) models: BTreeMap<u32, Option<Usage>>,
    pub(super) tool_calls: u32,
}
fn validate_key(key: &str) -> Result<()> {
    if key.trim().is_empty() || key.len() > 256 {
        return Err(Error::Input(
            "summary requires a key of 1..256 bytes".into(),
        ));
    }
    Ok(())
}
fn validate_content(content: &str) -> Result<()> {
    if content.trim().is_empty() || content.len() > 64 * 1024 {
        return Err(Error::Input(
            "summary content must be nonempty and <=64 KiB UTF-8".into(),
        ));
    }
    Ok(())
}
pub(super) fn check_scope(ledger: &Ledger, key: &str, depth: u32) -> Result<()> {
    if depth == 0 {
        return Err(Error::Input(
            "summary auxiliary work requires depth >=1".into(),
        ));
    }
    let entry = ledger
        .summaries
        .get(key)
        .ok_or_else(|| Error::Input("summary does not exist".into()))?;
    if entry.content.is_some() {
        return Err(Error::State("summary scope is closed".into()));
    }
    Ok(())
}
impl SummaryEntry {
    pub(super) fn validate(&self, key: &str) -> Result<()> {
        validate_key(key)?;
        self.input.validate()?;
        if self.execution_id.is_empty() || self.tool_calls == 0 {
            return Err(Error::Storage(
                "summary entry has no identity or initial attempt".into(),
            ));
        }
        if let Some(content) = &self.content {
            validate_content(content)?;
        }
        Ok(())
    }
    pub(super) fn cost(&self, ledger: &Ledger) -> SummaryCost {
        let mut cost = SummaryCost {
            model_calls: self.models.len() as u32,
            tool_calls: self.tool_calls,
            ..Default::default()
        };
        for (id, usage) in &self.models {
            if let Some(usage) = usage {
                cost.input_tokens = cost.input_tokens.saturating_add(usage.input_tokens);
                cost.output_tokens = cost.output_tokens.saturating_add(usage.output_tokens);
            } else if ledger.settled.contains(id) {
                cost.unmetered_model_calls += 1;
            } else {
                cost.pending_model_calls += 1;
            }
            cost.reserved_output_tokens = cost
                .reserved_output_tokens
                .saturating_add(*ledger.reservations.get(id).unwrap_or(&0));
        }
        cost
    }
    fn result(&self, key: &str, ledger: &Ledger) -> Option<SummaryResult> {
        self.content.as_ref().map(|content| SummaryResult {
            key: key.into(),
            execution_id: self.execution_id.clone(),
            summary: ContextSummary {
                id: key.into(),
                source: self.input.source.clone(),
                content: content.clone(),
            },
            cost: self.cost(ledger),
        })
    }
}
impl Objective {
    pub fn check_summary(&self, key: &str, depth: u32) -> Result<()> {
        let mut ledger = self.lock()?;
        self.check_locked(&mut ledger, depth)?;
        check_scope(&ledger, key, depth)
    }
    pub fn begin_summary(&self, key: String, input: SummaryInput) -> Result<SummaryClaim> {
        validate_key(&key)?;
        input.validate()?;
        let mut ledger = self.lock()?;
        if let Some(entry) = ledger.summaries.get(&key) {
            if entry.input.source != input.source {
                return Err(Error::Input(
                    "summary key is already bound to another source".into(),
                ));
            }
            let result = entry.result(&key, &ledger).ok_or(Error::SummaryUncertain)?;
            return Ok(SummaryClaim {
                execution_id: entry.execution_id.clone(),
                result: Some(result),
            });
        }
        self.check_locked(&mut ledger, 0)?;
        if ledger.tool_calls >= self.policy.max_tool_calls {
            return Err(Error::ObjectiveLimit("tool_call_limit".into()));
        }
        if ledger.summaries.len() >= 64 {
            return Err(Error::Input(
                "at most 64 summary entries are supported per objective".into(),
            ));
        }
        let execution_id = uuid::Uuid::new_v4().to_string();
        ledger.tool_calls += 1;
        ledger.summaries.insert(
            key,
            SummaryEntry {
                input,
                execution_id: execution_id.clone(),
                content: None,
                models: BTreeMap::new(),
                tool_calls: 1,
            },
        );
        Ok(SummaryClaim {
            execution_id,
            result: None,
        })
    }
    pub fn finish_summary(
        &self,
        key: &str,
        execution_id: &str,
        content: String,
    ) -> Result<SummaryResult> {
        validate_content(&content)?;
        let mut ledger = self.lock()?;
        let entry = ledger
            .summaries
            .get_mut(key)
            .ok_or_else(|| Error::Input("summary does not exist".into()))?;
        if entry.execution_id != execution_id || entry.content.is_some() {
            return Err(Error::SessionConflict);
        }
        entry.content = Some(content);
        let _ = self.check_locked(&mut ledger, 0);
        Ok(ledger.summaries[key]
            .result(key, &ledger)
            .expect("known summary"))
    }
    pub fn reserve_summary_model(
        &self,
        key: &str,
        depth: u32,
        cap: Option<u64>,
    ) -> Result<super::ModelPermit> {
        self.reserve_model_scoped(depth, cap, Some(key))
    }
    pub fn count_summary_tool(&self, key: &str, depth: u32) -> Result<()> {
        self.count_tool_scoped(depth, Some(key))
    }
}
