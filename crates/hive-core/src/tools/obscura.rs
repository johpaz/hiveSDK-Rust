//! Browser tools backed by Obscura, a lightweight headless browser.
//!
//! The SDK drives `obscura mcp` over stdio, the same way `hive` does, and exposes
//! its tools as `obscura_*`. There is no Chromium, Playwright or Puppeteer
//! anywhere in this path: Obscura is a single small binary with its own JavaScript
//! engine, which is why it is the browser of this SDK.
//!
//! The session starts on the first call and keeps ONE live page; element refs
//! (`e3`) are only valid for the last snapshot. A timeout, cancellation or lost
//! connection leaves the page in an unknown state, so that session is discarded
//! and the next call starts a new one: an uncertain action is never replayed.
//!
//! Obscura blocks localhost and private networks itself (SSRF protection);
//! `allow_private_network` turns that off. Tools that run arbitrary page
//! JavaScript or expose cookies and storage are hidden unless the policy opts in.
use super::{Inner, fs, invalid, obscura_install, web::check_host};
use crate::{Error, Result, media, provider::CancellationToken};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

/// MCP revisions this session accepts from the server. Obscura 0.2.x answers with
/// an older revision than the one requested, so the reply is accepted if it is one
/// of these rather than required to match.
const KNOWN_REVISIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
const MAX_LINE: usize = 16 * 1024 * 1024;
const MAX_MESSAGES: usize = 4096;

