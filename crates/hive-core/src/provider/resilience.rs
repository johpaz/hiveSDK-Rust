//! Explicit retries and a circuit breaker for provider HTTP calls.
//!
//! Off unless the provider config has a `resilience` object. The rule that matters
//! is *what may be repeated*: only a request that provably was not processed. That is
//! a connection that never opened, or the server saying so itself with HTTP 429 or
//! 503. A timeout, a dropped stream, a 5xx other than 503 or any response that began
//! to arrive may have been processed (and billed), so those are never retried. Retries
//! happen inside the one logical call, so they cost no extra model call or budget
//! reservation, and they sit inside the call's own timeout and turn deadline.
//!
//! The breaker counts consecutive failures of the provider endpoint and, once it is
//! open, fails calls immediately without sending anything; after the cool-down a single
//! probe call decides whether it closes.
use crate::{Error, Result};
use serde::Deserialize;
use serde_json::Value;
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct RetryPolicy {
    /// Total tries including the first, 1..=5. 1 means no retries.
    pub max_attempts: u32,
    pub base_delay_ms: u64,
    /// Longest wait between tries, also the longest `Retry-After` honoured.
    pub max_delay_ms: u64,
}
impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 1,
            base_delay_ms: 500,
            max_delay_ms: 30_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct BreakerPolicy {
    pub failure_threshold: u32,
    pub cooldown_ms: u64,
}
impl Default for BreakerPolicy {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            cooldown_ms: 30_000,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    retry: RetryPolicy,
    circuit_breaker: Option<BreakerPolicy>,
}

#[derive(Default)]
struct BreakerState {
    consecutive: u32,
    open_until: Option<Instant>,
    /// When the single probe call started. A probe that never reports back (its call
    /// was cancelled) expires after one cool-down so the breaker cannot stay stuck.
    probing_since: Option<Instant>,
}

pub(super) struct Resilience {
    pub retry: RetryPolicy,
    breaker: Option<BreakerPolicy>,
    state: Mutex<BreakerState>,
}

/// What a call to the provider showed about its health.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Health {
    /// The server answered (any status that is not a server-side failure).
    Reachable,
    /// Connection failure, timeout, 429 or 5xx.
    Failing,
}

fn invalid(message: &str) -> Error {
    Error::Input(message.into())
}

impl Resilience {
    pub(super) fn from_value(value: Value) -> Result<Self> {
        let config: Config = serde_json::from_value(value)
            .map_err(|error| Error::Input(format!("invalid resilience config: {error}")))?;
        let retry = &config.retry;
        if !(1..=5).contains(&retry.max_attempts)
            || !(1..=60_000).contains(&retry.base_delay_ms)
            || retry.max_delay_ms < retry.base_delay_ms
            || retry.max_delay_ms > 300_000
        {
            return Err(invalid(
                "retry needs max_attempts 1..=5, base_delay_ms 1..=60000 and max_delay_ms between base_delay_ms and 300000",
            ));
        }
        if let Some(breaker) = &config.circuit_breaker
            && (!(1..=100).contains(&breaker.failure_threshold)
                || !(100..=3_600_000).contains(&breaker.cooldown_ms))
        {
            return Err(invalid(
                "circuit_breaker needs failure_threshold 1..=100 and cooldown_ms 100..=3600000",
            ));
        }
        Ok(Self {
            retry: config.retry,
            breaker: config.circuit_breaker,
            state: Mutex::new(BreakerState::default()),
        })
    }

    /// Whether a call may be sent now. An open breaker refuses without any network traffic.
    pub(super) fn admit(&self) -> Result<()> {
        let Some(policy) = &self.breaker else {
            return Ok(());
        };
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::State("poisoned circuit breaker".into()))?;
        let Some(until) = state.open_until else {
            return Ok(());
        };
        let now = Instant::now();
        let cooldown = Duration::from_millis(policy.cooldown_ms);
        let probe_running = state
            .probing_since
            .is_some_and(|since| now.duration_since(since) < cooldown);
        if now < until || probe_running {
            let wait = until.saturating_duration_since(now).as_millis();
            return Err(Error::Provider(format!(
                "PROVIDER_CIRCUIT_OPEN: the provider failed {} times in a row; not sending (retry in about {} ms)",
                policy.failure_threshold, wait
            )));
        }
        // Cool-down over: this call is the single probe.
        state.probing_since = Some(now);
        Ok(())
    }

    pub(super) fn record(&self, health: Health) {
        let Some(policy) = &self.breaker else { return };
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        match health {
            Health::Reachable => *state = BreakerState::default(),
            Health::Failing => {
                state.consecutive = state.consecutive.saturating_add(1);
                state.probing_since = None;
                if state.consecutive >= policy.failure_threshold {
                    state.open_until =
                        Some(Instant::now() + Duration::from_millis(policy.cooldown_ms));
                }
            }
        }
    }

    /// How long to wait before try number `retry` (0 for the first retry), or `None`
    /// when the server asked for longer than `max_delay_ms` and the call should end.
    pub(super) fn delay(&self, retry: u32, retry_after: Option<Duration>) -> Option<Duration> {
        let max = self.retry.max_delay_ms;
        let ceiling = self
            .retry
            .base_delay_ms
            .saturating_mul(1u64 << retry.min(20))
            .min(max);
        // Equal jitter: half the ceiling plus a random share of the other half.
        let jitter = (uuid::Uuid::new_v4().as_u128() % u128::from(ceiling / 2 + 1)) as u64;
        let mut wait = Duration::from_millis(ceiling / 2 + jitter);
        if let Some(asked) = retry_after {
            if asked.as_millis() > u128::from(max) {
                return None;
            }
            wait = wait.max(asked);
        }
        Some(wait)
    }
}

/// `Retry-After` in whole seconds (the HTTP-date form is ignored).
pub(super) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}
