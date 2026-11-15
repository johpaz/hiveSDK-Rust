use hive_agent_core::{
    Action, Engine, Error, ModelReply, Tool, ToolCall, Usage,
    objective::ObjectivePolicy,
    sessions::{OperationResult, SessionStore},
};
use serde_json::json;

fn policy() -> ObjectivePolicy {
    ObjectivePolicy {
        max_model_calls: 3,
        max_tool_calls: 1,
        max_output_tokens: 10,
        max_total_output_tokens: 15,
        max_depth: 1,
        ..Default::default()
    }
}
fn engine() -> Engine {
    let mut engine = Engine::new("test".into(), None, vec![Tool { name: "work".into(),
        description: "Work".into(), parameters: json!({"type":"object","required":["n"],"properties":{"n":{"type":"integer"}}}),
        allow_repeated_results: false }], 8).unwrap();
    engine.bind_objective("root".into()).unwrap();
    engine
}
fn reply(content: Option<&str>, arguments: Option<serde_json::Value>) -> ModelReply {
    ModelReply {
        content: content.map(Into::into),
        provider_state: None,
        tool_calls: arguments
            .into_iter()
            .map(|arguments| ToolCall {
                id: "c".into(),
                name: "work".into(),
                arguments,
            })
            .collect(),
        usage: Some(Usage {
            input_tokens: 4,
            output_tokens: 3,
        }),
    }
}

#[test]
fn root_reopens_without_refreshing_policy_balances_or_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let deadline;
    {
        let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
        let mut policy = policy();
        policy.max_run_ms = Some(60_000);
        deadline =
            store.open_objective("root", policy.clone()).unwrap()["deadline_unix_ms"].clone();
        store
            .objective_command("root", r#"{"op":"model","depth":0,"cap":10}"#)
            .unwrap();
        assert_eq!(
            store.open_objective("root", policy).unwrap()["model_calls"],
            1
        );
    }
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let mut policy = policy();
    policy.max_run_ms = Some(60_000);
    let snapshot = store.open_objective("root", policy).unwrap();
    assert_eq!(snapshot["deadline_unix_ms"], deadline);
    assert_eq!(snapshot["reservations"]["1"], 10);
    assert!(matches!(
        store.open_objective("root", ObjectivePolicy::default()),
        Err(Error::SessionConfigMismatch)
    ));
    let other = store.for_tenant("other".into()).unwrap();
    assert!(other.inspect_objective("root").is_err());
    assert_eq!(
        other
            .open_objective("root", ObjectivePolicy::default())
            .unwrap()["model_calls"],
        0
    );
}

#[test]
fn session_model_claim_and_reservation_are_confirmed_and_settled_together() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    store.open_objective("root", policy()).unwrap();
    let start = store
        .begin_run("a", "s", 0, engine(), "Work".into())
        .unwrap();
    let claim = store.claim("a", "s", start.revision).unwrap();
    let op = claim.operation.unwrap();
    assert_eq!(op.objective_permit, Some(1));
    assert!(
        matches!(claim.action, Action::Model { request } if request.max_output_tokens == Some(10))
    );
    assert_eq!(
        store.inspect_objective("root").unwrap()["reservations"]["1"],
        10
    );
    // A stale claim cannot consume another permit or change the checkpoint.
    assert!(matches!(
        store.claim("a", "s", start.revision),
        Err(Error::SessionConflict)
    ));
    assert_eq!(store.inspect_objective("root").unwrap()["model_calls"], 1);
    let frame = store
        .advance(
            "a",
            "s",
            claim.revision,
            &op.execution_id,
            OperationResult::Model {
                reply: reply(Some("done"), None),
            },
        )
        .unwrap();
    assert!(matches!(frame.action, Action::Complete { .. }));
    let snapshot = store.inspect_objective("root").unwrap();
    assert_eq!(snapshot["output_tokens"], 3);
    assert_eq!(snapshot["input_tokens"], 4);
    assert_eq!(snapshot["reservations"], json!({}));
    assert!(
        store
            .advance(
                "a",
                "s",
                claim.revision,
                &op.execution_id,
                OperationResult::Model {
                    reply: reply(Some("duplicate"), None)
                }
            )
            .is_err()
    );
    assert_eq!(store.inspect_objective("root").unwrap()["output_tokens"], 3);
}

