//! Optional durable job scheduling. Queue, job and root-budget writes are atomic.
//! No job execution, retry, or reconciliation happens merely by opening a queue.
use crate::{Error, Result, sessions::SessionStore};
use hivedb_core::{ColOp, ScanOptions};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

const QUEUES: &str = "hive_sdk_rust_queues_v1";
const JOBS: &str = "hive_sdk_rust_jobs_v1";
const KEYS: &str = "hive_sdk_rust_job_keys_v1";
const EVENTS: &str = "hive_sdk_rust_events_v1";
const PORTABLE: u64 = 9_007_199_254_740_991;

fn invalid(message: &str) -> Error {
    Error::Input(message.into())
}
fn state(message: &str) -> Error {
    Error::State(message.into())
}
fn storage(error: impl std::fmt::Display) -> Error {
    Error::Storage(error.to_string())
}
fn valid_id(id: &str) -> Result<()> {
    if id.trim().is_empty() || id.len() > 512 || id.chars().any(char::is_control) {
        return Err(invalid(
            "job identifiers require 1..512 bytes without control characters",
        ));
    }
    Ok(())
}
fn default_depth() -> u32 {
    1
}
fn default_attempts() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct QueuePolicy {
    pub max_concurrency: u32,
    pub lease_ms: u64,
    pub retry_delay_ms: u64,
    pub task_timeout_ms: Option<u64>,
    /// Total retained jobs, including terminal audit records. No implicit purge.
    pub max_jobs: u32,
    pub max_payload_bytes: usize,
}
impl Default for QueuePolicy {
    fn default() -> Self {
        Self {
            max_concurrency: 4,
            lease_ms: 30_000,
            retry_delay_ms: 1000,
            task_timeout_ms: Some(1_800_000),
            max_jobs: 10_000,
            max_payload_bytes: 65_536,
        }
    }
}
impl QueuePolicy {
    fn validate(&self) -> Result<()> {
        if !(1..=256).contains(&self.max_concurrency)
            || !(100..=3_600_000).contains(&self.lease_ms)
            || self.retry_delay_ms > 86_400_000
            || self
                .task_timeout_ms
                .is_some_and(|n| n == 0 || n > 86_400_000)
            || !(1..=10_000).contains(&self.max_jobs)
            || !(1..=1_048_576).contains(&self.max_payload_bytes)
        {
            return Err(invalid("invalid durable queue policy"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct JobInput {
    pub kind: String,
    pub lane: String,
    pub payload: Value,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub not_before: Option<u64>,
    #[serde(default = "default_attempts")]
    pub max_attempts: u32,
    #[serde(default)]
    pub retry_safe: bool,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub objective_id: Option<String>,
    #[serde(default = "default_depth")]
    pub depth: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Pending,
    Running,
    Cancelling,
    Uncertain,
    Completed,
    Failed,
    Cancelled,
}
impl JobStatus {
    fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
    fn occupies_lane(self) -> bool {
        matches!(self, Self::Running | Self::Cancelling | Self::Uncertain)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub id: String,
    pub revision: u64,
    pub input: JobInput,
    pub status: JobStatus,
    pub sequence: u64,
    pub attempts: u32,
    pub not_before: u64,
    pub created_at: u64,
    pub started_at: Option<u64>,
    pub attempt_started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub execution_id: Option<String>,
    pub worker_id: Option<String>,
    pub lease_expires_at: Option<u64>,
    pub cancellation_requested: bool,
    pub result: Option<Value>,
    pub error: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredJob {
    version: u32,
    tenant: String,
    queue: String,
    job: Job,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Slot {
    kind: String,
    lane: String,
    priority: i32,
    sequence: u64,
    not_before: u64,
    status: JobStatus,
    lease_expires_at: Option<u64>,
}
impl From<&Job> for Slot {
    fn from(job: &Job) -> Self {
        Self {
            kind: job.input.kind.clone(),
            lane: job.input.lane.clone(),
            priority: job.input.priority,
            sequence: job.sequence,
            not_before: job.not_before,
            status: job.status,
            lease_expires_at: job.lease_expires_at,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Meta {
    version: u32,
    tenant: String,
    id: String,
    policy: QueuePolicy,
    sequence: u64,
    /// Last event sequence written; events share the transaction of the change they describe.
    #[serde(default)]
    events: u64,
    /// Terminal jobs removed by `purge_jobs`; they no longer count against `max_jobs`.
    #[serde(default)]
    purged: u64,
    slots: BTreeMap<String, Slot>,
}
struct Change {
    writes: Vec<ColOp>,
    result: Result<Value>,
}
impl Change {
    fn value(value: impl Serialize) -> Result<Self> {
        Ok(Self {
            writes: vec![],
            result: Ok(serde_json::to_value(value)?),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum JobOutcome {
    Completed {
        result: Value,
    },
    Failed {
        error: String,
        #[serde(default)]
        retryable: bool,
    },
    Cancelled,
    Uncertain {
        error: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Resolution {
    Completed { result: Value },
    Failed { error: String },
    Cancelled,
    Retry,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    #[cfg(feature = "scheduler")]
    ScheduleCreate {
        schedule_id: String,
        spec: ScheduleSpec,
    },
    #[cfg(feature = "scheduler")]
    ScheduleGet {
        schedule_id: String,
    },
    #[cfg(feature = "scheduler")]
    ScheduleEnable {
        schedule_id: String,
        enabled: bool,
    },
    #[cfg(feature = "scheduler")]
    ScheduleTick {
        schedule_id: String,
        observed: Option<u64>,
    },
    #[cfg(feature = "scheduler")]
    ScheduleList {
        #[serde(default = "list_limit")]
        limit: usize,
        cursor: Option<String>,
    },
    #[cfg(feature = "scheduler")]
    ScheduleNext {
        schedule: crate::scheduler::Schedule,
        after: u64,
    },
    Open {
        #[serde(default)]
        policy: QueuePolicy,
    },
    Inspect,
    Enqueue {
        input: JobInput,
    },
    Get {
        job_id: String,
    },
    List {
        #[serde(default = "list_limit")]
        limit: usize,
        cursor: Option<String>,
    },
    Claim {
        worker_id: String,
        kinds: Vec<String>,
    },
    Heartbeat {
        job_id: String,
        execution_id: String,
    },
    Finish {
        job_id: String,
        execution_id: String,
        outcome: JobOutcome,
    },
    Cancel {
        job_id: String,
    },
    CancelExecution {
        job_id: String,
        execution_id: String,
    },
    Maintain,
    GroupCreate {
        group_id: String,
        spec: GroupSpec,
    },
    GroupGet {
        group_id: String,
    },
    GroupSynthesize {
        group_id: String,
    },
    GroupCancel {
        group_id: String,
    },
    JobsList {
        #[serde(default)]
        filter: admin::JobFilter,
        #[serde(default = "list_limit")]
        limit: usize,
        #[serde(default)]
        cursor: Option<String>,
    },
    JobRetry {
        job_id: String,
        #[serde(default)]
        force: bool,
    },
    JobsCancelWhere {
        filter: admin::JobFilter,
    },
    Events {
        #[serde(default)]
        after: u64,
        #[serde(default = "list_limit")]
        limit: usize,
    },
    JobsPurge {
        filter: admin::JobFilter,
        finished_before: u64,
        #[serde(default = "list_limit")]
        limit: usize,
    },
    EventsPrune {
        up_to: u64,
    },
    GoalCreate {
        goal_id: String,
        spec: GoalSpec,
    },
    GoalGet {
        goal_id: String,
    },
    GoalTick {
        goal_id: String,
    },
    GoalCancel {
        goal_id: String,
    },
    GroupVerificationInput {
        group_id: String,
        task_id: String,
    },
    GroupRecordProof {
        group_id: String,
        task_id: String,
        verification_key: String,
    },
    Reconcile {
        job_id: String,
        execution_id: String,
        resolution: Resolution,
    },
}
fn list_limit() -> usize {
    50
}

#[derive(Clone)]
pub struct DurableQueue {
    store: SessionStore,
    id: String,
    clock: Arc<dyn Fn() -> Result<u64> + Send + Sync>,
}
impl SessionStore {
    pub fn queue(&self, id: &str) -> Result<DurableQueue> {
        valid_id(id)?;
        Ok(DurableQueue {
            store: self.clone(),
            id: id.into(),
            clock: Arc::new(crate::budget::now_ms),
        })
    }
}
impl DurableQueue {
    fn key(&self) -> String {
        json!([self.store.tenant_id, self.id]).to_string()
    }
    fn job_key(&self, id: &str) -> Result<String> {
        valid_id(id)?;
        Ok(json!([self.store.tenant_id, self.id, id]).to_string())
    }
    fn prefix(&self) -> String {
        let key = self.key();
        format!("{},", &key[..key.len() - 1])
    }
    fn read_meta(&self) -> Result<Option<(u64, Meta)>> {
        let Some(row) = self
            .store
            .db
            .col_get(QUEUES, &self.key())
            .map_err(storage)?
        else {
            return Ok(None);
        };
        let meta: Meta = serde_json::from_value(row.doc)?;
        if meta.version != 1 || meta.tenant != self.store.tenant_id || meta.id != self.id {
            return Err(state("QUEUE_SCOPE_MISMATCH"));
        }
        meta.policy.validate()?;
        Ok(Some((row.version, meta)))
    }
    pub fn get(&self, id: &str) -> Result<Option<Job>> {
        let Some(row) = self
            .store
            .db
            .col_get(JOBS, &self.job_key(id)?)
            .map_err(storage)?
        else {
            return Ok(None);
        };
        let record: StoredJob = serde_json::from_value(row.doc)?;
        if record.version != 1
            || record.tenant != self.store.tenant_id
            || record.queue != self.id
            || record.job.id != id
            || record.job.revision != row.version
        {
            return Err(state("JOB_SCOPE_MISMATCH"));
        }
        Ok(Some(record.job))
    }
    fn save(&self, meta: &mut Meta, job: &mut Job, writes: &mut Vec<ColOp>) -> Result<()> {
        let previous = job.revision;
        // Every status change is an event in the same atomic batch; heartbeats are not.
        let before = if previous == 0 {
            None
        } else {
            meta.slots.get(&job.id).map(|slot| slot.status)
        };
        if before != Some(job.status) {
            let subject = json!({"job_id":job.id,"kind":job.input.kind,"lane":job.input.lane,"status":job.status,
                "objective_id":job.input.objective_id,"attempts":job.attempts});
            self.emit(meta, writes, "job.status", subject)?;
        }
        job.revision = previous
            .checked_add(1)
            .filter(|v| *v <= PORTABLE)
            .ok_or_else(|| state("JOB_REVISION_EXHAUSTED"))?;
        if job.status.terminal() {
            meta.slots.remove(&job.id);
        } else {
            meta.slots.insert(job.id.clone(), Slot::from(&*job));
        }
        let record = StoredJob {
            version: 1,
            tenant: self.store.tenant_id.clone(),
            queue: self.id.clone(),
            job: job.clone(),
        };
        writes.push(ColOp::Put {
            collection: JOBS.into(),
            id: self.job_key(&job.id)?,
            doc: serde_json::to_value(record)?,
            expected_version: Some(previous),
        });
        Ok(())
    }
    fn transact(&self, work: impl Fn(&mut Meta, u64) -> Result<Change>) -> Result<Value> {
        let _guard = self
            .store
            .objective_lock
            .lock()
            .map_err(|_| state("poisoned queue/objective transaction guard"))?;
        for _ in 0..32 {
            let (revision, mut meta) = self.read_meta()?.ok_or_else(|| state("QUEUE_NOT_OPEN"))?;
            let now = (self.clock)()?;
            if now > PORTABLE {
                return Err(invalid("queue clock exceeds portable range"));
            }
            let mut change = work(&mut meta, now)?;
            if change.writes.is_empty() {
                return change.result;
            }
            change.writes.push(ColOp::Put {
                collection: QUEUES.into(),
                id: self.key(),
                doc: serde_json::to_value(meta)?,
                expected_version: Some(revision),
            });
            match self.store.db.col_batch(&change.writes) {
                Ok(_) => return change.result,
                Err(error) if error.to_string().contains("version conflict on") => continue,
                Err(error) => return Err(storage(error)),
            }
        }
        Err(state("QUEUE_CONTENTION"))
    }
    pub fn open(&self, policy: QueuePolicy) -> Result<Value> {
        policy.validate()?;
        for _ in 0..32 {
            if let Some((_, meta)) = self.read_meta()? {
                if meta.policy != policy {
                    return Err(state("QUEUE_POLICY_MISMATCH"));
                }
                return Ok(json!({"id":self.id,"policy":policy}));
            }
            let meta = Meta {
                version: 1,
                tenant: self.store.tenant_id.clone(),
                id: self.id.clone(),
                policy: policy.clone(),
                sequence: 0,
                events: 0,
                purged: 0,
                slots: BTreeMap::new(),
            };
            match self.store.db.col_batch(&[ColOp::Put {
                collection: QUEUES.into(),
                id: self.key(),
                doc: serde_json::to_value(meta)?,
                expected_version: Some(0),
            }]) {
                Ok(_) => return Ok(json!({"id":self.id,"policy":policy})),
                Err(error) if error.to_string().contains("version conflict on") => continue,
                Err(error) => return Err(storage(error)),
            }
        }
        Err(state("QUEUE_CONTENTION"))
    }
    fn check_input(&self, input: &JobInput, policy: &QueuePolicy) -> Result<()> {
        valid_id(&input.kind)?;
        valid_id(&input.lane)?;
        if let Some(key) = &input.idempotency_key {
            valid_id(key)?;
        }
        if let Some(id) = &input.objective_id {
            valid_id(id)?;
        }
        if !input.payload.is_object()
            || serde_json::to_vec(&input.payload)?.len() > policy.max_payload_bytes
            || !(1..=100).contains(&input.max_attempts)
            || input.not_before.is_some_and(|n| n > PORTABLE)
            || input.depth > 256
        {
            return Err(invalid("invalid durable job payload, schedule or limits"));
        }
        Ok(())
    }
    pub fn enqueue(&self, input: JobInput) -> Result<Job> {
        let value = self.transact(|meta, now| self.prepare_enqueue(meta, &input, now))?;
        Ok(serde_json::from_value(value)?)
    }
    fn prepare_enqueue(&self, meta: &mut Meta, input: &JobInput, now: u64) -> Result<Change> {
        self.check_input(input, &meta.policy)?;
        let key = input
            .idempotency_key
            .as_ref()
            .map(|key| json!([self.store.tenant_id, self.id, key]).to_string());
        if let Some(key) = &key
            && let Some(row) = self.store.db.col_get(KEYS, key).map_err(storage)?
        {
            let id = row.doc["job_id"]
                .as_str()
                .ok_or_else(|| state("invalid job idempotency record"))?;
            let job = self
                .get(id)?
                .ok_or_else(|| state("missing idempotent job"))?;
            if job.input != *input {
                return Err(state("JOB_IDEMPOTENCY_CONFLICT"));
            }
            return Change::value(job);
        }
        if meta.sequence - meta.purged >= u64::from(meta.policy.max_jobs) {
            return Err(state("QUEUE_RETENTION_LIMIT"));
        }
        if let Some(id) = &input.objective_id {
            self.store
                .prepare_objective(id, &json!({"op":"check","depth":input.depth}).to_string())?
                .result?;
        }
        meta.sequence += 1;
        let mut job = Job {
            id: format!("{:020}", meta.sequence),
            revision: 0,
            input: input.clone(),
            status: JobStatus::Pending,
            sequence: meta.sequence,
            attempts: 0,
            not_before: input.not_before.unwrap_or(now),
            created_at: now,
            started_at: None,
            attempt_started_at: None,
            finished_at: None,
            execution_id: None,
            worker_id: None,
            lease_expires_at: None,
            cancellation_requested: false,
            result: None,
            error: None,
        };
        let mut writes = vec![];
        self.save(meta, &mut job, &mut writes)?;
        if let Some(key) = &key {
            writes.push(ColOp::Put {
                collection: KEYS.into(),
                id: key.clone(),
                doc: json!({"job_id":job.id}),
                expected_version: Some(0),
            });
        }
        Ok(Change {
            writes,
            result: Ok(serde_json::to_value(job)?),
        })
    }
    pub fn claim(&self, worker_id: &str, kinds: &[String]) -> Result<Option<Job>> {
        valid_id(worker_id)?;
        if kinds.len() > 256 {
            return Err(invalid("at most 256 executor kinds are allowed"));
        }
        for kind in kinds {
            valid_id(kind)?;
        }
        let value = self.transact(|meta, now| {
            let occupied: Vec<_> = meta
                .slots
                .values()
                .filter(|s| s.status.occupies_lane())
                .collect();
            if occupied.len() >= meta.policy.max_concurrency as usize {
                return Change::value(None::<Job>);
            }
            let lanes: BTreeSet<_> = occupied.iter().map(|s| s.lane.as_str()).collect();
            let selected = meta
                .slots
                .iter()
                .filter(|(_, s)| {
                    s.status == JobStatus::Pending
                        && s.not_before <= now
                        && kinds.contains(&s.kind)
                        && !lanes.contains(s.lane.as_str())
                })
                .min_by(|(_, a), (_, b)| {
                    b.priority
                        .cmp(&a.priority)
                        .then(a.sequence.cmp(&b.sequence))
                })
                .map(|(id, _)| id.clone());
            let Some(id) = selected else {
                return Change::value(None::<Job>);
            };
            let mut job = self
                .get(&id)?
                .ok_or_else(|| state("missing scheduled job"))?;
            let mut writes = vec![];
            if let Some(root) = &job.input.objective_id {
                let prepared = self.store.prepare_objective(
                    root,
                    &json!({"op":"tool","depth":job.input.depth}).to_string(),
                )?;
                if let Some(write) = prepared.write {
                    writes.push(write);
                }
                if let Err(error) = prepared.result {
                    job.status = JobStatus::Failed;
                    job.error = Some(error.to_string());
                    job.finished_at = Some(now);
                    self.save(meta, &mut job, &mut writes)?;
                    return Ok(Change {
                        writes,
                        result: Err(error),
                    });
                }
            }
            job.status = JobStatus::Running;
            job.attempts += 1;
            job.worker_id = Some(worker_id.into());
            job.execution_id = Some(uuid::Uuid::new_v4().to_string());
            job.lease_expires_at = Some(
                now.checked_add(meta.policy.lease_ms)
                    .filter(|n| *n <= PORTABLE)
                    .ok_or_else(|| invalid("lease exceeds portable range"))?,
            );
            job.started_at.get_or_insert(now);
            job.attempt_started_at = Some(now);
            job.error = None;
            self.save(meta, &mut job, &mut writes)?;
            Ok(Change {
                writes,
                result: Ok(serde_json::to_value(Some(job))?),
            })
        })?;
        Ok(serde_json::from_value(value)?)
    }
    fn leased(&self, id: &str, execution_id: &str, now: u64) -> Result<Job> {
        valid_id(execution_id)?;
        let job = self.get(id)?.ok_or_else(|| state("JOB_NOT_FOUND"))?;
        if !matches!(job.status, JobStatus::Running | JobStatus::Cancelling)
            || job.execution_id.as_deref() != Some(execution_id)
        {
            return Err(state("JOB_FENCE_MISMATCH"));
        }
        if job.lease_expires_at.is_none_or(|deadline| deadline <= now) {
            return Err(state("JOB_LEASE_EXPIRED"));
        }
        Ok(job)
    }
    fn check_running_limits(
        &self,
        meta: &Meta,
        job: &mut Job,
        now: u64,
        writes: &mut Vec<ColOp>,
    ) -> Result<()> {
        if meta.policy.task_timeout_ms.is_some_and(|timeout| {
            job.attempt_started_at
                .is_some_and(|started| now.saturating_sub(started) >= timeout)
        }) {
            job.status = JobStatus::Cancelling;
            job.cancellation_requested = true;
            job.error = Some("job task deadline exceeded".into());
        }
        if let Some(root) = &job.input.objective_id {
            let prepared = self.store.prepare_objective(
                root,
                &json!({"op":"check","depth":job.input.depth}).to_string(),
            )?;
            if let Some(write) = prepared.write {
                writes.push(write);
            }
            if let Err(error) = prepared.result {
                job.status = JobStatus::Cancelling;
                job.cancellation_requested = true;
                job.error = Some(error.to_string());
            }
        }
        Ok(())
    }
    pub fn heartbeat(&self, id: &str, execution_id: &str) -> Result<Job> {
        let value = self.transact(|meta, now| {
            let mut job = self.leased(id, execution_id, now)?;
            let mut writes = vec![];
            self.check_running_limits(meta, &mut job, now, &mut writes)?;
            job.lease_expires_at = Some(
                now.checked_add(meta.policy.lease_ms)
                    .filter(|n| *n <= PORTABLE)
                    .ok_or_else(|| invalid("lease exceeds portable range"))?,
            );
            self.save(meta, &mut job, &mut writes)?;
            Ok(Change {
                writes,
                result: Ok(serde_json::to_value(job)?),
            })
        })?;
        Ok(serde_json::from_value(value)?)
    }
    fn release(job: &mut Job, now: u64) {
        job.worker_id = None;
        job.lease_expires_at = None;
        if job.status.terminal() {
            job.finished_at = Some(now);
        }
    }
    fn retry(job: &mut Job, policy: &QueuePolicy, now: u64) -> Result<()> {
        if !job.input.retry_safe || job.attempts >= job.input.max_attempts {
            return Err(state("JOB_RETRY_NOT_ALLOWED"));
        }
        let factor = 1u64 << job.attempts.saturating_sub(1).min(16);
        let delay = policy.retry_delay_ms.saturating_mul(factor).min(86_400_000);
        job.status = JobStatus::Pending;
        job.worker_id = None;
        job.lease_expires_at = None;
        job.execution_id = None;
        job.not_before = now
            .checked_add(delay)
            .filter(|n| *n <= PORTABLE)
            .ok_or_else(|| invalid("retry schedule exceeds portable range"))?;
        Ok(())
    }
    fn valid_result(value: &Value, policy: &QueuePolicy) -> Result<()> {
        if serde_json::to_vec(value)?.len() > policy.max_payload_bytes {
            return Err(invalid("job result exceeds configured byte limit"));
        }
        Ok(())
    }
    fn valid_error(error: &str) -> Result<()> {
        if error.len() > 4096 {
            return Err(invalid("job error exceeds 4096 bytes"));
        }
        Ok(())
    }
    pub fn finish(&self, id: &str, execution_id: &str, outcome: JobOutcome) -> Result<Job> {
        let value = self.transact(|meta, now| {
            let mut job = self.leased(id, execution_id, now)?;
            let mut writes = vec![];
            self.check_running_limits(meta, &mut job, now, &mut writes)?;
            match &outcome {
                JobOutcome::Uncertain { error } => {
                    Self::valid_error(error)?;
                    job.status = JobStatus::Uncertain;
                    job.error = Some(error.clone());
                }
                JobOutcome::Completed { result } => {
                    Self::valid_result(result, &meta.policy)?;
                    if job.cancellation_requested {
                        job.status = JobStatus::Cancelled;
                    } else {
                        job.status = JobStatus::Completed;
                        job.result = Some(result.clone());
                    }
                }
                JobOutcome::Cancelled => job.status = JobStatus::Cancelled,
                JobOutcome::Failed { error, retryable } => {
                    Self::valid_error(error)?;
                    job.error = Some(error.clone());
                    if job.cancellation_requested {
                        job.status = JobStatus::Cancelled;
                    } else if *retryable
                        && job.input.retry_safe
                        && job.attempts < job.input.max_attempts
                    {
                        Self::retry(&mut job, &meta.policy, now)?;
                    } else {
                        job.status = JobStatus::Failed;
                    }
                }
            }
            if job.status != JobStatus::Uncertain {
                Self::release(&mut job, now);
            }
            self.save(meta, &mut job, &mut writes)?;
            Ok(Change {
                writes,
                result: Ok(serde_json::to_value(job)?),
            })
        })?;
        Ok(serde_json::from_value(value)?)
    }
    pub fn cancel(&self, id: &str) -> Result<Job> {
        self.cancel_inner(id, None)
    }
    pub fn cancel_execution(&self, id: &str, execution_id: &str) -> Result<Job> {
        self.cancel_inner(id, Some(execution_id))
    }
    fn cancel_inner(&self, id: &str, execution_id: Option<&str>) -> Result<Job> {
        let value = self.transact(|meta, now| {
            let mut job = match execution_id {
                Some(token) => {
                    valid_id(token)?;
                    let job = self.get(id)?.ok_or_else(|| state("JOB_NOT_FOUND"))?;
                    // Record cancellation even after this owner's lease expired,
                    // so maintenance cannot replay safe work the caller stopped.
                    // A reclaimed/new attempt always has a different fence.
                    if !job.status.occupies_lane() || job.execution_id.as_deref() != Some(token) {
                        return Err(state("JOB_FENCE_MISMATCH"));
                    }
                    job
                }
                None => self.get(id)?.ok_or_else(|| state("JOB_NOT_FOUND"))?,
            };
            if job.status.terminal() || job.cancellation_requested {
                return Change::value(job);
            }
            job.cancellation_requested = true;
            if job.status == JobStatus::Pending {
                job.status = JobStatus::Cancelled;
                Self::release(&mut job, now);
            } else if job.status == JobStatus::Running {
                job.status = JobStatus::Cancelling;
            }
            let mut writes = vec![];
            self.save(meta, &mut job, &mut writes)?;
            Ok(Change {
                writes,
                result: Ok(serde_json::to_value(job)?),
            })
        })?;
        Ok(serde_json::from_value(value)?)
    }
    pub fn maintain(&self) -> Result<Vec<Job>> {
        let value = self.transact(|meta, now| {
            let expired: Vec<_> = meta
                .slots
                .iter()
                .filter(|(_, s)| {
                    matches!(s.status, JobStatus::Running | JobStatus::Cancelling)
                        && s.lease_expires_at.is_some_and(|n| n <= now)
                })
                .map(|(id, _)| id.clone())
                .collect();
            let mut jobs = vec![];
            let mut writes = vec![];
            for id in expired {
                let mut job = self.get(&id)?.ok_or_else(|| state("missing expired job"))?;
                job.error = Some("lease expired before an acknowledged outcome".into());
                if job.input.retry_safe
                    && !job.cancellation_requested
                    && job.attempts < job.input.max_attempts
                {
                    Self::retry(&mut job, &meta.policy, now)?;
                } else {
                    job.status = JobStatus::Uncertain;
                }
                self.save(meta, &mut job, &mut writes)?;
                jobs.push(job);
            }
            Ok(Change {
                writes,
                result: Ok(serde_json::to_value(jobs)?),
            })
        })?;
        Ok(serde_json::from_value(value)?)
    }
    pub fn reconcile(&self, id: &str, execution_id: &str, resolution: Resolution) -> Result<Job> {
        let value = self.transact(|meta, now| {
            let mut job = self.get(id)?.ok_or_else(|| state("JOB_NOT_FOUND"))?;
            if job.status != JobStatus::Uncertain
                || job.execution_id.as_deref() != Some(execution_id)
            {
                return Err(state("JOB_FENCE_MISMATCH"));
            }
            match &resolution {
                Resolution::Completed { result } => {
                    Self::valid_result(result, &meta.policy)?;
                    job.status = JobStatus::Completed;
                    job.result = Some(result.clone());
                }
                Resolution::Failed { error } => {
                    Self::valid_error(error)?;
                    job.status = JobStatus::Failed;
                    job.error = Some(error.clone());
                }
                Resolution::Cancelled => job.status = JobStatus::Cancelled,
                Resolution::Retry => {
                    if job.cancellation_requested {
                        return Err(state("JOB_CANCELLATION_REQUESTED"));
                    }
                    Self::retry(&mut job, &meta.policy, now)?;
                }
            }
            Self::release(&mut job, now);
            let mut writes = vec![];
            self.save(meta, &mut job, &mut writes)?;
            Ok(Change {
                writes,
                result: Ok(serde_json::to_value(job)?),
            })
        })?;
        Ok(serde_json::from_value(value)?)
    }
    pub fn list(&self, limit: usize, cursor: Option<&str>) -> Result<Value> {
        if !(1..=200).contains(&limit) {
            return Err(invalid("job list limit must be 1..200"));
        }
        let prefix = self.prefix();
        if cursor.is_some_and(|cursor| !cursor.starts_with(&prefix)) {
            return Err(invalid("job cursor belongs to a different queue or tenant"));
        }
        let rows = self
            .store
            .db
            .col_scan(
                JOBS,
                &ScanOptions {
                    prefix: Some(prefix),
                    start: cursor.map(str::to_owned),
                    limit: limit + 2,
                    ..Default::default()
                },
            )
            .map_err(storage)?;
        let mut items = vec![];
        let mut last = None;
        for row in rows {
            if cursor == Some(row.id.as_str()) {
                continue;
            }
            if items.len() == limit {
                return Ok(json!({"items":items,"next_cursor":last}));
            }
            let stored: StoredJob = serde_json::from_value(row.doc)?;
            if stored.tenant != self.store.tenant_id
                || stored.queue != self.id
                || stored.version != 1
                || stored.job.revision != row.version
            {
                return Err(state("JOB_SCOPE_MISMATCH"));
            }
            items.push(stored.job);
            last = Some(row.id);
        }
        Ok(json!({"items":items,"next_cursor":null}))
    }
    pub fn inspect(&self) -> Result<Value> {
        let (_, meta) = self.read_meta()?.ok_or_else(|| state("QUEUE_NOT_OPEN"))?;
        let mut counts = BTreeMap::<String, usize>::new();
        for slot in meta.slots.values() {
            *counts
                .entry(serde_json::to_value(slot.status)?.as_str().unwrap().into())
                .or_default() += 1;
        }
        Ok(
            json!({"id":self.id,"policy":meta.policy,"retained_jobs":meta.sequence - meta.purged,"active_counts":counts}),
        )
    }
    pub fn dispatch(&self, command: Value) -> Result<Value> {
        Ok(match serde_json::from_value::<Command>(command)? {
            #[cfg(feature = "scheduler")]
            Command::ScheduleCreate { schedule_id, spec } => {
                self.create_schedule(&schedule_id, spec)?
            }
            #[cfg(feature = "scheduler")]
            Command::ScheduleGet { schedule_id } => {
                serde_json::to_value(self.get_schedule(&schedule_id)?)?
            }
            #[cfg(feature = "scheduler")]
            Command::ScheduleEnable {
                schedule_id,
                enabled,
            } => self.enable_schedule(&schedule_id, enabled)?,
            #[cfg(feature = "scheduler")]
            Command::ScheduleTick {
                schedule_id,
                observed,
            } => self.tick_schedule(&schedule_id, observed)?,
            #[cfg(feature = "scheduler")]
            Command::ScheduleList { limit, cursor } => {
                self.list_schedules(limit, cursor.as_deref())?
            }
            #[cfg(feature = "scheduler")]
            Command::ScheduleNext { schedule, after } => {
                serde_json::to_value(schedule.next_after(after)?)?
            }
            Command::GroupCreate { group_id, spec } => self.create_group(&group_id, spec)?,
            Command::GroupGet { group_id } => self.group(&group_id)?,
            Command::GroupSynthesize { group_id } => {
                serde_json::to_value(self.synthesize_group(&group_id)?)?
            }
            Command::GroupCancel { group_id } => self.cancel_group(&group_id)?,
            Command::JobsList {
                filter,
                limit,
                cursor,
            } => self.list_where(&filter, limit, cursor.as_deref())?,
            Command::JobRetry { job_id, force } => {
                serde_json::to_value(self.retry_failed(&job_id, force)?)?
            }
            Command::JobsCancelWhere { filter } => self.cancel_where(&filter)?,
            Command::Events { after, limit } => self.events(after, limit)?,
            Command::JobsPurge {
                filter,
                finished_before,
                limit,
            } => self.purge_jobs(&filter, finished_before, limit)?,
            Command::EventsPrune { up_to } => self.prune_events(up_to)?,
            Command::GoalCreate { goal_id, spec } => self.create_goal(&goal_id, spec)?,
            Command::GoalGet { goal_id } => self.goal(&goal_id)?,
            Command::GoalTick { goal_id } => self.goal_tick(&goal_id)?,
            Command::GoalCancel { goal_id } => self.cancel_goal(&goal_id)?,
            Command::GroupVerificationInput { group_id, task_id } => {
                self.group_verification_input(&group_id, &task_id)?
            }
            Command::GroupRecordProof {
                group_id,
                task_id,
                verification_key,
            } => self.record_group_proof(&group_id, &task_id, &verification_key)?,
            Command::Open { policy } => self.open(policy)?,
            Command::Inspect => self.inspect()?,
            Command::Enqueue { input } => serde_json::to_value(self.enqueue(input)?)?,
            Command::Get { job_id } => serde_json::to_value(self.get(&job_id)?)?,
            Command::List { limit, cursor } => self.list(limit, cursor.as_deref())?,
            Command::Claim { worker_id, kinds } => {
                serde_json::to_value(self.claim(&worker_id, &kinds)?)?
            }
            Command::Heartbeat {
                job_id,
                execution_id,
            } => serde_json::to_value(self.heartbeat(&job_id, &execution_id)?)?,
            Command::Finish {
                job_id,
                execution_id,
                outcome,
            } => serde_json::to_value(self.finish(&job_id, &execution_id, outcome)?)?,
            Command::Cancel { job_id } => serde_json::to_value(self.cancel(&job_id)?)?,
            Command::Maintain => serde_json::to_value(self.maintain()?)?,
            Command::CancelExecution {
                job_id,
                execution_id,
            } => serde_json::to_value(self.cancel_execution(&job_id, &execution_id)?)?,
            Command::Reconcile {
                job_id,
                execution_id,
                resolution,
            } => serde_json::to_value(self.reconcile(&job_id, &execution_id, resolution)?)?,
        })
    }
}

#[cfg(test)]
mod tests;
mod worker;
pub use worker::{JobExecutor, WorkerPool};

mod groups;
pub use groups::{GroupSpec, SpecialistTask};
mod admin;
#[cfg(test)]
mod admin_tests;
#[cfg(test)]
mod goal_tests;
mod goals;
pub use admin::JobFilter;
pub use goals::GoalSpec;

#[cfg(feature = "scheduler")]
mod schedules;
#[cfg(feature = "scheduler")]
pub use schedules::{MisfirePolicy, ScheduleSpec};
#[cfg(all(test, feature = "scheduler"))]
mod schedule_tests;
