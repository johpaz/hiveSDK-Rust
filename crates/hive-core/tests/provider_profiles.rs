use hive_agent_core::provider::{ModelEvent, NativeProvider, presets};
use hive_agent_core::{Error, ModelRequest, Tool};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
};

struct Fake {
    base: String,
    requests: mpsc::UnboundedReceiver<String>,
}
async fn fake(response: &'static str) -> Fake {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let (sender, requests) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let sender = sender.clone();
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let mut buffer = vec![0u8; 16384];
                loop {
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    raw.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&raw);
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if body.len() >= length || read == 0 {
                            break;
                        }
                    } else if read == 0 {
                        break;
                    }
                }
                let _ = sender.send(String::from_utf8_lossy(&raw).into_owned());
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    Fake { base, requests }
}

const CHAT: &str = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n";
fn reply(body: Value) -> &'static str {
    Box::leak(format!("{CHAT}{body}").into_boxed_str())
}
fn tool_reply(name: &str) -> &'static str {
    reply(
        json!({"choices":[{"finish_reason":"tool_calls","message":{"content":null,"tool_calls":[
        {"id":"c1","type":"function","function":{"name":name,"arguments":"{}"}}]}}],
        "usage":{"prompt_tokens":3,"completion_tokens":1}}),
    )
}
fn tool(name: &str, parameters: Value) -> Tool {
    Tool {
        name: name.into(),
        description: "d".into(),
        parameters,
        allow_repeated_results: false,
    }
}
fn request(model: &str, tools: Vec<Tool>) -> ModelRequest {
    let messages = vec![hive_agent_core::Message {
        role: "user".into(),
        content: Some("hi".into()),
        attachments: vec![],
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
    }];
    ModelRequest {
        model: model.into(),
        messages,
        tools,
        max_output_tokens: None,
        deadline_unix_ms: None,
        context: None,
        structured_output: None,
    }
}
fn provider(preset: &str, base: &str) -> NativeProvider {
    NativeProvider::from_json(&json!({"preset":preset,"base_url":base}).to_string()).unwrap()
}
async fn complete(
    provider: &NativeProvider,
    request: ModelRequest,
) -> Result<hive_agent_core::ModelReply, Error> {
    let stream = provider.start(request, false);
    loop {
        if let ModelEvent::Reply { reply } = stream.next().await?.expect("reply before end") {
            return Ok(reply);
        }
    }
}
fn body_of(raw: &str) -> Value {
    serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap()
}
fn header_of(raw: &str, name: &str) -> Option<String> {
    raw.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

#[test]
fn every_reference_provider_has_a_preset_that_builds() {
    let ids: Vec<_> = presets().iter().map(|preset| preset.id).collect();
    assert_eq!(
        ids,
        [
            "openai",
            "anthropic",
            "gemini",
            "groq",
            "mistral",
            "openrouter",
            "deepseek",
            "kimi",
            "nvidia",
            "qwen",
            "minimax",
            "opencode-go",
            "hiveagents",
            "z-ai",
            "modelscope",
            "ollama"
        ]
    );
    for preset in presets() {
        NativeProvider::from_json(&json!({"preset":preset.id,"api_key":"k"}).to_string())
            .unwrap_or_else(|error| panic!("{}: {error}", preset.id));
        assert!(preset.base_url.starts_with("http"));
    }
    let by_id = |id: &str| {
        presets()
            .into_iter()
            .find(|preset| preset.id == id)
            .unwrap()
    };
    assert_eq!(
        (
            by_id("anthropic").kind,
            by_id("gemini").kind,
            by_id("deepseek").kind
        ),
        ("anthropic", "gemini", "openai_compatible")
    );
    assert_eq!(
        by_id("modelscope").base_url,
        "https://api-inference.modelscope.ai/v1"
    );
}

#[test]
fn preset_configuration_is_validated_and_overridable() {
    let build = |config: Value| NativeProvider::from_json(&config.to_string());
    assert!(
        matches!(build(json!({"preset":"nope"})), Err(Error::Input(m)) if m.contains("unknown provider preset"))
    );
    assert!(
        matches!(build(json!({"preset":"anthropic","kind":"gemini"})), Err(Error::Input(m)) if m.contains("not `gemini`"))
    );
    assert!(build(json!({"preset":"deepseek","kind":"openai_compatible"})).is_ok());
    assert!(build(json!({"preset":7})).is_err());
    assert!(
        build(json!({"preset":"openai","base_url":"ftp://x"})).is_err(),
        "overrides are still validated"
    );
    assert!(build(json!({"preset":"openai","profile":{"bogus":true}})).is_err());
    assert!(build(json!({"preset":"openai","profile":{"session_header":"bad header"}})).is_err());
    assert!(
        build(json!({"kind":"anthropic","base_url":"https://x","profile":{}})).is_err(),
        "profile is OpenAI-only"
    );
    assert!(
        build(json!({"base_url":"https://x","profile":{"normalize_tool_names":true}})).is_ok(),
        "profiles also work without a preset"
    );
}

#[tokio::test]
async fn tool_names_are_normalized_on_the_wire_and_restored_on_the_reply() {
    let mut server = fake(tool_reply("my__tool__name")).await;
    let provider = provider("openai", &server.base);
    let mut req = request(
        "m",
        vec![
            tool("my.tool name", json!({"type":"object"})),
            tool("plain_ok", json!({"type":"object"})),
        ],
    );
    req.messages.push(hive_agent_core::Message {
        role: "assistant".into(),
        content: None,
        attachments: vec![],
        tool_calls: vec![hive_agent_core::ToolCall {
            id: "h".into(),
            name: "my.tool name".into(),
            arguments: json!({}),
        }],
        tool_call_id: None,
        provider_state: None,
    });
    req.messages.push(hive_agent_core::Message {
        role: "tool".into(),
        content: Some("{}".into()),
        attachments: vec![],
        tool_calls: vec![],
        tool_call_id: Some("h".into()),
        provider_state: None,
    });
    let reply = complete(&provider, req).await.unwrap();
    assert_eq!(
        reply.tool_calls[0].name, "my.tool name",
        "reply maps back to the original name"
    );
    let body = body_of(&server.requests.recv().await.unwrap());
    let names: Vec<_> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["my__tool__name", "plain_ok"]);
    assert_eq!(
        body["messages"][1]["tool_calls"][0]["function"]["name"], "my__tool__name",
        "history is rewritten too"
    );
}

