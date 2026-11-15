use super::*;
use crate::objective::ObjectivePolicy;
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) fn setup(
    policy: QueuePolicy,
) -> (
    tempfile::TempDir,
    SessionStore,
    DurableQueue,
    Arc<AtomicU64>,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let mut queue = store.queue("work").unwrap();
    let clock = Arc::new(AtomicU64::new(1000));
    let source = clock.clone();
    queue.clock = Arc::new(move || Ok(source.load(Ordering::SeqCst)));
    queue.open(policy).unwrap();
    (dir, store, queue, clock)
}
pub(super) fn input(lane: &str) -> JobInput {
    serde_json::from_value(json!({"kind":"test","lane":lane,"payload":{"text":"Hello"}})).unwrap()
}
fn claim(queue: &DurableQueue) -> Option<Job> {
    queue.claim("worker", &["test".into()]).unwrap()
}
pub(super) fn complete(queue: &DurableQueue, job: &Job) -> Job {
    queue
        .finish(
            &job.id,
            job.execution_id.as_deref().unwrap(),
            JobOutcome::Completed {
                result: json!({"ok":true}),
            },
        )
        .unwrap()
}

#[test]
fn priority_fifo_lanes_and_concurrency_are_durable_scheduler_decisions() {
    let (_dir, _store, q, _clock) = setup(QueuePolicy {
        max_concurrency: 2,
        ..Default::default()
    });
    let first = q.enqueue(input("a")).unwrap();
    let second = q.enqueue(input("a")).unwrap();
    let mut high = input("b");
    high.priority = 10;
    let high = q.enqueue(high).unwrap();
    let pending = q.enqueue(input("c")).unwrap();
    assert_eq!(claim(&q).unwrap().id, high.id);
    let active = claim(&q).unwrap();
    assert_eq!(active.id, first.id);
    assert!(claim(&q).is_none());
    complete(&q, &active);
    let next = claim(&q).unwrap();
    assert_eq!(next.id, second.id);
    complete(&q, &next);
    assert_eq!(claim(&q).unwrap().id, pending.id);
}

