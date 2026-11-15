//! Native dispatch loop. Hosts supply executors; Rust owns claims and renewal.
use super::{DurableQueue, Job, JobOutcome, QueuePolicy, state, valid_id};
use crate::{Error, Result, provider::CancellationToken};
use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::task::JoinSet;

type Execution = Pin<Box<dyn Future<Output = Result<JobOutcome>> + Send + 'static>>;
pub trait JobExecutor: Send + Sync + 'static {
    fn execute(&self, job: Job, cancel: CancellationToken) -> Execution;
}
impl<F, Fut> JobExecutor for F
where
    F: Fn(Job, CancellationToken) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<JobOutcome>> + Send + 'static,
{
    fn execute(&self, job: Job, cancel: CancellationToken) -> Execution {
        Box::pin(self(job, cancel))
    }
}

pub struct WorkerPool {
    queue: DurableQueue,
    worker_id: String,
    executors: BTreeMap<String, Arc<dyn JobExecutor>>,
    stop: CancellationToken,
}
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| state("QUEUE_WORKER_PANICKED"))?
}
impl WorkerPool {
    pub fn new(queue: DurableQueue, worker_id: String) -> Result<Self> {
        valid_id(&worker_id)?;
        Ok(Self {
            queue,
            worker_id,
            executors: BTreeMap::new(),
            stop: CancellationToken::new(),
        })
    }
    pub fn register<E: JobExecutor>(&mut self, kind: &str, executor: E) -> Result<()> {
        valid_id(kind)?;
        if self.executors.contains_key(kind) || self.executors.len() >= 256 {
            return Err(state("duplicate executor kind or registry limit exceeded"));
        }
        self.executors.insert(kind.into(), Arc::new(executor));
        Ok(())
    }
    /// Stops new claims and requests cooperative cancellation of active work.
    /// run/run_once must still be awaited so callbacks and storage writes join.
    pub fn stop(&self) {
        self.stop.cancel();
    }
    pub async fn run_once(&self) -> Result<Vec<Job>> {
        if self.executors.is_empty() {
            return Err(state("queue has no registered executors"));
        }
        if self.stop.is_cancelled() {
            return Ok(vec![]);
        }
        let queue = self.queue.clone();
        let policy = blocking(move || {
            queue.maintain()?;
            Ok(queue
                .read_meta()?
                .ok_or_else(|| state("QUEUE_NOT_OPEN"))?
                .1
                .policy)
        })
        .await?;
        let mut tasks = JoinSet::new();
        let mut error = None;
        let mut completed = vec![];
        for _ in 0..policy.max_concurrency {
            if self.stop.is_cancelled() {
                break;
            }
            let queue = self.queue.clone();
            let worker = self.worker_id.clone();
            let kinds = self.executors.keys().cloned().collect::<Vec<_>>();
            let next = blocking(move || queue.claim(&worker, &kinds)).await;
            match next {
                Ok(Some(job)) => {
                    let executor = self.executors[&job.input.kind].clone();
                    tasks.spawn(execute(
                        self.queue.clone(),
                        job,
                        executor,
                        self.stop.child_token(),
                        policy.clone(),
                    ));
                }
                Ok(None) => break,
                Err(err) => {
                    error = Some(err);
                    break;
                }
            }
        }
        // Never detach already-dispatched callbacks when a later claim fails.
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(job)) => completed.push(job),
                Ok(Err(err)) => {
                    error.get_or_insert(err);
                }
                Err(_) => {
                    error.get_or_insert_with(|| state("QUEUE_EXECUTOR_PANICKED"));
                }
            }
        }
        if let Some(error) = error {
            return Err(error);
        }
        completed.sort_by_key(|job| job.sequence);
        Ok(completed)
    }
    /// Explicit polling, no inference or execution when no eligible jobs exist.
    pub async fn run(&self, poll_ms: u64) -> Result<()> {
        if !(10..=60_000).contains(&poll_ms) {
            return Err(super::invalid("poll interval must be 10..60000 ms"));
        }
        loop {
            if self.stop.is_cancelled() {
                return Ok(());
            }
            match self.run_once().await {
                Ok(_) | Err(Error::ObjectiveLimit(_)) => {}
                Err(error) => return Err(error),
            }
            tokio::select! {_ = self.stop.cancelled()=>return Ok(()),_ = tokio::time::sleep(Duration::from_millis(poll_ms))=>{}}
        }
    }
}
async fn execute(
    queue: DurableQueue,
    job: Job,
    executor: Arc<dyn JobExecutor>,
    cancel: CancellationToken,
    policy: QueuePolicy,
) -> Result<Job> {
    let execution = job
        .execution_id
        .clone()
        .ok_or_else(|| state("missing job execution fence"))?;
    let id = job.id.clone();
    if cancel.is_cancelled() {
        let q = queue.clone();
        let j = id.clone();
        let e = execution.clone();
        return blocking(move || q.finish(&j, &e, JobOutcome::Cancelled)).await;
    }
    // Spawn the host future so a panic becomes an uncertain outcome rather than
    // unwinding past the lease bookkeeping. Cancellation remains cooperative.
    let token = cancel.clone();
    let mut task = tokio::spawn(async move { executor.execute(job, token).await });
    let mut heartbeat = tokio::time::interval(Duration::from_millis(
        (policy
            .lease_ms
            .min(policy.task_timeout_ms.unwrap_or(u64::MAX))
            / 3)
        .max(1),
    ));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut stopping = false;
    let outcome = loop {
        tokio::select! {
            result=&mut task=>break match result {
                Ok(Ok(outcome))=>outcome,
                Ok(Err(_))=>JobOutcome::Uncertain{error:"executor failed without a confirmed outcome".into()},
                Err(_)=>JobOutcome::Uncertain{error:"executor panicked without a confirmed outcome".into()},
            },
            _=cancel.cancelled(),if !stopping=>{
                stopping=true;
                let q=queue.clone();let j=id.clone();let e=execution.clone();
                if let Err(err)=blocking(move||q.cancel_execution(&j,&e)).await {
                    let _=task.await;return Err(err);
                }
            },
            _=heartbeat.tick()=>{
                let q=queue.clone();let j=id.clone();let e=execution.clone();
                match blocking(move||q.heartbeat(&j,&e)).await {
                    Ok(renewed)=>{if renewed.cancellation_requested {cancel.cancel();}},
                    Err(err)=>{cancel.cancel();let _=task.await;return Err(err);},
                }
            }
        }
    };
    let q = queue.clone();
    let j = id.clone();
    let e = execution.clone();
    match blocking(move || q.finish(&j, &e, outcome)).await {
        Ok(job) => Ok(job),
        Err(error) => {
            // An invalid/oversized callback result cannot be silently discarded
            // while reporting success. Fence the unknown outcome if still leased.
            let q = queue;
            let j = id;
            let e = execution;
            let _ = blocking(move || {
                q.finish(
                    &j,
                    &e,
                    JobOutcome::Uncertain {
                        error: "executor outcome could not be committed".into(),
                    },
                )
            })
            .await;
            Err(error)
        }
    }
}