fn default_startup() -> u64 {
    10_000
}
fn default_call() -> u64 {
    60_000
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObscuraPolicy {
    /// Obscura binary. Otherwise `OBSCURA_PATH`, then `obscura` on `PATH`, then
    /// `~/.hive/bin/obscura` and `~/.hivecode/bin/obscura` (shared with Hive).
    pub executable: Option<String>,
    pub stealth: bool,
    /// Proxy URL passed as `--proxy`.
    pub proxy: Option<String>,
    pub user_agent: Option<String>,
    /// Let pages reach localhost and private networks (`--allow-private-network`).
    pub allow_private_network: bool,
    /// When non-empty, pages may only be on these exact hosts. Checked on the URL
    /// given to `obscura_navigate`/`obscura_tab_new` and on where the page ends up.
    pub allowed_hosts: Vec<String>,
    /// Offer `obscura_evaluate` (arbitrary JavaScript in the page). Off by default.
    pub allow_evaluate: bool,
    /// Offer the cookie and storage-state tools, which hand session secrets to the
    /// model. Off by default.
    pub allow_credentials: bool,
    /// Download the latest Obscura when none is found (see `obscura_install`). On by
    /// default; also off when `HIVE_OBSCURA_AUTO_INSTALL` is `0` or `false`. An
    /// explicit `executable`/`OBSCURA_PATH` that does not exist is never replaced by a download.
    pub auto_install: bool,
    /// Where a downloaded Obscura goes: an absolute directory, `~/.hive/bin` by default
    /// (shared with Hive, and already searched for it).
    pub install_dir: Option<String>,
    pub startup_timeout_ms: u64,
    /// Default per-call timeout; a call may ask for less or more (up to 180 s).
    pub call_timeout_ms: u64,
}

impl Default for ObscuraPolicy {
    fn default() -> Self {
        Self {
            executable: None,
            stealth: false,
            proxy: None,
            user_agent: None,
            allow_private_network: false,
            allowed_hosts: vec![],
            allow_evaluate: false,
            allow_credentials: false,
            auto_install: true,
            install_dir: None,
            startup_timeout_ms: default_startup(),
            call_timeout_ms: default_call(),
        }
    }
}

impl ObscuraPolicy {
    pub(super) fn validate(&self) -> Result<()> {
        if !(1..=120_000).contains(&self.startup_timeout_ms)
            || !(1..=180_000).contains(&self.call_timeout_ms)
        {
            return Err(invalid(
                "obscura timeouts must be 1..=120000 (startup) and 1..=180000 (call) ms",
            ));
        }
        let clean = |value: &str| {
            !value.is_empty()
                && value.len() <= 2048
                && !value.chars().any(|c| c.is_control() || c.is_whitespace())
        };
        if self
            .executable
            .as_deref()
            .is_some_and(|path| path.is_empty() || path.contains('\0'))
        {
            return Err(invalid("obscura executable must be a non-empty path"));
        }
        if self
            .install_dir
            .as_deref()
            .is_some_and(|dir| dir.contains('\0') || !std::path::Path::new(dir).is_absolute())
        {
            return Err(invalid("obscura install_dir must be an absolute path"));
        }
        if self.proxy.as_deref().is_some_and(|proxy| !clean(proxy)) {
            return Err(invalid("obscura proxy must be a URL without whitespace"));
        }
        if self.user_agent.as_deref().is_some_and(|agent| {
            agent.is_empty() || agent.len() > 512 || agent.chars().any(char::is_control)
        }) {
            return Err(invalid(
                "obscura user_agent must be 1..512 printable characters",
            ));
        }
        Ok(())
    }

    fn arguments(&self) -> Vec<String> {
        let mut args = vec!["mcp".to_owned()];
        if self.stealth {
            args.push("--stealth".into());
        }
        if let Some(proxy) = &self.proxy {
            args.extend(["--proxy".into(), proxy.clone()]);
        }
        if let Some(agent) = &self.user_agent {
            args.extend(["--user-agent".into(), agent.clone()]);
        }
        if self.allow_private_network {
            args.push("--allow-private-network".into());
        }
        args
    }

    /// The Obscura to run, if one is installed: `executable`, `OBSCURA_PATH`, `PATH`, then
    /// `~/.hive/bin` and `~/.hivecode/bin`. `None` means none is installed anywhere; an
    /// explicit path that does not exist is an error, never a reason to download.
    fn locate(&self) -> Result<Option<PathBuf>> {
        let os = std::env::consts::OS;
        let binary = obscura_install::binary_name(os);
        let explicit = self.executable.clone().or_else(|| {
            std::env::var("OBSCURA_PATH")
                .ok()
                .filter(|value| !value.is_empty())
        });
        if let Some(path) = explicit {
            let path = PathBuf::from(path);
            return path
                .is_file()
                .then_some(Some(path))
                .ok_or_else(|| invalid("the obscura executable does not exist"));
        }
        let from_path = std::env::var_os("PATH")
            .into_iter()
            .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
            .map(|dir| dir.join(binary));
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        let managed = home.into_iter().flat_map(|home| {
            [
                home.join(".hive").join("bin").join(binary),
                home.join(".hivecode").join("bin").join(binary),
            ]
        });
        Ok(from_path
            .chain(managed)
            .find(|candidate| candidate.is_file()))
    }

    fn auto_install_enabled(&self) -> bool {
        self.auto_install
            && !matches!(
                std::env::var("HIVE_OBSCURA_AUTO_INSTALL").as_deref(),
                Ok("0" | "false")
            )
    }

    fn install_directory(&self) -> Result<PathBuf> {
        self.install_dir
            .as_ref()
            .map(PathBuf::from)
            .or_else(obscura_install::default_dir)
            .ok_or_else(|| invalid("cannot determine a home directory; set obscura.install_dir"))
    }

    /// Install (or with `update`, refresh) the latest release now.
    pub(super) async fn install_latest(&self, update: bool) -> Result<obscura_install::Installed> {
        obscura_install::install(
            &obscura_install::Sources::github(),
            &self.install_directory()?,
            std::env::consts::OS,
            std::env::consts::ARCH,
            update,
        )
        .await
    }
}

fn state(message: &str) -> Error {
    Error::State(format!("OBSCURA_ERROR: {message}"))
}

/// A live `obscura mcp` process.
pub(crate) struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Session {
    async fn start(policy: &ObscuraPolicy) -> Result<Self> {
        let executable = match policy.locate()? {
            Some(path) => path,
            None if policy.auto_install_enabled() => policy.install_latest(false).await?.path,
            None => {
                return Err(invalid(
                    "Obscura is not installed and automatic installation is off: call installObscura(), put it on PATH or in ~/.hive/bin, or set OBSCURA_PATH or obscura.executable",
                ));
            }
        };
        let mut command = Command::new(executable);
        command.args(policy.arguments()).env_clear();
        for key in [
            "PATH",
            "HOME",
            "USER",
            "USERPROFILE",
            "TMPDIR",
            "TMP",
            "TEMP",
            "SystemRoot",
            "WINDIR",
            "COMSPEC",
            "PATHEXT",
            "LANG",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| state("cannot start the obscura process"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| state("missing obscura stdin"))?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| state("missing obscura stdout"))?,
        );
        let mut session = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        let handshake = tokio::time::timeout(Duration::from_millis(policy.startup_timeout_ms), async {
            let reply = session
                .request("initialize", json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"hive-sdk-rust","version":env!("CARGO_PKG_VERSION")}}))
                .await?;
            let revision = reply["protocolVersion"].as_str().unwrap_or_default();
            if !KNOWN_REVISIONS.contains(&revision) {
                return Err(state("the server answered with an unsupported MCP revision"));
            }
            session.notify("notifications/initialized").await
        })
        .await
        .map_err(|_| state("obscura did not complete the MCP handshake in time"))?;
        handshake?;
        Ok(session)
    }

    async fn send(&mut self, message: &Value) -> Result<()> {
        let mut line = message.to_string().into_bytes();
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .await
            .map_err(|_| state("the obscura process is not accepting input"))?;
        self.stdin
            .flush()
            .await
            .map_err(|_| state("the obscura process is not accepting input"))
    }

    async fn notify(&mut self, method: &str) -> Result<()> {
        self.send(&json!({"jsonrpc":"2.0","method":method})).await
    }

    /// One JSON-RPC round trip. Notifications are skipped; server-initiated
    /// requests get `ping` answered and everything else refused.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        for _ in 0..MAX_MESSAGES {
            let mut line = Vec::new();
            let read = (&mut self.stdout)
                .take(MAX_LINE as u64 + 1)
                .read_until(b'\n', &mut line)
                .await
                .map_err(|_| state("cannot read from obscura"))?;
            if read == 0 {
                return Err(state("the obscura process closed the connection"));
            }
            if line.len() > MAX_LINE {
                return Err(state("obscura sent a message above the size limit"));
            }
            let Ok(message) = serde_json::from_slice::<Value>(&line) else {
                continue;
            };
            match (
                message.get("id"),
                message.get("method").and_then(Value::as_str),
            ) {
                (Some(received), None) if *received == json!(id) => {
                    if let Some(error) = message.get("error") {
                        return Err(state(
                            error["message"]
                                .as_str()
                                .unwrap_or("the server reported an error"),
                        ));
                    }
                    return Ok(message["result"].clone());
                }
                (Some(request), Some(method)) => {
                    let reply = if method == "ping" {
                        json!({"jsonrpc":"2.0","id":request,"result":{}})
                    } else {
                        json!({"jsonrpc":"2.0","id":request,"error":{"code":-32601,"message":"method not supported"}})
                    };
                    self.send(&reply).await?;
                }
                _ => {}
            }
        }
        Err(state("obscura sent too many messages without answering"))
    }

    /// Stop the process now, without an async runtime. For exit paths.
    pub(crate) fn kill_now(mut self) {
        let _ = self.child.start_kill();
    }
}

