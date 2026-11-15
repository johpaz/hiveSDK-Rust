use hive_agent_core::sessions::{OperationResult, SessionStore};
use hive_agent_core::{Action, Engine, Error, ModelReply, Tool, ToolCall, Usage};
use serde_json::json;

fn initial() -> Engine {
    Engine::new(
        "test".into(),
        None,
        vec![Tool {
            name: "pay".into(),
            description: "Payment".into(),
            parameters: json!({"type":"object"}),
            allow_repeated_results: false,
        }],
        8,
    )
    .unwrap()
}
fn tool_reply() -> ModelReply {
    ModelReply {
        provider_state: None,
        content: None,
        tool_calls: vec![ToolCall {
            id: "call-1".into(),
            name: "pay".into(),
            arguments: json!({"amount":10}),
        }],
        usage: Some(Usage {
            input_tokens: 5,
            output_tokens: 3,
        }),
    }
}
fn final_reply() -> ModelReply {
    ModelReply {
        provider_state: None,
        content: Some("Paid".into()),
        tool_calls: vec![],
        usage: Some(Usage {
            input_tokens: 7,
            output_tokens: 2,
        }),
    }
}

#[test]
fn confirmed_tool_checkpoint_resumes_without_resetting_usage_or_replaying_tool() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let start = store
        .begin_run("a", "s", 0, initial(), "Pay".into())
        .unwrap();
    let claim = store.claim("a", "s", start.revision).unwrap();
    let frame = store
        .advance(
            "a",
            "s",
            claim.revision,
            &claim.operation.unwrap().execution_id,
            OperationResult::Model {
                reply: tool_reply(),
            },
        )
        .unwrap();
    let claim = store.claim("a", "s", frame.revision).unwrap();
    let frame = store
        .advance(
            "a",
            "s",
            claim.revision,
            &claim.operation.unwrap().execution_id,
            OperationResult::Tool {
                result: json!({"receipt":"r1"}),
            },
        )
        .unwrap();
    drop(store);
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let loaded = store.load_run("a", "s", &initial()).unwrap();
    assert_eq!(loaded.run_id, start.run_id);
    assert_eq!(loaded.revision, frame.revision);
    let resumed = store.resume_run("a", "s", loaded.revision).unwrap();
    let Action::Model { request } = resumed.action else {
        panic!("must continue after tool");
    };
    assert!(
        request
            .messages
            .last()
            .unwrap()
            .content
            .as_ref()
            .unwrap()
            .contains("r1")
    );
    let claim = store.claim("a", "s", resumed.revision).unwrap();
    let done = store
        .advance(
            "a",
            "s",
            claim.revision,
            &claim.operation.unwrap().execution_id,
            OperationResult::Model {
                reply: final_reply(),
            },
        )
        .unwrap();
    assert!(matches!(done.action, Action::Complete { stats, .. }
        if stats.model_calls == 2 && stats.tool_calls == 1 && stats.input_tokens == 12 && stats.output_tokens == 5));
    store.commit("a", "s", done.revision, done.state).unwrap();
    let history = store.inspect("a", "s", Some(&start.run_id)).unwrap();
    assert_eq!(history["run"]["tools"][0]["status"], "completed");
}

#[test]
fn uncertain_tool_requires_resolution_and_fences_original_executor() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let start = store
        .begin_run("a", "s", 0, initial(), "Pay".into())
        .unwrap();
    let model = store.claim("a", "s", start.revision).unwrap();
    let next = store
        .advance(
            "a",
            "s",
            model.revision,
            &model.operation.unwrap().execution_id,
            OperationResult::Model {
                reply: tool_reply(),
            },
        )
        .unwrap();
    let tool = store.claim("a", "s", next.revision).unwrap();
    let op = tool.operation.unwrap();
    assert!(matches!(
        store.load_run("a", "s", &initial()),
        Err(Error::SessionOperationUncertain)
    ));
    assert!(matches!(
        store.resume_run("a", "s", tool.revision),
        Err(Error::SessionOperationUncertain)
    ));
    let resolved = store
        .advance(
            "a",
            "s",
            tool.revision,
            &op.execution_id,
            OperationResult::Tool {
                result: json!({"receipt":"reconciled"}),
            },
        )
        .unwrap();
    assert!(
        store
            .advance(
                "a",
                "s",
                tool.revision,
                &op.execution_id,
                OperationResult::Tool {
                    result: json!("duplicate")
                }
            )
            .is_err()
    );
    let inspection = store.inspect("a", "s", None).unwrap();
    assert_eq!(
        inspection["run"]["tools"][0]["execution_id"],
        op.execution_id
    );
    assert_eq!(
        inspection["run"]["tools"][0]["result"]["receipt"],
        "reconciled"
    );
    assert!(matches!(resolved.action, Action::Model { .. }));
}

