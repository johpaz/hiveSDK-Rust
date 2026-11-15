use hive_agent_core::provider::request_body;
use hive_agent_core::sessions::SessionStore;
use hive_agent_core::{
    Action, ContextOverflow, Engine, InputTokenBudget, InputTokenFraming, LoopPolicy, ModelReply,
    ModelRequest, StopReason, TokenEncoding, Tool, ToolCall, Usage,
};
use serde_json::json;

fn policy(encoding: TokenEncoding, max_tokens: u64) -> LoopPolicy {
    LoopPolicy {
        input_token_budget: Some(InputTokenBudget {
            encoding,
            max_tokens,
            overhead_tokens: 7,
            framing: Default::default(),
            max_total_tokens: None,
            context_window_tokens: None,
        }),
        ..Default::default()
    }
}
fn engine(p: LoopPolicy) -> Engine {
    Engine::with_policy(
        "private-model-alias".into(),
        Some("System".into()),
        vec![Tool {
            name: "read".into(),
            description: "Leer 🦀".into(),
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
            allow_repeated_results: false,
        }],
        8,
        p,
    )
    .unwrap()
}
fn request(state: &Engine) -> &ModelRequest {
    let Action::Model { request } = state.action() else {
        panic!("expected model")
    };
    request
}
fn text(req: &ModelRequest) -> String {
    format!(
        "{{\"model\":{},\"messages\":{},\"tools\":{}}}",
        serde_json::to_string(&req.model).unwrap(),
        serde_json::to_string(&req.messages).unwrap(),
        serde_json::to_string(&req.tools).unwrap()
    )
}
fn reference(req: &ModelRequest, encoding: TokenEncoding) -> u64 {
    let bpe = match encoding {
        TokenEncoding::Cl100kBase => tiktoken_rs::cl100k_base_singleton(),
        TokenEncoding::O200kBase => tiktoken_rs::o200k_base_singleton(),
    };
    bpe.encode_ordinary(&text(req)).len() as u64
}
fn finish(state: &mut Engine) {
    state
        .accept_reply(ModelReply {
            provider_state: None,
            content: Some("finished".into()),
            tool_calls: vec![],
            usage: None,
        })
        .unwrap();
}

#[test]
fn explicit_encodings_count_the_full_envelope_with_unicode_escapes_and_literals() {
    for encoding in [TokenEncoding::Cl100kBase, TokenEncoding::O200kBase] {
        let mut state = engine(policy(encoding, 10000));
        state
            .start("Hola 🦀\n\"\\ <|endoftext|> お誕生日おめでとう".into())
            .unwrap();
        let req = request(&state);
        let report = req.context.as_ref().unwrap();
        let estimate = report.token_estimate.as_ref().unwrap();
        assert_eq!(report.serialized_bytes, text(req).len() as u64);
        assert_eq!(estimate.encoding, encoding);
        assert_eq!(estimate.serialized_tokens, reference(req, encoding));
        assert_eq!(estimate.overhead_tokens, 7);
        assert_eq!(estimate.total_tokens, estimate.serialized_tokens + 7);
        assert!(request_body(req).get("context").is_none());
        assert!(request_body(req).get("input_token_budget").is_none());
    }
}

#[test]
fn exact_limit_allows_a_call_but_one_token_less_stops_before_reserving() {
    let encoding = TokenEncoding::Cl100kBase;
    let mut sample = engine(policy(encoding, 10000));
    sample.start("Read".into()).unwrap();
    let limit = reference(request(&sample), encoding) + 7;
    let mut state = engine(policy(encoding, limit));
    state.start("Read".into()).unwrap();
    assert!(matches!(state.action(), Action::Model { .. }));
    let mut state = engine(LoopPolicy {
        max_total_output_tokens: Some(30),
        ..policy(encoding, limit - 1)
    });
    state.start("Read".into()).unwrap();
    assert!(
        matches!(state.action(),Action::Stopped{reason:StopReason::ContextLimit,stats} if stats.model_calls==0&&stats.reserved_output_tokens==0)
    );
}

