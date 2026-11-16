use hive_agent_core::provider::{anthropic, gemini, request_body};
use hive_agent_core::sessions::SessionStore;
use hive_agent_core::{
    Action, Attachment, AttachmentKind, Engine, Error, InputTokenBudget, InputTokenFraming,
    LoopPolicy, Message, ModelRequest, Prompt, PromptParts, StopReason, TokenEncoding,
};
use serde_json::{Value, json};

const PNG: &str = "iVBORw0KGgo=";

fn attachment(kind: AttachmentKind, media_type: &str) -> Attachment {
    Attachment {
        kind,
        media_type: media_type.into(),
        data: Some(PNG.into()),
        url: None,
        name: None,
    }
}
fn by_url(kind: AttachmentKind, media_type: &str, url: &str) -> Attachment {
    Attachment {
        data: None,
        url: Some(url.into()),
        ..attachment(kind, media_type)
    }
}
fn prompt(text: &str, attachments: Vec<Attachment>) -> Prompt {
    Prompt::Parts(PromptParts {
        text: text.into(),
        attachments,
    })
}
fn budget(attachment_tokens: Option<u64>, framing: InputTokenFraming) -> LoopPolicy {
    LoopPolicy {
        input_token_budget: Some(InputTokenBudget {
            encoding: TokenEncoding::O200kBase,
            max_tokens: 1_000_000,
            overhead_tokens: 0,
            framing,
            max_total_tokens: None,
            context_window_tokens: None,
            attachment_tokens,
        }),
        ..Default::default()
    }
}
fn engine(policy: LoopPolicy) -> Engine {
    Engine::with_policy("m".into(), Some("S".into()), vec![], 4, policy).unwrap()
}
fn request(state: &Engine) -> &ModelRequest {
    let Action::Model { request } = state.action() else {
        panic!("expected model")
    };
    request
}
fn started(attachments: Vec<Attachment>) -> Engine {
    let mut state = engine(LoopPolicy::default());
    state.start_prompt(prompt("Look", attachments)).unwrap();
    state
}

#[test]
fn text_only_messages_keep_their_exact_wire_form() {
    let state = started(vec![]);
    let message = serde_json::to_value(&request(&state).messages[1]).unwrap();
    assert_eq!(message, json!({"role":"user","content":"Look"}));
    assert!(request_body(request(&state)).unwrap()["messages"][1]["content"].is_string());
    let old: Message = serde_json::from_value(json!({"role":"user","content":"x"})).unwrap();
    assert!(old.attachments.is_empty());
}

#[test]
fn prompts_accept_plain_strings_or_structured_parts() {
    let plain: Prompt = serde_json::from_value(json!("hello")).unwrap();
    assert_eq!(plain, Prompt::Text("hello".into()));
    let rich: Prompt = serde_json::from_value(json!({
        "text":"look","attachments":[{"kind":"image","media_type":"image/png","data":PNG}]
    }))
    .unwrap();
    assert!(matches!(&rich, Prompt::Parts(parts) if parts.attachments.len() == 1));
    assert!(serde_json::from_value::<Prompt>(json!({"text":"x","bogus":1})).is_err());
    assert!(
        serde_json::from_value::<Prompt>(
            json!({"text":"x","attachments":[{"kind":"video","media_type":"video/mp4","data":PNG}]})
        )
        .is_err()
    );
}

#[test]
fn attachments_are_validated_before_the_turn_starts() {
    let bad = |a: Attachment| {
        let mut state = engine(LoopPolicy::default());
        let error = state.start_prompt(prompt("x", vec![a])).unwrap_err();
        assert!(matches!(error, Error::Input(_)), "{error:?}");
        assert!(
            state.messages().len() <= 1,
            "failed starts must not append the message"
        );
    };
    bad(Attachment {
        data: Some("not base64!".into()),
        ..attachment(AttachmentKind::Image, "image/png")
    });
    bad(Attachment {
        data: Some("abc".into()),
        ..attachment(AttachmentKind::Image, "image/png")
    });
    bad(Attachment {
        data: Some("====".into()),
        ..attachment(AttachmentKind::Image, "image/png")
    });
    bad(Attachment {
        url: Some("https://x/y.png".into()),
        ..attachment(AttachmentKind::Image, "image/png")
    });
    bad(Attachment {
        data: None,
        ..attachment(AttachmentKind::Image, "image/png")
    });
    bad(attachment(AttachmentKind::Image, "audio/wav"));
    bad(attachment(AttachmentKind::Audio, "image/png"));
    bad(attachment(AttachmentKind::Document, "image/png"));
    bad(attachment(AttachmentKind::Image, "Image/PNG"));
    bad(attachment(AttachmentKind::Image, "png"));
    bad(by_url(AttachmentKind::Image, "image/png", "ftp://x/y.png"));
    bad(by_url(
        AttachmentKind::Image,
        "image/png",
        "https://user:pw@x/y.png",
    ));
    bad(by_url(AttachmentKind::Image, "image/png", "not a url"));
    bad(Attachment {
        name: Some("a\nb".into()),
        ..attachment(AttachmentKind::Document, "application/pdf")
    });
    bad(Attachment {
        data: Some("A".repeat(8 * 1024 * 1024 + 4)),
        ..attachment(AttachmentKind::Image, "image/png")
    });

    let mut state = engine(LoopPolicy::default());
    let nine = vec![attachment(AttachmentKind::Image, "image/png"); 9];
    assert!(state.start_prompt(prompt("x", nine)).is_err());
    let big = Attachment {
        data: Some("A".repeat(6 * 1024 * 1024)),
        ..attachment(AttachmentKind::Image, "image/png")
    };
    assert!(
        state
            .start_prompt(prompt("x", vec![big.clone(), big.clone(), big]))
            .is_err(),
        "per-message total"
    );
    assert!(
        state
            .start_prompt(prompt(
                "  ",
                vec![attachment(AttachmentKind::Image, "image/png")]
            ))
            .is_err(),
        "text stays required"
    );
    assert!(
        state
            .start_prompt(prompt(
                "ok",
                vec![attachment(AttachmentKind::Document, "text/plain")]
            ))
            .is_ok()
    );
}

