use super::tests::setup;
use super::*;
use crate::objective::ObjectivePolicy;

fn spec(extra: Value) -> GoalSpec {
    let mut base = json!({"objective_id":"root","goal":"Ship the report","planner_agent_id":"planner","coordinator_agent_id":"lead","max_rounds":2});
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().cloned().unwrap_or_default());
    serde_json::from_value(base).unwrap()
}
fn start(extra: Value) -> (tempfile::TempDir, DurableQueue, Value) {
    let (dir, store, q, _) = setup(QueuePolicy::default());
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    let goal = q.create_goal("g", spec(extra)).unwrap();
    (dir, q, goal)
}
/// Claim the one pending job of `kind` and finish it with `result`.
fn run(q: &DurableQueue, kind: &str, result: Value) -> Job {
    let job = q
        .claim("w", &[kind.into()])
        .unwrap()
        .unwrap_or_else(|| panic!("no pending job of {kind}"));
    q.finish(
        &job.id,
        job.execution_id.as_deref().unwrap(),
        JobOutcome::Completed { result },
    )
    .unwrap()
}
fn plan(tasks: Value) -> Value {
    json!({"tasks":tasks})
}
fn two_tasks() -> Value {
    plan(
        json!([{"id":"a","specialist_id":"research","prompt":"Find evidence"},{"id":"b","specialist_id":"writer","prompt":"Draft"}]),
    )
}
fn kind(goal: &Value, path: &[&str]) -> String {
    path.iter()
        .fold(&goal["kinds"], |value, key| match key.parse::<usize>() {
            Ok(index) => &value[index],
            Err(_) => &value[*key],
        })
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn a_goal_runs_plan_delegate_synthesize_review_to_acceptance() {
    let (_dir, q, goal) = start(json!({}));
    assert_eq!(
        (goal["status"].clone(), goal["round"].clone()),
        (json!("planning"), json!(1))
    );
    assert_eq!(
        q.goal_tick("g").unwrap()["status"],
        "planning",
        "nothing happens until the planner job has a result"
    );
    run(&q, &kind(&goal, &["plan"]), two_tasks());
    let goal = q.goal_tick("g").unwrap();
    assert_eq!(goal["status"], "running");
    assert_eq!(goal["group"]["tasks"].as_array().unwrap().len(), 2);
    let specialist = kind(&goal, &["rounds", "0", "specialist"]);
    run(&q, &specialist, json!({"text":"evidence"}));
    assert_eq!(
        q.goal_tick("g").unwrap()["status"],
        "running",
        "fan-in waits for every task"
    );
    run(&q, &specialist, json!({"text":"draft"}));
    let goal = q.goal_tick("g").unwrap();
    assert_eq!(goal["rounds"][0]["synthesis_job"]["status"], "pending");
    run(
        &q,
        &kind(&goal, &["rounds", "0", "synthesis"]),
        json!({"text":"all good"}),
    );
    let goal = q.goal_tick("g").unwrap();
    assert_eq!(goal["status"], "reviewing");
    run(
        &q,
        &kind(&goal, &["review"]),
        json!({"accepted":true,"evidence":"report.pdf exists","feedback":""}),
    );
    let goal = q.goal_tick("g").unwrap();
    assert_eq!(goal["status"], "accepted");
    assert_eq!(
        goal["rounds"][0]["verdict"]["evidence"],
        "report.pdf exists"
    );
    let jobs = q.inspect().unwrap()["retained_jobs"].clone();
    assert_eq!(jobs, 1 + 2 + 1 + 1, "plan, two tasks, synthesis, review");
    assert_eq!(q.goal_tick("g").unwrap()["status"], "accepted");
    assert_eq!(
        q.inspect().unwrap()["retained_jobs"],
        jobs,
        "a terminal goal enqueues nothing"
    );
}

#[test]
fn a_rejected_round_replans_with_the_feedback_and_the_rounds_are_bounded() {
    let (_dir, q, goal) = start(json!({}));
    let one = plan(json!([{"id":"a","specialist_id":"research","prompt":"Try"}]));
    let mut current = goal;
    for round in 1..=2 {
        run(&q, &kind(&current, &["plan"]), one.clone());
        current = q.goal_tick("g").unwrap();
        run(
            &q,
            &kind(
                &current,
                &["rounds", &(round - 1).to_string(), "specialist"],
            ),
            json!({"text":"x"}),
        );
        current = q.goal_tick("g").unwrap();
        run(
            &q,
            &kind(&current, &["rounds", &(round - 1).to_string(), "synthesis"]),
            json!({"text":"s"}),
        );
        current = q.goal_tick("g").unwrap();
        assert_eq!(current["status"], "reviewing", "round {round}");
        run(
            &q,
            &kind(&current, &["review"]),
            json!({"accepted":false,"feedback":format!("missing chart {round}")}),
        );
        current = q.goal_tick("g").unwrap();
        if round == 1 {
            assert_eq!(
                (current["status"].clone(), current["round"].clone()),
                (json!("planning"), json!(2))
            );
            let prompt = q
                .get(current["rounds"][1]["plan_job"]["id"].as_str().unwrap())
                .unwrap()
                .unwrap()
                .input
                .payload["prompt"]
                .as_str()
                .unwrap()
                .to_owned();
            assert!(
                prompt.contains("missing chart 1") && prompt.contains("NOT accepted"),
                "{prompt}"
            );
        }
    }
    assert_eq!(
        (current["status"].clone(), current["error"].clone()),
        (json!("failed"), json!("MAX_ROUNDS_EXHAUSTED"))
    );
    assert_eq!(current["rounds"].as_array().unwrap().len(), 2);
}

#[test]
fn bad_plans_and_reviews_fail_the_goal_instead_of_being_guessed() {
    for (extra, result, reason) in [
        (json!({}), json!({"tasks":[]}), "INVALID_PLAN"),
        (json!({}), json!({"text":"I will do it"}), "INVALID_PLAN"),
        (
            json!({}),
            plan(
                json!([{"id":"a","specialist_id":"x","prompt":"p"},{"id":"a","specialist_id":"x","prompt":"p"}]),
            ),
            "duplicate task id",
        ),
        (
            json!({"max_tasks_per_round":1}),
            two_tasks(),
            "INVALID_PLAN",
        ),
        (
            json!({"specialist_ids":["research"]}),
            two_tasks(),
            "not allowed",
        ),
        (
            json!({}),
            plan(json!([{"id":"a","specialist_id":"x","prompt":"  "}])),
            "INVALID_PLAN",
        ),
    ] {
        let (_dir, q, goal) = start(extra);
        run(&q, &kind(&goal, &["plan"]), result);
        let goal = q.goal_tick("g").unwrap();
        assert_eq!(goal["status"], "failed");
        assert!(goal["error"].as_str().unwrap().contains(reason), "{goal}");
        assert!(
            goal["group"].is_null(),
            "no delegation group exists for a rejected plan"
        );
    }
    for (verdict, reason) in [
        (json!({"accepted":true}), "non-empty evidence"),
        (json!({"accepted":"yes"}), "boolean"),
        (json!("accepted"), "boolean"),
    ] {
        let (_dir, q, goal) = start(json!({}));
        run(&q, &kind(&goal, &["plan"]), two_tasks());
        let goal = q.goal_tick("g").unwrap();
        let specialist = kind(&goal, &["rounds", "0", "specialist"]);
        run(&q, &specialist, json!(1));
        run(&q, &specialist, json!(2));
        let goal = q.goal_tick("g").unwrap();
        run(&q, &kind(&goal, &["rounds", "0", "synthesis"]), json!("s"));
        let goal = q.goal_tick("g").unwrap();
        run(&q, &kind(&goal, &["review"]), verdict);
        let goal = q.goal_tick("g").unwrap();
        assert_eq!(goal["status"], "failed");
        assert!(goal["error"].as_str().unwrap().contains(reason), "{goal}");
    }
}

#[test]
fn a_failed_stage_fails_the_goal_and_an_uncertain_one_blocks_it_without_replay() {
    let (_dir, q, goal) = start(json!({}));
    let job = q.claim("w", &[kind(&goal, &["plan"])]).unwrap().unwrap();
    q.finish(
        &job.id,
        job.execution_id.as_deref().unwrap(),
        JobOutcome::Failed {
            error: "model down".into(),
            retryable: false,
        },
    )
    .unwrap();
    let failed = q.goal_tick("g").unwrap();
    assert_eq!(failed["status"], "failed");
    assert!(failed["error"].as_str().unwrap().contains("model down"));

    let (_dir, store, q, clock) = setup(QueuePolicy {
        lease_ms: 100,
        ..Default::default()
    });
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    let goal = q.create_goal("g", spec(json!({}))).unwrap();
    let job = q.claim("w", &[kind(&goal, &["plan"])]).unwrap().unwrap();
    // The worker disappears: after the lease the job's outcome is unknown.
    clock.fetch_add(10_000, std::sync::atomic::Ordering::SeqCst);
    q.maintain().unwrap();
    assert_eq!(
        q.get(&job.id).unwrap().unwrap().status,
        JobStatus::Uncertain
    );
    let blocked = q.goal_tick("g").unwrap();
    assert_eq!(
        (
            blocked["status"].clone(),
            blocked["blocked_uncertain"].clone()
        ),
        (json!("planning"), json!(true))
    );
    assert_eq!(
        q.inspect().unwrap()["retained_jobs"],
        1,
        "the uncertain job is never replayed"
    );
}

#[test]
fn creation_is_idempotent_and_conflicting_specs_are_refused() {
    let (_dir, q, goal) = start(json!({}));
    assert_eq!(
        q.create_goal("g", spec(json!({}))).unwrap()["rounds"],
        goal["rounds"]
    );
    assert_eq!(q.inspect().unwrap()["retained_jobs"], 1);
    assert!(
        q.create_goal("g", spec(json!({"goal":"Something else"})))
            .unwrap_err()
            .to_string()
            .contains("GOAL_IDEMPOTENCY_CONFLICT")
    );
    for bad in [
        json!({"goal":"  "}),
        json!({"max_rounds":0}),
        json!({"max_rounds":9}),
        json!({"max_tasks_per_round":65}),
        json!({"depth":0}),
        json!({"planner_agent_id":""}),
    ] {
        assert!(q.create_goal("other", spec(bad.clone())).is_err(), "{bad}");
    }
    assert!(q.create_goal(&"x".repeat(201), spec(json!({}))).is_err());
    assert!(
        q.goal("missing")
            .unwrap_err()
            .to_string()
            .contains("GOAL_NOT_FOUND")
    );
    assert_eq!(
        q.inspect().unwrap()["retained_jobs"],
        1,
        "refused goals enqueue nothing"
    );
}

#[test]
fn concurrent_ticks_apply_each_transition_once() {
    let (_dir, q, goal) = start(json!({}));
    run(&q, &kind(&goal, &["plan"]), two_tasks());
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let q = q.clone();
            std::thread::spawn(move || q.goal_tick("g").unwrap())
        })
        .collect();
    for handle in handles {
        assert_eq!(handle.join().unwrap()["status"], "running");
    }
    assert_eq!(
        q.inspect().unwrap()["retained_jobs"],
        3,
        "one plan and two tasks, however many ticks raced"
    );
    assert_eq!(q.goal("g").unwrap()["rounds"].as_array().unwrap().len(), 1);
}

