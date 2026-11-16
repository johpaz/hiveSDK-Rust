use hive_agent_core::provider::{ModelEvent, NativeProvider};
use hive_agent_core::{Error, ModelRequest};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

/// One scripted reply per connection, in order; the last one repeats. `Drop` closes
/// the connection without answering, `Refuse` is not scripted (nothing listens).
#[derive(Clone)]
enum Step {
    Status(u16, &'static [(&'static str, &'static str)]),
    Ok,
    Drop,
    Stall,
}
struct Server {
    port: u16,
    hits: Arc<AtomicUsize>,
}
async fn serve(steps: Vec<Step>) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let index = counter.fetch_add(1, Ordering::SeqCst);
            let step = steps[index.min(steps.len() - 1)].clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 65536];
                let _ = stream.read(&mut buffer).await;
                match step {
                    Step::Drop => {}
                    Step::Stall => tokio::time::sleep(Duration::from_secs(30)).await,
                    Step::Ok => {
                        let body = json!({"choices":[{"message":{"role":"assistant","content":"hola"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}).to_string();
                        let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
                    }
                    Step::Status(code, headers) => {
                        let extra: String = headers
                            .iter()
                            .map(|(k, v)| format!("{k}: {v}\r\n"))
                            .collect();
                        let _ = stream.write_all(format!("HTTP/1.1 {code} X\r\n{extra}Content-Length: 2\r\nConnection: close\r\n\r\nno").as_bytes()).await;
                    }
                }
            });
        }
    });
    Server { port, hits }
}
fn provider(port: u16, resilience: serde_json::Value, timeout_ms: u64) -> NativeProvider {
    NativeProvider::from_json(&json!({"base_url":format!("http://127.0.0.1:{port}/v1"),"timeout_ms":timeout_ms,"resilience":resilience}).to_string()).unwrap()
}
fn request() -> ModelRequest {
    ModelRequest {
        structured_output: None,
        model: "m".into(),
        messages: vec![],
        tools: vec![],
        max_output_tokens: None,
        deadline_unix_ms: None,
        context: None,
    }
}
async fn call(provider: &NativeProvider) -> Result<String, Error> {
    let stream = provider.start(request(), false);
    match stream.next().await? {
        Some(ModelEvent::Reply { reply }) => Ok(reply.content.unwrap_or_default()),
        other => panic!("unexpected {other:?}"),
    }
}
fn message(error: Error) -> String {
    error.to_string()
}
fn quick(attempts: u32) -> serde_json::Value {
    json!({"retry":{"max_attempts":attempts,"base_delay_ms":5,"max_delay_ms":50}})
}

