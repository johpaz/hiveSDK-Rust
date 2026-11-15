//! Durable checkpoints and tool ledger. Uncertain operations never restart implicitly.
use crate::{Action, Engine, Error, ModelReply, Result, ToolCall};
use hivedb_core::{ColOp, HiveDB, HiveError, PutOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

const COLLECTION: &str = "hive_sdk_rust_sessions_v1";
const RUNS: &str = "hive_sdk_rust_runs_v1";

#[derive(Clone)]
pub struct SessionStore {
    pub(crate) db: Arc<HiveDB>,
    pub(crate) tenant_id: String,
    pub(crate) objective_lock: Arc<Mutex<()>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Ready,
    Running,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedSession {
    pub revision: u64,
    pub status: SessionStatus,
    pub state: Engine,
}

#[derive(Serialize, Deserialize)]
struct Record {
    schema_version: u32,
    tenant_id: String,
    agent_id: String,
    session_id: String,
    status: SessionStatus,
    state: Engine,
    #[serde(default)]
    run: Option<RunCheckpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    pub execution_id: String,
    pub kind: String,
    pub tool_call: Option<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective_permit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecution {
    pub execution_id: String,
    pub call: ToolCall,
    pub status: String,
    pub result: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunCheckpoint {
    pub run_id: String,
    /// Optional root for this turn of an otherwise unbound conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objective_id: Option<String>,
    pub state: Engine,
    pub in_flight: Option<Operation>,
    pub tools: Vec<ToolExecution>,
    pub abandoned: bool,
    #[serde(default)]
    pub objective_depth: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_key: Option<String>,
}

impl RunCheckpoint {
    fn root(&self) -> Option<&String> {
        self.objective_id
            .as_ref()
            .or(self.state.objective_id.as_ref())
    }
}

#[derive(Debug, Serialize)]
pub struct RunFrame {
    pub revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub objective_id: Option<String>,
    pub state: Engine,
    pub action: Action,
    pub run_id: String,
    pub operation: Option<Operation>,
    pub objective_depth: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary_key: Option<String>,
}

impl RunFrame {
    fn new(revision: u64, run: &RunCheckpoint) -> Self {
        Self {
            revision,
            objective_id: run.root().cloned(),
            state: run.state.clone(),
            action: run.state.action().clone(),
            run_id: run.run_id.clone(),
            operation: run.in_flight.clone(),
            objective_depth: run.objective_depth,
            summary_key: run.summary_key.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperationResult {
    Model { reply: ModelReply },
    Tool { result: Value },
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Command {
    #[cfg(feature = "harness")]
    Queue {
        id: String,
        command: Value,
    },
    #[cfg(feature = "services")]
    Services {
        command: Value,
    },
    SetContextPlan {
        agent_id: String,
        session_id: String,
        revision: u64,
        initial: Engine,
        plan: Option<crate::memory::ContextPlan>,
    },
    OpenObjective {
        id: String,
        policy: crate::objective::ObjectivePolicy,
    },
    Objective {
        id: String,
        command: Value,
    },
    Check {
        agent_id: String,
        session_id: String,
        revision: u64,
    },
    Reject {
        agent_id: String,
        session_id: String,
        revision: u64,
    },
    BeginRun {
        agent_id: String,
        session_id: String,
        revision: u64,
        initial: Engine,
        prompt: String,
        #[serde(default)]
        objective_depth: u32,
        #[serde(default)]
        summary_key: Option<String>,
        #[serde(default)]
        objective_id: Option<String>,
    },
    LoadRun {
        agent_id: String,
        session_id: String,
        initial: Engine,
    },
    ResumeRun {
        agent_id: String,
        session_id: String,
        revision: u64,
    },
    Claim {
        agent_id: String,
        session_id: String,
        revision: u64,
    },
    Advance {
        agent_id: String,
        session_id: String,
        revision: u64,
        execution_id: String,
        outcome: OperationResult,
    },
    Inspect {
        agent_id: String,
        session_id: String,
        run_id: Option<String>,
    },
    Resolve {
        agent_id: String,
        session_id: String,
        revision: u64,
        execution_id: String,
        outcome: OperationResult,
    },
    Load {
        agent_id: String,
        session_id: String,
        initial: Engine,
    },
    Begin {
        agent_id: String,
        session_id: String,
        revision: u64,
        initial: Engine,
    },
    Commit {
        agent_id: String,
        session_id: String,
        revision: u64,
        state: Engine,
    },
    Recover {
        agent_id: String,
        session_id: String,
    },
}

pub(crate) fn storage(error: HiveError) -> Error {
    match error {
        HiveError::InvalidInput(message) if message.starts_with("version conflict on ") => {
            Error::SessionConflict
        }
        error => Error::Storage(error.to_string()),
    }
}

fn valid_id(id: &str) -> Result<()> {
    if id.trim().is_empty() {
        return Err(Error::Input("session identifiers must not be empty".into()));
    }
    Ok(())
}

fn config(state: &Engine) -> Value {
    json!({"version": state.version, "model": state.model, "tools": state.tools,
        "max_turns": state.max_turns, "policy": state.policy, "objective_id": state.objective_id,
        "system": state.messages.iter().filter(|m| m.role == "system").collect::<Vec<_>>()})
}

// JSON bridges normalize portable integral floats (1.0 -> 1, -0.0 -> 0).
// Accept that representation change without weakening revision CAS or treating
// large integers / changed values as equivalent.
fn same_json(left: &Value, right: &Value) -> bool {
    if left == right {
        return true;
    }
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a
            .as_f64()
            .zip(b.as_f64())
            .is_some_and(|(a, b)| a == b && a.fract() == 0.0 && a.abs() <= 9_007_199_254_740_991.0),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_json(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.get(key).is_some_and(|other| same_json(value, other)))
        }
        _ => false,
    }
}

impl SessionStore {
    pub fn set_context_plan(
        &self,
        agent: &str,
        session: &str,
        revision: u64,
        initial: Engine,
        plan: Option<crate::memory::ContextPlan>,
    ) -> Result<LoadedSession> {
        valid_id(agent)?;
        valid_id(session)?;
        let mut state = if let Some((current, record)) = self.read(agent, session)? {
            if current != revision {
                return Err(Error::SessionConflict);
            }
            if !matches!(record.status, SessionStatus::Ready) {
                return Err(Error::SessionInterrupted);
            }
            if !same_json(&config(&record.state), &config(&initial)) {
                return Err(Error::SessionConfigMismatch);
            }
            record.state
        } else {
            if revision != 0 {
                return Err(Error::SessionConflict);
            }
            initial
        };
        state.set_context_plan(plan)?;
        let next = if let Some((current, mut record)) = self.read(agent, session)? {
            if current != revision {
                return Err(Error::SessionConflict);
            }
            record.state = state.clone();
            self.write_record(agent, session, revision, record)?
        } else {
            self.write(
                agent,
                session,
                revision,
                state.clone(),
                SessionStatus::Ready,
            )?
        };
        Ok(LoadedSession {
            revision: next,
            status: SessionStatus::Ready,
            state,
        })
    }

    pub fn dispatch(&self, command: &str) -> Result<String> {
        let value = match serde_json::from_str::<Command>(command)? {
            #[cfg(feature = "harness")]
            Command::Queue { id, command } => self.queue(&id)?.dispatch(command)?,
            #[cfg(feature = "services")]
            Command::Services { command } => self.services().dispatch(command)?,
            Command::SetContextPlan {
                agent_id,
                session_id,
                revision,
                initial,
                plan,
            } => serde_json::to_value(self.set_context_plan(
                &agent_id,
                &session_id,
                revision,
                initial,
                plan,
            )?)?,
            Command::OpenObjective { id, policy } => self.open_objective(&id, policy)?,
            Command::Objective { id, command } => {
                self.objective_command(&id, &command.to_string())?
            }
            Command::Check {
                agent_id,
                session_id,
                revision,
            } => serde_json::to_value(self.check(&agent_id, &session_id, revision)?)?,
            Command::Reject {
                agent_id,
                session_id,
                revision,
            } => serde_json::to_value(self.reject(&agent_id, &session_id, revision)?)?,
            Command::BeginRun {
                agent_id,
                session_id,
                revision,
                initial,
                prompt,
                objective_depth,
                summary_key,
                objective_id,
            } => serde_json::to_value(self.begin_run_with_objective(
                &agent_id,
                &session_id,
                revision,
                initial,
                prompt,
                objective_depth,
                summary_key,
                objective_id,
            )?)?,
            Command::LoadRun {
                agent_id,
                session_id,
                initial,
            } => serde_json::to_value(self.load_run(&agent_id, &session_id, &initial)?)?,
            Command::ResumeRun {
                agent_id,
                session_id,
                revision,
            } => serde_json::to_value(self.resume_run(&agent_id, &session_id, revision)?)?,
            Command::Claim {
                agent_id,
                session_id,
                revision,
            } => serde_json::to_value(self.claim(&agent_id, &session_id, revision)?)?,
            Command::Advance {
                agent_id,
                session_id,
                revision,
                execution_id,
                outcome,
            }
            | Command::Resolve {
                agent_id,
                session_id,
                revision,
                execution_id,
                outcome,
            } => serde_json::to_value(self.advance(
                &agent_id,
                &session_id,
                revision,
                &execution_id,
                outcome,
            )?)?,
            Command::Inspect {
                agent_id,
                session_id,
                run_id,
            } => self.inspect(&agent_id, &session_id, run_id.as_deref())?,
            Command::Load {
                agent_id,
                session_id,
                initial,
            } => serde_json::to_value(self.load(&agent_id, &session_id, &initial)?)?,
            Command::Begin {
                agent_id,
                session_id,
                revision,
                initial,
            } => json!(self.begin(&agent_id, &session_id, revision, initial)?),
            Command::Commit {
                agent_id,
                session_id,
                revision,
                state,
            } => json!(self.commit(&agent_id, &session_id, revision, state)?),
            Command::Recover {
                agent_id,
                session_id,
            } => json!(self.recover(&agent_id, &session_id)?),
        };
        Ok(value.to_string())
    }

    pub fn open(path: impl AsRef<Path>, tenant_id: String) -> Result<Self> {
        valid_id(&tenant_id)?;
        Self::from_db(Arc::new(HiveDB::open(path).map_err(storage)?), tenant_id)
    }

    /// Share the host's existing database instead of reopening its files.
    pub fn from_db(db: Arc<HiveDB>, tenant_id: String) -> Result<Self> {
        valid_id(&tenant_id)?;
        Ok(Self {
            db,
            tenant_id,
            objective_lock: Arc::new(Mutex::new(())),
        })
    }

    /// Multiple tenant-scoped stores can share one database without reopening its files.
    pub fn for_tenant(&self, tenant_id: String) -> Result<Self> {
        valid_id(&tenant_id)?;
        Ok(Self {
            db: self.db.clone(),
            tenant_id,
            objective_lock: self.objective_lock.clone(),
        })
    }

    fn key(&self, agent_id: &str, session_id: &str) -> Result<String> {
        valid_id(agent_id)?;
        valid_id(session_id)?;
        // Tuple encoding avoids collisions from arbitrary delimiters in caller identifiers.
        Ok(serde_json::to_string(&(
            &self.tenant_id,
            agent_id,
            session_id,
        ))?)
    }

    fn read(&self, agent_id: &str, session_id: &str) -> Result<Option<(u64, Record)>> {
        let Some(doc) = self
            .db
            .col_get(COLLECTION, &self.key(agent_id, session_id)?)
            .map_err(storage)?
        else {
            return Ok(None);
        };
        let record: Record = serde_json::from_value(doc.doc)?;
        if !matches!(record.schema_version, 1..=6)
            || (record.schema_version == 3 && record.state.objective_id.is_none())
            || (record.schema_version < 4
                && (record.state.context_plan.is_some()
                    || record
                        .run
                        .as_ref()
                        .is_some_and(|run| run.state.context_plan.is_some())))
            || record.state.version != 1
            || record.tenant_id != self.tenant_id
            || record.agent_id != agent_id
            || record.session_id != session_id
        {
            return Err(Error::Storage(
                "unsupported or mismatched session record".into(),
            ));
        }
        if !matches!(
            record.state.action(),
            Action::Idle | Action::Complete { .. } | Action::Stopped { .. }
        ) {
            return Err(Error::Storage("invalid committed session state".into()));
        }
        if let Some(run) = &record.run {
            if run.objective_id.as_ref().is_some_and(|id| {
                record.schema_version < 6
                    || record.state.objective_id.is_some()
                    || valid_id(id).is_err()
            }) || (run.objective_depth > 0 && run.root().is_none())
            {
                return Err(Error::Storage("invalid per-turn objective scope".into()));
            }
            if run.summary_key.as_ref().is_some_and(|key| {
                record.schema_version < 5
                    || run.objective_depth == 0
                    || run.root().is_none()
                    || key.trim().is_empty()
                    || key.len() > 256
            }) {
                return Err(Error::Storage("invalid summary scope in checkpoint".into()));
            }
            if run.state.version != 1 || !same_json(&config(&run.state), &config(&record.state)) {
                return Err(Error::Storage(
                    "checkpoint configuration does not match session".into(),
                ));
            }
            if let Some(op) = &run.in_flight {
                let valid = match (op.kind.as_str(), run.state.action()) {
                    ("model", Action::Model { .. }) => op.tool_call.is_none(),
                    ("tool", Action::Tool { call }) => op.tool_call.as_ref() == Some(call),
                    _ => false,
                };
                if !valid {
                    return Err(Error::Storage(
                        "checkpoint operation does not match state".into(),
                    ));
                }
                if (op.kind == "model" && run.root().is_some()) != op.objective_permit.is_some() {
                    return Err(Error::Storage(
                        "objective permit does not match operation".into(),
                    ));
                }
            }
        }
        Ok(Some((doc.version, record)))
    }

    pub fn load(
        &self,
        agent_id: &str,
        session_id: &str,
        initial: &Engine,
    ) -> Result<LoadedSession> {
        match self.read(agent_id, session_id)? {
            None => Ok(LoadedSession {
                revision: 0,
                status: SessionStatus::Ready,
                state: initial.clone(),
            }),
            Some((revision, record)) => {
                if !same_json(&config(&record.state), &config(initial)) {
                    return Err(Error::SessionConfigMismatch);
                }
                if matches!(record.status, SessionStatus::Running) {
                    return Err(Error::SessionInterrupted);
                }
                Ok(LoadedSession {
                    revision,
                    status: record.status,
                    state: record.state,
                })
            }
        }
    }

    fn write(
        &self,
        agent_id: &str,
        session_id: &str,
        revision: u64,
        state: Engine,
        status: SessionStatus,
    ) -> Result<u64> {
        let record = Record {
            schema_version: if state.context_plan.is_some() {
                4
            } else if state.objective_id.is_some() {
                3
            } else {
                2
            },
            tenant_id: self.tenant_id.clone(),
            agent_id: agent_id.into(),
            session_id: session_id.into(),
            status,
            state,
            run: None,
        };
        self.db
            .col_put(
                COLLECTION,
                &self.key(agent_id, session_id)?,
                &serde_json::to_value(record)?,
                PutOptions {
                    expected_version: Some(revision),
                },
            )
            .map_err(storage)
    }

    /// Claim before any provider/tool/hook is invoked. CAS prevents stale writers from doing work.
    pub fn begin(
        &self,
        agent_id: &str,
        session_id: &str,
        revision: u64,
        initial: Engine,
    ) -> Result<u64> {
        if let Some((current, record)) = self.read(agent_id, session_id)? {
            if current != revision {
                return Err(Error::SessionConflict);
            }
            if matches!(record.status, SessionStatus::Running) {
                return Err(Error::SessionInterrupted);
            }
            if !same_json(
                &serde_json::to_value(&record.state)?,
                &serde_json::to_value(&initial)?,
            ) {
                return Err(Error::SessionConflict);
            }
        } else if revision != 0 {
            return Err(Error::SessionConflict);
        }
        if !matches!(initial.action(), Action::Idle | Action::Complete { .. }) {
            return Err(Error::State("cannot start a stopped session".into()));
        }
        self.write(
            agent_id,
            session_id,
            revision,
            initial,
            SessionStatus::Running,
        )
    }

    pub fn commit(
        &self,
        agent_id: &str,
        session_id: &str,
        revision: u64,
        state: Engine,
    ) -> Result<u64> {
        if !matches!(
            state.action(),
            Action::Complete { .. } | Action::Stopped { .. }
        ) {
            return Err(Error::State(
                "only terminal run states can be committed".into(),
            ));
        }
        let Some((current, mut record)) = self.read(agent_id, session_id)? else {
            return Err(Error::SessionConflict);
        };
        if current != revision || !matches!(record.status, SessionStatus::Running) {
            return Err(Error::SessionConflict);
        }
        if !same_json(&config(&record.state), &config(&state)) {
            return Err(Error::SessionConfigMismatch);
        }
        if let Some(run) = &record.run
            && (run.in_flight.is_some()
                || !same_json(
                    &serde_json::to_value(&run.state)?,
                    &serde_json::to_value(&state)?,
                ))
        {
            return Err(Error::State(
                "commit must match the durable checkpoint".into(),
            ));
        }
        record.state = state;
        record.status = SessionStatus::Ready;
        self.write_record(agent_id, session_id, revision, record)
    }

    /// Explicitly discard an unfinished run and retain its previously committed conversation.
    /// The caller must ensure the original executor has stopped and reconcile external effects.
    pub fn recover(&self, agent_id: &str, session_id: &str) -> Result<u64> {
        let Some((revision, mut record)) = self.read(agent_id, session_id)? else {
            return Err(Error::Input("session does not exist".into()));
        };
        if !matches!(record.status, SessionStatus::Running) {
            return Err(Error::State("session does not need recovery".into()));
        }
        record.status = SessionStatus::Ready;
        if let Some(run) = &mut record.run {
            run.abandoned = true;
        }
        self.write_record(agent_id, session_id, revision, record)
    }

    fn write_record(
        &self,
        agent: &str,
        session: &str,
        revision: u64,
        record: Record,
    ) -> Result<u64> {
        self.write_record_with_extra(agent, session, revision, record, vec![])
    }

    fn write_record_with_extra(
        &self,
        agent: &str,
        session: &str,
        revision: u64,
        mut record: Record,
        mut ops: Vec<ColOp>,
    ) -> Result<u64> {
        if record.schema_version == 6
            || record
                .run
                .as_ref()
                .is_some_and(|run| run.objective_id.is_some())
        {
            record.schema_version = 6;
        } else if record.schema_version == 5
            || record
                .run
                .as_ref()
                .is_some_and(|run| run.summary_key.is_some())
        {
            record.schema_version = 5;
        } else if record.state.context_plan.is_some()
            || record.schema_version == 4
            || record
                .run
                .as_ref()
                .is_some_and(|run| run.state.context_plan.is_some())
        {
            record.schema_version = 4;
        } else if record.state.objective_id.is_some() {
            record.schema_version = 3;
        }
        let next = revision
            .checked_add(1)
            .ok_or_else(|| Error::Storage("revision overflow".into()))?;
        ops.push(ColOp::Put {
            collection: COLLECTION.into(),
            id: self.key(agent, session)?,
            doc: serde_json::to_value(&record)?,
            expected_version: Some(revision),
        });
        if let Some(run) = &record.run {
            ops.push(ColOp::Put {
                collection: RUNS.into(),
                id: serde_json::to_string(&(&self.tenant_id, agent, session, &run.run_id))?,
                doc: json!({"schema_version":1,"tenant_id":self.tenant_id,"agent_id":agent,
                    "session_id":session,"status":record.status,"run":run}),
                expected_version: None,
            });
        }
        self.db.col_batch(&ops).map_err(storage)?;
        Ok(next)
    }

    fn objective_effect(&self, id: &str, command: Value, writes: &mut Vec<ColOp>) -> Result<Value> {
        let prepared = self.prepare_objective(id, &command.to_string())?;
        match prepared.result {
            Ok(value) => {
                if let Some(write) = prepared.write {
                    writes.push(write);
                }
                Ok(value)
            }
            Err(error) => {
                // A deadline may become terminal even when this operation is refused.
                if let Some(write) = prepared.write {
                    self.db.col_batch(&[write]).map_err(storage)?;
                }
                Err(error)
            }
        }
    }

    fn active(&self, agent: &str, session: &str, revision: u64) -> Result<Record> {
        let Some((current, record)) = self.read(agent, session)? else {
            return Err(Error::SessionConflict);
        };
        if current != revision || !matches!(record.status, SessionStatus::Running) {
            return Err(Error::SessionConflict);
        }
        if record.run.is_none() {
            return Err(Error::State(
                "legacy run has no checkpoint; use explicit recovery".into(),
            ));
        }
        Ok(record)
    }

    pub fn begin_run(
        &self,
        agent: &str,
        session: &str,
        revision: u64,
        initial: Engine,
        prompt: String,
    ) -> Result<RunFrame> {
        self.begin_run_at_depth(agent, session, revision, initial, prompt, 0)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn begin_run_at_depth(
        &self,
        agent: &str,
        session: &str,
        revision: u64,
        initial: Engine,
        prompt: String,
        objective_depth: u32,
    ) -> Result<RunFrame> {
        self.begin_run_scoped(
            agent,
            session,
            revision,
            initial,
            prompt,
            objective_depth,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn begin_run_scoped(
        &self,
        agent: &str,
        session: &str,
        revision: u64,
        initial: Engine,
        prompt: String,
        objective_depth: u32,
        summary_key: Option<String>,
    ) -> Result<RunFrame> {
        self.begin_run_with_objective(
            agent,
            session,
            revision,
            initial,
            prompt,
            objective_depth,
            summary_key,
            None,
        )
    }

    /// Begin a turn under an existing durable root without rebinding the conversation.
    /// A bound session cannot replace its root; interrupted turns cannot change scope.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_run_with_objective(
        &self,
        agent: &str,
        session: &str,
        revision: u64,
        initial: Engine,
        prompt: String,
        objective_depth: u32,
        summary_key: Option<String>,
        objective_id: Option<String>,
    ) -> Result<RunFrame> {
        if let Some(id) = &objective_id {
            valid_id(id)?;
            if initial.objective_id.is_some() {
                return Err(Error::Input(
                    "per-turn objective requires an unbound session".into(),
                ));
            }
        }
        // Check ownership without claiming it until the initial checkpoint is ready to commit.
        let loaded = self.load(agent, session, &initial)?;
        if loaded.revision != revision
            || !same_json(
                &serde_json::to_value(&loaded.state)?,
                &serde_json::to_value(&initial)?,
            )
        {
            return Err(Error::SessionConflict);
        }
        if let Some(id) = objective_id.as_ref().or(initial.objective_id.as_ref()) {
            let command = if let Some(key) = &summary_key {
                json!({"op":"summary_check","key":key,"depth":objective_depth})
            } else {
                json!({"op":"check","depth":objective_depth})
            };
            self.objective_command(id, &command.to_string())?;
        } else if objective_depth != 0 || summary_key.is_some() {
            return Err(Error::Input(
                "objective depth requires a bound objective".into(),
            ));
        }
        let mut state = initial.clone();
        state.start(prompt)?;
        let run = RunCheckpoint {
            run_id: uuid::Uuid::new_v4().to_string(),
            objective_id,
            state,
            in_flight: None,
            tools: vec![],
            abandoned: false,
            objective_depth,
            summary_key,
        };
        let record = Record {
            schema_version: 2,
            tenant_id: self.tenant_id.clone(),
            agent_id: agent.into(),
            session_id: session.into(),
            status: SessionStatus::Running,
            state: initial,
            run: Some(run.clone()),
        };
        let revision = self.write_record(agent, session, revision, record)?;
        Ok(RunFrame::new(revision, &run))
    }

    pub fn load_run(&self, agent: &str, session: &str, initial: &Engine) -> Result<RunFrame> {
        let Some((revision, record)) = self.read(agent, session)? else {
            return Err(Error::Input("session does not exist".into()));
        };
        if !same_json(&config(&record.state), &config(initial)) {
            return Err(Error::SessionConfigMismatch);
        }
        if !matches!(record.status, SessionStatus::Running) {
            return Err(Error::State("session has no interrupted run".into()));
        }
        let run = record.run.ok_or_else(|| {
            Error::State("legacy run has no checkpoint; use explicit recovery".into())
        })?;
        if run.in_flight.is_some() {
            return Err(Error::SessionOperationUncertain);
        }
        Ok(RunFrame::new(revision, &run))
    }

    pub fn resume_run(&self, agent: &str, session: &str, revision: u64) -> Result<RunFrame> {
        let mut record = self.active(agent, session, revision)?;
        let run = record.run.as_mut().unwrap();
        if run.in_flight.is_some() {
            return Err(Error::SessionOperationUncertain);
        }
        run.state.preflight()?;
        let run = run.clone();
        let revision = self.write_record(agent, session, revision, record)?;
        Ok(RunFrame::new(revision, &run))
    }

    pub fn claim(&self, agent: &str, session: &str, revision: u64) -> Result<RunFrame> {
        let _guard = self
            .objective_lock
            .lock()
            .map_err(|_| Error::Storage("poisoned objective transaction guard".into()))?;
        let mut record = self.active(agent, session, revision)?;
        let run = record.run.as_mut().unwrap();
        if run.in_flight.is_some() {
            return Err(Error::SessionOperationUncertain);
        }
        if matches!(
            run.state.action(),
            Action::Complete { .. } | Action::Stopped { .. }
        ) {
            return Err(Error::State(
                "terminal runs have no operation to claim".into(),
            ));
        }
        run.state.preflight()?;
        if matches!(run.state.action(), Action::Stopped { .. }) {
            let mut frame = RunFrame::new(revision, run);
            frame.revision = self.write_record(agent, session, revision, record)?;
            return Ok(frame);
        }
        let execution_id = uuid::Uuid::new_v4().to_string();
        let mut objective_writes = vec![];
        let mut objective_permit = None;
        if let Some(id) = run.root() {
            let mut command = match run.state.action() {
                Action::Model { request } => {
                    json!({"op":"model","depth":run.objective_depth,"cap":request.max_output_tokens})
                }
                Action::Tool { .. } => json!({"op":"tool","depth":run.objective_depth}),
                _ => return Err(Error::State("no objective operation to claim".into())),
            };
            if let Some(key) = &run.summary_key {
                command["op"] = json!(if matches!(run.state.action(), Action::Model { .. }) {
                    "summary_model"
                } else {
                    "summary_tool"
                });
                command["key"] = json!(key);
            }
            let effect = self.objective_effect(id, command, &mut objective_writes)?;
            if matches!(run.state.action(), Action::Model { .. }) {
                let permit: crate::objective::ModelPermit = serde_json::from_value(effect)?;
                objective_permit = Some(permit.id);
                run.state.apply_objective_permit(&permit)?;
            }
        }
        let operation = match run.state.action() {
            Action::Model { .. } => Operation {
                execution_id,
                kind: "model".into(),
                tool_call: None,
                objective_permit,
            },
            Action::Tool { call } => {
                run.tools.push(ToolExecution {
                    execution_id: execution_id.clone(),
                    call: call.clone(),
                    status: "started".into(),
                    result: None,
                });
                Operation {
                    execution_id,
                    kind: "tool".into(),
                    tool_call: Some(call.clone()),
                    objective_permit: None,
                }
            }
            _ => {
                return Err(Error::State(
                    "terminal runs have no operation to claim".into(),
                ));
            }
        };
        run.in_flight = Some(operation);
        let mut frame = RunFrame::new(revision, run);
        frame.revision =
            self.write_record_with_extra(agent, session, revision, record, objective_writes)?;
        Ok(frame)
    }

    pub fn reject(&self, agent: &str, session: &str, revision: u64) -> Result<RunFrame> {
        let _guard = self
            .objective_lock
            .lock()
            .map_err(|_| Error::Storage("poisoned objective transaction guard".into()))?;
        let mut record = self.active(agent, session, revision)?;
        let run = record.run.as_mut().unwrap();
        if run.in_flight.is_some() {
            return Err(Error::SessionOperationUncertain);
        }
        let Action::ToolRejected { call, result } = run.state.action() else {
            return Err(Error::State("no schema rejection is pending".into()));
        };
        let entry = ToolExecution {
            execution_id: uuid::Uuid::new_v4().to_string(),
            call: call.clone(),
            status: "rejected".into(),
            result: Some(result.clone()),
        };
        let mut objective_writes = vec![];
        if let Some(id) = run.root() {
            let command = if let Some(key) = &run.summary_key {
                json!({"op":"summary_tool","key":key,"depth":run.objective_depth})
            } else {
                json!({"op":"tool","depth":run.objective_depth})
            };
            self.objective_effect(id, command, &mut objective_writes)?;
        }
        run.state.accept_rejection()?;
        run.tools.push(entry);
        let mut frame = RunFrame::new(revision, run);
        frame.revision =
            self.write_record_with_extra(agent, session, revision, record, objective_writes)?;
        Ok(frame)
    }

    pub fn check(&self, agent: &str, session: &str, revision: u64) -> Result<RunFrame> {
        let mut record = self.active(agent, session, revision)?;
        let run = record.run.as_mut().unwrap();
        if run.in_flight.is_some() {
            return Err(Error::SessionOperationUncertain);
        }
        let before = serde_json::to_value(&run.state)?;
        run.state.preflight()?;
        let changed = before != serde_json::to_value(&run.state)?;
        let mut frame = RunFrame::new(revision, run);
        if changed {
            frame.revision = self.write_record(agent, session, revision, record)?;
        }
        Ok(frame)
    }

    pub fn advance(
        &self,
        agent: &str,
        session: &str,
        revision: u64,
        execution_id: &str,
        outcome: OperationResult,
    ) -> Result<RunFrame> {
        let _guard = self
            .objective_lock
            .lock()
            .map_err(|_| Error::Storage("poisoned objective transaction guard".into()))?;
        let mut record = self.active(agent, session, revision)?;
        let run = record.run.as_mut().unwrap();
        let op = run
            .in_flight
            .as_ref()
            .ok_or_else(|| Error::State("no operation is in flight".into()))?;
        if op.execution_id != execution_id {
            return Err(Error::SessionConflict);
        }
        let mut objective_writes = vec![];
        match (op.kind.as_str(), outcome) {
            ("model", OperationResult::Model { reply }) => {
                let usage = reply.usage.clone();
                run.state.accept_reply(reply)?;
                if let Some(id) = run.root() {
                    let permit = op
                        .objective_permit
                        .ok_or_else(|| Error::Storage("missing objective model permit".into()))?;
                    self.objective_effect(
                        id,
                        json!({"op":"settle","id":permit,"usage":usage}),
                        &mut objective_writes,
                    )?;
                    if usage.is_none() {
                        run.state.stop(crate::StopReason::UsageUnavailable);
                    }
                }
            }
            ("tool", OperationResult::Tool { result }) => {
                let call = op
                    .tool_call
                    .as_ref()
                    .ok_or_else(|| Error::Storage("missing tool invocation".into()))?;
                run.state.accept_tool(&call.id, result.clone())?;
                let entry = run
                    .tools
                    .iter_mut()
                    .find(|t| t.execution_id == execution_id)
                    .ok_or_else(|| Error::Storage("missing tool ledger entry".into()))?;
                entry.status = "completed".into();
                entry.result = Some(result);
            }
            _ => {
                return Err(Error::Input(
                    "operation result type does not match claim".into(),
                ));
            }
        }
        run.in_flight = None;
        let mut frame = RunFrame::new(revision, run);
        frame.revision =
            self.write_record_with_extra(agent, session, revision, record, objective_writes)?;
        Ok(frame)
    }

    pub fn inspect(&self, agent: &str, session: &str, run_id: Option<&str>) -> Result<Value> {
        if let Some(run_id) = run_id {
            self.key(agent, session)?;
            valid_id(run_id)?;
            let key = serde_json::to_string(&(&self.tenant_id, agent, session, run_id))?;
            return Ok(self
                .db
                .col_get(RUNS, &key)
                .map_err(storage)?
                .map(|d| d.doc)
                .unwrap_or(Value::Null));
        }
        let Some((revision, record)) = self.read(agent, session)? else {
            return Ok(Value::Null);
        };
        Ok(json!({"revision":revision,"status":record.status,"run":record.run}))
    }
}