#[test]
fn concurrent_idempotent_enqueue_and_claim_use_database_cas_across_handles() {
    let (_dir, store, q, _clock) = setup(QueuePolicy {
        max_concurrency: 2,
        ..Default::default()
    });
    let mut spec = input("same");
    spec.idempotency_key = Some("dedup".into());
    let handles: Vec<_> = (0..12)
        .map(|_| {
            let other = SessionStore::from_db(store.db.clone(), "tenant".into())
                .unwrap()
                .queue("work")
                .unwrap();
            let spec = spec.clone();
            std::thread::spawn(move || other.enqueue(spec).unwrap())
        })
        .collect();
    let jobs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(jobs.iter().all(|job| job.id == jobs[0].id));
    assert_eq!(q.inspect().unwrap()["retained_jobs"], 1);
    spec.payload = json!({"changed":true});
    assert!(
        q.enqueue(spec)
            .unwrap_err()
            .to_string()
            .contains("IDEMPOTENCY_CONFLICT")
    );
    let handles: Vec<_> = (0..12)
        .map(|_| {
            let other = SessionStore::from_db(store.db.clone(), "tenant".into())
                .unwrap()
                .queue("work")
                .unwrap();
            std::thread::spawn(move || other.claim("another", &["test".into()]).unwrap())
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .filter_map(|h| h.join().unwrap())
            .count(),
        1
    );
    assert_eq!(q.get(&jobs[0].id).unwrap().unwrap().attempts, 1);
}

#[test]
fn lost_unsafe_lease_blocks_lane_and_late_results_until_reconciled() {
    let (_dir, _store, q, clock) = setup(QueuePolicy {
        lease_ms: 100,
        max_concurrency: 2,
        ..Default::default()
    });
    let job = q.enqueue(input("same")).unwrap();
    q.enqueue(input("same")).unwrap();
    q.enqueue(input("other")).unwrap();
    let running = claim(&q).unwrap();
    assert_eq!(job.id, running.id);
    clock.store(1100, Ordering::SeqCst);
    assert!(
        q.finish(
            &job.id,
            running.execution_id.as_deref().unwrap(),
            JobOutcome::Completed { result: json!(1) }
        )
        .unwrap_err()
        .to_string()
        .contains("LEASE_EXPIRED")
    );
    let expired = q.maintain().unwrap();
    assert_eq!(expired[0].status, JobStatus::Uncertain);
    assert_eq!(claim(&q).unwrap().input.lane, "other");
    assert!(claim(&q).is_none());
    assert!(
        q.reconcile(&job.id, "wrong", Resolution::Cancelled)
            .is_err()
    );
    q.reconcile(
        &job.id,
        running.execution_id.as_deref().unwrap(),
        Resolution::Completed {
            result: json!({"receipt":"external-1"}),
        },
    )
    .unwrap();
    assert_eq!(claim(&q).unwrap().input.lane, "same");
    assert!(
        q.finish(
            &job.id,
            running.execution_id.as_deref().unwrap(),
            JobOutcome::Completed { result: json!(2) }
        )
        .is_err()
    );
}

#[test]
fn explicit_retry_safety_preserves_attempts_backoff_and_execution_fences() {
    let (_dir, _store, q, clock) = setup(QueuePolicy {
        lease_ms: 100,
        retry_delay_ms: 10,
        ..Default::default()
    });
    let mut spec = input("a");
    spec.retry_safe = true;
    spec.max_attempts = 2;
    q.enqueue(spec).unwrap();
    let old = claim(&q).unwrap();
    clock.store(1100, Ordering::SeqCst);
    assert_eq!(q.maintain().unwrap()[0].status, JobStatus::Pending);
    assert!(claim(&q).is_none());
    clock.store(1110, Ordering::SeqCst);
    let new = claim(&q).unwrap();
    assert_eq!(new.attempts, 2);
    assert_ne!(new.execution_id, old.execution_id);
    assert!(
        q.finish(
            &old.id,
            old.execution_id.as_deref().unwrap(),
            JobOutcome::Cancelled
        )
        .is_err()
    );
    let failed = q
        .finish(
            &new.id,
            new.execution_id.as_deref().unwrap(),
            JobOutcome::Failed {
                error: "confirmed transient failure".into(),
                retryable: true,
            },
        )
        .unwrap();
    assert_eq!(failed.status, JobStatus::Failed);
    assert!(claim(&q).is_none());
}

#[test]
fn cancellation_retains_running_lane_and_discards_late_success() {
    let (_dir, _store, q, _clock) = setup(Default::default());
    let waiting = q.enqueue(input("later")).unwrap();
    let job = q.enqueue(input("a")).unwrap();
    q.enqueue(input("a")).unwrap();
    assert_eq!(q.cancel(&waiting.id).unwrap().status, JobStatus::Cancelled);
    let active = claim(&q).unwrap();
    assert_eq!(active.id, job.id);
    assert_eq!(q.cancel(&job.id).unwrap().status, JobStatus::Cancelling);
    assert!(claim(&q).is_none());
    assert!(
        q.heartbeat(&job.id, active.execution_id.as_deref().unwrap())
            .unwrap()
            .cancellation_requested
    );
    let cancelled = complete(&q, &active);
    assert_eq!(cancelled.status, JobStatus::Cancelled);
    assert!(cancelled.result.is_none());
    assert!(claim(&q).is_some());
}

#[test]
fn claims_consume_root_attempts_atomically_and_never_refresh_budget() {
    let (_dir, store, q, _clock) = setup(Default::default());
    let policy = ObjectivePolicy {
        max_tool_calls: 1,
        ..Default::default()
    };
    store.open_objective("root", policy.clone()).unwrap();
    for lane in ["a", "b"] {
        let mut spec = input(lane);
        spec.objective_id = Some("root".into());
        q.enqueue(spec).unwrap();
    }
    let first = claim(&q).unwrap();
    assert!(
        q.claim("second", &["test".into()])
            .unwrap_err()
            .to_string()
            .contains("tool_call_limit")
    );
    assert_eq!(store.inspect_objective("root").unwrap()["tool_calls"], 1);
    q.heartbeat(&first.id, first.execution_id.as_deref().unwrap())
        .unwrap();
    complete(&q, &first);
    store.open_objective("root", policy).unwrap();
    assert_eq!(store.inspect_objective("root").unwrap()["tool_calls"], 1);
    let jobs = q.list(10, None).unwrap();
    assert_eq!(jobs["items"][1]["status"], "failed");
    assert_eq!(jobs["items"][1]["attempts"], 0);
}

#[test]
fn separate_queues_cannot_race_past_one_shared_root_budget() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    store
        .open_objective(
            "root",
            ObjectivePolicy {
                max_tool_calls: 1,
                ..Default::default()
            },
        )
        .unwrap();
    let queues: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|name| {
            let s = SessionStore::from_db(store.db.clone(), "t".into()).unwrap();
            let q = s.queue(name).unwrap();
            q.open(Default::default()).unwrap();
            let mut spec = input(name);
            spec.objective_id = Some("root".into());
            q.enqueue(spec).unwrap();
            q
        })
        .collect();
    let handles: Vec<_> = queues
        .into_iter()
        .map(|q| std::thread::spawn(move || q.claim("w", &["test".into()])))
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|result| result.is_ok())
            .count(),
        1
    );
    assert_eq!(store.inspect_objective("root").unwrap()["tool_calls"], 1);
}

