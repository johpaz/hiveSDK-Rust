use hive_agent_core::provider::{ModelEvent, anthropic::Anthropic};
use hive_agent_core::{Error, ModelRequest};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

fn provider(port: u16) -> Arc<Anthropic> {
    Arc::new(Anthropic::from_json(&json!({"base_url":format!("http://127.0.0.1:{port}/v1"),"api_key":"test-only","timeout_ms":5000,"default_max_output_tokens":9}).to_string()).unwrap())
}
fn request() -> ModelRequest {
    serde_json::from_value(json!({"model":"test","messages":[{"role":"user","content":"Read"}],"tools":[],"max_output_tokens":5,"context":{"serialized_bytes":20,"dropped_messages":0}})).unwrap()
}
async fn read_request(socket: &mut TcpStream) -> Value {
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
            let headers = String::from_utf8_lossy(&data[..end]).to_lowercase();
            assert!(headers.starts_with("post /v1/messages http/1.1"));
            assert!(headers.contains("authorization: bearer test-only"));
            assert!(headers.contains("anthropic-version: 2026-10-21"));
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            if data.len() >= end + 4 + length {
                let body: Value = serde_json::from_slice(&data[end + 4..end + 4 + length]).unwrap();
                assert_eq!(body["max_tokens"], 5);
                assert!(body.get("context").is_none());
                assert!(body.get("deadline_unix_ms").is_none());
                return body;
            }
        }
    }
}
fn frame(value: Value) -> String {
    format!(
        "event: {}\ndata: {value}\n\n",
        value["type"].as_str().unwrap()
    )
}
fn start() -> Value {
    json!({"type":"message_start","message":{"type":"message","role":"assistant","content":[],"stop_reason":null,"usage":{"input_tokens":2,"cache_creation_input_tokens":3,"cache_read_input_tokens":4,"output_tokens":0}}})
}
async fn closed(socket: &mut TcpStream) {
    let mut bytes = [0];
    let result = tokio::time::timeout(Duration::from_secs(3), socket.read(&mut bytes))
        .await
        .unwrap();
    assert!(matches!(result, Ok(0) | Err(_)));
}

#[tokio::test]
async fn json_and_native_sse_use_messages_headers_caps_cache_usage_and_one_reused_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        for streaming in [false, true] {
            let body = read_request(&mut socket).await;
            assert_eq!(body["stream"], streaming);
            assert!(body.get("stream_options").is_none());
            let body = if !streaming {
                json!({"type":"message","role":"assistant","content":[{"type":"text","text":"🐝"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"cache_creation_input_tokens":3,"cache_read_input_tokens":4,"output_tokens":1}}).to_string()
            } else {
                [start(),json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"🐝"}}),json!({"type":"content_block_stop","index":0}),json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),json!({"type":"message_stop"})].into_iter().map(frame).collect::<String>()
            };
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: {}\r\n\r\n",
                        body.len(),
                        if streaming {
                            "text/event-stream"
                        } else {
                            "application/json"
                        }
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            for bytes in body.as_bytes().chunks(1) {
                socket.write_all(bytes).await.unwrap();
            }
        }
    });
    let provider = provider(port);
    for streaming in [false, true] {
        let stream = provider.start(request(), streaming);
        let mut replies = 0;
        while let Some(event) = stream.next().await.unwrap() {
            if let ModelEvent::Reply { reply } = event {
                replies += 1;
                assert_eq!(reply.content.as_deref(), Some("🐝"));
                assert_eq!(reply.usage.as_ref().unwrap().input_tokens, 9);
                assert_eq!(reply.usage.as_ref().unwrap().output_tokens, 1);
            }
        }
        assert_eq!(replies, 1);
    }
    server.await.unwrap();
}

#[tokio::test]
async fn valid_looking_tool_stream_without_message_stop_is_an_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let body=[start(),json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"c","name":"read","input":{}}}),json!({"type":"content_block_stop","index":0}),json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}})].into_iter().map(frame).collect::<String>();
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    let stream = provider(port).start(request(), true);
    let mut replies = 0;
    loop {
        match stream.next().await {
            Ok(Some(ModelEvent::Reply { .. })) => replies += 1,
            Ok(Some(_)) => {}
            Err(Error::Provider(_)) => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(replies, 0);
    server.await.unwrap();
}

#[tokio::test]
async fn cancellation_and_global_deadline_close_anthropic_http_before_headers() {
    for cancel in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sent, received) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            sent.send(()).unwrap();
            closed(&mut socket).await;
        });
        let mut request = request();
        if !cancel {
            request.deadline_unix_ms = Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64
                    + 1000,
            );
        }
        let stream = provider(port).start(request, true);
        received.await.unwrap();
        if cancel {
            stream.cancel();
            assert!(matches!(stream.next().await, Err(Error::Cancelled)));
        } else {
            assert!(matches!(stream.next().await, Err(Error::DeadlineExceeded)));
        }
        server.await.unwrap();
    }
}
