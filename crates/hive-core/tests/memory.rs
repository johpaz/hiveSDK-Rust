use hive_agent_core::memory::{ContextPlan, ContextSearchOptions, ContextSummary};
use hive_agent_core::sessions::{SessionStatus, SessionStore};
use hive_agent_core::{
    Action, ContextOverflow, Engine, LoopPolicy, ModelReply, StopReason, Tool, ToolCall,
};
use serde_json::json;

fn engine(policy: LoopPolicy) -> Engine {
    Engine::with_policy(
        "test".into(),
        Some("System".into()),
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
fn done(state: &mut Engine, content: &str) {
    state
        .accept_reply(ModelReply {
            provider_state: None,
            content: Some(content.into()),
            tool_calls: vec![],
            usage: Some(hive_agent_core::Usage {
                input_tokens: 1,
                output_tokens: 1,
            }),
        })
        .unwrap();
}
fn history(policy: LoopPolicy) -> Engine {
    let mut state = engine(policy);
    state.start("Pedido R7, entrega viernes 🦀".into()).unwrap();
    done(&mut state, "Recibido R7");
    state.start("Factura R8".into()).unwrap();
    done(&mut state, "Pagada R8");
    state
}
fn summary(state: &Engine) -> ContextPlan {
    ContextPlan {
        summary: Some(ContextSummary {
            id: "summary-v1".into(),
            source: state.capture_context(1, 5).unwrap(),
            content: "R7 viernes, R8 pagada".into(),
        }),
        retrieved: vec![],
    }
}

#[test]
fn sources_cover_closed_turns_and_reject_hashes_from_another_history() {
    let mut state = history(Default::default());
    let source = state.capture_context(1, 3).unwrap();
    assert_eq!(source.sha256.len(), 64);
    assert_eq!(state.read_context(&source).unwrap().len(), 2);
    for (start, end) in [(0, 3), (1, 2), (2, 3), (1, 6), (3, 1)] {
        assert!(state.capture_context(start, end).is_err());
    }
    let mut changed = serde_json::to_value(&state).unwrap();
    changed["messages"][1]["content"] = json!("Changed receipt");
    let changed: Engine = serde_json::from_value(changed).unwrap();
    assert!(changed.read_context(&source).is_err());
    state.start("Active turn".into()).unwrap();
    assert!(state.capture_context(5, 6).is_err());
    assert!(state.read_context(&source).is_ok());
}

#[test]
fn lexical_retrieval_has_exact_bounded_bytes_and_prefers_recent_ties() {
    let state = history(Default::default());
    let found = state
        .search_context("R7 viernes", Default::default())
        .unwrap();
    assert_eq!(found.hits.len(), 1);
    assert_eq!(found.hits[0].score, 2);
    assert_eq!(found.hits[0].source.start, 1);
    assert_eq!(
        found.serialized_bytes,
        serde_json::to_vec(&found.hits).unwrap().len() as u64
    );
    let exact = found.serialized_bytes;
    assert_eq!(
        state
            .search_context(
                "R7",
                ContextSearchOptions {
                    max_turns: 1,
                    max_bytes: exact
                }
            )
            .unwrap()
            .hits
            .len(),
        1
    );
    assert!(
        state
            .search_context(
                "R7",
                ContextSearchOptions {
                    max_turns: 1,
                    max_bytes: exact - 1
                }
            )
            .unwrap()
            .hits
            .is_empty()
    );
    assert!(
        state
            .search_context("role", Default::default())
            .unwrap()
            .hits
            .is_empty()
    );
    let ties = state
        .search_context(
            "R7 R8",
            ContextSearchOptions {
                max_turns: 1,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(ties.hits[0].source.start, 3);
    for query in ["", "!!!"] {
        assert!(state.search_context(query, Default::default()).is_err());
    }
    assert!(
        state
            .search_context(
                "R7",
                ContextSearchOptions {
                    max_turns: 0,
                    ..Default::default()
                }
            )
            .is_err()
    );
}

#[test]
fn summary_and_retrieved_turns_project_without_changing_history_or_system() {
    let mut state = history(Default::default());
    let original = serde_json::to_value(state.messages()).unwrap();
    let mut plan = summary(&state);
    plan.retrieved.push(state.capture_context(1, 3).unwrap());
    state.set_context_plan(Some(plan)).unwrap();
    state.start("Next".into()).unwrap();
    let Action::Model { request } = state.action() else {
        panic!("expected model")
    };
    assert_eq!(request.messages.len(), 5);
    assert_eq!(request.messages[0].role, "system");
    assert_eq!(request.messages[1].role, "user");
    let data: serde_json::Value =
        serde_json::from_str(request.messages[1].content.as_deref().unwrap()).unwrap();
    assert_eq!(data["context_summary"]["id"], "summary-v1");
    assert_eq!(request.messages[2].content, state.messages()[1].content);
    assert_eq!(
        request.messages.last().unwrap().content.as_deref(),
        Some("Next")
    );
    let report = request.context.as_ref().unwrap();
    assert_eq!(report.summary_id.as_deref(), Some("summary-v1"));
    assert_eq!(report.dropped_messages, 2);
    assert_eq!(report.retrieved_sources.len(), 1);
    assert_eq!(
        serde_json::to_value(&state.messages()[..5]).unwrap(),
        original
    );
    let before = serde_json::to_value(&state).unwrap();
    state.preflight().unwrap();
    assert_eq!(before, serde_json::to_value(&state).unwrap());
    assert!(state.set_context_plan(None).is_err());
}

#[test]
fn pinned_summary_and_original_turns_never_get_dropped_to_make_a_request_fit() {
    let mut sample = history(Default::default());
    let plan = summary(&sample);
    sample.set_context_plan(Some(plan)).unwrap();
    sample.start("Next".into()).unwrap();
    let Action::Model { request } = sample.action() else {
        panic!()
    };
    let bytes = serde_json::to_vec(
        &json!({"model":request.model,"messages":request.messages,"tools":request.tools}),
    )
    .unwrap()
    .len() as u64;
    let mut state = history(LoopPolicy {
        max_context_bytes: Some(bytes - 1),
        context_overflow: ContextOverflow::DropOldestTurns,
        max_total_output_tokens: Some(20),
        ..Default::default()
    });
    let plan = summary(&state);
    state.set_context_plan(Some(plan)).unwrap();
    state.start("Next".into()).unwrap();
    assert!(
        matches!(state.action(),Action::Stopped {reason:StopReason::ContextLimit,stats} if stats.model_calls==0 && stats.reserved_output_tokens==0)
    );
    assert_eq!(state.messages().len(), 6);
    assert!(state.context_plan().unwrap().summary.is_some());
}

#[test]
fn malformed_plans_and_duplicate_or_partial_retrieval_are_rejected() {
    let mut state = history(Default::default());
    let mut plan = summary(&state);
    plan.summary.as_mut().unwrap().content.clear();
    assert!(state.set_context_plan(Some(plan)).is_err());
    let mut plan = summary(&state);
    plan.summary.as_mut().unwrap().source = state.capture_context(3, 5).unwrap();
    assert!(state.set_context_plan(Some(plan)).is_err());
    let mut plan = summary(&state);
    plan.summary.as_mut().unwrap().source.sha256 = "0".repeat(64);
    assert!(state.set_context_plan(Some(plan)).is_err());
    let source = state.capture_context(1, 3).unwrap();
    assert!(
        state
            .set_context_plan(Some(ContextPlan {
                summary: None,
                retrieved: vec![source.clone(), source]
            }))
            .is_err()
    );
    let all = state.capture_context(1, 5).unwrap();
    assert!(
        state
            .set_context_plan(Some(ContextPlan {
                summary: None,
                retrieved: vec![all]
            }))
            .is_err()
    );
    assert!(state.context_plan().is_none());
}

#[test]
fn tool_batches_are_retrieved_whole_and_active_turns_are_excluded() {
    let mut state = engine(Default::default());
    state.start("Lookup receipt".into()).unwrap();
    state
        .accept_reply(ModelReply {
            provider_state: None,
            content: None,
            usage: None,
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: json!({"receipt":"R9"}),
            }],
        })
        .unwrap();
    state
        .accept_tool("c", json!({"delivered":"martes"}))
        .unwrap();
    assert!(
        state
            .search_context("martes", Default::default())
            .unwrap()
            .hits
            .is_empty()
    );
    done(&mut state, "R9 confirmed");
    let hit = state
        .search_context("R9 martes", Default::default())
        .unwrap()
        .hits
        .remove(0);
    assert_eq!(hit.messages.len(), 4);
    assert_eq!(hit.messages[1].tool_calls[0].id, "c");
    assert_eq!(hit.messages[2].tool_call_id.as_deref(), Some("c"));
}

#[test]
fn context_plan_is_cas_persisted_and_cannot_replace_an_active_checkpoint() {
    let directory = tempfile::tempdir().unwrap();
    let store = SessionStore::open(directory.path(), "tenant".into()).unwrap();
    let initial = history(Default::default());
    let plan = summary(&initial);
    let revision = store.begin("a", "s", 0, initial.clone()).unwrap();
    let revision = store.commit("a", "s", revision, initial.clone()).unwrap();
    let loaded = store
        .set_context_plan("a", "s", revision, initial.clone(), Some(plan))
        .unwrap();
    assert!(matches!(loaded.status, SessionStatus::Ready));
    assert!(
        store
            .set_context_plan("a", "s", revision, initial, None)
            .is_err()
    );
    drop(store);
    let store = SessionStore::open(directory.path(), "tenant".into()).unwrap();
    let loaded = store.load("a", "s", &engine(Default::default())).unwrap();
    assert_eq!(
        loaded
            .state
            .context_plan()
            .unwrap()
            .summary
            .as_ref()
            .unwrap()
            .id,
        "summary-v1"
    );
    let run = store
        .begin_run(
            "a",
            "s",
            loaded.revision,
            loaded.state.clone(),
            "Next".into(),
        )
        .unwrap();
    assert!(
        store
            .set_context_plan("a", "s", run.revision, loaded.state, None)
            .is_err()
    );
    let before = serde_json::to_value(&run.state).unwrap();
    let resumed = store.resume_run("a", "s", run.revision).unwrap();
    assert_eq!(serde_json::to_value(resumed.state).unwrap(), before);
}

#[test]
fn source_hash_normalizes_portable_integral_floats_across_json_bridges() {
    let mut state = engine(Default::default());
    state.start("Read".into()).unwrap();
    state
        .accept_reply(ModelReply {
            provider_state: None,
            content: None,
            usage: None,
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: json!({"n":1.0,"z":-0.0}),
            }],
        })
        .unwrap();
    state.accept_tool("c", json!(5)).unwrap();
    done(&mut state, "done");
    let source = state.capture_context(1, 5).unwrap();
    let mut bridge = serde_json::to_value(&state).unwrap();
    bridge["messages"][2]["tool_calls"][0]["arguments"] = json!({"n":1,"z":0});
    let restored: Engine = serde_json::from_value(bridge.clone()).unwrap();
    assert_eq!(source, restored.capture_context(1, 5).unwrap());
    bridge["messages"][2]["tool_calls"][0]["arguments"]["n"] = json!(1.5);
    let changed: Engine = serde_json::from_value(bridge).unwrap();
    assert!(changed.read_context(&source).is_err());
}