#[test]
fn reopening_an_uncertain_model_retains_permit_until_verified_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let (revision, execution);
    {
        let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
        store.open_objective("root", policy()).unwrap();
        let start = store
            .begin_run("a", "s", 0, engine(), "Work".into())
            .unwrap();
        let claim = store.claim("a", "s", start.revision).unwrap();
        revision = claim.revision;
        execution = claim.operation.unwrap().execution_id;
    }
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    store.open_objective("root", policy()).unwrap();
    assert!(matches!(
        store.load_run("a", "s", &engine()),
        Err(Error::SessionOperationUncertain)
    ));
    assert_eq!(
        store.inspect_objective("root").unwrap()["reservations"]["1"],
        10
    );
    store
        .advance(
            "a",
            "s",
            revision,
            &execution,
            OperationResult::Model {
                reply: reply(Some("verified"), None),
            },
        )
        .unwrap();
    let snapshot = store.inspect_objective("root").unwrap();
    assert_eq!(snapshot["model_calls"], 1);
    assert_eq!(snapshot["output_tokens"], 3);
    assert_eq!(snapshot["reservations"], json!({}));
    let checkpoint = store.load_run("a", "s", &engine()).unwrap();
    assert!(matches!(checkpoint.action, Action::Complete { content, .. } if content == "verified"));
}

#[test]
fn rejected_tools_and_actual_tools_share_the_same_durable_allowance() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    store.open_objective("root", policy()).unwrap();
    let start = store
        .begin_run("a", "s", 0, engine(), "Work".into())
        .unwrap();
    let claim = store.claim("a", "s", start.revision).unwrap();
    let frame = store
        .advance(
            "a",
            "s",
            claim.revision,
            &claim.operation.unwrap().execution_id,
            OperationResult::Model {
                reply: reply(None, Some(json!({}))),
            },
        )
        .unwrap();
    assert!(matches!(frame.action, Action::ToolRejected { .. }));
    let frame = store.reject("a", "s", frame.revision).unwrap();
    assert_eq!(store.inspect_objective("root").unwrap()["tool_calls"], 1);
    let claim = store.claim("a", "s", frame.revision).unwrap();
    let frame = store
        .advance(
            "a",
            "s",
            claim.revision,
            &claim.operation.unwrap().execution_id,
            OperationResult::Model {
                reply: reply(None, Some(json!({"n":1}))),
            },
        )
        .unwrap();
    assert!(matches!(
        store.claim("a", "s", frame.revision),
        Err(Error::ObjectiveLimit(_))
    ));
    let inspection = store.inspect("a", "s", None).unwrap();
    assert!(inspection["run"]["in_flight"].is_null());
    assert_eq!(inspection["run"]["tools"].as_array().unwrap().len(), 1);
}

#[test]
fn root_binding_and_checkpoint_depth_cannot_be_silently_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    store.open_objective("root", policy()).unwrap();
    let frame = store
        .begin_run_at_depth("a", "child", 0, engine(), "Work".into(), 1)
        .unwrap();
    assert_eq!(frame.objective_depth, 1);
    assert_eq!(
        store
            .load_run("a", "child", &engine())
            .unwrap()
            .objective_depth,
        1
    );
    let unbound = Engine::new("test".into(), None, vec![], 8).unwrap();
    assert!(matches!(
        store.load("a", "child", &unbound),
        Err(Error::SessionConfigMismatch)
    ));
    assert!(matches!(
        store.begin_run_at_depth("a", "deep", 0, engine(), "Work".into(), 2),
        Err(Error::ObjectiveLimit(_))
    ));
    assert!(store.inspect("a", "deep", None).unwrap().is_null());
    assert_eq!(store.inspect_objective("root").unwrap()["model_calls"], 0);
}

