use hive_agent_core::provider::{
    ModelEvent, NativeProvider,
    anthropic::{Accumulator, parse_reply, request_body},
};
use hive_agent_core::{Action, Engine, LoopPolicy, ModelRequest, Tool};
use serde_json::{Value, json};

fn message(content: Value, reason: &str) -> Value {
    json!({"type":"message","role":"assistant","content":content,"stop_reason":reason,
        "usage":{"input_tokens":10,"cache_creation_input_tokens":4,"cache_read_input_tokens":6,"output_tokens":3}})
}
fn start() -> Value {
    json!({"type":"message_start","message":{"type":"message","role":"assistant","content":[],"stop_reason":null,
        "usage":{"input_tokens":10,"cache_creation_input_tokens":4,"cache_read_input_tokens":6,"output_tokens":0}}})
}
fn block_start(index: usize, block: Value) -> Value {
    json!({"type":"content_block_start","index":index,"content_block":block})
}
fn block_stop(index: usize) -> Value {
    json!({"type":"content_block_stop","index":index})
}
fn delta(reason: &str, output: u64) -> Value {
    json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":output}})
}
fn input(index: usize, text: &str) -> Value {
    json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":text}})
}
fn tool() -> Value {
    json!({"type":"tool_use","id":"c","name":"read","input":{}})
}
fn request() -> ModelRequest {
    serde_json::from_value(
        json!({"model":"test","messages":[{"role":"user","content":"Read"}],"tools":[]}),
    )
    .unwrap()
}

#[test]
fn request_maps_system_schema_tool_batches_errors_and_output_cap_without_metadata() {
    let mut state = Engine::with_policy(
        "test".into(),
        Some("System".into()),
        vec![Tool {
            name: "read".into(),
            description: "Read".into(),
            parameters: json!({"type":"object"}),
            allow_repeated_results: false,
        }],
        8,
        LoopPolicy {
            max_output_tokens: Some(11),
            max_context_bytes: Some(10000),
            ..Default::default()
        },
    )
    .unwrap();
    state.start("Read twice".into()).unwrap();
    let mut response = tool();
    response["id"] = json!("a");
    let mut response2 = tool();
    response2["id"] = json!("b");
    state
        .accept_reply(
            parse_reply(message(
                json!([{"type":"text","text":"Checking"},response,response2]),
                "tool_use",
            ))
            .unwrap(),
        )
        .unwrap();
    state.accept_tool("a", json!(42)).unwrap();
    state
        .accept_tool("b", json!({"error":{"message":"not found"}}))
        .unwrap();
    let Action::Model { request } = state.action() else {
        panic!()
    };
    let body = request_body(request, 4096).unwrap();
    assert_eq!(body["system"], json!([{"type":"text","text":"System"}]));
    assert_eq!(body["max_tokens"], 11);
    assert_eq!(body["tools"][0]["input_schema"], json!({"type":"object"}));
    assert!(body["tools"][0].get("allow_repeated_results").is_none());
    assert_eq!(body["messages"].as_array().unwrap().len(), 3);
    assert_eq!(body["messages"][1]["content"][1]["type"], "tool_use");
    assert_eq!(body["messages"][2]["role"], "user");
    assert_eq!(body["messages"][2]["content"].as_array().unwrap().len(), 2);
    assert_eq!(body["messages"][2]["content"][0]["content"], "42");
    assert_eq!(body["messages"][2]["content"][1]["is_error"], true);
    for key in [
        "context",
        "max_output_tokens",
        "deadline_unix_ms",
        "input_token_budget",
        "stream_options",
    ] {
        assert!(body.get(key).is_none());
    }
}

#[test]
fn default_cap_is_used_only_without_a_neutral_cap_and_invalid_requests_fail() {
    let mut req = request();
    assert_eq!(request_body(&req, 9).unwrap()["max_tokens"], 9);
    req.max_output_tokens = Some(3);
    assert_eq!(request_body(&req, 9).unwrap()["max_tokens"], 3);
    req.max_output_tokens = Some(0);
    assert!(request_body(&req, 9).is_err());
    for messages in [
        json!([]),
        json!([{"role":"developer","content":"x"}]),
        json!([{"role":"tool","content":"1","tool_call_id":"orphan"}]),
        json!([{"role":"user","content":"x"},{"role":"system","content":"late"}]),
        json!([{"role":"user","content":"x"},{"role":"assistant","content":null,"tool_calls":[{"id":"c","name":"read","arguments":{}}]}]),
    ] {
        let req: ModelRequest =
            serde_json::from_value(json!({"model":"test","messages":messages,"tools":[]})).unwrap();
        assert!(request_body(&req, 9).is_err());
    }
}