#[test]
fn memory_records_fence_older_writers_even_after_removing_the_plan() {
    use hivedb_core::{HiveDB, PutOptions};
    let directory = tempfile::tempdir().unwrap();
    let state = history(Default::default());
    let store = SessionStore::open(directory.path(), "tenant".into()).unwrap();
    let revision = store.begin("a", "s", 0, state.clone()).unwrap();
    let revision = store.commit("a", "s", revision, state.clone()).unwrap();
    let loaded = store
        .set_context_plan("a", "s", revision, state.clone(), Some(summary(&state)))
        .unwrap();
    drop(store);
    let db = HiveDB::open(directory.path()).unwrap();
    let key = serde_json::to_string(&("tenant", "a", "s")).unwrap();
    let mut record = db
        .col_get("hive_sdk_rust_sessions_v1", &key)
        .unwrap()
        .unwrap()
        .doc;
    assert_eq!(record["schema_version"], 4);
    record["schema_version"] = json!(2);
    db.col_put(
        "hive_sdk_rust_sessions_v1",
        &key,
        &record,
        PutOptions::default(),
    )
    .unwrap();
    drop(db);
    let store = SessionStore::open(directory.path(), "tenant".into()).unwrap();
    assert!(store.load("a", "s", &state).is_err());
    drop(store);
    let db = HiveDB::open(directory.path()).unwrap();
    record["schema_version"] = json!(4);
    db.col_put(
        "hive_sdk_rust_sessions_v1",
        &key,
        &record,
        PutOptions::default(),
    )
    .unwrap();
    drop(db);
    let store = SessionStore::open(directory.path(), "tenant".into()).unwrap();
    let loaded = store.load("a", "s", &loaded.state).unwrap();
    store
        .set_context_plan("a", "s", loaded.revision, loaded.state, None)
        .unwrap();
    drop(store);
    let db = HiveDB::open(directory.path()).unwrap();
    assert_eq!(
        db.col_get("hive_sdk_rust_sessions_v1", &key)
            .unwrap()
            .unwrap()
            .doc["schema_version"],
        4
    );
}

