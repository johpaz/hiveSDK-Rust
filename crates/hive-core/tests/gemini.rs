use hive_agent_core::provider::{
    ModelEvent, NativeProvider, anthropic,
    gemini::{Accumulator, parse_reply, request_body, route},
};
use hive_agent_core::sessions::{OperationResult, SessionStore};
use hive_agent_core::{Action, Engine, LoopPolicy, ModelReply, ModelRequest, ProviderState, Tool};
use serde_json::{Value, json};

fn parts() -> Value {
    json!([{"text":"checking","thoughtSignature":"text-sig"},{"functionCall":{"id":"remote","name":"read","args":{}},"thoughtSignature":"tool-sig"}])
}
fn reply(parts: Value) -> Value {
    json!({"candidates":[{"index":0,"content":{"role":"model","parts":parts},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"cachedContentTokenCount":4,"candidatesTokenCount":2,"thoughtsTokenCount":1}})
}
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
fn request(state: &Engine) -> &ModelRequest {
    let Action::Model { request } = state.action() else {
        panic!()
    };
    request
}
fn direct() -> ModelRequest {
    serde_json::from_value(
        json!({"model":"test","messages":[{"role":"user","content":"Read"}],"tools":[]}),
    )
    .unwrap()
}

#[test]
fn signed_parts_survive_the_engine_and_map_to_native_wire_without_reconstruction() {
    let mut state = engine(LoopPolicy {
        max_output_tokens: Some(5),
        ..Default::default()
    });
    state.start("Read".into()).unwrap();
    state
        .accept_reply(parse_reply(reply(parts())).unwrap())
        .unwrap();
    state.accept_tool("remote", json!(42)).unwrap();
    let body = request_body(request(&state), 9).unwrap();
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 5);
    assert_eq!(
        body["systemInstruction"]["parts"],
        json!([{"text":"System"}])
    );
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
        json!({"type":"object"})
    );
    assert_eq!(body["contents"][1]["parts"], parts());
    assert_eq!(
        body["contents"][2]["parts"],
        json!([{"functionResponse":{"id":"remote","name":"read","response":{"result":42}}}])
    );
    for key in [
        "model",
        "stream",
        "context",
        "deadline_unix_ms",
        "provider_state",
    ] {
        assert!(body.get(key).is_none());
    }
}

#[test]
fn absent_wire_ids_get_local_ids_but_are_not_fabricated_in_function_responses() {
    let p = json!([{"functionCall":{"name":"read","args":{}}},{"functionCall":{"name":"read"}}]);
    let response = parse_reply(reply(p.clone())).unwrap();
    assert_eq!(response.tool_calls[0].id, "gemini-call-0");
    assert_eq!(response.tool_calls[1].id, "gemini-call-1");
    let mut state = engine(Default::default());
    state.start("Read".into()).unwrap();
    state.accept_reply(response).unwrap();
    state
        .accept_tool("gemini-call-0", json!({"error":{"message":"missing"}}))
        .unwrap();
    state.accept_tool("gemini-call-1", json!([1, 2])).unwrap();
    let body = request_body(request(&state), 9).unwrap();
    assert_eq!(body["contents"][1]["parts"], p);
    assert_eq!(body["contents"][2]["parts"].as_array().unwrap().len(), 2);
    assert!(
        body["contents"][2]["parts"][0]["functionResponse"]
            .get("id")
            .is_none()
    );
    assert_eq!(
        body["contents"][2]["parts"][1]["functionResponse"]["response"],
        json!({"result":[1,2]})
    );
}

