use hive_agent_core::provider::{ModelEvent, NativeProvider, gemini::Gemini};
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
fn provider(port: u16) -> Arc<Gemini> {
    Arc::new(Gemini::from_json(&json!({"base_url":format!("http://127.0.0.1:{port}/v1beta"),"api_key":"test-only","timeout_ms":5000}).to_string()).unwrap())
}
fn request() -> ModelRequest {
    serde_json::from_value(json!({"model":"models/test","messages":[{"role":"user","content":"Read"}],"tools":[],"max_output_tokens":5})).unwrap()
}
fn value(stop: bool) -> Value {
    let mut candidate = json!({"index":0,"content":{"role":"model","parts":[{"text":"🐝","thoughtSignature":"signature"}]}});
    if stop {
        candidate["finishReason"] = json!("STOP");
    }
    json!({"candidates":[candidate],"usageMetadata":{"promptTokenCount":10,"cachedContentTokenCount":4,"candidatesTokenCount":2,"thoughtsTokenCount":1}})
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
            let header = String::from_utf8_lossy(&data[..end]).to_lowercase();
            assert!(header.contains("x-goog-api-key: test-only"));
            assert!(!header.contains("authorization:"));
            assert!(
                header.starts_with("post /v1beta/models/test:generatecontent ")
                    || header
                        .starts_with("post /v1beta/models/test:streamgeneratecontent?alt=sse ")
            );
            let length: usize = header
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            if data.len() >= end + 4 + length {
                let body: Value = serde_json::from_slice(&data[end + 4..end + 4 + length]).unwrap();
                assert_eq!(body["generationConfig"]["maxOutputTokens"], 5);
                assert!(body.get("model").is_none());
                assert!(body.get("stream").is_none());
                return body;
            }
        }
    }
}
async fn closed(socket: &mut TcpStream) {
    let mut byte = [0];
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), socket.read(&mut byte))
            .await
            .unwrap(),
        Ok(0) | Err(_)
    ));
}

#[tokio::test]
async fn native_json_and_sse_routes_headers_utf8_signatures_usage_and_connection_reuse() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        for stream in [false, true] {
            read_request(&mut socket).await;
            let body = if stream {
                format!("data: {}\n\n", value(true))
            } else {
                value(true).to_string()
            };
            socket
                .write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).as_bytes(),
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
        let mut count = 0;
        while let Some(event) = stream.next().await.unwrap() {
            if let ModelEvent::Reply { reply } = event {
                count += 1;
                assert_eq!(reply.content.as_deref(), Some("🐝"));
                assert_eq!(reply.usage.as_ref().unwrap().input_tokens, 10);
                assert_eq!(reply.usage.as_ref().unwrap().output_tokens, 3);
                let state = serde_json::to_value(reply.provider_state).unwrap();
                assert_eq!(state["parts"][0]["thoughtSignature"], "signature");
            }
        }
        assert_eq!(count, 1);
    }
    server.await.unwrap();
}

#[tokio::test]
async fn eof_without_stop_or_inside_a_final_sse_frame_never_yields_a_reply() {
    for partial in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            let body = if partial {
                format!("data: {}\n\ndata: {{", value(true))
            } else {
                format!("data: {}\n\n", value(false))
            };
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
        loop {
            match stream.next().await {
                Ok(Some(ModelEvent::Reply { .. })) => panic!("premature reply"),
                Ok(Some(_)) => {}
                Err(Error::Provider(_)) => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        server.await.unwrap();
    }
}

#[tokio::test]
async fn gemini_cancel_and_global_deadline_close_http_before_headers() {
    for cancel in [true, false] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sent, received) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            sent.send(()).unwrap();
            closed(&mut socket).await;
        });
        let mut req = request();
        if !cancel {
            req.deadline_unix_ms = Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64
                    + 1000,
            );
        }
        let stream = provider(port).start(req, true);
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

#[tokio::test]
async fn a_different_native_protocol_refuses_signed_state_before_connecting() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut req = request();
    req.messages.push(serde_json::from_value(json!({"role":"assistant","content":"done","provider_state":{"provider":"gemini","version":1,"parts":[{"text":"done","thoughtSignature":"sig"}]}})).unwrap());
    for kind in ["openai_compatible", "anthropic"] {
        let provider = NativeProvider::from_json(
            &json!({"kind":kind,"base_url":format!("http://127.0.0.1:{port}/v1"),"api_key":null})
                .to_string(),
        )
        .unwrap();
        let stream = provider.start(req.clone(), false);
        assert!(matches!(stream.next().await, Err(Error::Input(_))));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}
