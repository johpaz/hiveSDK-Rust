use super::*;
use crate::config;
use hivedb_core::HiveDB;
use std::{path::Path, sync::Mutex};
use tokio::net::{TcpListener, TcpStream};

fn seed(home: &Path, base: &str) {
    std::fs::create_dir_all(home.join("data")).unwrap();
    std::fs::write(home.join(".master.key"), hex::encode([7u8; 32])).unwrap();
    let db = HiveDB::open(home.join("data/hivedb")).unwrap();
    for (collection, id, doc) in [
        (
            "users",
            "u1",
            json!({"id":"u1","name":"Prueba","email":null,"password_hash":null}),
        ),
        (
            "agents",
            "a1",
            json!({"id":"a1","user_id":"u1","name":"Hive","role":"coordinator","enabled":true,
            "provider_id":"openrouter","model_id":"openrouter/vendor/model","system_prompt":"Responde en español","max_iterations":8}),
        ),
        (
            "providers",
            "openrouter",
            json!({"id":"openrouter","base_url":base,"active":true,"enabled":true,"category":"llm"}),
        ),
        (
            "models",
            "openrouter/vendor/model",
            json!({"id":"openrouter/vendor/model","provider_id":"openrouter","active":true,"enabled":true,"model_type":"llm"}),
        ),
        (
            "secrets",
            "provider:openrouter:api_key",
            json!({"ciphertext":"4397db772f5a3b6f11253a:00817a4b10c76182d25e8b7933ea36fb","iv":"030303030303030303030303"}),
        ),
        (
            "secrets",
            "provider:openrouter:headers",
            json!({"ciphertext":"5edcfb2e1c4126360f32267ed1758c688611929ac7e1b60d:b0122c250ff3b8e5794d822377c580a7","iv":"030303030303030303030303"}),
        ),
    ] {
        db.col_put(collection, id, &doc, PutOptions::default())
            .unwrap();
    }
}

// No sockets: exercise the production durable turn driver with deterministic SDK model events.
struct FixtureProvider {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    stall: bool,
}
struct FixtureStream {
    events: Mutex<std::collections::VecDeque<ModelEvent>>,
    stall: bool,
}
impl TurnProvider for FixtureProvider {
    type Stream = FixtureStream;
    type Child = FixtureProvider;
    fn child(&self, _: NativeProvider) -> Self::Child {
        FixtureProvider {
            requests: self.requests.clone(),
            stall: self.stall,
        }
    }
    fn start(&self, request: ModelRequest) -> Self::Stream {
        self.requests.lock().unwrap().push(request);
        FixtureStream { stall:self.stall, events: Mutex::new([
            ModelEvent::TextDelta { text:"Hola ".into() },
            ModelEvent::TextDelta { text:"Rust".into() },
            ModelEvent::Reply { reply:serde_json::from_value(json!({"content":"Hola Rust","tool_calls":[],"usage":{"input_tokens":10,"output_tokens":2}})).unwrap() },
        ].into()) }
    }
}
impl TurnStream for FixtureStream {
    async fn next(&self) -> Result<Option<ModelEvent>> {
        if self.stall {
            std::future::pending::<()>().await;
        }
        Ok(self.events.lock().unwrap().pop_front())
    }
    fn cancel(&self) {}
}

#[tokio::test]
async fn durable_driver_streams_context_and_reopens_without_network() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let (_, created) = route(
        &app,
        Request::post("/api/conversations")
            .body(b"{}".to_vec())
            .unwrap(),
    )
    .unwrap();
    let thread = created["conversation"]["id"].as_str().unwrap().to_owned();
    let selected = app.selection().unwrap();
    let handle = app.session(&thread, &selected).unwrap();
    for prompt in ["primero", "segundo"] {
        let (output, mut rx) = mpsc::channel(32);
        drive_turn(
            app.clone(),
            handle.clone(),
            FixtureProvider {
                requests: requests.clone(),
                stall: false,
            },
            &thread,
            prompt.into(),
            output,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        assert_eq!(
            events
                .iter()
                .filter(|v| v["isChunk"] == true)
                .map(|v| v["content"].as_str().unwrap())
                .collect::<String>(),
            "Hola Rust"
        );
        assert_eq!(events.last().unwrap()["processStatus"], "done");
    }
    assert_eq!(requests.lock().unwrap()[1].messages.len(), 4);
    assert_eq!(requests.lock().unwrap()[0].model, "vendor/model");
    let rows = app
        .db
        .col_scan(
            "conversations",
            &hivedb_core::ScanOptions {
                prefix: Some(format!("{thread}:")),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(rows.len(), 4);
    assert!(rows[0].id.ends_with(":000000000000001"));
    assert!(rows[3].id.ends_with(":000000000000004"));
    assert!(rows.iter().all(|r| r.doc["source"] == "message"));
    drop(handle);
    drop(selected);
    drop(app);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let selected = app.selection().unwrap();
    let handle = app.session(&thread, &selected).unwrap();
    let (output, _rx) = mpsc::channel(32);
    drive_turn(
        app,
        handle,
        FixtureProvider {
            requests: requests.clone(),
            stall: false,
        },
        &thread,
        "tercero".into(),
        output,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(requests.lock().unwrap()[2].messages.len(), 6);
}

#[tokio::test]
async fn durable_driver_cancellation_preserves_uncertain_operation_without_network() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let (_, created) = route(
        &app,
        Request::post("/api/conversations")
            .body(b"{}".to_vec())
            .unwrap(),
    )
    .unwrap();
    let thread = created["conversation"]["id"].as_str().unwrap().to_owned();
    let selected = app.selection().unwrap();
    let handle = app.session(&thread, &selected).unwrap();
    let token = CancellationToken::new();
    let canceller = token.clone();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observed = requests.clone();
    tokio::spawn(async move {
        loop {
            if !observed.lock().unwrap().is_empty() {
                canceller.cancel();
                break;
            }
            tokio::task::yield_now().await;
        }
    });
    let (output, _rx) = mpsc::channel(32);
    let result = drive_turn(
        app.clone(),
        handle.clone(),
        FixtureProvider {
            requests: requests.clone(),
            stall: true,
        },
        &thread,
        "hola".into(),
        output,
        token,
    )
    .await;
    assert!(result.unwrap_err().to_string().contains("cancelado"));
    assert!(
        dispatch(&handle, json!({"op":"begin_run","prompt":"otra"}))
            .await
            .is_err()
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
    drop(handle);
    drop(selected);
    drop(app);
    let app = App::open(home.path().to_owned()).unwrap();
    assert!(app.session(&thread, &app.selection().unwrap()).is_err());
}

#[test]
fn hive_auth_and_http_routes_without_network() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = App::open(home.path().to_owned()).unwrap();
    app.db
        .col_put(
            "users",
            "u1",
            &json!({"id":"u1","email":"test@example.com",
        "password_hash":"$2b$04$abcdefghijklmnopqrstuuxDugxrpcpW3csMl3JkYMLtX7yxJF.2."}),
            PutOptions::default(),
        )
        .unwrap();
    assert_eq!(
        route(&app, Request::get("/api/users").body(vec![]).unwrap())
            .unwrap()
            .0,
        401
    );
    let login = |password: &str| {
        route(
            &app,
            Request::post("/api/auth/login")
                .body(
                    json!({"email":" TEST@example.com ","password":password})
                        .to_string()
                        .into_bytes(),
                )
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(login("bad").0, 401);
    assert_eq!(login("fixture-password").1["authToken"], app.token);
    let (code, users) = route(
        &app,
        Request::get("/api/users")
            .header("Authorization", format!("Bearer {}", app.token))
            .body(vec![])
            .unwrap(),
    )
    .unwrap();
    assert_eq!(code, 200);
    assert!(!users.to_string().contains("password"));
    assert!(config::verify_password("fixture-password","$argon2id$v=19$m=4096,t=2,p=1$Zml4dHVyZS1zYWx0LTEyMw$mKtogUCitoeq+g3Lc2tcpR/QM9bXmV2mXQGaPumnfSQ").unwrap());
    assert!(!config::verify_password("incorrecta","$argon2id$v=19$m=4096,t=2,p=1$Zml4dHVyZS1zYWx0LTEyMw$mKtogUCitoeq+g3Lc2tcpR/QM9bXmV2mXQGaPumnfSQ").unwrap());
}

async fn serve(app: Arc<App>) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let app = app.clone();
            tokio::spawn(async move {
                let _ = connection(app, socket).await;
            });
        }
    });
    (address, task)
}

async fn provider() -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let requests = seen.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let requests = requests.clone();
            tokio::spawn(async move {
                let request = read_request(&mut socket).await.unwrap();
                assert_eq!(request.uri().path(), "/v1/chat/completions");
                assert_eq!(request.headers()["authorization"], "Bearer fixture-key");
                assert_eq!(request.headers()["x-fixture"], "sdk-rust");
                let body: Value = serde_json::from_slice(request.body()).unwrap();
                let stall =
                    body["messages"].as_array().unwrap().last().unwrap()["content"] == "stall";
                let fail =
                    body["messages"].as_array().unwrap().last().unwrap()["content"] == "fail";
                requests.lock().unwrap().push(body);
                if fail {
                    respond(&mut socket, 429, json!({"error":"fixture"}))
                        .await
                        .unwrap();
                    return;
                }
                let mut sse = String::new();
                for value in [
                    json!({"choices":[{"index":0,"delta":{"content":"Hola "},"finish_reason":null}]}),
                    json!({"choices":[{"index":0,"delta":{"content":"Rust"},"finish_reason":null}]}),
                    json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                    json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
                ] {
                    sse.push_str(&format!("data: {value}\n\n"));
                }
                sse.push_str("data: [DONE]\n\n");
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    sse.len()
                );
                socket.write_all(headers.as_bytes()).await.unwrap();
                if stall {
                    let _ = socket.write_all(b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n").await;
                    let _ = socket.read_u8().await;
                    return;
                }
                let _ = socket.write_all(sse.as_bytes()).await;
            });
        }
    });
    (base, seen, task)
}