#[test]
fn cancelling_stops_pending_work_and_later_ticks_do_nothing() {
    let (_dir, q, goal) = start(json!({}));
    run(&q, &kind(&goal, &["plan"]), two_tasks());
    let running = q.goal_tick("g").unwrap();
    let cancelled = q.cancel_goal("g").unwrap();
    assert_eq!(cancelled["status"], "cancelled");
    for task in running["group"]["tasks"].as_array().unwrap() {
        assert_eq!(
            q.get(task["job"]["id"].as_str().unwrap())
                .unwrap()
                .unwrap()
                .status,
            JobStatus::Cancelled
        );
    }
    let before = q.inspect().unwrap()["retained_jobs"].clone();
    assert_eq!(q.goal_tick("g").unwrap()["status"], "cancelled");
    assert_eq!(q.cancel_goal("g").unwrap()["status"], "cancelled");
    assert_eq!(q.inspect().unwrap()["retained_jobs"], before);
    // A goal cancelled while still planning cancels its planner job too.
    let (_dir, q, goal) = start(json!({}));
    q.cancel_goal("g").unwrap();
    assert!(q.claim("w", &[kind(&goal, &["plan"])]).unwrap().is_none());
}

#[test]
fn commands_drive_goals_through_the_json_dispatch() {
    let (_dir, q, goal) = start(json!({}));
    let report = q.dispatch(json!({"op":"goal_get","goal_id":"g"})).unwrap();
    assert_eq!(report["id"], goal["id"]);
    run(&q, &kind(&goal, &["plan"]), two_tasks());
    assert_eq!(
        q.dispatch(json!({"op":"goal_tick","goal_id":"g"})).unwrap()["status"],
        "running"
    );
    assert_eq!(
        q.dispatch(json!({"op":"goal_cancel","goal_id":"g"}))
            .unwrap()["status"],
        "cancelled"
    );
    let made = q.dispatch(json!({"op":"goal_create","goal_id":"h","spec":{"objective_id":"root","goal":"x","planner_agent_id":"p","coordinator_agent_id":"c"}})).unwrap();
    assert_eq!(
        (
            made["spec"]["max_rounds"].clone(),
            made["spec"]["max_tasks_per_round"].clone()
        ),
        (json!(3), json!(8))
    );
}
