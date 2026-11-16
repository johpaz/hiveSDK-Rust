use hive_agent_core::provider::request_body;
use hive_agent_core::sessions::{OperationResult, SessionStore};
use hive_agent_core::{Action, Engine, LoopPolicy, ModelReply, StopReason, Tool, ToolCall, Usage};
use serde_json::json;

fn initial(policy: LoopPolicy) -> Engine {
    Engine::with_policy(
        "test".into(),
        None,
        vec![Tool {
            name: "read".into(),
            description: "Read".into(),
            parameters: json!({"type":"object"}),
            allow_repeated_results: false,
        }],
        8,
        policy,
    )
    .unwrap()
}
fn policy() -> LoopPolicy {
    LoopPolicy {
        max_output_tokens: Some(4),
        max_total_output_tokens: Some(5),
        ..Default::default()
    }
}
fn reply(output: u64, tool: bool) -> ModelReply {
    ModelReply {
        provider_state: None,
        content: if tool { None } else { Some("done".into()) },
        tool_calls: if tool {
            vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: json!({}),
            }]
        } else {
            vec![]
        },
        usage: Some(Usage {
            input_tokens: 10,
            output_tokens: output,
        }),
    }
}
fn request(state: &Engine) -> &hive_agent_core::ModelRequest {
    let Action::Model { request } = state.action() else {
        panic!("expected model")
    };
    request
}
fn expire(state: Engine) -> Engine {
    let mut value = serde_json::to_value(state).unwrap();
    value["deadline_unix_ms"] = json!(1);
    serde_json::from_value(value).unwrap()
}

#[test]
fn reserves_before_dispatch_reconciles_and_clamps_the_next_request() {
    let mut state = initial(policy());
    state.start("read".into()).unwrap();
    assert_eq!(request(&state).max_output_tokens, Some(4));
    assert_eq!(
        serde_json::to_value(&state).unwrap()["stats"]["reserved_output_tokens"],
        4
    );
    assert_eq!(
        request_body(request(&state)).unwrap()["max_completion_tokens"],
        4
    );
    state.accept_reply(reply(3, true)).unwrap();
    state.accept_tool("c", json!(1)).unwrap();
    assert_eq!(request(&state).max_output_tokens, Some(2));
    state.preflight().unwrap();
    state.preflight().unwrap();
    assert_eq!(
        serde_json::to_value(&state).unwrap()["stats"]["reserved_output_tokens"],
        2
    );
    state.accept_reply(reply(2, false)).unwrap();
    assert!(
        matches!(state.action(),Action::Complete{stats,..} if stats.output_tokens==5&&stats.reserved_output_tokens==0)
    );
}

#[test]
fn exhausted_budget_stops_before_a_tool_and_provider_overrun_is_reported() {
    let mut state = initial(LoopPolicy {
        max_total_output_tokens: Some(3),
        ..Default::default()
    });
    state.start("read".into()).unwrap();
    state.accept_reply(reply(3, true)).unwrap();
    assert!(
        matches!(state.action(),Action::Stopped{reason:StopReason::OutputLimit,stats} if stats.tool_calls==0)
    );
    let mut state = initial(policy());
    state.start("read".into()).unwrap();
    state.accept_reply(reply(6, false)).unwrap();
    assert!(
        matches!(state.action(),Action::Stopped{reason:StopReason::OutputLimit,stats} if stats.output_tokens==6)
    );
}

#[test]
fn missing_usage_keeps_the_reservation_instead_of_refunding_unknown_spend() {
    let mut state = initial(policy());
    state.start("read".into()).unwrap();
    let mut response = reply(1, false);
    response.usage = None;
    state.accept_reply(response).unwrap();
    assert!(
        matches!(state.action(),Action::Stopped{reason:StopReason::UsageUnavailable,stats}
        if stats.reserved_output_tokens==4&&stats.unmetered_model_calls==1)
    );
}