type Client = WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>;
async fn connect_ws(base: &str) -> Client {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!(
        "{}/ws?session=u1",
        base.replacen("http", "ws", 1)
    ))
    .await
    .unwrap();
    let welcome = frame(&mut ws).await;
    assert_eq!(welcome["type"], "welcome");
    ws
}
async fn frame<S: AsyncRead + AsyncWrite + Unpin>(ws: &mut WebSocketStream<S>) -> Value {
    let message = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    serde_json::from_str(message.to_text().unwrap()).unwrap()
}
async fn message(ws: &mut Client, thread: &str, content: &str) -> Vec<Value> {
    ws.send(Message::Text(
        json!({"type":"message","threadId":thread,"content":content}).to_string(),
    ))
    .await
    .unwrap();
    let mut events = Vec::new();
    loop {
        let next = frame(ws).await;
        let done = next["type"] == "process"
            && next["turnDone"] != false
            && matches!(next["processStatus"].as_str(), Some("done" | "error"));
        events.push(next);
        if done {
            return events;
        }
    }
}
async fn conversation(base: &str) -> String {
    reqwest::Client::new()
        .post(format!("{base}/api/conversations"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["conversation"]["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn real_http_ws_sdk_streams_two_turns_and_reopens_hivedb() {
    let (provider_url, seen, provider_task) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &provider_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let (base, server) = serve(app.clone()).await;
    let health = reqwest::get(format!("{base}/health"))
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(health["backend"], "hive-gateway-rust");
    assert_eq!(health["capabilities"], json!(CAPABILITIES));
    let thread = conversation(&base).await;
    let mut ws = connect_ws(&base).await;
    for prompt in ["primero", "segundo"] {
        let events = message(&mut ws, &thread, prompt).await;
        assert!(!events.iter().any(|v| v["type"] == "error"), "{events:?}");
        assert_eq!(
            events
                .iter()
                .filter(|v| v["isChunk"] == true)
                .map(|v| v["content"].as_str().unwrap())
                .collect::<String>(),
            "Hola Rust"
        );
        assert_eq!(events.last().unwrap()["processStatus"], "done");
    }
    {
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["model"], "vendor/model");
        assert_eq!(requests[1]["messages"].as_array().unwrap().len(), 4);
        assert_eq!(requests[1]["messages"][2]["content"], "Hola Rust");
    }
    assert_eq!(config::docs(&app.db, "conversations").unwrap().len(), 4);
    ws.close(None).await.unwrap();
    drop(ws);
    app.shutdown.cancel();
    server.abort();
    let _ = server.await;
    for _ in 0..100 {
        if Arc::strong_count(&app) == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(Arc::strong_count(&app), 1);
    drop(app);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let (base, server) = serve(app.clone()).await;
    let mut ws = connect_ws(&base).await;
    let events = message(&mut ws, &thread, "tercero").await;
    assert_eq!(
        events.last().unwrap()["processStatus"],
        "done",
        "{events:?}"
    );
    assert_eq!(
        seen.lock().unwrap()[2]["messages"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    app.shutdown.cancel();
    server.abort();
    provider_task.abort();
}

#[tokio::test]
async fn cancellation_and_provider_failure_are_not_retried() {
    let (url, seen, provider_task) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let (base, server) = serve(app.clone()).await;
    let mut ws = connect_ws(&base).await;
    let thread = conversation(&base).await;
    ws.send(Message::Text(
        json!({"type":"message","threadId":thread,"content":"stall"}).to_string(),
    ))
    .await
    .unwrap();
    loop {
        if frame(&mut ws).await["isChunk"] == true {
            break;
        }
    }
    ws.send(Message::Text(
        json!({"type":"stop","threadId":thread}).to_string(),
    ))
    .await
    .unwrap();
    loop {
        let event = frame(&mut ws).await;
        if event["type"] == "error" {
            assert!(event["error"].as_str().unwrap().contains("cancelado"));
            break;
        }
    }
    assert_eq!(frame(&mut ws).await["processStatus"], "error");
    let again = message(&mut ws, &thread, "no repetir").await;
    assert!(again.iter().any(|v| v["type"] == "error"));
    assert_eq!(seen.lock().unwrap().len(), 1);
    let other = conversation(&base).await;
    let events = message(&mut ws, &other, "fail").await;
    assert!(
        events
            .iter()
            .any(|v| v["error"].as_str().is_some_and(|s| s.contains("HTTP 429")))
    );
    assert_eq!(seen.lock().unwrap().len(), 2);
    app.shutdown.cancel();
    server.abort();
    provider_task.abort();
}

#[tokio::test]
async fn copied_credentials_protect_rest_ws_and_login_without_leaking_hashes() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    app.db
        .col_put(
            "users",
            "u1",
            &json!({"id":"u1","name":"Fixture","email":"test@example.com",
        "password_hash":"$2b$04$abcdefghijklmnopqrstuuxDugxrpcpW3csMl3JkYMLtX7yxJF.2."}),
            PutOptions::default(),
        )
        .unwrap();
    let (base, server) = serve(app.clone()).await;
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("{base}/api/users"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert!(
        tokio_tungstenite::connect_async(format!(
            "{}/ws?session=u1",
            base.replacen("http", "ws", 1)
        ))
        .await
        .is_err()
    );
    assert_eq!(
        client
            .post(format!("{base}/api/auth/login"))
            .json(&json!({"email":"test@example.com","password":"bad"}))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let login = client
        .post(format!("{base}/api/auth/login"))
        .json(&json!({"email":" TEST@example.com ","password":"fixture-password"}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(login["authToken"], app.token);
    let users = client
        .get(format!("{base}/api/users"))
        .bearer_auth(&app.token)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert!(!users.to_string().contains("password"));
    assert_eq!(
        client
            .get(format!("{base}/health"))
            .header("Origin", "https://example.com")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        client
            .get(format!("{base}/api/cron"))
            .bearer_auth(&app.token)
            .send()
            .await
            .unwrap()
            .status(),
        501
    );
    app.shutdown.cancel();
    server.abort();
}

#[test]
fn hive_crypto_and_model_ids_are_compatible() {
    let key = [7u8; 32];
    let nonce = [3u8; 12];
    let plaintext = "fixture-key";
    let cipher = ring::aead::LessSafeKey::new(
        ring::aead::UnboundKey::new(&ring::aead::AES_256_GCM, &key).unwrap(),
    );
    let mut bytes = plaintext.as_bytes().to_vec();
    let tag = cipher
        .seal_in_place_separate_tag(
            ring::aead::Nonce::assume_unique_for_key(nonce),
            ring::aead::Aad::empty(),
            &mut bytes,
        )
        .unwrap();
    let encrypted = format!("{}:{}", hex::encode(bytes), hex::encode(tag.as_ref()));
    assert_eq!(
        config::decrypt(&key, &encrypted, &hex::encode(nonce)).unwrap(),
        plaintext
    );
    // Generated with node:crypto using Hive's ciphertext:tag format, independently of ring.
    assert_eq!(
        config::decrypt(
            &key,
            "4397db772f5a3b6f11253a:00817a4b10c76182d25e8b7933ea36fb",
            &hex::encode(nonce)
        )
        .unwrap(),
        plaintext
    );
    assert!(config::decrypt(&[8u8; 32], &encrypted, &hex::encode(nonce)).is_err());
    assert_eq!(
        config::wire_model("nvidia", "nvidia/nvidia/model"),
        "nvidia/model"
    );
    assert_eq!(
        config::wire_model("anthropic", "claude-model"),
        "claude-model"
    );
}

#[test]
fn isolated_copy_refuses_a_live_database_and_preserves_source() {
    let source = tempfile::tempdir().unwrap();
    seed(source.path(), "http://127.0.0.1:9/v1");
    let destination_parent = tempfile::tempdir().unwrap();
    let target = destination_parent.path().join("copy");
    let before = std::fs::read(source.path().join("data/hivedb/collections.redb")).unwrap();
    let db = HiveDB::open(source.path().join("data/hivedb")).unwrap();
    assert!(config::copy_home(source.path(), &target).is_err());
    assert!(!target.exists());
    drop(db);
    // Snapshot after closing (redb can update its own headers when opened).
    let before_copy = std::fs::read(source.path().join("data/hivedb/collections.redb")).unwrap();
    config::copy_home(source.path(), &target).unwrap();
    assert_eq!(
        std::fs::read(source.path().join("data/hivedb/collections.redb")).unwrap(),
        before_copy
    );
    assert!(config::copy_home(source.path(), &target).is_err());
    let copied = App::open(target).unwrap();
    assert!(copied.selection().is_ok());
    assert!(!before.is_empty());
}

#[test]
fn missing_configuration_does_not_choose_another_provider() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = App::open(home.path().to_owned()).unwrap();
    let provider = app.db.col_get("providers", "openrouter").unwrap().unwrap();
    let mut doc = provider.doc;
    doc["active"] = json!(false);
    app.db
        .col_put("providers", "openrouter", &doc, PutOptions::default())
        .unwrap();
    assert!(
        app.selection()
            .err()
            .unwrap()
            .to_string()
            .contains("no está activo")
    );
}

#[test]
fn explicit_coordinator_model_wins_over_active_default_as_in_hive() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = App::open(home.path().to_owned()).unwrap();
    let mut row = app
        .db
        .col_get("models", "openrouter/vendor/model")
        .unwrap()
        .unwrap();
    row.doc["active"] = json!(false);
    app.db
        .col_put("models", &row.id, &row.doc, PutOptions::default())
        .unwrap();
    assert!(app.selection().is_ok());
    let mut agent = app.db.col_get("agents", "a1").unwrap().unwrap();
    agent.doc["provider_id"] = json!("__none__");
    agent.doc["model_id"] = json!("__none__");
    app.db
        .col_put("agents", &agent.id, &agent.doc, PutOptions::default())
        .unwrap();
    assert!(app.selection().is_err());
}

#[test]
fn native_endpoints_follow_hive_provider_defaults_and_preserve_custom_prefixes() {
    assert_eq!(
        config::provider_endpoint("gemini", None).unwrap(),
        "https://generativelanguage.googleapis.com/v1beta"
    );
    assert_eq!(
        config::provider_endpoint("anthropic", Some("https://api.anthropic.com")).unwrap(),
        "https://api.anthropic.com/v1"
    );
    assert_eq!(
        config::provider_endpoint("gemini", Some("https://fixture.test/proxy/v1beta/")).unwrap(),
        "https://fixture.test/proxy/v1beta"
    );
    assert_eq!(
        config::provider_endpoint("openrouter", Some("https://fixture.test/api/v1")).unwrap(),
        "https://fixture.test/api/v1"
    );
    assert!(config::provider_endpoint("openrouter", None).is_err());
}

#[tokio::test]
async fn http_parser_and_websocket_handshake_work_over_memory_transport() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    for (raw, expected) in [
        (
            "GET /health HTTP/1.1\r\nHost: fixture\r\n\r\n",
            "HTTP/1.1 200",
        ),
        (
            "GET /health HTTP/1.1\r\nHost: fixture\r\nOrigin: https://example.com\r\n\r\n",
            "HTTP/1.1 403",
        ),
        (
            "POST /api/conversations HTTP/1.1\r\nHost: fixture\r\nContent-Length: 2\r\n\r\n{}",
            "HTTP/1.1 201",
        ),
        (
            "POST /api/conversations HTTP/1.1\r\nHost: fixture\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n{}",
            "HTTP/1.1 400",
        ),
        (
            "POST /api/conversations HTTP/1.1\r\nHost: fixture\r\nContent-Length: 1048577\r\n\r\n",
            "HTTP/1.1 400",
        ),
    ] {
        let (mut client, server) = tokio::io::duplex(8192);
        let app = app.clone();
        let task = tokio::spawn(async move {
            connection(app, server).await.unwrap();
        });
        client.write_all(raw.as_bytes()).await.unwrap();
        let mut reply = String::new();
        client.read_to_string(&mut reply).await.unwrap();
        task.await.unwrap();
        assert!(reply.starts_with(expected), "{reply}");
    }
    let (client, server) = tokio::io::duplex(8192);
    let host = app.clone();
    let task = tokio::spawn(async move {
        connection(host, server).await.unwrap();
    });
    let (mut ws, _) = tokio_tungstenite::client_async("ws://fixture/ws?session=u1", client)
        .await
        .unwrap();
    assert_eq!(frame(&mut ws).await["type"], "welcome");
    ws.send(Message::Text(
        json!({"type":"canvas_subscribe"}).to_string(),
    ))
    .await
    .unwrap();
    ws.send(Message::Text(
        json!({"type":"message","threadId":"other/webchat/test","content":"hola"}).to_string(),
    ))
    .await
    .unwrap();
    assert_eq!(frame(&mut ws).await["type"], "canvas:snapshot");
    assert_eq!(frame(&mut ws).await["type"], "error");
    assert_eq!(frame(&mut ws).await["processStatus"], "error");
    ws.close(None).await.unwrap();
    task.await.unwrap();
    assert!(decode("%aé").is_err());
}

#[derive(Clone)]
struct DelegatingProvider {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    stall_child: bool,
}
impl TurnProvider for DelegatingProvider {
    type Stream = FixtureStream;
    type Child = Self;
    fn child(&self, _: NativeProvider) -> Self {
        self.clone()
    }
    fn start(&self, request: ModelRequest) -> Self::Stream {
        let child = request.tools.is_empty();
        let has_result = request.messages.iter().any(|m| m.role == "tool");
        self.requests.lock().unwrap().push(request);
        let reply = if !child && !has_result {
            json!({"content":null,"tool_calls":[{"id":"delegate-1","name":"task_delegate","arguments":{"worker_id":"w1","task_description":"Analiza esta consulta","mode":"sync"}}],"usage":{"input_tokens":10,"output_tokens":2}})
        } else {
            json!({"content":if child {"Informe del especialista"}else{"Respuesta con el informe"},"tool_calls":[],"usage":{"input_tokens":10,"output_tokens":2}})
        };
        let mut events = std::collections::VecDeque::new();
        if let Some(text) = reply["content"].as_str() {
            events.push_back(ModelEvent::TextDelta { text: text.into() });
        }
        events.push_back(ModelEvent::Reply {
            reply: serde_json::from_value(reply).unwrap(),
        });
        FixtureStream {
            events: Mutex::new(events),
            stall: child && self.stall_child,
        }
    }
}

fn seed_worker(app: &App) {
    app.db.col_put("agents","w1",&json!({"id":"w1","user_id":"","source":"catalog","name":"Analista","description":"Analiza consultas",
        "role":"worker","enabled":true,"status":"idle","provider_id":"__none__","model_id":"__none__","max_iterations":4}),PutOptions::default()).unwrap();
}

#[tokio::test]
async fn hivelive_bridge_acknowledges_once_and_does_not_redelegate_results() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio_tungstenite::tungstenite::protocol::Role;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let thread = seed_live(&app);
    let query = std::collections::HashMap::from([("threadId".into(), thread.clone())]);
    let runtime = crate::realtime::LiveSession::new(
        app.clone(),
        crate::realtime::LiveConfig::load(&app, "u1", &query).unwrap(),
    );
    let (native, browser) = tokio::io::duplex(65536);
    let (upstream, gemini) = tokio::io::duplex(65536);
    let mut browser = WebSocketStream::from_raw_socket(browser, Role::Client, None).await;
    let mut gemini = WebSocketStream::from_raw_socket(gemini, Role::Server, None).await;
    let executions = Arc::new(AtomicUsize::new(0));
    let count = executions.clone();
    let driver = tokio::spawn(async move {
        let mut runtime = runtime;
        let mut native = WebSocketStream::from_raw_socket(native, Role::Server, None).await;
        let mut upstream = WebSocketStream::from_raw_socket(upstream, Role::Client, None).await;
        runtime.setup(&mut upstream).await.unwrap();
        runtime.ready(&mut native).await.unwrap();
        runtime.relay(&mut native,&mut upstream,&CancellationToken::new(),&mut move|prompt,delivery| {
            assert_eq!(prompt,"Analiza esta consulta");count.fetch_add(1,Ordering::SeqCst);
            delivery.try_send(json!({"type":"live_result","ok":true,"content":"Informe de la colmena"})).unwrap();
            Ok("fixture-job".into())
        }).await
    });
    frame(&mut gemini).await;
    gemini
        .send(Message::text(json!({"setupComplete":{}}).to_string()))
        .await
        .unwrap();
    frame(&mut browser).await;
    frame(&mut browser).await;
    let call = json!({"toolCall":{"functionCalls":[{"id":"same","name":"consultar_a_bee","args":{"peticion":"Analiza esta consulta"}}]}});
    for _ in 0..2 {
        gemini.send(Message::text(call.to_string())).await.unwrap();
        let response = frame(&mut gemini).await;
        assert_eq!(
            response["toolResponse"]["functionResponses"][0]["response"]["status"],
            "accepted"
        );
    }
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    gemini
        .send(Message::text(
            json!({"serverContent":{"turnComplete":true}}).to_string(),
        ))
        .await
        .unwrap();
    let narration = tokio::time::timeout(Duration::from_secs(3), frame(&mut gemini))
        .await
        .unwrap();
    assert!(
        narration["realtimeInput"]["text"]
            .as_str()
            .unwrap()
            .contains("Informe de la colmena")
    );
    let mut duplicate = call.clone();
    duplicate["toolCall"]["functionCalls"][0]["id"] = json!("new-id");
    gemini
        .send(Message::text(duplicate.to_string()))
        .await
        .unwrap();
    assert_eq!(
        frame(&mut gemini).await["toolResponse"]["functionResponses"][0]["response"]["ok"],
        false
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "a spoken result is not a new user request"
    );
    let own = CancellationToken::new();
    let other = CancellationToken::new();
    {
        let mut active = app.active.lock().unwrap();
        active.insert(thread.clone(), own.clone());
        active.insert("other-thread".into(), other.clone());
    }
    gemini.send(Message::text(json!({"toolCall":{"functionCalls":[{"id":"cancel","name":"cancelar_tarea","args":{}}]}}).to_string())).await.unwrap();
    assert_eq!(
        frame(&mut gemini).await["toolResponse"]["functionResponses"][0]["response"]["status"],
        "cancellation_requested"
    );
    assert!(own.is_cancelled());
    assert!(!other.is_cancelled());
    let mut changed = call.clone();
    changed["toolCall"]["functionCalls"][0]["args"]["peticion"] = json!("different request");
    gemini
        .send(Message::text(changed.to_string()))
        .await
        .unwrap();
    assert!(
        driver
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("reutilizó")
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn hivelive_authentication_and_configuration_fail_before_ready() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let mut user = app.db.col_get("users", "u1").unwrap().unwrap().doc;
    user["email"] = json!("test@example.test");
    user["password_hash"] = json!("configured");
    app.db
        .col_put("users", "u1", &user, PutOptions::default())
        .unwrap();
    let (server, mut client) = tokio::io::duplex(65536);
    let host = app.clone();
    let task = tokio::spawn(async move {
        connection(host, server).await.unwrap();
    });
    client
        .write_all(b"GET /realtime?session=u1&token=bad HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    task.await.unwrap();
    assert!(
        String::from_utf8(response)
            .unwrap()
            .starts_with("HTTP/1.1 401")
    );
    let (server, mut client) = tokio::io::duplex(65536);
    let host = app.clone();
    let task = tokio::spawn(async move {
        connection(host, server).await.unwrap();
    });
    client.write_all(format!("GET /realtime?session=u1&token={} HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",app.token).as_bytes()).await.unwrap();
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        headers.push(client.read_u8().await.unwrap());
    }
    assert!(
        String::from_utf8(headers)
            .unwrap()
            .starts_with("HTTP/1.1 101")
    );
    let mut client = WebSocketStream::from_raw_socket(
        client,
        tokio_tungstenite::tungstenite::protocol::Role::Client,
        None,
    )
    .await;
    let error = frame(&mut client).await;
    assert_eq!(error["type"], "error");
    assert!(error["error"].as_str().unwrap().contains("Gemini"));
    assert_eq!(frame(&mut client).await["type"], "closed");
    task.await.unwrap();
    assert!(app.live_sessions.lock().unwrap().is_empty());
}

fn seed_live(app: &App) -> String {
    app.db
        .col_put(
            "providers",
            "gemini",
            &json!({"id":"gemini","enabled":true,"active":true,"base_url":null}),
            PutOptions::default(),
        )
        .unwrap();
    let secret = app
        .db
        .col_get("secrets", "provider:openrouter:api_key")
        .unwrap()
        .unwrap()
        .doc;
    app.db
        .col_put(
            "secrets",
            "provider:gemini:api_key",
            &secret,
            PutOptions::default(),
        )
        .unwrap();
    for id in ["gemini-2.5-flash-native-audio-latest", "gemini-3.8-live"] {
        app.db.col_put("models",id,&json!({"id":id,"provider_id":"gemini","model_type":"realtime","enabled":true,"active":false,
            "input_per_1m":2.0,"output_per_1m":4.0}),PutOptions::default()).unwrap();
    }
    route(
        app,
        Request::post("/api/conversations")
            .body(b"{}".to_vec())
            .unwrap(),
    )
    .unwrap()
    .1["conversation"]["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn hivelive_configuration_comes_from_hivedb_and_does_not_leak_credentials() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = App::open(home.path().to_owned()).unwrap();
    let thread = seed_live(&app);
    let query = std::collections::HashMap::from([
        ("threadId".into(), thread),
        ("voice".into(), "Puck".into()),
        ("lang".into(), "es-MX".into()),
    ]);
    let cfg = crate::realtime::LiveConfig::load(&app, "u1", &query).unwrap();
    assert_eq!(cfg.model, "gemini-3.8-live");
    assert_eq!(cfg.input_price, Some(2.0));
    assert_eq!(cfg.output_price, Some(4.0));
    let setup = cfg.setup(None);
    assert_eq!(
        setup["setup"]["generationConfig"]["speechConfig"]["voiceConfig"]["prebuiltVoiceConfig"]["voiceName"],
        "Puck"
    );
    assert!(
        setup["setup"]["generationConfig"]["speechConfig"]
            .get("languageCode")
            .is_none()
    );
    assert!(cfg.prompt.contains("es-MX"));
    assert!(!setup.to_string().contains(&cfg.key));
    assert!(!cfg.endpoint.contains(&cfg.key));
    assert_eq!(
        cfg.redact(&format!("provider error {}", cfg.key)),
        "provider error [redactado]"
    );
    let mut live = app
        .db
        .col_get("models", "gemini-2.5-flash-native-audio-latest")
        .unwrap()
        .unwrap()
        .doc;
    live["active"] = json!(true);
    app.db
        .col_put(
            "models",
            "gemini-2.5-flash-native-audio-latest",
            &live,
            PutOptions::default(),
        )
        .unwrap();
    assert_eq!(
        crate::realtime::LiveConfig::load(&app, "u1", &query)
            .unwrap()
            .model,
        "gemini-2.5-flash-native-audio-latest"
    );
    let mut invalid = query.clone();
    invalid.insert("threadId".into(), "not-owned".into());
    assert!(crate::realtime::LiveConfig::load(&app, "u1", &invalid).is_err());
    let mut provider = app.db.col_get("providers", "gemini").unwrap().unwrap().doc;
    provider["active"] = json!(false);
    app.db
        .col_put("providers", "gemini", &provider, PutOptions::default())
        .unwrap();
    assert!(crate::realtime::LiveConfig::load(&app, "u1", &query).is_err());
}

#[tokio::test]
async fn hivelive_memory_relay_waits_for_setup_and_persists_two_measured_voice_turns() {
    live_media_roundtrip(false).await;
}

#[tokio::test]
async fn hivelive_accepts_binary_json_during_setup_and_streaming() {
    live_media_roundtrip(true).await;
}

async fn live_media_roundtrip(binary: bool) {
    let wire = |text: String| {
        if binary {
            Message::Binary(text.into_bytes())
        } else {
            Message::text(text)
        }
    };
    use base64::Engine;
    use tokio_tungstenite::tungstenite::protocol::Role;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let thread = seed_live(&app);
    let query = std::collections::HashMap::from([("threadId".into(), thread.clone())]);
    let cfg = crate::realtime::LiveConfig::load(&app, "u1", &query).unwrap();
    let runtime = crate::realtime::LiveSession::new(app.clone(), cfg);
    let (native, browser) = tokio::io::duplex(65536);
    let (upstream, gemini) = tokio::io::duplex(65536);
    let mut browser = WebSocketStream::from_raw_socket(browser, Role::Client, None).await;
    let mut gemini = WebSocketStream::from_raw_socket(gemini, Role::Server, None).await;
    let driver = tokio::spawn(async move {
        let mut runtime = runtime;
        let mut native = WebSocketStream::from_raw_socket(native, Role::Server, None).await;
        let mut upstream = WebSocketStream::from_raw_socket(upstream, Role::Client, None).await;
        runtime.setup(&mut upstream).await.unwrap();
        runtime.ready(&mut native).await.unwrap();
        runtime
            .relay(
                &mut native,
                &mut upstream,
                &CancellationToken::new(),
                &mut |_, _| Err("unexpected work".into()),
            )
            .await
            .unwrap()
    });
    let setup: Value =
        serde_json::from_str(gemini.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
    assert_eq!(setup["setup"]["model"], "models/gemini-3.8-live");
    assert!(
        tokio::time::timeout(Duration::from_millis(20), browser.next())
            .await
            .is_err()
    );
    gemini
        .send(wire(json!({"setupComplete":{}}).to_string()))
        .await
        .unwrap();
    assert_eq!(frame(&mut browser).await["type"], "pricing");
    assert_eq!(frame(&mut browser).await["type"], "ready");
    browser
        .send(Message::Binary(vec![0, 0, 1, 0]))
        .await
        .unwrap();
    let audio = frame(&mut gemini).await;
    assert_eq!(
        audio["realtimeInput"]["audio"]["mimeType"],
        "audio/pcm;rate=16000"
    );
    let jpeg = base64::engine::general_purpose::STANDARD.encode([0xff, 0xd8, 0xff, 0xd9]);
    browser
        .send(Message::text(
            json!({"type":"video","data":jpeg,"mimeType":"image/jpeg","source":"screen"})
                .to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(
        frame(&mut gemini).await["realtimeInput"]["video"]["mimeType"],
        "image/jpeg"
    );
    let pcm = vec![0, 0, 2, 0];
    gemini.send(wire(json!({"serverContent":{"inputTranscription":{"text":"Hola"},"outputTranscription":{"text":"Primera respuesta"},
        "modelTurn":{"parts":[{"inlineData":{"data":base64::engine::general_purpose::STANDARD.encode(&pcm),"mimeType":"audio/pcm;rate=24000"}}]},"turnComplete":true},
        "usageMetadata":{"promptTokenCount":100,"toolUsePromptTokenCount":3,"responseTokenCount":5,"thoughtsTokenCount":2}}).to_string())).await.unwrap();
    let mut received_pcm = false;
    loop {
        match browser.next().await.unwrap().unwrap() {
            Message::Binary(data) => {
                assert_eq!(data, pcm);
                received_pcm = true;
            }
            Message::Text(text) => {
                let event: Value = serde_json::from_str(&text).unwrap();
                if event["type"] == "turn_complete" {
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(received_pcm);
    gemini.send(wire(json!({"serverContent":{"inputTranscription":{"text":"Otra"},"outputTranscription":{"text":"Segunda respuesta"},"turnComplete":true},
        "usageMetadata":{"promptTokenCount":5,"responseTokenCount":2}}).to_string())).await.unwrap();
    let mut total = None;
    loop {
        let event = frame(&mut browser).await;
        if event["type"] == "usage" {
            total = Some((
                event["inputTokens"].as_u64().unwrap(),
                event["outputTokens"].as_u64().unwrap(),
            ));
        }
        if event["type"] == "turn_complete" {
            break;
        }
    }
    assert_eq!(total, Some((108, 9)));
    let speaking = json!({"serverContent":{"modelTurn":{"parts":[{"inlineData":{"data":base64::engine::general_purpose::STANDARD.encode(&pcm),"mimeType":"audio/pcm;rate=24000"}}]}}});
    gemini.send(wire(speaking.to_string())).await.unwrap();
    assert!(matches!(
        browser.next().await.unwrap().unwrap(),
        Message::Binary(_)
    ));
    browser
        .send(Message::text(json!({"type":"interrupt"}).to_string()))
        .await
        .unwrap();
    assert_eq!(frame(&mut browser).await["type"], "interrupted");
    assert_eq!(
        frame(&mut gemini).await["clientContent"]["turnComplete"],
        false
    );
    gemini.send(wire(speaking.to_string())).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), browser.next())
            .await
            .is_err(),
        "late audio must not reach playback after manual interruption"
    );
    gemini
        .send(wire(
            json!({"serverContent":{"interrupted":true,"turnComplete":true}}).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(frame(&mut browser).await["type"], "interrupted");
    assert_eq!(frame(&mut browser).await["type"], "turn_complete");
    browser.close(None).await.unwrap();
    assert_eq!(driver.await.unwrap(), crate::realtime::End::Closed);
    let records = config::docs(&app.db, "conversations").unwrap();
    assert_eq!(records.len(), 4);
    assert!(
        records
            .iter()
            .all(|record| record["source"] == "realtime_chat")
    );
    assert!(
        crate::conversations::visible_messages(&app, &thread, 40)
            .unwrap()
            .is_empty()
    );
    assert_eq!(config::docs(&app.db, "usageRecords").unwrap().len(), 2);
    let summary = route(&app, Request::get("/api/usage-stats").body(vec![]).unwrap())
        .unwrap()
        .1;
    assert_eq!(summary["totalTokens"], 117);
    assert!(
        !crate::realtime::LiveConfig::load(&app, "u1", &query)
            .unwrap()
            .history
            .is_empty()
    );
}

#[tokio::test]
async fn webchat_lists_owned_threads_and_continues_legacy_text_history_in_isolation() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let create = |title: &str| {
        route(
            &app,
            Request::post("/api/conversations")
                .body(json!({"title":title}).to_string().into_bytes())
                .unwrap(),
        )
        .unwrap()
        .1["conversation"]["id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let first = create("Primera");
    let second = create("Segunda");
    for (id, owner, channel, archived, at) in [
        (first.as_str(), "u1", "webchat", false, 10),
        (second.as_str(), "u1", "webchat", false, 20),
        ("opaque-foreign", "u2", "webchat", false, 40),
        ("opaque-archived", "u1", "webchat", true, 50),
        ("opaque-other", "u1", "telegram", false, 60),
    ] {
        let mut row = app
            .db
            .col_get("conversationThreads", &first)
            .unwrap()
            .unwrap()
            .doc;
        row["id"] = json!(id);
        row["user_id"] = json!(owner);
        row["channel"] = json!(channel);
        row["archived"] = json!(archived);
        row["last_message_at"] = json!(at);
        app.db
            .col_put("conversationThreads", id, &row, PutOptions::default())
            .unwrap();
    }
    let list = route(
        &app,
        Request::get("/api/conversations?channel=webchat")
            .body(vec![])
            .unwrap(),
    )
    .unwrap()
    .1;
    assert_eq!(list["conversations"].as_array().unwrap().len(), 2);
    assert_eq!(list["conversations"][0]["id"], second);
    assert!(!crate::conversations::owned(&app, "opaque-foreign", "u1").unwrap());
    for id in ["opaque-foreign", "opaque-archived", "opaque-other"] {
        assert_eq!(
            route(
                &app,
                Request::get(format!("/api/chat/history?threadId={id}"))
                    .body(vec![])
                    .unwrap()
            )
            .unwrap()
            .0,
            404
        );
    }
    for (sequence, role, source, content) in [
        (1, "user", "message", "Pregunta anterior"),
        (2, "user", "delegation_summary", "internal"),
        (3, "assistant", "message", "Respuesta anterior"),
        (4, "tool", "message", "tool output"),
    ] {
        let id = format!("{first}:{sequence:015}");
        app.db.col_put("conversations",&id,&json!({"id":id,"thread_id":first,"role":role,"source":source,"content":content,"created_at":now()}),PutOptions::default()).unwrap();
    }
    app.db
        .col_put(
            "counters",
            &format!("conversations:{first}"),
            &json!({"value":4}),
            PutOptions::default(),
        )
        .unwrap();
    let query = format!("/api/chat/history?threadId={}", first.replace('/', "%2F"));
    let history = route(&app, Request::get(&query).body(vec![]).unwrap())
        .unwrap()
        .1;
    assert_eq!(history["messages"].as_array().unwrap().len(), 2);
    assert_eq!(history["messages"][0]["content"], "Pregunta anterior");
    assert_eq!(history["canContinue"], true);
    let selected = app.selection().unwrap();
    let handle = app.session(&first, &selected).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (output, _rx) = mpsc::channel(32);
    drive_turn(
        app.clone(),
        handle,
        FixtureProvider {
            requests: requests.clone(),
            stall: false,
        },
        &first,
        "Continúa".into(),
        output,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests[0].messages.len(), 4);
        assert_eq!(
            requests[0].messages[1].content.as_deref(),
            Some("Pregunta anterior")
        );
        assert!(
            !requests[0]
                .messages
                .iter()
                .any(|message| message.content.as_deref() == Some("internal"))
        );
    }
    let handle = app.session(&second, &selected).unwrap();
    let (output, _rx) = mpsc::channel(32);
    drive_turn(
        app.clone(),
        handle,
        FixtureProvider {
            requests: requests.clone(),
            stall: false,
        },
        &second,
        "Otro chat".into(),
        output,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(requests.lock().unwrap()[1].messages.len(), 2);
    let history = route(&app, Request::get(&query).body(vec![]).unwrap())
        .unwrap()
        .1;
    assert!(!history["narrations"].as_array().unwrap().is_empty());
    assert!(
        history["narrations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|frame| frame["threadId"] == first)
    );
}

#[test]
fn webchat_history_exposes_uncertain_checkpoints_without_resuming_them() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = App::open(home.path().to_owned()).unwrap();
    let created = route(
        &app,
        Request::post("/api/conversations")
            .body(b"{}".to_vec())
            .unwrap(),
    )
    .unwrap()
    .1;
    let id = created["conversation"]["id"].as_str().unwrap();
    let handle = app.session(id, &app.selection().unwrap()).unwrap();
    handle
        .dispatch(r#"{"op":"begin_run","prompt":"pending"}"#)
        .unwrap();
    let before = handle.dispatch(r#"{"op":"claim"}"#).unwrap();
    let query = format!("/api/chat/history?threadId={}", id.replace('/', "%2F"));
    let history = route(&app, Request::get(query).body(vec![]).unwrap())
        .unwrap()
        .1;
    assert_eq!(history["canContinue"], false);
    assert_eq!(handle.dispatch(r#"{"op":"frame"}"#).unwrap(), before);
}

#[test]
fn dashboard_auth_native_stats_and_bounded_queries() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = App::open(home.path().to_owned()).unwrap();
    let mut user = app.db.col_get("users", "u1").unwrap().unwrap().doc;
    user["email"] = json!("test@example.test");
    user["password_hash"] = json!("configured");
    app.db
        .col_put("users", "u1", &user, PutOptions::default())
        .unwrap();
    assert!(
        route(&app, Request::get("/health").body(vec![]).unwrap())
            .unwrap()
            .1["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("dashboard"))
    );
    for path in [
        "/api/system-stats",
        "/api/activity-stats",
        "/api/usage-stats",
        "/api/version",
        "/api/agents/proposals",
    ] {
        assert_eq!(
            route(&app, Request::get(path).body(vec![]).unwrap())
                .unwrap()
                .0,
            401
        );
    }
    let get = |path: &str| {
        route(
            &app,
            Request::get(path)
                .header("Authorization", format!("Bearer {}", app.token))
                .body(vec![])
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(get("/api/version").1["status"], "local");
    assert_eq!(get("/api/activity-stats?hours=100000").0, 400);
    assert_eq!(get("/api/usage-stats?hours=0").0, 400);
    let connection = app.dashboard.connect();
    let stats = get("/api/system-stats").1;
    assert_eq!(stats["runtime"], "rust");
    assert!(stats["memory"].get("heapUsed").is_none());
    assert_eq!(stats["connections"], 1);
    drop(connection);
    assert_eq!(get("/api/system-stats").1["connections"], 0);
    assert_eq!(
        get("/api/activity-stats?hours=12")
            .1
            .as_array()
            .unwrap()
            .len(),
        13
    );
}

#[test]
fn dashboard_usage_merges_hive_history_and_records_native_usage_exactly_once() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = App::open(home.path().to_owned()).unwrap();
    let now = 1_800_000_000_000;
    let old = crate::dashboard::hour_bucket(now - 3_600_000).unwrap();
    app.db
        .col_put(
            "usageRollups",
            &old,
            &json!({"inputTokens":7,"outputTokens":3,"costUsd":1.0,"toonSavedTokens":16,
        "byProvider":{"legacy":{"inputTokens":7,"outputTokens":3,"costUsd":1.0}}}),
            PutOptions::default(),
        )
        .unwrap();
    let mut model = app
        .db
        .col_get("models", "openrouter/vendor/model")
        .unwrap()
        .unwrap()
        .doc;
    model["input_per_1m"] = json!(2.0);
    model["output_per_1m"] = json!(4.0);
    app.db
        .col_put(
            "models",
            "openrouter/vendor/model",
            &model,
            PutOptions::default(),
        )
        .unwrap();
    let usage = hive_agent_core::Usage {
        input_tokens: 100,
        output_tokens: 50,
    };
    for _ in 0..2 {
        crate::dashboard::record_usage(
            &app,
            "same-execution",
            "openrouter",
            "openrouter/vendor/model",
            Some(&usage),
            now,
        )
        .unwrap();
    }
    let summary = crate::dashboard::route(&app, "/api/usage-stats", "hours=1", now)
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(summary["totalTokens"], 160);
    assert_eq!(summary["byProvider"]["openrouter"]["tokens"], 150);
    assert_eq!(summary["toonSavedTokens"], 16);
    assert!((summary["totalCostUsd"].as_f64().unwrap() - 1.0004).abs() < 1e-9);
    assert_eq!(config::docs(&app.db, "usageRecords").unwrap().len(), 1);
    crate::dashboard::record_usage(&app, "unknown", "other", "unpriced", None, now).unwrap();
    let summary = crate::dashboard::route(&app, "/api/usage-stats", "hours=1", now)
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(summary["costIncomplete"], true);
    assert_eq!(
        summary["totalTokens"], 160,
        "unknown usage is not fabricated"
    );
    let empty = crate::dashboard::route(&app, "/api/usage-stats", "hours=1", now + 4 * 3_600_000)
        .unwrap()
        .unwrap()
        .1;
    assert_eq!(empty["totalTokens"], 0);
}

#[tokio::test]
async fn dashboard_message_rollups_are_atomic_and_proposals_are_read_only() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    seed_worker(&app);
    let (_, created) = route(
        &app,
        Request::post("/api/conversations")
            .body(b"{}".to_vec())
            .unwrap(),
    )
    .unwrap();
    let thread = created["conversation"]["id"].as_str().unwrap().to_owned();
    for _ in 0..2 {
        record(
            app.clone(),
            thread.clone(),
            "same-message".into(),
            "user",
            "Hola".into(),
        )
        .await
        .unwrap();
    }
    let activity = route(
        &app,
        Request::get("/api/activity-stats?hours=1")
            .body(vec![])
            .unwrap(),
    )
    .unwrap()
    .1;
    assert_eq!(
        activity
            .as_array()
            .unwrap()
            .iter()
            .map(|point| point["count"].as_u64().unwrap())
            .sum::<u64>(),
        1
    );
    assert_eq!(
        route(
            &app,
            Request::get("/api/system-stats").body(vec![]).unwrap()
        )
        .unwrap()
        .1["recentMessages"],
        1
    );
    app.db.col_put("agentProposals","p1",&json!({"id":"p1","agent_id":"w1","status":"proposed","type":"disable_agent","proposed_change_json":"{\"reason\":\"review\"}","created_at":now()}),PutOptions::default()).unwrap();
    let proposals = route(
        &app,
        Request::get("/api/agents/proposals").body(vec![]).unwrap(),
    )
    .unwrap()
    .1;
    assert_eq!(proposals["proposals"][0]["agentName"], "Analista");
    assert_eq!(proposals["proposals"][0]["reason"], "review");
    assert_eq!(
        app.db.col_get("agents", "w1").unwrap().unwrap().doc["enabled"],
        true
    );
}

#[test]
fn dashboard_backfills_only_older_native_message_links_once_across_restarts() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = App::open(home.path().to_owned()).unwrap();
    let at = now();
    let bucket = crate::dashboard::hour_bucket(at).unwrap();
    app.db
        .col_put(
            "activityRollups",
            &bucket,
            &json!({"messageCount":7}),
            PutOptions::default(),
        )
        .unwrap();
    app.db
        .col_put(
            "conversations",
            "old-native",
            &json!({"created_at":at,"content":"existing"}),
            PutOptions::default(),
        )
        .unwrap();
    app.db
        .col_put(
            "hive_sdk_rust_message_links_v1",
            "native-link",
            &json!({"message_id":"old-native"}),
            PutOptions::default(),
        )
        .unwrap();
    for _ in 0..2 {
        crate::dashboard::route(&app, "/api/activity-stats", "hours=1", at).unwrap();
    }
    assert_eq!(
        app.db
            .col_get("activityRollups", &bucket)
            .unwrap()
            .unwrap()
            .doc["messageCount"],
        8
    );
    drop(app);
    let app = App::open(home.path().to_owned()).unwrap();
    crate::dashboard::route(&app, "/api/activity-stats", "hours=1", at).unwrap();
    assert_eq!(
        app.db
            .col_get("activityRollups", &bucket)
            .unwrap()
            .unwrap()
            .doc["messageCount"],
        8
    );
}

#[derive(Clone)]
struct BatchDelegatingProvider {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    workers: usize,
}
impl TurnProvider for BatchDelegatingProvider {
    type Stream = FixtureStream;
    type Child = Self;
    fn child(&self, _: NativeProvider) -> Self {
        self.clone()
    }
    fn start(&self, request: ModelRequest) -> Self::Stream {
        let coordinator = !request.tools.is_empty();
        let has_results = request
            .messages
            .iter()
            .any(|message| message.role == "tool");
        self.requests.lock().unwrap().push(request);
        let reply = if coordinator && !has_results {
            json!({"content":null,"tool_calls":(1..=self.workers).map(|index| json!({
                "id":format!("call-{index}"),"name":"task_delegate",
                "arguments":{"worker_id":format!("w{index}"),"task_description":format!("Analiza la consulta {index}")}
            })).collect::<Vec<_>>(),"usage":{"input_tokens":10,"output_tokens":2}})
        } else {
            json!({"content":if coordinator {"Respuesta final con todos los informes"} else {"Informe del especialista"},
                "tool_calls":[],"usage":{"input_tokens":10,"output_tokens":2}})
        };
        FixtureStream {
            stall: false,
            events: Mutex::new(
                [ModelEvent::Reply {
                    reply: serde_json::from_value(reply).unwrap(),
                }]
                .into(),
            ),
        }
    }
}

#[tokio::test]
async fn a_configured_iteration_can_consult_the_entire_roster_then_synthesize() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let mut coordinator = app.db.col_get("agents", "a1").unwrap().unwrap().doc;
    coordinator["max_iterations"] = json!(2);
    app.db
        .col_put("agents", "a1", &coordinator, PutOptions::default())
        .unwrap();
    seed_worker(&app);
    let template = app.db.col_get("agents", "w1").unwrap().unwrap().doc;
    for index in 2..=17 {
        let mut worker = template.clone();
        let id = format!("w{index}");
        worker["id"] = json!(id);
        worker["name"] = json!(format!("Especialista {index}"));
        app.db
            .col_put("agents", &id, &worker, PutOptions::default())
            .unwrap();
    }
    let (_, created) = route(
        &app,
        Request::post("/api/conversations")
            .body(b"{}".to_vec())
            .unwrap(),
    )
    .unwrap();
    let thread = created["conversation"]["id"].as_str().unwrap();
    let selection = app.selection().unwrap();
    let handle = app.session(thread, &selection).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (output, mut receiver) = mpsc::channel(1024);
    drive_turn(
        app.clone(),
        handle,
        BatchDelegatingProvider {
            requests: requests.clone(),
            workers: 17,
        },
        thread,
        "Consulta a todos los especialistas".into(),
        output,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    {
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            19,
            "two coordinator iterations plus seventeen specialists"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.tools.is_empty())
                .count(),
            17
        );
    }
    let mut completed = 0;
    while let Some(event) = receiver.recv().await {
        if event["turnDone"] == true && event["processStatus"] == "done" {
            completed += 1;
        }
    }
    assert_eq!(completed, 1);
}

#[tokio::test]
async fn delegation_narrates_real_child_execution_and_office_states_without_network() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    seed_worker(&app);
    let mut model = app
        .db
        .col_get("models", "openrouter/vendor/model")
        .unwrap()
        .unwrap()
        .doc;
    model["context_window"] = json!(50000);
    app.db
        .col_put(
            "models",
            "openrouter/vendor/model",
            &model,
            PutOptions::default(),
        )
        .unwrap();
    model["id"] = json!("worker-model");
    model["context_window"] = json!(100000);
    app.db
        .col_put("models", "worker-model", &model, PutOptions::default())
        .unwrap();
    let mut worker = app.db.col_get("agents", "w1").unwrap().unwrap().doc;
    worker["provider_id"] = json!("openrouter");
    worker["model_id"] = json!("worker-model");
    app.db
        .col_put("agents", "w1", &worker, PutOptions::default())
        .unwrap();
    let mut office = app.office_events.subscribe();
    let (_, created) = route(
        &app,
        Request::post("/api/conversations")
            .body(b"{}".to_vec())
            .unwrap(),
    )
    .unwrap();
    let thread = created["conversation"]["id"].as_str().unwrap().to_owned();
    let selected = app.selection().unwrap();
    let handle = app.session(&thread, &selected).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (output, mut rx) = mpsc::channel(32);
    drive_turn(
        app.clone(),
        handle,
        DelegatingProvider {
            requests: requests.clone(),
            stall_child: false,
        },
        &thread,
        "Consulta al analista".into(),
        output,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests
                .iter()
                .map(|request| request.max_output_tokens)
                .collect::<Vec<_>>(),
            vec![Some(7500), Some(15000), Some(7500)],
            "each model gets the output reserve derived from its HiveDB context without a hidden 2048/8192 clamp"
        );
        assert!(
            requests
                .iter()
                .all(|request| request.deadline_unix_ms.is_none()),
            "each model call must receive its own timeout rather than the remaining turn deadline: {:?}",
            requests
                .iter()
                .map(|request| request.deadline_unix_ms)
                .collect::<Vec<_>>()
        );
        assert!(requests[0].tools.iter().any(|t| t.name == "task_delegate"));
        assert!(requests[1].tools.is_empty());
        assert!(requests[2].messages.iter().any(|m| {
            m.role == "tool"
                && m.content
                    .as_deref()
                    .unwrap_or_default()
                    .contains("Informe del especialista")
        }));
    }
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    assert_eq!(events.iter().filter(|e| e["turnDone"] == true).count(), 1);
    assert!(
        events.iter().any(|e| e["agentId"] == "w1"
            && e["processStatus"] == "done"
            && e["turnDone"] == false)
    );
    assert!(
        !events
            .iter()
            .any(|e| e["isChunk"] == true && e["content"] == "Informe del especialista")
    );
    let mut frames = Vec::new();
    while let Ok(event) = office.try_recv() {
        frames.push(event);
    }
    assert!(frames.iter().any(|e| {
        e["type"] == "canvas:snapshot"
            && e["data"]["edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["source"] == "a1" && edge["target"] == "w1")
    }));
    assert!(
        frames
            .iter()
            .any(|e| e["data"]["nodes"].as_array().is_some_and(|nodes| nodes
                .iter()
                .any(|node| node["id"] == "w1" && node["data"]["status"] == "thinking")))
    );
    let snapshot = activity::snapshot(&app.db, "u1").unwrap();
    assert!(snapshot["data"]["edges"].as_array().unwrap().is_empty());
    assert!(
        snapshot["data"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["data"]["status"] == "idle")
    );
    let narration = config::docs(&app.db, "narrationEvents").unwrap();
    assert!(
        narration
            .iter()
            .any(|e| e["agent_id"] == "w1" && e["kind"] == "worker_started")
    );
    assert_eq!(config::docs(&app.db, "conversations").unwrap().len(), 2);
    assert_eq!(config::docs(&app.db, "usageRecords").unwrap().len(), 3);
    let usage = route(&app, Request::get("/api/usage-stats").body(vec![]).unwrap())
        .unwrap()
        .1;
    assert_eq!(usage["totalTokens"], 36);
    assert_eq!(usage["byModel"]["worker-model"]["tokens"], 12);
    assert_eq!(usage["byModel"]["openrouter/vendor/model"]["tokens"], 24);
    let history = route(
        &app,
        Request::get(format!(
            "/api/chat/history?threadId={}",
            thread.replace('/', "%2F")
        ))
        .body(vec![])
        .unwrap(),
    )
    .unwrap()
    .1;
    let mut steps = std::collections::BTreeMap::new();
    let mut turns = std::collections::BTreeSet::new();
    for frame in history["narrations"].as_array().unwrap() {
        steps.insert(
            frame["stepId"].as_str().unwrap(),
            frame["processStatus"].as_str().unwrap(),
        );
        turns.insert(frame["messageId"].as_str().unwrap());
    }
    assert_eq!(
        steps.len(),
        3,
        "root activity, delegation and specialist activity"
    );
    assert!(steps.values().all(|status| *status == "done"));
    assert_eq!(turns.len(), 1);
}

#[tokio::test]
async fn stopping_a_delegation_cancels_child_and_parent_and_clears_office_edges() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    seed_worker(&app);
    let (_, created) = route(
        &app,
        Request::post("/api/conversations")
            .body(b"{}".to_vec())
            .unwrap(),
    )
    .unwrap();
    let thread = created["conversation"]["id"].as_str().unwrap().to_owned();
    let selected = app.selection().unwrap();
    let handle = app.session(&thread, &selected).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observed = requests.clone();
    let token = CancellationToken::new();
    let stop = token.clone();
    tokio::spawn(async move {
        loop {
            if observed.lock().unwrap().len() == 2 {
                stop.cancel();
                break;
            }
            tokio::task::yield_now().await;
        }
    });
    let (output, _rx) = mpsc::channel(32);
    let result = drive_turn(
        app.clone(),
        handle.clone(),
        DelegatingProvider {
            requests: requests.clone(),
            stall_child: true,
        },
        &thread,
        "Consulta".into(),
        output,
        token,
    )
    .await;
    assert!(result.unwrap_err().to_string().contains("cancelado"));
    assert_eq!(requests.lock().unwrap().len(), 2);
    let frame = dispatch(&handle, json!({"op":"frame"})).await.unwrap();
    let source = app.store.lock().unwrap().as_ref().unwrap().clone();
    let root_id = frame["objective_id"].as_str().unwrap();
    let root = source.inspect_objective(root_id).unwrap();
    assert_eq!(root["stopped"], "cancelled");
    assert_eq!(root["model_calls"], 2);
    assert_eq!(root["tool_calls"], 2);
    assert_eq!(root["reservations"].as_object().unwrap().len(), 1);
    let queue = source
        .queue(&format!("gateway.specialists.{root_id}"))
        .unwrap();
    let jobs = queue.list(50, None).unwrap();
    assert_eq!(jobs["items"][0]["status"], "uncertain");
    let group_id = jobs["items"][0]["input"]["payload"]["delegation"]["group_id"]
        .as_str()
        .unwrap();
    assert_eq!(queue.group(group_id).unwrap()["cancelled"], true);

    assert!(
        dispatch(&handle, json!({"op":"begin_run","prompt":"otra"}))
            .await
            .is_err()
    );
    let snapshot = activity::snapshot(&app.db, "u1").unwrap();
    assert!(snapshot["data"]["edges"].as_array().unwrap().is_empty());
    assert!(
        snapshot["data"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|n| n["data"]["status"] == "error")
    );
}

#[tokio::test]
async fn office_aggregates_concurrent_runs_and_reconciles_interrupted_activity() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let first = Progress {
        app: app.clone(),
        run: "first".into(),
        root: "first".into(),
        thread: "u1/webchat/first".into(),
        user: "u1".into(),
        agent: "a1".into(),
        name: "Hive".into(),
        task: "Primera".into(),
        parent: None,
    };
    let mut second = first.clone();
    second.run = "second".into();
    second.task = "Segunda".into();
    first
        .emit(
            "start",
            "worker_started",
            "thinking",
            "running",
            "Revisando",
            None,
        )
        .await
        .unwrap();
    second
        .emit(
            "start",
            "worker_started",
            "thinking",
            "running",
            "Revisando",
            None,
        )
        .await
        .unwrap();
    first
        .emit(
            "end",
            "group_ready",
            "completed",
            "done",
            "Completado",
            None,
        )
        .await
        .unwrap();
    let live = activity::snapshot(&app.db, "u1").unwrap();
    assert_eq!(live["data"]["nodes"][0]["data"]["status"], "thinking");
    assert_eq!(live["data"]["nodes"][0]["data"]["currentTask"], "Segunda");
    drop(first);
    drop(second);
    drop(app);
    let app = App::open(home.path().to_owned()).unwrap();
    let stopped = activity::snapshot(&app.db, "u1").unwrap();
    assert_eq!(stopped["data"]["nodes"][0]["data"]["status"], "error");
    assert_eq!(
        config::docs(&app.db, "narrationEvents").unwrap().len(),
        3,
        "reopening doesn't replay model work or fabricate extra steps"
    );
}

#[test]
fn office_includes_catalog_workers_respects_disabled_delegation_and_isolates_named_owners() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = App::open(home.path().to_owned()).unwrap();
    seed_worker(&app);
    let mut foreign = app.db.col_get("agents", "w1").unwrap().unwrap().doc;
    foreign["id"] = json!("foreign");
    foreign["user_id"] = json!("u2");
    app.db
        .col_put("agents", "foreign", &foreign, PutOptions::default())
        .unwrap();
    let (_, wire) = route(&app, Request::get("/api/agents").body(vec![]).unwrap()).unwrap();
    assert_eq!(wire["agents"].as_array().unwrap().len(), 2);
    assert!(app.selection_for_agent("w1").is_ok());
    assert!(app.selection_for_agent("foreign").is_err());
    let config: Value = serde_json::from_str(&app.selection().unwrap().engine_config).unwrap();
    assert_eq!(config["tools"][0]["name"], "task_delegate");
    app.db
        .col_put(
            "tools",
            "task_delegate",
            &json!({"enabled":true,"active":false}),
            PutOptions::default(),
        )
        .unwrap();
    let config: Value = serde_json::from_str(&app.selection().unwrap().engine_config).unwrap();
    assert!(
        !config["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "task_delegate")
    );
}

#[derive(Clone)]
struct FailingChildProvider {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}
struct FailingChildStream {
    normal: Option<FixtureStream>,
}
impl TurnStream for FailingChildStream {
    async fn next(&self) -> Result<Option<ModelEvent>> {
        match &self.normal {
            Some(stream) => stream.next().await,
            None => Err("PROVIDER_ERROR: HiveAgents inference returned HTTP 503".into()),
        }
    }
    fn cancel(&self) {}
}
impl TurnProvider for FailingChildProvider {
    type Stream = FailingChildStream;
    type Child = Self;
    fn child(&self, _: NativeProvider) -> Self {
        self.clone()
    }
    fn start(&self, request: ModelRequest) -> Self::Stream {
        let normal = if request.tools.is_empty() {
            self.requests.lock().unwrap().push(request);
            None
        } else {
            Some(
                DelegatingProvider {
                    requests: self.requests.clone(),
                    stall_child: false,
                }
                .start(request),
            )
        };
        FailingChildStream { normal }
    }
}

#[tokio::test]
async fn failed_specialist_keeps_error_identity_and_does_not_repeat_inference() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    seed_worker(&app);
    let (_, created) = route(
        &app,
        Request::post("/api/conversations")
            .body(b"{}".to_vec())
            .unwrap(),
    )
    .unwrap();
    let thread = created["conversation"]["id"].as_str().unwrap().to_owned();
    let selected = app.selection().unwrap();
    let handle = app.session(&thread, &selected).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (output, _rx) = mpsc::channel(32);
    let error = drive_turn(
        app.clone(),
        handle.clone(),
        FailingChildProvider {
            requests: requests.clone(),
        },
        &thread,
        "Consulta al analista".into(),
        output,
        CancellationToken::new(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("Analista (w1)") && error.contains("HiveAgents") && error.contains("503"),
        "{error}"
    );
    assert!(error.contains("max_output_tokens=4096"), "{error}");
    assert_eq!(
        requests.lock().unwrap().len(),
        2,
        "one parent call and one child attempt, without retries"
    );
    assert!(
        dispatch(&handle, json!({"op":"begin_run","prompt":"retry"}))
            .await
            .is_err()
    );
    let events = config::docs(&app.db, "narrationEvents").unwrap();
    assert!(events.iter().any(|e| {
        e["agent_id"] == "a1"
            && e["status"] == "error"
            && e["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("Analista (w1)")
    }));
    assert_eq!(
        config::docs(&app.db, "conversations").unwrap().len(),
        1,
        "no fabricated assistant answer"
    );
}

#[tokio::test]
async fn hivelive_resumption_consumes_handle_and_does_not_replay_history() {
    use tokio_tungstenite::tungstenite::protocol::Role;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let thread = seed_live(&app);
    let query = std::collections::HashMap::from([("threadId".into(), thread)]);
    let mut cfg = crate::realtime::LiveConfig::load(&app, "u1", &query).unwrap();
    cfg.history = vec![json!({"role":"user","parts":[{"text":"Do not replay"}]})];
    let mut runtime = crate::realtime::LiveSession::new(app, cfg);
    runtime.resume = Some("confirmed-handle".into());
    let (upstream, gemini) = tokio::io::duplex(65536);
    let mut upstream = WebSocketStream::from_raw_socket(upstream, Role::Client, None).await;
    let mut gemini = WebSocketStream::from_raw_socket(gemini, Role::Server, None).await;
    let driver = tokio::spawn(async move {
        runtime.setup(&mut upstream).await.unwrap();
        assert!(runtime.resume.is_none());
        (runtime, upstream)
    });
    assert_eq!(
        frame(&mut gemini).await["setup"]["sessionResumption"]["handle"],
        "confirmed-handle"
    );
    gemini
        .send(Message::text(json!({"setupComplete":{}}).to_string()))
        .await
        .unwrap();
    let (_runtime, _upstream) = driver.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), gemini.next())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn hivelive_setup_preserves_provider_close_reason_without_secrets() {
    use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Role, frame::coding::CloseCode};
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let thread = seed_live(&app);
    let query = std::collections::HashMap::from([("threadId".into(), thread)]);
    let cfg = crate::realtime::LiveConfig::load(&app, "u1", &query).unwrap();
    let key = cfg.key.clone();
    let mut runtime = crate::realtime::LiveSession::new(app, cfg);
    let (upstream, gemini) = tokio::io::duplex(65536);
    let mut upstream = WebSocketStream::from_raw_socket(upstream, Role::Client, None).await;
    let mut gemini = WebSocketStream::from_raw_socket(gemini, Role::Server, None).await;
    let driver =
        tokio::spawn(async move { runtime.setup(&mut upstream).await.unwrap_err().to_string() });
    let _setup = frame(&mut gemini).await;
    gemini
        .send(Message::Close(Some(CloseFrame {
            code: CloseCode::Policy,
            reason: format!("Unsupported languageCode {key}").into(),
        })))
        .await
        .unwrap();
    let error = driver.await.unwrap();
    assert!(error.contains("1008"), "{error}");
    assert!(error.contains("Unsupported languageCode"), "{error}");
    assert!(!error.contains(&key));
}

#[test]
fn hivelive_model_picker_persists_only_valid_realtime_selection() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = App::open(home.path().to_owned()).unwrap();
    let thread = seed_live(&app);
    let get = || Request::get("/api/realtime/models").body(vec![]).unwrap();
    let (status, catalog) = route(&app, get()).unwrap();
    assert_eq!(status, 200);
    assert_eq!(catalog["selectedId"], "gemini-3.8-live");
    assert_eq!(catalog["models"].as_array().unwrap().len(), 2);
    assert!(!catalog.to_string().contains("api_key"));
    let select = |id: &str| {
        Request::post("/api/realtime/models")
            .body(json!({"modelId":id}).to_string().into_bytes())
            .unwrap()
    };
    for invalid in ["missing", "openrouter/vendor/model"] {
        assert_eq!(route(&app, select(invalid)).unwrap().0, 400);
    }
    let next = "gemini-2.5-flash-native-audio-latest";
    assert_eq!(route(&app, select(next)).unwrap().1["selectedId"], next);
    let query = std::collections::HashMap::from([("threadId".into(), thread)]);
    assert_eq!(
        crate::realtime::LiveConfig::load(&app, "u1", &query)
            .unwrap()
            .model,
        next
    );
    assert_eq!(
        app.db
            .col_get("models", "openrouter/vendor/model")
            .unwrap()
            .unwrap()
            .doc["active"],
        true
    );
    assert_eq!(
        app.db
            .col_get("models", "gemini-3.8-live")
            .unwrap()
            .unwrap()
            .doc["active"],
        false
    );
    assert_eq!(route(&app, select("gemini-3.8-live")).unwrap().0, 200);
    let mut disabled = app.db.col_get("models", next).unwrap().unwrap().doc;
    disabled["enabled"] = json!(false);
    app.db
        .col_put("models", next, &disabled, PutOptions::default())
        .unwrap();
    assert_eq!(route(&app, select(next)).unwrap().0, 400);
    drop(app);
    let reopened = App::open(home.path().to_owned()).unwrap();
    assert_eq!(
        route(&reopened, get()).unwrap().1["selectedId"],
        "gemini-3.8-live"
    );
    assert_eq!(
        crate::realtime::LiveConfig::load(&reopened, "u1", &query)
            .unwrap()
            .model,
        "gemini-3.8-live"
    );
}

#[test]
fn desktop_migration_sets_gemini_38_in_the_database_once() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    {
        let db = HiveDB::open(home.path().join("data/hivedb")).unwrap();
        db.col_put("models","old-live",&json!({"id":"old-live","provider_id":"gemini","model_type":"realtime","active":true,"enabled":true}),Default::default()).unwrap();
    }
    let app = App::open(home.path().to_owned()).unwrap();
    let model = app
        .db
        .col_get("models", "gemini-3.8-live")
        .unwrap()
        .unwrap()
        .doc;
    assert_eq!(model["active"], true);
    assert_eq!(model["enabled"], true);
    assert_eq!(
        app.db.col_get("models", "old-live").unwrap().unwrap().doc["active"],
        false
    );
    let selected = route(
        &app,
        Request::get("/api/realtime/models").body(vec![]).unwrap(),
    )
    .unwrap()
    .1;
    assert_eq!(selected["selectedId"], "gemini-3.8-live");
    assert_eq!(
        route(
            &app,
            Request::post("/api/realtime/models")
                .body(json!({"modelId":"old-live"}).to_string().into_bytes())
                .unwrap()
        )
        .unwrap()
        .0,
        200
    );
    drop(app);
    let app = App::open(home.path().to_owned()).unwrap();
    assert_eq!(
        app.db.col_get("models", "old-live").unwrap().unwrap().doc["active"],
        true,
        "later explicit choices survive restart"
    );
}

#[test]
fn desktop_views_read_and_write_canonical_settings_without_exposing_secrets() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = App::open(home.path().to_owned()).unwrap();
    for (col, id, doc) in [
        (
            "tools",
            "t",
            json!({"id":"t","name":"Buscar","enabled":true,"active":true}),
        ),
        (
            "skills",
            "s",
            json!({"id":"s","name":"Resumen","tools":["t"],"triggers":[]}),
        ),
        (
            "ethics",
            "e",
            json!({"id":"e","name":"Regla","content":"Texto","active":true}),
        ),
        (
            "mcpServers",
            "m",
            json!({"id":"m","name":"MCP","enabled":true,"transport":"http","url":"https://host.test/mcp?token=private"}),
        ),
    ] {
        app.db.col_put(col, id, &doc, Default::default()).unwrap();
    }
    for path in [
        "/api/providers",
        "/api/agents",
        "/api/channels",
        "/api/tools",
        "/api/skills",
        "/api/mcp/servers",
        "/api/tts-local/status",
        "/api/ethics",
        "/api/users",
        "/api/notes",
        "/api/cron",
        "/api/meetings",
        "/api/a2ui/surfaces",
    ] {
        let (status, value) = route(&app, Request::get(path).body(vec![]).unwrap()).unwrap();
        assert_eq!(status, 200, "{path}: {value}");
        assert!(!value.to_string().contains("ciphertext"));
        assert!(!value.to_string().contains("token=private"));
        assert!(!value.to_string().contains("password_hash"));
    }
    let edit = |method: &str, path: &str, body: Value| {
        route(
            &app,
            Request::builder()
                .method(method)
                .uri(path)
                .body(body.to_string().into_bytes())
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(
        edit(
            "PUT",
            "/api/providers/openrouter",
            json!({"apiKey":"replacement-private-key"})
        )
        .0,
        200
    );
    assert_eq!(
        config::secret(&app.db, &app.home, "provider:openrouter:api_key")
            .unwrap()
            .as_deref(),
        Some("replacement-private-key")
    );
    let wire = route(&app, Request::get("/api/providers").body(vec![]).unwrap())
        .unwrap()
        .1;
    assert!(!wire.to_string().contains("replacement-private-key"));
    assert_eq!(wire["providers"][0]["has_api_key"], true);
    let (_, created) = edit(
        "POST",
        "/api/agents",
        json!({"name":"Especialista","providerId":"openrouter","modelId":"openrouter/vendor/model"}),
    );
    let id = created["agentId"].as_str().unwrap();
    assert_eq!(
        app.db.col_get("agents", id).unwrap().unwrap().doc["role"],
        "worker"
    );
    assert!(app.selection_for_agent(id).is_ok());
    assert_eq!(
        edit(
            "PUT",
            &format!("/api/agents/{id}"),
            json!({"systemPrompt":"Comprueba","maxIterations":12})
        )
        .0,
        200
    );
    assert_eq!(
        app.db.col_get("agents", id).unwrap().unwrap().doc["system_prompt"],
        "Comprueba"
    );
    assert_eq!(
        edit(
            "PATCH",
            "/api/user/settings?userId=u1",
            json!({"name":"Ana","timezone":"America/Bogota"})
        )
        .0,
        200
    );
    assert_eq!(
        app.db.col_get("users", "u1").unwrap().unwrap().doc["name"],
        "Ana"
    );
    assert_eq!(
        edit("POST", "/api/tools/t/toggle", json!({"active":false})).0,
        200
    );
    assert_eq!(
        app.db.col_get("tools", "t").unwrap().unwrap().doc["active"],
        false
    );
    let (_, channel) = edit(
        "POST",
        "/api/channels",
        json!({"type":"telegram","config":{"botToken":"private-bot-token"}}),
    );
    let channel = channel["id"].as_str().unwrap();
    assert!(
        !app.db
            .col_get("channels", channel)
            .unwrap()
            .unwrap()
            .doc
            .to_string()
            .contains("private-bot-token")
    );
    assert!(
        config::secret(&app.db, &app.home, &format!("channel:{channel}:config"))
            .unwrap()
            .unwrap()
            .contains("private-bot-token")
    );
}

#[test]
fn desktop_panel_surfaces_are_persisted_scoped_idempotent_and_actionable() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = App::open(home.path().to_owned()).unwrap();
    let agent = json!({"id":"a1","name":"Hive"});
    let create = json!({"surfaceId":"form","theme":{}});
    crate::panel::execute(
        &app,
        "u1",
        "thread",
        &agent,
        "a2ui_create_surface",
        &create,
        "op1",
    )
    .unwrap();
    let components = json!({"surfaceId":"form","components":[{"id":"root","component":{"Column":{"children":{"explicitList":["text"]}}}},{"id":"text","component":{"Text":{"text":{"literalString":"Hola"}}}}]});
    crate::panel::execute(
        &app,
        "u1",
        "thread",
        &agent,
        "a2ui_update_components",
        &components,
        "op2",
    )
    .unwrap();
    crate::panel::execute(
        &app,
        "u1",
        "thread",
        &agent,
        "a2ui_create_surface",
        &create,
        "op1",
    )
    .unwrap();
    let row = app
        .db
        .col_get("hive_rust_a2ui", "u1:form")
        .unwrap()
        .unwrap()
        .doc;
    assert_eq!(row["components"].as_array().unwrap().len(), 2);
    let frames = crate::panel::frames(&row);
    assert_eq!(frames[0]["type"], "a2ui:createSurface");
    assert_eq!(frames[1]["type"], "a2ui:updateComponents");
    let action = json!({"surfaceId":"form","sourceComponentId":"text","name":"save","context":{"name":"Ana"}});
    let (thread, prompt) = crate::panel::action(&app, "u1", &action).unwrap();
    assert_eq!(thread, "thread");
    assert!(prompt.contains("Ana"));
    assert!(crate::panel::action(&app, "u2", &action).is_err());
    assert!(
        crate::panel::execute(
            &app,
            "u2",
            "thread",
            &agent,
            "a2ui_delete_surface",
            &create,
            "foreign"
        )
        .is_err()
    );
}

#[test]
fn desktop_meetings_read_history_and_export_the_actual_transcript() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = App::open(home.path().to_owned()).unwrap();
    app.db
        .col_put(
            "meetingSessions",
            "m1",
            &json!({"id":"m1","user_id":"u1","title":"Prueba","status":"stopped","started_at":1}),
            Default::default(),
        )
        .unwrap();
    app.db
        .col_put(
            "meetingSegments",
            "m1:0000",
            &json!({"id":"m1:0000","seq":0,"speaker":"Ana","text":"Contenido confirmado"}),
            Default::default(),
        )
        .unwrap();
    let listed = route(&app, Request::get("/api/meetings").body(vec![]).unwrap())
        .unwrap()
        .1;
    assert_eq!(listed["sessions"][0]["segment_count"], 1);
    let (status, report) = route(
        &app,
        Request::post("/api/meetings/m1/report")
            .body(b"{}".to_vec())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(status, 200);
    let path = std::path::Path::new(report["report_path"].as_str().unwrap());
    assert!(path.starts_with(home.path()));
    assert!(
        std::fs::read_to_string(path)
            .unwrap()
            .contains("Ana: Contenido confirmado")
    );
    let (status, value) = route(
        &app,
        Request::post("/api/meetings").body(b"{}".to_vec()).unwrap(),
    )
    .unwrap();
    assert_eq!(status, 400);
    assert_eq!(
        value["ok"], false,
        "no fake recording without STT configuration"
    );
}

#[tokio::test]
async fn desktop_meeting_socket_transcribes_in_memory_and_rejects_duplicate_capture() {
    use base64::Engine;
    use tokio_tungstenite::tungstenite::protocol::Role;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    app.db.col_put("models","fixture-stt",&json!({"id":"fixture-stt","provider_id":"openrouter","model_type":"stt","enabled":true,"active":true}),Default::default()).unwrap();
    let (status, created) = route(
        &app,
        Request::post("/api/meetings")
            .body(json!({"title":"Audio de prueba"}).to_string().into_bytes())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(status, 200);
    let id = created["session"]["id"].as_str().unwrap().to_owned();
    let (native, browser) = tokio::io::duplex(65536);
    let mut browser = WebSocketStream::from_raw_socket(browser, Role::Client, None).await;
    let host = app.clone();
    let meeting = id.clone();
    let driver = tokio::spawn(async move {
        let native = WebSocketStream::from_raw_socket(native, Role::Server, None).await;
        crate::meetings::serve_with(host, "u1".into(), meeting, native, |_, audio| async move {
            assert!(audio.starts_with(b"RIFF"));
            Ok("Transcripción confirmada".into())
        })
        .await
        .unwrap();
    });
    browser
        .send(Message::text(json!({"type":"ping"}).to_string()))
        .await
        .unwrap();
    assert_eq!(frame(&mut browser).await["type"], "pong");
    let (duplicate, client) = tokio::io::duplex(65536);
    let mut client = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
    let host = app.clone();
    let meeting = id.clone();
    let second = tokio::spawn(async move {
        let socket = WebSocketStream::from_raw_socket(duplicate, Role::Server, None).await;
        crate::meetings::serve_with(host, "u1".into(), meeting, socket, |_, _| async {
            panic!("duplicate must not transcribe");
            #[allow(unreachable_code)]
            Ok(String::new())
        })
        .await
        .unwrap();
    });
    assert_eq!(frame(&mut client).await["type"], "error");
    assert_eq!(frame(&mut client).await["type"], "meeting_stopped");
    second.await.unwrap();
    assert_eq!(
        app.db.col_get("meetingSessions", &id).unwrap().unwrap().doc["status"],
        "active"
    );
    let mut audio = vec![0u8; 44];
    audio[..4].copy_from_slice(b"RIFF");
    audio[8..12].copy_from_slice(b"WAVE");
    browser.send(Message::text(json!({"type":"audio_chunk","mime_type":"audio/wav","audio":base64::engine::general_purpose::STANDARD.encode(audio)}).to_string())).await.unwrap();
    let transcript = frame(&mut browser).await;
    assert_eq!(transcript["type"], "transcript_segment");
    assert_eq!(transcript["text"], "Transcripción confirmada");
    browser
        .send(Message::text(json!({"type":"meeting_stop"}).to_string()))
        .await
        .unwrap();
    assert_eq!(frame(&mut browser).await["segment_count"], 1);
    driver.await.unwrap();
    assert!(app.live_sessions.lock().unwrap().is_empty());
    assert_eq!(config::docs(&app.db, "meetingSegments").unwrap().len(), 1);
}

#[test]
fn desktop_configuration_endpoints_require_the_same_authentication_as_chat() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = App::open(home.path().to_owned()).unwrap();
    app.db
        .col_put(
            "users",
            "u1",
            &json!({"id":"u1","email":"test@example.com","password_hash":"configured"}),
            Default::default(),
        )
        .unwrap();
    for path in [
        "/api/providers",
        "/api/channels",
        "/api/agents",
        "/api/mcp/servers",
        "/api/meetings",
        "/api/cron",
        "/api/notes",
    ] {
        assert_eq!(
            route(&app, Request::get(path).body(vec![]).unwrap())
                .unwrap()
                .0,
            401
        );
        assert_eq!(
            route(
                &app,
                Request::get(path)
                    .header("Authorization", format!("Bearer {}", app.token))
                    .body(vec![])
                    .unwrap()
            )
            .unwrap()
            .0,
            200
        );
    }
    assert_eq!(
        route(
            &app,
            Request::post("/api/http-request")
                .body(b"{}".to_vec())
                .unwrap()
        )
        .unwrap()
        .0,
        401
    );
}

#[derive(Clone)]
struct PanelFixture {
    step: Arc<Mutex<usize>>,
}
impl TurnProvider for PanelFixture {
    type Stream = FixtureStream;
    type Child = Self;
    fn child(&self, _: NativeProvider) -> Self {
        self.clone()
    }
    fn start(&self, _: ModelRequest) -> Self::Stream {
        let mut step = self.step.lock().unwrap();
        let turn = *step;
        *step += 1;
        let tool = match turn {
            0 => Some(
                json!({"id":"create","name":"a2ui_create_surface","arguments":{"surfaceId":"form"}}),
            ),
            1 => Some(
                json!({"id":"components","name":"a2ui_update_components","arguments":{"surfaceId":"form","components":[{"id":"root","component":"Text","text":"Panel confirmado"}]}}),
            ),
            _ => None,
        };
        let reply = json!({"content":if tool.is_some(){Value::Null}else{json!("El panel está listo")},"tool_calls":tool.into_iter().collect::<Vec<_>>(),"usage":{"input_tokens":10,"output_tokens":2}});
        FixtureStream {
            stall: false,
            events: Mutex::new(
                [ModelEvent::Reply {
                    reply: serde_json::from_value(reply).unwrap(),
                }]
                .into(),
            ),
        }
    }
}
#[tokio::test]
async fn desktop_panel_tools_execute_through_the_durable_sdk_driver() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let created = route(
        &app,
        Request::post("/api/conversations")
            .body(b"{}".to_vec())
            .unwrap(),
    )
    .unwrap()
    .1;
    let thread = created["conversation"]["id"].as_str().unwrap();
    let selection = app.selection().unwrap();
    let handle = app.session(thread, &selection).unwrap();
    let mut canvas = app.office_events.subscribe();
    let (output, _rx) = mpsc::channel(64);
    drive_turn(
        app.clone(),
        handle,
        PanelFixture {
            step: Arc::new(Mutex::new(0)),
        },
        thread,
        "Crea un panel".into(),
        output,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let row = app
        .db
        .col_get("hive_rust_a2ui", "u1:form")
        .unwrap()
        .unwrap()
        .doc;
    assert_eq!(row["thread_id"], thread);
    assert_eq!(row["components"][0]["text"], "Panel confirmado");
    let mut rendered = false;
    while let Ok(event) = canvas.try_recv() {
        if event["type"] == "a2ui:updateComponents" {
            rendered = true;
        }
    }
    assert!(rendered);
    assert_eq!(
        config::docs(&app.db, "hive_rust_a2ui_operations")
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn multimodal_defaults_are_independent_and_persist_in_canonical_channel_configuration() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = App::open(home.path().to_owned()).unwrap();
    for (id, typ, caps) in [
        ("eye", "llm", "[\"chat\",\"vision\"]"),
        ("paint", "image", "[\"image_generation\"]"),
        ("ears", "stt", "[\"transcription\"]"),
    ] {
        app.db.col_put("models",id,&json!({"id":id,"provider_id":"openrouter","model_type":typ,"capabilities":caps,"enabled":true,"active":true}),Default::default()).unwrap();
    }
    app.db
        .col_put(
            "channels",
            "wc",
            &json!({"id":"wc","user_id":"u1","type":"webchat","config":"{\"other_setting\":true}"}),
            Default::default(),
        )
        .unwrap();
    let set = |id: &str, purpose: &str| {
        route(
            &app,
            Request::post(format!("/api/models/{id}/default"))
                .body(json!({"purpose":purpose}).to_string().into_bytes())
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(set("paint", "vision").0, 400);
    assert_eq!(set("eye", "vision").0, 200);
    assert_eq!(set("ears", "stt").0, 200);
    assert_eq!(app.selection().unwrap().model_id, "openrouter/vendor/model");
    assert_eq!(
        app.db
            .col_get("models", "gemini-3.8-live")
            .unwrap()
            .unwrap()
            .doc["active"],
        true
    );
    let channel: Value = serde_json::from_str(
        &config::secret(&app.db, &app.home, "channel:wc:config")
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(channel["vision_model_id"], "eye");
    assert_eq!(channel["vision_provider"], "openrouter");
    assert_eq!(channel["other_setting"], true);
    let (_, providers) = route(&app, Request::get("/api/providers").body(vec![]).unwrap()).unwrap();
    let models = providers["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == "openrouter")
        .unwrap()["models"]
        .as_array()
        .unwrap();
    assert_eq!(
        models.iter().find(|m| m["id"] == "eye").unwrap()["default_for"],
        json!(["vision"])
    );
    drop(app);
    let app = App::open(home.path().to_owned()).unwrap();
    assert_eq!(
        crate::model_defaults::get(&app).unwrap()["stt_model_id"],
        "ears"
    );
}

#[test]
fn piper_status_and_settings_use_authenticated_native_routes() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = App::open(home.path().to_owned()).unwrap();
    let (code, status) = route(
        &app,
        Request::get("/api/tts-local/status").body(vec![]).unwrap(),
    )
    .unwrap();
    assert_eq!(code, 200);
    assert_eq!(status["installed"], false);
    assert_eq!(status["running"], false);
    assert_eq!(status["models"].as_array().unwrap().len(), 3);
    let post = |op: &str, body: Value| {
        route(
            &app,
            Request::post(format!("/api/tts-local/{op}"))
                .body(body.to_string().into_bytes())
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(post("start", json!({})).0, 400);
    assert_eq!(post("speak", json!({"text":"Hola"})).0, 400);
    assert_eq!(post("config", json!({"length_scale":0.8})).0, 200);
    assert_eq!(post("config", json!({"noise_w":-1})).0, 400);
    assert_eq!(
        app.db
            .col_get("config", "piper_local")
            .unwrap()
            .unwrap()
            .doc["length_scale"],
        0.8
    );
}

#[derive(Clone)]
struct GroupProvider {
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    barrier: Option<Arc<tokio::sync::Barrier>>,
    disable: Option<Arc<App>>,
}
struct GroupStream {
    reply: Mutex<Option<hive_agent_core::ModelReply>>,
    barrier: Option<Arc<tokio::sync::Barrier>>,
}
impl TurnStream for GroupStream {
    async fn next(&self) -> Result<Option<ModelEvent>> {
        let reply = self.reply.lock().unwrap().take();
        if let Some(reply) = reply {
            if let Some(barrier) = &self.barrier {
                barrier.wait().await;
            }
            Ok(Some(ModelEvent::Reply { reply }))
        } else {
            Ok(None)
        }
    }
    fn cancel(&self) {}
}
impl TurnProvider for GroupProvider {
    type Stream = GroupStream;
    type Child = Self;
    fn child(&self, _: NativeProvider) -> Self {
        self.clone()
    }
    fn start(&self, request: ModelRequest) -> Self::Stream {
        let child = request.tools.is_empty();
        let has_result = request.messages.iter().any(|m| m.role == "tool");
        self.requests.lock().unwrap().push(request);
        let reply = if !child && !has_result {
            if let Some(app) = &self.disable {
                let mut row = app.db.col_get("agents", "w2").unwrap().unwrap().doc;
                row["enabled"] = json!(false);
                app.db
                    .col_put("agents", "w2", &row, PutOptions::default())
                    .unwrap();
            }
            json!({"content":null,"tool_calls":[{"id":"group-call","name":"task_delegate_group","arguments":{"tasks":[
                {"task_id":"a","worker_id":"w1","task_description":"Consulta A"},
                {"task_id":"b","worker_id":"w2","task_description":"Consulta B"}]}}],"usage":{"input_tokens":10,"output_tokens":2}})
        } else {
            json!({"content":if child {"Informe independiente"} else {"Una respuesta final"},"usage":{"input_tokens":10,"output_tokens":2}})
        };
        GroupStream {
            reply: Mutex::new(Some(serde_json::from_value(reply).unwrap())),
            barrier: if child { self.barrier.clone() } else { None },
        }
    }
}
fn seed_second_worker(app: &App) {
    seed_worker(app);
    let mut row = app.db.col_get("agents", "w1").unwrap().unwrap().doc;
    row["id"] = json!("w2");
    row["name"] = json!("Segundo analista");
    app.db
        .col_put("agents", "w2", &row, PutOptions::default())
        .unwrap();
}
#[tokio::test]
async fn durable_group_runs_specialists_in_parallel_and_continues_one_parent_checkpoint() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    seed_second_worker(&app);
    // Use the public route's canonical thread construction.
    let (_, created) = route(
        &app,
        Request::post("/api/conversations")
            .body(b"{}".to_vec())
            .unwrap(),
    )
    .unwrap();
    let thread = created["conversation"]["id"].as_str().unwrap().to_owned();
    let selected = app.selection().unwrap();
    let handle = app.session(&thread, &selected).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (output, mut rx) = mpsc::channel(128);
    tokio::time::timeout(
        Duration::from_secs(5),
        drive_turn(
            app.clone(),
            handle.clone(),
            GroupProvider {
                requests: requests.clone(),
                barrier: Some(Arc::new(tokio::sync::Barrier::new(2))),
                disable: None,
            },
            &thread,
            "Consulta a ambos".into(),
            output,
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    let frame = dispatch(&handle, json!({"op":"frame"})).await.unwrap();
    let source = app.store.lock().unwrap().as_ref().unwrap().clone();
    let root = source
        .inspect_objective(frame["objective_id"].as_str().unwrap())
        .unwrap();
    assert_eq!(root["model_calls"], 4);
    assert_eq!(root["tool_calls"], 3);
    assert_eq!(root["output_tokens"], 8);
    assert_eq!(root["reservations"], json!({}));
    let queue = source
        .queue(&format!(
            "gateway.specialists.{}",
            frame["objective_id"].as_str().unwrap()
        ))
        .unwrap();
    let jobs = queue.list(50, None).unwrap()["items"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(jobs.len(), 2);
    assert_ne!(
        jobs[0]["input"]["payload"]["delegation"]["session_id"],
        jobs[1]["input"]["payload"]["delegation"]["session_id"]
    );
    for job in &jobs {
        assert_eq!(job["status"], "completed");
        assert_eq!(job["input"]["objective_id"], frame["objective_id"]);
        assert_eq!(job["attempts"], 1);
    }
    let group_id = jobs[0]["input"]["payload"]["delegation"]["group_id"]
        .as_str()
        .unwrap();
    let group = queue.group(group_id).unwrap();
    assert_eq!(group["status"], "ready");
    assert!(group["synthesis"].is_null());
    let requests = requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 4);
    let parent = requests
        .iter()
        .find(|request| {
            !request.tools.is_empty() && request.messages.iter().any(|m| m.role == "tool")
        })
        .unwrap();
    let result = parent.messages.iter().find(|m| m.role == "tool").unwrap();
    let result: Value = serde_json::from_str(result.content.as_deref().unwrap()).unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(result["results"].as_array().unwrap().len(), 2);
    assert!(
        result["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["checks"]["status"] == "unchecked")
    );
    let mut final_messages = 0;
    while let Some(event) = rx.recv().await {
        if event["turnDone"] == true {
            final_messages += 1;
        }
    }
    assert_eq!(final_messages, 1);
}
#[tokio::test]
async fn durable_group_preserves_confirmed_preparation_failures_for_the_coordinator() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    seed_second_worker(&app);
    let (_, created) = route(
        &app,
        Request::post("/api/conversations")
            .body(b"{}".to_vec())
            .unwrap(),
    )
    .unwrap();
    let thread = created["conversation"]["id"].as_str().unwrap();
    let handle = app.session(thread, &app.selection().unwrap()).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (output, _rx) = mpsc::channel(128);
    drive_turn(
        app.clone(),
        handle,
        GroupProvider {
            requests: requests.clone(),
            barrier: None,
            disable: Some(app.clone()),
        },
        thread,
        "Consulta".into(),
        output,
        CancellationToken::new(),
    )
    .await
    .unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let last = requests.last().unwrap();
    let content = last
        .messages
        .iter()
        .find(|m| m.role == "tool")
        .unwrap()
        .content
        .as_deref()
        .unwrap();
    let result: Value = serde_json::from_str(content).unwrap();
    assert_eq!(result["ok"], false);
    assert_eq!(result["results"][1]["worker_id"], "w2");
    assert_eq!(result["results"][1]["status"], "failed");
    assert!(
        result["results"][1]["error"]
            .as_str()
            .unwrap()
            .contains("deshabilitado")
    );
}

#[derive(Clone)]
struct CrashChildProvider(Arc<std::path::PathBuf>);
impl TurnProvider for CrashChildProvider {
    type Stream = FixtureStream;
    type Child = Self;
    fn child(&self, _: NativeProvider) -> Self {
        self.clone()
    }
    fn start(&self, request: ModelRequest) -> Self::Stream {
        if request.tools.is_empty() {
            // The durable model and queue claims are already committed.
            std::fs::write(self.0.join("effect"), "one fixture effect").unwrap();
            std::process::exit(17);
        }
        DelegatingProvider {
            requests: Arc::new(Mutex::new(Vec::new())),
            stall_child: false,
        }
        .start(request)
    }
}
#[test]
fn durable_gateway_crash_child_fixture() {
    let Some(home) = std::env::var_os("HIVE_GATEWAY_CRASH_FIXTURE") else {
        return;
    };
    let home = std::path::PathBuf::from(home);
    let thread = std::fs::read_to_string(home.join("thread")).unwrap();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let app = Arc::new(App::open(home.clone()).unwrap());
        let handle = app.session(&thread, &app.selection().unwrap()).unwrap();
        let (output, _rx) = mpsc::channel(128);
        drive_turn(
            app,
            handle,
            CrashChildProvider(Arc::new(home)),
            &thread,
            "Consulta".into(),
            output,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    });
    panic!("fixture never reached the child model");
}
#[test]
fn durable_gateway_crash_retains_group_root_and_uncertain_operation_without_replay() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let thread = {
        let app = App::open(home.path().to_owned()).unwrap();
        seed_worker(&app);
        let (_, created) = route(
            &app,
            Request::post("/api/conversations")
                .body(b"{}".to_vec())
                .unwrap(),
        )
        .unwrap();
        created["conversation"]["id"].as_str().unwrap().to_owned()
    };
    std::fs::write(home.path().join("thread"), &thread).unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "server::tests::durable_gateway_crash_child_fixture",
            "--nocapture",
        ])
        .env("HIVE_GATEWAY_CRASH_FIXTURE", home.path())
        .env_remove("HIVE_DB_PATH")
        .output()
        .unwrap();
    assert_eq!(
        child.status.code(),
        Some(17),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    let app = App::open(home.path().to_owned()).unwrap();
    assert!(app.session(&thread, &app.selection().unwrap()).is_err());
    let source = app.store.lock().unwrap().as_ref().unwrap().clone();
    let checkpoint = source.inspect("a1", &thread, None).unwrap();
    assert_eq!(checkpoint["status"], "running");
    let root_id = checkpoint["run"]["objective_id"].as_str().unwrap();
    let root = source.inspect_objective(root_id).unwrap();
    assert_eq!(root["model_calls"], 2);
    assert_eq!(root["tool_calls"], 2);
    assert_eq!(root["reservations"].as_object().unwrap().len(), 1);
    let queue = source
        .queue(&format!("gateway.specialists.{root_id}"))
        .unwrap();
    let jobs = queue.list(50, None).unwrap();
    let jobs = jobs["items"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    let job = &jobs[0];
    assert_eq!(job["status"], "running");
    assert_eq!(job["attempts"], 1);
    let binding = &job["input"]["payload"]["delegation"];
    let specialist = source
        .inspect(
            binding["agent_id"].as_str().unwrap(),
            binding["session_id"].as_str().unwrap(),
            None,
        )
        .unwrap();
    assert_eq!(specialist["run"]["objective_id"], root_id);
    assert_eq!(specialist["run"]["in_flight"]["kind"], "model");
    assert_eq!(
        std::fs::read_to_string(home.path().join("effect")).unwrap(),
        "one fixture effect"
    );
    assert!(
        queue
            .claim(
                "restarted",
                &[job["input"]["kind"].as_str().unwrap().into()]
            )
            .unwrap()
            .is_none()
    );
    assert_eq!(source.inspect_objective(root_id).unwrap()["model_calls"], 2);
    // With the old owner process stopped, explicit administration can record uncertainty.
    queue
        .finish(
            job["id"].as_str().unwrap(),
            job["execution_id"].as_str().unwrap(),
            hive_agent_core::harness::JobOutcome::Uncertain {
                error: "fixture process exited after effect".into(),
            },
        )
        .unwrap();
    assert_eq!(
        queue.group(binding["group_id"].as_str().unwrap()).unwrap()["status"],
        "blocked_uncertain"
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join("effect")).unwrap(),
        "one fixture effect"
    );
}