fn selector() -> Value {
    json!({"type":"string","minLength":1,"maxLength":2000})
}
fn reference() -> Value {
    json!({"type":"string","minLength":1,"maxLength":64})
}

type Spec = (&'static str, &'static str, Value, Vec<&'static str>);

pub(super) fn specs(policy: &ObscuraPolicy) -> Vec<Spec> {
    let max_chars = json!({"type":"integer","minimum":1,"maximum":1_000_000});
    let limit = json!({"type":"integer","minimum":1,"maximum":1000});
    let seconds = json!({"type":"integer","minimum":1,"maximum":120});
    let url = json!({"type":"string","minLength":1,"maxLength":4096});
    let path = json!({"type":"string","minLength":1,"maxLength":4096});
    let create_dirs = json!({"type":"boolean"});
    let inches = json!({"type":"number","minimum":0,"maximum":100});
    let mut tools: Vec<Spec> = vec![
        (
            "obscura_navigate",
            "Open an http(s) page and wait for it to load, keeping one live session. Returns the final URL and title: read the page next with obscura_snapshot or obscura_markdown.",
            json!({"url":url,"waitUntil":{"type":"string","enum":["load","domcontentloaded","networkidle0"]}}),
            vec!["url"],
        ),
        (
            "obscura_back",
            "Go back in the page history.",
            json!({}),
            vec![],
        ),
        (
            "obscura_forward",
            "Go forward in the page history.",
            json!({}),
            vec![],
        ),
        (
            "obscura_reload",
            "Reload the current page.",
            json!({}),
            vec![],
        ),
        (
            "obscura_close",
            "Close the current page and reset the session state.",
            json!({}),
            vec![],
        ),
        (
            "obscura_snapshot",
            "The page as text: URL, title, readable body text and interactive element refs. The preferred first read after navigating.",
            json!({"max_chars":max_chars}),
            vec![],
        ),
        (
            "obscura_markdown",
            "The page as Markdown (headings, paragraphs, lists, links, code): denser than plain text for articles.",
            json!({"max_chars":max_chars}),
            vec![],
        ),
        (
            "obscura_links",
            "Every link on the page as {text, href}, optionally only same-origin ones.",
            json!({"limit":limit,"internal_only":{"type":"boolean"}}),
            vec![],
        ),
        (
            "obscura_interactive_elements",
            "Every clickable or typeable element with a ref. Use it before acting, instead of guessing selectors.",
            json!({"limit":limit}),
            vec![],
        ),
        (
            "obscura_detect_forms",
            "Every form with its action, method and fields. Understand a form before filling it.",
            json!({}),
            vec![],
        ),
        (
            "obscura_extract",
            "Extract a structured object: a map of field name to CSS selector (`selector@attr` for attributes, a `field[]` name for arrays).",
            json!({"schema":{"type":"object","maxProperties":100,"additionalProperties":{"type":"string","maxLength":2000}}}),
            vec!["schema"],
        ),
        (
            "obscura_get_attribute",
            "An attribute of an element (href, src, value, class, data-*).",
            json!({"attribute":{"type":"string","minLength":1,"maxLength":200},"ref":reference(),"selector":selector()}),
            vec!["attribute"],
        ),
        (
            "obscura_count",
            "How many elements match a CSS selector: a cheap existence or pagination probe.",
            json!({"selector":selector()}),
            vec!["selector"],
        ),
        (
            "obscura_search",
            "Substring matches in the visible page text, with context.",
            json!({"query":{"type":"string","minLength":1,"maxLength":1000},"case_sensitive":{"type":"boolean"},"limit":limit,"context_chars":{"type":"integer","minimum":0,"maximum":1000}}),
            vec!["query"],
        ),
        (
            "obscura_click",
            "Click an element by ref (preferred) or CSS selector.",
            json!({"ref":reference(),"selector":selector()}),
            vec![],
        ),
        (
            "obscura_fill",
            "Set the value of an input, replacing what it holds. Pass a ref (preferred) or a selector.",
            json!({"ref":reference(),"selector":selector(),"value":{"type":"string","maxLength":100000}}),
            vec!["value"],
        ),
        (
            "obscura_type",
            "Type text into an input, appending to what it holds.",
            json!({"ref":reference(),"selector":selector(),"text":{"type":"string","maxLength":100000}}),
            vec!["text"],
        ),
        (
            "obscura_fill_form",
            "Fill several inputs in one call; each field is {ref?, selector?, value, type?} with type text, check, uncheck or select. Optionally click a submit element afterwards.",
            json!({"fields":{"type":"array","minItems":1,"maxItems":100,"items":{"type":"object","additionalProperties":false,"required":["value"],"properties":{
                    "ref":reference(),"selector":selector(),"value":{"type":"string","maxLength":100000},"type":{"type":"string","enum":["text","check","uncheck","select"]}}}},
                "submit_ref":reference(),"submit_selector":selector()}),
            vec!["fields"],
        ),
        (
            "obscura_press_key",
            "Dispatch a key (Enter, Tab, Escape…) on an element or the document.",
            json!({"key":{"type":"string","minLength":1,"maxLength":50},"selector":selector()}),
            vec!["key"],
        ),
        (
            "obscura_select_option",
            "Choose an option of a <select> by value or visible text.",
            json!({"selector":selector(),"value":{"type":"string","maxLength":10000}}),
            vec!["selector", "value"],
        ),
        (
            "obscura_scroll",
            "Scroll the page or an element (top, bottom, up, down, left, right); `bottom` triggers infinite-scroll loaders.",
            json!({"direction":{"type":"string","enum":["top","bottom","up","down","left","right"]},"amount":{"type":"integer","minimum":1,"maximum":100000},"ref":reference(),"selector":selector()}),
            vec![],
        ),
        (
            "obscura_wait_for",
            "Wait until a CSS selector appears in the DOM (30 s by default).",
            json!({"selector":selector(),"timeout":seconds}),
            vec!["selector"],
        ),
        (
            "obscura_wait_for_text",
            "Wait until a substring appears in the rendered page text: result messages, notifications.",
            json!({"text":{"type":"string","minLength":1,"maxLength":1000},"timeout":seconds}),
            vec!["text"],
        ),
        (
            "obscura_network_requests",
            "The network requests the page made (URLs, statuses, timings).",
            json!({}),
            vec![],
        ),
        (
            "obscura_console_messages",
            "The console messages the page logged.",
            json!({}),
            vec![],
        ),
        (
            "obscura_screenshot",
            "A PNG of the rendered viewport, when the installed Obscura supports it. With path it is saved inside the sandbox root; otherwise it is returned inline if small enough. Prefer obscura_snapshot for text.",
            json!({"width":{"type":"integer","minimum":1,"maximum":10000},"height":{"type":"integer","minimum":1,"maximum":10000},"path":path.clone(),"create_dirs":create_dirs.clone()}),
            vec![],
        ),
        (
            "obscura_pdf",
            "Export the rendered page as a one-page PDF that is only an image of the page (Obscura embeds a screenshot): it has no text layer, is not paginated and cannot be searched or copied. For a searchable, styled PDF read the page with obscura_markdown and write it with office_write_pdf. With path it is saved inside the sandbox root; otherwise it is returned inline if small enough.",
            json!({"landscape":{"type":"boolean"},"print_background":{"type":"boolean"},"scale":{"type":"number","minimum":0.1,"maximum":2},
                "paper_width":inches.clone(),"paper_height":inches.clone(),"margin_top":inches.clone(),"margin_bottom":inches.clone(),"margin_left":inches.clone(),"margin_right":inches,
                "path":path,"create_dirs":create_dirs}),
            vec![],
        ),
        (
            "obscura_tab_new",
            "Open a new isolated tab; later calls act on the most recent tab.",
            json!({"url":url}),
            vec![],
        ),
        (
            "obscura_tab_list",
            "Every open tab with its id, URL, title and which one is active.",
            json!({}),
            vec![],
        ),
        (
            "obscura_tab_switch",
            "Make a tab the active one.",
            json!({"tab_id":{"type":"string","minLength":1,"maxLength":200}}),
            vec!["tab_id"],
        ),
        (
            "obscura_tab_close",
            "Close a tab by id (the active one by default).",
            json!({"tab_id":{"type":"string","minLength":1,"maxLength":200}}),
            vec![],
        ),
    ];
    if policy.allow_evaluate {
        tools.push((
            "obscura_evaluate",
            "Evaluate a JavaScript expression in the page and return the result as JSON.",
            json!({"expression":{"type":"string","minLength":1,"maxLength":100000}}),
            vec!["expression"],
        ));
    }
    if policy.allow_credentials {
        tools.extend([
            ("obscura_get_cookies", "The cookies in the browser's jar, one JSON object per line.", json!({"domain":{"type":"string","maxLength":255}}), vec![]),
            ("obscura_set_cookie", "Add or replace a cookie, to reuse a session token.",
             json!({"name":{"type":"string","minLength":1,"maxLength":255},"value":{"type":"string","maxLength":8192},"domain":{"type":"string","minLength":1,"maxLength":255},
                    "path":{"type":"string","maxLength":2048},"secure":{"type":"boolean"},"http_only":{"type":"boolean"}}), vec!["name", "value", "domain"]),
            ("obscura_clear_cookies", "Remove every cookie from the jar.", json!({}), vec![]),
            ("obscura_storage_state", "The whole session state (cookies, localStorage, sessionStorage) as JSON.", json!({}), vec![]),
            ("obscura_set_storage_state", "Restore a session state returned by obscura_storage_state.", json!({"state":{"type":"object"}}), vec!["state"]),
        ]);
    }
    for tool in &mut tools {
        // Every tool accepts a client-side timeout that is never sent to the server.
        if let Some(properties) = tool.2.as_object_mut() {
            properties.insert(
                "timeout_ms".into(),
                json!({"type":"integer","minimum":1,"maximum":180_000}),
            );
        }
    }
    tools
}

/// Arguments that stay on the SDK side and are never forwarded to Obscura.
const LOCAL_ARGUMENTS: [&str; 3] = ["timeout_ms", "path", "create_dirs"];

/// Stop the Obscura process, if one is running.
pub(super) async fn shutdown(inner: &Inner) -> bool {
    match inner.obscura_state.lock().await.take() {
        Some(mut session) => {
            let _ = session.stdin.shutdown().await;
            let _ = session.child.kill().await;
            true
        }
        None => false,
    }
}

pub(super) async fn run(
    inner: &Inner,
    name: &str,
    arguments: &Value,
    cancel: &CancellationToken,
) -> Result<Value> {
    let policy = inner
        .obscura
        .as_ref()
        .ok_or_else(|| invalid("obscura tools are disabled"))?;
    if matches!(name, "obscura_navigate" | "obscura_tab_new")
        && let Some(url) = arguments["url"].as_str()
    {
        check_url(policy, url).await?;
    }
    let mut slot = inner.obscura_state.lock().await;
    if slot.is_none() {
        let started = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            started = Session::start(policy) => started?,
        };
        *slot = Some(started);
    }
    let timeout = Duration::from_millis(
        arguments["timeout_ms"]
            .as_u64()
            .unwrap_or(policy.call_timeout_ms),
    );
    let session = slot.as_mut().expect("just started");
    let outcome = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Error::Cancelled),
        outcome = tokio::time::timeout(timeout, operate(inner, policy, session, name, arguments)) => outcome.unwrap_or(Err(Error::Timeout)),
    };
    // A cancelled, timed-out or disconnected call leaves the page in an unknown
    // state: that session is dropped and the next call starts afresh.
    let lost = matches!(outcome, Err(Error::Cancelled | Error::Timeout))
        || matches!(&outcome, Err(Error::State(message)) if message.starts_with("OBSCURA_ERROR: the obscura process") || message.starts_with("OBSCURA_ERROR: cannot read") || message.starts_with("OBSCURA_ERROR: obscura sent"));
    if lost && let Some(session) = slot.take() {
        session.kill_now();
    }
    outcome
}

