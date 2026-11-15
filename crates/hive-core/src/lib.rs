//! Shared agent state machine. Bindings only drive actions and execute host callbacks.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;

#[cfg(feature = "browser")]
pub mod browser;
mod budget;
mod context;
pub mod durable_handle;
pub mod handles;
#[cfg(feature = "harness")]
pub mod harness;
pub mod mcp;
pub mod memory;
pub mod objective;
pub mod provider;
#[cfg(feature = "scheduler")]
pub mod scheduler;
#[cfg(feature = "services")]
pub mod services;
pub mod sessions;
mod structured;
mod tokens;
mod validation;
pub use structured::StructuredOutput;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("CLOSED: session store is closed")]
    StoreClosed,
    #[error("SUMMARY_OPERATION_UNCERTAIN: inspect and reconcile the summary before repeating it")]
    SummaryUncertain,
    #[error(
        "TRIGGER_OPERATION_UNCERTAIN: inspect and reconcile the trigger before dispatching another occurrence"
    )]
    TriggerUncertain,
    #[error(
        "VERIFICATION_OPERATION_UNCERTAIN: inspect and reconcile the verification before repeating it"
    )]
    VerificationUncertain,
    #[error("OBJECTIVE_LIMIT: {0}")]
    ObjectiveLimit(String),
    #[error("MCP_ERROR: {0}")]
    Mcp(String),
    #[error("MCP_REMOTE_ERROR: {0}")]
    McpRemote(String),
    #[error(
        "MCP_OPERATION_UNCERTAIN: tool response was not confirmed; inspect and reconcile before resuming"
    )]
    McpOperationUncertain,
    #[error("INVALID_INPUT: {0}")]
    Input(String),
    #[error("INVALID_STATE: {0}")]
    State(String),
    #[error("PROVIDER_ERROR: {0}")]
    Provider(String),
    #[error("INVALID_JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("STORAGE_ERROR: {0}")]
    Storage(String),
    #[error("SESSION_CONFLICT: session changed since it was loaded")]
    SessionConflict,
    #[error("SESSION_INTERRUPTED: an unfinished run requires explicit recovery")]
    SessionInterrupted,
    #[error("SESSION_CONFIG_MISMATCH: reopen with the original model, tools, system and policy")]
    SessionConfigMismatch,
    #[error(
        "SESSION_OPERATION_UNCERTAIN: inspect and resolve the in-flight operation before resuming"
    )]
    SessionOperationUncertain,
    #[error("INVALID_TOOL_SCHEMA: {0}")]
    InvalidSchema(String),
    #[error("CANCELLED: model request was cancelled")]
    Cancelled,
    #[error("TIMEOUT: model request exceeded its deadline")]
    Timeout,
    #[error("DEADLINE_EXCEEDED: run deadline elapsed")]
    DeadlineExceeded,
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    /// Opt out for deliberate polling or tools whose repeated output is meaningful.
    #[serde(default)]
    pub allow_repeated_results: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProviderState {
    Gemini { version: u32, parts: Vec<Value> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<ProviderState>,
}
impl Message {
    fn text(role: &str, text: String) -> Self {
        Self {
            role: role.into(),
            content: Some(text),
            tool_calls: vec![],
            tool_call_id: None,
            provider_state: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<Tool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<StructuredOutput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContextReport {
    pub serialized_bytes: u64,
    pub dropped_messages: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_estimate: Option<TokenEstimate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retrieved_sources: Vec<memory::ContextSource>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TokenEncoding {
    Cl100kBase,
    O200kBase,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum InputTokenFraming {
    #[default]
    NeutralJson,
    OpenaiChatJson,
    AnthropicMessagesJson,
    GeminiGenerateContentJson,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputTokenBudget {
    pub encoding: TokenEncoding,
    pub max_tokens: u64,
    #[serde(default)]
    pub overhead_tokens: u64,
    #[serde(default)]
    pub framing: InputTokenFraming,
    #[serde(default)]
    pub max_total_tokens: Option<u64>,
    #[serde(default)]
    pub context_window_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TokenEstimate {
    pub encoding: TokenEncoding,
    #[serde(default)]
    pub framing: InputTokenFraming,
    pub serialized_tokens: u64,
    pub overhead_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ContextOverflow {
    #[default]
    Stop,
    DropOldestTurns,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelReply {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_state: Option<ProviderState>,
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunStats {
    pub model_calls: u32,
    pub tool_calls: u32,
    #[serde(default)]
    pub tool_rejections: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub unmetered_model_calls: u32,
    #[serde(default)]
    pub reserved_output_tokens: u64,
    #[serde(default)]
    pub reserved_input_tokens: u64,
    #[serde(default)]
    pub estimated_input_tokens: u64,
    #[serde(default)]
    pub charged_input_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoopPolicy {
    pub max_tool_calls: u32,
    pub max_identical_tool_results: Option<u32>,
    pub max_total_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub max_total_output_tokens: Option<u64>,
    pub max_run_ms: Option<u64>,
    pub max_context_bytes: Option<u64>,
    pub context_overflow: ContextOverflow,
    pub input_token_budget: Option<InputTokenBudget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<StructuredOutput>,
}
impl Default for LoopPolicy {
    fn default() -> Self {
        Self {
            max_tool_calls: 16,
            max_identical_tool_results: Some(2),
            max_total_tokens: None,
            max_output_tokens: None,
            max_total_output_tokens: None,
            max_run_ms: None,
            max_context_bytes: None,
            context_overflow: ContextOverflow::Stop,
            input_token_budget: None,
            structured_output: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    ModelCallLimit,
    ToolCallLimit,
    RepeatedToolResult,
    TokenLimit,
    UsageUnavailable,
    OutputLimit,
    DeadlineExceeded,
    ContextLimit,
    InputLimit,
    InvalidStructuredOutput,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    Idle,
    Model { request: ModelRequest },
    Tool { call: ToolCall },
    ToolRejected { call: ToolCall, result: Value },
    Complete { content: String, stats: RunStats },
    Stopped { reason: StopReason, stats: RunStats },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Engine {
    version: u32,
    model: String,
    tools: Vec<Tool>,
    messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_plan: Option<memory::ContextPlan>,
    max_turns: u32,
    turns: u32,
    pending: Vec<ToolCall>,
    action: Action,
    policy: LoopPolicy,
    stats: RunStats,
    observations: Vec<Value>,
    #[serde(default)]
    deadline_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    objective_id: Option<String>,
}

impl Engine {
    pub fn new(
        model: String,
        system: Option<String>,
        tools: Vec<Tool>,
        max_turns: u32,
    ) -> Result<Self> {
        Self::with_policy(model, system, tools, max_turns, LoopPolicy::default())
    }

    pub fn with_policy(
        model: String,
        system: Option<String>,
        tools: Vec<Tool>,
        max_turns: u32,
        policy: LoopPolicy,
    ) -> Result<Self> {
        if model.trim().is_empty() || max_turns == 0 {
            return Err(Error::Input(
                "model and positive max_turns are required".into(),
            ));
        }
        if policy.max_tool_calls == 0
            || policy.max_identical_tool_results == Some(0)
            || policy.max_total_tokens == Some(0)
            || policy.max_output_tokens == Some(0)
            || policy.max_total_output_tokens == Some(0)
            || policy.max_run_ms == Some(0)
            || policy.max_context_bytes == Some(0)
        {
            return Err(Error::Input(
                "loop limits must be positive or null where supported".into(),
            ));
        }
        if policy.max_context_bytes.is_none()
            && policy.input_token_budget.is_none()
            && policy.context_overflow != ContextOverflow::Stop
        {
            return Err(Error::Input(
                "context_overflow requires a byte or input token budget".into(),
            ));
        }
        if let Some(budget) = &policy.input_token_budget
            && (budget.max_tokens == 0
                || budget.overhead_tokens >= budget.max_tokens
                || budget.max_total_tokens == Some(0)
                || budget.context_window_tokens == Some(0)
                || (budget.context_window_tokens.is_some()
                    && policy.max_output_tokens.is_none()
                    && policy.max_total_output_tokens.is_none()))
        {
            return Err(Error::Input(
                "input limits must be positive, max_tokens must exceed overhead_tokens, and a context window requires an explicit output cap".into(),
            ));
        }
        let mut names = HashSet::new();
        if let Some(output) = &policy.structured_output {
            output.validate()?;
        }
        for tool in &tools {
            if tool.name.trim().is_empty()
                || !names.insert(&tool.name)
                || tool.parameters.get("type").and_then(Value::as_str) != Some("object")
            {
                return Err(Error::Input(
                    "tools require unique names and object JSON Schemas".into(),
                ));
            }
            validation::compile(&tool.parameters)?;
        }
        Ok(Self {
            version: 1,
            model,
            tools,
            messages: system
                .into_iter()
                .map(|s| Message::text("system", s))
                .collect(),
            max_turns,
            turns: 0,
            pending: vec![],
            context_plan: None,
            action: Action::Idle,
            policy,
            stats: RunStats::default(),
            observations: vec![],
            deadline_unix_ms: None,
            objective_id: None,
        })
    }

    pub fn action(&self) -> &Action {
        &self.action
    }
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }
    pub(crate) fn seed_history(&mut self, history: Vec<Message>) -> Result<()> {
        if !matches!(self.action, Action::Idle)
            || history.len() > 200
            || history.iter().any(|message| {
                !matches!(message.role.as_str(), "user" | "assistant")
                    || message.content.is_none()
                    || !message.tool_calls.is_empty()
                    || message.tool_call_id.is_some()
                    || message.provider_state.is_some()
            })
        {
            return Err(Error::Input("initial_history only accepts up to 200 plain user/assistant messages on an idle engine".into()));
        }
        self.messages.extend(history);
        Ok(())
    }

    pub fn bind_objective(&mut self, id: String) -> Result<()> {
        if !matches!(self.action, Action::Idle) || id.trim().is_empty() {
            return Err(Error::Input(
                "bind a nonempty objective id before starting the engine".into(),
            ));
        }
        self.objective_id = Some(id);
        Ok(())
    }

    pub(crate) fn apply_objective_permit(&mut self, permit: &objective::ModelPermit) -> Result<()> {
        let Action::Model { request } = &mut self.action else {
            return Err(Error::State(
                "objective permit requires a pending model".into(),
            ));
        };
        request.max_output_tokens = Some(permit.max_output_tokens);
        if self.policy.max_total_output_tokens.is_some() {
            self.stats.reserved_output_tokens = permit.max_output_tokens;
        }
        if let Some(deadline) = permit.deadline_unix_ms {
            let deadline = self
                .deadline_unix_ms
                .map_or(deadline, |own| own.min(deadline));
            self.deadline_unix_ms = Some(deadline);
            request.deadline_unix_ms = Some(deadline);
        }
        Ok(())
    }

    pub fn start(&mut self, prompt: String) -> Result<()> {
        if !matches!(self.action, Action::Idle | Action::Complete { .. }) {
            return Err(Error::State(
                "finish the active run before starting another".into(),
            ));
        }
        if prompt.trim().is_empty() {
            return Err(Error::Input("prompt is empty".into()));
        }
        let deadline = self
            .policy
            .max_run_ms
            .map(|limit| {
                budget::now_ms()?
                    .checked_add(limit)
                    .filter(|value| *value <= 9_007_199_254_740_991)
                    .ok_or_else(|| {
                        Error::Input("run deadline exceeds the portable timestamp range".into())
                    })
            })
            .transpose()?;
        self.deadline_unix_ms = deadline;
        self.turns = 0;
        self.stats = RunStats::default();
        self.observations.clear();
        self.messages.push(Message::text("user", prompt));
        self.request_model()
    }

    fn request_model(&mut self) -> Result<()> {
        if self.deadline_expired()? {
            self.stop(StopReason::DeadlineExceeded);
            return Ok(());
        }
        if self.turns >= self.max_turns {
            self.stop(StopReason::ModelCallLimit);
            return Ok(());
        }
        let cap = self.output_cap();
        if cap == Some(0) {
            self.stop(StopReason::OutputLimit);
            return Ok(());
        }
        let request = match self.prepare_request(cap)? {
            Ok(request) => request,
            Err(reason) => {
                self.stats.reserved_output_tokens = 0;
                self.stats.reserved_input_tokens = 0;
                self.stop(reason);
                return Ok(());
            }
        };
        // Local tokenization may take time, especially when initializing an encoding.
        if self.deadline_expired()? {
            self.stats.reserved_output_tokens = 0;
            self.stats.reserved_input_tokens = 0;
            self.stop(StopReason::DeadlineExceeded);
            return Ok(());
        }
        self.stats.reserved_output_tokens = if self.policy.max_total_output_tokens.is_some() {
            cap.unwrap_or(0)
        } else {
            0
        };
        self.stats.reserved_input_tokens = request
            .context
            .as_ref()
            .and_then(|context| context.token_estimate.as_ref())
            .map_or(0, |estimate| estimate.total_tokens);
        self.action = Action::Model { request };
        Ok(())
    }

    fn stop(&mut self, reason: StopReason) {
        self.action = Action::Stopped {
            reason,
            stats: self.stats.clone(),
        };
    }

    fn next_tool(&mut self) -> Result<()> {
        if self
            .stats
            .tool_calls
            .saturating_add(self.stats.tool_rejections)
            >= self.policy.max_tool_calls
        {
            self.stop(StopReason::ToolCallLimit);
        } else if let Some(call) = self.pending.first() {
            let tool = self
                .tools
                .iter()
                .find(|tool| tool.name == call.name)
                .ok_or_else(|| Error::State("pending tool is not registered".into()))?;
            self.action =
                match validation::rejection(&call.name, &tool.parameters, &call.arguments)? {
                    Some(result) => Action::ToolRejected {
                        call: call.clone(),
                        result,
                    },
                    None => Action::Tool { call: call.clone() },
                };
        }
        Ok(())
    }

    pub(crate) fn refresh_pending_tool(&mut self) -> Result<()> {
        if let Action::Tool { call } | Action::ToolRejected { call, .. } = &self.action {
            if self.pending.first() != Some(call) {
                return Err(Error::State("pending tool does not match action".into()));
            }
            self.next_tool()?;
        }
        Ok(())
    }

    pub fn accept_reply(&mut self, reply: ModelReply) -> Result<()> {
        if !matches!(self.action, Action::Model { .. }) {
            return Err(Error::State("no model response is expected".into()));
        }
        if let Some(state) = &reply.provider_state {
            provider::gemini::validate_state(state, &reply.content, &reply.tool_calls)?;
        }
        let reservation = match &self.action {
            Action::Model { request } => request.max_output_tokens,
            _ => None,
        };
        let mut ids = HashSet::new();
        for call in &reply.tool_calls {
            if call.id.is_empty()
                || !ids.insert(&call.id)
                || !self.tools.iter().any(|t| t.name == call.name)
            {
                return Err(Error::Input(
                    "unknown tool or duplicate/empty call ID".into(),
                ));
            }
        }
        if reply.tool_calls.is_empty() && reply.content.is_none() {
            return Err(Error::Provider(
                "response has neither content nor tool calls".into(),
            ));
        }
        self.turns += 1;
        self.stats.model_calls += 1;
        let estimated_input = match &self.action {
            Action::Model { request } => request
                .context
                .as_ref()
                .and_then(|context| context.token_estimate.as_ref())
                .map_or(0, |estimate| estimate.total_tokens),
            _ => 0,
        };
        self.stats.estimated_input_tokens = self
            .stats
            .estimated_input_tokens
            .saturating_add(estimated_input);
        if self.policy.input_token_budget.is_some() {
            self.stats.charged_input_tokens = self.stats.charged_input_tokens.saturating_add(
                estimated_input.max(reply.usage.as_ref().map_or(0, |usage| usage.input_tokens)),
            );
        }
        self.stats.reserved_input_tokens = 0;
        let exceeded_reservation = reply
            .usage
            .as_ref()
            .is_some_and(|usage| reservation.is_some_and(|cap| usage.output_tokens > cap));
        if let Some(usage) = reply.usage {
            self.stats.input_tokens = self.stats.input_tokens.saturating_add(usage.input_tokens);
            self.stats.output_tokens = self.stats.output_tokens.saturating_add(usage.output_tokens);
            self.stats.reserved_output_tokens = 0;
        } else {
            self.stats.unmetered_model_calls += 1;
            if self.policy.max_total_tokens.is_some()
                || self.policy.max_total_output_tokens.is_some()
                || self
                    .policy
                    .input_token_budget
                    .as_ref()
                    .is_some_and(|budget| budget.max_total_tokens.is_some())
            {
                self.stop(StopReason::UsageUnavailable);
                return Ok(());
            }
        }
        if self.deadline_expired()? {
            self.stop(StopReason::DeadlineExceeded);
            return Ok(());
        }
        if self
            .policy
            .input_token_budget
            .as_ref()
            .and_then(|budget| budget.max_total_tokens)
            .is_some_and(|limit| self.stats.charged_input_tokens > limit)
        {
            self.stop(StopReason::InputLimit);
            return Ok(());
        }
        if exceeded_reservation
            || (!reply.tool_calls.is_empty()
                && self
                    .policy
                    .max_total_output_tokens
                    .is_some_and(|limit| self.stats.output_tokens >= limit))
        {
            self.stop(StopReason::OutputLimit);
            return Ok(());
        }
        if self.policy.max_total_tokens.is_some_and(|limit| {
            self.stats
                .input_tokens
                .saturating_add(self.stats.output_tokens)
                >= limit
        }) {
            self.stop(StopReason::TokenLimit);
            return Ok(());
        }
        if !reply.tool_calls.is_empty() && self.turns >= self.max_turns {
            self.stop(StopReason::ModelCallLimit);
            return Ok(());
        }
        self.messages.push(Message {
            provider_state: reply.provider_state,
            role: "assistant".into(),
            content: reply.content.clone(),
            tool_calls: reply.tool_calls.clone(),
            tool_call_id: None,
        });
        self.pending = reply.tool_calls;
        if self.pending.is_empty() {
            if let Some(output) = &self.policy.structured_output
                && !output.accepts(reply.content.as_deref().unwrap_or_default())?
            {
                self.stop(StopReason::InvalidStructuredOutput);
                return Ok(());
            }
            self.action = Action::Complete {
                content: reply.content.unwrap_or_default(),
                stats: self.stats.clone(),
            };
        } else {
            self.next_tool()?;
        }
        Ok(())
    }

    pub fn accept_tool(&mut self, id: &str, result: Value) -> Result<()> {
        match &self.action {
            Action::Tool { call } if call.id == id => {}
            _ => {
                return Err(Error::State(
                    "tool result does not match pending call".into(),
                ));
            }
        }
        self.finish_tool(id, result, false)
    }

    pub fn accept_rejection(&mut self) -> Result<()> {
        let Action::ToolRejected { call, result } = &self.action else {
            return Err(Error::State("no schema rejection is pending".into()));
        };
        let id = call.id.clone();
        let result = result.clone();
        self.finish_tool(&id, result, true)
    }

    fn finish_tool(&mut self, id: &str, result: Value, rejected: bool) -> Result<()> {
        let call = self.pending[0].clone();
        if rejected {
            self.stats.tool_rejections += 1;
        } else {
            self.stats.tool_calls += 1;
        }
        let repeated = rejected
            || !self
                .tools
                .iter()
                .any(|t| t.name == call.name && t.allow_repeated_results);
        let observation = json!({"name": call.name, "arguments": call.arguments, "result": result});
        self.observations.push(observation.clone());
        self.messages.push(Message {
            provider_state: None,
            role: "tool".into(),
            content: Some(result.to_string()),
            tool_calls: vec![],
            tool_call_id: Some(id.into()),
        });
        self.pending.remove(0);
        if self.deadline_expired()? {
            self.stop(StopReason::DeadlineExceeded);
            Ok(())
        } else if repeated
            && self.policy.max_identical_tool_results.is_some_and(|limit| {
                self.observations
                    .iter()
                    .filter(|o| **o == observation)
                    .count()
                    >= limit as usize
            })
        {
            self.stop(StopReason::RepeatedToolResult);
            Ok(())
        } else if !self.pending.is_empty() {
            self.next_tool()
        } else {
            self.request_model()
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Command {
    Create {
        model: String,
        system: Option<String>,
        #[serde(default)]
        initial_history: Vec<Message>,
        #[serde(default)]
        tools: Vec<Tool>,
        #[serde(default = "default_turns")]
        max_turns: u32,
        #[serde(default)]
        policy: LoopPolicy,
        #[serde(default)]
        objective_id: Option<String>,
    },
    Start {
        state: Engine,
        prompt: String,
    },
    Reply {
        state: Engine,
        reply: ModelReply,
    },
    ToolResult {
        state: Engine,
        id: String,
        result: Value,
    },
    Reject {
        state: Engine,
    },
    Tick {
        state: Engine,
    },
    CaptureContext {
        state: Engine,
        start: usize,
        end: usize,
    },
    ReadContext {
        state: Engine,
        source: memory::ContextSource,
    },
    SearchContext {
        state: Engine,
        query: String,
        #[serde(default)]
        options: memory::ContextSearchOptions,
    },
    SetContextPlan {
        state: Engine,
        plan: Option<memory::ContextPlan>,
    },
}
fn default_turns() -> u32 {
    8
}

/// Internal versioned JSON bridge shared by both bindings; not a public persistence format.
pub fn dispatch(command: &str) -> Result<String> {
    let engine = match serde_json::from_str::<Command>(command)? {
        Command::Create {
            model,
            system,
            initial_history,
            tools,
            max_turns,
            policy,
            objective_id,
        } => {
            let mut engine = Engine::with_policy(model, system, tools, max_turns, policy)?;
            engine.seed_history(initial_history)?;
            if let Some(id) = objective_id {
                engine.bind_objective(id)?;
            }
            engine
        }
        Command::Start { mut state, prompt } => {
            state.start(prompt)?;
            state
        }
        Command::Reply { mut state, reply } => {
            state.accept_reply(reply)?;
            state
        }
        Command::ToolResult {
            mut state,
            id,
            result,
        } => {
            state.accept_tool(&id, result)?;
            state
        }
        Command::Reject { mut state } => {
            state.accept_rejection()?;
            state
        }
        Command::Tick { mut state } => {
            state.preflight()?;
            state
        }
        Command::CaptureContext { state, start, end } => {
            return Ok(serde_json::to_string(&state.capture_context(start, end)?)?);
        }
        Command::ReadContext { state, source } => {
            return Ok(serde_json::to_string(&state.read_context(&source)?)?);
        }
        Command::SearchContext {
            state,
            query,
            options,
        } => {
            return Ok(serde_json::to_string(
                &state.search_context(&query, options)?,
            )?);
        }
        Command::SetContextPlan { mut state, plan } => {
            state.set_context_plan(plan)?;
            state
        }
    };
    Ok(json!({ "state": engine, "action": engine.action() }).to_string())
}
