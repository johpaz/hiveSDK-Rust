//! Owned in-memory engine. Host callbacks never execute while its lock is held.
use crate::{Action, Engine, Error, LoopPolicy, Message, ModelReply, Prompt, Result, Tool, memory};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Mutex;

pub struct EngineHandle {
    inner: Mutex<State>,
}
struct State {
    committed: Engine,
    working: Option<Engine>,
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Config {
    Create {
        model: String,
        system: Option<String>,
        #[serde(default)]
        initial_history: Vec<Message>,
        #[serde(default)]
        tools: Vec<Tool>,
        #[serde(default = "crate::default_turns")]
        max_turns: u32,
        #[serde(default)]
        policy: LoopPolicy,
        #[serde(default)]
        objective_id: Option<String>,
    },
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Frame,
    Start {
        prompt: Prompt,
    },
    Reply {
        reply: ModelReply,
    },
    ToolResult {
        id: String,
        result: Value,
    },
    Reject,
    Tick,
    Commit,
    Abort,
    Messages,
    ContextPlan,
    CaptureContext {
        start: usize,
        end: usize,
    },
    ReadContext {
        source: memory::ContextSource,
    },
    SearchContext {
        query: String,
        #[serde(default)]
        options: memory::ContextSearchOptions,
    },
    SetContextPlan {
        plan: Option<memory::ContextPlan>,
    },
}
fn terminal(engine: &Engine) -> bool {
    matches!(
        engine.action(),
        Action::Complete { .. } | Action::Stopped { .. }
    )
}
fn frame(engine: &Engine) -> Value {
    json!({"state":{"deadline_unix_ms":engine.deadline_unix_ms},"action":engine.action()})
}
pub(crate) fn create_engine(config: &str) -> Result<Engine> {
    let Config::Create {
        model,
        system,
        initial_history,
        tools,
        max_turns,
        policy,
        objective_id,
    } = serde_json::from_str(config)?;
    let mut committed = Engine::with_policy(model, system, tools, max_turns, policy)?;
    committed.seed_history(initial_history)?;
    if let Some(id) = objective_id {
        committed.bind_objective(id)?;
    }
    Ok(committed)
}
impl EngineHandle {
    pub fn new(config: &str) -> Result<Self> {
        Ok(Self {
            inner: Mutex::new(State {
                committed: create_engine(config)?,
                working: None,
            }),
        })
    }
    pub fn dispatch(&self, command: &str) -> Result<String> {
        // Decode before locking. Invalid commands cannot change owned state.
        let operation: Operation = serde_json::from_str(command)?;
        let mut state = self
            .inner
            .lock()
            .map_err(|_| Error::State("poisoned engine handle".into()))?;
        let result = match operation {
            Operation::Messages => serde_json::to_value(state.committed.messages())?,
            Operation::ContextPlan => serde_json::to_value(state.committed.context_plan())?,
            Operation::CaptureContext { start, end } => {
                serde_json::to_value(state.committed.capture_context(start, end)?)?
            }
            Operation::ReadContext { source } => {
                serde_json::to_value(state.committed.read_context(&source)?)?
            }
            Operation::SearchContext { query, options } => {
                serde_json::to_value(state.committed.search_context(&query, options)?)?
            }
            Operation::SetContextPlan { plan } => {
                if state.working.is_some() {
                    return Err(Error::State(
                        "context cannot change during an active turn".into(),
                    ));
                }
                state.committed.set_context_plan(plan)?;
                frame(&state.committed)
            }
            Operation::Frame => frame(state.working.as_ref().unwrap_or(&state.committed)),
            Operation::Start { prompt } => {
                if state.working.is_some() {
                    return Err(Error::State(
                        "engine handle already has an active turn".into(),
                    ));
                }
                let mut working = state.committed.clone();
                working.start_prompt(prompt)?;
                state.working = Some(working);
                frame(state.working.as_ref().expect("just started"))
            }
            Operation::Commit => {
                let working = state
                    .working
                    .as_ref()
                    .ok_or_else(|| Error::State("no active turn to commit".into()))?;
                if !terminal(working) {
                    return Err(Error::State("cannot commit a nonterminal turn".into()));
                }
                state.committed = state.working.take().expect("checked active turn");
                frame(&state.committed)
            }
            Operation::Abort => {
                state.working = None;
                Value::Null
            }
            operation => {
                let working = state
                    .working
                    .as_mut()
                    .ok_or_else(|| Error::State("no active turn".into()))?;
                match operation {
                    Operation::Reply { reply } => working.accept_reply(reply)?,
                    Operation::ToolResult { id, result } => working.accept_tool(&id, result)?,
                    Operation::Reject => working.accept_rejection()?,
                    Operation::Tick => working.preflight()?,
                    _ => unreachable!("handled non-transition operation"),
                }
                frame(working)
            }
        };
        Ok(serde_json::to_string(&result)?)
    }
}
