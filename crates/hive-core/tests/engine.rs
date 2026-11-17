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
    let body = request_body(request).unwrap();
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

#[test]
fn a_reply_with_reasoning_and_a_tool_call_is_accepted_and_the_reasoning_comes_back_in_the_next_request()
 {
    use hive_agent_core::ProviderState;
    let mut e = engine(LoopPolicy::default(), 8);
    let mut reply = call("a");
    reply.provider_state = Some(ProviderState::Reasoning {
        content: "primero busco".into(),
    });
    // This is what a real reasoning model (DeepSeek, NVIDIA) sends next to a tool call.
    e.accept_reply(reply)
        .expect("the engine accepts a reply that carries reasoning");
    e.accept_tool("a", json!({"answer":42})).unwrap();
    let Action::Model { request } = e.action() else {
        panic!("expected model");
    };
    let assistant = request
        .messages
        .iter()
        .find(|m| m.role == "assistant")
        .unwrap();
    assert_eq!(
        assistant.provider_state,
        Some(ProviderState::Reasoning {
            content: "primero busco".into()
        })
    );
    // Through the OpenAI wire body it is `reasoning_content` (the profile decides who gets it).
    let body = request_body(request).unwrap();
    assert_eq!(body["messages"][2]["reasoning_content"], "primero busco");
    e.accept_reply(final_reply()).unwrap();
    // An empty note is refused, not stored.
    let mut e = engine(LoopPolicy::default(), 8);
    let mut bad = call("a");
    bad.provider_state = Some(ProviderState::Reasoning {
        content: String::new(),
    });
    assert!(e.accept_reply(bad).is_err());
}

const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
fn shot(width: u32) -> serde_json::Value {
    json!({"mime_type":"image/png","width":width,"data":PNG})
}
fn image_engine(policy: LoopPolicy) -> Engine {
    engine(
        LoopPolicy {
            image_tools: vec!["lookup".into()],
            max_tool_calls: 32,
            max_identical_tool_results: None,
            ..policy
        },
        16,
    )
}
fn with_images(e: &Engine) -> Vec<usize> {
    e.messages()
        .iter()
        .enumerate()
        .filter(|(_, m)| !m.attachments.is_empty())
        .map(|(i, _)| i)
        .collect()
}

#[test]
fn a_picture_in_an_image_tools_result_reaches_the_model_as_an_image_not_as_base64_text() {
    let mut e = image_engine(LoopPolicy::default());
    e.accept_reply(call("a")).unwrap();
    e.accept_tool("a", shot(640)).unwrap();
    let messages = e.messages();
    let tool = &messages[messages.len() - 2];
    assert_eq!(tool.role, "tool");
    let text = tool.content.as_deref().unwrap();
    assert!(
        !text.contains(PNG) && text.contains("\"image_attached\":true") && text.contains("640"),
        "{text}"
    );
    let image = &messages[messages.len() - 1];
    assert_eq!(image.role, "user");
    assert!(image.content.as_deref().unwrap().contains("`lookup`"));
    assert_eq!(image.attachments.len(), 1);
    assert_eq!(image.attachments[0].media_type, "image/png");
    assert_eq!(image.attachments[0].data.as_deref(), Some(PNG));
    // The next request carries it, and the state survives a round trip.
    let Action::Model { request } = e.action() else {
        panic!("expected model")
    };
    assert_eq!(request.messages.last().unwrap().attachments.len(), 1);
    let saved = serde_json::to_string(&e).unwrap();
    let restored: Engine = serde_json::from_str(&saved).unwrap();
    assert_eq!(with_images(&restored), with_images(&e));
}

#[test]
fn pictures_of_a_batch_come_after_all_its_results_so_providers_accept_the_order() {
    let mut e = image_engine(LoopPolicy::default());
    let mut reply = call("a");
    reply.tool_calls.push(call("b").tool_calls.remove(0));
    e.accept_reply(reply).unwrap();
    e.accept_tool("a", shot(1)).unwrap();
    assert!(
        with_images(&e).is_empty(),
        "held until the batch is complete"
    );
    e.accept_tool("b", shot(2)).unwrap();
    let roles: Vec<&str> = e.messages().iter().map(|m| m.role.as_str()).collect();
    assert_eq!(
        &roles[roles.len() - 4..],
        ["tool", "tool", "user", "user"],
        "{roles:?}"
    );
}