#[test]
fn reopening_keeps_claims_idempotency_and_requires_explicit_maintenance() {
    let dir = tempfile::tempdir().unwrap();
    let id;
    {
        let store = SessionStore::open(dir.path(), "t".into()).unwrap();
        let mut q = store.queue("q").unwrap();
        q.clock = Arc::new(|| Ok(1000));
        q.open(Default::default()).unwrap();
        let mut spec = input("a");
        spec.idempotency_key = Some("key".into());
        id = q.enqueue(spec).unwrap().id;
        claim(&q).unwrap();
    }
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let q = store.queue("q").unwrap();
    q.open(Default::default()).unwrap();
    assert_eq!(q.get(&id).unwrap().unwrap().status, JobStatus::Running);
    assert!(claim(&q).is_none());
    q.maintain().unwrap();
    assert_eq!(q.get(&id).unwrap().unwrap().status, JobStatus::Uncertain);
    let other = store
        .for_tenant("other".into())
        .unwrap()
        .queue("q")
        .unwrap();
    other.open(Default::default()).unwrap();
    assert!(other.get(&id).unwrap().is_none());
}

#[test]
fn pagination_scopes_policy_bounds_schedules_and_result_limits() {
    let (_dir, store, q, _clock) = setup(QueuePolicy {
        max_payload_bytes: 128,
        ..Default::default()
    });
    for i in 0..17 {
        q.enqueue(input(&format!("l{i}"))).unwrap();
    }
    let mut ids = vec![];
    let mut cursor = None;
    loop {
        let page = q.list(3, cursor.as_deref()).unwrap();
        ids.extend(
            page["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|j| j["id"].as_str().unwrap().to_owned()),
        );
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(ids.len(), 17);
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 17);
    let cursor = q.list(1, None).unwrap()["next_cursor"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        store
            .for_tenant("another".into())
            .unwrap()
            .queue("work")
            .unwrap()
            .list(1, Some(&cursor))
            .is_err()
    );
    assert!(
        q.open(QueuePolicy {
            max_concurrency: 9,
            ..Default::default()
        })
        .is_err()
    );
    assert!(q.dispatch(json!({"op":"enqueue","input":{"kind":"test","lane":"a","payload":{},"project_id":"no"}})).is_err());
    let active = claim(&q).unwrap();
    assert!(
        q.finish(
            &active.id,
            active.execution_id.as_deref().unwrap(),
            JobOutcome::Completed {
                result: json!("a".repeat(129))
            }
        )
        .is_err()
    );
    assert_eq!(
        q.get(&active.id).unwrap().unwrap().status,
        JobStatus::Running
    );
    assert!(q.claim("w", &[]).unwrap().is_none());
}

#[tokio::test]
async fn native_pool_renews_leases_and_never_overlaps_one_lane() {
    use std::sync::{Mutex, atomic::AtomicUsize};
    use std::time::Duration;
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let q = store.queue("native").unwrap();
    q.open(QueuePolicy {
        lease_ms: 150,
        max_concurrency: 2,
        ..Default::default()
    })
    .unwrap();
    for lane in ["a", "a", "b", "b"] {
        q.enqueue(input(lane)).unwrap();
    }
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let lanes = Arc::new(Mutex::new(BTreeSet::new()));
    let mut pool = WorkerPool::new(q.clone(), "native-worker".into()).unwrap();
    let (a, p, l) = (active.clone(), peak.clone(), lanes.clone());
    pool.register(
        "test",
        move |job: Job, _cancel: crate::provider::CancellationToken| {
            let (a, p, l) = (a.clone(), p.clone(), l.clone());
            async move {
                assert!(l.lock().unwrap().insert(job.input.lane.clone()));
                let count = a.fetch_add(1, Ordering::SeqCst) + 1;
                p.fetch_max(count, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(220)).await;
                a.fetch_sub(1, Ordering::SeqCst);
                assert!(l.lock().unwrap().remove(&job.input.lane));
                Ok(JobOutcome::Completed {
                    result: json!({"ok":true}),
                })
            }
        },
    )
    .unwrap();
    for _ in 0..4 {
        pool.run_once().await.unwrap();
    }
    assert_eq!(peak.load(Ordering::SeqCst), 2);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    let items = q.list(20, None).unwrap();
    for job in items["items"].as_array().unwrap() {
        assert_eq!(job["status"], "completed");
        assert_eq!(job["attempts"], 1);
        assert!(job["revision"].as_u64().unwrap() > 3);
    }
}

#[tokio::test]
async fn native_pool_cancellation_joins_executor_before_releasing_lane() {
    use std::{sync::atomic::AtomicBool, time::Duration};
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let q = store.queue("native").unwrap();
    q.open(QueuePolicy {
        lease_ms: 150,
        ..Default::default()
    })
    .unwrap();
    let job = q.enqueue(input("a")).unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let joined = Arc::new(AtomicBool::new(false));
    let mut pool = WorkerPool::new(q.clone(), "worker".into()).unwrap();
    let (ready, done) = (started.clone(), joined.clone());
    pool.register(
        "test",
        move |_job: Job, cancel: crate::provider::CancellationToken| {
            let (ready, done) = (ready.clone(), done.clone());
            async move {
                ready.notify_one();
                cancel.cancelled().await;
                tokio::time::sleep(Duration::from_millis(20)).await;
                done.store(true, Ordering::SeqCst);
                Ok(JobOutcome::Cancelled)
            }
        },
    )
    .unwrap();
    let pool = Arc::new(pool);
    let runner = pool.clone();
    let task = tokio::spawn(async move { runner.run_once().await });
    tokio::time::timeout(Duration::from_secs(2), started.notified())
        .await
        .unwrap();
    q.cancel(&job.id).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result[0].status, JobStatus::Cancelled);
    assert!(joined.load(Ordering::SeqCst));
    pool.stop();
    assert!(pool.run_once().await.unwrap().is_empty());
}

