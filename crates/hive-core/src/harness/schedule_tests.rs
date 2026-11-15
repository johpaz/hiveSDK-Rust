use super::tests::{complete, input, setup};
use super::*;
use crate::{objective::ObjectivePolicy, scheduler::Schedule};
use std::sync::atomic::Ordering;
fn spec(schedule: Value) -> ScheduleSpec {
    serde_json::from_value(json!({"schedule":schedule,"job":{"kind":"test","lane":"scheduled","payload":{"prompt":"fixture"}},"misfire_policy":"fire_once"})).unwrap()
}
#[test]
fn schedules_start_paused_reopen_idempotently_and_publish_once_across_handles() {
    let (_dir, store, q, clock) = setup(QueuePolicy::default());
    let spec = spec(json!({"kind":"at","unix_ms":1000}));
    assert_eq!(
        q.create_schedule("one", spec.clone()).unwrap()["status"],
        "paused"
    );
    assert_eq!(q.tick_schedule("one", None).unwrap()["outcome"], "idle");
    q.enable_schedule("one", true).unwrap();
    let handles = (0..8)
        .map(|_| {
            let mut q = SessionStore::from_db(store.db.clone(), "tenant".into())
                .unwrap()
                .queue("work")
                .unwrap();
            let source = clock.clone();
            q.clock = Arc::new(move || Ok(source.load(Ordering::SeqCst)));
            std::thread::spawn(move || q.tick_schedule("one", Some(1000)).unwrap())
        })
        .collect::<Vec<_>>();
    let results = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .filter(|r| r["outcome"] == "published")
            .count(),
        1
    );
    assert_eq!(q.inspect().unwrap()["retained_jobs"], 1);
    assert_eq!(
        q.create_schedule("one", spec.clone()).unwrap()["published"],
        1
    );
    let mut changed = spec;
    changed.job.payload = json!({"different":true});
    assert!(
        q.create_schedule("one", changed)
            .unwrap_err()
            .to_string()
            .contains("IDEMPOTENCY")
    );
    let job = q.claim("worker", &["test".into()]).unwrap().unwrap();
    complete(&q, &job);
    assert_eq!(
        q.get_schedule("one").unwrap().unwrap()["status"],
        "completed"
    );
}
#[test]
fn missing_worker_results_block_future_occurrences_until_reconciled() {
    let (_dir, _store, q, clock) = setup(QueuePolicy::default());
    q.create_schedule(
        "repeat",
        spec(json!({"kind":"interval","every_ms":1000,"anchor_unix_ms":1000})),
    )
    .unwrap();
    q.enable_schedule("repeat", true).unwrap();
    q.tick_schedule("repeat", None).unwrap();
    let job = q.claim("worker", &["test".into()]).unwrap().unwrap();
    clock.store(40_000, Ordering::SeqCst);
    q.maintain().unwrap();
    assert_eq!(
        q.tick_schedule("repeat", None).unwrap()["outcome"],
        "blocked"
    );
    assert_eq!(q.get_schedule("repeat").unwrap().unwrap()["next_due"], 2000);
    q.reconcile(
        &job.id,
        job.execution_id.as_deref().unwrap(),
        Resolution::Completed {
            result: json!("external receipt"),
        },
    )
    .unwrap();
    let next = q.tick_schedule("repeat", None).unwrap();
    assert_eq!(next["outcome"], "published");
    assert_eq!(
        next["job"]["input"]["payload"]["schedule"]["scheduled_for"],
        2000
    );
    assert_eq!(
        q.get_schedule("repeat").unwrap().unwrap()["next_due"],
        41_000
    );
    assert_eq!(q.tick_schedule("repeat", None).unwrap()["outcome"], "idle");
    assert_eq!(q.inspect().unwrap()["retained_jobs"], 2);
}
#[test]
fn missed_dates_skip_or_fire_once_explicitly_and_never_burst() {
    let (_dir, _store, q, _clock) = setup(QueuePolicy::default());
    for (id, policy) in [
        ("skip", MisfirePolicy::Skip),
        ("fire", MisfirePolicy::FireOnce),
    ] {
        let mut spec = spec(json!({"kind":"at","unix_ms":1000}));
        spec.misfire_policy = policy;
        spec.misfire_grace_ms = 100;
        q.create_schedule(id, spec).unwrap();
        q.enable_schedule(id, true).unwrap();
    }
    assert_eq!(
        q.tick_schedule("skip", Some(100_000)).unwrap()["outcome"],
        "skipped"
    );
    assert_eq!(q.get_schedule("skip").unwrap().unwrap()["published"], 0);
    assert_eq!(
        q.tick_schedule("fire", Some(100_000)).unwrap()["outcome"],
        "published"
    );
    assert_eq!(
        q.tick_schedule("fire", Some(100_000)).unwrap()["outcome"],
        "idle"
    );
    assert!(q.tick_schedule("fire", Some(1000)).is_err());
}
#[test]
fn schedule_job_and_cursor_rollback_when_queue_capacity_or_payload_is_exhausted() {
    let (_dir, _store, q, _clock) = setup(QueuePolicy {
        max_jobs: 1,
        ..Default::default()
    });
    q.enqueue(input("unrelated")).unwrap();
    q.create_schedule("one", spec(json!({"kind":"at","unix_ms":1000})))
        .unwrap();
    q.enable_schedule("one", true).unwrap();
    assert!(q.tick_schedule("one", None).is_err());
    let state = q.get_schedule("one").unwrap().unwrap();
    assert_eq!(state["published"], 0);
    assert_eq!(state["next_due"], 1000);
    let (_dir, _store, q, _clock) = setup(QueuePolicy {
        max_payload_bytes: 50,
        ..Default::default()
    });
    q.create_schedule("one", spec(json!({"kind":"at","unix_ms":1000})))
        .unwrap();
    q.enable_schedule("one", true).unwrap();
    assert!(q.tick_schedule("one", None).is_err());
    assert_eq!(q.inspect().unwrap()["retained_jobs"], 0);
    assert_eq!(q.get_schedule("one").unwrap().unwrap()["published"], 0);
}
#[test]
fn max_runs_start_stop_pause_and_root_budget_are_never_refreshed() {
    let (_dir, store, q, clock) = setup(QueuePolicy::default());
    store
        .open_objective(
            "root",
            ObjectivePolicy {
                max_tool_calls: 1,
                ..Default::default()
            },
        )
        .unwrap();
    let mut spec = spec(json!({"kind":"interval","every_ms":1000,"anchor_unix_ms":0}));
    spec.start_at = Some(1500);
    spec.stop_at = Some(10_000);
    spec.max_runs = Some(2);
    spec.job.objective_id = Some("root".into());
    assert_eq!(
        q.create_schedule("rooted", spec.clone()).unwrap()["next_due"],
        2000
    );
    q.enable_schedule("rooted", true).unwrap();
    clock.store(2000, Ordering::SeqCst);
    q.tick_schedule("rooted", None).unwrap();
    assert_eq!(store.inspect_objective("root").unwrap()["tool_calls"], 0);
    let first = q.claim("worker", &["test".into()]).unwrap().unwrap();
    complete(&q, &first);
    q.enable_schedule("rooted", false).unwrap();
    clock.store(3000, Ordering::SeqCst);
    assert_eq!(q.tick_schedule("rooted", None).unwrap()["outcome"], "idle");
    q.enable_schedule("rooted", true).unwrap();
    q.tick_schedule("rooted", None).unwrap();
    assert!(q.claim("worker", &["test".into()]).is_err());
    assert_eq!(
        q.get_schedule("rooted").unwrap().unwrap()["status"],
        "completed"
    );
    assert_eq!(store.inspect_objective("root").unwrap()["tool_calls"], 1);
    assert_eq!(q.create_schedule("rooted", spec).unwrap()["published"], 2);
    assert_eq!(q.tick_schedule("rooted", None).unwrap()["outcome"], "idle");
}
#[test]
fn schedule_scopes_and_pagination_do_not_mix_tenants_or_queues() {
    let (_dir, store, q, _clock) = setup(QueuePolicy::default());
    for id in ["a", "b", "c"] {
        q.create_schedule(id, spec(json!({"kind":"at","unix_ms":1000})))
            .unwrap();
    }
    let first = q.list_schedules(2, None).unwrap();
    assert_eq!(first["items"].as_array().unwrap().len(), 2);
    assert_eq!(
        q.list_schedules(2, first["next_cursor"].as_str()).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let other = store
        .for_tenant("other".into())
        .unwrap()
        .queue("work")
        .unwrap();
    other.open(QueuePolicy::default()).unwrap();
    assert!(other.get_schedule("a").unwrap().is_none());
    assert!(
        other
            .list_schedules(2, first["next_cursor"].as_str())
            .is_err()
    );
    assert!(
        q.create_schedule("bad", spec(json!({"kind":"interval","every_ms":0})))
            .is_err()
    );
    let mut invalid = spec(json!({"kind":"at","unix_ms":1000}));
    invalid.job.idempotency_key = Some("reserved".into());
    assert!(q.create_schedule("bad", invalid).is_err());
    let mut invalid = spec(json!({"kind":"at","unix_ms":1000}));
    invalid.job.payload = json!({"schedule":{}});
    assert!(q.create_schedule("bad", invalid).is_err());
    assert!(q.tick_schedule("missing", Some(u64::MAX)).is_err());
}
#[test]
fn calendar_supports_aliases_seconds_dst_leap_years_and_strict_boundaries() {
    fn ms(date: &str) -> u64 {
        chrono::DateTime::parse_from_rfc3339(date)
            .unwrap()
            .timestamp_millis() as u64
    }
    fn cron(expression: &str, tz: &str) -> Schedule {
        Schedule::Cron {
            expression: expression.into(),
            timezone: tz.into(),
            dom_and_dow: false,
        }
    }
    assert_eq!(
        cron("0 8 * * MON-FRI", "America/Bogota")
            .next_after(ms("2026-11-13T13:00:00Z"))
            .unwrap(),
        Some(ms("2026-11-16T13:00:00Z"))
    );
    assert_eq!(
        cron("*/15 * * * * *", "UTC")
            .next_after(ms("2026-01-01T00:00:00Z"))
            .unwrap(),
        Some(ms("2026-01-01T00:00:15Z"))
    );
    assert_eq!(
        cron("30 2 * * *", "America/New_York")
            .next_after(ms("2026-03-08T00:00:00Z"))
            .unwrap(),
        Some(ms("2026-03-09T06:30:00Z"))
    );
    assert_eq!(
        cron("30 1 * * *", "America/New_York")
            .next_after(ms("2026-11-01T05:30:00Z"))
            .unwrap(),
        Some(ms("2026-11-02T06:30:00Z"))
    );
    assert_eq!(
        cron("0 0 29 feb *", "UTC")
            .next_after(ms("2096-02-29T00:00:00Z"))
            .unwrap(),
        Some(ms("2104-02-29T00:00:00Z"))
    );
    assert_eq!(
        cron("* * * * *", "UTC").next_at_or_after(0).unwrap(),
        Some(0)
    );
    assert_eq!(
        cron("0 0 30 2 *", "UTC")
            .next_after(ms("2026-01-01T00:00:00Z"))
            .unwrap(),
        None
    );
    for expression in [
        "*/0 * * * *",
        "80 * * * *",
        "* * * * * * *",
        "1-2-3 * * * *",
        "* * * */Jan *",
    ] {
        assert!(cron(expression, "UTC").validate().is_err(), "{expression}");
    }
    assert!(cron("* * * * *", "not/a/zone").validate().is_err());
}

#[test]
fn cron_day_steps_restrict_dates_even_when_the_range_starts_with_a_wildcard() {
    let from = chrono::DateTime::parse_from_rfc3339("2026-11-01T00:00:00Z")
        .unwrap()
        .timestamp_millis() as u64;
    let odd_days = Schedule::Cron {
        expression: "0 0 */2 * *".into(),
        timezone: "UTC".into(),
        dom_and_dow: false,
    };
    let expected = chrono::DateTime::parse_from_rfc3339("2026-11-03T00:00:00Z")
        .unwrap()
        .timestamp_millis() as u64;
    assert_eq!(odd_days.next_after(from).unwrap(), Some(expected));
}

#[tokio::test]
async fn native_scheduler_loop_only_publishes_and_stops_without_discarding_queued_work() {
    let (_dir, _store, q, _clock) = setup(QueuePolicy::default());
    q.create_schedule("one", spec(json!({"kind":"at","unix_ms":1000})))
        .unwrap();
    q.enable_schedule("one", true).unwrap();
    let token = crate::provider::CancellationToken::new();
    let stop = token.clone();
    let worker = q.clone();
    let task = tokio::spawn(async move { worker.run_scheduler(10, stop).await });
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if q.get_schedule("one").unwrap().unwrap()["published"] == 1 {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    token.cancel();
    task.await.unwrap().unwrap();
    let job = &q.list(50, None).unwrap()["items"][0];
    assert_eq!(job["status"], "pending");
    assert_eq!(job["attempts"], 0);
}
