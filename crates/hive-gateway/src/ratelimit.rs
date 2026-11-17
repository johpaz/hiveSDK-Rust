//! Rate limits: a token bucket per key, configured by the administrator.
//!
//! Off until configured (`PUT /api/rate-limits`), so nothing changes by default. Each limit is
//! a number of events per minute; the bucket holds that many (a burst up to the limit) and
//! refills evenly. Keys are independent, so one noisy chat or user never uses up another's.
//!
//! What is limited, and by what key:
//! - `api`: REST requests, per authenticated user.
//! - `chat`: web chat messages (WebSocket), per user: every message is a model call.
//! - `channel`: messages from a Telegram, Discord, Slack or WhatsApp chat, per chat. Each one is
//!   a model call a stranger-reachable bot would otherwise pay for.
//! - `webhook`: requests to a channel's public webhook, per channel, counted before anything is read.
//! - `auth_failures`: requests with a missing or wrong token, all together. It only ever slows
//!   down callers that are failing to authenticate, never one with a valid token.
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

/// What a limit applies to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Api,
    Chat,
    Channel,
    Webhook,
    AuthFailure,
}

/// Events per minute for each kind; `None` is unlimited.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(crate) struct Config {
    pub api: Option<u32>,
    pub chat: Option<u32>,
    pub channel: Option<u32>,
    pub webhook: Option<u32>,
    pub auth_failures: Option<u32>,
}

pub(crate) const FIELDS: [&str; 5] = [
    "api_per_minute",
    "chat_per_minute",
    "channel_per_minute",
    "webhook_per_minute",
    "auth_failures_per_minute",
];

impl Config {
    pub(crate) fn limit(&self, kind: Kind) -> Option<u32> {
        match kind {
            Kind::Api => self.api,
            Kind::Chat => self.chat,
            Kind::Channel => self.channel,
            Kind::Webhook => self.webhook,
            Kind::AuthFailure => self.auth_failures,
        }
    }

    /// Parse and validate the body of `PUT /api/rate-limits`: only the known fields, each an
    /// integer of 1 to 100000 or `null` for unlimited.
    pub(crate) fn from_json(value: &serde_json::Value) -> Result<Self, String> {
        let object = value.as_object().ok_or("se esperaba un objeto")?;
        if let Some(unknown) = object.keys().find(|key| !FIELDS.contains(&key.as_str())) {
            return Err(format!("campo desconocido: {unknown}"));
        }
        let read = |key: &str| -> Result<Option<u32>, String> {
            match &object.get(key) {
                None | Some(serde_json::Value::Null) => Ok(None),
                Some(number) => number
                    .as_u64()
                    .filter(|n| (1..=100_000).contains(n))
                    .map(|n| Some(n as u32))
                    .ok_or_else(|| format!("{key} debe ser un entero entre 1 y 100000 (o null)")),
            }
        };
        Ok(Self {
            api: read("api_per_minute")?,
            chat: read("chat_per_minute")?,
            channel: read("channel_per_minute")?,
            webhook: read("webhook_per_minute")?,
            auth_failures: read("auth_failures_per_minute")?,
        })
    }

    pub(crate) fn to_json(self) -> serde_json::Value {
        serde_json::json!({"api_per_minute":self.api,"chat_per_minute":self.chat,"channel_per_minute":self.channel,
            "webhook_per_minute":self.webhook,"auth_failures_per_minute":self.auth_failures})
    }
}

struct Bucket {
    tokens: f64,
    last: Instant,
}

#[derive(Default)]
pub(crate) struct Limiter {
    buckets: HashMap<String, Bucket>,
}

impl Limiter {
    /// Whether `key`'s bucket has an event available, without taking it. `Err(ms)` says how
    /// long until one would be; a key never seen has all of its events.
    pub(crate) fn available(
        &mut self,
        key: &str,
        per_minute: u32,
        now: Instant,
    ) -> Result<(), u64> {
        let Some(bucket) = self.buckets.get(key) else {
            return Ok(());
        };
        let capacity = f64::from(per_minute);
        let rate = capacity / 60.0;
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        let tokens = (bucket.tokens + elapsed * rate).min(capacity);
        if tokens >= 1.0 {
            Ok(())
        } else {
            Err((((1.0 - tokens) / rate) * 1000.0).ceil() as u64)
        }
    }