#[tokio::test]
async fn a_rate_limited_or_unavailable_request_is_retried_until_it_is_accepted() {
    let server = serve(vec![
        Step::Status(429, &[]),
        Step::Status(503, &[]),
        Step::Ok,
    ])
    .await;
    assert_eq!(
        call(&provider(server.port, quick(3), 5000)).await.unwrap(),
        "hola"
    );
    assert_eq!(server.hits.load(Ordering::SeqCst), 3);
    // Attempts are bounded: the last refusal is reported as it was.
    let server = serve(vec![Step::Status(429, &[])]).await;
    let error = message(
        call(&provider(server.port, quick(3), 5000))
            .await
            .unwrap_err(),
    );
    assert!(error.contains("HTTP 429"), "{error}");
    assert_eq!(server.hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn nothing_that_may_have_been_processed_is_ever_repeated() {
    // Server errors other than 503, client errors, a dropped connection and a timeout:
    // each is sent exactly once even with retries enabled.
    for (name, step) in [
        ("500", Step::Status(500, &[])),
        ("502", Step::Status(502, &[])),
        ("504", Step::Status(504, &[])),
        ("400", Step::Status(400, &[])),
        ("401", Step::Status(401, &[])),
        ("drop", Step::Drop),
    ] {
        let server = serve(vec![step, Step::Ok]).await;
        assert!(
            call(&provider(server.port, quick(4), 5000)).await.is_err(),
            "{name}"
        );
        assert_eq!(
            server.hits.load(Ordering::SeqCst),
            1,
            "{name} must not be retried"
        );
    }
    let server = serve(vec![Step::Stall, Step::Ok]).await;
    assert!(matches!(
        call(&provider(server.port, quick(4), 300)).await,
        Err(Error::Timeout)
    ));
    assert_eq!(
        server.hits.load(Ordering::SeqCst),
        1,
        "a timeout is uncertain and is not repeated"
    );
}

#[tokio::test]
async fn a_connection_that_never_opened_is_retried_then_reported() {
    // Nothing listens on this port: every try fails to connect, so all of them are made.
    let free = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = free.local_addr().unwrap().port();
    drop(free);
    let started = Instant::now();
    let error = message(
        call(&provider(
            port,
            json!({"retry":{"max_attempts":3,"base_delay_ms":40,"max_delay_ms":80}}),
            5000,
        ))
        .await
        .unwrap_err(),
    );
    assert!(error.contains("HTTP request failed"), "{error}");
    assert!(
        started.elapsed() >= Duration::from_millis(40),
        "it waited between tries: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn retry_after_is_honoured_up_to_the_configured_ceiling() {
    let server = serve(vec![Step::Status(429, &[("Retry-After", "1")]), Step::Ok]).await;
    let started = Instant::now();
    let policy = json!({"retry":{"max_attempts":2,"base_delay_ms":5,"max_delay_ms":2000}});
    assert_eq!(
        call(&provider(server.port, policy, 10_000)).await.unwrap(),
        "hola"
    );
    assert!(
        started.elapsed() >= Duration::from_millis(1000),
        "{:?}",
        started.elapsed()
    );
    // A server that asks for longer than we allow ends the call at once instead of waiting.
    let server = serve(vec![Step::Status(429, &[("Retry-After", "120")]), Step::Ok]).await;
    let started = Instant::now();
    let error = message(
        call(&provider(server.port, quick(3), 5000))
            .await
            .unwrap_err(),
    );
    assert!(
        error.contains("HTTP 429") && started.elapsed() < Duration::from_secs(2),
        "{error}"
    );
    assert_eq!(server.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retries_stay_inside_the_call_timeout_and_can_be_cancelled() {
    let server = serve(vec![Step::Status(503, &[("Retry-After", "1")])]).await;
    let policy = json!({"retry":{"max_attempts":5,"base_delay_ms":5,"max_delay_ms":5000}});
    let started = Instant::now();
    let result = call(&provider(server.port, policy.clone(), 400)).await;
    assert!(matches!(result, Err(Error::Timeout)), "{result:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the call timeout bounds the waiting"
    );
    let slow = provider(
        serve(vec![Step::Status(503, &[("Retry-After", "1")])])
            .await
            .port,
        policy,
        10_000,
    );
    let stream = slow.start(request(), false);
    tokio::time::sleep(Duration::from_millis(100)).await;
    stream.cancel();
    // Cancelling ends the stream promptly instead of sitting out the Retry-After wait.
    let ended = tokio::time::timeout(Duration::from_millis(800), stream.next())
        .await
        .expect("a cancelled call does not keep waiting");
    assert!(
        matches!(ended, Ok(None) | Err(Error::Cancelled)),
        "{ended:?}"
    );
}

#[tokio::test]
async fn the_breaker_opens_fails_fast_without_sending_and_closes_after_a_good_probe() {
    let server = serve(vec![
        Step::Status(500, &[]),
        Step::Status(500, &[]),
        Step::Ok,
    ])
    .await;
    let breaker = json!({"circuit_breaker":{"failure_threshold":2,"cooldown_ms":300}});
    let provider = provider(server.port, breaker, 5000);
    assert!(
        call(&provider)
            .await
            .unwrap_err()
            .to_string()
            .contains("HTTP 500")
    );
    assert!(
        call(&provider)
            .await
            .unwrap_err()
            .to_string()
            .contains("HTTP 500")
    );
    assert_eq!(server.hits.load(Ordering::SeqCst), 2);
    // Open: refused locally, nothing reaches the server.
    for _ in 0..3 {
        let error = message(call(&provider).await.unwrap_err());
        assert!(error.contains("PROVIDER_CIRCUIT_OPEN"), "{error}");
    }
    assert_eq!(
        server.hits.load(Ordering::SeqCst),
        2,
        "an open breaker sends nothing"
    );
    // After the cool-down one probe goes through; it succeeds and the breaker closes.
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(call(&provider).await.unwrap(), "hola");
    assert_eq!(call(&provider).await.unwrap(), "hola");
    assert_eq!(server.hits.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn a_failed_probe_reopens_the_breaker_and_client_errors_do_not_trip_it() {
    let server = serve(vec![Step::Status(500, &[])]).await;
    let provider = provider(
        server.port,
        json!({"circuit_breaker":{"failure_threshold":1,"cooldown_ms":200}}),
        5000,
    );
    assert!(call(&provider).await.is_err());
    assert!(message(call(&provider).await.unwrap_err()).contains("CIRCUIT_OPEN"));
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(
        message(call(&provider).await.unwrap_err()).contains("HTTP 500"),
        "the probe reached the server"
    );
    assert!(
        message(call(&provider).await.unwrap_err()).contains("CIRCUIT_OPEN"),
        "and its failure reopened it"
    );
    assert_eq!(server.hits.load(Ordering::SeqCst), 2);
    // 4xx answers prove the endpoint is up: they never open the breaker.
    let client_errors = serve(vec![Step::Status(401, &[])]).await;
    let guarded = self::provider(
        client_errors.port,
        json!({"circuit_breaker":{"failure_threshold":1,"cooldown_ms":60000}}),
        5000,
    );
    for _ in 0..4 {
        assert!(message(call(&guarded).await.unwrap_err()).contains("HTTP 401"));
    }
    assert_eq!(client_errors.hits.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn a_probe_that_is_cancelled_cannot_leave_the_breaker_stuck() {
    let server = serve(vec![Step::Status(500, &[]), Step::Stall, Step::Ok]).await;
    let provider = provider(
        server.port,
        json!({"circuit_breaker":{"failure_threshold":1,"cooldown_ms":200}}),
        5000,
    );
    assert!(call(&provider).await.is_err());
    tokio::time::sleep(Duration::from_millis(250)).await;
    let probe = provider.start(request(), false);
    tokio::time::sleep(Duration::from_millis(80)).await;
    probe.cancel();
    drop(probe);
    // While the (now abandoned) probe is recent, others are still refused...
    assert!(message(call(&provider).await.unwrap_err()).contains("CIRCUIT_OPEN"));
    // ...but it expires after a cool-down, so a later call probes again and recovers.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(call(&provider).await.unwrap(), "hola");
}

#[test]
fn resilience_config_is_validated_and_never_applies_to_managed_providers() {
    let build = |resilience: serde_json::Value| {
        NativeProvider::from_json(
            &json!({"base_url":"http://127.0.0.1:1/v1","resilience":resilience}).to_string(),
        )
    };
    assert!(build(json!({})).is_ok());
    assert!(build(quick(5)).is_ok());
    for bad in [
        json!({"retry":{"max_attempts":0}}),
        json!({"retry":{"max_attempts":6}}),
        json!({"retry":{"base_delay_ms":0}}),
        json!({"retry":{"base_delay_ms":100,"max_delay_ms":50}}),
        json!({"retry":{"max_delay_ms":999_999}}),
        json!({"circuit_breaker":{"failure_threshold":0}}),
        json!({"circuit_breaker":{"cooldown_ms":1}}),
        json!({"retries":3}),
        json!({"retry":{"jitter":true}}),
    ] {
        assert!(build(bad.clone()).is_err(), "{bad}");
    }
    for kind in ["anthropic", "gemini"] {
        let config = json!({"kind":kind,"base_url":"http://127.0.0.1:1","api_key":"k","resilience":quick(2)}).to_string();
        assert!(
            NativeProvider::from_json(&config).is_ok(),
            "{kind} supports it"
        );
    }
    let managed = json!({"kind":"hiveagents","resilience":quick(2)}).to_string();
    assert!(NativeProvider::from_json(&managed).is_err());
}