#[tokio::test]
async fn colliding_names_fail_before_any_request() {
    let mut server = fake(tool_reply("x")).await;
    let provider = provider("openai", &server.base);
    let req = request(
        "m",
        vec![
            tool("a.b", json!({"type":"object"})),
            tool("a__b", json!({"type":"object"})),
        ],
    );
    let error = complete(&provider, req).await.unwrap_err();
    assert!(
        matches!(&error, Error::Input(m) if m.contains("collide")),
        "{error:?}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), server.requests.recv())
            .await
            .is_err(),
        "nothing was sent"
    );
}

#[tokio::test]
async fn normalization_follows_the_reference_rules() {
    let mut server = fake(tool_reply("x")).await;
    let provider = provider("openai", &server.base);
    let long = "n".repeat(70);
    let names = [
        "9lives",
        "9 lives",
        "a.b.c",
        "a..b",
        "über",
        "é",
        "ok-name_1",
    ];
    let tools = names
        .iter()
        .map(|n| tool(n, json!({"type":"object"})))
        .chain([tool(&long, json!({"type":"object"}))])
        .collect();
    let _ = complete(&provider, request("m", tools)).await;
    let body = body_of(&server.requests.recv().await.unwrap());
    let wire: Vec<_> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        &wire[..7],
        [
            "9lives",
            "_9__lives",
            "a__b__c",
            "a__b",
            "__ber",
            "__",
            "ok-name_1"
        ]
    );
    assert_eq!(wire[7], "n".repeat(64));
}