    /// Take one event from `key`'s bucket, which holds `per_minute` and refills at that rate.
    /// `Err(ms)` says how long until an event would be allowed.
    pub(crate) fn check(&mut self, key: &str, per_minute: u32, now: Instant) -> Result<(), u64> {
        if self.buckets.len() > 10_000 {
            // Idle buckets are full again; dropping them changes nothing and bounds the memory.
            self.buckets
                .retain(|_, bucket| now.duration_since(bucket.last) < Duration::from_secs(120));
        }
        let capacity = f64::from(per_minute);
        let rate = capacity / 60.0;
        let bucket = self.buckets.entry(key.to_owned()).or_insert(Bucket {
            tokens: capacity,
            last: now,
        });
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(capacity);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            Err((((1.0 - bucket.tokens) / rate) * 1000.0).ceil() as u64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bucket_allows_a_burst_up_to_the_limit_then_refills_evenly() {
        let mut limiter = Limiter::default();
        let start = Instant::now();
        for _ in 0..6 {
            assert!(
                limiter.check("k", 6, start).is_ok(),
                "a burst up to the limit"
            );
        }
        let wait = limiter.check("k", 6, start).unwrap_err();
        assert_eq!(wait, 10_000, "6 per minute is one every 10 s");
        assert!(
            limiter
                .check("k", 6, start + Duration::from_secs(5))
                .is_err(),
            "half a token is not enough"
        );
        assert_eq!(
            limiter
                .check("k", 6, start + Duration::from_secs(5))
                .unwrap_err(),
            5_000
        );
        assert!(
            limiter
                .check("k", 6, start + Duration::from_secs(10))
                .is_ok()
        );
        assert!(
            limiter
                .check("k", 6, start + Duration::from_secs(10))
                .is_err(),
            "and only one"
        );
        // After a long quiet spell it holds the limit, never more.
        let later = start + Duration::from_secs(3600);
        for _ in 0..6 {
            assert!(limiter.check("k", 6, later).is_ok());
        }
        assert!(limiter.check("k", 6, later).is_err());
    }

    #[test]
    fn keys_are_independent_and_a_changed_limit_applies_at_once() {
        let mut limiter = Limiter::default();
        let now = Instant::now();
        assert!(limiter.check("a", 1, now).is_ok() && limiter.check("a", 1, now).is_err());
        assert!(
            limiter.check("b", 1, now).is_ok(),
            "another key has its own bucket"
        );
        // Raising the limit refills at the new rate and caps at the new capacity.
        assert!(limiter.check("a", 60, now + Duration::from_secs(2)).is_ok());
        for _ in 0..5 {
            let _ = limiter.check("c", 600, now);
        }
        assert!(limiter.check("c", 600, now).is_ok());
    }

    #[test]
    fn memory_is_bounded_and_idle_buckets_are_dropped() {
        let mut limiter = Limiter::default();
        let start = Instant::now();
        for n in 0..10_050 {
            let _ = limiter.check(&format!("k{n}"), 10, start);
        }
        let later = start + Duration::from_secs(300);
        let _ = limiter.check("fresh", 10, later);
        assert!(
            limiter.buckets.len() < 100,
            "{} buckets kept",
            limiter.buckets.len()
        );
        assert!(
            limiter.check("k5", 10, later).is_ok(),
            "a dropped bucket starts full again, which is what it would have refilled to"
        );
    }

    #[test]
    fn the_configuration_accepts_only_known_fields_in_range() {
        let ok = Config::from_json(&serde_json::json!({"api_per_minute":120,"channel_per_minute":6,"chat_per_minute":null})).unwrap();
        assert_eq!(
            (ok.api, ok.channel, ok.chat, ok.webhook, ok.auth_failures),
            (Some(120), Some(6), None, None, None)
        );
        assert_eq!(ok.limit(Kind::Api), Some(120));
        assert_eq!(ok.limit(Kind::Webhook), None);
        for bad in [
            serde_json::json!({"api_per_minute":0}),
            serde_json::json!({"api_per_minute":100001}),
            serde_json::json!({"api_per_minute":"60"}),
            serde_json::json!({"api_per_minute":1.5}),
            serde_json::json!({"api_per_minute":-1}),
            serde_json::json!({"bogus":1}),
            serde_json::json!([1]),
        ] {
            assert!(Config::from_json(&bad).is_err(), "{bad}");
        }
        assert_eq!(
            Config::from_json(&serde_json::json!({})).unwrap(),
            Config::default(),
            "an empty object clears every limit"
        );
    }
}
