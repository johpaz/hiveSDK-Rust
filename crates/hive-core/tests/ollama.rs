//! Ollama's native chat API: request bodies, non-streaming and NDJSON replies, and what happens
//! when the server misbehaves, against a fake server.
use hive_agent_core::provider::{ModelEvent, NativeProvider, ollama::Ollama};
use hive_agent_core::{Error, ModelRequest};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

fn provider(port: u16, extra: Value) -> Arc<Ollama> {
    let mut config = json!({"base_url":format!("http://127.0.0.1:{port}/api"),"timeout_ms":5000});
    for (key, value) in extra.as_object().unwrap() {
        config[key] = value.clone();
    }
    Arc::new(Ollama::from_json(&config.to_string()).unwrap())
}
fn request(extra: Value) -> ModelRequest {
    let mut value = json!({"model":"llama3.2","messages":[{"role":"user","content":"Hola"}],"tools":[],"max_output_tokens":50});
    for (key, v) in extra.as_object().unwrap() {
        value[key] = v.clone();
    }
    serde_json::from_value(value).unwrap()
}
async fn read_request(socket: &mut TcpStream) -> (String, Value) {
    let mut data = vec![];
    loop {
        let mut bytes = [0; 4096];
        let n = tokio::time::timeout(Duration::from_secs(3), socket.read(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert!(n > 0);
        data.extend_from_slice(&bytes[..n]);
        if let Some(end) = data.windows(4).position(|v| v == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&data[..end]).to_lowercase();
            let length: usize = header
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            if data.len() >= end + 4 + length {
                return (
                    header,
                    serde_json::from_slice(&data[end + 4..end + 4 + length]).unwrap(),
                );
            }
        }
    }
}
/// Serve one request with the given status and body, returning what was asked.
async fn serve(
    status: &str,
    content_type: &str,
    body: String,
) -> (u16, tokio::task::JoinHandle<(String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (status, content_type) = (status.to_owned(), content_type.to_owned());
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let asked = read_request(&mut socket).await;
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
        asked
    });
    (port, task)
}
async fn collect(
    provider: &Arc<Ollama>,
    request: ModelRequest,
    stream: bool,
) -> Result<Vec<ModelEvent>, Error> {
    let events = provider.start(request, stream);
    let mut out = vec![];
    while let Some(event) = events.next().await? {
        out.push(event);
    }
    Ok(out)
}
fn reply(events: &[ModelEvent]) -> &hive_agent_core::ModelReply {
    events
        .iter()
        .find_map(|e| {
            if let ModelEvent::Reply { reply } = e {
                Some(reply)
            } else {
                None
            }
        })
        .expect("a reply")
}

#[tokio::test]
async fn a_plain_answer_goes_to_api_chat_with_the_options_and_reports_usage() {
    let body = json!({"model":"llama3.2","message":{"role":"assistant","content":"Hola, soy Ollama","thinking":"pensando"},"done":true,"done_reason":"stop","prompt_eval_count":12,"eval_count":5}).to_string();
    let (port, task) = serve("200 OK", "application/json", body).await;
    let provider = provider(
        port,
        json!({"api_key":"test-only","num_ctx":16384,"keep_alive":"10m","think":false}),
    );
    let events = collect(&provider, request(json!({"messages":[{"role":"system","content":"Sé breve"},{"role":"user","content":"Hola"}]})), false).await.unwrap();
    let reply = reply(&events);
    assert_eq!(
        reply.content.as_deref(),
        Some("Hola, soy Ollama"),
        "the thinking text is not part of the answer"
    );
    assert!(reply.tool_calls.is_empty() && reply.provider_state.is_none());
    let usage = reply.usage.clone().unwrap();
    assert_eq!((usage.input_tokens, usage.output_tokens), (12, 5));
    let (header, body) = task.await.unwrap();
    assert!(header.starts_with("post /api/chat "), "{header}");
    assert!(header.contains("authorization: bearer test-only"));
    assert_eq!(body["model"], "llama3.2");
    assert_eq!(body["stream"], false);
    assert_eq!(
        body["options"],
        json!({"num_predict":50,"num_ctx":16384}),
        "the context size is asked for, not left to Ollama's small default"
    );
    assert_eq!(
        (body["think"].clone(), body["keep_alive"].clone()),
        (json!(false), json!("10m"))
    );
    assert_eq!(
        body["messages"],
        json!([{"role":"system","content":"Sé breve"},{"role":"user","content":"Hola"}])
    );
    assert!(body.get("tools").is_none() && body.get("format").is_none());
}