#[tokio::test]
async fn schemas_are_adjusted_only_as_the_profile_asks() {
    let schema = json!({"type":"object","additionalProperties":false,"properties":{
        "list":{"type":"array"},
        "nested":{"type":"object","additionalProperties":false,"properties":{"inner":{"type":"array","items":{"type":"string"}}}},
        "additionalProperties":{"type":"string"}}});
    let mut server = fake(tool_reply("t")).await;
    let _ = complete(
        &provider("mistral", &server.base),
        request("m", vec![tool("t", schema.clone())]),
    )
    .await;
    let mistral = body_of(&server.requests.recv().await.unwrap());
    let parameters = &mistral["tools"][0]["function"]["parameters"];
    assert!(parameters.get("additionalProperties").is_none());
    assert!(
        parameters["properties"]["nested"]
            .get("additionalProperties")
            .is_none()
    );
    assert_eq!(parameters["properties"]["list"]["items"], json!({}));
    assert_eq!(
        parameters["properties"]["nested"]["properties"]["inner"]["items"],
        json!({"type":"string"})
    );
    assert_eq!(
        parameters["properties"]["additionalProperties"],
        json!({"type":"string"}),
        "a property named like the keyword is kept"
    );

    let _ = complete(
        &provider("openai", &server.base),
        request("m", vec![tool("t", schema)]),
    )
    .await;
    let openai = body_of(&server.requests.recv().await.unwrap());
    assert_eq!(
        openai["tools"][0]["function"]["parameters"]["additionalProperties"], false,
        "openai keeps strictness"
    );
    assert_eq!(
        openai["tools"][0]["function"]["parameters"]["properties"]["list"]["items"],
        json!({})
    );
}

#[tokio::test]
async fn tool_choice_and_parallelism_only_apply_when_tools_are_sent() {
    let mut server = fake(reply(
        json!({"choices":[{"finish_reason":"stop","message":{"content":"ok"}}]}),
    ))
    .await;
    let kimi = provider("kimi", &server.base);
    complete(
        &kimi,
        request("kimi-k2", vec![tool("t", json!({"type":"object"}))]),
    )
    .await
    .unwrap();
    let with_tools = body_of(&server.requests.recv().await.unwrap());
    assert_eq!(
        (
            &with_tools["parallel_tool_calls"],
            &with_tools["tool_choice"]
        ),
        (&json!(false), &json!("auto"))
    );
    complete(&kimi, request("kimi-k2", vec![])).await.unwrap();
    let plain = body_of(&server.requests.recv().await.unwrap());
    assert!(plain.get("parallel_tool_calls").is_none() && plain.get("tool_choice").is_none());
    complete(
        &provider("groq", &server.base),
        request("m", vec![tool("t", json!({"type":"object"}))]),
    )
    .await
    .unwrap();
    assert!(
        body_of(&server.requests.recv().await.unwrap())
            .get("parallel_tool_calls")
            .is_none()
    );
}

#[tokio::test]
async fn models_without_tool_support_are_rejected_not_silently_stripped() {
    let mut server = fake(reply(
        json!({"choices":[{"finish_reason":"stop","message":{"content":"ok"}}]}),
    ))
    .await;
    let with = |model: &str| request(model, vec![tool("t", json!({"type":"object"}))]);
    for (preset, model) in [
        ("deepseek", "deepseek-reasoner"),
        ("deepseek", "DeepSeek-R1"),
        ("openrouter", "deepseek/deepseek-r1:free"),
        ("openrouter", "vendor/deepseek-r1-distill"),
        ("deepseek", "x/r1"),
    ] {
        let error = complete(&provider(preset, &server.base), with(model))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, Error::Input(m) if m.contains("does not support tools")),
            "{preset}/{model}: {error:?}"
        );
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(50), server.requests.recv())
            .await
            .is_err()
    );
    for (preset, model) in [
        ("deepseek", "deepseek-chat"),
        ("deepseek", "model-r10"),
        ("openai", "deepseek-reasoner"),
    ] {
        complete(&provider(preset, &server.base), with(model))
            .await
            .unwrap_or_else(|e| panic!("{preset}/{model}: {e:?}"));
    }
    complete(
        &provider("deepseek", &server.base),
        request("deepseek-reasoner", vec![]),
    )
    .await
    .expect("no tools, no restriction");
}

