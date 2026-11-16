use super::tests::{input, setup};
use super::*;
use crate::objective::ObjectivePolicy;

fn job(kind: &str, lane: &str, extra: Value) -> JobInput {
    let mut value = json!({"kind":kind,"lane":lane,"payload":{"n":1}});
    value
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().cloned().unwrap_or_default());
    serde_json::from_value(value).unwrap()
}
fn filter(value: Value) -> JobFilter {
    serde_json::from_value(value).unwrap()
}
fn fail(q: &DurableQueue, kind: &str) -> Job {
    let claimed = q.claim("w", &[kind.into()]).unwrap().unwrap();
    q.finish(
        &claimed.id,
        claimed.execution_id.as_deref().unwrap(),
        JobOutcome::Failed {
            error: "boom".into(),
            retryable: false,
        },
    )
    .unwrap()
}

#[test]
fn listing_filters_by_status_kind_lane_and_objective_and_pages_with_a_cursor() {
    let (_dir, store, q, _) = setup(QueuePolicy::default());
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    for n in 0..5 {
        q.enqueue(job("a", &format!("lane-{n}"), json!({})))
            .unwrap();
    }
    q.enqueue(job("b.sub", "other", json!({"objective_id":"root"})))
        .unwrap();
    q.enqueue(job("b.sup", "other2", json!({}))).unwrap();
    let done = q.claim("w", &["a".into()]).unwrap().unwrap();
    q.finish(
        &done.id,
        done.execution_id.as_deref().unwrap(),
        JobOutcome::Completed { result: json!(1) },
    )
    .unwrap();
    let ids = |value: &Value| {
        value["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|job| job["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(&q
            .list_where(&filter(json!({"kind":"a"})), 50, None)
            .unwrap())
        .len(),
        5
    );
    assert_eq!(
        ids(&q
            .list_where(&filter(json!({"status":["completed"]})), 50, None)
            .unwrap()),
        std::slice::from_ref(&done.id)
    );
    assert_eq!(
        ids(&q
            .list_where(&filter(json!({"kind_prefix":"b."})), 50, None)
            .unwrap())
        .len(),
        2
    );
    assert_eq!(
        ids(&q
            .list_where(&filter(json!({"lane":"lane-3"})), 50, None)
            .unwrap())
        .len(),
        1
    );
    assert_eq!(
        ids(&q
            .list_where(&filter(json!({"objective_id":"root"})), 50, None)
            .unwrap())
        .len(),
        1
    );
    assert_eq!(
        ids(&q
            .list_where(&filter(json!({"status":["pending"],"kind":"a"})), 50, None)
            .unwrap())
        .len(),
        4
    );
    assert!(q.list_where(&JobFilter::default(), 0, None).is_err());
    assert!(serde_json::from_value::<JobFilter>(json!({"bogus":1})).is_err());
    // Paging with a one-item limit visits every match exactly once.
    let (mut seen, mut cursor) = (vec![], None::<String>);
    loop {
        let page = q
            .list_where(&filter(json!({"status":["pending"]})), 1, cursor.as_deref())
            .unwrap();
        seen.extend(ids(&page));
        match page["next_cursor"].as_str() {
            Some(next) => cursor = Some(next.to_owned()),
            None => break,
        }
    }
    assert_eq!(seen.len(), 6);
    assert_eq!(seen.iter().collect::<BTreeSet<_>>().len(), 6);
}

#[test]
fn failed_jobs_are_retried_explicitly_within_attempts_unless_forced() {
    let (_dir, store, q, clock) = setup(QueuePolicy::default());
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    q.enqueue(job(
        "a",
        "l1",
        json!({"max_attempts":2,"objective_id":"root"}),
    ))
    .unwrap();
    let failed = fail(&q, "a");
    assert_eq!((failed.status, failed.attempts), (JobStatus::Failed, 1));
    clock.fetch_add(5, std::sync::atomic::Ordering::SeqCst);
    let again = q.retry_failed(&failed.id, false).unwrap();
    assert_eq!(
        (
            again.status,
            again.error.clone(),
            again.attempts,
            again.execution_id.clone()
        ),
        (JobStatus::Pending, None, 1, None)
    );
    let second = fail(&q, "a");
    assert_eq!(second.attempts, 2);
    assert!(
        q.retry_failed(&failed.id, false)
            .unwrap_err()
            .to_string()
            .contains("JOB_RETRY_NOT_ALLOWED")
    );
    let forced = q.retry_failed(&failed.id, true).unwrap();
    assert_eq!(forced.status, JobStatus::Pending);
    let third = q.claim("w", &["a".into()]).unwrap().unwrap();
    assert_eq!(
        third.attempts, 3,
        "attempts keep counting; a retry never resets the history"
    );
    assert_ne!(
        third.execution_id, second.execution_id,
        "each run has its own fence"
    );
    // Only failed jobs can be retried.
    assert!(
        q.retry_failed(&third.id, true)
            .unwrap_err()
            .to_string()
            .contains("JOB_NOT_RETRYABLE")
    );
    assert!(
        q.retry_failed("99999", false)
            .unwrap_err()
            .to_string()
            .contains("JOB_NOT_FOUND")
    );
    q.finish(
        &third.id,
        third.execution_id.as_deref().unwrap(),
        JobOutcome::Completed { result: json!(1) },
    )
    .unwrap();
    assert!(
        q.retry_failed(&third.id, true)
            .unwrap_err()
            .to_string()
            .contains("JOB_NOT_RETRYABLE")
    );
}

#[test]
fn a_retry_is_checked_against_the_objective_and_refused_once_it_is_cancelled() {
    let (_dir, store, q, _) = setup(QueuePolicy::default());
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    q.enqueue(job(
        "a",
        "l1",
        json!({"objective_id":"root","max_attempts":3}),
    ))
    .unwrap();
    let failed = fail(&q, "a");
    store
        .objective_command("root", r#"{"op":"cancel"}"#)
        .unwrap();
    let error = q.retry_failed(&failed.id, true).unwrap_err().to_string();
    assert!(error.to_lowercase().contains("cancel"), "{error}");
    let still = q.get(&failed.id).unwrap().unwrap();
    assert_eq!(
        (still.status, still.error.as_deref()),
        (JobStatus::Failed, Some("boom")),
        "a refused retry changes nothing"
    );
    assert_eq!(
        q.events(0, 100).unwrap()["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "job.retried")
            .count(),
        0
    );
}

#[test]
fn bulk_cancel_stops_matching_work_and_leaves_the_rest() {
    let (_dir, _store, q, _) = setup(QueuePolicy::default());
    for n in 0..4 {
        q.enqueue(job("batch", &format!("b{n}"), json!({})))
            .unwrap();
    }
    let keep = q.enqueue(job("keep", "k", json!({}))).unwrap();
    let running = q.claim("w", &["batch".into()]).unwrap().unwrap();
    let result = q.cancel_where(&filter(json!({"kind":"batch"}))).unwrap();
    assert_eq!(result["cancelled"].as_array().unwrap().len(), 4);
    assert_eq!(result["truncated"], false);
    assert_eq!(
        q.get(&running.id).unwrap().unwrap().status,
        JobStatus::Cancelling,
        "a running job is asked to stop"
    );
    assert_eq!(q.get(&keep.id).unwrap().unwrap().status, JobStatus::Pending);
    assert_eq!(
        q.cancel_where(&filter(json!({"kind":"batch"}))).unwrap()["cancelled"],
        json!([]),
        "a second call is a no-op: cancelling jobs are not cancelled again"
    );
    assert_eq!(
        q.cancel_where(&filter(json!({"kind":"nothing"}))).unwrap()["cancelled"],
        json!([])
    );
    // Terminal statuses in the filter are ignored: finished jobs are never touched.
    assert_eq!(
        q.cancel_where(&filter(json!({"status":["completed","failed"]})))
            .unwrap()["cancelled"],
        json!([])
    );
}

#[test]
fn every_status_change_is_a_monotonic_event_committed_with_it() {
    let (_dir, _store, q, _) = setup(QueuePolicy::default());
    let first = q.enqueue(input("lane")).unwrap();
    let claimed = q.claim("w", &["test".into()]).unwrap().unwrap();
    q.heartbeat(&claimed.id, claimed.execution_id.as_deref().unwrap())
        .unwrap();
    q.finish(
        &claimed.id,
        claimed.execution_id.as_deref().unwrap(),
        JobOutcome::Completed { result: json!(1) },
    )
    .unwrap();
    let page = q.events(0, 100).unwrap();
    let kinds: Vec<_> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["seq"].as_u64().unwrap(),
                e["subject"]["status"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            (1, "pending".to_owned()),
            (2, "running".to_owned()),
            (3, "completed".to_owned())
        ],
        "a heartbeat is not an event"
    );
    assert_eq!(
        (page["next_after"].clone(), page["latest"].clone()),
        (json!(3), json!(3))
    );
    assert_eq!(page["items"][0]["subject"]["job_id"], first.id);
    // Cursor semantics: resume after the last seen sequence.
    assert_eq!(
        q.events(2, 100).unwrap()["items"].as_array().unwrap().len(),
        1
    );
    assert_eq!(q.events(3, 100).unwrap()["items"], json!([]));
    assert_eq!(q.events(0, 2).unwrap()["next_after"], 2);
    assert!(q.events(0, 0).is_err());
    // Events appear atomically with the change: a refused change leaves none behind.
    let before = q.events(0, 100).unwrap()["latest"].clone();
    assert!(q.retry_failed(&first.id, true).is_err());
    assert_eq!(q.events(0, 100).unwrap()["latest"], before);
    // Pruning is explicit and keeps the sequence monotonic.
    assert_eq!(q.prune_events(2).unwrap()["removed"], 2);
    assert_eq!(
        q.events(0, 100).unwrap()["items"].as_array().unwrap().len(),
        1
    );
    q.enqueue(job("test", "lane2", json!({}))).unwrap();
    assert_eq!(q.events(3, 100).unwrap()["items"][0]["seq"], 4);
    // Events are scoped to their queue and tenant.
    let other = _store.queue("elsewhere").unwrap();
    other.open(QueuePolicy::default()).unwrap();
    assert_eq!(other.events(0, 10).unwrap()["items"], json!([]));
}

#[test]
fn retries_and_goal_transitions_are_events_too_and_dispatch_exposes_everything() {
    let (_dir, store, q, _) = setup(QueuePolicy::default());
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    q.enqueue(job("a", "l", json!({}))).unwrap();
    let failed = fail(&q, "a");
    q.dispatch(json!({"op":"job_retry","job_id":failed.id,"force":true}))
        .unwrap();
    let events = q.dispatch(json!({"op":"events","after":0})).unwrap();
    let types: Vec<&str> = events["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(types.last(), Some(&"job.retried"));
    assert_eq!(
        q.dispatch(json!({"op":"jobs_list","filter":{"status":["pending"]}}))
            .unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        q.dispatch(json!({"op":"jobs_cancel_where","filter":{"kind":"a"}}))
            .unwrap()["cancelled"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    q.create_goal("g", serde_json::from_value(json!({"objective_id":"root","goal":"x","planner_agent_id":"p","coordinator_agent_id":"c"})).unwrap()).unwrap();
    q.cancel_goal("g").unwrap();
    let goal_events: Vec<Value> = q.events(0, 1000).unwrap()["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "goal.transition")
        .cloned()
        .collect();
    assert_eq!(
        goal_events
            .iter()
            .map(|e| e["subject"]["status"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["planning", "cancelled"]
    );
    assert_eq!(
        q.dispatch(json!({"op":"events_prune","up_to":1})).unwrap()["removed"],
        1
    );
}

#[test]
fn purging_frees_retention_for_a_long_lived_queue_but_protects_what_is_referenced() {
    let (_dir, store, q, clock) = setup(QueuePolicy {
        max_jobs: 6,
        ..Default::default()
    });
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    for n in 0..6 {
        q.enqueue(job(
            "cron.task",
            &format!("l{n}"),
            json!({"idempotency_key":format!("k{n}")}),
        ))
        .unwrap();
    }
    assert!(
        q.enqueue(job("cron.task", "x", json!({})))
            .unwrap_err()
            .to_string()
            .contains("RETENTION")
    );
    for _ in 0..4 {
        let claimed = q.claim("w", &["cron.task".into()]).unwrap().unwrap();
        q.finish(
            &claimed.id,
            claimed.execution_id.as_deref().unwrap(),
            JobOutcome::Completed { result: json!(1) },
        )
        .unwrap();
    }
    clock.fetch_add(10_000, std::sync::atomic::Ordering::SeqCst);
    let cutoff = 1000 + 10_000;
    // The purge needs an explicit scope and never touches group/goal jobs or live work.
    assert!(q.purge_jobs(&JobFilter::default(), cutoff, 50).is_err());
    assert!(
        q.purge_jobs(&filter(json!({"kind_prefix":"hive."})), cutoff, 50)
            .is_err()
    );
    assert!(
        q.purge_jobs(&filter(json!({"kind":"hive.goal.plan.x"})), cutoff, 50)
            .is_err()
    );
    assert!(
        q.purge_jobs(&filter(json!({"kind_prefix":"h"})), cutoff, 50)
            .is_err(),
        "a prefix that could cover hive.* is refused"
    );
    assert_eq!(
        q.purge_jobs(
            &filter(json!({"kind":"cron.task","status":["pending","running"]})),
            cutoff,
            50
        )
        .unwrap()["purged"],
        0,
        "live states are never purged"
    );
    assert_eq!(
        q.purge_jobs(&filter(json!({"kind":"cron.task"})), 1, 50)
            .unwrap()["purged"],
        0,
        "only jobs that ended before the cutoff"
    );
    assert_eq!(
        q.purge_jobs(&filter(json!({"kind":"cron.task"})), cutoff, 3)
            .unwrap()["purged"],
        3
    );
    assert_eq!(q.inspect().unwrap()["retained_jobs"], 3);
    assert_eq!(
        q.list_where(&JobFilter::default(), 50, None).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    // Retention is available again, and a purged job's idempotency key can be reused.
    q.enqueue(job("cron.task", "fresh", json!({"idempotency_key":"k0"})))
        .unwrap();
    assert_eq!(
        q.purge_jobs(&filter(json!({"kind":"cron.task"})), cutoff, 50)
            .unwrap()["purged"],
        1
    );
    assert!(
        q.events(0, 1000).unwrap()["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "jobs.purged" && e["subject"]["count"] == 3)
    );
}
