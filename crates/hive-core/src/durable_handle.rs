//! Durable engine ownership; storage CAS and objective transactions stay in SessionStore.
use crate::{
    Engine, Error, Result, memory,
    sessions::{OperationResult, RunFrame, SessionStore},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, RwLock};

pub struct DurableEngineHandle {
    store: Arc<Mutex<Option<SessionStore>>>,
    agent: String,
    session: String,
    visible: RwLock<Arc<Engine>>,
    inner: Mutex<State>,
}
struct State {
    revision: u64,
    run: Option<RunFrame>,
    active: bool,
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Frame,
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
    BeginRun {
        prompt: String,
        #[serde(default)]
        objective_depth: u32,
        #[serde(default)]
        summary_key: Option<String>,
        #[serde(default)]
        objective_id: Option<String>,
    },
    ResumeRun,
    Claim,
    Check,
    Reject,
    Advance {
        execution_id: String,
        outcome: OperationResult,
    },
    Commit,
}
fn engine_frame(engine: &Engine) -> Value {
    json!({"state":{"deadline_unix_ms":engine.deadline_unix_ms,"objective_id":engine.objective_id},"action":engine.action()})
}
fn run_frame(run: &RunFrame) -> Value {
    let mut value = engine_frame(&run.state);
    let object = value.as_object_mut().expect("frame object");
    object.insert("revision".into(), json!(run.revision));
    object.insert("run_id".into(), json!(run.run_id));
    object.insert("operation".into(), json!(run.operation));
    object.insert("objective_depth".into(), json!(run.objective_depth));
    if let Some(id) = &run.objective_id {
        object.insert("objective_id".into(), json!(id));
    }
    if let Some(key) = &run.summary_key {
        object.insert("summary_key".into(), json!(key));
    }
    value
}
impl DurableEngineHandle {
    pub fn open(
        store: Arc<Mutex<Option<SessionStore>>>,
        agent: String,
        session: String,
        config: &str,
        resume: bool,
    ) -> Result<Self> {
        let initial = crate::handles::create_engine(config)?;
        let guard = store
            .lock()
            .map_err(|_| Error::Storage("poisoned store handle".into()))?;
        let source = guard.as_ref().ok_or(Error::StoreClosed)?;
        let (visible, revision, run) = if resume {
            let run = source.load_run(&agent, &session, &initial)?;
            (run.state.clone(), run.revision, Some(run))
        } else {
            let loaded = source.load(&agent, &session, &initial)?;
            (loaded.state, loaded.revision, None)
        };
        drop(guard);
        Ok(Self {
            store,
            agent,
            session,
            visible: RwLock::new(Arc::new(visible)),
            inner: Mutex::new(State {
                revision,
                run,
                active: false,
            }),
        })
    }
    fn visible(&self) -> Result<Arc<Engine>> {
        Ok(self
            .visible
            .read()
            .map_err(|_| Error::State("poisoned visible engine".into()))?
            .clone())
    }
    fn publish(&self, engine: Engine) -> Result<()> {
        *self
            .visible
            .write()
            .map_err(|_| Error::State("poisoned visible engine".into()))? = Arc::new(engine);
        Ok(())
    }
    pub fn dispatch(&self, command: &str) -> Result<String> {
        let command: Command = serde_json::from_str(command)?;
        // These snapshots never wait for storage I/O or the transition mutex.
        let query = match &command {
            Command::Messages => Some(serde_json::to_value(self.visible()?.messages())?),
            Command::ContextPlan => Some(serde_json::to_value(self.visible()?.context_plan())?),
            Command::CaptureContext { start, end } => Some(serde_json::to_value(
                self.visible()?.capture_context(*start, *end)?,
            )?),
            Command::ReadContext { source } => {
                Some(serde_json::to_value(self.visible()?.read_context(source)?)?)
            }
            Command::SearchContext { query, options } => Some(serde_json::to_value(
                self.visible()?.search_context(query, options.clone())?,
            )?),
            _ => None,
        };
        if let Some(value) = query {
            return Ok(serde_json::to_string(&value)?);
        }
        let mut state = self
            .inner
            .lock()
            .map_err(|_| Error::State("poisoned durable handle".into()))?;
        let store_guard = self
            .store
            .lock()
            .map_err(|_| Error::Storage("poisoned store handle".into()))?;
        let store = store_guard.as_ref().ok_or(Error::StoreClosed)?;
        let next = match command {
            Command::Frame => {
                let mut frame = if let Some(run) = &state.run {
                    run_frame(run)
                } else {
                    engine_frame(self.visible()?.as_ref())
                };
                frame["revision"] = json!(state.revision);
                return Ok(frame.to_string());
            }
            Command::SetContextPlan { plan } => {
                if state.active {
                    return Err(Error::SessionInterrupted);
                }
                let loaded = store.set_context_plan(
                    &self.agent,
                    &self.session,
                    state.revision,
                    (*self.visible()?).clone(),
                    plan,
                )?;
                state.revision = loaded.revision;
                let mut frame = engine_frame(&loaded.state);
                frame["revision"] = json!(state.revision);
                self.publish(loaded.state)?;
                return Ok(frame.to_string());
            }
            Command::BeginRun {
                prompt,
                objective_depth,
                summary_key,
                objective_id,
            } => {
                if state.active {
                    return Err(Error::SessionInterrupted);
                }
                store.begin_run_with_objective(
                    &self.agent,
                    &self.session,
                    state.revision,
                    (*self.visible()?).clone(),
                    prompt,
                    objective_depth,
                    summary_key,
                    objective_id,
                )?
            }
            Command::ResumeRun => {
                if state.active {
                    return Err(Error::SessionInterrupted);
                }
                store.resume_run(&self.agent, &self.session, state.revision)?
            }
            Command::Commit => {
                if !state.active {
                    return Err(Error::State("no active durable turn".into()));
                }
                let run = state
                    .run
                    .as_ref()
                    .ok_or_else(|| Error::State("no durable checkpoint".into()))?;
                let revision = store.commit(
                    &self.agent,
                    &self.session,
                    state.revision,
                    run.state.clone(),
                )?;
                self.publish(run.state.clone())?;
                state.revision = revision;
                state.active = false;
                return Ok(json!(revision).to_string());
            }
            command => {
                if !state.active {
                    return Err(Error::State(
                        "durable turn must begin or resume before a transition".into(),
                    ));
                }
                match command {
                    Command::Claim => store.claim(&self.agent, &self.session, state.revision)?,
                    Command::Check => store.check(&self.agent, &self.session, state.revision)?,
                    Command::Reject => store.reject(&self.agent, &self.session, state.revision)?,
                    Command::Advance {
                        execution_id,
                        outcome,
                    } => store.advance(
                        &self.agent,
                        &self.session,
                        state.revision,
                        &execution_id,
                        outcome,
                    )?,
                    _ => unreachable!("queries handled before transitions"),
                }
            }
        };
        let output = run_frame(&next);
        state.revision = next.revision;
        state.run = Some(next);
        state.active = true;
        Ok(output.to_string())
    }
}