#[test]
fn only_the_newest_pictures_stay_in_the_context() {
    let mut e = image_engine(LoopPolicy::default());
    for (i, id) in ["a", "b", "c", "d"].iter().enumerate() {
        e.accept_reply(call(id)).unwrap();
        e.accept_tool(id, shot(i as u32)).unwrap();
    }
    let shown = with_images(&e);
    assert_eq!(shown.len(), 2, "the default keeps the last two");
    let old: Vec<&str> = e
        .messages()
        .iter()
        .filter(|m| m.role == "user" && m.attachments.is_empty())
        .filter_map(|m| m.content.as_deref())
        .filter(|c| c.contains("removed from the context"))
        .collect();
    assert_eq!(old.len(), 2);
    let mut one = image_engine(LoopPolicy {
        max_tool_images: Some(1),
        ..Default::default()
    });
    for id in ["a", "b"] {
        one.accept_reply(call(id)).unwrap();
        one.accept_tool(id, shot(1)).unwrap();
    }
    assert_eq!(with_images(&one).len(), 1);
}

#[test]
fn only_declared_tools_can_attach_pictures_and_odd_results_stay_text() {
    // Not declared: the same result stays as it came.
    let mut plain = engine(LoopPolicy::default(), 8);
    plain.accept_reply(call("a")).unwrap();
    plain.accept_tool("a", shot(1)).unwrap();
    assert!(with_images(&plain).is_empty());
    assert!(
        plain
            .messages()
            .last()
            .unwrap()
            .content
            .as_deref()
            .unwrap()
            .contains(PNG)
    );
    // Declared, but not a picture / not valid.
    for result in [
        json!({"text":"hi"}),
        json!({"mime_type":"text/plain","data":"aGk="}),
        json!({"mime_type":"image/png","data":"not base64!"}),
        json!("string"),
    ] {
        let mut e = image_engine(LoopPolicy::default());
        e.accept_reply(call("a")).unwrap();
        e.accept_tool("a", result).unwrap();
        assert!(with_images(&e).is_empty());
    }
}

#[test]
fn a_token_budget_without_an_attachment_allowance_leaves_the_picture_out_and_says_so() {
    use hive_agent_core::{InputTokenBudget, TokenEncoding};
    let budget: InputTokenBudget =
        serde_json::from_value(json!({"encoding":"cl100k_base","max_tokens":100000})).unwrap();
    let _ = TokenEncoding::Cl100kBase;
    let mut e = image_engine(LoopPolicy {
        input_token_budget: Some(budget),
        ..Default::default()
    });
    e.accept_reply(call("a")).unwrap();
    e.accept_tool("a", shot(1)).unwrap();
    assert!(with_images(&e).is_empty());
    let tool = e
        .messages()
        .iter()
        .rev()
        .find(|m| m.role == "tool")
        .unwrap();
    assert!(tool.content.as_deref().unwrap().contains("image_omitted"));
    assert!(
        matches!(e.action(), Action::Model { .. }),
        "the run continues"
    );
}

#[test]
fn image_tools_must_name_tools_of_the_engine_and_the_retention_is_bounded() {
    for policy in [
        LoopPolicy {
            image_tools: vec!["nope".into()],
            ..Default::default()
        },
        LoopPolicy {
            max_tool_images: Some(0),
            ..Default::default()
        },
        LoopPolicy {
            max_tool_images: Some(17),
            ..Default::default()
        },
    ] {
        assert!(Engine::with_policy("test".into(), None, vec![tool()], 4, policy).is_err());
    }
}

#[test]
fn every_provider_adapter_sends_the_tool_picture_as_an_image_after_the_tool_result() {
    let mut e = image_engine(LoopPolicy::default());
    e.accept_reply(call("a")).unwrap();
    e.accept_tool("a", shot(7)).unwrap();
    let Action::Model { request } = e.action() else {
        panic!("expected model")
    };
    // OpenAI-compatible: the tool message, then a user message with an image part.
    let openai = request_body(request).unwrap();
    let messages = openai["messages"].as_array().unwrap();
    assert_eq!(messages[messages.len() - 2]["role"], "tool");
    let last = &messages[messages.len() - 1];
    assert_eq!(last["role"], "user");
    assert!(
        last["content"].to_string().contains("image_url")
            && last["content"].to_string().contains(PNG),
        "{last}"
    );
    // Anthropic.
    let anthropic = hive_agent_core::provider::anthropic::request_body(request, 100).unwrap();
    let text = anthropic["messages"].to_string();
    assert!(
        text.contains("tool_result") && text.contains("\"type\":\"image\"") && text.contains(PNG),
        "{text}"
    );
    assert!(text.find("tool_result").unwrap() < text.find("\"type\":\"image\"").unwrap());
    // Gemini.
    let gemini = hive_agent_core::provider::gemini::request_body(request, 100).unwrap();
    let text = gemini["contents"].to_string();
    assert!(
        text.contains("functionResponse") && text.contains("inlineData") && text.contains(PNG),
        "{text}"
    );
    assert!(text.find("functionResponse").unwrap() < text.find("inlineData").unwrap());
}
