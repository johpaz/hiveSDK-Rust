//! Native Gemini Live transport and Hive's existing /realtime protocol.
mod config;
mod session;
pub(crate) use config::LiveConfig;
pub(crate) use session::serve;
#[cfg(test)]
pub(crate) use session::{End, LiveSession};

mod catalog;
pub(crate) use catalog::route;

pub(crate) use catalog::ensure_default;