#[tokio::test]
async fn claim_failure_does_not_detach_dispatched_executors_or_reset_root_budget() {
    use std::{sync::atomic::AtomicBool, time::Duration};
    let (_dir, store, q, _clock) = setup(Default::default());
    store
        .open_objective(
            "root",
            ObjectivePolicy {
                max_tool_calls: 1,
                ..Default::default()
            },
        )
        .unwrap();
    for lane in ["a", "b"] {
        let mut spec = input(lane);
        spec.objective_id = Some("root".into());
        q.enqueue(spec).unwrap();
    }
    let done = Arc::new(AtomicBool::new(false));
    let completed = done.clone();
    let mut pool = WorkerPool::new(q.clone(), "w".into()).unwrap();
    pool.register(
        "test",
        move |_job: Job, _cancel: crate::provider::CancellationToken| {
            let done = completed.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                done.store(true, Ordering::SeqCst);
                Ok(JobOutcome::Completed { result: json!(1) })
            }
        },
    )
    .unwrap();
    assert!(
        pool.run_once()
            .await
            .unwrap_err()
            .to_string()
            .contains("tool_call_limit")
    );
    assert!(done.load(Ordering::SeqCst));
    assert_eq!(q.list(10, None).unwrap()["items"][0]["status"], "completed");
    assert_eq!(store.inspect_objective("root").unwrap()["tool_calls"], 1);
}