#[test]
fn overhead_is_budgeted_and_never_reported_as_actual_provider_usage() {
    let mut state = engine(policy(TokenEncoding::O200kBase, 10000));
    state.start("Read".into()).unwrap();
    state
        .accept_reply(ModelReply {
            provider_state: None,
            content: Some("done".into()),
            tool_calls: vec![],
            usage: Some(Usage {
                input_tokens: 123,
                output_tokens: 4,
            }),
        })
        .unwrap();
    assert!(
        matches!(state.action(),Action::Complete{stats,..} if stats.input_tokens==123&&stats.output_tokens==4)
    );
    state.start("Again".into()).unwrap();
    finish(&mut state);
    assert!(
        matches!(state.action(),Action::Complete{stats,..} if stats.input_tokens==0&&stats.unmetered_model_calls==1)
    );
}

#[test]
fn token_budget_alone_drops_old_turns_and_preflight_does_not_change_the_result() {
    let p = LoopPolicy {
        context_overflow: ContextOverflow::DropOldestTurns,
        ..policy(TokenEncoding::Cl100kBase, 160)
    };
    let mut state = engine(p);
    state.start("old ".repeat(60)).unwrap();
    finish(&mut state);
    state.start("current ".repeat(60)).unwrap();
    assert_eq!(
        request(&state).context.as_ref().unwrap().dropped_messages,
        2
    );
    assert_eq!(request(&state).messages.len(), 2);
    assert_eq!(state.messages().len(), 4);
    let before = serde_json::to_value(&state).unwrap();
    state.preflight().unwrap();
    state.preflight().unwrap();
    assert_eq!(serde_json::to_value(state).unwrap(), before);
}

#[test]
fn both_byte_and_token_limits_must_fit_the_same_selection() {
    let p = LoopPolicy {
        max_context_bytes: Some(850),
        context_overflow: ContextOverflow::DropOldestTurns,
        ..policy(TokenEncoding::Cl100kBase, 160)
    };
    let mut state = engine(p);
    state.start("old ".repeat(60)).unwrap();
    finish(&mut state);
    state.start("current ".repeat(60)).unwrap();
    let report = request(&state).context.as_ref().unwrap();
    assert_eq!(report.dropped_messages, 2);
    assert!(report.serialized_bytes <= 850);
    assert!(report.token_estimate.as_ref().unwrap().total_tokens <= 160);
    let mut state = engine(LoopPolicy {
        max_context_bytes: Some(5),
        ..policy(TokenEncoding::Cl100kBase, 10000)
    });
    state.start("Read".into()).unwrap();
    assert!(matches!(
        state.action(),
        Action::Stopped {
            reason: StopReason::ContextLimit,
            ..
        }
    ));
}

#[test]
fn large_active_tool_result_is_kept_without_another_reserved_model_call() {
    let mut state = engine(LoopPolicy {
        max_total_output_tokens: Some(30),
        context_overflow: ContextOverflow::DropOldestTurns,
        ..policy(TokenEncoding::Cl100kBase, 160)
    });
    state.start("Read".into()).unwrap();
    state
        .accept_reply(ModelReply {
            provider_state: None,
            content: None,
            usage: Some(Usage {
                input_tokens: 1,
                output_tokens: 1,
            }),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: json!({}),
            }],
        })
        .unwrap();
    state
        .accept_tool("c", json!("result ".repeat(1000)))
        .unwrap();
    assert!(
        matches!(state.action(),Action::Stopped{reason:StopReason::ContextLimit,stats} if stats.model_calls==1&&stats.tool_calls==1&&stats.reserved_output_tokens==0)
    );
    assert_eq!(state.messages().last().unwrap().role, "tool");
}

