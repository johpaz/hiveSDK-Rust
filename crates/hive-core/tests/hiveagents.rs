use hive_agent_core::provider::NativeProvider;
use serde_json::json;

#[test]
fn native_hiveagents_adapter_accepts_hive_context_and_manages_model_lifecycle() {
    let config = json!({"kind":"hiveagents","base_url":"https://fixture.invalid/v1",
        "api_key":"test-only","context_window":32768});
    assert!(
        NativeProvider::from_json(&config.to_string()).is_ok(),
        "HiveAgents needs its own native adapter; generic OpenAI omits loading and the local alias"
    );
}

#[test]
fn hiveagents_rejects_invalid_context_before_contacting_the_backend() {
    let config = json!({"kind":"hiveagents","base_url":"https://fixture.invalid/v1","api_key":null,"context_window":0});
    assert!(NativeProvider::from_json(&config.to_string()).is_err());
}

// This wire test needs TCP loopback. The lifecycle unit tests run without sockets.
#[tokio::test]
async fn wire_cold_backend_loads_the_gguf_then_infers_once_using_local() {
    use hive_agent_core::{ModelRequest, provider::ModelEvent};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        for step in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(socket.read_u8().await.unwrap());
            }
            let header = String::from_utf8(header).unwrap().to_lowercase();
            assert!(header.contains("authorization: bearer test-only"));
            assert!(!header.contains("x-stainless-"));
            assert!(!header.contains("user-agent:"));
            let len = header
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map_or(0, |s| s.trim().parse::<usize>().unwrap());
            let mut body = vec![0; len];
            socket.read_exact(&mut body).await.unwrap();
            let response=match step {
                0=>{assert!(header.starts_with("get /api/status "));json!({"loaded":false})},
                1=>{
                    assert!(header.starts_with("post /api/load "));
                    let value:serde_json::Value=serde_json::from_slice(&body).unwrap();
                    assert_eq!(value,json!({"model":"Qwen.gguf","config":{"ctx":32768}}));
                    json!({"success":true})
                },
                2=>{assert!(header.starts_with("get /api/status "));json!({"loaded":true,"model":{"name":"Qwen.gguf"}})},
                _=>{
                    assert!(header.starts_with("post /v1/chat/completions "));
                    let value:serde_json::Value=serde_json::from_slice(&body).unwrap();
                    assert_eq!(value["model"],"local");assert_eq!(value["max_tokens"],4);
                    json!({"choices":[{"message":{"content":"Ready","tool_calls":[]},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":2}})
                },
            }.to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",response.len()).as_bytes()).await.unwrap();
        }
    });
    let provider=NativeProvider::from_json(&json!({"kind":"hiveagents","base_url":format!("http://127.0.0.1:{port}/v1"),"api_key":"test-only","context_window":32768,"timeout_ms":5000}).to_string()).unwrap()
        .with_headers(std::collections::HashMap::from([("User-Agent".into(),"blocked-agent".into()),("X-Stainless-Lang".into(),"test".into())])).unwrap();
    let request = ModelRequest {
        model: "Qwen.gguf".into(),
        messages: vec![],
        tools: vec![],
        max_output_tokens: Some(4),
        deadline_unix_ms: None,
        context: None,
        structured_output: None,
    };
    let result = provider
        .start(request, false)
        .next()
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(result,ModelEvent::Reply{reply} if reply.content.as_deref()==Some("Ready")));
    server.await.unwrap();
}
