//! Native filesystem, command and web tools.
//!
//! Like the catalog tools, these are plain [`Tool`] definitions plus an executor.
//! The Agent loop stays the authority for claims, budgets and uncertainty; this
//! module only enforces its own sandbox: a canonical filesystem root, an explicit
//! program allowlist (no shell), public-host-only web access unless opted out,
//! and byte caps on everything returned to the model.
mod command;
#[cfg(feature = "computer-use")]
mod computer;
mod fs;
mod obscura;
mod obscura_install;
mod office;
mod web;

pub use command::CommandPolicy;
#[cfg(feature = "computer-use")]
pub use computer::{ComputerBackend, ComputerPolicy, ScriptedDesktop};
pub use obscura::ObscuraPolicy;
pub use office::{OfficePolicy, PdfPageImage, PdfPageImages, pdf_page_images};
pub use web::{SearchProvider, WebPolicy};

use crate::{Error, Result, Tool, provider::CancellationToken};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

const MAX_LIMIT: usize = 1024 * 1024 * 16;

fn default_read() -> usize {
    256 * 1024
}
fn default_write() -> usize {
    1024 * 1024
}
fn default_output() -> usize {
    128 * 1024
}

/// Sandbox configuration. Command and web tools are disabled unless configured.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolsConfig {
    /// Directory every filesystem and command working path must stay inside.
    pub root: PathBuf,
    #[serde(default = "default_read")]
    pub max_read_bytes: usize,
    #[serde(default = "default_write")]
    pub max_write_bytes: usize,
    /// Cap for command output and fetched web bodies, per stream/response.
    #[serde(default = "default_output")]
    pub max_output_bytes: usize,
    #[serde(default)]
    pub command: Option<CommandPolicy>,
    #[serde(default)]
    pub web: Option<WebPolicy>,
    /// Enables the `office_*` tools (Word, Excel, PowerPoint inside the root).
    #[serde(default)]
    pub office: Option<OfficePolicy>,
    /// Enables the `obscura_*` browser tools, backed by the Obscura headless browser.
    #[serde(default)]
    pub obscura: Option<ObscuraPolicy>,
    /// Enables the `computer_*` tools (screenshots, pointer, keyboard). Off unless given, and
    /// input is a separate grant inside it; see docs/computer-use.md.
    #[cfg(feature = "computer-use")]
    #[serde(default)]
    pub computer: Option<ComputerPolicy>,
}

impl ToolsConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_read_bytes: default_read(),
            max_write_bytes: default_write(),
            max_output_bytes: default_output(),
            command: None,
            web: None,
            office: None,
            obscura: None,
            #[cfg(feature = "computer-use")]
            computer: None,
        }
    }
}

pub(crate) struct Inner {
    pub(crate) root: PathBuf,
    pub(crate) max_read: usize,
    pub(crate) max_write: usize,
    pub(crate) max_output: usize,
    pub(crate) command: Option<CommandPolicy>,
    pub(crate) web: Option<WebPolicy>,
    pub(crate) office: Option<OfficePolicy>,
    pub(crate) obscura: Option<ObscuraPolicy>,
    #[cfg(feature = "computer-use")]
    pub(crate) computer: Option<ComputerPolicy>,
    #[cfg(feature = "computer-use")]
    pub(crate) computer_state: computer::State,
    /// The running Obscura session, started lazily and stopped by `close` or on drop.
    pub(crate) obscura_state: tokio::sync::Mutex<Option<obscura::Session>>,
}