#[tokio::test]
async fn executor_errors_and_invalid_results_remain_uncertain() {
    let (_dir, _store, q, _clock) = setup(QueuePolicy {
        max_payload_bytes: 64,
        ..Default::default()
    });
    let job = q.enqueue(input("a")).unwrap();
    let mut pool = WorkerPool::new(q.clone(), "w".into()).unwrap();
    pool.register(
        "test",
        |_job: Job, _cancel: crate::provider::CancellationToken| async {
            Err(state("application callback failed"))
        },
    )
    .unwrap();
    let result = pool.run_once().await.unwrap();
    assert_eq!(result[0].status, JobStatus::Uncertain);
    q.reconcile(
        &job.id,
        result[0].execution_id.as_deref().unwrap(),
        Resolution::Cancelled,
    )
    .unwrap();
    let next = q.enqueue(input("b")).unwrap();
    let mut pool = WorkerPool::new(q.clone(), "other".into()).unwrap();
    pool.register(
        "test",
        |_job: Job, _cancel: crate::provider::CancellationToken| async {
            Ok(JobOutcome::Completed {
                result: json!("x".repeat(65)),
            })
        },
    )
    .unwrap();
    assert!(pool.run_once().await.is_err());
    assert_eq!(
        q.get(&next.id).unwrap().unwrap().status,
        JobStatus::Uncertain
    );
}

#[test]
fn task_deadline_cancels_even_if_completion_precedes_the_next_heartbeat() {
    let (_dir, _store, q, clock) = setup(QueuePolicy {
        lease_ms: 100,
        task_timeout_ms: Some(20),
        ..Default::default()
    });
    q.enqueue(input("a")).unwrap();
    let job = claim(&q).unwrap();
    clock.store(1020, Ordering::SeqCst);
    let finished = q
        .finish(
            &job.id,
            job.execution_id.as_deref().unwrap(),
            JobOutcome::Completed {
                result: json!("late"),
            },
        )
        .unwrap();
    assert_eq!(finished.status, JobStatus::Cancelled);
    assert!(finished.result.is_none());
    assert_eq!(
        finished.error.as_deref(),
        Some("job task deadline exceeded")
    );
    q.enqueue(input("a")).unwrap();
    let job = claim(&q).unwrap();
    clock.store(1040, Ordering::SeqCst);
    assert!(
        q.heartbeat(&job.id, job.execution_id.as_deref().unwrap())
            .unwrap()
            .cancellation_requested
    );
}