#[test]
fn durable_resume_preserves_profile_selection_and_full_history() {
    let p = LoopPolicy {
        context_overflow: ContextOverflow::DropOldestTurns,
        ..policy(TokenEncoding::Cl100kBase, 160)
    };
    let mut initial = engine(p.clone());
    initial.start("old ".repeat(60)).unwrap();
    finish(&mut initial);
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let frame = store
        .begin_run("a", "s", 0, initial, "current ".repeat(60))
        .unwrap();
    let before = serde_json::to_value(&frame.state).unwrap();
    drop(store);
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let frame = store.resume_run("a", "s", frame.revision).unwrap();
    assert_eq!(serde_json::to_value(&frame.state).unwrap(), before);
    assert_eq!(frame.state.messages().len(), 4);
    let mut changed = p;
    changed.input_token_budget.as_mut().unwrap().encoding = TokenEncoding::O200kBase;
    assert!(matches!(
        store.load_run("a", "s", &engine(changed)),
        Err(hive_agent_core::Error::SessionConfigMismatch)
    ));
}

#[test]
fn malformed_token_profiles_fail_at_construction_and_old_records_still_load() {
    for (max_tokens, overhead_tokens) in [(0, 0), (7, 7), (7, 8)] {
        assert!(
            Engine::with_policy(
                "test".into(),
                None,
                vec![],
                8,
                LoopPolicy {
                    input_token_budget: Some(InputTokenBudget {
                        encoding: TokenEncoding::Cl100kBase,
                        max_tokens,
                        overhead_tokens,
                        framing: Default::default(),
                        max_total_tokens: None,
                        context_window_tokens: None,
                    }),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
    assert!(
        serde_json::from_value::<LoopPolicy>(
            json!({"input_token_budget":{"encoding":"unknown","max_tokens":100}})
        )
        .is_err()
    );
    assert!(
        serde_json::from_value::<LoopPolicy>(
            json!({"input_token_budget":{"encoding":"cl100k_base","max_tokens":100,"typo":1}})
        )
        .is_err()
    );
    let mut state = engine(Default::default());
    state.start("Read".into()).unwrap();
    let mut old = serde_json::to_value(&state).unwrap();
    old["policy"]
        .as_object_mut()
        .unwrap()
        .remove("input_token_budget");
    let mut restored: Engine = serde_json::from_value(old).unwrap();
    restored.preflight().unwrap();
    assert_eq!(
        serde_json::to_value(restored).unwrap(),
        serde_json::to_value(state).unwrap()
    );
}

fn estimate(state: &Engine) -> u64 {
    request(state)
        .context
        .as_ref()
        .unwrap()
        .token_estimate
        .as_ref()
        .unwrap()
        .total_tokens
}
fn tool_reply(state: &mut Engine, input_tokens: u64) {
    state
        .accept_reply(ModelReply {
            provider_state: None,
            content: None,
            usage: Some(Usage {
                input_tokens,
                output_tokens: 1,
            }),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: json!({}),
            }],
        })
        .unwrap();
}

#[test]
fn wire_profiles_tokenize_the_shared_adapter_payload_instead_of_a_chat_template() {
    for framing in [
        InputTokenFraming::OpenaiChatJson,
        InputTokenFraming::AnthropicMessagesJson,
        InputTokenFraming::GeminiGenerateContentJson,
    ] {
        let mut p = policy(TokenEncoding::Cl100kBase, 10_000);
        p.input_token_budget.as_mut().unwrap().framing = framing;
        let mut state = engine(p);
        state.start("Read 🦀".into()).unwrap();
        for after_tool in [false, true] {
            if after_tool {
                tool_reply(&mut state, 1);
                state.accept_tool("c", json!({"path":"niño"})).unwrap();
            }
            let req = request(&state);
            let mut body = match framing {
                InputTokenFraming::OpenaiChatJson => request_body(req),
                InputTokenFraming::AnthropicMessagesJson => {
                    hive_agent_core::provider::anthropic::request_body(req, 1).unwrap()
                }
                _ => hive_agent_core::provider::gemini::request_body(req, 1).unwrap(),
            };
            for key in [
                "max_tokens",
                "max_completion_tokens",
                "generationConfig",
                "stream",
            ] {
                body.as_object_mut().unwrap().remove(key);
            }
            let expected = tiktoken_rs::cl100k_base_singleton()
                .encode_ordinary(&body.to_string())
                .len() as u64;
            let report = req
                .context
                .as_ref()
                .unwrap()
                .token_estimate
                .as_ref()
                .unwrap();
            assert_eq!(report.framing, framing);
            assert_eq!(report.serialized_tokens, expected);
            assert_eq!(report.total_tokens, expected + 7);
        }
    }
}

#[test]
fn combined_window_keeps_the_explicit_output_cap_and_checks_the_exact_boundary() {
    let mut sample = engine(policy(TokenEncoding::Cl100kBase, 10_000));
    sample.start("Read".into()).unwrap();
    let input = estimate(&sample);
    for (window, fits) in [(input + 20, true), (input + 19, false)] {
        let mut p = policy(TokenEncoding::Cl100kBase, 10_000);
        p.max_output_tokens = Some(20);
        p.max_total_output_tokens = Some(40);
        p.input_token_budget.as_mut().unwrap().context_window_tokens = Some(window);
        let mut state = engine(p);
        state.start("Read".into()).unwrap();
        if fits {
            assert_eq!(request(&state).max_output_tokens, Some(20));
        } else {
            assert!(
                matches!(state.action(), Action::Stopped {reason: StopReason::ContextLimit, stats} if stats.model_calls==0 && stats.reserved_input_tokens==0 && stats.reserved_output_tokens==0)
            );
        }
    }
    let mut p = policy(TokenEncoding::Cl100kBase, 10_000);
    p.input_token_budget.as_mut().unwrap().context_window_tokens = Some(1000);
    assert!(Engine::with_policy("test".into(), None, vec![], 8, p).is_err());
}

#[test]
fn accumulated_input_is_reserved_once_reconciled_and_preserved_across_serialization() {
    let mut sample = engine(policy(TokenEncoding::Cl100kBase, 10_000));
    sample.start("Read".into()).unwrap();
    let first = estimate(&sample);
    tool_reply(&mut sample, 1);
    sample.accept_tool("c", json!(5)).unwrap();
    let second = estimate(&sample);
    for (total, fits) in [(first + second, true), (first + second - 1, false)] {
        let mut p = policy(TokenEncoding::Cl100kBase, 10_000);
        p.input_token_budget.as_mut().unwrap().max_total_tokens = Some(total);
        let mut state = engine(p);
        state.start("Read".into()).unwrap();
        state.preflight().unwrap();
        state.preflight().unwrap();
        assert_eq!(
            serde_json::to_value(&state).unwrap()["stats"]["estimated_input_tokens"],
            0
        );
        tool_reply(&mut state, 1);
        state = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
        state.accept_tool("c", json!(5)).unwrap();
        assert_eq!(state.messages().last().unwrap().role, "tool");
        if fits {
            let before = serde_json::to_value(&state).unwrap();
            state.preflight().unwrap();
            assert_eq!(before, serde_json::to_value(&state).unwrap());
            assert_eq!(before["stats"]["charged_input_tokens"], first);
            assert_eq!(before["stats"]["reserved_input_tokens"], second);
            state
                .accept_reply(ModelReply {
                    provider_state: None,
                    content: Some("done".into()),
                    tool_calls: vec![],
                    usage: Some(Usage {
                        input_tokens: 1,
                        output_tokens: 1,
                    }),
                })
                .unwrap();
            assert!(
                matches!(state.action(), Action::Complete {stats,..} if stats.estimated_input_tokens==total && stats.input_tokens==2 && stats.charged_input_tokens==total && stats.reserved_input_tokens==0)
            );
        } else {
            assert!(
                matches!(state.action(), Action::Stopped {reason: StopReason::InputLimit,stats} if stats.model_calls==1 && stats.tool_calls==1 && stats.charged_input_tokens==first && stats.reserved_input_tokens==0)
            );
        }
    }
}

#[test]
fn actual_input_overrun_or_unavailable_usage_prevents_followup_effects() {
    for usage in [
        None,
        Some(Usage {
            input_tokens: 1001,
            output_tokens: 1,
        }),
    ] {
        let expected = if usage.is_none() {
            StopReason::UsageUnavailable
        } else {
            StopReason::InputLimit
        };
        let mut p = policy(TokenEncoding::Cl100kBase, 10_000);
        p.input_token_budget.as_mut().unwrap().max_total_tokens = Some(1000);
        let mut state = engine(p);
        state.start("Read".into()).unwrap();
        state
            .accept_reply(ModelReply {
                provider_state: None,
                content: None,
                tool_calls: vec![ToolCall {
                    id: "c".into(),
                    name: "read".into(),
                    arguments: json!({}),
                }],
                usage,
            })
            .unwrap();
        assert!(
            matches!(state.action(), Action::Stopped {reason,stats} if reason==&expected && stats.model_calls==1 && stats.tool_calls==0 && stats.reserved_input_tokens==0)
        );
    }
}

#[test]
fn window_selection_drops_only_old_turns_and_keeps_the_active_tool_pair() {
    let mut p = policy(TokenEncoding::Cl100kBase, 10_000);
    p.max_output_tokens = Some(20);
    p.max_context_bytes = Some(1100);
    p.context_overflow = ContextOverflow::DropOldestTurns;
    let mut sample = engine(p.clone());
    sample.start("current ".repeat(60)).unwrap();
    let window = estimate(&sample) + 20;
    p.input_token_budget.as_mut().unwrap().context_window_tokens = Some(window);
    let mut state = engine(p);
    state.start("old ".repeat(60)).unwrap();
    finish(&mut state);
    state.start("current ".repeat(60)).unwrap();
    let req = request(&state);
    assert_eq!(req.context.as_ref().unwrap().dropped_messages, 2);
    assert_eq!(req.messages.len(), 2);
    assert_eq!(state.messages().len(), 4);
    assert_eq!(estimate(&state) + 20, window);
    tool_reply(&mut state, 1);
    state
        .accept_tool("c", json!("result ".repeat(500)))
        .unwrap();
    assert!(matches!(
        state.action(),
        Action::Stopped {
            reason: StopReason::ContextLimit,
            ..
        }
    ));
    assert_eq!(state.messages().last().unwrap().role, "tool");
    assert_eq!(
        state.messages()[state.messages().len() - 2].tool_calls[0].id,
        "c"
    );
}

#[test]
fn unknown_durable_model_keeps_the_input_reservation_until_its_known_reply() {
    use hive_agent_core::sessions::OperationResult;
    let mut p = policy(TokenEncoding::Cl100kBase, 10_000);
    p.input_token_budget.as_mut().unwrap().max_total_tokens = Some(1000);
    let directory = tempfile::tempdir().unwrap();
    let store = SessionStore::open(directory.path(), "t".into()).unwrap();
    let start = store
        .begin_run("a", "s", 0, engine(p.clone()), "Read".into())
        .unwrap();
    let reserved = estimate(&start.state);
    let claim = store.claim("a", "s", start.revision).unwrap();
    let execution_id = claim.operation.unwrap().execution_id;
    drop(store);
    let store = SessionStore::open(directory.path(), "t".into()).unwrap();
    let view = store.inspect("a", "s", None).unwrap();
    assert_eq!(
        view["run"]["state"]["stats"]["reserved_input_tokens"],
        reserved
    );
    assert_eq!(view["run"]["state"]["stats"]["charged_input_tokens"], 0);
    assert!(matches!(
        store.load_run("a", "s", &engine(p)),
        Err(hive_agent_core::Error::SessionOperationUncertain)
    ));
    let done = store
        .advance(
            "a",
            "s",
            claim.revision,
            &execution_id,
            OperationResult::Model {
                reply: ModelReply {
                    provider_state: None,
                    content: Some("known".into()),
                    tool_calls: vec![],
                    usage: Some(Usage {
                        input_tokens: 200,
                        output_tokens: 1,
                    }),
                },
            },
        )
        .unwrap();
    assert!(
        matches!(done.action, Action::Complete {stats,..} if stats.reserved_input_tokens==0 && stats.charged_input_tokens==200 && stats.estimated_input_tokens==reserved)
    );
}
