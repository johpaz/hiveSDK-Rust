use hive_agent_core::provider::{ModelEvent, OpenAiCompatible, OpenAiConfig};
use hive_agent_core::{Error, ModelRequest};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
};

fn provider(port: u16, timeout_ms: u64, max_response_bytes: usize) -> Arc<OpenAiCompatible> {
    Arc::new(
        OpenAiCompatible::new(OpenAiConfig {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key: None,
            timeout_ms,
            max_response_bytes,
            output_token_parameter: Default::default(),
        })
        .unwrap(),
    )
}
fn request() -> ModelRequest {
    ModelRequest {
        structured_output: None,
        model: "test".into(),
        messages: vec![],
        tools: vec![],
        max_output_tokens: None,
        deadline_unix_ms: None,
        context: None,
    }
}

#[tokio::test]
async fn mismatched_input_framing_is_rejected_before_connecting() {
    use hive_agent_core::{ContextReport, InputTokenFraming, TokenEncoding, TokenEstimate};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let client = provider(port, 1000, 1000);
    let mut req = request();
    req.context = Some(ContextReport {
        summary_id: None,
        retrieved_sources: vec![],
        serialized_bytes: 0,
        dropped_messages: 0,
        token_estimate: Some(TokenEstimate {
            encoding: TokenEncoding::Cl100kBase,
            framing: InputTokenFraming::AnthropicMessagesJson,
            serialized_tokens: 1,
            overhead_tokens: 0,
            total_tokens: 1,
        }),
    });
    assert!(
        matches!(client.start(req, false).next().await, Err(Error::Input(message)) if message.contains("framing"))
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err()
    );
}

#[test]
fn invalid_transport_config_is_rejected_before_starting_requests() {
    for (url, timeout, bytes) in [
        ("file:///tmp/llm", 1000, 100),
        ("https://user:private@example.com/v1", 1000, 100),
        ("https://example.com/v1?key=private", 1000, 100),
        ("https://example.com/v1", 0, 100),
        ("https://example.com/v1", 1000, 0),
    ] {
        let result = OpenAiCompatible::new(OpenAiConfig {
            base_url: url.into(),
            api_key: None,
            timeout_ms: timeout,
            max_response_bytes: bytes,
            output_token_parameter: Default::default(),
        });
        assert!(result.is_err());
    }
}
async fn read_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut data = vec![];
    loop {
        let mut buf = [0; 4096];
        let count = stream.read(&mut buf).await.unwrap();
        assert!(count > 0);
        data.extend_from_slice(&buf[..count]);
        if let Some(end) = data.windows(4).position(|v| v == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&data[..end]).to_lowercase();
            let length: usize = header
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            if data.len() >= end + 4 + length {
                return data;
            }
        }
    }
}
async fn assert_closed(stream: &mut TcpStream) {
    let mut byte = [0];
    let result = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut byte))
        .await
        .expect("client must close socket");
    assert!(matches!(result, Ok(0) | Err(_)));
}

#[tokio::test]
async fn cancellation_before_headers_closes_the_pending_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sent, received) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        sent.send(()).unwrap();
        assert_closed(&mut socket).await;
    });
    let stream = provider(port, 5000, 1_000_000).start(request(), true);
    received.await.unwrap();
    stream.cancel();
    assert!(matches!(stream.next().await, Err(Error::Cancelled)));
    server.await.unwrap();
}

#[tokio::test]
async fn deadline_while_receiving_sse_closes_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n").await.unwrap();
        assert_closed(&mut socket).await;
    });
    let stream = provider(port, 250, 1_000_000).start(request(), true);
    assert!(matches!(
        stream.next().await.unwrap(),
        Some(ModelEvent::TextDelta { .. })
    ));
    assert!(matches!(stream.next().await, Err(Error::Timeout)));
    server.await.unwrap();
}