#[test]
fn thought_text_is_hidden_but_signed_empty_and_signature_only_parts_are_preserved() {
    let p = json!([{"thought":true,"text":"private summary","thoughtSignature":"hidden"},{"text":"visible"},{"text":"","thoughtSignature":"empty"},{"thoughtSignature":"only"}]);
    let response = parse_reply(reply(p.clone())).unwrap();
    assert_eq!(response.content.as_deref(), Some("visible"));
    assert_eq!(
        response.provider_state,
        Some(ProviderState::Gemini {
            version: 1,
            parts: p.as_array().unwrap().clone()
        })
    );
    let mut state = engine(Default::default());
    state.start("Read".into()).unwrap();
    state.accept_reply(response).unwrap();
    state.start("Continue".into()).unwrap();
    assert_eq!(
        request_body(request(&state), 9).unwrap()["contents"][1]["parts"],
        p
    );
}

#[test]
fn usage_counts_cached_prompt_once_and_adds_thought_and_tool_prompt_tokens() {
    let mut value = reply(json!([{"text":"done"}]));
    value["usageMetadata"]["toolUsePromptTokenCount"] = json!(3);
    let usage = parse_reply(value).unwrap().usage.unwrap();
    assert_eq!(usage.input_tokens, 13);
    assert_eq!(usage.output_tokens, 3);
    for metadata in [
        json!({"promptTokenCount":-1,"candidatesTokenCount":1}),
        json!({"promptTokenCount":1,"cachedContentTokenCount":2,"candidatesTokenCount":1}),
        json!({"promptTokenCount":1,"candidatesTokenCount":u64::MAX,"thoughtsTokenCount":1}),
    ] {
        let mut value = reply(json!([{"text":"done"}]));
        value["usageMetadata"] = metadata;
        assert!(parse_reply(value).is_err());
    }
    let mut value = reply(json!([{"text":"done"}]));
    value.as_object_mut().unwrap().remove("usageMetadata");
    assert!(parse_reply(value).unwrap().usage.is_none());
}