#[tokio::test]
async fn tools_calls_results_images_and_structured_output_use_the_native_shapes() {
    let body = json!({"message":{"role":"assistant","content":"","tool_calls":[
        {"function":{"name":"lookup","arguments":{"q":"respuesta"}}},
        {"id":"call_srv","function":{"name":"lookup","arguments":"{\"q\":\"otra\"}"}}]},"done":true,"done_reason":"stop"}).to_string();
    let (port, task) = serve("200 OK", "application/json", body).await;
    let provider = provider(port, json!({}));
    let tool = json!({"name":"lookup","description":"Busca","parameters":{"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}});
    let history = json!([
        {"role":"user","content":"Mira","attachments":[{"kind":"image","media_type":"image/png","data":"aGk="}]},
        {"role":"assistant","content":null,"tool_calls":[{"id":"t1","name":"lookup","arguments":{"q":"x"}}]},
        {"role":"tool","content":"{\"ok\":true}","tool_call_id":"t1"},
        {"role":"assistant","content":"Hecho"},
        {"role":"user","content":"Otra"}]);
    let events = collect(&provider, request(json!({"messages":history,"tools":[tool],"structured_output":{"name":"out","schema":{"type":"object","properties":{"a":{"type":"string"}}}}})), false).await.unwrap();
    let reply = reply(&events);
    assert!(reply.content.is_none());
    assert_eq!(reply.tool_calls.len(), 2);
    assert_eq!(
        (
            reply.tool_calls[0].name.as_str(),
            &reply.tool_calls[0].arguments
        ),
        ("lookup", &json!({"q":"respuesta"}))
    );
    assert_eq!(
        reply.tool_calls[1].arguments,
        json!({"q":"otra"}),
        "arguments given as JSON text are read"
    );
    assert_eq!(
        reply.tool_calls[1].id, "call_srv",
        "a server-given id is kept"
    );
    assert!(
        reply.tool_calls[0].id.starts_with("call_")
            && reply.tool_calls[0].id != reply.tool_calls[1].id,
        "made ids are unique"
    );
    let (_, body) = task.await.unwrap();
    assert_eq!(
        body["tools"][0],
        json!({"type":"function","function":{"name":"lookup","description":"Busca","parameters":tool["parameters"]}})
    );
    assert_eq!(
        body["format"],
        json!({"type":"object","properties":{"a":{"type":"string"}}})
    );
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(
        messages[0]["images"],
        json!(["aGk="]),
        "inline base64, no data: prefix"
    );
    assert_eq!(
        messages[1]["tool_calls"],
        json!([{"function":{"name":"lookup","arguments":{"q":"x"}}}])
    );
    assert_eq!(messages[1]["content"], "");
    assert_eq!(
        messages[2],
        json!({"role":"tool","tool_name":"lookup","content":"{\"ok\":true}"}),
        "the result is tied to its call by the tool's name"
    );
}

#[tokio::test]
async fn an_ndjson_stream_gives_text_deltas_whole_tool_calls_usage_and_one_reply() {
    let lines = [
        json!({"message":{"role":"assistant","content":"Ho"},"done":false}),
        json!({"message":{"role":"assistant","content":"la 🐝"},"done":false}),
        json!({"message":{"role":"assistant","content":"","tool_calls":[{"function":{"name":"lookup","arguments":{"q":"x"}}}]},"done":false}),
        json!({"message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":7,"eval_count":3}),
    ];
    let body = lines
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let (port, task) = serve("200 OK", "application/x-ndjson", body).await;
    let events = collect(&provider(port, json!({})), request(json!({})), true)
        .await
        .unwrap();
    let texts: Vec<&str> = events
        .iter()
        .filter_map(|e| {
            if let ModelEvent::TextDelta { text } = e {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(texts, ["Ho", "la 🐝"]);
    assert!(events.iter().any(|e| matches!(e, ModelEvent::ToolCallDelta { index: 0, name: Some(n), arguments, .. } if n == "lookup" && arguments == "{\"q\":\"x\"}")));
    assert!(events.iter().any(|e| matches!(e, ModelEvent::Usage { usage } if usage.input_tokens == 7 && usage.output_tokens == 3)));
    let reply = reply(&events);
    assert_eq!(reply.content.as_deref(), Some("Hola 🐝"));
    assert_eq!(reply.tool_calls.len(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ModelEvent::Reply { .. }))
            .count(),
        1
    );
    assert_eq!(task.await.unwrap().1["stream"], true);
}

#[tokio::test]
async fn a_server_that_misbehaves_is_an_error_never_a_partial_answer() {
    let cases: Vec<(&str, &str, String, &str)> = vec![
        ("500 Internal Server Error", "application/json", json!({"error":"secreto de la ruta /home/x"}).to_string(), "HTTP 500"),
        ("200 OK", "application/json", json!({"error":"model not found"}).to_string(), "server reported an error"),
        ("200 OK", "application/json", json!({"message":{"content":"corta"},"done":true,"done_reason":"length"}).to_string(), "truncated"),
        ("200 OK", "application/json", json!({"message":{"content":"x"},"done":false}).to_string(), "incomplete"),
        ("200 OK", "application/json", json!({"message":{"content":""},"done":true,"done_reason":"stop"}).to_string(), "neither text nor tool"),
        ("200 OK", "application/json", json!({"message":{"content":"","tool_calls":[{"function":{"arguments":{}}}]},"done":true}).to_string(), "without a name"),
        ("200 OK", "application/json", json!({"message":{"content":"","tool_calls":[{"function":{"name":"a","arguments":[1]}}]},"done":true}).to_string(), "JSON object"),
        ("200 OK", "application/json", "no es json".to_owned(), "invalid response JSON"),
    ];
    for (status, content_type, body, expected) in cases {
        let (port, _task) = serve(status, content_type, body).await;
        let error = collect(&provider(port, json!({})), request(json!({})), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{expected}: {error}");
        assert!(
            !error.contains("secreto") && !error.contains("/home/x"),
            "the server's text is not echoed: {error}"
        );
    }
    // Streams: cut before `done`, an error line, a truncated answer, and a line past the limit.
    for (body, expected) in [
        (
            format!("{}\n", json!({"message":{"content":"par"},"done":false})),
            "ended before",
        ),
        (
            format!("{}\n", json!({"error":"se cayó"})),
            "server reported an error",
        ),
        (
            format!(
                "{}\n",
                json!({"message":{"content":"x"},"done":true,"done_reason":"length"})
            ),
            "truncated",
        ),
        ("{ no json\n".to_owned(), "invalid JSON"),
    ] {
        let (port, _task) = serve("200 OK", "application/x-ndjson", body).await;
        let error = collect(&provider(port, json!({})), request(json!({})), true)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{expected}: {error}");
    }
    let (port, _task) = serve("200 OK", "application/x-ndjson", "x".repeat(4000)).await;
    let small = provider(port, json!({"max_response_bytes":1000}));
    assert!(
        collect(&small, request(json!({})), true)
            .await
            .unwrap_err()
            .to_string()
            .contains("byte limit")
    );
}

#[tokio::test]
async fn what_the_native_api_cannot_carry_is_refused_not_dropped() {
    let provider = provider(9, json!({}));
    for attachment in [
        json!({"kind":"image","media_type":"image/png","url":"https://example.com/a.png"}),
        json!({"kind":"audio","media_type":"audio/wav","data":"aGk="}),
        json!({"kind":"document","media_type":"application/pdf","data":"aGk="}),
    ] {
        let req =
            request(json!({"messages":[{"role":"user","content":"x","attachments":[attachment]}]}));
        let error = collect(&provider, req, false).await.unwrap_err();
        assert!(matches!(error, Error::Input(_)), "{error:?}");
    }
    assert!(
        collect(&provider, request(json!({"model":""})), false)
            .await
            .is_err()
    );
    assert!(
        collect(&provider, request(json!({"max_output_tokens":0})), false)
            .await
            .is_err()
    );
    // Configuration is checked up front.
    for config in [
        json!({"base_url":"ftp://x/api"}),
        json!({"base_url":"http://x/api","num_ctx":0}),
        json!({"base_url":"http://x/api","keep_alive":"a b"}),
        json!({"base_url":"http://x/api","default_max_output_tokens":0}),
        json!({"base_url":"http://x/api","surprise":1}),
    ] {
        assert!(Ollama::from_json(&config.to_string()).is_err(), "{config}");
    }
}

#[tokio::test]
async fn native_provider_builds_kind_ollama_and_the_engine_can_use_it_end_to_end() {
    let body = json!({"message":{"role":"assistant","content":"listo"},"done":true,"done_reason":"stop","prompt_eval_count":3,"eval_count":1}).to_string();
    let (port, task) = serve("200 OK", "application/json", body).await;
    let native = NativeProvider::from_json(
        &json!({"kind":"ollama","base_url":format!("http://127.0.0.1:{port}/api"),"num_ctx":8192})
            .to_string(),
    )
    .unwrap();
    let events = native.start(request(json!({})), false);
    let mut got = None;
    while let Some(event) = events.next().await.unwrap() {
        if let ModelEvent::Reply { reply } = event {
            got = Some(reply);
        }
    }
    assert_eq!(got.unwrap().content.as_deref(), Some("listo"));
    assert_eq!(task.await.unwrap().1["options"]["num_ctx"], 8192);
    // A profile is an OpenAI-compatible notion.
    assert!(
        NativeProvider::from_json(
            &json!({"kind":"ollama","base_url":"http://x/api","profile":{}}).to_string()
        )
        .is_err()
    );
    // Provider state of another protocol is not replayed through Ollama.
    let foreign: ModelRequest = serde_json::from_value(json!({"model":"m","messages":[{"role":"user","content":"x"},{"role":"assistant","content":"y","provider_state":{"provider":"reasoning","content":"z"}}],"tools":[]})).unwrap();
    let native = NativeProvider::from_json(
        &json!({"kind":"ollama","base_url":"http://127.0.0.1:9/api"}).to_string(),
    )
    .unwrap();
    let events = native.start(foreign, false);
    assert!(events.next().await.is_err());
}

#[test]
fn the_output_limit_is_derived_from_the_context_window_unless_one_is_given() {
    use hive_agent_core::provider::{MAX_DERIVED_OUTPUT_TOKENS, derive_output_cap};
    // 15% of the window, never less than 1 and never more than the ceiling.
    assert_eq!(derive_output_cap(8_192), 1_228);
    assert_eq!(derive_output_cap(128_000), 19_200);
    assert_eq!(derive_output_cap(1_000_000), MAX_DERIVED_OUTPUT_TOKENS);
    assert_eq!(derive_output_cap(3), 1);
    assert_eq!(derive_output_cap(u64::MAX), MAX_DERIVED_OUTPUT_TOKENS);
}
