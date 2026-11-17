//! The output limit a provider sends when a request names none: derived from the model's context
//! window (15%), unless a limit is configured or the request carries its own.
use hive_agent_core::ModelRequest;
use hive_agent_core::provider::{ModelEvent, NativeProvider};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

/// Serve one request with a canned body and return what the client asked.
async fn ask(kind: Value, extra_request: Value, body: Value) -> Value {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut data = vec![];
        loop {
            let mut chunk = [0; 4096];
            let n = socket.read(&mut chunk).await.unwrap();
            data.extend_from_slice(&chunk[..n]);
            if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&data[..end]).to_lowercase();
                let length: usize = header
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                if data.len() >= end + 4 + length {
                    let text = body.to_string();
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len()).as_bytes()).await.unwrap();
                    return serde_json::from_slice::<Value>(&data[end + 4..end + 4 + length])
                        .unwrap();
                }
            }
        }
    });
    let mut config = kind;
    config["base_url"] = json!(format!(
        "http://127.0.0.1:{port}{}",
        config["base_url"].as_str().unwrap_or("")
    ));
    let provider = NativeProvider::from_json(&config.to_string()).unwrap();
    let mut request = json!({"model":"m","messages":[{"role":"user","content":"hola"}],"tools":[]});
    for (key, value) in extra_request.as_object().unwrap() {
        request[key] = value.clone();
    }
    let events = provider.start(
        serde_json::from_value::<ModelRequest>(request).unwrap(),
        false,
    );
    while let Ok(Some(event)) = events.next().await {
        if matches!(event, ModelEvent::Reply { .. }) {
            break;
        }
    }
    server.await.unwrap()
}

fn openai() -> Value {
    json!({"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}})
}
fn anthropic() -> Value {
    json!({"id":"m","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})
}
fn gemini() -> Value {
    json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1}})
}
fn ollama() -> Value {
    json!({"message":{"role":"assistant","content":"ok"},"done":true,"done_reason":"stop"})
}

#[tokio::test]
async fn every_protocol_derives_its_output_limit_from_the_context_window() {
    // OpenAI-compatible: the cap is sent only when a window is known.
    let with = ask(
        json!({"kind":"openai_compatible","base_url":"/v1","context_window":64_000}),
        json!({}),
        openai(),
    )
    .await;
    assert_eq!(with["max_completion_tokens"], 9_600);
    let without = ask(
        json!({"kind":"openai_compatible","base_url":"/v1"}),
        json!({}),
        openai(),
    )
    .await;
    assert!(
        without.get("max_completion_tokens").is_none() && without.get("max_tokens").is_none(),
        "unchanged without a window"
    );
    let old_name = ask(json!({"kind":"openai_compatible","base_url":"/v1","context_window":64_000,"output_token_parameter":"max_tokens"}), json!({}), openai()).await;
    assert_eq!(
        old_name["max_tokens"], 9_600,
        "the configured parameter name is honoured"
    );
    // Anthropic, Gemini and Ollama have a default instead of "none".
    let a = ask(
        json!({"kind":"anthropic","base_url":"/v1","context_window":200_000}),
        json!({}),
        anthropic(),
    )
    .await;
    assert_eq!(a["max_tokens"], 30_000);
    let a_plain = ask(
        json!({"kind":"anthropic","base_url":"/v1"}),
        json!({}),
        anthropic(),
    )
    .await;
    assert_eq!(
        a_plain["max_tokens"], 4096,
        "the old default without a window"
    );
    let g = ask(
        json!({"kind":"gemini","base_url":"/v1beta","context_window":1_000_000}),
        json!({}),
        gemini(),
    )
    .await;
    assert_eq!(
        g["generationConfig"]["maxOutputTokens"], 32_768,
        "capped at the ceiling"
    );
    let o = ask(
        json!({"kind":"ollama","base_url":"/api","context_window":32_768}),
        json!({}),
        ollama(),
    )
    .await;
    assert_eq!(o["options"]["num_predict"], 4_915);
    let o_ctx = ask(
        json!({"kind":"ollama","base_url":"/api","num_ctx":16_384}),
        json!({}),
        ollama(),
    )
    .await;
    assert_eq!(
        o_ctx["options"]["num_predict"], 2_457,
        "the window Ollama is asked for is the one it has"
    );
}

#[tokio::test]
async fn an_explicit_limit_always_wins_and_bad_values_are_refused() {
    let request_cap = ask(
        json!({"kind":"anthropic","base_url":"/v1","context_window":200_000}),
        json!({"max_output_tokens":77}),
        anthropic(),
    )
    .await;
    assert_eq!(request_cap["max_tokens"], 77, "the request's own limit");
    let configured = ask(json!({"kind":"anthropic","base_url":"/v1","context_window":200_000,"default_max_output_tokens":500}), json!({}), anthropic()).await;
    assert_eq!(
        configured["max_tokens"], 500,
        "the configured default beats the derived one"
    );
    let openai_request = ask(
        json!({"kind":"openai_compatible","base_url":"/v1","context_window":64_000}),
        json!({"max_output_tokens":123}),
        openai(),
    )
    .await;
    assert_eq!(openai_request["max_completion_tokens"], 123);
    for bad in [
        json!({"kind":"anthropic","base_url":"http://x/v1","context_window":0}),
        json!({"kind":"anthropic","base_url":"http://x/v1","default_max_output_tokens":0}),
        json!({"kind":"gemini","base_url":"http://x/v1beta","context_window":0}),
        json!({"kind":"ollama","base_url":"http://x/api","context_window":0}),
    ] {
        assert!(
            NativeProvider::from_json(&bad.to_string()).is_err(),
            "{bad}"
        );
    }
}