#[tokio::test]
async fn opencode_go_sends_a_stable_session_header_and_user_agent() {
    let mut server = fake(reply(
        json!({"choices":[{"finish_reason":"stop","message":{"content":"ok"}}]}),
    ))
    .await;
    let go = provider("opencode-go", &server.base);
    complete(&go, request("m", vec![])).await.unwrap();
    complete(&go, request("m", vec![])).await.unwrap();
    let (first, second) = (
        server.requests.recv().await.unwrap(),
        server.requests.recv().await.unwrap(),
    );
    let session = header_of(&first, "x-opencode-session").expect("session header");
    assert_eq!(session.len(), 32);
    assert!(session.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(
        header_of(&second, "x-opencode-session"),
        Some(session.clone()),
        "stable per provider"
    );
    assert!(
        header_of(&first, "user-agent")
            .unwrap()
            .starts_with("hive-sdk-rust/")
    );
    complete(&provider("opencode-go", &server.base), request("m", vec![]))
        .await
        .unwrap();
    assert_ne!(
        header_of(&server.requests.recv().await.unwrap(), "x-opencode-session"),
        Some(session),
        "distinct per provider instance"
    );

    complete(&provider("openai", &server.base), request("m", vec![]))
        .await
        .unwrap();
    assert!(header_of(&server.requests.recv().await.unwrap(), "x-opencode-session").is_none());
}

#[tokio::test]
async fn streaming_maps_the_final_reply_but_not_the_name_fragments() {
    let sse = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n"
        .to_owned()
        + "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"my__to\",\"arguments\":\"{}\"}}]}}]}\n\n"
        + "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"ol\"}}]}}]}\n\n"
        + "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n"
        + "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1}}\n\n"
        + "data: [DONE]\n\n";
    let server = fake(Box::leak(sse.into_boxed_str())).await;
    let stream = provider("openai", &server.base).start(
        request("m", vec![tool("my.tool", json!({"type":"object"}))]),
        true,
    );
    let (mut fragments, mut reply) = (vec![], None);
    while let Some(event) = stream.next().await.unwrap() {
        match event {
            ModelEvent::ToolCallDelta {
                name: Some(name), ..
            } => fragments.push(name),
            ModelEvent::Reply { reply: r } => reply = Some(r),
            _ => {}
        }
    }
    assert_eq!(fragments, ["my__to", "ol"], "fragments keep the wire name");
    assert_eq!(reply.unwrap().tool_calls[0].name, "my.tool");
}

// ---- reasoning, thinking switches and orphaned tool calls ------------------------------------

fn message(role: &str, content: Option<&str>) -> hive_agent_core::Message {
    hive_agent_core::Message {
        role: role.into(),
        content: content.map(str::to_owned),
        attachments: vec![],
        tool_calls: vec![],
        tool_call_id: None,
        provider_state: None,
    }
}
fn call(id: &str) -> hive_agent_core::ToolCall {
    hive_agent_core::ToolCall {
        id: id.into(),
        name: "lookup".into(),
        arguments: json!({}),
    }
}
fn answer(id: &str) -> hive_agent_core::Message {
    let mut result = message("tool", Some("{\"ok\":true}"));
    result.tool_call_id = Some(id.into());
    result
}
fn asks(id: &str, reasoning: Option<&str>) -> hive_agent_core::Message {
    let mut assistant = message("assistant", None);
    assistant.tool_calls = vec![call(id)];
    assistant.provider_state = reasoning.map(|content| hive_agent_core::ProviderState::Reasoning {
        content: content.into(),
    });
    assistant
}
const TEXT: &str = "{\"choices\":[{\"finish_reason\":\"stop\",\"message\":{\"content\":\"listo\"}}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1}}";

#[tokio::test]
async fn reasoning_content_is_kept_with_its_reply_and_sent_back_only_to_providers_that_need_it() {
    // The reply of a reasoning model carries its thinking next to the tool call.
    let thinking = reply(
        json!({"choices":[{"finish_reason":"tool_calls","message":{"content":null,"reasoning_content":"primero busco",
        "tool_calls":[{"id":"c1","type":"function","function":{"name":"lookup","arguments":"{}"}}]}}],
        "usage":{"prompt_tokens":3,"completion_tokens":1}}),
    );
    for preset in ["deepseek", "kimi", "openai"] {
        let server = fake(thinking).await;
        let got = complete(
            &provider(preset, &server.base),
            request("m", vec![tool("lookup", json!({"type":"object"}))]),
        )
        .await
        .unwrap();
        assert_eq!(
            got.provider_state,
            Some(hive_agent_core::ProviderState::Reasoning {
                content: "primero busco".into()
            }),
            "{preset}: the reply keeps its reasoning"
        );
    }
    // The next request replays the assistant message with it, for the providers whose profile asks for it.
    for (preset, sent) in [
        ("deepseek", true),
        ("kimi", true),
        ("openai", false),
        ("nvidia", false),
    ] {
        let mut server = fake(Box::leak(format!("{CHAT}{TEXT}").into_boxed_str())).await;
        let mut next = request(
            "deepseek-chat",
            vec![tool("lookup", json!({"type":"object"}))],
        );
        next.messages.push(asks("c1", Some("primero busco")));
        next.messages.push(answer("c1"));
        complete(&provider(preset, &server.base), next)
            .await
            .unwrap();
        let body = body_of(&server.requests.recv().await.unwrap());
        let assistant = &body["messages"][1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(
            assistant.get("reasoning_content").is_some(),
            sent,
            "{preset}: {assistant}"
        );
        if sent {
            assert_eq!(assistant["reasoning_content"], "primero busco");
        }
        assert_eq!(
            assistant["tool_calls"][0]["id"], "c1",
            "the calls it came with are still there"
        );
    }
    // Reasoning is OpenAI-compatible state: it is not replayed through another protocol.
    let server = fake(Box::leak(format!("{CHAT}{TEXT}").into_boxed_str())).await;
    let mut next = request("claude", vec![]);
    next.messages.push(asks("c1", Some("x")));
    next.messages.push(answer("c1"));
    let anthropic = NativeProvider::from_json(
        &json!({"preset":"anthropic","base_url":server.base}).to_string(),
    )
    .unwrap();
    assert!(
        complete(&anthropic, next).await.is_err(),
        "Anthropic refuses another provider's state"
    );
}

#[tokio::test]
async fn streamed_reasoning_deltas_are_assembled_into_the_replys_state() {
    let sse = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n"
        .to_owned()
        + "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"pienso \"}}]}\n\n"
        + "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"un poco\"}}]}\n\n"
        + "data: {\"choices\":[{\"delta\":{\"content\":\"Hola\"}}]}\n\n"
        + "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n"
        + "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1}}\n\n"
        + "data: [DONE]\n\n";
    let server = fake(Box::leak(sse.into_boxed_str())).await;
    let stream = provider("deepseek", &server.base).start(request("deepseek-chat", vec![]), true);
    let mut got = None;
    while let Some(event) = stream.next().await.unwrap() {
        if let ModelEvent::Reply { reply } = event {
            got = Some(reply);
        }
    }
    let got = got.unwrap();
    assert_eq!(got.content.as_deref(), Some("Hola"));
    assert_eq!(
        got.provider_state,
        Some(hive_agent_core::ProviderState::Reasoning {
            content: "pienso un poco".into()
        })
    );
}

#[tokio::test]
async fn nvidia_thinking_is_switched_on_per_model_family_only_when_asked_and_never_for_unlisted_families()
 {
    let ask = |model: &'static str, profile: Value| async move {
        let mut server = fake(Box::leak(format!("{CHAT}{TEXT}").into_boxed_str())).await;
        let config = json!({"preset":"nvidia","base_url":server.base,"profile":profile});
        let provider = NativeProvider::from_json(&config.to_string()).unwrap();
        complete(&provider, request(model, vec![])).await.unwrap();
        body_of(&server.requests.recv().await.unwrap())
    };
    // Off by default: NIM's own default is left alone.
    assert!(
        ask("z-ai/glm-5.2", json!({}))
            .await
            .get("chat_template_kwargs")
            .is_none()
    );
    // On: a different key per family, chosen by the model id (case-insensitive).
    for (model, expected) in [
        (
            "z-ai/glm-5.2",
            json!({"enable_thinking":true,"clear_thinking":false}),
        ),
        ("minimaxai/MiniMax-M3", json!({"thinking_mode":"enabled"})),
        ("moonshotai/kimi-k2.6", json!({"thinking":true})),
        ("deepseek-ai/deepseek-v3.2", json!({"thinking":true})),
        ("qwen/qwen3-next", json!({"enable_thinking":true})),
        ("qwen/QwQ-32b", json!({"enable_thinking":true})),
    ] {
        assert_eq!(
            ask(model, json!({"thinking":true})).await["chat_template_kwargs"],
            expected,
            "{model}"
        );
    }
    // A family that is not listed (Nemotron already reasons) never gets a key its template may refuse.
    assert!(
        ask("nvidia/nemotron-3-super", json!({"thinking":true}))
            .await
            .get("chat_template_kwargs")
            .is_none()
    );
    // The caller's profile adds to the preset's instead of replacing it: its own rules win.
    let custom = ask(
        "acme/model-x",
        json!({"thinking":true,"thinking_kwargs":[{"models":["acme"],"kwargs":{"think":"high"}}]}),
    )
    .await;
    assert_eq!(custom["chat_template_kwargs"], json!({"think":"high"}));
}

#[tokio::test]
async fn a_history_with_orphaned_tool_halves_is_cleaned_for_the_wire_and_a_complete_one_is_untouched()
 {
    let send = |history: Vec<hive_agent_core::Message>| async move {
        let mut server = fake(Box::leak(format!("{CHAT}{TEXT}").into_boxed_str())).await;
        let mut next = request("m", vec![tool("lookup", json!({"type":"object"}))]);
        next.messages = history;
        complete(&provider("openai", &server.base), next)
            .await
            .unwrap();
        body_of(&server.requests.recv().await.unwrap())["messages"].clone()
    };
    let shape = |messages: &Value| -> Vec<String> {
        messages
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                format!(
                    "{}{}",
                    m["role"].as_str().unwrap(),
                    m["tool_calls"]
                        .as_array()
                        .map_or(String::new(), |c| format!("[{}]", c.len()))
                )
            })
            .collect()
    };
    // Complete: unchanged.
    let whole = send(vec![
        message("user", Some("hi")),
        asks("c1", None),
        answer("c1"),
        message("assistant", Some("ok")),
        message("user", Some("otra")),
    ])
    .await;
    assert_eq!(
        shape(&whole),
        ["user", "assistant[1]", "tool", "assistant", "user"]
    );
    // A window that starts after the call: the answer is orphaned and goes.
    let cut = send(vec![answer("c0"), message("user", Some("sigo"))]).await;
    assert_eq!(shape(&cut), ["user"]);
    // A call that was never answered goes with its (empty) message; with text, the text stays.
    let dangling = send(vec![
        message("user", Some("hi")),
        asks("c1", None),
        message("user", Some("¿y?")),
    ])
    .await;
    assert_eq!(shape(&dangling), ["user", "user"]);
    let mut speaking = asks("c9", None);
    speaking.content = Some("voy a buscar".into());
    let kept_text = send(vec![
        message("user", Some("hi")),
        speaking,
        message("user", Some("¿y?")),
    ])
    .await;
    assert_eq!(shape(&kept_text), ["user", "assistant", "user"]);
    assert_eq!(kept_text[1]["content"], "voy a buscar");
    // Two calls, one answered: only the answered one stays, so the pair is complete.
    let mut two = message("assistant", None);
    two.tool_calls = vec![call("a"), call("b")];
    let half = send(vec![message("user", Some("hi")), two, answer("b")]).await;
    assert_eq!(shape(&half), ["user", "assistant[1]", "tool"]);
    assert_eq!(half[1]["tool_calls"][0]["id"], "b");
    // An answer repeated for the same call is sent once.
    let twice = send(vec![
        message("user", Some("hi")),
        asks("c1", None),
        answer("c1"),
        answer("c1"),
    ])
    .await;
    assert_eq!(shape(&twice), ["user", "assistant[1]", "tool"]);
    // An answer that comes before its call is an orphan, not a match.
    let early = send(vec![
        answer("c1"),
        message("user", Some("hi")),
        asks("c1", None),
        answer("c1"),
    ])
    .await;
    assert_eq!(shape(&early), ["user", "assistant[1]", "tool"]);
}