#[tokio::test]
async fn complete_requests_reuse_the_same_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        for _ in 0..2 {
            read_request(&mut socket).await;
            let body = r#"{"choices":[{"message":{"content":"ok"}}]}"#;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    let client = provider(port, 1000, 1_000_000);
    for _ in 0..2 {
        let stream = client.start(request(), false);
        assert!(matches!(
            stream.next().await.unwrap(),
            Some(ModelEvent::Reply { .. })
        ));
        assert!(stream.next().await.unwrap().is_none());
    }
    server.await.unwrap();
}

#[tokio::test]
async fn missing_done_marker_is_a_failure_after_partial_text() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":\"stop\"}]}\n\n";
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
    let stream = provider(port, 1000, 1_000_000).start(request(), true);
    assert!(matches!(
        stream.next().await.unwrap(),
        Some(ModelEvent::TextDelta { .. })
    ));
    assert!(
        matches!(stream.next().await,Err(Error::Provider(message)) if message.contains("[DONE]"))
    );
    server.await.unwrap();
}

#[tokio::test]
async fn response_byte_limit_applies_before_decoding_json() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n01234567890123456789")
            .await
            .unwrap();
    });
    let stream = provider(port, 1000, 10).start(request(), false);
    assert!(
        matches!(stream.next().await,Err(Error::Provider(message)) if message.contains("byte limit"))
    );
    server.await.unwrap();
}

#[tokio::test]
async fn cancelling_a_backpressured_stream_releases_the_reader() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sent, received) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let frame = "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n";
        let body = frame.repeat(200);
        socket
            .write_all(format!("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{body}").as_bytes())
            .await
            .unwrap();
        sent.send(()).unwrap();
        assert_closed(&mut socket).await;
    });
    let stream = provider(port, 5000, 1_000_000).start(request(), true);
    received.await.unwrap();
    assert!(matches!(
        stream.next().await.unwrap(),
        Some(ModelEvent::TextDelta { .. })
    ));
    stream.cancel();
    assert!(matches!(stream.next().await, Err(Error::Cancelled)));
    server.await.unwrap();
}

#[tokio::test]
async fn global_deadline_wins_over_a_longer_request_timeout_and_closes_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (sent, received) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        sent.send(()).unwrap();
        assert_closed(&mut socket).await;
    });
    let client = provider(port, 5000, 1_000_000);
    let mut request = request();
    request.deadline_unix_ms = Some(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 250,
    );
    let stream = client.start(request, true);
    tokio::time::timeout(Duration::from_secs(3), received)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(stream.next().await, Err(Error::DeadlineExceeded)));
    server.await.unwrap();
}

#[tokio::test]
async fn an_already_expired_request_never_connects() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let client = provider(port, 5000, 1_000_000);
    let mut request = request();
    request.deadline_unix_ms = Some(1);
    let stream = client.start(request, false);
    assert!(matches!(stream.next().await, Err(Error::DeadlineExceeded)));
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn legacy_output_cap_is_explicit_and_internal_deadline_is_not_sent() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let data = read_request(&mut socket).await;
        let offset = data.windows(4).position(|v| v == b"\r\n\r\n").unwrap() + 4;
        let body: serde_json::Value = serde_json::from_slice(&data[offset..]).unwrap();
        assert_eq!(body["max_tokens"], 3);
        assert!(body.get("max_completion_tokens").is_none());
        assert!(body.get("deadline_unix_ms").is_none());
        assert!(body.get("max_output_tokens").is_none());
        let body = r#"{"choices":[{"message":{"content":"ok"}}],"usage":{"prompt_tokens":1,"completion_tokens":2}}"#;
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
    let client = Arc::new(
        OpenAiCompatible::new(OpenAiConfig {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key: None,
            timeout_ms: 1000,
            max_response_bytes: 1_000_000,
            output_token_parameter: hive_agent_core::provider::OutputTokenParameter::MaxTokens,
        })
        .unwrap(),
    );
    let mut request = request();
    request.max_output_tokens = Some(3);
    request.deadline_unix_ms = Some(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 2000,
    );
    let stream = client.start(request, false);
    assert!(matches!(
        stream.next().await.unwrap(),
        Some(ModelEvent::Reply { .. })
    ));
    server.await.unwrap();
}
