use hive_agent_core::provider::request_body;
use hive_agent_core::sessions::SessionStore;
use hive_agent_core::{
    Action, ContextOverflow, Engine, LoopPolicy, ModelReply, StopReason, Tool, ToolCall,
};
use serde_json::{Value, json};

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
fn policy(limit: u64) -> LoopPolicy {
    LoopPolicy {
        max_context_bytes: Some(limit),
        context_overflow: ContextOverflow::DropOldestTurns,
        ..Default::default()
    }
}
fn request(state: &Engine) -> Value {
    let Action::Model { request } = state.action() else {
        panic!("expected model")
    };
    serde_json::to_value(request).unwrap()
}
fn size(request: &Value) -> u64 {
    serde_json::to_vec(
        &json!({"model":request["model"],"messages":request["messages"],"tools":request["tools"]}),
    )
    .unwrap()
    .len() as u64
}
fn done(state: &mut Engine, text: &str) {
    state
        .accept_reply(ModelReply {
            provider_state: None,
            content: Some(text.into()),
            tool_calls: vec![],
            usage: None,
        })
        .unwrap();
}

#[test]
fn exact_json_byte_boundary_includes_utf8_escaping_and_tool_schema() {
    let text = "é🦀\n\"\\";
    let mut sample = engine(Default::default());
    sample.start(text.into()).unwrap();
    let bytes = size(&request(&sample));
    let mut state = engine(policy(bytes));
    state.start(text.into()).unwrap();
    assert_eq!(
        request(&state)["context"],
        json!({"serialized_bytes":bytes,"dropped_messages":0})
    );
    let mut too_small = engine(policy(bytes - 1));
    too_small.start(text.into()).unwrap();
    assert!(
        matches!(too_small.action(), Action::Stopped {reason:StopReason::ContextLimit,stats} if stats.model_calls == 0)
    );
}

#[test]
fn stop_mode_does_not_drop_history_or_reserve_an_unmade_call() {
    let mut state = engine(LoopPolicy {
        max_context_bytes: Some(400),
        max_total_output_tokens: Some(20),
        ..Default::default()
    });
    state.start("x".repeat(1000)).unwrap();
    assert!(
        matches!(state.action(), Action::Stopped {reason:StopReason::ContextLimit,stats} if stats.model_calls==0&&stats.reserved_output_tokens==0)
    );
    assert_eq!(
        state.messages().last().unwrap().content.as_deref(),
        Some("x".repeat(1000).as_str())
    );
}

#[test]
fn evicts_oldest_complete_turn_and_keeps_recent_history_and_system() {
    let mut baseline = engine(Default::default());
    baseline.start("a".repeat(200)).unwrap();
    done(&mut baseline, "b");
    baseline.start("recent".into()).unwrap();
    done(&mut baseline, "answer");
    baseline.start("current".repeat(20)).unwrap();
    let mut expected = request(&baseline);
    expected["messages"].as_array_mut().unwrap().drain(1..3);
    let mut state = engine(policy(size(&expected)));
    state.start("a".repeat(200)).unwrap();
    done(&mut state, "b");
    state.start("recent".into()).unwrap();
    done(&mut state, "answer");
    state.start("current".repeat(20)).unwrap();
    let req = request(&state);
    assert_eq!(req["context"]["dropped_messages"], 2);
    assert_eq!(req["messages"][0]["content"], "System");
    assert_eq!(req["messages"][1]["content"], "recent");
    assert_eq!(req["messages"].as_array().unwrap().len(), 4);
    assert_eq!(req["context"]["serialized_bytes"], size(&req));
    assert_eq!(state.messages().len(), 6);
    let before = serde_json::to_value(&state).unwrap();
    state.preflight().unwrap();
    state.preflight().unwrap();
    assert_eq!(serde_json::to_value(&state).unwrap(), before);
}

#[test]
fn old_tool_batch_is_removed_as_a_whole_while_active_calls_stay_paired() {
    let mut state = engine(policy(1000));
    state.start("old".into()).unwrap();
    state
        .accept_reply(ModelReply {
            provider_state: None,
            content: None,
            usage: None,
            tool_calls: vec![
                ToolCall {
                    id: "a".into(),
                    name: "read".into(),
                    arguments: json!({}),
                },
                ToolCall {
                    id: "b".into(),
                    name: "read".into(),
                    arguments: json!({}),
                },
            ],
        })
        .unwrap();
    state.accept_tool("a", json!("a".repeat(100))).unwrap();
    state.accept_tool("b", json!("b".repeat(100))).unwrap();
    done(&mut state, "finished");
    state.start("current".repeat(50)).unwrap();
    assert_eq!(request(&state)["context"]["dropped_messages"], 5);
    state
        .accept_reply(ModelReply {
            provider_state: None,
            content: None,
            usage: None,
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: json!({}),
            }],
        })
        .unwrap();
    state.accept_tool("c", json!(42)).unwrap();
    let req = request(&state);
    assert_eq!(req["messages"].as_array().unwrap().len(), 4);
    assert_eq!(req["messages"][2]["tool_calls"][0]["id"], "c");
    assert_eq!(req["messages"][3]["tool_call_id"], "c");
    assert_eq!(req["context"]["serialized_bytes"], size(&req));
    assert_eq!(state.messages().len(), 9);
}