#[test]
fn output_reservation_survives_a_crash_and_is_reconciled_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let frame = store
        .begin_run("a", "s", 0, initial(policy()), "read".into())
        .unwrap();
    let claim = store.claim("a", "s", frame.revision).unwrap();
    let operation = claim.operation.unwrap();
    drop(store);
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let inspection = store.inspect("a", "s", None).unwrap();
    assert_eq!(
        inspection["run"]["state"]["stats"]["reserved_output_tokens"],
        4
    );
    assert!(store.load_run("a", "s", &initial(policy())).is_err());
    let done = store
        .advance(
            "a",
            "s",
            claim.revision,
            &operation.execution_id,
            OperationResult::Model {
                reply: reply(3, false),
            },
        )
        .unwrap();
    assert!(
        matches!(done.action,Action::Complete{stats,..} if stats.output_tokens==3&&stats.reserved_output_tokens==0)
    );
    assert!(
        store
            .advance(
                "a",
                "s",
                claim.revision,
                &operation.execution_id,
                OperationResult::Model {
                    reply: reply(3, false)
                }
            )
            .is_err()
    );
}

#[test]
fn expired_clean_checkpoint_stops_without_restarting_the_clock() {
    let mut state = initial(LoopPolicy {
        max_run_ms: Some(60_000),
        ..Default::default()
    });
    state.start("read".into()).unwrap();
    let mut state = expire(state);
    state.preflight().unwrap();
    assert!(matches!(
        state.action(),
        Action::Stopped {
            reason: StopReason::DeadlineExceeded,
            ..
        }
    ));
}

#[test]
fn late_known_tool_result_is_recorded_before_deadline_stop() {
    let mut state = initial(LoopPolicy {
        max_run_ms: Some(60_000),
        ..Default::default()
    });
    state.start("read".into()).unwrap();
    state.accept_reply(reply(1, true)).unwrap();
    let mut state = expire(state);
    state.accept_tool("c", json!({"known":true})).unwrap();
    assert!(
        matches!(state.action(),Action::Stopped{reason:StopReason::DeadlineExceeded,stats} if stats.tool_calls==1)
    );
    assert!(
        state
            .messages()
            .last()
            .unwrap()
            .content
            .as_ref()
            .unwrap()
            .contains("known")
    );
}

#[test]
fn clean_resume_preserves_deadline_and_reservation() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let p = LoopPolicy {
        max_run_ms: Some(60_000),
        ..policy()
    };
    let frame = store
        .begin_run("a", "s", 0, initial(p), "read".into())
        .unwrap();
    let old = serde_json::to_value(&frame.state).unwrap();
    let resumed = store.resume_run("a", "s", frame.revision).unwrap();
    assert_eq!(serde_json::to_value(resumed.state).unwrap(), old);
}

#[test]
fn invalid_limits_fail_before_start() {
    for p in [
        LoopPolicy {
            max_run_ms: Some(0),
            ..Default::default()
        },
        LoopPolicy {
            max_output_tokens: Some(0),
            ..Default::default()
        },
        LoopPolicy {
            max_total_output_tokens: Some(0),
            ..Default::default()
        },
    ] {
        assert!(Engine::with_policy("test".into(), None, vec![], 8, p).is_err());
    }
    let mut state = initial(LoopPolicy {
        max_run_ms: Some(u64::MAX),
        ..Default::default()
    });
    assert!(state.start("read".into()).is_err());
}

#[test]
fn expiration_between_checkpoint_and_claim_stops_without_claiming_an_operation() {
    use hivedb_core::{HiveDB, PutOptions};
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let policy = LoopPolicy {
        max_run_ms: Some(60_000),
        ..policy()
    };
    store
        .begin_run("a", "s", 0, initial(policy.clone()), "read".into())
        .unwrap();
    drop(store);
    let db = HiveDB::open(dir.path()).unwrap();
    let key = serde_json::to_string(&("t", "a", "s")).unwrap();
    let mut record = db
        .col_get("hive_sdk_rust_sessions_v1", &key)
        .unwrap()
        .unwrap()
        .doc;
    record["run"]["state"]["deadline_unix_ms"] = json!(1);
    db.col_put(
        "hive_sdk_rust_sessions_v1",
        &key,
        &record,
        PutOptions::default(),
    )
    .unwrap();
    drop(db);
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let loaded = store.load_run("a", "s", &initial(policy)).unwrap();
    let stopped = store.claim("a", "s", loaded.revision).unwrap();
    assert!(stopped.operation.is_none());
    assert!(
        matches!(stopped.action,Action::Stopped{reason:StopReason::DeadlineExceeded,stats}
        if stats.model_calls==0&&stats.reserved_output_tokens==0)
    );
    store
        .commit("a", "s", stopped.revision, stopped.state)
        .unwrap();
}