#[test]
fn openai_maps_images_audio_and_documents_and_rejects_the_rest() {
    let mut named = attachment(AttachmentKind::Document, "application/pdf");
    named.name = Some("spec.pdf".into());
    let state = started(vec![
        attachment(AttachmentKind::Image, "image/png"),
        by_url(AttachmentKind::Image, "image/jpeg", "https://x/y.jpg"),
        attachment(AttachmentKind::Audio, "audio/wav"),
        attachment(AttachmentKind::Audio, "audio/mpeg"),
        named,
        attachment(AttachmentKind::Document, "application/pdf"),
    ]);
    let parts = request_body(request(&state)).unwrap()["messages"][1]["content"].clone();
    assert_eq!(parts[0], json!({"type":"text","text":"Look"}));
    assert_eq!(
        parts[1],
        json!({"type":"image_url","image_url":{"url":format!("data:image/png;base64,{PNG}")}})
    );
    assert_eq!(parts[2]["image_url"]["url"], "https://x/y.jpg");
    assert_eq!(
        parts[3],
        json!({"type":"input_audio","input_audio":{"data":PNG,"format":"wav"}})
    );
    assert_eq!(parts[4]["input_audio"]["format"], "mp3");
    assert_eq!(
        parts[5],
        json!({"type":"file","file":{"filename":"spec.pdf","file_data":format!("data:application/pdf;base64,{PNG}")}})
    );
    assert_eq!(parts[6]["file"]["filename"], "document");

    for rejected in [
        by_url(AttachmentKind::Audio, "audio/wav", "https://x/a.wav"),
        attachment(AttachmentKind::Audio, "audio/ogg"),
        by_url(
            AttachmentKind::Document,
            "application/pdf",
            "https://x/a.pdf",
        ),
    ] {
        let state = started(vec![rejected]);
        assert!(matches!(
            request_body(request(&state)),
            Err(Error::Input(_))
        ));
    }
}

#[test]
fn anthropic_maps_images_and_pdfs_and_rejects_audio() {
    let mut pdf = attachment(AttachmentKind::Document, "application/pdf");
    pdf.name = Some("spec.pdf".into());
    let state = started(vec![
        attachment(AttachmentKind::Image, "image/png"),
        by_url(AttachmentKind::Image, "image/webp", "https://x/y.webp"),
        pdf,
        by_url(
            AttachmentKind::Document,
            "application/pdf",
            "https://x/a.pdf",
        ),
    ]);
    let blocks =
        anthropic::request_body(request(&state), 100).unwrap()["messages"][0]["content"].clone();
    assert_eq!(
        blocks[0],
        json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}})
    );
    assert_eq!(
        blocks[1],
        json!({"type":"image","source":{"type":"url","url":"https://x/y.webp"}})
    );
    assert_eq!(blocks[2]["type"], "document");
    assert_eq!(blocks[2]["title"], "spec.pdf");
    assert_eq!(blocks[3]["source"]["type"], "url");
    assert_eq!(
        blocks[4],
        json!({"type":"text","text":"Look"}),
        "media leads the text"
    );

    for rejected in [
        attachment(AttachmentKind::Audio, "audio/wav"),
        attachment(AttachmentKind::Image, "image/bmp"),
        attachment(AttachmentKind::Document, "text/plain"),
    ] {
        let state = started(vec![rejected]);
        assert!(anthropic::request_body(request(&state), 100).is_err());
    }
}

#[test]
fn gemini_maps_inline_data_only() {
    let state = started(vec![
        attachment(AttachmentKind::Image, "image/png"),
        attachment(AttachmentKind::Audio, "audio/wav"),
        attachment(AttachmentKind::Document, "application/pdf"),
    ]);
    let parts = gemini::request_body(request(&state), 100).unwrap()["contents"][0]["parts"].clone();
    assert_eq!(
        parts[0],
        json!({"inlineData":{"mimeType":"image/png","data":PNG}})
    );
    assert_eq!(parts[1]["inlineData"]["mimeType"], "audio/wav");
    assert_eq!(parts[2]["inlineData"]["mimeType"], "application/pdf");
    assert_eq!(parts[3], json!({"text":"Look"}));
    let state = started(vec![by_url(
        AttachmentKind::Image,
        "image/png",
        "https://x/y.png",
    )]);
    assert!(gemini::request_body(request(&state), 100).is_err());
}