#[test]
fn json_reply_normalizes_text_calls_and_total_cache_input_usage() {
    let response = parse_reply(message(
        json!([{"type":"text","text":"A"},tool(),{"type":"text","text":"B"}]),
        "tool_use",
    ))
    .unwrap();
    assert_eq!(response.content.as_deref(), Some("AB"));
    assert_eq!(response.tool_calls[0].arguments, json!({}));
    let usage = response.usage.unwrap();
    assert_eq!(usage.input_tokens, 20);
    assert_eq!(usage.output_tokens, 3);
    let mut missing = message(json!([{"type":"text","text":"done"}]), "end_turn");
    missing.as_object_mut().unwrap().remove("usage");
    assert!(parse_reply(missing).unwrap().usage.is_none());
}

#[test]
fn truncated_refused_paused_unknown_or_unrepresentable_replies_do_not_become_tool_calls() {
    for reason in [
        "max_tokens",
        "model_context_window_exceeded",
        "pause_turn",
        "refusal",
        "unknown",
        "end_turn",
    ] {
        assert!(parse_reply(message(json!([tool()]), reason)).is_err());
    }
    for block in [
        json!({"type":"thinking","thinking":"private"}),
        json!({"type":"server_tool_use","id":"s"}),
        json!({"type":"text","text":"quoted","citations":[{}]}),
    ] {
        assert!(parse_reply(message(json!([block]), "end_turn")).is_err());
    }
    let mut refused = message(json!([{"type":"text","text":"no"}]), "end_turn");
    refused["stop_details"] = json!({"type":"refusal"});
    assert!(parse_reply(refused).is_err());
    assert!(parse_reply(message(json!([tool(), tool()]), "tool_use")).is_err());
}

#[test]
fn sse_text_and_cumulative_usage_are_committed_once_only_at_message_stop() {
    let mut acc = Accumulator::default();
    let mut events = vec![];
    for event in [
        start(),
        block_start(0, json!({"type":"text","text":"A"})),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"🐝"}}),
        block_stop(0),
        delta("end_turn", 3),
        delta("end_turn", 5),
    ] {
        events.extend(acc.push(event).unwrap());
    }
    assert!(!events.iter().any(|e| matches!(e, ModelEvent::Reply { .. })));
    let final_events = acc.push(json!({"type":"message_stop"})).unwrap();
    let ModelEvent::Reply { reply } = final_events.into_iter().next().unwrap() else {
        panic!()
    };
    assert_eq!(reply.content.as_deref(), Some("A🐝"));
    assert_eq!(reply.usage.as_ref().unwrap().input_tokens, 20);
    assert_eq!(reply.usage.as_ref().unwrap().output_tokens, 5);
    let output: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let ModelEvent::Usage { usage } = e {
                Some(usage.output_tokens)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(output, vec![0, 3, 5]);
    assert!(acc.push(json!({"type":"ping"})).is_err());
}

#[test]
fn streamed_tools_use_contiguous_neutral_indices_despite_text_blocks_and_fragmented_json() {
    let mut acc = Accumulator::default();
    let mut events = vec![];
    for event in [
        start(),
        block_start(0, json!({"type":"text","text":"checking"})),
        block_stop(0),
        block_start(1, tool()),
        input(1, "{\"path\":"),
        input(1, "\"🐝\"}"),
        block_stop(1),
        delta("tool_use", 3),
    ] {
        events.extend(acc.push(event).unwrap());
    }
    let indices: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let ModelEvent::ToolCallDelta { index, .. } = e {
                Some(*index)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(indices, vec![0, 0, 0]);
    assert!(!events.iter().any(|e| matches!(e, ModelEvent::Reply { .. })));
    let reply = acc
        .push(json!({"type":"message_stop"}))
        .unwrap()
        .pop()
        .unwrap();
    let ModelEvent::Reply { reply } = reply else {
        panic!()
    };
    assert_eq!(reply.tool_calls[0].arguments, json!({"path":"🐝"}));
}

#[test]
fn empty_and_complete_initial_tool_inputs_are_preserved() {
    for initial in [json!({}), json!({"path":"x"})] {
        let mut acc = Accumulator::default();
        let mut block = tool();
        block["input"] = initial.clone();
        let mut arguments = String::new();
        for event in [
            start(),
            block_start(0, block),
            block_stop(0),
            delta("tool_use", 3),
            json!({"type":"message_stop"}),
        ] {
            for e in acc.push(event).unwrap() {
                if let ModelEvent::ToolCallDelta {
                    arguments: part, ..
                } = e
                {
                    arguments.push_str(&part);
                }
            }
        }
        assert_eq!(serde_json::from_str::<Value>(&arguments).unwrap(), initial);
    }
}

#[test]
fn invalid_sse_order_identity_types_and_incomplete_blocks_are_rejected() {
    let mut acc = Accumulator::default();
    assert!(acc.push(block_stop(0)).is_err());
    for invalid in [
        start(),
        block_start(1, tool()),
        block_stop(0),
        json!({"type":"message_stop"}),
        json!({"type":"unknown"}),
    ] {
        let mut acc = Accumulator::default();
        acc.push(start()).unwrap();
        assert!(acc.push(invalid).is_err());
    }
    for invalid in [
        block_start(0, tool()),
        input(1, "{}"),
        delta("tool_use", 3),
        json!({"type":"message_stop"}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}}),
    ] {
        let mut acc = Accumulator::default();
        acc.push(start()).unwrap();
        acc.push(block_start(0, tool())).unwrap();
        assert!(acc.push(invalid).is_err());
    }
}

