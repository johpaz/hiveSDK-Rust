use super::{Inner, fs::resolve, invalid};
use crate::{Error, Result, provider::CancellationToken};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};

const MAX_TIMEOUT_MS: u64 = 600_000;

/// Dropping a JoinHandle normally detaches it, leaving inherited pipes alive.
struct PipeTask<T>(tokio::task::JoinHandle<T>);
impl<T> Drop for PipeTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn default_timeout() -> u64 {
    30_000
}

/// Programs are matched by exact name and executed directly, never through a
/// shell. The child gets a cleared environment: the host `PATH` plus `env`.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandPolicy {
    pub allowed_programs: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_timeout")]
    pub max_timeout_ms: u64,
}

impl CommandPolicy {
    pub(super) fn validate(&self) -> Result<()> {
        if self.allowed_programs.is_empty() {
            return Err(invalid("command policy needs at least one allowed program"));
        }
        if self.allowed_programs.iter().any(|name| {
            name.is_empty() || name.contains(['/', '\\']) || name.contains(char::is_whitespace)
        }) {
            return Err(invalid(
                "allowed programs must be bare names without separators",
            ));
        }
        if self.max_timeout_ms == 0 || self.max_timeout_ms > MAX_TIMEOUT_MS {
            return Err(invalid(format!(
                "max_timeout_ms must be 1..={MAX_TIMEOUT_MS}"
            )));
        }
        Ok(())
    }
}

/// Keep draining after the cap so a chatty child never blocks on a full pipe.
async fn capped(mut stream: impl AsyncRead + Unpin, max: usize) -> (Vec<u8>, bool) {
    let (mut kept, mut truncated, mut buffer) = (Vec::new(), false, [0u8; 8192]);
    while let Ok(read) = stream.read(&mut buffer).await {
        if read == 0 {
            break;
        }
        let room = max.saturating_sub(kept.len());
        truncated |= read > room;
        kept.extend_from_slice(&buffer[..read.min(room)]);
    }
    (kept, truncated)
}

pub(super) async fn run(
    inner: &Inner,
    arguments: &Value,
    cancel: &CancellationToken,
) -> Result<Value> {
    let policy = inner
        .command
        .as_ref()
        .ok_or_else(|| invalid("commands are disabled"))?;
    let program = arguments["program"].as_str().unwrap_or_default();
    if !policy.allowed_programs.iter().any(|name| name == program) {
        return Err(invalid("program is not in the allowlist"));
    }
    let args: Vec<&str> = arguments["args"]
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let cwd = resolve(&inner.root, arguments["cwd"].as_str().unwrap_or("."), false)?;
    if !cwd.is_dir() {
        return Err(invalid("cwd is not a directory"));
    }
    let timeout = arguments["timeout_ms"]
        .as_u64()
        .unwrap_or(policy.max_timeout_ms);
    if timeout > policy.max_timeout_ms {
        return Err(invalid("timeout_ms exceeds the policy maximum"));
    }
    let stdin = arguments["stdin"].as_str();

    let mut command = Command::new(program);
    command
        .args(&args)
        .current_dir(&cwd)
        .env_clear()
        .envs(&policy.env)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if !policy.env.contains_key("PATH")
        && let Some(path) = std::env::var_os("PATH")
    {
        command.env("PATH", path);
    }
    let mut child = command
        .spawn()
        .map_err(|error| Error::State(format!("could not start program: {}", error.kind())))?;
    let _input = if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let text = text.to_owned();
        Some(PipeTask(tokio::spawn(async move {
            let _ = pipe.write_all(text.as_bytes()).await;
        })))
    } else {
        None
    };
    let max = inner.max_output;
    let out = PipeTask(tokio::spawn(capped(
        child.stdout.take().expect("piped stdout"),
        max,
    )));
    let err = PipeTask(tokio::spawn(capped(
        child.stderr.take().expect("piped stderr"),
        max,
    )));

    let outcome = tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        status = tokio::time::timeout(Duration::from_millis(timeout), child.wait()) => Some(status),
    };
    let (exit_code, timed_out) = match outcome {
        None => {
            let _ = child.kill().await;
            return Err(Error::Cancelled);
        }
        Some(Ok(status)) => (
            status
                .map_err(|e| Error::State(format!("wait failed: {}", e.kind())))?
                .code(),
            false,
        ),
        Some(Err(_)) => {
            let _ = child.kill().await;
            (None, true)
        }
    };
    // A grandchild holding the pipes must not hang the tool after the kill.
    let drain = |mut handle: PipeTask<(Vec<u8>, bool)>| async move {
        tokio::time::timeout(Duration::from_secs(2), &mut handle.0)
            .await
            .ok()
            .and_then(|joined| joined.ok())
            .unwrap_or_else(|| (Vec::new(), true))
    };
    let ((stdout, out_cut), (stderr, err_cut)) = (drain(out).await, drain(err).await);
    Ok(json!({
        "exit_code":exit_code,"timed_out":timed_out,
        "stdout":String::from_utf8_lossy(&stdout),"stderr":String::from_utf8_lossy(&stderr),
        "truncated":out_cut || err_cut
    }))
}