#[derive(Clone)]
pub struct NativeTools {
    inner: Arc<Inner>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Dispatch {
    ToolDefinitions,
    ExecuteTool {
        name: String,
        arguments: Value,
    },
    InstallObscura {
        #[serde(default)]
        update: bool,
    },
    /// Host controls of the `computer_*` tools. `stop` takes effect for every later action and
    /// lets go of anything held down; `resume` and `reset` (the action budget) are the host's too.
    ComputerStop,
    ComputerResume,
    ComputerReset,
    ComputerStatus,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Input(message.into())
}

impl NativeTools {
    pub fn new(config: ToolsConfig) -> Result<Self> {
        for (name, value) in [
            ("max_read_bytes", config.max_read_bytes),
            ("max_write_bytes", config.max_write_bytes),
            ("max_output_bytes", config.max_output_bytes),
        ] {
            if value == 0 || value > MAX_LIMIT {
                return Err(invalid(format!("{name} must be between 1 and {MAX_LIMIT}")));
            }
        }
        let root = std::fs::canonicalize(&config.root)
            .map_err(|_| invalid("tools root must be an existing directory"))?;
        if !root.is_dir() {
            return Err(invalid("tools root must be an existing directory"));
        }
        if let Some(command) = &config.command {
            command.validate()?;
        }
        if let Some(web) = &config.web {
            web.validate()?;
        }
        if let Some(office) = &config.office {
            office.validate()?;
        }
        if let Some(obscura) = &config.obscura {
            obscura.validate()?;
        }
        #[cfg(feature = "computer-use")]
        if let Some(computer) = &config.computer {
            computer.validate()?;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                root,
                max_read: config.max_read_bytes,
                max_write: config.max_write_bytes,
                max_output: config.max_output_bytes,
                command: config.command,
                web: config.web,
                office: config.office,
                obscura: config.obscura,
                #[cfg(feature = "computer-use")]
                computer: config.computer,
                #[cfg(feature = "computer-use")]
                computer_state: Default::default(),
                obscura_state: Default::default(),
            }),
        })
    }

    pub fn from_json(config: Value) -> Result<Self> {
        Self::new(serde_json::from_value(config)?)
    }

    /// Only tools that this configuration can actually execute are advertised.
    pub fn tool_definitions(&self) -> Vec<Tool> {
        let path = json!({"type":"string","maxLength":4096});
        let limit = |max: u64| json!({"type":"integer","minimum":1,"maximum":max});
        let mut tools: Vec<(&str, &str, Value, Vec<&str>)> = vec![
            (
                "fs_read",
                "Read a UTF-8 text file inside the sandbox root, by line window.",
                json!({"path":path,"offset":{"type":"integer","minimum":0,"maximum":9007199254740990u64},"limit":limit(10000)}),
                vec!["path"],
            ),
            (
                "fs_write",
                "Create or atomically replace a UTF-8 text file inside the sandbox root.",
                json!({"path":path,"content":{"type":"string"},"create_dirs":{"type":"boolean"}}),
                vec!["path", "content"],
            ),
            (
                "fs_edit",
                "Replace exact text in a file; fails unless the match is unique or replace_all is true.",
                json!({"path":path,"old":{"type":"string","minLength":1},"new":{"type":"string"},"replace_all":{"type":"boolean"}}),
                vec!["path", "old", "new"],
            ),
            (
                "fs_list",
                "List a directory inside the sandbox root.",
                json!({"path":path,"limit":limit(1000)}),
                vec![],
            ),
            (
                "fs_glob",
                "Find files by glob pattern (*, ?, **) relative to the sandbox root; results list each directory's files before its subdirectories.",
                json!({"pattern":{"type":"string","minLength":1,"maxLength":512},"limit":limit(1000)}),
                vec!["pattern"],
            ),
            (
                "fs_grep",
                "Search file contents for a literal string.",
                json!({"pattern":{"type":"string","minLength":1,"maxLength":512},"path":path,
                    "glob":{"type":"string","maxLength":512},"case_insensitive":{"type":"boolean"},"limit":limit(500)}),
                vec!["pattern"],
            ),
        ];
        if self.inner.command.is_some() {
            tools.push((
                "command_run",
                "Run an allowlisted program directly (no shell) with a timeout and capped output.",
                json!({"program":{"type":"string","minLength":1,"maxLength":256},
                       "args":{"type":"array","items":{"type":"string","maxLength":8192},"maxItems":128},
                       "cwd":path,"stdin":{"type":"string","maxLength":1048576},
                       "timeout_ms":{"type":"integer","minimum":1}}),
                vec!["program"],
            ));
        }
        if let Some(web) = &self.inner.web {
            tools.push((
                "web_fetch",
                "HTTP GET a public http(s) URL; redirects are reported, not followed.",
                json!({"url":{"type":"string","minLength":1,"maxLength":4096}}),
                vec!["url"],
            ));
            if web.search.is_some() {
                tools.push((
                    "web_search",
                    "Search the web through the configured provider.",
                    json!({"query":{"type":"string","minLength":1,"maxLength":512},"limit":limit(20)}),
                    vec!["query"],
                ));
            }
        }
        if self.inner.office.is_some() {
            tools.extend(office::specs());
        }
        if let Some(obscura) = &self.inner.obscura {
            tools.extend(obscura::specs(obscura));
        }
        #[cfg(feature = "computer-use")]
        if let Some(computer) = &self.inner.computer {
            tools.extend(computer::specs(computer));
        }
        tools
            .into_iter()
            .map(|(name, description, properties, required)| Tool {
                name: name.into(),
                description: description.into(),
                parameters: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
                // A click always answers {"ok":true}, and an unchanged screen looks the same twice.
                allow_repeated_results: name.starts_with("computer_"),
            })
            .collect()
    }

    pub async fn execute_tool(
        &self,
        name: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let tool = self
            .tool_definitions()
            .into_iter()
            .find(|tool| tool.name == name)
            .ok_or_else(|| invalid("unknown or disabled native tool"))?;
        if !crate::validation::compile(&tool.parameters)?.is_valid(&arguments) {
            return Err(invalid("native tool arguments do not match schema"));
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let inner = self.inner.clone();
        match name {
            "command_run" => command::run(&inner, &arguments, cancel).await,
            #[cfg(feature = "computer-use")]
            name if name.starts_with("computer_") => {
                computer::run(&inner, name, &arguments, cancel).await
            }
            name if name.starts_with("obscura_") => {
                obscura::run(&inner, name, &arguments, cancel).await
            }
            name if name.starts_with("office_") => {
                let name = name.to_owned();
                tokio::task::spawn_blocking(move || office::run(&inner, &name, &arguments))
                    .await
                    .map_err(|_| Error::State("office tool task failed".into()))?
            }
            "web_fetch" | "web_search" => {
                let policy = inner.web.as_ref().expect("enabled web tool");
                let operation = async {
                    if name == "web_fetch" {
                        web::fetch(&inner, &arguments, cancel).await
                    } else {
                        web::search(&inner, &arguments, cancel).await
                    }
                };
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(Error::Cancelled),
                    result = tokio::time::timeout(std::time::Duration::from_millis(policy.timeout_ms), operation) => result.map_err(|_| Error::Timeout)?,
                }
            }
            _ => {
                let name = name.to_owned();
                tokio::task::spawn_blocking(move || fs::run(&inner, &name, &arguments))
                    .await
                    .map_err(|_| Error::State("filesystem tool task failed".into()))?
            }
        }
    }

    /// Stop the Obscura session the `obscura_*` tools started, if any. Safe to call
    /// at any time; the next browser call starts a new one.
    /// Download and install the latest Obscura (into `obscura.install_dir`, `~/.hive/bin`
    /// by default) after checking its published SHA-256. An existing install is kept
    /// unless `update` is true, and even then only replaced by a different release.
    /// Returns `{path, version, asset, sha256, already_current}`.
    pub async fn install_obscura(&self, update: bool) -> Result<Value> {
        obscura::install(&self.inner, update).await
    }

    pub async fn close(&self) {
        obscura::shutdown(&self.inner).await;
        #[cfg(feature = "computer-use")]
        self.inner.computer_state.shutdown().await;
    }

    /// Replace the desktop with a scripted one (for the tests of a host that embeds these tools).
    #[doc(hidden)]
    #[cfg(feature = "computer-use")]
    pub fn attach_scripted_desktop(&self, width: u32, height: u32) -> ScriptedDesktop {
        computer::attach_scripted(&self.inner.computer_state, (width, height))
    }

    /// Refuse every later `computer_*` action until [`computer_resume`](Self::computer_resume).
    /// Safe from any thread at any time; an action already running is interrupted at its next
    /// step by the caller's cancellation, and its held keys are released when it ends.
    #[cfg(feature = "computer-use")]
    pub fn computer_stop(&self) {
        self.inner.computer_state.stop();
    }
    #[cfg(feature = "computer-use")]
    pub fn computer_resume(&self) {
        self.inner.computer_state.resume();
    }
    /// Give the `computer_*` tools a fresh action budget.
    #[cfg(feature = "computer-use")]
    pub fn computer_reset(&self) {
        self.inner.computer_state.reset();
    }
    /// `{stopped, actions}`.
    #[cfg(feature = "computer-use")]
    pub fn computer_status(&self) -> Value {
        self.inner.computer_state.status()
    }

    /// Best-effort synchronous stop of the session, for process-exit hooks. If a
    /// call is in flight (the session is locked) it is left to `Drop`.
    pub fn close_now(&self) {
        if let Ok(mut slot) = self.inner.obscura_state.try_lock()
            && let Some(session) = slot.take()
        {
            session.kill_now();
        }
    }

    pub async fn dispatch(&self, command: Value, cancel: &CancellationToken) -> Result<Value> {
        Ok(match serde_json::from_value::<Dispatch>(command)? {
            Dispatch::ToolDefinitions => serde_json::to_value(self.tool_definitions())?,
            Dispatch::ExecuteTool { name, arguments } => {
                self.execute_tool(&name, arguments, cancel).await?
            }
            #[cfg(feature = "computer-use")]
            Dispatch::ComputerStop => {
                self.computer_stop();
                self.computer_status()
            }
            #[cfg(feature = "computer-use")]
            Dispatch::ComputerResume => {
                self.computer_resume();
                self.computer_status()
            }
            #[cfg(feature = "computer-use")]
            Dispatch::ComputerReset => {
                self.computer_reset();
                self.computer_status()
            }
            #[cfg(feature = "computer-use")]
            Dispatch::ComputerStatus => self.computer_status(),
            #[cfg(not(feature = "computer-use"))]
            Dispatch::ComputerStop
            | Dispatch::ComputerResume
            | Dispatch::ComputerReset
            | Dispatch::ComputerStatus => {
                return Err(invalid("computer use is not built into this SDK"));
            }
            Dispatch::InstallObscura { update } => {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(Error::Cancelled),
                    report = self.install_obscura(update) => report?,
                }
            }
        })
    }
}