#[test]
fn oversized_active_tool_result_is_saved_then_stops_without_reserving_more() {
    let mut state = engine(LoopPolicy {
        max_total_output_tokens: Some(30),
        ..policy(500)
    });
    state.start("read".into()).unwrap();
    state
        .accept_reply(ModelReply {
            provider_state: None,
            content: None,
            usage: Some(hive_agent_core::Usage {
                input_tokens: 1,
                output_tokens: 1,
            }),
            tool_calls: vec![ToolCall {
                id: "a".into(),
                name: "read".into(),
                arguments: json!({}),
            }],
        })
        .unwrap();
    state.accept_tool("a", json!("x".repeat(1000))).unwrap();
    assert!(
        matches!(state.action(),Action::Stopped{reason:StopReason::ContextLimit,stats} if stats.model_calls==1&&stats.tool_calls==1&&stats.reserved_output_tokens==0)
    );
    assert_eq!(state.messages().last().unwrap().role, "tool");
    assert!(
        state
            .messages()
            .last()
            .unwrap()
            .content
            .as_ref()
            .unwrap()
            .len()
            > 1000
    );
}

#[test]
fn pinned_system_schema_and_active_user_cannot_be_evicted() {
    for part in ["system", "schema", "user"] {
        let mut state = Engine::with_policy(
            "test".into(),
            Some(if part == "system" {
                "s".repeat(1000)
            } else {
                "s".into()
            }),
            vec![Tool {
                name: "read".into(),
                description: if part == "schema" {
                    "d".repeat(1000)
                } else {
                    "d".into()
                },
                parameters: json!({"type":"object"}),
                allow_repeated_results: false,
            }],
            8,
            policy(500),
        )
        .unwrap();
        state
            .start(if part == "user" {
                "u".repeat(1000)
            } else {
                "u".into()
            })
            .unwrap();
        assert!(matches!(
            state.action(),
            Action::Stopped {
                reason: StopReason::ContextLimit,
                ..
            }
        ));
        assert_eq!(state.messages().len(), 2);
    }
}

#[test]
fn resume_rebuilds_the_same_bounded_request_from_full_durable_history() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let mut initial = engine(policy(500));
    initial.start("old".repeat(60)).unwrap();
    done(&mut initial, "finished");
    let frame = store
        .begin_run("a", "s", 0, initial, "current".repeat(20))
        .unwrap();
    let before = request(&frame.state);
    assert_eq!(before["context"]["dropped_messages"], 2);
    drop(store);
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let frame = store.resume_run("a", "s", frame.revision).unwrap();
    assert_eq!(request(&frame.state), before);
    assert_eq!(frame.state.messages().len(), 4);
    assert!(frame.operation.is_none());
}

#[test]
fn invalid_policy_is_rejected_and_old_records_default_to_unbounded() {
    for p in [
        policy(0),
        LoopPolicy {
            context_overflow: ContextOverflow::DropOldestTurns,
            ..Default::default()
        },
    ] {
        assert!(Engine::with_policy("test".into(), None, vec![], 8, p).is_err());
    }
    let mut state = engine(Default::default());
    state.start("hello".into()).unwrap();
    let mut old = serde_json::to_value(&state).unwrap();
    old["policy"]
        .as_object_mut()
        .unwrap()
        .remove("max_context_bytes");
    old["policy"]
        .as_object_mut()
        .unwrap()
        .remove("context_overflow");
    old["action"]["request"]
        .as_object_mut()
        .unwrap()
        .remove("context");
    let mut restored: Engine = serde_json::from_value(old).unwrap();
    restored.preflight().unwrap();
    assert_eq!(request(&restored), request(&state));
    let Action::Model { request: req } = state.action() else {
        panic!()
    };
    assert!(request_body(req).unwrap().get("context").is_none());
    let mut limited = engine(policy(500));
    limited.start("hi".into()).unwrap();
    let Action::Model { request: req } = limited.action() else {
        panic!()
    };
    assert!(req.context.is_some());
    assert!(request_body(req).unwrap().get("context").is_none());
}