async fn check_url(policy: &ObscuraPolicy, raw: &str) -> Result<()> {
    let url = reqwest::Url::parse(raw).map_err(|_| invalid("invalid URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(invalid("URL must be http(s) without embedded credentials"));
    }
    // Obscura enforces the private-network rule itself; only the allowlist is applied here.
    check_host(&policy.allowed_hosts, true, &url)
        .await
        .map(|_| ())
}

async fn call(session: &mut Session, tool: &str, arguments: Map) -> Result<Value> {
    session
        .request("tools/call", json!({"name":tool,"arguments":arguments}))
        .await
}
type Map = serde_json::Map<String, Value>;

struct Output {
    text: String,
    blobs: Vec<(String, Vec<u8>)>,
}

fn unpack(result: &Value) -> Result<Output> {
    let mut output = Output {
        text: String::new(),
        blobs: vec![],
    };
    let mut texts = Vec::new();
    for block in result["content"].as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => texts.push(block["text"].as_str().unwrap_or_default().to_owned()),
            Some("image") => {
                let data = block["data"]
                    .as_str()
                    .and_then(media::decode_base64)
                    .ok_or_else(|| state("an image block is not valid base64"))?;
                output.blobs.push((
                    block["mimeType"].as_str().unwrap_or("image/png").to_owned(),
                    data,
                ));
            }
            Some("resource") => {
                if let Some(blob) = block["resource"]["blob"].as_str() {
                    let data = media::decode_base64(blob)
                        .ok_or_else(|| state("a resource block is not valid base64"))?;
                    output.blobs.push((
                        block["resource"]["mimeType"]
                            .as_str()
                            .unwrap_or("application/octet-stream")
                            .to_owned(),
                        data,
                    ));
                } else if let Some(text) = block["resource"]["text"].as_str() {
                    texts.push(text.to_owned());
                }
            }
            _ => {}
        }
    }
    output.text = texts.join("\n").trim().to_owned();
    if result["isError"] == true {
        return Err(state(if output.text.is_empty() {
            "the tool failed"
        } else {
            &output.text
        }));
    }
    Ok(output)
}