#[test]
fn siblings_cannot_refresh_or_multiply_the_root_model_budget() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let mut policy = policy();
    policy.max_model_calls = 1;
    store.open_objective("root", policy).unwrap();
    let a = store
        .begin_run("a", "one", 0, engine(), "Work".into())
        .unwrap();
    let b = store
        .begin_run("a", "two", 0, engine(), "Work".into())
        .unwrap();
    store.claim("a", "one", a.revision).unwrap();
    assert!(matches!(
        store.claim("a", "two", b.revision),
        Err(Error::ObjectiveLimit(_))
    ));
    assert_eq!(store.inspect_objective("root").unwrap()["model_calls"], 1);
    assert!(store.inspect("a", "two", None).unwrap()["run"]["in_flight"].is_null());
}

#[test]
fn concurrent_session_claims_cannot_commit_a_partial_root_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let mut policy = policy();
    policy.max_model_calls = 1;
    store.open_objective("root", policy).unwrap();
    let starts: Vec<_> = (0..8)
        .map(|n| {
            let id = format!("s{n}");
            let revision = store
                .begin_run("a", &id, 0, engine(), "Work".into())
                .unwrap()
                .revision;
            (id, revision)
        })
        .collect();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = starts
        .iter()
        .map(|(id, revision)| {
            let store = store.clone();
            let barrier = barrier.clone();
            let id = id.clone();
            let revision = *revision;
            std::thread::spawn(move || {
                barrier.wait();
                store.claim("a", &id, revision)
            })
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .map(|h| h.join().unwrap().is_ok())
            .filter(|success| *success)
            .count(),
        1
    );
    let claimed = starts
        .iter()
        .filter(|(id, _)| !store.inspect("a", id, None).unwrap()["run"]["in_flight"].is_null())
        .count();
    assert_eq!(claimed, 1);
    let snapshot = store.inspect_objective("root").unwrap();
    assert_eq!(snapshot["model_calls"], 1);
    assert_eq!(snapshot["reservations"], json!({"1":10}));
}

#[test]
fn unknown_usage_is_checkpointed_and_blocks_other_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    store.open_objective("root", policy()).unwrap();
    let start = store
        .begin_run("a", "s", 0, engine(), "Work".into())
        .unwrap();
    let claim = store.claim("a", "s", start.revision).unwrap();
    let mut response = reply(Some("unmetered"), None);
    response.usage = None;
    let result = store
        .advance(
            "a",
            "s",
            claim.revision,
            &claim.operation.unwrap().execution_id,
            OperationResult::Model { reply: response },
        )
        .unwrap();
    assert!(matches!(
        result.action,
        Action::Stopped {
            reason: hive_agent_core::StopReason::UsageUnavailable,
            ..
        }
    ));
    let snapshot = store.inspect_objective("root").unwrap();
    assert_eq!(snapshot["stopped"], "usage_unavailable");
    assert_eq!(snapshot["reservations"], json!({"1":10}));
    assert!(matches!(
        store.begin_run("a", "other", 0, engine(), "Again".into()),
        Err(Error::ObjectiveLimit(_))
    ));
}

#[test]
fn simultaneous_confirmed_replies_settle_without_losing_any_usage() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    store
        .open_objective(
            "root",
            ObjectivePolicy {
                max_model_calls: 8,
                max_output_tokens: 3,
                max_total_output_tokens: 24,
                ..Default::default()
            },
        )
        .unwrap();
    let claims: Vec<_> = (0..8)
        .map(|n| {
            let id = format!("s{n}");
            let start = store
                .begin_run("a", &id, 0, engine(), "Work".into())
                .unwrap();
            let claim = store.claim("a", &id, start.revision).unwrap();
            (id, claim.revision, claim.operation.unwrap().execution_id)
        })
        .collect();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let handles: Vec<_> = claims
        .into_iter()
        .map(|(id, revision, execution)| {
            let store = store.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                store
                    .advance(
                        "a",
                        &id,
                        revision,
                        &execution,
                        OperationResult::Model {
                            reply: reply(Some("done"), None),
                        },
                    )
                    .unwrap()
            })
        })
        .collect();
    for handle in handles {
        assert!(matches!(
            handle.join().unwrap().action,
            Action::Complete { .. }
        ));
    }
    let snapshot = store.inspect_objective("root").unwrap();
    assert_eq!(snapshot["model_calls"], 8);
    assert_eq!(snapshot["output_tokens"], 24);
    assert_eq!(snapshot["input_tokens"], 32);
    assert_eq!(snapshot["reservations"], json!({}));
}