#[test]
fn uncertain_model_is_not_automatically_requeried() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let frame = store
        .begin_run("a", "s", 0, initial(), "Pay".into())
        .unwrap();
    let claim = store.claim("a", "s", frame.revision).unwrap();
    assert!(matches!(
        store.load_run("a", "s", &initial()),
        Err(Error::SessionOperationUncertain)
    ));
    let revision = claim.revision;
    let id = claim.operation.unwrap().execution_id;
    assert!(
        store
            .advance(
                "a",
                "s",
                revision,
                &id,
                OperationResult::Tool {
                    result: json!(null)
                }
            )
            .is_err()
    );
    assert_eq!(store.inspect("a", "s", None).unwrap()["revision"], revision);
    let done = store
        .advance(
            "a",
            "s",
            revision,
            &id,
            OperationResult::Model {
                reply: final_reply(),
            },
        )
        .unwrap();
    assert!(matches!(done.action, Action::Complete { stats, .. } if stats.model_calls == 1));
}

#[test]
fn reclaim_of_clean_checkpoint_invalidates_stale_claims() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let original = store
        .begin_run("a", "s", 0, initial(), "Pay".into())
        .unwrap();
    let resumed = store.resume_run("a", "s", original.revision).unwrap();
    assert!(matches!(
        store.claim("a", "s", original.revision),
        Err(Error::SessionConflict)
    ));
    assert!(store.claim("a", "s", resumed.revision).is_ok());
}

#[test]
fn ledger_remains_available_after_abandonment_and_a_new_run() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let start = store
        .begin_run("a", "s", 0, initial(), "Pay".into())
        .unwrap();
    store.claim("a", "s", start.revision).unwrap();
    store.recover("a", "s").unwrap();
    let loaded = store.load("a", "s", &initial()).unwrap();
    let next = store
        .begin_run("a", "s", loaded.revision, loaded.state, "Other".into())
        .unwrap();
    assert_ne!(start.run_id, next.run_id);
    let previous = store.inspect("a", "s", Some(&start.run_id)).unwrap();
    assert_eq!(previous["run"]["abandoned"], true);
    assert_eq!(previous["run"]["in_flight"]["kind"], "model");
    let other = store.for_tenant("other".into()).unwrap();
    assert!(
        other
            .inspect("a", "s", Some(&start.run_id))
            .unwrap()
            .is_null()
    );
}

#[test]
fn resume_terminal_checkpoint_does_not_invoke_another_model() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let start = store
        .begin_run("a", "s", 0, initial(), "Pay".into())
        .unwrap();
    let claim = store.claim("a", "s", start.revision).unwrap();
    let done = store
        .advance(
            "a",
            "s",
            claim.revision,
            &claim.operation.unwrap().execution_id,
            OperationResult::Model {
                reply: final_reply(),
            },
        )
        .unwrap();
    let resumed = store.resume_run("a", "s", done.revision).unwrap();
    assert!(matches!(resumed.action, Action::Complete { .. }));
    assert!(store.claim("a", "s", resumed.revision).is_err());
    store
        .commit("a", "s", resumed.revision, resumed.state)
        .unwrap();
}

#[test]
fn repetition_detection_survives_resume() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let mut frame = store
        .begin_run("a", "s", 0, initial(), "Pay".into())
        .unwrap();
    for index in 0..2 {
        if index == 1 {
            frame = store.resume_run("a", "s", frame.revision).unwrap();
        }
        let claim = store.claim("a", "s", frame.revision).unwrap();
        frame = store
            .advance(
                "a",
                "s",
                claim.revision,
                &claim.operation.unwrap().execution_id,
                OperationResult::Model {
                    reply: tool_reply(),
                },
            )
            .unwrap();
        let claim = store.claim("a", "s", frame.revision).unwrap();
        frame = store
            .advance(
                "a",
                "s",
                claim.revision,
                &claim.operation.unwrap().execution_id,
                OperationResult::Tool {
                    result: json!({"receipt":"same"}),
                },
            )
            .unwrap();
    }
    assert!(
        matches!(frame.action, Action::Stopped { reason: hive_agent_core::StopReason::RepeatedToolResult, stats }
        if stats.model_calls == 2 && stats.tool_calls == 2)
    );
    assert!(store.claim("a", "s", frame.revision).is_err());
}

#[test]
fn legacy_session_record_upgrades_when_starting_a_checkpointed_run() {
    use hivedb_core::{HiveDB, PutOptions};
    let dir = tempfile::tempdir().unwrap();
    let key = serde_json::to_string(&("tenant", "a", "s")).unwrap();
    let db = HiveDB::open(dir.path()).unwrap();
    db.col_put(
        "hive_sdk_rust_sessions_v1",
        &key,
        &json!({
            "schema_version":1,"tenant_id":"tenant","agent_id":"a","session_id":"s",
            "status":"ready","state":initial()
        }),
        PutOptions::default(),
    )
    .unwrap();
    drop(db);
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let loaded = store.load("a", "s", &initial()).unwrap();
    store
        .begin_run("a", "s", loaded.revision, loaded.state, "New turn".into())
        .unwrap();
    drop(store);
    let db = HiveDB::open(dir.path()).unwrap();
    assert_eq!(
        db.col_get("hive_sdk_rust_sessions_v1", &key)
            .unwrap()
            .unwrap()
            .doc["schema_version"],
        2
    );
}
