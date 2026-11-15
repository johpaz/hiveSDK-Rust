use super::Objective;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReviewEvidence {
    pub evaluation_id: String,
    pub justification: String,
}
impl ReviewEvidence {
    fn validate(&self) -> Result<()> {
        if self.evaluation_id.trim().is_empty()
            || self.evaluation_id.len() > 256
            || self.justification.trim().is_empty()
            || self.justification.len() > 4096
        {
            return Err(Error::Input(
                "model review requires bounded evaluation_id and justification".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct VerificationVerdict {
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Value>,
}
impl VerificationVerdict {
    fn validate(&self) -> Result<()> {
        if self
            .reason
            .as_ref()
            .is_some_and(|reason| reason.len() > 4096)
            || serde_json::to_vec(self)?.len() > MAX_BYTES
        {
            return Err(Error::Input(
                "verification verdict exceeds its byte limit".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationResult {
    pub key: String,
    pub execution_id: String,
    pub verdict: VerificationVerdict,
}

#[derive(Debug, Serialize)]
pub struct VerificationClaim {
    pub execution_id: String,
    pub result: Option<VerificationResult>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerificationEntry {
    input: Value,
    model_review: Option<ReviewEvidence>,
    execution_id: String,
    verdict: Option<VerificationVerdict>,
}
impl VerificationEntry {
    pub(crate) fn validate(&self, key: &str) -> Result<()> {
        validate_key_input(key, &self.input)?;
        if self.execution_id.is_empty() {
            return Err(Error::Storage("verification has no execution id".into()));
        }
        if let Some(review) = &self.model_review {
            review.validate()?;
        }
        if let Some(verdict) = &self.verdict {
            verdict.validate()?;
        }
        Ok(())
    }
    fn result(&self, key: &str) -> Option<VerificationResult> {
        self.verdict.clone().map(|verdict| VerificationResult {
            key: key.into(),
            execution_id: self.execution_id.clone(),
            verdict,
        })
    }
}

fn validate_key_input(key: &str, input: &Value) -> Result<()> {
    if key.trim().is_empty() || key.len() > 256 || serde_json::to_vec(input)?.len() > MAX_BYTES {
        return Err(Error::Input(
            "verification needs a nonempty key <=256 bytes and input <=64 KiB".into(),
        ));
    }
    Ok(())
}

impl Objective {
    pub fn begin_verification(
        &self,
        key: String,
        input: Value,
        model_review: Option<ReviewEvidence>,
    ) -> Result<VerificationClaim> {
        validate_key_input(&key, &input)?;
        if let Some(review) = &model_review {
            review.validate()?;
        }
        let mut ledger = self.lock()?;
        if let Some(entry) = ledger.verifications.get(&key) {
            if entry.input != input || entry.model_review != model_review {
                return Err(Error::Input(
                    "verification key is already bound to another input or review policy".into(),
                ));
            }
            let result = entry.result(&key).ok_or(Error::VerificationUncertain)?;
            return Ok(VerificationClaim {
                execution_id: entry.execution_id.clone(),
                result: Some(result),
            });
        }
        self.check_locked(&mut ledger, 0)?;
        if ledger.tool_calls >= self.policy.max_tool_calls {
            return Err(Error::ObjectiveLimit("tool_call_limit".into()));
        }
        let execution_id = uuid::Uuid::new_v4().to_string();
        ledger.tool_calls += 1;
        ledger.verifications.insert(
            key,
            VerificationEntry {
                input,
                model_review,
                execution_id: execution_id.clone(),
                verdict: None,
            },
        );
        Ok(VerificationClaim {
            execution_id,
            result: None,
        })
    }

    pub fn finish_verification(
        &self,
        key: &str,
        execution_id: &str,
        verdict: VerificationVerdict,
    ) -> Result<VerificationResult> {
        verdict.validate()?;
        let mut ledger = self.lock()?;
        let entry = ledger
            .verifications
            .get_mut(key)
            .ok_or_else(|| Error::Input("verification does not exist".into()))?;
        if entry.execution_id != execution_id || entry.verdict.is_some() {
            return Err(Error::SessionConflict);
        }
        entry.verdict = Some(verdict.clone());
        // Save a known late result, but never give the objective another deadline.
        let _ = self.check_locked(&mut ledger, 0);
        Ok(VerificationResult {
            key: key.into(),
            execution_id: execution_id.into(),
            verdict,
        })
    }
}