#[test]
fn incomplete_tool_json_and_truncation_never_emit_a_reply() {
    for args in ["{\"path\":", "[]", "null"] {
        let mut acc = Accumulator::default();
        acc.push(start()).unwrap();
        acc.push(block_start(0, tool())).unwrap();
        acc.push(input(0, args)).unwrap();
        assert!(acc.push(block_stop(0)).is_err());
    }
    let mut acc = Accumulator::default();
    for event in [
        start(),
        block_start(0, tool()),
        input(0, "{}"),
        block_stop(0),
    ] {
        assert!(
            !acc.push(event)
                .unwrap()
                .iter()
                .any(|e| matches!(e, ModelEvent::Reply { .. }))
        );
    }
    assert!(acc.push(delta("max_tokens", 3)).is_err());
}

#[test]
fn usage_rejects_negative_overflow_and_decreasing_cumulative_counts() {
    for usage in [
        json!({"input_tokens":-1,"output_tokens":1}),
        json!({"input_tokens":u64::MAX,"cache_read_input_tokens":1,"output_tokens":1}),
        json!({"input_tokens":1,"cache_creation_input_tokens":"bad","output_tokens":1}),
    ] {
        let mut body = message(json!([{"type":"text","text":"done"}]), "end_turn");
        body["usage"] = usage;
        assert!(parse_reply(body).is_err());
    }
    let mut acc = Accumulator::default();
    acc.push(start()).unwrap();
    acc.push(delta("end_turn", 5)).unwrap();
    assert!(acc.push(delta("end_turn", 3)).is_err());
}

#[test]
fn missing_stream_input_usage_remains_unknown_instead_of_fabricating_zero() {
    let mut first = start();
    first["message"].as_object_mut().unwrap().remove("usage");
    let mut acc = Accumulator::default();
    let mut events = vec![];
    for event in [
        first,
        block_start(0, json!({"type":"text","text":"done"})),
        block_stop(0),
        delta("end_turn", 3),
        json!({"type":"message_stop"}),
    ] {
        events.extend(acc.push(event).unwrap());
    }
    let ModelEvent::Reply { reply } = events.pop().unwrap() else {
        panic!()
    };
    assert!(reply.usage.is_none());
}

#[test]
fn native_provider_dispatch_validates_kind_version_and_config_before_requests() {
    let old = json!({"base_url":"http://127.0.0.1:1/v1","api_key":null});
    assert!(NativeProvider::from_json(&old.to_string()).is_ok());
    assert!(NativeProvider::from_json(&json!({"kind":"unknown"}).to_string()).is_err());
    let config = json!({"kind":"anthropic","base_url":"http://127.0.0.1:1/v1","api_key":null});
    assert!(NativeProvider::from_json(&config.to_string()).is_ok());
    for (key, value) in [
        ("api_version", json!("\nbad")),
        ("default_max_output_tokens", json!(0)),
        ("timeout_ms", json!(0)),
        ("base_url", json!("file:///tmp")),
        ("typo", json!(true)),
    ] {
        let mut config = config.clone();
        config[key] = value;
        assert!(NativeProvider::from_json(&config.to_string()).is_err());
    }
}
