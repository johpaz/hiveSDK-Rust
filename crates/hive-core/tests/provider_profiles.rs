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