#[test]
fn json_bridge_number_normalization_does_not_weaken_session_conflict_detection() {
    for (original, bridged, allowed) in [
        (json!(1.0), json!(1), true),
        (json!(-0.0), json!(0), true),
        (json!(1.0), json!(1.5), false),
        (
            json!(9007199254740993u64),
            json!(9007199254740992u64),
            false,
        ),
    ] {
        let mut state = engine(Default::default());
        state.start("Read".into()).unwrap();
        state
            .accept_reply(ModelReply {
                provider_state: None,
                content: None,
                usage: None,
                tool_calls: vec![ToolCall {
                    id: "c".into(),
                    name: "read".into(),
                    arguments: json!({"n":original}),
                }],
            })
            .unwrap();
        state.accept_tool("c", json!(5)).unwrap();
        done(&mut state, "done");
        let directory = tempfile::tempdir().unwrap();
        let store = SessionStore::open(directory.path(), "t".into()).unwrap();
        let revision = store.begin("a", "s", 0, state.clone()).unwrap();
        let revision = store.commit("a", "s", revision, state.clone()).unwrap();
        let mut bridge = serde_json::to_value(state).unwrap();
        bridge["messages"][2]["tool_calls"][0]["arguments"]["n"] = bridged;
        let bridge: Engine = serde_json::from_value(bridge).unwrap();
        let result = store.begin_run("a", "s", revision, bridge, "Next".into());
        if allowed {
            assert!(result.is_ok());
        } else {
            assert!(matches!(
                result,
                Err(hive_agent_core::Error::SessionConflict)
            ));
        }
    }
}