#[tokio::test]
async fn a_tool_call_written_as_text_becomes_a_real_one_only_when_the_profile_asks_and_the_tool_exists()
 {
    let content_reply = |text: &str| -> &'static str {
        reply(
            json!({"choices":[{"finish_reason":"stop","message":{"content":text}}],"usage":{"prompt_tokens":3,"completion_tokens":1}}),
        )
    };
    let run = |preset: &'static str, text: &'static str| async move {
        let server = fake(content_reply(text)).await;
        complete(
            &provider(preset, &server.base),
            request("qwen3", vec![tool("lookup", json!({"type":"object"}))]),
        )
        .await
        .unwrap()
    };
    // Ollama's profile reads them.
    let tagged = run("ollama", "Voy a buscar.\n<tool_call>\n{\"name\": \"lookup\", \"arguments\": {\"q\": \"rust\"}}\n</tool_call>").await;
    assert_eq!(tagged.tool_calls.len(), 1);
    assert_eq!(
        (
            tagged.tool_calls[0].name.as_str(),
            tagged.tool_calls[0].arguments.clone(),
            tagged.tool_calls[0].id.as_str()
        ),
        ("lookup", json!({"q":"rust"}), "text-call-0")
    );
    assert_eq!(
        tagged.content.as_deref(),
        Some("Voy a buscar."),
        "the call is taken out of the text"
    );
    // Several, with the arguments as a string; and a reply that is only the JSON, fenced.
    let two = run("ollama", "<tool_call>{\"name\":\"lookup\",\"arguments\":\"{\\\"q\\\":1}\"}</tool_call><tool_call>{\"name\":\"lookup\",\"parameters\":{\"q\":2}}</tool_call>").await;
    assert_eq!(
        two.tool_calls
            .iter()
            .map(|c| c.arguments["q"].clone())
            .collect::<Vec<_>>(),
        [json!(1), json!(2)]
    );
    assert_eq!(two.content, None);
    let fenced = run(
        "ollama",
        "```json\n{\"name\":\"lookup\",\"arguments\":{\"q\":\"x\"}}\n```",
    )
    .await;
    assert_eq!((fenced.tool_calls.len(), fenced.content), (1, None));
    let listed = run(
        "ollama",
        "[{\"name\":\"lookup\",\"arguments\":{}},{\"tool\":\"lookup\",\"args\":{\"z\":1}}]",
    )
    .await;
    assert_eq!(listed.tool_calls.len(), 2);
    // A tool that does not exist, bad JSON, arguments that are not an object, or JSON that is not a call: text.
    for text in [
        "<tool_call>{\"name\":\"rm_rf\",\"arguments\":{}}</tool_call>",
        "<tool_call>{not json}</tool_call>",
        "<tool_call>{\"name\":\"lookup\",\"arguments\":[1,2]}</tool_call>",
        "<tool_call>{\"name\":\"lookup\",\"arguments\":{}}",
        "{\"name\":\"Ana\",\"age\":3}",
        "[{\"name\":\"lookup\",\"arguments\":{}},{\"name\":\"other\",\"arguments\":{}}]",
        "El formato es <tool_call> seguido de JSON.",
    ] {
        let got = run("ollama", Box::leak(text.to_owned().into_boxed_str())).await;
        assert!(got.tool_calls.is_empty(), "{text}");
        assert!(got.content.is_some(), "{text}: stays as text");
    }
    // Not asked for (every other provider): a model quoting a call never runs it.
    let quoted = run(
        "openai",
        "<tool_call>{\"name\":\"lookup\",\"arguments\":{}}</tool_call>",
    )
    .await;
    assert!(quoted.tool_calls.is_empty() && quoted.content.is_some());
    // A real tool call is never touched.
    let server = fake(tool_reply("lookup")).await;
    let real = complete(
        &provider("ollama", &server.base),
        request("qwen3", vec![tool("lookup", json!({"type":"object"}))]),
    )
    .await
    .unwrap();
    assert_eq!(
        (real.tool_calls.len(), real.tool_calls[0].id.as_str()),
        (1, "c1")
    );
}