#[tokio::test]
async fn native_dispatch_loop_keeps_other_jobs_running_after_one_root_is_exhausted() {
    use std::time::Duration;
    let (_dir, store, q, _clock) = setup(Default::default());
    store
        .open_objective(
            "root",
            ObjectivePolicy {
                max_tool_calls: 1,
                ..Default::default()
            },
        )
        .unwrap();
    for lane in ["a", "b"] {
        let mut spec = input(lane);
        spec.objective_id = Some("root".into());
        q.enqueue(spec).unwrap();
    }
    q.enqueue(input("free")).unwrap();
    let notified = Arc::new(tokio::sync::Notify::new());
    let done = notified.clone();
    let mut pool = WorkerPool::new(q.clone(), "w".into()).unwrap();
    pool.register(
        "test",
        move |job: Job, _cancel: crate::provider::CancellationToken| {
            let done = done.clone();
            async move {
                if job.input.lane == "free" {
                    done.notify_one();
                }
                Ok(JobOutcome::Completed {
                    result: json!(true),
                })
            }
        },
    )
    .unwrap();
    let pool = Arc::new(pool);
    let runner = pool.clone();
    let run = tokio::spawn(async move { runner.run(10).await });
    tokio::time::timeout(Duration::from_secs(2), notified.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if q.list(10, None).unwrap()["items"][2]["status"] == "completed" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    pool.stop();
    tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(q.list(10, None).unwrap()["items"][1]["status"], "failed");
    assert_eq!(q.list(10, None).unwrap()["items"][2]["status"], "completed");
}

#[test]
fn scheduled_and_unregistered_jobs_do_not_spend_root_attempts() {
    let (_dir, store, q, clock) = setup(Default::default());
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    let mut later = input("future");
    later.not_before = Some(1100);
    later.objective_id = Some("root".into());
    let future = q.enqueue(later).unwrap();
    let mut unknown = input("other");
    unknown.kind = "unregistered".into();
    unknown.objective_id = Some("root".into());
    q.enqueue(unknown).unwrap();
    assert!(claim(&q).is_none());
    assert!(q.maintain().unwrap().is_empty());
    assert_eq!(store.inspect_objective("root").unwrap()["tool_calls"], 0);
    clock.store(1100, Ordering::SeqCst);
    assert_eq!(claim(&q).unwrap().id, future.id);
    assert_eq!(store.inspect_objective("root").unwrap()["tool_calls"], 1);
}

#[test]
fn confirmed_failures_retry_only_when_declared_safe_and_charge_each_attempt() {
    let (_dir, store, q, clock) = setup(QueuePolicy {
        retry_delay_ms: 10,
        ..Default::default()
    });
    let mut unsafe_job = input("unsafe");
    unsafe_job.max_attempts = 2;
    q.enqueue(unsafe_job).unwrap();
    let job = claim(&q).unwrap();
    assert_eq!(
        q.finish(
            &job.id,
            job.execution_id.as_deref().unwrap(),
            JobOutcome::Failed {
                error: "known".into(),
                retryable: true
            }
        )
        .unwrap()
        .status,
        JobStatus::Failed
    );
    store
        .open_objective(
            "root",
            ObjectivePolicy {
                max_tool_calls: 2,
                ..Default::default()
            },
        )
        .unwrap();
    let mut safe = input("safe");
    safe.max_attempts = 2;
    safe.retry_safe = true;
    safe.objective_id = Some("root".into());
    q.enqueue(safe).unwrap();
    let first = claim(&q).unwrap();
    assert_eq!(
        q.finish(
            &first.id,
            first.execution_id.as_deref().unwrap(),
            JobOutcome::Failed {
                error: "known".into(),
                retryable: true
            }
        )
        .unwrap()
        .status,
        JobStatus::Pending
    );
    assert!(claim(&q).is_none());
    clock.store(1010, Ordering::SeqCst);
    let second = claim(&q).unwrap();
    assert!(
        q.cancel_execution(&first.id, first.execution_id.as_deref().unwrap())
            .is_err()
    );
    complete(&q, &second);
    assert_eq!(store.inspect_objective("root").unwrap()["tool_calls"], 2);
}

#[test]
fn cancelling_an_expired_current_execution_prevents_safe_automatic_replay() {
    let (_dir, _store, q, clock) = setup(QueuePolicy {
        lease_ms: 100,
        retry_delay_ms: 0,
        ..Default::default()
    });
    let mut spec = input("a");
    spec.retry_safe = true;
    spec.max_attempts = 2;
    q.enqueue(spec).unwrap();
    let job = claim(&q).unwrap();
    clock.store(1100, Ordering::SeqCst);
    let stopped = q
        .cancel_execution(&job.id, job.execution_id.as_deref().unwrap())
        .unwrap();
    assert!(stopped.cancellation_requested);
    assert_eq!(q.maintain().unwrap()[0].status, JobStatus::Uncertain);
    assert!(claim(&q).is_none());
    assert!(
        q.reconcile(
            &job.id,
            job.execution_id.as_deref().unwrap(),
            Resolution::Retry
        )
        .is_err()
    );
}

fn group_spec() -> GroupSpec {
    serde_json::from_value(
        json!({"objective_id":"root","coordinator_agent_id":"lead","tasks":[
        {"id":"a","specialist_id":"research","prompt":"Find evidence"},
        {"id":"b","specialist_id":"review","prompt":"Review evidence"}]}),
    )
    .unwrap()
}
fn group_claim(q: &DurableQueue, group: &Value) -> Job {
    q.claim(
        "worker",
        &[group["specialist_kind"].as_str().unwrap().into()],
    )
    .unwrap()
    .unwrap()
}
#[test]
fn delegation_creation_is_atomic_idempotent_and_scoped() {
    let (_dir, store, q, _) = setup(QueuePolicy {
        max_jobs: 1,
        ..Default::default()
    });
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    assert!(
        q.create_group("turn", group_spec())
            .unwrap_err()
            .to_string()
            .contains("RETENTION")
    );
    assert_eq!(q.inspect().unwrap()["retained_jobs"], 0);
    assert!(q.group("turn").is_err());
    let q = store.queue("bigger").unwrap();
    q.open(QueuePolicy::default()).unwrap();
    let group = q.create_group("turn", group_spec()).unwrap();
    assert_eq!(group["status"], "waiting");
    assert_eq!(
        q.create_group("turn", group_spec()).unwrap()["job_ids"],
        group["job_ids"]
    );
    assert_eq!(q.inspect().unwrap()["retained_jobs"], 2);
    let mut changed = group_spec();
    changed.tasks[0].prompt = "different".into();
    assert!(
        q.create_group("turn", changed)
            .unwrap_err()
            .to_string()
            .contains("IDEMPOTENCY")
    );
    let other = SessionStore::from_db(store.db.clone(), "other".into())
        .unwrap()
        .queue("bigger")
        .unwrap();
    other.open(QueuePolicy::default()).unwrap();
    assert!(other.group("turn").is_err());
    let second = q.create_group("other-turn", group_spec()).unwrap();
    assert_ne!(second["specialist_kind"], group["specialist_kind"]);
    assert!(q.synthesize_group("turn").is_err());
}
#[test]
fn uncertain_children_block_fanin_and_failures_are_preserved() {
    let (_dir, store, q, clock) = setup(QueuePolicy::default());
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    let g = q.create_group("turn", group_spec()).unwrap();
    let lost = group_claim(&q, &g);
    let done = group_claim(&q, &g);
    complete(&q, &done);
    clock.store(40_000, Ordering::SeqCst);
    q.maintain().unwrap();
    assert_eq!(q.group("turn").unwrap()["status"], "blocked_uncertain");
    assert!(q.synthesize_group("turn").is_err());
    q.reconcile(
        &lost.id,
        lost.execution_id.as_deref().unwrap(),
        Resolution::Failed {
            error: "effect rejected".into(),
        },
    )
    .unwrap();
    assert_eq!(q.group("turn").unwrap()["status"], "ready");
    let synthesis = q.synthesize_group("turn").unwrap();
    assert_eq!(synthesis.input.depth, 0);
    assert_eq!(synthesis.input.objective_id.as_deref(), Some("root"));
    assert!(
        synthesis.input.payload["prompt"]
            .as_str()
            .unwrap()
            .contains("effect rejected")
    );
    assert!(
        synthesis.input.payload["prompt"]
            .as_str()
            .unwrap()
            .contains("unchecked")
    );
    assert_eq!(q.synthesize_group("turn").unwrap().id, synthesis.id);
}
#[test]
fn group_synthesis_uses_database_cas_across_independent_handles() {
    let (_dir, store, q, _) = setup(QueuePolicy::default());
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    let g = q.create_group("turn", group_spec()).unwrap();
    for _ in 0..2 {
        complete(&q, &group_claim(&q, &g));
    }
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let q = SessionStore::from_db(store.db.clone(), "tenant".into())
                .unwrap()
                .queue("work")
                .unwrap();
            std::thread::spawn(move || q.synthesize_group("turn").unwrap().id)
        })
        .collect();
    let ids: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(ids.iter().all(|id| *id == ids[0]));
    assert_eq!(q.inspect().unwrap()["retained_jobs"], 3);
}
#[test]
fn group_cancellation_keeps_inflight_capacity_until_acknowledged() {
    let (_dir, store, q, _) = setup(QueuePolicy::default());
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    let g = q.create_group("turn", group_spec()).unwrap();
    let active = group_claim(&q, &g);
    let report = q.cancel_group("turn").unwrap();
    assert_eq!(report["status"], "cancelled");
    assert_eq!(report["ready"], false);
    assert_eq!(
        q.get(&active.id).unwrap().unwrap().status,
        JobStatus::Cancelling
    );
    assert_eq!(complete(&q, &active).status, JobStatus::Cancelled);
    assert!(
        q.synthesize_group("turn")
            .unwrap_err()
            .to_string()
            .contains("CANCELLED")
    );
    assert_eq!(q.cancel_group("turn").unwrap()["cancelled"], true);
}
#[test]
fn proofs_bind_exact_results_and_gate_fanin_without_automatic_review() {
    let (_dir, store, q, _) = setup(QueuePolicy::default());
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    let mut spec = group_spec();
    spec.require_verification = true;
    let g = q.create_group("turn", spec).unwrap();
    assert!(q.group_verification_input("turn", "a").is_err());
    for _ in 0..2 {
        complete(&q, &group_claim(&q, &g));
    }
    assert_eq!(q.group("turn").unwrap()["status"], "waiting");
    assert!(q.record_group_proof("turn", "a", "missing").is_err());
    for (task, passed) in [("a", true), ("b", false)] {
        let input = q.group_verification_input("turn", task).unwrap();
        let claim = store
            .objective_command(
                "root",
                &json!({"op":"verify_begin","key":task,"input":input}).to_string(),
            )
            .unwrap();
        store.objective_command("root",&json!({"op":"verify_finish","key":task,"execution_id":claim["execution_id"],"verdict":{"passed":passed,"evidence":["test receipt"]}}).to_string()).unwrap();
        let proof = q.record_group_proof("turn", task, task).unwrap();
        assert_eq!(proof["met"], passed);
        assert_eq!(q.record_group_proof("turn", task, task).unwrap(), proof);
    }
    let report = q.group("turn").unwrap();
    assert_eq!(report["status"], "ready");
    assert_eq!(report["tasks"][1]["verification"], "failed");
    assert!(q.record_group_proof("turn", "a", "b").is_err());
    assert!(
        q.synthesize_group("turn").unwrap().input.payload["prompt"]
            .as_str()
            .unwrap()
            .contains("test receipt")
    );
}
#[test]
fn group_payload_overflow_rolls_back_without_truncating_outcomes() {
    let (_dir, store, q, _) = setup(QueuePolicy {
        max_payload_bytes: 1024,
        ..Default::default()
    });
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    let g = q.create_group("turn", group_spec()).unwrap();
    for _ in 0..2 {
        let job = group_claim(&q, &g);
        q.finish(
            &job.id,
            job.execution_id.as_deref().unwrap(),
            JobOutcome::Completed {
                result: json!("x".repeat(600)),
            },
        )
        .unwrap();
    }
    assert!(q.synthesize_group("turn").is_err());
    assert_eq!(q.inspect().unwrap()["retained_jobs"], 2);
    assert_eq!(q.group("turn").unwrap()["status"], "ready");
}

