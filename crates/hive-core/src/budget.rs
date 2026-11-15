use crate::{Action, Engine, Error, Result, StopReason};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn now_ms() -> Result<u64> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::State("system clock precedes Unix epoch".into()))?
            .as_millis(),
    )
    .map_err(|_| Error::State("system clock exceeds timestamp range".into()))
}

impl Engine {
    pub(crate) fn deadline_expired(&self) -> Result<bool> {
        if self.policy.max_run_ms.is_some()
            && self.deadline_unix_ms.is_none()
            && !matches!(self.action, Action::Idle)
        {
            return Err(Error::State("timed run has no persisted deadline".into()));
        }
        match self.deadline_unix_ms {
            Some(deadline) => Ok(now_ms()? >= deadline),
            None => Ok(false),
        }
    }

    pub(crate) fn output_cap(&self) -> Option<u64> {
        match (
            self.policy.max_output_tokens,
            self.policy.max_total_output_tokens,
        ) {
            (Some(cap), Some(total)) => {
                Some(cap.min(total.saturating_sub(self.stats.output_tokens)))
            }
            (None, Some(total)) => Some(total.saturating_sub(self.stats.output_tokens)),
            (cap, None) => cap,
        }
    }

    /// Recheck persisted policy before a dispatch; never resets a run's clock or usage.
    pub fn preflight(&mut self) -> Result<()> {
        if matches!(
            self.action,
            Action::Idle | Action::Complete { .. } | Action::Stopped { .. }
        ) {
            return Ok(());
        }
        if self.deadline_expired()? {
            self.stats.reserved_output_tokens = 0;
            self.stats.reserved_input_tokens = 0;
            self.stop(StopReason::DeadlineExceeded);
            return Ok(());
        }
        if matches!(self.action, Action::Model { .. }) {
            self.request_model()
        } else {
            self.refresh_pending_tool()
        }
    }
}