#[test]
fn attachments_only_belong_to_user_messages() {
    let state = started(vec![attachment(AttachmentKind::Image, "image/png")]);
    let mut req = request(&state).clone();
    req.messages[0].attachments = vec![attachment(AttachmentKind::Image, "image/png")];
    assert!(request_body(&req).is_err());
    assert!(anthropic::request_body(&req, 9).is_err());
    assert!(gemini::request_body(&req, 9).is_err());
}

#[test]
fn input_token_budgets_use_a_fixed_allowance_not_the_attachment_bytes() {
    for framing in [
        InputTokenFraming::NeutralJson,
        InputTokenFraming::OpenaiChatJson,
        InputTokenFraming::AnthropicMessagesJson,
        InputTokenFraming::GeminiGenerateContentJson,
    ] {
        let mut missing = engine(budget(None, framing));
        let error = missing
            .start_prompt(prompt(
                "x",
                vec![attachment(AttachmentKind::Image, "image/png")],
            ))
            .unwrap_err();
        assert!(
            error.to_string().contains("attachment_tokens"),
            "{framing:?}: {error}"
        );

        let estimate = |data: String, count: usize| {
            let mut state = engine(budget(Some(1500), framing));
            let image = Attachment {
                data: Some(data),
                ..attachment(AttachmentKind::Image, "image/png")
            };
            state
                .start_prompt(prompt("Look", vec![image; count]))
                .unwrap();
            request(&state)
                .context
                .as_ref()
                .unwrap()
                .token_estimate
                .clone()
                .unwrap()
        };
        let small = estimate("A".repeat(8), 1);
        let large = estimate("A".repeat(4 * 1024 * 1024), 1);
        assert_eq!(
            small, large,
            "{framing:?}: payload size must not change the estimate"
        );
        assert_eq!(small.overhead_tokens, 1500);
        assert_eq!(small.total_tokens, small.serialized_tokens + 1500);
        let two = estimate("A".repeat(8), 2);
        assert_eq!(two.overhead_tokens, 3000, "{framing:?}");

        let mut plain = engine(budget(None, framing));
        plain.start("Look".into()).unwrap();
        assert_eq!(
            request(&plain)
                .context
                .as_ref()
                .unwrap()
                .token_estimate
                .as_ref()
                .unwrap()
                .overhead_tokens,
            0
        );
    }
}

#[test]
fn a_tight_budget_stops_when_the_allowance_does_not_fit() {
    let mut policy = budget(Some(5000), InputTokenFraming::NeutralJson);
    policy.input_token_budget.as_mut().unwrap().max_tokens = 4000;
    let mut state = engine(policy);
    state
        .start_prompt(prompt(
            "Look",
            vec![attachment(AttachmentKind::Image, "image/png")],
        ))
        .unwrap();
    assert!(
        matches!(
            state.action(),
            Action::Stopped {
                reason: StopReason::ContextLimit,
                ..
            }
        ),
        "request must not be sent over budget: {:?}",
        state.action()
    );
}

#[test]
fn attachments_persist_in_durable_sessions_and_reload() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let initial = engine(LoopPolicy::default());
    let frame = store
        .begin_run_with_input(
            "a",
            "s",
            0,
            initial.clone(),
            prompt(
                "Look",
                vec![
                    attachment(AttachmentKind::Image, "image/png"),
                    by_url(AttachmentKind::Image, "image/png", "https://x/y.png"),
                ],
            ),
            0,
            None,
            None,
        )
        .unwrap();
    let message = &frame.state.messages()[1];
    assert_eq!(message.attachments.len(), 2);
    drop(store);

    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let reloaded = store.load_run("a", "s", &initial).unwrap();
    assert_eq!(
        reloaded.state.messages()[1].attachments,
        message.attachments
    );
    let body = request_body(match &reloaded.action {
        Action::Model { request } => request,
        other => panic!("expected model, got {other:?}"),
    })
    .unwrap();
    assert_eq!(body["messages"][1]["content"][1]["type"], "image_url");

    let json_command = json!({"op":"begin_run","agent_id":"a","session_id":"t","revision":0,
        "initial":serde_json::to_value(&initial).unwrap(),
        "prompt":{"text":"Look","attachments":[{"kind":"image","media_type":"image/png","data":PNG}]}});
    let frame: Value =
        serde_json::from_str(&store.dispatch(&json_command.to_string()).unwrap()).unwrap();
    assert_eq!(
        frame["state"]["messages"][1]["attachments"][0]["media_type"],
        "image/png"
    );
    let plain = json!({"op":"begin_run","agent_id":"a","session_id":"u","revision":0,
        "initial":serde_json::to_value(&initial).unwrap(),"prompt":"just text"});
    let frame: Value = serde_json::from_str(&store.dispatch(&plain.to_string()).unwrap()).unwrap();
    assert!(frame["state"]["messages"][1].get("attachments").is_none());
}