#[test]
fn proofs_reject_unrelated_inputs_unfinished_checks_and_empty_evidence() {
    let (_dir, store, q, _) = setup(QueuePolicy::default());
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    let g = q.create_group("turn", group_spec()).unwrap();
    for _ in 0..2 {
        complete(&q, &group_claim(&q, &g));
    }
    let exact = q.group_verification_input("turn", "a").unwrap();
    for (key, input, evidence) in [
        ("wrong", json!({"other":"job"}), json!(["receipt"])),
        ("empty", exact.clone(), json!([])),
        ("pending", exact, json!(["receipt"])),
    ] {
        let claim = store
            .objective_command(
                "root",
                &json!({"op":"verify_begin","key":key,"input":input}).to_string(),
            )
            .unwrap();
        if key != "pending" {
            store.objective_command("root",&json!({"op":"verify_finish","key":key,"execution_id":claim["execution_id"],"verdict":{"passed":true,"evidence":evidence}}).to_string()).unwrap();
        }
        assert!(q.record_group_proof("turn", "a", key).is_err());
    }
    assert!(q.group("turn").unwrap()["tasks"][0]["proof"].is_null());
}

#[test]
fn fanin_cannot_refresh_an_exhausted_root_budget() {
    let (_dir, store, q, _) = setup(QueuePolicy::default());
    store
        .open_objective(
            "root",
            ObjectivePolicy {
                max_tool_calls: 2,
                ..Default::default()
            },
        )
        .unwrap();
    let g = q.create_group("turn", group_spec()).unwrap();
    for _ in 0..2 {
        complete(&q, &group_claim(&q, &g));
    }
    let synthesis = q.synthesize_group("turn").unwrap();
    assert!(
        q.claim("coordinator", &[synthesis.input.kind])
            .unwrap_err()
            .to_string()
            .contains("OBJECTIVE_LIMIT")
    );
    assert_eq!(
        q.get(&synthesis.id).unwrap().unwrap().status,
        JobStatus::Failed
    );
    assert_eq!(store.inspect_objective("root").unwrap()["tool_calls"], 2);
    assert_eq!(q.synthesize_group("turn").unwrap().id, synthesis.id);
}
