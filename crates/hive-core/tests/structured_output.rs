use hive_agent_core::objective::ObjectivePolicy;
use hive_agent_core::sessions::{OperationResult, SessionStore};
use hive_agent_core::{
    Action, Engine, InputTokenBudget, InputTokenFraming, LoopPolicy, ModelReply, ModelRequest,
    StopReason, StructuredOutput, TokenEncoding, Tool, ToolCall, Usage,
};
use serde_json::json;

fn output() -> StructuredOutput {
    StructuredOutput {
        name: "answer_v1".into(),
        schema: json!({"type":"object","properties":{"answer":{"type":"string","minLength":2}},"required":["answer"],"additionalProperties":false}),
    }
}
fn policy() -> LoopPolicy {
    LoopPolicy {
        structured_output: Some(output()),
        max_output_tokens: Some(8),
        max_total_output_tokens: Some(20),
        ..Default::default()
    }
}
fn engine(policy: LoopPolicy) -> Engine {
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
fn request(state: &Engine) -> &ModelRequest {
    let Action::Model { request } = state.action() else {
        panic!("expected model")
    };
    request
}
fn reply(content: &str) -> ModelReply {
    ModelReply {
        provider_state: None,
        content: Some(content.into()),
        tool_calls: vec![],
        usage: Some(Usage {
            input_tokens: 3,
            output_tokens: 2,
        }),
    }
}
fn input_text(request: &ModelRequest) -> String {
    format!(
        "{{\"model\":{},\"messages\":{},\"tools\":{},\"structured_output\":{}}}",
        serde_json::to_string(&request.model).unwrap(),
        serde_json::to_string(&request.messages).unwrap(),
        serde_json::to_string(&request.tools).unwrap(),
        serde_json::to_string(&request.structured_output.as_ref().unwrap()).unwrap()
    )
}

#[test]
fn adapters_map_the_explicit_schema_without_rewriting_its_constraints() {
    let mut state = engine(policy());
    state.start("Answer 🦀".into()).unwrap();
    let req = request(&state);
    let openai = hive_agent_core::provider::request_body(req).unwrap();
    assert_eq!(
        openai["response_format"],
        json!({"type":"json_schema","json_schema":{"name":"answer_v1","schema":output().schema,"strict":true}})
    );
    let anthropic = hive_agent_core::provider::anthropic::request_body(req, 99).unwrap();
    assert_eq!(
        anthropic["output_config"],
        json!({"format":{"type":"json_schema","schema":output().schema}})
    );
    assert!(anthropic.get("response_format").is_none());
    let gemini = hive_agent_core::provider::gemini::request_body(req, 99).unwrap();
    assert_eq!(
        gemini["generationConfig"]["responseMimeType"],
        "application/json"
    );
    assert_eq!(
        gemini["generationConfig"]["responseJsonSchema"],
        output().schema
    );
    assert_eq!(gemini["generationConfig"]["maxOutputTokens"], 8);
    let mut plain = engine(Default::default());
    plain.start("Plain".into()).unwrap();
    assert!(
        hive_agent_core::provider::request_body(request(&plain))
            .unwrap()
            .get("response_format")
            .is_none()
    );
}

#[test]
fn invalid_final_output_is_known_kept_and_terminal_without_a_repair_call() {
    for content in [
        "not JSON",
        "```json\n{\"answer\":\"ok\"}\n```",
        "{}",
        "null",
        "{\"answer\":2}",
        "{\"answer\":\"x\"}",
        "{\"answer\":\"ok\",\"extra\":1}",
    ] {
        let mut state = engine(policy());
        state.start("Answer".into()).unwrap();
        state.accept_reply(reply(content)).unwrap();
        assert!(
            matches!(state.action(),Action::Stopped {reason:StopReason::InvalidStructuredOutput,stats} if stats.model_calls==1&&stats.input_tokens==3&&stats.output_tokens==2&&stats.reserved_output_tokens==0)
        );
        assert_eq!(
            state.messages().last().unwrap().content.as_deref(),
            Some(content)
        );
        let before = serde_json::to_value(&state).unwrap();
        state.preflight().unwrap();
        assert_eq!(before, serde_json::to_value(&state).unwrap());
        assert!(state.start("Retry automatically".into()).is_err());
    }
    let mut state = engine(policy());
    state.start("Answer".into()).unwrap();
    let content = " \n{\"answer\":\"sí🦀\"}\t";
    state.accept_reply(reply(content)).unwrap();
    assert!(matches!(state.action(),Action::Complete {content:text,..} if text==content));
}

#[test]
fn malformed_schemas_names_and_external_references_fail_at_construction() {
    for value in [
        json!({"name":"","schema":{}}),
        json!({"name":"bad.name","schema":{}}),
        json!({"name":"ok","schema":true}),
        json!({"name":"ok","schema":{"type":"nonsense"}}),
        json!({"name":"ok","schema":{"$ref":"https://example.com/private-schema"}}),
        json!({"name":"ok","schema":{"description":"x".repeat(65537)}}),
    ] {
        let output: StructuredOutput = serde_json::from_value(value).unwrap();
        assert!(
            Engine::with_policy(
                "test".into(),
                None,
                vec![],
                8,
                LoopPolicy {
                    structured_output: Some(output),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
}

#[test]
fn intermediate_tool_text_does_not_need_to_be_final_json() {
    let mut state = engine(policy());
    state.start("Read".into()).unwrap();
    state
        .accept_reply(ModelReply {
            provider_state: None,
            content: Some("I will read it".into()),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: json!({}),
            }],
            usage: Some(Usage {
                input_tokens: 2,
                output_tokens: 1,
            }),
        })
        .unwrap();
    assert!(matches!(state.action(), Action::Tool { .. }));
    state.accept_tool("c", json!(5)).unwrap();
    assert!(request(&state).structured_output.is_some());
    state.accept_reply(reply("{\"answer\":\"ok\"}")).unwrap();
    assert!(
        matches!(state.action(),Action::Complete {stats,..} if stats.model_calls==2&&stats.tool_calls==1)
    );
}

#[test]
fn schema_bytes_are_pinned_and_cannot_be_omitted_to_fit_input() {
    let mut sample = engine(policy());
    sample.start("Answer".into()).unwrap();
    let bytes = input_text(request(&sample)).len() as u64;
    for (limit, fits) in [(bytes, true), (bytes - 1, false)] {
        let mut state = engine(LoopPolicy {
            max_context_bytes: Some(limit),
            ..policy()
        });
        state.start("Answer".into()).unwrap();
        if fits {
            assert_eq!(
                request(&state).context.as_ref().unwrap().serialized_bytes,
                bytes
            );
        } else {
            assert!(
                matches!(state.action(),Action::Stopped {reason:StopReason::ContextLimit,stats} if stats.model_calls==0&&stats.reserved_output_tokens==0)
            );
        }
    }
}

#[test]
fn every_framing_counts_the_schema_including_gemini_generation_config() {
    for framing in [
        InputTokenFraming::NeutralJson,
        InputTokenFraming::OpenaiChatJson,
        InputTokenFraming::AnthropicMessagesJson,
        InputTokenFraming::GeminiGenerateContentJson,
    ] {
        let mut state = engine(LoopPolicy {
            input_token_budget: Some(InputTokenBudget {
                encoding: TokenEncoding::Cl100kBase,
                max_tokens: 10000,
                overhead_tokens: 7,
                framing,
                max_total_tokens: None,
                context_window_tokens: None,
                attachment_tokens: None,
            }),
            ..policy()
        });
        state.start("Answer".into()).unwrap();
        let req = request(&state);
        let text = if framing == InputTokenFraming::NeutralJson {
            input_text(req)
        } else {
            let mut body = match framing {
                InputTokenFraming::OpenaiChatJson => {
                    hive_agent_core::provider::request_body(req).unwrap()
                }
                InputTokenFraming::AnthropicMessagesJson => {
                    hive_agent_core::provider::anthropic::request_body(req, 1).unwrap()
                }
                _ => hive_agent_core::provider::gemini::request_body(req, 1).unwrap(),
            };
            for key in ["max_tokens", "max_completion_tokens", "stream"] {
                body.as_object_mut().unwrap().remove(key);
            }
            if framing == InputTokenFraming::GeminiGenerateContentJson {
                let config = body["generationConfig"].as_object_mut().unwrap();
                config.remove("maxOutputTokens");
                config.remove("candidateCount");
                assert!(config.contains_key("responseJsonSchema"));
            }
            body.to_string()
        };
        let expected = tiktoken_rs::cl100k_base_singleton()
            .encode_ordinary(&text)
            .len() as u64
            + 7;
        assert_eq!(
            req.context
                .as_ref()
                .unwrap()
                .token_estimate
                .as_ref()
                .unwrap()
                .total_tokens,
            expected
        );
    }
}

#[test]
fn durable_invalid_json_commits_known_usage_and_reopens_without_another_model() {
    let directory = tempfile::tempdir().unwrap();
    let store = SessionStore::open(directory.path(), "t".into()).unwrap();
    store
        .open_objective("root", ObjectivePolicy::default())
        .unwrap();
    let mut initial = engine(policy());
    initial.bind_objective("root".into()).unwrap();
    let start = store
        .begin_run("a", "s", 0, initial.clone(), "Answer".into())
        .unwrap();
    let model = store.claim("a", "s", start.revision).unwrap();
    let stopped = store
        .advance(
            "a",
            "s",
            model.revision,
            &model.operation.unwrap().execution_id,
            OperationResult::Model {
                reply: reply("not JSON"),
            },
        )
        .unwrap();
    assert!(stopped.operation.is_none());
    assert!(matches!(
        stopped.action,
        Action::Stopped {
            reason: StopReason::InvalidStructuredOutput,
            ..
        }
    ));
    assert_eq!(
        store.inspect_objective("root").unwrap()["reservations"],
        json!({})
    );
    drop(store);
    let store = SessionStore::open(directory.path(), "t".into()).unwrap();
    let loaded = store.load_run("a", "s", &initial).unwrap();
    let resumed = store.resume_run("a", "s", loaded.revision).unwrap();
    assert_eq!(
        resumed.state.messages().last().unwrap().content.as_deref(),
        Some("not JSON")
    );
    assert!(store.claim("a", "s", resumed.revision).is_err());
    store
        .commit("a", "s", resumed.revision, resumed.state)
        .unwrap();
    assert_eq!(store.inspect_objective("root").unwrap()["model_calls"], 1);
    let mut changed = policy();
    changed.structured_output.as_mut().unwrap().name = "changed".into();
    let mut changed = engine(changed);
    changed.bind_objective("root".into()).unwrap();
    assert!(store.load("a", "s", &changed).is_err());
}

#[test]
fn old_records_default_to_unstructured_and_schema_metadata_is_not_sent_as_a_prompt() {
    let mut state = engine(Default::default());
    state.start("Plain".into()).unwrap();
    let saved = serde_json::to_value(&state).unwrap();
    assert!(saved["policy"].get("structured_output").is_none());
    let mut restored: Engine = serde_json::from_value(saved.clone()).unwrap();
    restored.preflight().unwrap();
    assert_eq!(saved, serde_json::to_value(restored).unwrap());
    let mut state = engine(policy());
    state.start("Answer".into()).unwrap();
    assert_eq!(request(&state).messages.len(), 1);
    assert_eq!(
        request(&state).messages[0].content.as_deref(),
        Some("Answer")
    );
}