/// The first http(s) URL in a tool's reply, which is how its final address is read.
fn first_url(text: &str) -> Option<&str> {
    let start = text.find("https://").or_else(|| text.find("http://"))?;
    let rest = &text[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ',' | ')'))
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// With an allowlist, where the page ended up is checked after every action that can
/// move it. An unreadable address fails closed.
async fn guard(session: &mut Session, policy: &ObscuraPolicy) -> Result<()> {
    if policy.allowed_hosts.is_empty() {
        return Ok(());
    }
    let reply = call(
        session,
        "browser_evaluate",
        Map::from_iter([("expression".to_owned(), json!("location.href"))]),
    )
    .await?;
    let text = unpack(&reply)?.text;
    let verdict = match first_url(&text) {
        Some(url) => check_url(policy, url).await,
        None if text.contains("about:blank") => Ok(()),
        None => Err(invalid("the page address could not be read")),
    };
    if let Err(error) = verdict {
        let _ = call(session, "browser_close", Map::new()).await;
        return Err(match error {
            Error::Input(reason) => {
                Error::Input(format!("the page ended on a blocked address: {reason}"))
            }
            other => other,
        });
    }
    Ok(())
}

async fn operate(
    inner: &Inner,
    policy: &ObscuraPolicy,
    session: &mut Session,
    name: &str,
    arguments: &Value,
) -> Result<Value> {
    let server_tool = name.replacen("obscura_", "browser_", 1);
    let forwarded: Map = arguments
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(key, value)| !LOCAL_ARGUMENTS.contains(&key.as_str()) && !value.is_null())
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let reply = call(session, &server_tool, forwarded).await?;
    let output = unpack(&reply)?;
    if matches!(
        name,
        "obscura_navigate"
            | "obscura_back"
            | "obscura_forward"
            | "obscura_reload"
            | "obscura_click"
            | "obscura_press_key"
            | "obscura_fill_form"
            | "obscura_tab_new"
            | "obscura_tab_switch"
    ) {
        guard(session, policy).await?;
    }
    if let Some((mime, bytes)) = output.blobs.first() {
        let mut result = json!({"mime_type":mime,"bytes":bytes.len()});
        if let Some(size) = crate::image::info(bytes) {
            result["width"] = json!(size.width);
            result["height"] = json!(size.height);
        }
        if !output.text.is_empty() {
            result["text"] = json!(output.text);
        }
        if let Some(path) = arguments["path"].as_str() {
            result["path"] = json!(fs::write_bytes(
                inner,
                path,
                bytes,
                arguments["create_dirs"].as_bool() == Some(true)
            )?);
        } else {
            let encoded = media::encode_base64(bytes);
            if encoded.len() > inner.max_output {
                return Err(invalid(format!(
                    "the result is {} bytes; pass path to save it inside the sandbox",
                    bytes.len()
                )));
            }
            result["data"] = json!(encoded);
        }
        return Ok(result);
    }
    let mut end = output.text.len().min(inner.max_output);
    while !output.text.is_char_boundary(end) {
        end -= 1;
    }
    Ok(json!({"text":&output.text[..end],"truncated":end < output.text.len()}))
}

/// Install or update Obscura on request. Needs the obscura tools to be configured.
pub(super) async fn install(inner: &Inner, update: bool) -> Result<Value> {
    let policy = inner
        .obscura
        .as_ref()
        .ok_or_else(|| invalid("obscura tools are disabled"))?;
    Ok(policy.install_latest(update).await?.report())
}