#[test]
fn sse_waits_for_clean_eof_and_merges_usage_snapshots_without_duplicate_consumption() {
    let mut acc = Accumulator::default();
    let events=acc.push(json!({"candidates":[{"content":{"parts":[{"text":"A"}]}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":1}})).unwrap();
    assert!(!events.iter().any(|e| matches!(e, ModelEvent::Reply { .. })));
    acc.push(json!({"candidates":[{"content":{"parts":[{"text":"🐝","thoughtSignature":"tail"}]},"finishReason":"STOP"}],"usageMetadata":{"candidatesTokenCount":2,"thoughtsTokenCount":1}})).unwrap();
    acc.push(json!({"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":3,"thoughtsTokenCount":1}})).unwrap();
    let response = acc.finish().unwrap();
    assert_eq!(response.content.as_deref(), Some("A🐝"));
    assert_eq!(response.usage.unwrap().output_tokens, 4);
    let ProviderState::Gemini { parts, .. } = response.provider_state.unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[1]["thoughtSignature"], "tail");
}

#[test]
fn terminal_errors_preserve_the_reason_and_token_counts_without_response_content() {
    for (field, reason) in [
        ("finishReason", "MAX_TOKENS"),
        ("finishReason", "SAFETY"),
        ("finishReason", "MALFORMED_FUNCTION_CALL"),
        ("finishReason", "NEW_PROVIDER_REASON"),
    ] {
        let mut value = reply(json!([{"text":"private response"}]));
        value["candidates"][0][field] = json!(reason);
        let error = parse_reply(value).unwrap_err().to_string();
        assert!(error.contains(reason), "{error}");
        assert!(error.contains("candidates=2"), "{error}");
        assert!(error.contains("thoughts=1"), "{error}");
        assert!(!error.contains("private response"));
    }
    let error = parse_reply(json!({"promptFeedback":{"blockReason":"SAFETY","blockReasonMessage":"private prompt"},"candidates":[]})).unwrap_err().to_string();
    assert!(error.contains("blockReason=SAFETY"), "{error}");
    assert!(!error.contains("private prompt"));
}

#[test]
fn streaming_terminal_errors_include_cumulative_usage_and_remain_unconfirmed() {
    let mut acc = Accumulator::default();
    acc.push(json!({"candidates":[{"content":{"parts":[{"text":"partial"}]}}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":2,"thoughtsTokenCount":2046}})).unwrap();
    let error = acc
        .push(json!({"candidates":[{"finishReason":"MAX_TOKENS"}]}))
        .unwrap_err()
        .to_string();
    assert!(error.contains("finishReason=MAX_TOKENS"), "{error}");
    assert!(error.contains("thoughts=2046"), "{error}");
    assert!(acc.finish().is_err());
    for reason in [
        json!("private response\nsecret"),
        json!("A".repeat(1000)),
        json!({"secret":"private response"}),
    ] {
        let mut value = reply(json!([]));
        value["candidates"][0]["finishReason"] = reason;
        let error = parse_reply(value).unwrap_err().to_string();
        assert!(error.contains("finishReason=INVALID"), "{error}");
        assert!(!error.contains("private response"));
        assert!(error.len() < 512);
    }
}

#[test]
fn streams_without_stop_and_invalid_finish_reasons_or_blocked_prompts_fail() {
    let mut acc = Accumulator::default();
    acc.push(json!({"candidates":[{"content":{"parts":parts()}}]}))
        .unwrap();
    assert!(acc.finish().is_err());
    for reason in [
        "MAX_TOKENS",
        "SAFETY",
        "RECITATION",
        "MALFORMED_FUNCTION_CALL",
        "CONTINUATION",
    ] {
        let mut value = reply(parts());
        value["candidates"][0]["finishReason"] = json!(reason);
        assert!(parse_reply(value).is_err());
    }
    assert!(
        parse_reply(json!({"promptFeedback":{"blockReason":"SAFETY"},"candidates":[]})).is_err()
    );
    let mut value = reply(parts());
    value["candidates"].as_array_mut().unwrap().push(json!({}));
    assert!(parse_reply(value).is_err());
}

#[test]
fn malformed_calls_unsupported_parts_and_inconsistent_candidates_fail_before_tools() {
    for p in [
        json!([{"inlineData":{}}]),
        json!([{"functionCall":{"name":"read","args":[]}}]),
        json!([{"functionCall":{"name":"read","partialArgs":[]}}]),
        json!([{"text":"x","functionCall":{"name":"read"}}]),
        json!([{"thoughtSignature":false}]),
        json!([{"functionCall":{"id":"c","name":"read"}},{"functionCall":{"id":"c","name":"read"}}]),
    ] {
        assert!(parse_reply(reply(p)).is_err());
    }
    let mut value = reply(parts());
    value["candidates"][0]["citationMetadata"] = json!({});
    assert!(parse_reply(value).is_err());
    let mut value = reply(parts());
    value["candidates"][0]["index"] = json!(1);
    assert!(parse_reply(value).is_err());
    let mut acc = Accumulator::default();
    acc.push(reply(parts())).unwrap();
    assert!(
        acc.push(json!({"candidates":[{"content":{"parts":[{"text":"after"}]}}]}))
            .is_err()
    );
}

#[test]
fn mismatched_or_unknown_provider_state_is_rejected_atomically() {
    let mut state = engine(Default::default());
    state.start("Read".into()).unwrap();
    let before = serde_json::to_value(&state).unwrap();
    let mut response = parse_reply(reply(parts())).unwrap();
    response.tool_calls[0].arguments = json!({"forged":true});
    assert!(state.accept_reply(response).is_err());
    assert_eq!(serde_json::to_value(&state).unwrap(), before);
    let mut response = parse_reply(reply(parts())).unwrap();
    if let Some(ProviderState::Gemini { version, .. }) = &mut response.provider_state {
        *version = 2;
    }
    assert!(state.accept_reply(response).is_err());
    assert!(
        serde_json::from_value::<ProviderState>(
            json!({"provider":"unknown","version":1,"parts":[]})
        )
        .is_err()
    );
}

#[test]
fn signed_state_and_tool_results_survive_durable_checkpoint_resume() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let frame = store
        .begin_run("a", "s", 0, engine(Default::default()), "Read".into())
        .unwrap();
    let frame = store.claim("a", "s", frame.revision).unwrap();
    let frame = store
        .advance(
            "a",
            "s",
            frame.revision,
            &frame.operation.unwrap().execution_id,
            OperationResult::Model {
                reply: parse_reply(reply(parts())).unwrap(),
            },
        )
        .unwrap();
    let frame = store.claim("a", "s", frame.revision).unwrap();
    let frame = store
        .advance(
            "a",
            "s",
            frame.revision,
            &frame.operation.unwrap().execution_id,
            OperationResult::Tool { result: json!(42) },
        )
        .unwrap();
    let expected = request_body(request(&frame.state), 9).unwrap();
    drop(store);
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let resumed = store.resume_run("a", "s", frame.revision).unwrap();
    assert_eq!(request_body(request(&resumed.state), 9).unwrap(), expected);
    assert_eq!(expected["contents"][1]["parts"], parts());
    assert!(anthropic::request_body(request(&resumed.state), 9).is_err());
}

#[test]
fn signed_current_turn_survives_context_trimming_while_old_turns_are_omitted_whole() {
    let mut state = engine(LoopPolicy {
        max_context_bytes: Some(1500),
        context_overflow: hive_agent_core::ContextOverflow::DropOldestTurns,
        ..Default::default()
    });
    state.start("old ".repeat(200)).unwrap();
    state
        .accept_reply(
            parse_reply(reply(json!([{"text":"finished","thoughtSignature":"old"}]))).unwrap(),
        )
        .unwrap();
    state.start("current ".repeat(70)).unwrap();
    state
        .accept_reply(parse_reply(reply(parts())).unwrap())
        .unwrap();
    state.accept_tool("remote", json!(42)).unwrap();
    assert!(request(&state).context.as_ref().unwrap().dropped_messages >= 2);
    let body = request_body(request(&state), 9).unwrap();
    assert_eq!(body["contents"][1]["parts"], parts());
    assert_eq!(state.messages().len(), 6);
}

#[test]
fn routes_defaults_and_native_config_are_explicit_and_invalid_paths_never_become_urls() {
    assert_eq!(
        route("models/test", true).unwrap(),
        "models/test:streamGenerateContent?alt=sse"
    );
    for model in [
        "",
        "../test",
        "test?key=x",
        "models/test/extra",
        "test#fragment",
    ] {
        assert!(route(model, false).is_err());
    }
    let req = direct();
    assert_eq!(
        request_body(&req, 9).unwrap()["generationConfig"]["maxOutputTokens"],
        9
    );
    let config = json!({"kind":"gemini","base_url":"http://127.0.0.1:1/v1beta","api_key":null});
    assert!(NativeProvider::from_json(&config.to_string()).is_ok());
    for (key, value) in [
        ("default_max_output_tokens", json!(0)),
        ("base_url", json!("file:///tmp")),
        ("typo", json!(true)),
    ] {
        let mut config = config.clone();
        config[key] = value;
        assert!(NativeProvider::from_json(&config.to_string()).is_err());
    }
    let mut req = direct();
    req.messages[0].role = "tool".into();
    assert!(request_body(&req, 9).is_err());
}

#[test]
fn missing_provider_state_fields_in_old_replies_and_messages_default_to_none() {
    let response: ModelReply =
        serde_json::from_value(json!({"content":"done","tool_calls":[]})).unwrap();
    assert!(response.provider_state.is_none());
    let mut state = engine(Default::default());
    state.start("Read".into()).unwrap();
    state.accept_reply(response).unwrap();
    state.start("Again".into()).unwrap();
    assert!(state.messages().iter().all(|m| m.provider_state.is_none()));
    assert!(request_body(request(&state), 9).is_ok());
}
