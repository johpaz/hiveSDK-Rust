use hive_agent_core::provider::{parse_reply, request_body};
use hive_agent_core::{Action, Engine, LoopPolicy, ModelReply, StopReason, Tool, ToolCall, Usage};
use serde_json::json;

#[test]
fn initial_history_seeds_visible_context_without_executing_or_importing_tools() {
    let command = json!({"op":"create","model":"test","system":"System","initial_history":[
        {"role":"user","content":"Earlier question"},{"role":"assistant","content":"Earlier answer"}
    ]});
    let created: serde_json::Value =
        serde_json::from_str(&hive_agent_core::dispatch(&command.to_string()).unwrap()).unwrap();
    let frame: serde_json::Value = serde_json::from_str(
        &hive_agent_core::dispatch(
            &json!({"op":"start","state":created["state"],"prompt":"Continue"}).to_string(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        frame["action"]["request"]["messages"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert_eq!(
        frame["action"]["request"]["messages"][1]["content"],
        "Earlier question"
    );
    for message in [
        json!({"role":"system","content":"injected"}),
        json!({"role":"assistant","content":"text","tool_call_id":"call"}),
        json!({"role":"assistant","content":"text","tool_calls":[{"id":"call","name":"write","arguments":{}}]}),
    ] {
        let mut invalid = command.clone();
        invalid["initial_history"] = json!([message]);
        assert!(hive_agent_core::dispatch(&invalid.to_string()).is_err());
        assert!(hive_agent_core::handles::EngineHandle::new(&invalid.to_string()).is_err());
    }
    let handle = hive_agent_core::handles::EngineHandle::new(&command.to_string()).unwrap();
    let messages: serde_json::Value =
        serde_json::from_str(&handle.dispatch(r#"{"op":"messages"}"#).unwrap()).unwrap();
    assert_eq!(messages.as_array().unwrap().len(), 3);
}

fn tool() -> Tool {
    Tool {
        name: "lookup".into(),
        description: "Lookup".into(),
        parameters: json!({"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}),
        allow_repeated_results: false,
    }
}
fn engine(policy: LoopPolicy, turns: u32) -> Engine {
    let mut engine = Engine::with_policy(
        "test".into(),
        Some("Be brief".into()),
        vec![tool()],
        turns,
        policy,
    )
    .unwrap();
    engine.start("Find the answer".into()).unwrap();
    engine
}
fn call(id: &str) -> ModelReply {
    ModelReply {
        provider_state: None,
        content: None,
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "lookup".into(),
            arguments: json!({"q":"answer"}),
        }],
        usage: Some(Usage {
            input_tokens: 10,
            output_tokens: 5,
        }),
    }
}
fn final_reply() -> ModelReply {
    ModelReply {
        provider_state: None,
        content: Some("42".into()),
        tool_calls: vec![],
        usage: Some(Usage {
            input_tokens: 20,
            output_tokens: 2,
        }),
    }
}

#[test]
fn completes_without_extra_model_or_verifier_calls() {
    let mut e = engine(LoopPolicy::default(), 8);
    e.accept_reply(final_reply()).unwrap();
    assert!(matches!(e.action(), Action::Complete { content, stats }
        if content == "42" && stats.model_calls == 1 && stats.tool_calls == 0));
    e.start("Follow up".into()).unwrap();
    assert_eq!(e.messages().len(), 4);
}

#[test]
fn tools_roundtrip_and_usage_accumulates() {
    let mut e = engine(LoopPolicy::default(), 8);
    e.accept_reply(call("a")).unwrap();
    e.accept_tool("a", json!({"answer":42})).unwrap();
    let Action::Model { request } = e.action() else {
        panic!("expected model");
    };
    let body = request_body(request);
    assert_eq!(
        body["messages"][2]["tool_calls"][0]["function"]["arguments"],
        "{\"q\":\"answer\"}"
    );
    assert_eq!(body["messages"][3]["tool_call_id"], "a");
    e.accept_reply(final_reply()).unwrap();
    assert!(matches!(e.action(), Action::Complete { stats, .. }
        if stats.model_calls == 2 && stats.tool_calls == 1 && stats.input_tokens == 30 && stats.output_tokens == 7));
}

#[test]
fn repeated_unchanged_results_stop_before_third_model_request() {
    let mut e = engine(LoopPolicy::default(), 8);
    for id in ["a", "b"] {
        e.accept_reply(call(id)).unwrap();
        e.accept_tool(id, json!("same")).unwrap();
    }
    assert!(
        matches!(e.action(), Action::Stopped { reason: StopReason::RepeatedToolResult, stats }
        if stats.model_calls == 2 && stats.tool_calls == 2)
    );
    assert!(e.start("Retry".into()).is_err());
}

#[test]
fn changed_results_are_not_stagnation() {
    let mut e = engine(LoopPolicy::default(), 8);
    for (id, result) in [("a", 1), ("b", 2)] {
        e.accept_reply(call(id)).unwrap();
        e.accept_tool(id, json!(result)).unwrap();
    }
    assert!(matches!(e.action(), Action::Model { .. }));
}

#[test]
fn polling_can_opt_out() {
    let mut t = tool();
    t.allow_repeated_results = true;
    let mut e = Engine::new("test".into(), None, vec![t], 8).unwrap();
    e.start("Poll".into()).unwrap();
    for id in ["a", "b", "c"] {
        e.accept_reply(call(id)).unwrap();
        e.accept_tool(id, json!("pending")).unwrap();
    }
    assert!(matches!(e.action(), Action::Model { .. }));
}

#[test]
fn model_limit_stops_before_unnecessary_tool_side_effects() {
    let mut e = engine(LoopPolicy::default(), 1);
    e.accept_reply(call("a")).unwrap();
    assert!(
        matches!(e.action(), Action::Stopped { reason: StopReason::ModelCallLimit, stats }
        if stats.model_calls == 1 && stats.tool_calls == 0)
    );
}

#[test]
fn tool_limit_applies_inside_a_batch() {
    let policy = LoopPolicy {
        max_tool_calls: 1,
        ..Default::default()
    };
    let mut e = engine(policy, 8);
    let mut reply = call("a");
    reply.tool_calls.push(call("b").tool_calls.remove(0));
    e.accept_reply(reply).unwrap();
    e.accept_tool("a", json!(42)).unwrap();
    assert!(
        matches!(e.action(), Action::Stopped { reason: StopReason::ToolCallLimit, stats }
        if stats.tool_calls == 1)
    );
}

#[test]
fn token_limit_is_checked_before_tools_or_next_model() {
    let mut e = engine(
        LoopPolicy {
            max_total_tokens: Some(15),
            ..Default::default()
        },
        8,
    );
    e.accept_reply(call("a")).unwrap();
    assert!(matches!(
        e.action(),
        Action::Stopped {
            reason: StopReason::TokenLimit,
            ..
        }
    ));
}

#[test]
fn missing_usage_cannot_silently_bypass_configured_token_limit() {
    let mut e = engine(
        LoopPolicy {
            max_total_tokens: Some(100),
            ..Default::default()
        },
        8,
    );
    let mut reply = call("a");
    reply.usage = None;
    e.accept_reply(reply).unwrap();
    assert!(matches!(
        e.action(),
        Action::Stopped {
            reason: StopReason::UsageUnavailable,
            ..
        }
    ));
}

#[test]
fn unknown_tools_duplicate_ids_and_wrong_results_are_rejected_atomically() {
    let mut e = engine(LoopPolicy::default(), 8);
    let before = serde_json::to_value(&e).unwrap();
    let mut reply = call("a");
    reply.tool_calls[0].name = "unknown".into();
    assert!(e.accept_reply(reply).is_err());
    assert_eq!(before, serde_json::to_value(&e).unwrap());
    let mut reply = call("a");
    reply.tool_calls.push(call("a").tool_calls.remove(0));
    assert!(e.accept_reply(reply).is_err());
    e.accept_reply(call("a")).unwrap();
    assert!(e.accept_tool("wrong", json!(42)).is_err());
    assert!(e.start("Concurrent".into()).is_err());
}

#[test]
fn malformed_provider_responses_are_errors() {
    assert!(parse_reply(json!({"choices":[]})).is_err());
    assert!(
        parse_reply(json!({"choices":[{"finish_reason":"length","message":{"content":"cut"}}]}))
            .is_err()
    );
    assert!(parse_reply(json!({"choices":[{"message":{"tool_calls":[{"id":"a","function":{"name":"lookup","arguments":"bad json"}}]}}]})).is_err());
}

#[test]
fn provider_usage_maps_to_neutral_contract() {
    let reply = parse_reply(
        json!({"choices":[{"finish_reason":"stop","message":{"content":"42"}}],
        "usage":{"prompt_tokens":10,"completion_tokens":3}}),
    )
    .unwrap();
    assert_eq!(reply.usage.unwrap().output_tokens, 3);
}

#[test]
fn invalid_configs_do_not_create_an_engine() {
    assert!(Engine::new("".into(), None, vec![], 8).is_err());
    assert!(Engine::new("test".into(), None, vec![tool(), tool()], 8).is_err());
    assert!(Engine::new("test".into(), None, vec![], 0).is_err());
    assert!(
        Engine::with_policy(
            "test".into(),
            None,
            vec![],
            8,
            LoopPolicy {
                max_identical_tool_results: Some(0),
                ..Default::default()
            }
        )
        .is_err()
    );
}