#[test]
fn summary_and_retrieved_data_are_included_in_exact_bytes_and_bpe() {
    use hive_agent_core::{InputTokenBudget, TokenEncoding};
    let policy = LoopPolicy {
        max_output_tokens: Some(10),
        input_token_budget: Some(InputTokenBudget {
            encoding: TokenEncoding::Cl100kBase,
            max_tokens: 10000,
            overhead_tokens: 7,
            framing: Default::default(),
            max_total_tokens: Some(20000),
            context_window_tokens: Some(20000),
            attachment_tokens: None,
        }),
        ..Default::default()
    };
    let mut state = history(policy);
    let mut plan = summary(&state);
    plan.retrieved.push(state.capture_context(1, 3).unwrap());
    state.set_context_plan(Some(plan)).unwrap();
    state.start("Next 🦀".into()).unwrap();
    let Action::Model { request } = state.action() else {
        panic!()
    };
    let text = format!(
        "{{\"model\":{},\"messages\":{},\"tools\":{}}}",
        serde_json::to_string(&request.model).unwrap(),
        serde_json::to_string(&request.messages).unwrap(),
        serde_json::to_string(&request.tools).unwrap()
    );
    let report = request.context.as_ref().unwrap();
    assert_eq!(report.serialized_bytes, text.len() as u64);
    let expected = tiktoken_rs::cl100k_base_singleton()
        .encode_ordinary(&text)
        .len() as u64
        + 7;
    assert_eq!(
        report.token_estimate.as_ref().unwrap().total_tokens,
        expected
    );
    assert_eq!(
        serde_json::to_value(&state).unwrap()["stats"]["reserved_input_tokens"],
        expected
    );
}
