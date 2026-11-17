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
        200
    );
    assert_eq!(
        client
            .get(format!("{base}/api/not-implemented"))
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
fn a_neutral_copy_cannot_reach_real_services_on_its_own_and_leaves_the_source_alone() {
    let source = tempfile::tempdir().unwrap();
    seed(source.path(), "http://127.0.0.1:9/v1");
    {
        let db = HiveDB::open(source.path().join("data/hivedb")).unwrap();
        for (collection, id, doc) in [
            (
                "mcpServers",
                "correo",
                json!({"id":"correo","name":"Correo","enabled":true,"transport":"sse","url":"http://x"}),
            ),
            (
                "mcpServers",
                "apagado",
                json!({"id":"apagado","name":"Ya apagado","enabled":false,"transport":"sse","url":"http://x"}),
            ),
            (
                "cronJobs",
                "t1",
                json!({"id":"t1","name":"Diaria","status":"active","cron_expression":"0 19 * * *"}),
            ),
            (
                "cronJobs",
                "t2",
                json!({"id":"t2","name":"Pausada","status":"paused","cron_expression":"0 19 * * *"}),
            ),
            (
                "channels",
                "webchat",
                json!({"id":"webchat","type":"webchat","enabled":true}),
            ),
            (
                "channels",
                "telegram",
                json!({"id":"telegram","type":"telegram","enabled":true}),
            ),
        ] {
            db.col_put(collection, id, &doc, PutOptions::default())
                .unwrap();
        }
    }
    let parent = tempfile::tempdir().unwrap();
    let target = parent.path().join("copy");
    config::copy_home(source.path(), &target).unwrap();
    let changed = config::neutralize_home(&target).unwrap().join("\n");
    assert!(
        changed.contains("servidor MCP correo: apagado")
            && changed.contains("tarea programada t1: en pausa"),
        "{changed}"
    );
    assert!(changed.contains("canal telegram: apagado"), "{changed}");
    assert!(
        !changed.contains("apagado: apagado")
            && !changed.contains("t2")
            && !changed.contains("webchat"),
        "{changed}"
    );
    let copy = HiveDB::open(target.join("data/hivedb")).unwrap();
    assert_eq!(
        copy.col_get("mcpServers", "correo").unwrap().unwrap().doc["enabled"],
        json!(false)
    );
    assert_eq!(
        copy.col_get("cronJobs", "t1").unwrap().unwrap().doc["status"],
        json!("paused")
    );
    assert_eq!(
        copy.col_get("channels", "telegram").unwrap().unwrap().doc["enabled"],
        json!(false)
    );
    assert_eq!(
        copy.col_get("channels", "webchat").unwrap().unwrap().doc["enabled"],
        json!(true),
        "the web chat stays"
    );
    drop(copy);
    // The profile it was copied from is untouched.
    let original = HiveDB::open(source.path().join("data/hivedb")).unwrap();
    assert_eq!(
        original
            .col_get("mcpServers", "correo")
            .unwrap()
            .unwrap()
            .doc["enabled"],
        json!(true)
    );
    assert_eq!(
        original.col_get("cronJobs", "t1").unwrap().unwrap().doc["status"],
        json!("active")
    );
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
async fn hivelive_rejects_a_bad_video_frame_with_an_error_and_the_call_goes_on() {
    use base64::Engine;
    use tokio_tungstenite::tungstenite::protocol::Role;
    let b64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let thread = seed_live(&app);
    let query = std::collections::HashMap::from([("threadId".into(), thread)]);
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
    let _setup = frame(&mut gemini).await;
    gemini
        .send(Message::text(json!({"setupComplete":{}}).to_string()))
        .await
        .unwrap();
    assert_eq!(frame(&mut browser).await["type"], "pricing");
    assert_eq!(frame(&mut browser).await["type"], "ready");
    let jpeg = b64(&[0xff, 0xd8, 0xff, 0xd9]);
    let bad = [
        (
            json!({"type":"video","data":b64(b"\x89PNG\r\n\x1a\n"),"mimeType":"image/jpeg"}),
            "HiveLive solo admite fotogramas JPEG",
        ),
        (
            json!({"type":"video","data":jpeg,"mimeType":"image/png"}),
            "HiveLive solo admite fotogramas JPEG",
        ),
        (
            json!({"type":"video","data":jpeg,"mimeType":"image/jpeg","source":"webcam"}),
            "Fuente visual inválida",
        ),
        (
            json!({"type":"video","data":"%%%no%%%","mimeType":"image/jpeg"}),
            "JPEG base64 inválido",
        ),
        (
            json!({"type":"video","mimeType":"image/jpeg"}),
            "Fotograma sin datos",
        ),
        (
            json!({"type":"video","data":"A".repeat(2 * 1024 * 1024),"mimeType":"image/jpeg"}),
            "Fotograma demasiado grande",
        ),
    ];
    for (message, reason) in bad {
        browser
            .send(Message::text(message.to_string()))
            .await
            .unwrap();
        let event = frame(&mut browser).await;
        assert_eq!(
            (event["type"].clone(), event["error"].clone()),
            (json!("error"), json!(reason))
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(40), gemini.next())
                .await
                .is_err(),
            "a refused frame never reaches Gemini: {reason}"
        );
    }
    // The call is still alive: camera frame, screen frame and microphone audio all go through.
    for source in ["camera", "screen"] {
        browser
            .send(Message::text(
                json!({"type":"video","data":jpeg,"mimeType":"image/jpeg","source":source})
                    .to_string(),
            ))
            .await
            .unwrap();
        assert_eq!(
            frame(&mut gemini).await["realtimeInput"]["video"]["mimeType"],
            "image/jpeg",
            "{source}"
        );
    }
    browser
        .send(Message::Binary(vec![0, 0, 1, 0]))
        .await
        .unwrap();
    assert_eq!(
        frame(&mut gemini).await["realtimeInput"]["audio"]["mimeType"],
        "audio/pcm;rate=16000"
    );
    browser.close(None).await.unwrap();
    assert_eq!(driver.await.unwrap(), crate::realtime::End::Closed);
}

/// A fake MCP server over the classic HTTP+SSE transport (the one n8n speaks): `GET /mcp/x` opens
/// the event stream, whose first event names where to `POST`; answers come back on the stream.
struct FakeMcp {
    url: String,
    gets: Arc<AtomicUsize>,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
    /// While false the stream is refused (403), as a server that is down or not ready.
    open: Arc<std::sync::atomic::AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for FakeMcp {
    fn drop(&mut self) {
        self.task.abort();
    }
}
/// `drop_on_call`: the connection is cut while a tool call is running, so its outcome is unknown.
async fn fake_mcp(drop_on_call: bool) -> FakeMcp {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (gets, calls) = (Arc::new(AtomicUsize::new(0)), Arc::new(Mutex::new(vec![])));
    let open = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let stream: Arc<Mutex<Option<mpsc::UnboundedSender<String>>>> = Arc::default();
    let (g, c, o) = (gets.clone(), calls.clone(), open.clone());
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (gets, calls, open, stream) = (g.clone(), c.clone(), o.clone(), stream.clone());
            tokio::spawn(async move {
                let Ok(request) = read_request(&mut socket).await else {
                    return;
                };
                if request.method() == "GET" {
                    gets.fetch_add(1, AtomicOrdering::SeqCst);
                    if !open.load(AtomicOrdering::SeqCst) {
                        let _ = socket.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                        return;
                    }
                    let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\r\nevent: endpoint\ndata: /messages?sessionId=s1\n\n").await;
                    let (sender, mut receiver) = mpsc::unbounded_channel::<String>();
                    *stream.lock().unwrap() = Some(sender);
                    while let Some(text) = receiver.recv().await {
                        if text == "END" || socket.write_all(text.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                    return;
                }
                let body: Value = serde_json::from_slice(request.body()).unwrap_or(Value::Null);
                let _ = socket.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 8\r\nConnection: close\r\n\r\nAccepted").await;
                let Some(sender) = stream.lock().unwrap().clone() else {
                    return;
                };
                let tool = |name: &str| json!({"name":name,"description":format!("{name} tool"),"inputSchema":{"$schema":"http://json-schema.org/draft-07/schema#","type":"object","properties":{"value":{}},"required":["value"]}});
                let long = "x".repeat(70);
                let result = match body["method"].as_str() {
                    Some("initialize") => {
                        json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"1"}})
                    }
                    // `has space` and a 70-character name cannot be wire names and must be left out.
                    Some("tools/list") => {
                        json!({"tools":[tool("echo"),tool("has space"),tool(&long)]})
                    }
                    Some("tools/call") => {
                        calls.lock().unwrap().push((
                            body["params"]["name"]
                                .as_str()
                                .unwrap_or_default()
                                .to_owned(),
                            body["params"]["arguments"].clone(),
                        ));
                        if drop_on_call {
                            let _ = sender.send("END".into());
                            return;
                        }
                        json!({"content":[{"type":"text","text":"hecho"}],"structuredContent":{"echoed":body["params"]["arguments"]["value"]}})
                    }
                    _ => return,
                };
                let _ = sender.send(format!(
                    "event: message\ndata: {}\n\n",
                    json!({"jsonrpc":"2.0","id":body["id"],"result":result})
                ));
            });
        }
    });
    FakeMcp {
        url: format!("http://127.0.0.1:{port}/mcp/x"),
        gets,
        calls,
        open,
        task,
    }
}
fn put_mcp(app: &App, id: &str, row: Value) {
    let mut row = row;
    row["id"] = json!(id);
    row["name"] = json!(id.to_uppercase());
    app.db
        .col_put("mcpServers", id, &row, PutOptions::default())
        .unwrap();
}
fn mcp_row(app: &App, id: &str) -> Value {
    crate::mcp_servers::status_rows(app)
        .into_iter()
        .find(|r| r["id"] == id)
        .unwrap_or_else(|| panic!("no row {id}"))
}

/// A scripted model that asks for `mcp_ok__echo` once and then says "listo".
#[derive(Clone)]
struct McpToolFixture {
    step: Arc<Mutex<usize>>,
    seen: Arc<Mutex<Vec<String>>>,
}
impl TurnProvider for McpToolFixture {
    type Stream = FixtureStream;
    type Child = Self;
    fn child(&self, _: NativeProvider) -> Self {
        self.clone()
    }
    fn start(&self, request: ModelRequest) -> Self::Stream {
        self.seen
            .lock()
            .unwrap()
            .push(format!("{:?}", request.messages));
        let mut step = self.step.lock().unwrap();
        let first = *step == 0;
        *step += 1;
        let reply = if first {
            json!({"content":null,"tool_calls":[{"id":"c1","name":"mcp_ok__echo","arguments":{"value":"hola"}}],"usage":{"input_tokens":10,"output_tokens":2}})
        } else {
            json!({"content":"listo","tool_calls":[],"usage":{"input_tokens":10,"output_tokens":2}})
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
async fn mcp_turn(app: &Arc<App>) -> (Result<()>, McpToolFixture, mpsc::Receiver<Value>) {
    let thread = call(
        app,
        Some(&app.token),
        "POST",
        "/api/conversations",
        json!({}),
    )
    .1["conversation"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let handle = app.session(&thread, &app.selection().unwrap()).unwrap();
    let (output, receiver) = mpsc::channel(64);
    let fixture = McpToolFixture {
        step: Arc::default(),
        seen: Arc::default(),
    };
    let result = drive_turn(
        app.clone(),
        handle,
        fixture.clone(),
        &thread,
        "Haz algo".into(),
        output,
        CancellationToken::new(),
    )
    .await;
    (result, fixture, receiver)
}

#[tokio::test]
async fn mcp_servers_are_connected_with_their_real_state_and_tools_and_unsafe_rows_are_explained() {
    crate::mcp_servers::speed_up(100, 300);
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let (ok, off) = (fake_mcp(false).await, fake_mcp(false).await);
    put_mcp(
        &app,
        "ok",
        json!({"enabled":true,"transport":"sse","url":ok.url}),
    );
    put_mcp(
        &app,
        "off",
        json!({"enabled":false,"transport":"sse","url":off.url}),
    );
    put_mcp(
        &app,
        "proc",
        json!({"enabled":true,"transport":"stdio","command":"rm","args":["-rf","/"]}),
    );
    put_mcp(&app, "nourl", json!({"enabled":true,"transport":"sse"}));
    put_mcp(
        &app,
        "bad__id",
        json!({"enabled":true,"transport":"sse","url":ok.url}),
    );
    put_mcp(
        &app,
        "weird",
        json!({"enabled":true,"transport":"carrier-pigeon","url":ok.url}),
    );
    // Before anything runs, an enabled row is only "connecting" and shows nothing it does not have.
    assert_eq!(
        (
            mcp_row(&app, "ok")["status"].clone(),
            mcp_row(&app, "ok")["tools_count"].clone()
        ),
        (json!("connecting"), json!(0))
    );
    let supervisor = tokio::spawn(crate::mcp_servers::run(app.clone()));
    eventually("the server connects", || {
        mcp_row(&app, "ok")["status"] == "connected"
    })
    .await;
    let row = mcp_row(&app, "ok");
    assert_eq!(
        row["tools"],
        json!(["echo", "has space", "x".repeat(70)]),
        "the catalog is what the server really listed"
    );
    assert_eq!(row["error"], Value::Null);
    // The same through the API the desktop reads.
    let listed = call(&app, Some(&app.token), "GET", "/api/mcp/servers", json!({})).1;
    assert_eq!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == "ok")
            .unwrap()["status"],
        "connected"
    );
    // A disabled row is never contacted; a stdio row is never run (it would be a command chosen by whoever writes the row).
    assert_eq!(mcp_row(&app, "off")["status"], "disconnected");
    assert_eq!(
        off.gets.load(AtomicOrdering::SeqCst),
        0,
        "a disabled server is not even connected to"
    );
    let proc = mcp_row(&app, "proc");
    assert_eq!(proc["status"], "error");
    assert!(proc["error"].as_str().unwrap().contains("stdio"), "{proc}");
    for (id, expected) in [("nourl", "URL"), ("bad__id", "id"), ("weird", "transporte")] {
        let row = mcp_row(&app, id);
        assert_eq!(row["status"], "error", "{id}");
        assert!(
            row["error"].as_str().unwrap().contains(expected),
            "{id}: {row}"
        );
    }
    // Only the coordinator is offered tools, and only the ones that can be safe wire names.
    let specs = crate::mcp_servers::tool_specs(&app);
    assert_eq!(
        specs
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["mcp_ok__echo"]
    );
    assert!(
        specs[0]["parameters"].get("$schema").is_none()
            && specs[0]["description"].as_str().unwrap().contains("«ok»")
    );
    assert_eq!(
        crate::mcp_servers::resolve(&app, "mcp_ok__echo"),
        Some(("ok".into(), "echo".into()))
    );
    assert_eq!(crate::mcp_servers::resolve(&app, "mcp_ok__has space"), None);
    // The row is changed: disabling closes it, and enabling brings it back.
    put_mcp(
        &app,
        "ok",
        json!({"enabled":false,"transport":"sse","url":ok.url}),
    );
    app.db
        .col_put(
            "mcpServers",
            "ok",
            &json!({"id":"ok","name":"OK","enabled":false,"transport":"sse","url":ok.url}),
            PutOptions {
                expected_version: None,
            },
        )
        .ok();
    eventually("a disabled row disconnects", || {
        mcp_row(&app, "ok")["status"] == "disconnected"
    })
    .await;
    assert!(crate::mcp_servers::tool_specs(&app).is_empty());
    app.shutdown.cancel();
    let _ = supervisor.await;
}

#[tokio::test]
async fn an_mcp_tool_runs_once_through_the_policy_and_a_denied_one_never_reaches_the_server() {
    crate::mcp_servers::speed_up(100, 300);
    for (policy, approve, runs) in [
        (None, None, 1usize),
        (
            Some(
                json!({"rules":[{"tools":["mcp_ok__echo"],"decision":"ask","reason":"toca una cuenta real"}]}),
            ),
            Some(true),
            1,
        ),
        (
            Some(json!({"rules":[{"tools":["mcp_ok__echo"],"decision":"ask"}]})),
            Some(false),
            0,
        ),
        (
            Some(json!({"rules":[{"tools":["mcp_*"],"decision":"deny","reason":"no"}]})),
            None,
            0,
        ),
    ] {
        let home = tempfile::tempdir().unwrap();
        seed(home.path(), "http://127.0.0.1:9/v1");
        let app = Arc::new(App::open(home.path().to_owned()).unwrap());
        let server = fake_mcp(false).await;
        put_mcp(
            &app,
            "ok",
            json!({"enabled":true,"transport":"sse","url":server.url}),
        );
        let supervisor = tokio::spawn(crate::mcp_servers::run(app.clone()));
        eventually("connected", || mcp_row(&app, "ok")["status"] == "connected").await;
        if let Some(policy) = &policy {
            put_policy(&app, policy.clone(), 5000);
        }
        let turn = tokio::spawn({
            let app = app.clone();
            async move { mcp_turn(&app).await }
        });
        if let Some(approve) = approve {
            let id = loop {
                if let Some(id) = app.approvals.lock().unwrap().keys().next().cloned() {
                    break id;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            };
            assert!(
                server.calls.lock().unwrap().is_empty(),
                "nothing ran while it waited"
            );
            assert!(
                app.resolve_approval(
                    &id,
                    &crate::config::Principal {
                        user: "u1".into(),
                        admin: false
                    },
                    approve
                )
                .unwrap()
            );
        }
        let (result, fixture, _frames) = turn.await.unwrap();
        result.unwrap();
        let calls = server.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), runs, "{policy:?} approve={approve:?}");
        let told = fixture.seen.lock().unwrap().last().unwrap().clone();
        if runs == 1 {
            assert_eq!(calls[0], ("echo".to_owned(), json!({"value":"hola"})));
            assert!(
                told.contains("echoed") && told.contains("hecho"),
                "the model got the server's answer: {told}"
            );
        } else {
            assert!(
                told.contains("POLICY_DENIED"),
                "the model was told why: {told}"
            );
        }
        app.shutdown.cancel();
        let _ = supervisor.await;
    }
}

#[tokio::test]
async fn an_mcp_call_whose_connection_drops_is_reported_uncertain_and_is_not_repeated_even_after_it_reconnects()
 {
    crate::mcp_servers::speed_up(100, 300);
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let server = fake_mcp(true).await;
    put_mcp(
        &app,
        "ok",
        json!({"enabled":true,"transport":"sse","url":server.url}),
    );
    let supervisor = tokio::spawn(crate::mcp_servers::run(app.clone()));
    eventually("connected", || mcp_row(&app, "ok")["status"] == "connected").await;
    let (result, fixture, _frames) = mcp_turn(&app).await;
    result.unwrap();
    let told = fixture.seen.lock().unwrap().last().unwrap().clone();
    assert!(
        told.contains("MCP_OPERATION_UNCERTAIN") && told.contains("No la repitas"),
        "the model is told the outcome is unknown: {told}"
    );
    // The server's connection is closed, and the supervisor reconnects it on its own later; the call is never sent again.
    eventually("it reconnects by itself", || {
        server.gets.load(AtomicOrdering::SeqCst) >= 2
            && mcp_row(&app, "ok")["status"] == "connected"
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        server.calls.lock().unwrap().len(),
        1,
        "the uncertain call was sent exactly once"
    );
    app.shutdown.cancel();
    let _ = supervisor.await;
}

#[tokio::test]
async fn a_server_that_is_down_is_retried_by_the_supervisor_until_it_answers() {
    crate::mcp_servers::speed_up(100, 300);
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let server = fake_mcp(false).await;
    server.open.store(false, AtomicOrdering::SeqCst);
    put_mcp(
        &app,
        "ok",
        json!({"enabled":true,"transport":"sse","url":server.url}),
    );
    let supervisor = tokio::spawn(crate::mcp_servers::run(app.clone()));
    eventually(
        "it is reported as failed, with a reason and no secrets",
        || mcp_row(&app, "ok")["status"] == "error",
    )
    .await;
    let failed = mcp_row(&app, "ok");
    assert!(
        failed["error"].as_str().is_some_and(|e| !e.is_empty())
            && !failed.to_string().contains("s1"),
        "{failed}"
    );
    assert_eq!(failed["tools_count"], 0);
    server.open.store(true, AtomicOrdering::SeqCst);
    eventually("it connects once the server is up", || {
        mcp_row(&app, "ok")["status"] == "connected"
    })
    .await;
    assert!(
        server.gets.load(AtomicOrdering::SeqCst) >= 2,
        "it tried again by itself"
    );
    app.shutdown.cancel();
    let _ = supervisor.await;
}

/// A scripted model that makes the given tool calls one at a time and then says "listo".
#[derive(Clone)]
struct ScriptFixture {
    steps: Arc<Vec<(&'static str, Value)>>,
    /// What it says when the tool calls are done, one entry per final answer (then "listo").
    finals: Arc<Vec<&'static str>>,
    step: Arc<Mutex<usize>>,
    seen: Arc<Mutex<Vec<String>>>,
}
impl ScriptFixture {
    fn new(steps: Vec<(&'static str, Value)>) -> Self {
        Self {
            steps: Arc::new(steps),
            finals: Arc::default(),
            step: Arc::default(),
            seen: Arc::default(),
        }
    }
    fn saying(mut self, finals: Vec<&'static str>) -> Self {
        self.finals = Arc::new(finals);
        self
    }
}
impl TurnProvider for ScriptFixture {
    type Stream = FixtureStream;
    type Child = Self;
    fn child(&self, _: NativeProvider) -> Self {
        self.clone()
    }
    fn start(&self, request: ModelRequest) -> Self::Stream {
        self.seen
            .lock()
            .unwrap()
            .push(format!("{:?}", request.messages));
        let mut step = self.step.lock().unwrap();
        let reply = match self.steps.get(*step) {
            Some((name, arguments)) => {
                json!({"content":null,"tool_calls":[{"id":format!("c{step}"),"name":name,"arguments":arguments}],"usage":{"input_tokens":10,"output_tokens":2}})
            }
            None => {
                let said = self
                    .finals
                    .get(*step - self.steps.len())
                    .copied()
                    .unwrap_or("listo");
                json!({"content":said,"tool_calls":[],"usage":{"input_tokens":10,"output_tokens":2}})
            }
        };
        *step += 1;
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
/// Run a scripted turn, answering every approval with `answers` in order (then refusing); returns
/// how many approvals were asked.
async fn script_turn(
    app: &Arc<App>,
    steps: Vec<(&'static str, Value)>,
    answers: Vec<bool>,
) -> (Result<()>, ScriptFixture, usize) {
    let thread = call(
        app,
        Some(&app.token),
        "POST",
        "/api/conversations",
        json!({}),
    )
    .1["conversation"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    script_turn_in(app, &thread, steps, answers).await
}
async fn script_turn_in(
    app: &Arc<App>,
    thread: &str,
    steps: Vec<(&'static str, Value)>,
    answers: Vec<bool>,
) -> (Result<()>, ScriptFixture, usize) {
    script_turn_with(app, thread, ScriptFixture::new(steps), answers).await
}
async fn script_turn_with(
    app: &Arc<App>,
    thread: &str,
    fixture: ScriptFixture,
    answers: Vec<bool>,
) -> (Result<()>, ScriptFixture, usize) {
    let handle = app.session(thread, &app.selection().unwrap()).unwrap();
    let (output, _frames) = mpsc::channel(256);
    let turn = tokio::spawn({
        let (app, fixture, thread) = (app.clone(), fixture.clone(), thread.to_owned());
        async move {
            drive_turn(
                app,
                handle,
                fixture,
                &thread,
                "Haz algo".into(),
                output,
                CancellationToken::new(),
            )
            .await
        }
    });
    let mut asked = 0;
    let mut answers = answers.into_iter();
    while !turn.is_finished() {
        let pending = app.approvals.lock().unwrap().keys().next().cloned();
        if let Some(id) = pending {
            asked += 1;
            let owner = crate::config::Principal {
                user: "u1".into(),
                admin: false,
            };
            app.resolve_approval(&id, &owner, answers.next().unwrap_or(false))
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    (turn.await.unwrap(), fixture, asked)
}
fn desktop_app(
    settings: Value,
    vision: bool,
) -> (
    Arc<App>,
    tempfile::TempDir,
    hive_agent_core::tools::ScriptedDesktop,
) {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    if vision {
        app.db.col_put("models", "openrouter/vendor/model", &json!({"id":"openrouter/vendor/model","provider_id":"openrouter","active":true,"enabled":true,"model_type":"llm","capabilities":["vision"]}), PutOptions::default()).unwrap();
    }
    let (status, body) = call(&app, Some(&app.token), "PUT", "/api/computer", settings);
    assert_eq!(status, 200, "{body}");
    let desktop = app
        .computer_tools()
        .unwrap()
        .map(|t| t.attach_scripted_desktop(1920, 1080));
    (
        app,
        home,
        desktop.unwrap_or_else(|| {
            hive_agent_core::tools::NativeTools::from_json(
                json!({"root":std::env::temp_dir(),"computer":{}}),
            )
            .unwrap()
            .attach_scripted_desktop(1, 1)
        }),
    )
}
fn offered(app: &App) -> Vec<String> {
    let config: Value = serde_json::from_str(&app.selection().unwrap().engine_config).unwrap();
    config["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .filter(|n| n.starts_with("computer_"))
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn desktop_control_is_off_by_default_needs_vision_and_follows_the_switches() {
    let (app, _home, _) = desktop_app(json!({"enabled":false}), true);
    assert!(
        offered(&app).is_empty(),
        "off by default, even for a model that can see"
    );
    let got = call(&app, Some(&app.token), "GET", "/api/computer", json!({})).1;
    assert_eq!(
        (
            got["settings"]["enabled"].clone(),
            got["settings"]["allow_input"].clone(),
            got["model_has_vision"].clone()
        ),
        (json!(false), json!(false), json!(true))
    );
    // Screenshots only.
    call(
        &app,
        Some(&app.token),
        "PUT",
        "/api/computer",
        json!({"enabled":true}),
    );
    assert_eq!(offered(&app), ["computer_screenshot", "computer_wait"]);
    let config: Value = serde_json::from_str(&app.selection().unwrap().engine_config).unwrap();
    assert_eq!(
        config["policy"]["image_tools"],
        json!(["computer_screenshot"])
    );
    // With input.
    call(
        &app,
        Some(&app.token),
        "PUT",
        "/api/computer",
        json!({"enabled":true,"allow_input":true}),
    );
    assert_eq!(offered(&app).len(), 8);
    // A model that cannot see is not offered a loop that needs eyes.
    app.db.col_put("models", "openrouter/vendor/model", &json!({"id":"openrouter/vendor/model","provider_id":"openrouter","active":true,"enabled":true,"model_type":"llm"}), PutOptions::default()).unwrap();
    assert!(offered(&app).is_empty());
    let got = call(&app, Some(&app.token), "GET", "/api/computer", json!({})).1;
    assert_eq!(got["model_has_vision"], false);
    // Invalid settings are refused and change nothing.
    for bad in [
        json!({"surprise":1}),
        json!({"backend":"carrier-pigeon"}),
        json!({"max_dimension":10}),
        json!({"enabled":"yes"}),
        json!({"session_minutes":0}),
    ] {
        assert_eq!(
            call(&app, Some(&app.token), "PUT", "/api/computer", bad.clone()).0,
            400,
            "{bad}"
        );
    }
    assert_eq!(
        call(&app, Some(&app.token), "GET", "/api/computer", json!({})).1["settings"]["allow_input"],
        true
    );
    // Not for anyone without the administrator token. With a password configured the gateway is no
    // longer open, so a user's token is not an administrator's.
    app.db
        .col_put(
            "users",
            "u1",
            &json!({"id":"u1","name":"Uno","email":"uno@example.com","password_hash":"$2b$04$abcdefghijklmnopqrstuuxDugxrpcpW3csMl3JkYMLtX7yxJF.2."}),
            PutOptions::default(),
        )
        .unwrap();
    app.db
        .col_put(
            "users",
            "u2",
            &json!({"id":"u2","name":"Otra","email":null,"password_hash":null}),
            PutOptions::default(),
        )
        .unwrap();
    let user = issue(&app, "u2");
    for (method, path) in [
        ("GET", "/api/computer"),
        ("PUT", "/api/computer"),
        ("POST", "/api/computer/stop"),
        ("POST", "/api/computer/resume"),
    ] {
        assert_eq!(
            call(&app, Some(&user), method, path, json!({"enabled":true})).0,
            403,
            "{method} {path}"
        );
    }
    assert_eq!(
        call(&app, Some(&app.token), "GET", "/api/computer", json!({})).1["settings"]["allow_input"],
        true,
        "a user could not change it"
    );
}

#[tokio::test]
async fn a_turn_looks_then_acts_with_one_approval_for_the_conversation_and_sees_the_screenshot() {
    let (app, _home, desktop) = desktop_app(
        json!({"enabled":true,"allow_input":true,"max_dimension":960}),
        true,
    );
    let steps = vec![
        ("computer_screenshot", json!({})),
        ("computer_click", json!({"x":480,"y":270})),
        ("computer_click", json!({"x":10,"y":10})),
        ("computer_screenshot", json!({})),
    ];
    let (result, fixture, asked) = script_turn(&app, steps, vec![true]).await;
    result.unwrap();
    assert_eq!(
        asked, 1,
        "one approval covers both clicks (and none was needed for looking)"
    );
    let events = desktop.events();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.starts_with("button Left down"))
            .count(),
        2,
        "{events:?}"
    );
    assert_eq!(
        events[0], "move 961 541",
        "the click is mapped from the screenshot's space to the screen"
    );
    // After the first screenshot the model's next request carries it as an image, with a note.
    let seen = fixture.seen.lock().unwrap().clone();
    assert!(
        !seen[0].contains("Image returned"),
        "nothing before the first screenshot"
    );
    assert!(
        seen[1].contains("Image returned by the tool `computer_screenshot`")
            && seen[1].contains("media_type: \"image/"),
        "{}",
        &seen[1][seen[1].len().saturating_sub(600)..]
    );
    assert!(seen[1].contains("image_attached"));
    // The audit trail: asked once, then allowed on the strength of that approval.
    let audit = call(
        &app,
        Some(&app.token),
        "GET",
        "/api/tool-audit?limit=50",
        json!({}),
    )
    .1;
    let rows = audit
        .as_array()
        .cloned()
        .or_else(|| audit["entries"].as_array().cloned())
        .unwrap_or_default();
    let decisions = |tool: &str, decision: &str| {
        rows.iter()
            .filter(|r| r["tool"] == tool && r["decision"] == decision)
            .count()
    };
    assert_eq!(
        (
            decisions("computer_click", "ask_approved"),
            decisions("computer_click", "allow")
        ),
        (1, 1),
        "{rows:?}"
    );
}

#[tokio::test]
async fn a_refused_desktop_action_never_reaches_the_screen_and_the_model_is_told() {
    let (app, _home, desktop) = desktop_app(json!({"enabled":true,"allow_input":true}), true);
    let (result, fixture, asked) = script_turn(
        &app,
        vec![
            ("computer_click", json!({"x":5,"y":5})),
            ("computer_key", json!({"keys":"Return"})),
        ],
        vec![false, false],
    )
    .await;
    result.unwrap();
    assert_eq!(
        asked, 2,
        "a refusal grants nothing, so the next action asks again"
    );
    assert!(desktop.events().is_empty(), "{:?}", desktop.events());
    assert!(
        fixture
            .seen
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("POLICY_DENIED")
    );
}

#[tokio::test]
async fn a_deny_rule_beats_the_session_approval_and_nothing_is_asked() {
    let (app, _home, desktop) = desktop_app(json!({"enabled":true,"allow_input":true}), true);
    put_policy(
        &app,
        json!({"rules":[{"tools":["computer_type"],"decision":"deny","reason":"no se escribe"}]}),
        5000,
    );
    // Approve a click, which opens the conversation's grant; typing is still denied by the rule.
    let (result, fixture, asked) = script_turn(
        &app,
        vec![
            ("computer_click", json!({"x":5,"y":5})),
            ("computer_type", json!({"text":"hola"})),
        ],
        vec![true],
    )
    .await;
    result.unwrap();
    assert_eq!(asked, 1);
    assert!(
        desktop.events().iter().all(|e| !e.starts_with("key")),
        "{:?}",
        desktop.events()
    );
    assert!(
        fixture
            .seen
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("no se escribe")
    );
}

#[tokio::test]
async fn the_stop_switch_blocks_everything_ends_approvals_and_only_the_administrator_lifts_it() {
    let (app, _home, desktop) = desktop_app(json!({"enabled":true,"allow_input":true}), true);
    // One conversation, approved once.
    let thread = call(
        &app,
        Some(&app.token),
        "POST",
        "/api/conversations",
        json!({}),
    )
    .1["conversation"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (result, _, asked) = script_turn_in(
        &app,
        &thread,
        vec![("computer_click", json!({"x":1,"y":1}))],
        vec![true],
    )
    .await;
    result.unwrap();
    assert_eq!(asked, 1);
    assert!(app.computer_grant_live("u1", &thread));
    let stopped = call(
        &app,
        Some(&app.token),
        "POST",
        "/api/computer/stop",
        json!({}),
    )
    .1;
    assert_eq!(stopped["stopped"], true);
    assert!(
        !app.computer_grant_live("u1", &thread),
        "stopping ends the approvals given so far"
    );
    let before = desktop.events().len();
    // Even after approving again, the desktop does nothing while stopped, and the model hears why.
    let (result, fixture, _) = script_turn_in(
        &app,
        &thread,
        vec![
            ("computer_screenshot", json!({})),
            ("computer_click", json!({"x":1,"y":1})),
        ],
        vec![true],
    )
    .await;
    result.unwrap();
    assert_eq!(
        desktop.events().len(),
        before,
        "nothing was done while stopped"
    );
    assert!(
        fixture
            .seen
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("stopped")
    );
    let resumed = call(
        &app,
        Some(&app.token),
        "POST",
        "/api/computer/resume",
        json!({}),
    )
    .1;
    assert_eq!(resumed["stopped"], false);
    let (result, _, _) = script_turn_in(
        &app,
        &thread,
        vec![("computer_click", json!({"x":2,"y":2}))],
        vec![true],
    )
    .await;
    result.unwrap();
    assert!(desktop.events().len() > before);
    // The action budget is the administrator's to refill.
    let counted = call(&app, Some(&app.token), "GET", "/api/computer", json!({})).1["actions"]
        .as_u64()
        .unwrap();
    assert!(counted >= 1);
    assert_eq!(
        call(
            &app,
            Some(&app.token),
            "POST",
            "/api/computer/reset",
            json!({})
        )
        .1["actions"],
        0
    );
}

#[tokio::test]
async fn an_approval_expires_with_its_time_and_is_per_conversation() {
    let (app, _home, _) = desktop_app(
        json!({"enabled":true,"allow_input":true,"session_minutes":1}),
        true,
    );
    app.computer_grant("u1", "t1");
    assert!(app.computer_grant_live("u1", "t1"));
    assert!(
        !app.computer_grant_live("u1", "t2"),
        "another conversation was not approved"
    );
    assert!(!app.computer_grant_live("u2", "t1"), "nor another person");
    app.computer_grants.lock().unwrap().insert(
        "u1\u{0}t1".into(),
        std::time::Instant::now() - Duration::from_secs(1),
    );
    assert!(!app.computer_grant_live("u1", "t1"), "expired");
}

#[tokio::test]
async fn an_ollama_provider_uses_the_native_api_with_the_models_context_window() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
                    let body = r#"{"message":{"role":"assistant","content":"hola desde ollama"},"done":true,"done_reason":"stop","prompt_eval_count":4,"eval_count":3}"#;
                    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                    return (
                        header.lines().next().unwrap().to_owned(),
                        serde_json::from_slice::<Value>(&data[end + 4..end + 4 + length]).unwrap(),
                    );
                }
            }
        }
    });
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = App::open(home.path().to_owned()).unwrap();
    // The usual way to name a local Ollama is its OpenAI-compatible `/v1` address; the native API is at `/api`.
    for (id, doc) in [
        (
            "providers",
            json!({"id":"ollama","base_url":format!("http://127.0.0.1:{port}/v1"),"active":true,"enabled":true,"category":"llm"}),
        ),
        (
            "models",
            json!({"id":"llama3.2","provider_id":"ollama","active":true,"enabled":true,"model_type":"llm","context_window":32768}),
        ),
    ] {
        let key = doc["id"].as_str().unwrap().to_owned();
        app.db
            .col_put(id, &key, &doc, PutOptions::default())
            .unwrap();
    }
    app.db.col_put("agents", "a1", &json!({"id":"a1","user_id":"u1","name":"Hive","role":"coordinator","enabled":true,"provider_id":"ollama","model_id":"llama3.2","system_prompt":"x","max_iterations":8}), PutOptions::default()).unwrap();
    let selection = app.selection().unwrap();
    assert_eq!(
        (selection.provider_id.as_str(), selection.model_id.as_str()),
        ("ollama", "llama3.2")
    );
    let request: ModelRequest = serde_json::from_value(json!({"model":"llama3.2","messages":[{"role":"user","content":"Hola"}],"tools":[],"max_output_tokens":64})).unwrap();
    let events = selection.provider.start(request, false);
    let mut text = None;
    while let Some(event) = events.next().await.unwrap() {
        if let ModelEvent::Reply { reply } = event {
            text = reply.content;
        }
    }
    assert_eq!(text.as_deref(), Some("hola desde ollama"));
    let (line, body) = server.await.unwrap();
    assert!(line.starts_with("post /api/chat "), "{line}");
    assert_eq!(
        body["options"]["num_ctx"], 32768,
        "the model's context window is asked for explicitly"
    );
}

const TINY_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
fn tiny_png() -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(TINY_PNG)
        .unwrap()
}
fn make_vision(app: &App, vision: bool) {
    let mut row = json!({"id":"openrouter/vendor/model","provider_id":"openrouter","active":true,"enabled":true,"model_type":"llm"});
    if vision {
        row["capabilities"] = json!(["vision"]);
    }
    app.db
        .col_put(
            "models",
            "openrouter/vendor/model",
            &row,
            PutOptions::default(),
        )
        .unwrap();
}
/// The parts of the last user message a fake model was sent: `(texts, image data urls)`.
fn last_user_parts(body: &Value) -> (Vec<String>, Vec<String>) {
    let content = &body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .unwrap()["content"];
    match content {
        Value::String(text) => (vec![text.clone()], vec![]),
        parts => (
            parts
                .as_array()
                .unwrap()
                .iter()
                .filter(|p| p["type"] == "text")
                .filter_map(|p| p["text"].as_str().map(str::to_owned))
                .collect(),
            parts
                .as_array()
                .unwrap()
                .iter()
                .filter(|p| p["type"] == "image_url")
                .filter_map(|p| p["image_url"]["url"].as_str().map(str::to_owned))
                .collect(),
        ),
    }
}

#[tokio::test]
async fn slack_pictures_reach_a_model_that_can_see_and_only_real_pictures_from_slack_are_fetched() {
    let (llm_url, llm_seen, llm) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    make_vision(&app, true);
    let slack = fake_slack().await;
    slack
        .files
        .lock()
        .unwrap()
        .insert("ok.png".into(), ("image/png".into(), tiny_png()));
    slack.files.lock().unwrap().insert(
        "login".into(),
        ("text/html".into(), b"<html>sign in</html>".to_vec()),
    );
    slack.files.lock().unwrap().insert(
        "liar.png".into(),
        ("image/png".into(), b"not really a png".to_vec()),
    );
    add_channel(&app, "slack", "sl1", slack.config(&["D0123"]));
    let supervisor = tokio::spawn(crate::slack::run(app.clone()));
    eventually("connected", || {
        slack.connections.load(AtomicOrdering::SeqCst) == 1
    })
    .await;
    let file = |mime: &str, url: String, size: u64| json!({"mimetype":mime,"size":size,"url_private":url,"name":"foto.png"});
    // One real picture among files that must not be used: another host, an HTML login page, bytes
    // that are not an image, a PDF, an oversized declaration.
    slack.event("e1", "Ev1", json!({"type":"message","subtype":"file_share","channel":"D0123","user":"U1","text":"¿qué ves?","ts":"1","files":[
        file("image/png", format!("{}/files/ok.png", slack.api), 70),
        file("image/png", "https://evil.example/steal.png".into(), 70),
        file("image/png", format!("{}/files/login", slack.api), 70),
        file("image/png", format!("{}/files/liar.png", slack.api), 70),
        file("application/pdf", format!("{}/files/doc.pdf", slack.api), 70),
        file("image/png", format!("{}/files/big.png", slack.api), 99_000_000),
    ]}));
    eventually("answered", || slack.posts.lock().unwrap().len() == 1).await;
    let (texts, images) = last_user_parts(llm_seen.lock().unwrap().last().unwrap());
    assert_eq!(texts, ["¿qué ves?"]);
    assert_eq!(
        images,
        [format!("data:image/png;base64,{TINY_PNG}")],
        "exactly the one real picture"
    );
    let auth = slack.file_auth.lock().unwrap().clone();
    assert!(
        auth.contains(&"ok.png:Bearer xoxb-SECRETBOT".to_owned()),
        "{auth:?}"
    );
    assert!(
        !auth
            .iter()
            .any(|a| a.starts_with("doc.pdf") || a.starts_with("big.png")),
        "never fetched: {auth:?}"
    );
    // Another kind of message event (an edit) is not a message from a person.
    slack.event("e2", "Ev2", json!({"type":"message","subtype":"message_changed","channel":"D0123","user":"U1","text":"x","files":[file("image/png", format!("{}/files/ok.png", slack.api), 70)]}));
    // A picture alone (no words) is answered with a default question.
    slack.event("e3", "Ev3", json!({"type":"message","subtype":"file_share","channel":"D0123","user":"U1","text":"","ts":"1","files":[file("image/png", format!("{}/files/ok.png", slack.api), 70)]}));
    eventually("the picture alone is answered", || {
        slack.posts.lock().unwrap().len() == 2
    })
    .await;
    let (texts, images) = last_user_parts(llm_seen.lock().unwrap().last().unwrap());
    assert_eq!(
        (texts, images.len()),
        (vec!["Mira la imagen adjunta.".to_owned()], 1)
    );
    // Files with nothing usable and no words: told so, and the model is not asked at all.
    let asked = llm_seen.lock().unwrap().len();
    slack.event("e4", "Ev4", json!({"type":"message","subtype":"file_share","channel":"D0123","user":"U1","text":"","ts":"1","files":[file("application/pdf", format!("{}/files/doc.pdf", slack.api), 70)]}));
    eventually("told so", || {
        slack
            .posted_texts()
            .last()
            .is_some_and(|t| t.contains("Solo puedo leer imágenes"))
    })
    .await;
    assert_eq!(llm_seen.lock().unwrap().len(), asked, "no model call");
    // A model that cannot see is not sent the picture, and is told there was one.
    make_vision(&app, false);
    slack.event("e5", "Ev5", json!({"type":"message","subtype":"file_share","channel":"D0123","user":"U1","text":"mira esto","ts":"1","files":[file("image/png", format!("{}/files/ok.png", slack.api), 70)]}));
    eventually("answered without sight", || {
        slack.posts.lock().unwrap().len() == 4
    })
    .await;
    let (texts, images) = last_user_parts(llm_seen.lock().unwrap().last().unwrap());
    assert!(
        images.is_empty() && texts.join(" ").contains("no puede verlas"),
        "{texts:?}"
    );
    // Nothing is left waiting for a turn that never comes.
    assert!(app.turn_attachments.lock().unwrap().is_empty());
    app.shutdown.cancel();
    let _ = supervisor.await;
    llm.abort();
}

fn slack_signature(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let mut signed = format!("v0:{timestamp}:").into_bytes();
    signed.extend_from_slice(body);
    format!(
        "v0={}",
        hex::encode(
            ring::hmac::sign(
                &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes()),
                &signed
            )
            .as_ref()
        )
    )
}
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}
async fn slack_webhook(
    base: &str,
    channel: &str,
    body: &[u8],
    content_type: &str,
    timestamp: i64,
    signature: Option<String>,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{base}/webhook/slack/{channel}"))
        .header("Content-Type", content_type)
        .header("X-Slack-Request-Timestamp", timestamp.to_string())
        .body(body.to_vec());
    if let Some(signature) = signature {
        request = request.header("X-Slack-Signature", signature);
    }
    request.send().await.unwrap()
}

#[tokio::test]
async fn the_slack_webhook_verifies_its_signature_answers_events_and_commands_and_never_twice() {
    let (llm_url, llm_seen, llm) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    // Credentials on, so every ordinary route needs the bearer token.
    app.db.col_put("users", "u1", &json!({"id":"u1","name":"Uno","email":"uno@example.com","password_hash":"$2b$04$abcdefghijklmnopqrstuuxDugxrpcpW3csMl3JkYMLtX7yxJF.2."}), PutOptions::default()).unwrap();
    make_vision(&app, true);
    let slack = fake_slack().await;
    slack
        .files
        .lock()
        .unwrap()
        .insert("ok.png".into(), ("image/png".into(), tiny_png()));
    let mut config = slack.config(&["D0123"]);
    config["mode"] = json!("webhook");
    config["signing_secret"] = json!("signing-secret-test");
    config.as_object_mut().unwrap().remove("app_token");
    add_channel(&app, "slack", "sl1", config);
    let supervisor = tokio::spawn(crate::slack::run(app.clone()));
    eventually("listening", || {
        status_of(&app, "sl1")["status"] == "listening"
    })
    .await;
    assert_eq!(
        slack.connections.load(AtomicOrdering::SeqCst),
        0,
        "webhook mode opens no socket"
    );
    let (base, server) = serve(app.clone()).await;
    let secret = "signing-secret-test";
    let event = |id: &str, event: Value| {
        json!({"type":"event_callback","event_id":id,"event":event})
            .to_string()
            .into_bytes()
    };
    let message =
        |text: &str| json!({"type":"message","channel":"D0123","user":"U1","text":text,"ts":"1"});
    let now = unix_now();

    // Slack's URL verification handshake answers with the challenge.
    let body = json!({"type":"url_verification","challenge":"abc123"})
        .to_string()
        .into_bytes();
    let handshake = slack_webhook(
        &base,
        "sl1",
        &body,
        "application/json",
        now,
        Some(slack_signature(secret, now, &body)),
    )
    .await;
    assert_eq!(handshake.status(), 200);
    assert_eq!(
        handshake.json::<Value>().await.unwrap()["challenge"],
        "abc123"
    );

    // Unsigned, wrongly signed, signed for another body, signed with a stale timestamp, with a
    // garbled signature: all 401, and nothing reaches the model.
    let good = event("EvA", message("hola"));
    for (timestamp, signature, why) in [
        (now, None, "unsigned"),
        (
            now,
            Some(slack_signature("other-secret", now, &good)),
            "wrong secret",
        ),
        (
            now,
            Some(slack_signature(secret, now, b"another body")),
            "another body",
        ),
        (
            now - 3600,
            Some(slack_signature(secret, now - 3600, &good)),
            "a replay from an hour ago",
        ),
        (now, Some("v0=zz".to_owned()), "garbled"),
        (
            now,
            Some(slack_signature(secret, now, &good).replace("v0=", "v1=")),
            "wrong version",
        ),
    ] {
        let response = slack_webhook(
            &base,
            "sl1",
            &good,
            "application/json",
            timestamp,
            signature,
        )
        .await;
        assert_eq!(response.status(), 401, "{why}");
    }
    assert!(llm_seen.lock().unwrap().is_empty());

    // A signed message from an allowed channel is answered with the bot token, in that channel.
    let sent = slack_webhook(
        &base,
        "sl1",
        &good,
        "application/json",
        now,
        Some(slack_signature(secret, now, &good)),
    )
    .await;
    assert_eq!(sent.status(), 200);
    eventually("answered", || slack.posts.lock().unwrap().len() == 1).await;
    {
        let posts = slack.posts.lock().unwrap();
        assert_eq!(
            (
                posts[0].0["channel"].clone(),
                posts[0].0["text"].clone(),
                posts[0].1.clone()
            ),
            (
                json!("D0123"),
                json!("Hola Rust"),
                "Bearer xoxb-SECRETBOT".to_owned()
            )
        );
    }
    // The same event again (Slack redelivers when it thinks one failed) does nothing.
    for _ in 0..2 {
        let again = slack_webhook(
            &base,
            "sl1",
            &good,
            "application/json",
            now,
            Some(slack_signature(secret, now, &good)),
        )
        .await;
        assert_eq!(again.status(), 200);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        (
            slack.posts.lock().unwrap().len(),
            llm_seen.lock().unwrap().len()
        ),
        (1, 1)
    );
    // And it is remembered on disk, so a restart would not run it again either.
    let remembered = app
        .db
        .col_get("channelState", "slack:sl1")
        .unwrap()
        .unwrap()
        .doc;
    assert_eq!(remembered["seen"], json!(["EvA"]));

    // A stranger's channel, a bot, and a message edit are ignored.
    for (id, ev) in [
        (
            "EvB",
            json!({"type":"message","channel":"C999","user":"U1","text":"intruso","ts":"1"}),
        ),
        (
            "EvC",
            json!({"type":"message","channel":"D0123","bot_id":"B1","text":"soy bot","ts":"1"}),
        ),
        (
            "EvD",
            json!({"type":"message","subtype":"message_changed","channel":"D0123","user":"U1","text":"editado","ts":"1"}),
        ),
    ] {
        let body = event(id, ev);
        assert_eq!(
            slack_webhook(
                &base,
                "sl1",
                &body,
                "application/json",
                now,
                Some(slack_signature(secret, now, &body))
            )
            .await
            .status(),
            200
        );
    }
    // A picture arrives through the webhook the same way as through Socket Mode.
    let body = event(
        "EvE",
        json!({"type":"message","subtype":"file_share","channel":"D0123","user":"U1","text":"mira","ts":"1","files":[{"mimetype":"image/png","size":70,"url_private":format!("{}/files/ok.png", slack.api),"name":"a.png"}]}),
    );
    slack_webhook(
        &base,
        "sl1",
        &body,
        "application/json",
        now,
        Some(slack_signature(secret, now, &body)),
    )
    .await;
    eventually("the picture is answered", || {
        slack.posts.lock().unwrap().len() == 2
    })
    .await;
    assert_eq!(
        last_user_parts(llm_seen.lock().unwrap().last().unwrap())
            .1
            .len(),
        1
    );
    assert_eq!(
        llm_seen.lock().unwrap().len(),
        2,
        "the stranger, the bot and the edit never reached the model"
    );

    // A slash command arrives form-encoded and is answered through its response_url.
    let form = format!(
        "command=%2Fhive&text=hola+comando&channel_id=D0123&response_url={}",
        format!("{}/response/r1", slack.api)
            .replace(':', "%3A")
            .replace('/', "%2F")
    );
    let sent = slack_webhook(
        &base,
        "sl1",
        form.as_bytes(),
        "application/x-www-form-urlencoded",
        now,
        Some(slack_signature(secret, now, form.as_bytes())),
    )
    .await;
    assert_eq!(sent.status(), 200);
    eventually("the command is answered", || {
        slack
            .responses
            .lock()
            .unwrap()
            .iter()
            .any(|(name, _)| name == "r1")
    })
    .await;
    let forged = slack_webhook(
        &base,
        "sl1",
        form.as_bytes(),
        "application/x-www-form-urlencoded",
        now,
        Some(slack_signature("nope", now, form.as_bytes())),
    )
    .await;
    assert_eq!(forged.status(), 401);

    // Unknown channels, other kinds of channel, and GET are not served.
    for channel in ["nope", "wa1"] {
        let response = slack_webhook(
            &base,
            channel,
            &good,
            "application/json",
            now,
            Some(slack_signature(secret, now, &good)),
        )
        .await;
        assert_eq!(response.status(), 404, "{channel}");
    }
    assert_eq!(
        reqwest::get(format!("{base}/webhook/slack/sl1"))
            .await
            .unwrap()
            .status(),
        405
    );
    // The rest of the gateway still needs the bearer token.
    assert_eq!(
        reqwest::get(format!("{base}/api/providers"))
            .await
            .unwrap()
            .status(),
        401
    );
    // The status says where Slack should call, and never a secret.
    let status = status_of(&app, "sl1").to_string();
    assert!(
        !status.contains("signing-secret") && !status.contains("xoxb"),
        "{status}"
    );

    server.abort();
    app.shutdown.cancel();
    let _ = supervisor.await;
    llm.abort();
}

#[tokio::test]
async fn slack_approval_buttons_work_through_the_signed_webhook_too() {
    for (action, ran) in [("hive_approve", true), ("hive_deny", false)] {
        let (llm_url, llm) = tool_provider().await;
        let home = tempfile::tempdir().unwrap();
        seed(home.path(), &llm_url);
        let app = Arc::new(App::open(home.path().to_owned()).unwrap());
        app.db.col_put("users", "u1", &json!({"id":"u1","name":"Uno","email":"uno@example.com","password_hash":"$2b$04$abcdefghijklmnopqrstuuxDugxrpcpW3csMl3JkYMLtX7yxJF.2."}), PutOptions::default()).unwrap();
        put_policy(
            &app,
            json!({"rules":[{"tools":["a2ui_create_surface"],"decision":"ask","reason":"crea una interfaz"}]}),
            30_000,
        );
        let slack = fake_slack().await;
        let mut config = slack.config(&["D0123"]);
        config["mode"] = json!("webhook");
        config["signing_secret"] = json!("signing-secret-test");
        config.as_object_mut().unwrap().remove("app_token");
        add_channel(&app, "slack", "sl1", config);
        let supervisor = tokio::spawn(crate::slack::run(app.clone()));
        eventually("listening", || {
            status_of(&app, "sl1")["status"] == "listening"
        })
        .await;
        let (base, server) = serve(app.clone()).await;
        let (secret, now) = ("signing-secret-test", unix_now());
        let event = json!({"type":"event_callback","event_id":"EvQ","event":{"type":"message","channel":"D0123","user":"U1","text":"crea el formulario","ts":"1"}}).to_string().into_bytes();
        slack_webhook(
            &base,
            "sl1",
            &event,
            "application/json",
            now,
            Some(slack_signature(secret, now, &event)),
        )
        .await;
        eventually("the question with buttons", || {
            slack
                .posts
                .lock()
                .unwrap()
                .iter()
                .any(|(body, _)| body.get("blocks").is_some())
        })
        .await;
        let question = slack
            .posts
            .lock()
            .unwrap()
            .iter()
            .find(|(body, _)| body.get("blocks").is_some())
            .unwrap()
            .0
            .clone();
        let id = question["blocks"][1]["elements"][0]["value"]
            .as_str()
            .unwrap()
            .to_owned();
        // A button press arrives form-encoded as `payload=<json>`.
        let press = |value: &str, channel: &str, action: &str, name: &str| {
            let payload = json!({"type":"block_actions","channel":{"id":channel},"user":{"id":"U1"},
                "actions":[{"action_id":action,"value":value}],"response_url":format!("{}/response/{name}", slack.api)}).to_string();
            let encoded: String = payload
                .bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect();
            format!("payload={encoded}").into_bytes()
        };
        // Unsigned or wrongly signed presses decide nothing; neither does a press of another question,
        // another channel, or another action.
        let forged = press(&id, "D0123", action, "forged");
        assert_eq!(
            slack_webhook(
                &base,
                "sl1",
                &forged,
                "application/x-www-form-urlencoded",
                now,
                None
            )
            .await
            .status(),
            401
        );
        assert_eq!(
            slack_webhook(
                &base,
                "sl1",
                &forged,
                "application/x-www-form-urlencoded",
                now,
                Some(slack_signature("nope", now, &forged))
            )
            .await
            .status(),
            401
        );
        for (value, channel, act, name) in [
            ("not-this-one", "D0123", action, "old"),
            (id.as_str(), "C9999", action, "stranger"),
            (id.as_str(), "D0123", "something_else", "odd"),
        ] {
            let body = press(value, channel, act, name);
            assert_eq!(
                slack_webhook(
                    &base,
                    "sl1",
                    &body,
                    "application/x-www-form-urlencoded",
                    now,
                    Some(slack_signature(secret, now, &body))
                )
                .await
                .status(),
                200
            );
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !surface_exists(&app) && app.approvals.lock().unwrap().len() == 1,
            "still waiting"
        );
        assert!(slack.responses.lock().unwrap().is_empty());
        // The right press decides, the message loses its buttons, and the turn goes on.
        let body = press(&id, "D0123", action, "done");
        assert_eq!(
            slack_webhook(
                &base,
                "sl1",
                &body,
                "application/x-www-form-urlencoded",
                now,
                Some(slack_signature(secret, now, &body))
            )
            .await
            .status(),
            200
        );
        eventually("the turn finishes", || {
            slack.posted_texts().iter().any(|t| t == "Listo")
        })
        .await;
        assert_eq!(surface_exists(&app), ran, "{action}");
        let replaced = slack
            .responses
            .lock()
            .unwrap()
            .iter()
            .find(|(name, _)| name == "done")
            .unwrap()
            .1
            .clone();
        assert_eq!(
            (
                replaced["replace_original"].clone(),
                replaced["text"].clone()
            ),
            (
                json!(true),
                json!(if ran {
                    "Aprobado. Continúo."
                } else {
                    "Denegado."
                })
            )
        );
        // Pressing it again changes nothing.
        let again = press(&id, "D0123", action, "again");
        slack_webhook(
            &base,
            "sl1",
            &again,
            "application/x-www-form-urlencoded",
            now,
            Some(slack_signature(secret, now, &again)),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            slack
                .responses
                .lock()
                .unwrap()
                .iter()
                .all(|(name, _)| name != "again")
        );
        assert!(
            app.approvals.lock().unwrap().is_empty()
                && app.chat_approvals.lock().unwrap().is_empty()
        );
        server.abort();
        app.shutdown.cancel();
        let _ = supervisor.await;
        llm.abort();
    }
}

#[tokio::test]
async fn a_slack_webhook_channel_needs_its_signing_secret_and_a_socket_channel_its_app_token() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let slack = fake_slack().await;
    let base = |extra: Value| {
        let mut config = slack.config(&["D0123"]);
        config.as_object_mut().unwrap().remove("app_token");
        for (key, value) in extra.as_object().unwrap() {
            config[key] = value.clone();
        }
        config
    };
    add_channel(&app, "slack", "bad1", base(json!({"mode":"webhook"})));
    add_channel(
        &app,
        "slack",
        "bad2",
        base(json!({"mode":"webhook","signing_secret":"short"})),
    );
    add_channel(
        &app,
        "slack",
        "bad3",
        base(json!({"mode":"carrier-pigeon","signing_secret":"signing-secret-test"})),
    );
    add_channel(&app, "slack", "bad4", base(json!({})));
    let supervisor = tokio::spawn(crate::slack::run(app.clone()));
    for id in ["bad1", "bad2", "bad3", "bad4"] {
        eventually("refused", || status_of(&app, id)["status"] == "error").await;
        let status = status_of(&app, id).to_string();
        assert!(
            !status.contains("signing-secret") && !status.contains("SECRET"),
            "{id}: {status}"
        );
    }
    let (base_url, server) = serve(app.clone()).await;
    let now = unix_now();
    let body = b"{}";
    for id in ["bad1", "bad2"] {
        let response = slack_webhook(
            &base_url,
            id,
            body,
            "application/json",
            now,
            Some(slack_signature("signing-secret-test", now, body)),
        )
        .await;
        assert_eq!(
            response.status(),
            404,
            "{id}: a misconfigured channel is a plain 404"
        );
    }
    server.abort();
    app.shutdown.cancel();
    let _ = supervisor.await;
}

/// A fake decision oracle: answers request `n` with `replies[n]` (the last repeats), or HTTP 500.
struct FakeOracle {
    url: String,
    seen: Arc<Mutex<Vec<(String, Value)>>>,
    down: Arc<std::sync::atomic::AtomicBool>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for FakeOracle {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn oracle_says(choice: &str, confidence: f64, supported: f64) -> Value {
    json!({"answers":{"next":{"choice":choice,"confidence":confidence},"supported":{"noul":supported}},"usage":{"input_tokens":60,"output_tokens":4}})
}
async fn fake_oracle(replies: Vec<Value>) -> FakeOracle {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/decide", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let down = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (record, is_down) = (seen.clone(), down.clone());
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (record, is_down, replies) = (record.clone(), is_down.clone(), replies.clone());
            tokio::spawn(async move {
                let Ok(request) = read_request(&mut socket).await else {
                    return;
                };
                let auth = request
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                let body: Value = serde_json::from_slice(request.body()).unwrap_or(Value::Null);
                let index = {
                    let mut seen = record.lock().unwrap();
                    seen.push((auth, body));
                    seen.len() - 1
                };
                if is_down.load(AtomicOrdering::SeqCst) {
                    let _ = respond(&mut socket, 500, json!({"error":"caído"})).await;
                    return;
                }
                let reply = replies
                    .get(index)
                    .or(replies.last())
                    .cloned()
                    .unwrap_or(Value::Null);
                let _ = respond(&mut socket, 200, reply).await;
            });
        }
    });
    FakeOracle {
        url,
        seen,
        down,
        task,
    }
}
fn enable_oracle(app: &App, oracle: &FakeOracle, extra: Value) {
    let mut settings = json!({"enabled":true,"endpoint":oracle.url,"model":"jev","api_key":"oracle-key-123","max_corrections":1});
    for (key, value) in extra.as_object().unwrap() {
        settings[key] = value.clone();
    }
    let (status, body) = call(app, Some(&app.token), "PUT", "/api/oracle", settings);
    assert_eq!(status, 200, "{body}");
}
/// What the person would read in the conversation: the assistant's messages.
fn assistant_messages(app: &App, thread: &str) -> Vec<String> {
    crate::conversations::visible_messages(app, thread, 40)
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "assistant")
        .filter_map(|m| m["content"].as_str().map(str::to_owned))
        .collect()
}
fn new_thread(app: &App) -> String {
    call(
        app,
        Some(&app.token),
        "POST",
        "/api/conversations",
        json!({}),
    )
    .1["conversation"]["id"]
        .as_str()
        .unwrap()
        .to_owned()
}
/// Model calls charged so far to the turns' Objectives (the newest one).
fn charged_model_calls(app: &App) -> u64 {
    config::docs(&app.db, "hive_sdk_rust_objectives_v1")
        .unwrap()
        .iter()
        .filter_map(|d| d["ledger"]["model_calls"].as_u64())
        .next_back()
        .unwrap_or(0)
}

#[tokio::test]
async fn the_oracle_is_off_by_default_its_settings_are_validated_and_its_key_is_never_returned() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let got = call(&app, Some(&app.token), "GET", "/api/oracle", json!({})).1;
    assert_eq!(
        (
            got["settings"]["enabled"].clone(),
            got["has_api_key"].clone()
        ),
        (json!(false), json!(false))
    );
    assert_eq!(app.oracle_corrections(), 0);
    assert!(app.oracle_client().unwrap().is_none());
    for bad in [
        json!({"enabled":true}),
        json!({"enabled":true,"endpoint":"http://x/y"}),
        json!({"enabled":true,"model":"jev"}),
        json!({"endpoint":"ftp://x/y","model":"jev"}),
        json!({"endpoint":"http://u:p@x/y","model":"jev"}),
        json!({"max_corrections":0}),
        json!({"max_corrections":9}),
        json!({"min_confidence":0.1}),
        json!({"timeout_ms":5}),
        json!({"surprise":1}),
        json!({"api_key":"has spaces"}),
        json!({"api_key":""}),
        json!({"enabled":"yes"}),
    ] {
        assert_eq!(
            call(&app, Some(&app.token), "PUT", "/api/oracle", bad.clone()).0,
            400,
            "{bad}"
        );
    }
    assert!(
        !call(&app, Some(&app.token), "GET", "/api/oracle", json!({})).1["settings"]["enabled"]
            .as_bool()
            .unwrap(),
        "refused settings change nothing"
    );
    let oracle = fake_oracle(vec![oracle_says("finish", 0.9, 0.9)]).await;
    enable_oracle(&app, &oracle, json!({}));
    let got = call(&app, Some(&app.token), "GET", "/api/oracle", json!({}));
    assert_eq!(got.0, 200);
    assert_eq!(
        (
            got.1["settings"]["enabled"].clone(),
            got.1["has_api_key"].clone(),
            app.oracle_corrections()
        ),
        (json!(true), json!(true), 1)
    );
    assert!(
        !got.1.to_string().contains("oracle-key-123"),
        "the key is a secret: {}",
        got.1
    );
    assert!(
        app.db
            .col_get("oracle", "settings")
            .unwrap()
            .unwrap()
            .doc
            .to_string()
            .find("oracle-key-123")
            .is_none(),
        "nor is it stored with the settings"
    );
    // Only administrators.
    app.db.col_put("users", "u1", &json!({"id":"u1","name":"Uno","email":"uno@example.com","password_hash":"$2b$04$abcdefghijklmnopqrstuuxDugxrpcpW3csMl3JkYMLtX7yxJF.2."}), PutOptions::default()).unwrap();
    app.db
        .col_put(
            "users",
            "u2",
            &json!({"id":"u2","name":"Dos","email":null,"password_hash":null}),
            PutOptions::default(),
        )
        .unwrap();
    let user = issue(&app, "u2");
    for method in ["GET", "PUT"] {
        assert_eq!(
            call(
                &app,
                Some(&user),
                method,
                "/api/oracle",
                json!({"enabled":false})
            )
            .0,
            403,
            "{method}"
        );
    }
    // Turned off again, nothing is asked.
    call(
        &app,
        Some(&app.token),
        "PUT",
        "/api/oracle",
        json!({"enabled":false}),
    );
    assert_eq!(app.oracle_corrections(), 0);
    assert!(app.oracle_client().unwrap().is_none());
}

#[tokio::test]
async fn a_confident_no_from_the_oracle_sends_the_answer_back_once_and_only_the_corrected_one_is_delivered()
 {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let oracle = fake_oracle(vec![oracle_says("continue", 0.9, 0.2)]).await;
    enable_oracle(&app, &oracle, json!({}));
    let thread = new_thread(&app);
    let fixture = ScriptFixture::new(vec![]).saying(vec!["borrador", "corregida"]);
    let (result, fixture, _) = script_turn_with(&app, &thread, fixture, vec![]).await;
    result.unwrap();
    // The person sees one answer, the corrected one; the draft was never delivered.
    assert_eq!(assistant_messages(&app, &thread), ["corregida"]);
    // One oracle call, with the key, about the draft; after a correction the answer is accepted without asking again.
    let asked = oracle.seen.lock().unwrap().clone();
    assert_eq!(
        asked.len(),
        1,
        "max_corrections is 1, so the corrected answer is not reviewed again"
    );
    assert_eq!(asked[0].0, "Bearer oracle-key-123");
    assert_eq!(
        (
            asked[0].1["model"].clone(),
            asked[0].1["state"]["answer"].clone(),
            asked[0].1["state"]["request"].clone()
        ),
        (json!("jev"), json!("borrador"), json!("Haz algo"))
    );
    assert_eq!(asked[0].1["questions"]["next"]["type"], "choice");
    assert_eq!(asked[0].1["questions"]["supported"]["type"], "noul");
    // The model was told why and asked to correct; it saw its own draft in the history.
    let seen = fixture.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert!(
        seen[1].contains("Automatic review")
            && seen[1].contains("incomplete")
            && seen[1].contains("backed"),
        "{}",
        &seen[1][seen[1].len().saturating_sub(700)..]
    );
    assert!(seen[1].contains("borrador"));
    // Two model calls and the oracle's one are all on the turn's Objective.
    assert_eq!(charged_model_calls(&app), 3);
}

#[tokio::test]
async fn a_confident_yes_or_a_silent_oracle_never_delays_the_answer() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let oracle = fake_oracle(vec![oracle_says("finish", 0.95, 0.9)]).await;
    enable_oracle(&app, &oracle, json!({}));
    let thread = new_thread(&app);
    let (result, fixture, _) = script_turn_with(
        &app,
        &thread,
        ScriptFixture::new(vec![]).saying(vec!["respuesta"]),
        vec![],
    )
    .await;
    result.unwrap();
    assert_eq!(assistant_messages(&app, &thread), ["respuesta"]);
    assert_eq!(
        (
            oracle.seen.lock().unwrap().len(),
            fixture.seen.lock().unwrap().len()
        ),
        (1, 1),
        "asked once, one model call"
    );
    // An oracle that is down: the answer still goes out, every time, and after three failures it is not even tried.
    oracle.down.store(true, AtomicOrdering::SeqCst);
    for round in 0..5 {
        let thread = new_thread(&app);
        let (result, _, _) = script_turn_with(
            &app,
            &thread,
            ScriptFixture::new(vec![]).saying(vec!["igual"]),
            vec![],
        )
        .await;
        result.unwrap();
        assert_eq!(
            assistant_messages(&app, &thread),
            ["igual"],
            "round {round}"
        );
    }
    assert_eq!(
        oracle.seen.lock().unwrap().len(),
        1 + 3,
        "three failures open the cooldown, then nothing is sent"
    );
}

#[tokio::test]
async fn an_oracle_whose_advice_keeps_changing_nothing_is_set_aside() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let oracle = fake_oracle(vec![oracle_says("continue", 0.95, 0.9)]).await;
    enable_oracle(&app, &oracle, json!({}));
    // The model gives the same answer after each correction: the oracle was not needed.
    for round in 0..3 {
        let thread = new_thread(&app);
        let (result, _, _) = script_turn_with(
            &app,
            &thread,
            ScriptFixture::new(vec![]).saying(vec!["lo mismo", "lo mismo"]),
            vec![],
        )
        .await;
        result.unwrap();
        assert_eq!(
            assistant_messages(&app, &thread),
            ["lo mismo"],
            "round {round}"
        );
    }
    assert_eq!(oracle.seen.lock().unwrap().len(), 3);
    let (client, _) = app.oracle_client().unwrap().unwrap();
    assert!(
        client.status().unwrap().cooling_down,
        "three overruled decisions set it aside"
    );
    // Set aside: the next turn is not asked and costs nothing extra.
    let thread = new_thread(&app);
    let (result, fixture, _) = script_turn_with(
        &app,
        &thread,
        ScriptFixture::new(vec![]).saying(vec!["directa"]),
        vec![],
    )
    .await;
    result.unwrap();
    assert_eq!(
        (
            assistant_messages(&app, &thread),
            oracle.seen.lock().unwrap().len(),
            fixture.seen.lock().unwrap().len()
        ),
        (vec!["directa".to_owned()], 3, 1)
    );
}

#[tokio::test]
async fn the_oracle_is_told_what_the_tools_returned() {
    crate::mcp_servers::speed_up(100, 300);
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let mcp = fake_mcp(false).await;
    put_mcp(
        &app,
        "ok",
        json!({"enabled":true,"transport":"sse","url":mcp.url}),
    );
    let supervisor = tokio::spawn(crate::mcp_servers::run(app.clone()));
    eventually("connected", || mcp_row(&app, "ok")["status"] == "connected").await;
    let oracle = fake_oracle(vec![oracle_says("finish", 0.9, 0.9)]).await;
    enable_oracle(&app, &oracle, json!({}));
    let thread = new_thread(&app);
    let fixture =
        ScriptFixture::new(vec![("mcp_ok__echo", json!({"value":"hola"}))]).saying(vec!["hecho"]);
    let (result, _, _) = script_turn_with(&app, &thread, fixture, vec![]).await;
    result.unwrap();
    let asked = oracle.seen.lock().unwrap().clone();
    let results = asked[0].1["state"]["tool_results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(
        (results[0]["tool"].clone(), results[0]["ok"].clone()),
        (json!("mcp_ok__echo"), json!(true))
    );
    assert!(
        results[0]["excerpt"].as_str().unwrap().contains("hola"),
        "{results:?}"
    );
    app.shutdown.cancel();
    let _ = supervisor.await;
}

#[test]
fn a_turn_that_may_be_corrected_gets_budget_for_its_corrections_and_the_oracles_calls() {
    let base = config::turn_policy(&json!({"max_iterations":4}), 2).unwrap();
    let same = crate::oracle::with_review_budget(base.clone(), 0);
    assert_eq!(
        (same.max_model_calls, same.max_tool_calls),
        (base.max_model_calls, base.max_tool_calls)
    );
    let more = crate::oracle::with_review_budget(base.clone(), 2);
    assert_eq!(
        more.max_model_calls,
        base.max_model_calls * 3 + 3,
        "three rounds of model calls plus one oracle call each"
    );
    assert_eq!(more.max_tool_calls, base.max_tool_calls * 3);
    assert!(more.max_total_output_tokens > base.max_total_output_tokens);
}

#[tokio::test]
async fn telegram_pictures_reach_a_model_that_can_see_and_only_real_pictures_of_the_right_size_are_used()
 {
    let (llm_url, llm_seen, llm) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    make_vision(&app, true);
    let telegram = fake_telegram().await;
    telegram.file(
        "small",
        "photos/s.jpg",
        "image/jpeg",
        b"not an image at all".to_vec(),
    );
    telegram.file("big", "photos/b.png", "image/png", tiny_png());
    telegram.file(
        "login",
        "photos/x.png",
        "text/html",
        b"<html>sign in</html>".to_vec(),
    );
    telegram.file("doc", "documents/d.png", "image/png", tiny_png());
    telegram.file("evil", "../../etc/passwd", "image/png", tiny_png());
    add_telegram(
        &app,
        "tg1",
        "u1",
        json!({"bot_token":"123:SECRET","allowed_chat_ids":[42],"api_base":telegram.base,"poll_timeout_s":1}),
    );
    let supervisor = tokio::spawn(crate::telegram::run(app.clone()));
    // A photo comes in several sizes: the largest that fits is the one used, with its caption.
    telegram.push_media(
        10,
        42,
        json!({"caption":"¿qué ves?","photo":[
        {"file_id":"small","file_size":900,"width":90,"height":90},
        {"file_id":"big","file_size":4000,"width":800,"height":800},
        {"file_id":"huge","file_size":99_000_000,"width":8000,"height":8000}]}),
    );
    eventually("answered", || telegram.sent().len() == 1).await;
    let (texts, images) = last_user_parts(llm_seen.lock().unwrap().last().unwrap());
    assert_eq!(texts, ["¿qué ves?"]);
    assert_eq!(
        images,
        [format!("data:image/png;base64,{TINY_PNG}")],
        "the largest size that fits, and not the huge one"
    );
    // The token is in the file URL (Telegram's design); no redirect is followed and nothing else is sent to it.
    assert_eq!(
        telegram.store.requests(),
        ["photos/b.png:token-in-url=true"]
    );
    // A picture sent as a file (document) is read too, and a picture alone gets a default question.
    telegram.push_media(11, 42, json!({"document":{"file_id":"doc","file_name":"foto.png","mime_type":"image/png","file_size":70}}));
    eventually("the document is answered", || telegram.sent().len() == 2).await;
    let (texts, images) = last_user_parts(llm_seen.lock().unwrap().last().unwrap());
    assert_eq!(
        (texts, images.len()),
        (vec!["Mira la imagen adjunta.".to_owned()], 1)
    );
    // Unusable files and no words: told so, and the model is not asked. (HTML, an unknown id, a
    // file_path that tries to leave, a PDF.)
    let asked = llm_seen.lock().unwrap().len();
    for (id, extra) in [
        (12, json!({"photo":[{"file_id":"login","file_size":50}]})),
        (13, json!({"photo":[{"file_id":"nope","file_size":50}]})),
        (14, json!({"photo":[{"file_id":"evil","file_size":50}]})),
        (
            15,
            json!({"document":{"file_id":"doc","file_name":"a.pdf","mime_type":"application/pdf","file_size":50}}),
        ),
    ] {
        telegram.push_media(id, 42, extra);
    }
    eventually("each one is told so", || {
        telegram
            .sent()
            .iter()
            .filter(|(_, t)| t.contains("Solo puedo leer imágenes"))
            .count()
            == 4
    })
    .await;
    assert_eq!(llm_seen.lock().unwrap().len(), asked, "no model call");
    // Another chat is never answered, picture or not.
    telegram.push_media(
        16,
        99,
        json!({"caption":"hola","photo":[{"file_id":"big","file_size":4000}]}),
    );
    // A model that cannot see is not sent the picture, and is told there was one.
    make_vision(&app, false);
    telegram.push_media(
        17,
        42,
        json!({"caption":"mira esto","photo":[{"file_id":"big","file_size":4000}]}),
    );
    eventually("answered without sight", || telegram.sent().len() == 7).await;
    let (texts, images) = last_user_parts(llm_seen.lock().unwrap().last().unwrap());
    assert!(
        images.is_empty() && texts.join(" ").contains("no puede verlas"),
        "{texts:?}"
    );
    assert!(
        telegram.sent().iter().all(|(chat, _)| *chat == 42),
        "the stranger's chat got nothing"
    );
    assert!(app.turn_attachments.lock().unwrap().is_empty());
    app.shutdown.cancel();
    let _ = supervisor.await;
    llm.abort();
}

#[tokio::test]
async fn discord_pictures_are_read_from_discords_hosts_only_and_checked_by_their_bytes() {
    let (llm_url, llm_seen, llm) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    make_vision(&app, true);
    let discord = fake_discord().await;
    discord.store.put("ok.png", "image/png", tiny_png());
    discord
        .store
        .put("liar.png", "image/png", b"not really a png".to_vec());
    add_channel(&app, "discord", "dc1", discord.config(&["777"]));
    let supervisor = tokio::spawn(crate::discord::run(app.clone()));
    eventually("connected", || {
        discord.connections.load(AtomicOrdering::SeqCst) == 1
    })
    .await;
    // One real picture among a liar, a PDF and an oversized declaration.
    discord.message_with(
        "777",
        "¿y esto?",
        &[
            ("ok.png", "image/png", 70),
            ("liar.png", "image/png", 70),
            ("doc.pdf", "application/pdf", 70),
            ("big.png", "image/png", 99_000_000),
        ],
    );
    eventually("answered", || discord.posts.lock().unwrap().len() == 1).await;
    let (texts, images) = last_user_parts(llm_seen.lock().unwrap().last().unwrap());
    assert_eq!(texts, ["¿y esto?"]);
    assert_eq!(images, [format!("data:image/png;base64,{TINY_PNG}")]);
    let asked = discord.store.requests();
    assert!(
        asked.contains(&"ok.png:".to_owned()),
        "discord's files need no credential, and none is sent: {asked:?}"
    );
    assert!(
        !asked
            .iter()
            .any(|a| a.starts_with("doc.pdf") || a.starts_with("big.png")),
        "{asked:?}"
    );
    // Picture alone, from a channel that is not allowed (ignored), and nothing usable.
    discord.message_with("777", "", &[("ok.png", "image/png", 70)]);
    eventually("the picture alone is answered", || {
        discord.posts.lock().unwrap().len() == 2
    })
    .await;
    assert_eq!(
        last_user_parts(llm_seen.lock().unwrap().last().unwrap()).0,
        ["Mira la imagen adjunta."]
    );
    discord.message_with("555", "intruso", &[("ok.png", "image/png", 70)]);
    let before = llm_seen.lock().unwrap().len();
    discord.message_with("777", "", &[("doc.pdf", "application/pdf", 70)]);
    eventually("told so", || {
        discord.posts.lock().unwrap().iter().any(|(_, body, _)| {
            body["content"]
                .as_str()
                .is_some_and(|t| t.contains("Solo puedo leer imágenes"))
        })
    })
    .await;
    assert_eq!(
        llm_seen.lock().unwrap().len(),
        before,
        "neither the stranger nor the PDF reached the model"
    );
    // A picture URL that is not on Discord (the message says where) is never fetched.
    let _ = discord.push.send(json!({"id":"m","channel_id":"777","author":{"id":"5"},"content":"mira","type":0,"attachments":[{"filename":"x.png","content_type":"image/png","size":70,"url":"https://evil.example/x.png"}]}));
    eventually("answered without the picture", || {
        discord.posts.lock().unwrap().len() == 4
    })
    .await;
    assert!(
        last_user_parts(llm_seen.lock().unwrap().last().unwrap())
            .1
            .is_empty()
    );
    app.shutdown.cancel();
    let _ = supervisor.await;
    llm.abort();
}

#[tokio::test]
async fn whatsapp_pictures_are_fetched_with_the_access_token_through_the_media_id() {
    let (llm_url, llm_seen, llm) = provider().await;
    let (graph, sent, graph_task, media) = fake_graph_with_media().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    app.db.col_put("users", "u1", &json!({"id":"u1","name":"Uno","email":"uno@example.com","password_hash":"$2b$04$abcdefghijklmnopqrstuuxDugxrpcpW3csMl3JkYMLtX7yxJF.2."}), PutOptions::default()).unwrap();
    make_vision(&app, true);
    add_channel(
        &app,
        "whatsapp",
        "wa1",
        json!({"access_token":"EAABSECRET","phone_number_id":"1055","app_secret":"shh","verify_token":"vt","allowed_numbers":["+57 300 111 2222"],"api_base":graph}),
    );
    // The media id 77 is a real picture; 78 declares a picture and serves HTML.
    for (id, mime, bytes) in [
        ("77", "image/png", tiny_png()),
        ("78", "image/png", b"<html>no</html>".to_vec()),
    ] {
        media.put(&format!("info-{id}"), mime, bytes.clone());
        media.put(id, mime, bytes);
    }
    let (base, server) = serve(app.clone()).await;
    let image = |mid: &str, media_id: &str, caption: Option<&str>| {
        let mut message = json!({"id":mid,"from":"573001112222","timestamp":"1","type":"image","image":{"id":media_id,"mime_type":"image/png"}});
        if let Some(caption) = caption {
            message["image"]["caption"] = json!(caption);
        }
        let body = webhook_body("1055", json!([message]));
        let signature = sign("shh", &body);
        (body, signature)
    };
    let (body, signature) = image("wamid.1", "77", Some("¿qué es?"));
    assert_eq!(
        post_webhook(&base, "wa1", body, Some(signature))
            .await
            .status(),
        200
    );
    eventually("answered", || {
        sent.lock().unwrap().iter().any(|(path, body, _)| {
            path.ends_with("/messages") && body["text"]["body"] == "Hola Rust"
        })
    })
    .await;
    let (texts, images) = last_user_parts(llm_seen.lock().unwrap().last().unwrap());
    assert_eq!(
        (texts, images),
        (
            vec!["¿qué es?".to_owned()],
            vec![format!("data:image/png;base64,{TINY_PNG}")]
        )
    );
    // Both the lookup and the download carried the access token.
    assert_eq!(media.requests(), ["77:Bearer EAABSECRET"]);
    assert!(
        sent.lock()
            .unwrap()
            .iter()
            .any(|(path, _, auth)| path == "/77" && auth == "Bearer EAABSECRET"),
        "the lookup too"
    );
    // A picture whose bytes are not one, and a media id that is not safe, are not used (the person is told).
    let asked = llm_seen.lock().unwrap().len();
    for (mid, media_id) in [("wamid.2", "78"), ("wamid.3", "../../x")] {
        let (body, signature) = image(mid, media_id, None);
        assert_eq!(
            post_webhook(&base, "wa1", body, Some(signature))
                .await
                .status(),
            200
        );
    }
    eventually("each one is told so", || {
        sent.lock()
            .unwrap()
            .iter()
            .filter(|(_, body, _)| {
                body["text"]["body"]
                    .as_str()
                    .is_some_and(|t| t.contains("Solo puedo leer imágenes"))
            })
            .count()
            == 2
    })
    .await;
    assert_eq!(llm_seen.lock().unwrap().len(), asked);
    server.abort();
    graph_task.abort();
    llm.abort();
}

/// Serve one request with a canned JSON body and return the request line and body it received.
async fn one_request(reply: Value) -> (u16, tokio::task::JoinHandle<(String, Value)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let request = read_request(&mut socket).await.unwrap();
        let line = request.uri().path().to_owned();
        let body = serde_json::from_slice::<Value>(request.body()).unwrap();
        let _ = respond(&mut socket, 200, reply).await;
        (line, body)
    });
    (port, task)
}

#[tokio::test]
async fn the_models_context_window_from_hivedb_reaches_every_kind_of_provider() {
    let ask = |app: &App| {
        let selection = app.selection().unwrap();
        // A request that names no output limit: the provider derives one from the model's window.
        let request: ModelRequest = serde_json::from_value(
            json!({"model":"m","messages":[{"role":"user","content":"hola"}],"tools":[]}),
        )
        .unwrap();
        selection.provider.start(request, false)
    };
    let drain = |events: ModelStream| async move {
        while let Ok(Some(event)) = events.next().await {
            if matches!(event, ModelEvent::Reply { .. }) {
                break;
            }
        }
    };
    let setup = |provider_id: &str, base: String, provider_extra: Value, window: Option<u64>| {
        let home = tempfile::tempdir().unwrap();
        seed(home.path(), "http://127.0.0.1:9/v1");
        let app = App::open(home.path().to_owned()).unwrap();
        let mut provider =
            json!({"id":provider_id,"base_url":base,"active":true,"enabled":true,"category":"llm"});
        for (key, value) in provider_extra.as_object().unwrap() {
            provider[key] = value.clone();
        }
        let mut model = json!({"id":format!("{provider_id}/m"),"provider_id":provider_id,"active":true,"enabled":true,"model_type":"llm"});
        if let Some(window) = window {
            model["context_window"] = json!(window);
        }
        app.db
            .col_put("providers", provider_id, &provider, PutOptions::default())
            .unwrap();
        app.db
            .col_put(
                "models",
                &format!("{provider_id}/m"),
                &model,
                PutOptions::default(),
            )
            .unwrap();
        app.db.col_put("agents", "a1", &json!({"id":"a1","user_id":"u1","name":"Hive","role":"coordinator","enabled":true,"provider_id":provider_id,"model_id":format!("{provider_id}/m"),"system_prompt":"x","max_iterations":8}), PutOptions::default()).unwrap();
        (home, app)
    };
    // Anthropic: max_tokens is derived from the window (15% of 200000).
    let (port, server) = one_request(json!({"id":"m","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})).await;
    let (_home, app) = setup(
        "anthropic",
        format!("http://127.0.0.1:{port}"),
        json!({}),
        Some(200_000),
    );
    drain(ask(&app)).await;
    assert_eq!(server.await.unwrap().1["max_tokens"], 30_000);
    // The same provider with a model row that has no window keeps the SDK's old default.
    let (port, server) = one_request(json!({"id":"m","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})).await;
    let (_home, app) = setup(
        "anthropic",
        format!("http://127.0.0.1:{port}"),
        json!({}),
        None,
    );
    drain(ask(&app)).await;
    assert_eq!(server.await.unwrap().1["max_tokens"], 4096);
    // Gemini.
    let (port, server) = one_request(json!({"candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1}})).await;
    let (_home, app) = setup(
        "gemini",
        format!("http://127.0.0.1:{port}"),
        json!({}),
        Some(1_000_000),
    );
    drain(ask(&app)).await;
    assert_eq!(
        server.await.unwrap().1["generationConfig"]["maxOutputTokens"],
        32_768,
        "capped at the ceiling"
    );
    // OpenAI-compatible (a provider that is none of the above): the cap is sent, derived from the window.
    let (port, server) = one_request(json!({"choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}})).await;
    let (_home, app) = setup(
        "openrouter",
        format!("http://127.0.0.1:{port}/v1"),
        json!({}),
        Some(64_000),
    );
    drain(ask(&app)).await;
    assert_eq!(server.await.unwrap().1["max_tokens"], 9_600);
    // Ollama: the model's window, up to the ceiling; the operator's num_ctx wins; and the output limit follows the window it asks for.
    for (provider_extra, window, expected_ctx, expected_predict) in [
        (json!({}), Some(8_192u64), 8_192u64, 1_228u64),
        (json!({}), Some(262_144), 32_768, 4_915),
        (json!({"num_ctx":65_536}), Some(262_144), 65_536, 9_830),
        (json!({"num_ctx":8_192}), Some(262_144), 8_192, 1_228),
    ] {
        let (port, server) = one_request(
            json!({"message":{"role":"assistant","content":"ok"},"done":true,"done_reason":"stop"}),
        )
        .await;
        let (_home, app) = setup(
            "ollama",
            format!("http://127.0.0.1:{port}/v1"),
            provider_extra.clone(),
            window,
        );
        drain(ask(&app)).await;
        let (path, body) = server.await.unwrap();
        assert_eq!(path, "/api/chat");
        assert_eq!(
            (
                body["options"]["num_ctx"].clone(),
                body["options"]["num_predict"].clone()
            ),
            (json!(expected_ctx), json!(expected_predict)),
            "{provider_extra} {window:?}"
        );
    }
}

/// A server that answers every request with one status and body, and records what it was asked.
struct FakeHttp {
    url: String,
    seen: Arc<Mutex<Vec<(String, String, Value)>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for FakeHttp {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn fake_http(status: u16, reply: Value) -> FakeHttp {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<(String, String, Value)>>> = Arc::default();
    let record = seen.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (record, reply) = (record.clone(), reply.clone());
            tokio::spawn(async move {
                let Ok(request) = read_request(&mut socket).await else {
                    return;
                };
                let auth = request
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                record.lock().unwrap().push((
                    request.uri().path().to_owned(),
                    auth,
                    serde_json::from_slice(request.body()).unwrap_or(Value::Null),
                ));
                let _ = respond(&mut socket, status, reply).await;
            });
        }
    });
    FakeHttp { url, seen, task }
}

/// A PDF with one empty page: valid, with no text at all (what a scan looks like to a text reader).
fn blank_pdf() -> Vec<u8> {
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>".to_owned(),
    ];
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets = vec![];
    for (number, body) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.extend(format!("{} 0 obj\n{body}\nendobj\n", number + 1).as_bytes());
    }
    let xref = pdf.len();
    pdf.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
    for offset in offsets {
        pdf.extend(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

/// A PDF of `pages` pages, each holding one 2x2 gray picture (what a scanner leaves behind).
fn pdf_with_page_pictures(pages: usize) -> Vec<u8> {
    let zlib = |bytes: &[u8]| {
        let mut out = vec![0x78, 0x01, 1];
        out.extend((bytes.len() as u16).to_le_bytes());
        out.extend((!(bytes.len() as u16)).to_le_bytes());
        out.extend(bytes);
        let (mut a, mut b) = (1u32, 0u32);
        for byte in bytes {
            a = (a + *byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        out.extend(((b << 16) | a).to_be_bytes());
        out
    };
    let mut pdf = b"%PDF-1.4\n".to_vec();
    let mut offsets: Vec<usize> = vec![];
    let mut add = |pdf: &mut Vec<u8>, body: Vec<u8>| {
        offsets.push(pdf.len());
        pdf.extend(format!("{} 0 obj\n", offsets.len()).as_bytes());
        pdf.extend(body);
        pdf.extend(b"\nendobj\n");
    };
    add(&mut pdf, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());
    let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", 3 + i * 2)).collect();
    add(
        &mut pdf,
        format!(
            "<< /Type /Pages /Kids [{}] /Count {pages} >>",
            kids.join(" ")
        )
        .into_bytes(),
    );
    for i in 0..pages {
        add(&mut pdf, format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /XObject << /Im0 {} 0 R >> >> >>", 4 + i * 2).into_bytes());
        let data = zlib(&[0, 80, 160, 255]);
        let mut image = format!("<< /Type /XObject /Subtype /Image /Width 2 /Height 2 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode /Length {} >>\nstream\n", data.len()).into_bytes();
        image.extend(data);
        image.extend(b"\nendstream");
        add(&mut pdf, image);
    }
    let xref = pdf.len();
    pdf.extend(format!("xref\n0 {}\n0000000000 65535 f \n", offsets.len() + 1).as_bytes());
    for offset in &offsets {
        pdf.extend(format!("{offset:010} 00000 n \n").as_bytes());
    }
    pdf.extend(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            offsets.len() + 1
        )
        .as_bytes(),
    );
    pdf
}

/// An app with a documents folder holding a PDF with text, a scanned (blank) PDF and a picture, an OCR
/// provider (a fake Mistral) and a vision chat model, and the settings choosing `choice`.
struct OcrFixture {
    app: Arc<App>,
    folder: tempfile::TempDir,
    mistral: FakeHttp,
    _home: tempfile::TempDir,
}
async fn ocr_fixture(llm_url: &str, choice: Option<(&str, &str)>, extra: Value) -> OcrFixture {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    make_vision(&app, true);
    let mistral = fake_http(200, json!({"pages":[{"index":0,"markdown":"# Factura\nTotal: 42"},{"index":1,"markdown":""}],"usage_info":{"pages_processed":2}})).await;
    app.db.col_put("providers", "mistral", &json!({"id":"mistral","base_url":format!("{}/v1", mistral.url),"active":true,"enabled":true,"category":"ocr"}), PutOptions::default()).unwrap();
    app.db.col_put("models", "mistral/mistral-ocr-4-1", &json!({"id":"mistral/mistral-ocr-4-1","provider_id":"mistral","wire_id":"mistral-ocr-4-1","name":"Mistral OCR","model_type":"vision","capabilities":["ocr"],"enabled":true,"active":true}), PutOptions::default()).unwrap();
    config::store_secret(&app, "provider:mistral:api_key", "mistral-key-123").unwrap();
    let folder = tempfile::tempdir().unwrap();
    std::fs::write(folder.path().join("escaneo.pdf"), blank_pdf()).unwrap();
    std::fs::write(folder.path().join("foto.png"), tiny_png()).unwrap();
    let tools =
        hive_agent_core::tools::NativeTools::from_json(json!({"root":folder.path(),"office":{}}))
            .unwrap();
    tools.execute_tool("office_write_pdf", json!({"path":"con-texto.pdf","content":"Contrato de arrendamiento. Valor mensual 1500."}), &CancellationToken::new()).await.unwrap();
    if let Some((provider, model)) = choice {
        let mut settings =
            json!({"enabled":true,"folder":folder.path(),"provider_id":provider,"model_id":model});
        for (key, value) in extra.as_object().unwrap() {
            settings[key] = value.clone();
        }
        let (status, body) = call(&app, Some(&app.token), "PUT", "/api/ocr", settings);
        assert_eq!(status, 200, "{body}");
    }
    OcrFixture {
        app,
        folder,
        mistral,
        _home: home,
    }
}
async fn ocr(app: &Arc<App>, arguments: Value) -> Value {
    crate::ocr::execute(app, arguments, &CancellationToken::new()).await
}

#[tokio::test]
async fn the_ocr_settings_are_off_by_default_validated_and_only_offer_providers_that_can_read() {
    let (llm_url, _llm_seen, llm) = provider().await;
    let f = ocr_fixture(&llm_url, None, json!({})).await;
    let app = &f.app;
    let got = call(app, Some(&app.token), "GET", "/api/ocr", json!({})).1;
    assert_eq!(
        (
            got["settings"]["enabled"].clone(),
            got["ready"].clone(),
            got["settings"]["ask"].clone()
        ),
        (json!(false), json!(false), json!(true)),
        "off, and asking before sending, by default"
    );
    // Only models that can read pictures of text, of providers that are on and have a key.
    let list: Vec<(String, String, bool)> = got["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["provider_id"].as_str().unwrap().to_owned(),
                c["kind"].as_str().unwrap().to_owned(),
                c["supported"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        list[0],
        ("mistral".into(), "ocr".into(), true),
        "the dedicated endpoint is listed first: {list:?}"
    );
    assert!(
        list.iter()
            .any(|(p, k, _)| p == "openrouter" && k == "vision"),
        "{list:?}"
    );
    // A provider with a dedicated OCR model but no adapter is shown as unsupported, not hidden.
    app.db
        .col_put(
            "providers",
            "other-ocr",
            &json!({"id":"other-ocr","base_url":"http://x","active":true,"enabled":true}),
            PutOptions::default(),
        )
        .unwrap();
    app.db.col_put("models", "other-ocr/m", &json!({"id":"other-ocr/m","provider_id":"other-ocr","model_type":"vision","capabilities":["ocr"],"enabled":true}), PutOptions::default()).unwrap();
    config::store_secret(app, "provider:other-ocr:api_key", "k").unwrap();
    let got = call(app, Some(&app.token), "GET", "/api/ocr", json!({})).1;
    let other = got["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["provider_id"] == "other-ocr")
        .unwrap()
        .clone();
    assert_eq!(
        (other["supported"].clone(), other["note"].is_string()),
        (json!(false), true)
    );
    // A provider without a key, and a model that cannot see, are not offered.
    app.db
        .col_put(
            "providers",
            "nokey",
            &json!({"id":"nokey","base_url":"http://x","active":true,"enabled":true}),
            PutOptions::default(),
        )
        .unwrap();
    app.db.col_put("models", "nokey/m", &json!({"id":"nokey/m","provider_id":"nokey","model_type":"llm","capabilities":["vision"],"enabled":true}), PutOptions::default()).unwrap();
    app.db.col_put("models", "mistral/chat", &json!({"id":"mistral/chat","provider_id":"mistral","model_type":"llm","capabilities":["chat"],"enabled":true}), PutOptions::default()).unwrap();
    let ids: Vec<String> =
        call(app, Some(&app.token), "GET", "/api/ocr", json!({})).1["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["model_id"].as_str().unwrap().to_owned())
            .collect();
    assert!(
        !ids.contains(&"nokey/m".to_owned()) && !ids.contains(&"mistral/chat".to_owned()),
        "{ids:?}"
    );
    // Invalid settings are refused and change nothing.
    let folder = f.folder.path().to_str().unwrap();
    for bad in [
        json!({"enabled":true}),
        json!({"enabled":true,"folder":"relativa","provider_id":"mistral","model_id":"mistral/mistral-ocr-4-1"}),
        json!({"enabled":true,"folder":"/no/existe/nunca","provider_id":"mistral","model_id":"mistral/mistral-ocr-4-1"}),
        json!({"enabled":true,"folder":folder}),
        json!({"enabled":true,"folder":folder,"provider_id":"mistral","model_id":"mistral/chat"}),
        json!({"enabled":true,"folder":folder,"provider_id":"other-ocr","model_id":"other-ocr/m"}),
        json!({"enabled":true,"folder":folder,"provider_id":"nokey","model_id":"nokey/m"}),
        json!({"session_minutes":0}),
        json!({"max_pages":0}),
        json!({"max_pages":201}),
        json!({"ask":"si"}),
        json!({"surprise":1}),
    ] {
        assert_eq!(
            call(app, Some(&app.token), "PUT", "/api/ocr", bad.clone()).0,
            400,
            "{bad}"
        );
    }
    assert_eq!(
        call(app, Some(&app.token), "GET", "/api/ocr", json!({})).1["settings"]["enabled"],
        false
    );
    let good = call(
        app,
        Some(&app.token),
        "PUT",
        "/api/ocr",
        json!({"enabled":true,"folder":folder,"provider_id":"mistral","model_id":"mistral/mistral-ocr-4-1"}),
    );
    assert_eq!(
        (good.0, good.1["ready"].clone()),
        (200, json!(true)),
        "{}",
        good.1
    );
    // Only administrators.
    app.db.col_put("users", "u1", &json!({"id":"u1","name":"Uno","email":"uno@example.com","password_hash":"$2b$04$abcdefghijklmnopqrstuuxDugxrpcpW3csMl3JkYMLtX7yxJF.2."}), PutOptions::default()).unwrap();
    app.db
        .col_put(
            "users",
            "u2",
            &json!({"id":"u2","name":"Dos","email":null,"password_hash":null}),
            PutOptions::default(),
        )
        .unwrap();
    let user = issue(app, "u2");
    for method in ["GET", "PUT"] {
        assert_eq!(
            call(
                app,
                Some(&user),
                method,
                "/api/ocr",
                json!({"enabled":false})
            )
            .0,
            403,
            "{method}"
        );
    }
    // The coordinator is offered the tool only while it is on and usable.
    let offered = |app: &App| {
        serde_json::from_str::<Value>(&app.selection().unwrap().engine_config).unwrap()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "ocr_read")
    };
    assert!(offered(app));
    call(
        app,
        Some(&app.token),
        "PUT",
        "/api/ocr",
        json!({"enabled":false}),
    );
    assert!(!offered(app));
    llm.abort();
}

#[tokio::test]
async fn a_pdf_with_text_is_read_in_place_and_nothing_is_sent_out_while_a_scan_goes_to_the_ocr_endpoint()
 {
    let (llm_url, _seen, llm) = provider().await;
    let f = ocr_fixture(
        &llm_url,
        Some(("mistral", "mistral/mistral-ocr-4-1")),
        json!({}),
    )
    .await;
    let text = ocr(&f.app, json!({"path":"con-texto.pdf"})).await;
    assert_eq!(
        (
            text["ok"].clone(),
            text["method"].clone(),
            text["sent_out"].clone()
        ),
        (json!(true), json!("text_layer"), json!(false)),
        "{text}"
    );
    assert!(
        text["text"]
            .as_str()
            .unwrap()
            .contains("Contrato de arrendamiento"),
        "{text}"
    );
    assert!(
        f.mistral.seen.lock().unwrap().is_empty(),
        "nothing left the machine"
    );
    // A scan has no text: it goes to the dedicated endpoint, with the key, as a data URI, and says so.
    let scan = ocr(&f.app, json!({"path":"escaneo.pdf"})).await;
    assert_eq!(
        (
            scan["ok"].clone(),
            scan["method"].clone(),
            scan["sent_out"].clone(),
            scan["provider"].clone()
        ),
        (
            json!(true),
            json!("ocr_endpoint"),
            json!(true),
            json!("mistral")
        ),
        "{scan}"
    );
    assert_eq!(
        scan["pages"][0],
        json!({"page":1,"text":"# Factura\nTotal: 42"})
    );
    assert_eq!(
        scan["unread_pages"],
        json!([2]),
        "a page that came back empty is reported, not filled in"
    );
    let asked = f.mistral.seen.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        (asked[0].0.as_str(), asked[0].1.as_str()),
        ("/v1/ocr", "Bearer mistral-key-123")
    );
    assert_eq!(asked[0].2["model"], "mistral-ocr-4-1");
    assert!(
        asked[0].2["document"]["document_url"]
            .as_str()
            .unwrap()
            .starts_with("data:application/pdf;base64,JVBER"),
        "{}",
        asked[0].2["document"]["type"]
    );
    assert_eq!(
        (
            asked[0].2["document"]["type"].clone(),
            asked[0].2["pages"].clone(),
            asked[0].2["include_image_base64"].clone()
        ),
        (
            json!("document_url"),
            json!((0..50).collect::<Vec<u64>>()),
            json!(false)
        )
    );
    // Pages are asked for from 1 and sent from 0.
    ocr(
        &f.app,
        json!({"path":"escaneo.pdf","first_page":2,"last_page":3}),
    )
    .await;
    assert_eq!(f.mistral.seen.lock().unwrap()[1].2["pages"], json!([1, 2]));
    // A picture goes as an image.
    let picture = ocr(&f.app, json!({"path":"foto.png"})).await;
    assert_eq!(picture["method"], "ocr_endpoint", "{picture}");
    assert_eq!(
        f.mistral.seen.lock().unwrap()[2].2["document"]["type"],
        "image_url"
    );
    assert!(
        f.mistral.seen.lock().unwrap()[2].2["document"]["image_url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,")
    );
    llm.abort();
}

#[tokio::test]
async fn a_vision_model_reads_documents_and_pictures_when_it_is_the_chosen_method() {
    let (llm_url, llm_seen, llm) = provider().await;
    let f = ocr_fixture(
        &llm_url,
        Some(("openrouter", "openrouter/vendor/model")),
        json!({}),
    )
    .await;
    // A provider that only reads pictures cannot read a scanned PDF, and says which ones can.
    let refused = ocr(&f.app, json!({"path":"escaneo.pdf"})).await;
    assert_eq!(refused["ok"], json!(false), "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("solo lee imágenes")
            && refused["error"].as_str().unwrap().contains("Gemini"),
        "{refused}"
    );
    assert!(llm_seen.lock().unwrap().is_empty(), "nothing was sent");
    // A scan whose pages are pictures is read page by page, one request each, by the same provider.
    std::fs::write(
        f.folder.path().join("escaneo-imagenes.pdf"),
        pdf_with_page_pictures(2),
    )
    .unwrap();
    let paged = ocr(&f.app, json!({"path":"escaneo-imagenes.pdf"})).await;
    assert_eq!(
        (
            paged["ok"].clone(),
            paged["method"].clone(),
            paged["sent_out"].clone()
        ),
        (json!(true), json!("vision_page_images"), json!(true)),
        "{paged}"
    );
    assert_eq!(paged["pages"].as_array().unwrap().len(), 2, "{paged}");
    assert!(
        paged["text"].as_str().unwrap().contains("--- página 1 ---")
            && paged["text"].as_str().unwrap().contains("--- página 2 ---"),
        "{paged}"
    );
    assert!(paged.get("unread_pages").is_none(), "{paged}");
    assert_eq!(llm_seen.lock().unwrap().len(), 2, "one request per page");
    let page_request = llm_seen.lock().unwrap().last().unwrap().to_string();
    assert!(
        page_request.contains("data:image/png;base64,") && page_request.contains("(2)"),
        "{page_request}"
    );
    llm_seen.lock().unwrap().clear();
    // One whose adapter takes documents (here an OpenAI-compatible server) reads it.
    f.app.db.col_put("providers", "openai", &json!({"id":"openai","base_url":llm_url,"active":true,"enabled":true,"category":"llm"}), PutOptions::default()).unwrap();
    f.app.db.col_put("models", "openai/m", &json!({"id":"openai/m","provider_id":"openai","wire_id":"m","active":true,"enabled":true,"model_type":"llm","capabilities":["vision"]}), PutOptions::default()).unwrap();
    config::store_secret(&f.app, "provider:openai:api_key", "fixture-key").unwrap();
    config::store_secret(
        &f.app,
        "provider:openai:headers",
        r#"{"x-fixture":"sdk-rust"}"#,
    )
    .unwrap();
    let (status, body) = call(
        &f.app,
        Some(&f.app.token),
        "PUT",
        "/api/ocr",
        json!({"enabled":true,"folder":f.folder.path(),"provider_id":"openai","model_id":"openai/m"}),
    );
    assert_eq!(status, 200, "{body}");
    let scan = ocr(
        &f.app,
        json!({"path":"escaneo.pdf","first_page":1,"last_page":1}),
    )
    .await;
    assert_eq!(
        (
            scan["ok"].clone(),
            scan["method"].clone(),
            scan["text"].clone()
        ),
        (json!(true), json!("vision_document"), json!("Hola Rust")),
        "{scan}"
    );
    assert!(
        scan["warnings"].to_string().contains("first_page"),
        "a page range this method cannot honor is said, not ignored: {scan}"
    );
    let request = llm_seen.lock().unwrap().last().unwrap().clone();
    let content = request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .unwrap()["content"]
        .to_string();
    assert!(
        content.contains("Transcribe literalmente")
            && content.contains("application/pdf")
            && content.contains("[ilegible]"),
        "{content}"
    );
    assert!(
        f.mistral.seen.lock().unwrap().is_empty(),
        "the dedicated endpoint was not used"
    );
    let picture = ocr(&f.app, json!({"path":"foto.png"})).await;
    assert_eq!(
        (picture["method"].clone(), picture["text"].clone()),
        (json!("vision_image"), json!("Hola Rust")),
        "{picture}"
    );
    let request = llm_seen.lock().unwrap().last().unwrap().clone();
    assert!(
        request["messages"]
            .to_string()
            .contains(&format!("data:image/png;base64,{TINY_PNG}")),
        "the picture reached the model"
    );
    llm.abort();
}

#[tokio::test]
async fn ocr_reads_only_inside_its_folder_and_fails_in_plain_words() {
    let (llm_url, _seen, llm) = provider().await;
    let f = ocr_fixture(
        &llm_url,
        Some(("mistral", "mistral/mistral-ocr-4-1")),
        json!({"max_pages":3}),
    )
    .await;
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secreto.png"), tiny_png()).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secreto.png"),
        f.folder.path().join("enlace.png"),
    )
    .unwrap();
    std::fs::write(f.folder.path().join("notas.txt"), "hola").unwrap();
    std::fs::write(
        f.folder.path().join("mentira.png"),
        b"esto no es una imagen",
    )
    .unwrap();
    let big = std::fs::File::create(f.folder.path().join("enorme.pdf")).unwrap();
    big.set_len(26 * 1024 * 1024).unwrap();
    let error = |value: Value| value["error"].as_str().unwrap_or_default().to_owned();
    for (arguments, expected) in [
        (
            json!({"path":format!("../{}/secreto.png", outside.path().file_name().unwrap().to_string_lossy())}),
            "fuera de la carpeta",
        ),
        (
            json!({"path":"../../../../../../../../etc/passwd"}),
            "fuera de la carpeta",
        ),
        (
            // An absolute path is read as relative to the folder, so it cannot name anything outside it.
            json!({"path":format!("{}/secreto.png", outside.path().display())}),
            "no existe",
        ),
        (json!({"path":"enlace.png"}), "fuera de la carpeta"),
        (json!({"path":"no-existe.pdf"}), "no existe"),
        (json!({"path":"notas.txt"}), "solo se leen"),
        (json!({"path":"mentira.png"}), "no es un PDF ni una imagen"),
        (json!({"path":"enorme.pdf"}), "pesa más de"),
        (
            json!({"path":"escaneo.pdf","first_page":5,"last_page":2}),
            "last_page",
        ),
        (
            json!({"path":"escaneo.pdf","first_page":1,"last_page":9}),
            "como mucho 3 páginas",
        ),
        (json!({"path":""}), "path"),
        (json!({}), "falta path"),
    ] {
        let result = ocr(&f.app, arguments.clone()).await;
        assert_eq!(result["ok"], json!(false), "{arguments}");
        assert!(
            error(result.clone()).contains(expected),
            "{arguments}: {result}"
        );
    }
    assert!(
        f.mistral.seen.lock().unwrap().is_empty(),
        "none of those was sent anywhere"
    );
    // The provider fails: a plain error that does not repeat its words.
    let down = fake_http(500, json!({"error":"secreto interno /home/x"})).await;
    f.app.db.col_put("providers", "mistral", &json!({"id":"mistral","base_url":format!("{}/v1", down.url),"active":true,"enabled":true,"category":"ocr"}), PutOptions::default()).unwrap();
    let failed = ocr(&f.app, json!({"path":"escaneo.pdf"})).await;
    assert_eq!(failed["ok"], json!(false));
    assert!(
        error(failed.clone()).contains("HTTP 500")
            && !failed.to_string().contains("secreto interno"),
        "{failed}"
    );
    // The chosen provider loses its key: the scan cannot be read, and it says why (the text PDF still can).
    config::store_secret(&f.app, "provider:mistral:api_key", "").unwrap_or(());
    f.app.db.col_put("providers", "mistral", &json!({"id":"mistral","base_url":format!("{}/v1", down.url),"active":false,"enabled":false}), PutOptions::default()).unwrap();
    let gone = ocr(&f.app, json!({"path":"escaneo.pdf"})).await;
    assert!(error(gone).contains("ya no está disponible"));
    assert_eq!(
        ocr(&f.app, json!({"path":"con-texto.pdf"})).await["method"],
        "text_layer"
    );
    // Off means off.
    call(
        &f.app,
        Some(&f.app.token),
        "PUT",
        "/api/ocr",
        json!({"enabled":false}),
    );
    assert!(error(ocr(&f.app, json!({"path":"con-texto.pdf"})).await).contains("desactivada"));
    llm.abort();
}

#[tokio::test]
async fn sending_a_document_out_needs_one_approval_per_conversation_unless_the_settings_say_otherwise()
 {
    let (llm_url, _seen, llm) = provider().await;
    // Asking (the default): one approval opens the conversation, a second call is not asked again.
    let f = ocr_fixture(
        &llm_url,
        Some(("mistral", "mistral/mistral-ocr-4-1")),
        json!({}),
    )
    .await;
    let steps = || {
        vec![
            ("ocr_read", json!({"path":"escaneo.pdf"})),
            ("ocr_read", json!({"path":"foto.png"})),
        ]
    };
    let (result, fixture, asked) = script_turn(&f.app, steps(), vec![true]).await;
    result.unwrap();
    assert_eq!(asked, 1, "one approval for both documents");
    assert_eq!(f.mistral.seen.lock().unwrap().len(), 2);
    assert!(
        fixture
            .seen
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("ocr_endpoint"),
        "the model got the result, with its method"
    );
    // A refusal sends nothing, and the model is told.
    let f2 = ocr_fixture(
        &llm_url,
        Some(("mistral", "mistral/mistral-ocr-4-1")),
        json!({}),
    )
    .await;
    let (result, fixture, asked) = script_turn(&f2.app, steps(), vec![false, false]).await;
    result.unwrap();
    assert_eq!(asked, 2, "a refusal grants nothing");
    assert!(f2.mistral.seen.lock().unwrap().is_empty());
    assert!(
        fixture
            .seen
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("POLICY_DENIED")
    );
    // A deny rule still wins.
    let f3 = ocr_fixture(
        &llm_url,
        Some(("mistral", "mistral/mistral-ocr-4-1")),
        json!({}),
    )
    .await;
    put_policy(
        &f3.app,
        json!({"rules":[{"tools":["ocr_read"],"decision":"deny","reason":"no se leen documentos"}]}),
        5000,
    );
    let (result, fixture, asked) = script_turn(&f3.app, steps(), vec![true]).await;
    result.unwrap();
    assert_eq!(asked, 0);
    assert!(
        f3.mistral.seen.lock().unwrap().is_empty()
            && fixture
                .seen
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .contains("no se leen documentos")
    );
    // Not asking: allowed by the usual policy, and nothing is asked.
    let f4 = ocr_fixture(
        &llm_url,
        Some(("mistral", "mistral/mistral-ocr-4-1")),
        json!({"ask":false}),
    )
    .await;
    let (result, _, asked) = script_turn(&f4.app, steps(), vec![]).await;
    result.unwrap();
    assert_eq!((asked, f4.mistral.seen.lock().unwrap().len()), (0, 2));
    // The approval is per conversation and expires.
    f.app.ocr_grants.lock().unwrap().insert(
        "u1\u{0}otra".into(),
        std::time::Instant::now() - Duration::from_secs(1),
    );
    assert!(!f.app.ocr_grant_live("u1", "otra"), "expired");
    assert!(!f.app.ocr_grant_live("u1", "otra-conversacion"));
    llm.abort();
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

// ---- several users -----------------------------------------------------------------

/// Two users, the owner `u1` (with credentials, so authentication is on) and `u2`, each
/// with a coordinator. Returns the app and the administrator (shared) token.
fn two_users() -> (tempfile::TempDir, Arc<App>) {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    for (id, doc) in [
        (
            "u1",
            json!({"id":"u1","name":"Uno","email":"uno@example.com","password_hash":"$2b$04$abcdefghijklmnopqrstuuxDugxrpcpW3csMl3JkYMLtX7yxJF.2."}),
        ),
        (
            "u2",
            json!({"id":"u2","name":"Dos","email":null,"password_hash":null}),
        ),
    ] {
        app.db
            .col_put("users", id, &doc, PutOptions::default())
            .unwrap();
    }
    app.db
        .col_put(
            "agents",
            "a2",
            &json!({"id":"a2","user_id":"u2","name":"Hive Dos","role":"coordinator","enabled":true,
                "provider_id":"openrouter","model_id":"openrouter/vendor/model","system_prompt":"Responde","max_iterations":8}),
            PutOptions::default(),
        )
        .unwrap();
    (home, app)
}
fn call(app: &App, token: Option<&str>, method: &str, path: &str, body: Value) -> (u16, Value) {
    let mut request = Request::builder().method(method).uri(path);
    if let Some(token) = token {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    route(app, request.body(body.to_string().into_bytes()).unwrap()).unwrap()
}
/// Issue a token for `user` with the administrator token.
fn issue(app: &App, user: &str) -> String {
    let (status, body) = call(
        app,
        Some(&app.token),
        "POST",
        &format!("/api/users/{user}/token"),
        json!({}),
    );
    assert_eq!(status, 200, "{body}");
    body["token"].as_str().unwrap().to_owned()
}

#[test]
fn tokens_authenticate_a_single_user_are_shown_once_and_can_be_replaced_or_revoked() {
    let (_home, app) = two_users();
    let admin = app.authenticate(Some(&app.token)).unwrap().unwrap();
    assert_eq!(
        (admin.user.as_str(), admin.admin),
        ("u1", true),
        "the shared token is the owner and the administrator"
    );
    assert!(
        app.authenticate(None).unwrap().is_none()
            && app.authenticate(Some("nope")).unwrap().is_none()
    );
    let first = issue(&app, "u2");
    assert!(first.starts_with("hvu_") && first.len() >= 64);
    let user = app.authenticate(Some(&first)).unwrap().unwrap();
    assert_eq!((user.user.as_str(), user.admin), ("u2", false));
    // Only the hash is stored.
    let stored = app.db.col_get("users", "u2").unwrap().unwrap().doc;
    assert_eq!(stored["api_token_hash"], App::hash_token(&first));
    assert!(!stored.to_string().contains(&first));
    // Issuing again replaces the old token; revoking removes access.
    let second = issue(&app, "u2");
    assert_ne!(first, second);
    assert!(
        app.authenticate(Some(&first)).unwrap().is_none(),
        "the replaced token stops working"
    );
    assert!(app.authenticate(Some(&second)).unwrap().is_some());
    assert_eq!(
        call(
            &app,
            Some(&app.token),
            "DELETE",
            "/api/users/u2/token",
            json!({})
        )
        .1["revoked"],
        true
    );
    assert!(app.authenticate(Some(&second)).unwrap().is_none());
    // Only the administrator can manage tokens, for known users only, with the right method.
    let again = issue(&app, "u2");
    assert_eq!(
        call(&app, Some(&again), "POST", "/api/users/u1/token", json!({})).0,
        403,
        "a user cannot mint tokens, not even their own"
    );
    assert_eq!(
        call(&app, None, "POST", "/api/users/u2/token", json!({})).0,
        401
    );
    assert_eq!(
        call(
            &app,
            Some(&app.token),
            "POST",
            "/api/users/ghost/token",
            json!({})
        )
        .0,
        404
    );
    assert_eq!(
        call(
            &app,
            Some(&app.token),
            "GET",
            "/api/users/u2/token",
            json!({})
        )
        .0,
        405
    );
    // Without credentials configured the gateway stays open and acts as the owner, as before.
    let open_home = tempfile::tempdir().unwrap();
    seed(open_home.path(), "http://127.0.0.1:9/v1");
    let open = App::open(open_home.path().to_owned()).unwrap();
    assert_eq!(
        open.authenticate(None).unwrap().unwrap(),
        crate::config::Principal {
            user: "u1".into(),
            admin: true
        }
    );
}

#[test]
fn a_user_token_reaches_only_that_users_own_surface() {
    let (_home, app) = two_users();
    let token = issue(&app, "u2");
    for (method, path) in [
        ("GET", "/api/providers"),
        ("GET", "/api/channels"),
        ("GET", "/api/tools"),
        ("GET", "/api/skills"),
        ("GET", "/api/mcp/servers"),
        ("GET", "/api/ethics"),
        ("GET", "/api/user/settings"),
        ("POST", "/api/http-request"),
        ("POST", "/api/setup/verify-provider"),
        ("GET", "/api/meetings"),
        ("GET", "/api/dashboard/summary"),
        ("POST", "/api/users/u1/token"),
        ("DELETE", "/api/users/u2/token"),
        ("GET", "/api/realtime/config"),
        ("PUT", "/api/providers/openrouter"),
    ] {
        let (status, body) = call(&app, Some(&token), method, path, json!({}));
        assert_eq!(
            status, 403,
            "{method} {path} must need the administrator token: {body}"
        );
    }
    assert_eq!(call(&app, None, "GET", "/api/providers", json!({})).0, 401);
    let (status, users) = call(&app, Some(&token), "GET", "/api/users", json!({}));
    assert_eq!(
        (
            status,
            users["users"].as_array().unwrap().len(),
            users["users"][0]["id"].clone()
        ),
        (200, 1, json!("u2")),
        "a user sees only their own profile"
    );
    let owner = call(&app, Some(&app.token), "GET", "/api/users", json!({})).1;
    assert_eq!(
        (
            owner["users"].as_array().unwrap().len(),
            owner["users"][0]["id"].clone()
        ),
        (1, json!("u1")),
        "unchanged for the administrator: the owner's profile"
    );
    let (status, agents) = call(&app, Some(&token), "GET", "/api/agents", json!({}));
    assert_eq!(status, 200);
    let ids: Vec<String> = agents["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["id"].as_str().map(str::to_owned))
        .collect();
    assert!(
        ids.contains(&"a2".to_owned()) && !ids.contains(&"a1".to_owned()),
        "{ids:?}"
    );
    assert_eq!(
        call(&app, Some(&token), "GET", "/api/office", json!({})).0,
        200
    );
    // No response ever carries a token hash, a password hash or a secret.
    for path in [
        "/api/users",
        "/api/agents",
        "/api/office",
        "/api/cron",
        "/api/notes",
    ] {
        let text = call(&app, Some(&token), "GET", path, json!({}))
            .1
            .to_string();
        assert!(
            !text.contains("api_token_hash")
                && !text.contains("password_hash")
                && !text.contains("ciphertext"),
            "{path}"
        );
    }
    let admin_users = call(&app, Some(&app.token), "GET", "/api/users", json!({}))
        .1
        .to_string();
    assert!(
        !admin_users.contains("api_token_hash") && !admin_users.contains(&App::hash_token(&token)),
        "the administrator's user list does not leak token hashes"
    );
}

#[test]
fn conversations_tasks_and_notes_are_isolated_between_users() {
    let (_home, app) = two_users();
    let (one, two) = (app.token.clone(), issue(&app, "u2"));
    let id_of = |reply: &(u16, Value)| reply.1["conversation"]["id"].as_str().unwrap().to_owned();
    let first = call(
        &app,
        Some(&one),
        "POST",
        "/api/conversations",
        json!({"title":"Del uno"}),
    );
    let second = call(
        &app,
        Some(&two),
        "POST",
        "/api/conversations",
        json!({"title":"Del dos"}),
    );
    assert_eq!((first.0, second.0), (201, 201));
    let (first_id, second_id) = (id_of(&first), id_of(&second));
    assert!(
        first_id.starts_with("u1/") && second_id.starts_with("u2/"),
        "{first_id} {second_id}"
    );
    let titles = |token: &str| -> Vec<String> {
        call(&app, Some(token), "GET", "/api/conversations", json!({})).1["conversations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["title"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(titles(&one), ["Del uno"]);
    assert_eq!(titles(&two), ["Del dos"]);
    assert_eq!(
        call(
            &app,
            Some(&two),
            "GET",
            &format!("/api/chat/history?threadId={first_id}"),
            json!({})
        )
        .0,
        404,
        "history of someone else's conversation is unknown, not forbidden"
    );
    assert_eq!(
        call(
            &app,
            Some(&two),
            "GET",
            &format!("/api/chat/history?threadId={second_id}"),
            json!({})
        )
        .0,
        200
    );
    // Scheduled tasks: each user sees and controls only their own; shared rows belong to the owner.
    for (id, user) in [("t1", "u1"), ("t2", "u2"), ("shared", "")] {
        app.db.col_put("cronJobs", id, &json!({"id":id,"user_id":user,"name":id,"status":"paused","task_type":"interval","interval_sec":3600,"task":"x"}), PutOptions::default()).unwrap();
    }
    let ids = |token: &str| -> Vec<String> {
        let mut ids: Vec<String> =
            call(&app, Some(token), "GET", "/api/cron", json!({})).1["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| t["id"].as_str().unwrap().to_owned())
                .collect();
        ids.sort();
        ids
    };
    assert_eq!(ids(&one), ["shared", "t1"]);
    assert_eq!(ids(&two), ["t2"]);
    for (method, path) in [
        ("GET", "/api/cron/t1"),
        ("GET", "/api/cron/t1/history"),
        ("POST", "/api/cron/t1/trigger"),
        ("POST", "/api/cron/t1/pause"),
        ("POST", "/api/cron/t1/resume"),
        ("DELETE", "/api/cron/t1"),
        ("GET", "/api/cron/shared"),
        ("DELETE", "/api/cron/shared"),
    ] {
        assert_eq!(
            call(&app, Some(&two), method, path, json!({})).0,
            404,
            "{method} {path} as the other user"
        );
    }
    assert!(
        app.db.col_get("cronJobs", "t1").unwrap().is_some()
            && app.db.col_get("cronJobs", "shared").unwrap().is_some(),
        "nothing of theirs was touched"
    );
    assert_eq!(
        call(&app, Some(&two), "GET", "/api/cron/t2", json!({})).0,
        200
    );
    assert_eq!(
        call(&app, Some(&two), "DELETE", "/api/cron/t2", json!({})).0,
        200
    );
    assert!(app.db.col_get("cronJobs", "t2").unwrap().is_none());
    // Notes and canvas surfaces follow the conversation owner and the surface owner.
    app.db.col_put("scratchpad", "n1", &json!({"id":"n1","threadId":first_id,"key":"k","value":"secreto del uno","source":"t","createdAt":1000,"updatedAt":1000}), PutOptions::default()).unwrap();
    app.db
        .col_put(
            "hive_rust_a2ui",
            "u1:s",
            &json!({"user_id":"u1","surfaceId":"s"}),
            PutOptions::default(),
        )
        .unwrap();
    assert!(
        call(&app, Some(&one), "GET", "/api/notes", json!({}))
            .1
            .to_string()
            .contains("secreto del uno")
    );
    assert!(
        !call(&app, Some(&two), "GET", "/api/notes", json!({}))
            .1
            .to_string()
            .contains("secreto del uno")
    );
    assert_eq!(
        call(
            &app,
            Some(&two),
            "DELETE",
            "/api/a2ui/surfaces/s",
            json!({})
        )
        .0,
        404
    );
    assert!(app.db.col_get("hive_rust_a2ui", "u1:s").unwrap().is_some());
}

#[test]
fn every_user_has_their_own_store_and_their_own_scheduler_queue() {
    let (_home, app) = two_users();
    let (store_one, store_two) = (
        app.store_handle("u1").unwrap(),
        app.store_handle("u2").unwrap(),
    );
    // Same tenant id twice: the same store; different users: different tenants.
    assert!(
        Arc::ptr_eq(&app.store_for("u1").unwrap(), &app.store),
        "the owner's store is the original one"
    );
    assert!(Arc::ptr_eq(
        &app.store_for("u2").unwrap(),
        &app.store_for("u2").unwrap()
    ));
    assert!(app.store_for("ghost").is_err());
    let objective = |store: &hive_agent_core::sessions::SessionStore, id: &str| {
        store.open_objective(id, Default::default())
    };
    objective(&store_one, "same-id").unwrap();
    objective(&store_two, "same-id").unwrap();
    assert!(
        store_two
            .objective_command("same-id", r#"{"op":"cancel"}"#)
            .is_ok()
    );
    let snapshot = |store: &hive_agent_core::sessions::SessionStore| {
        store
            .objective_command("same-id", r#"{"op":"inspect"}"#)
            .unwrap()
    };
    assert_ne!(
        snapshot(&store_one),
        snapshot(&store_two),
        "cancelling u2's objective did not touch u1's of the same name"
    );
    // Their cron schedules live in separate queues: the same task id never collides.
    let now = now();
    for user in ["u1", "u2"] {
        app.db.col_put("cronJobs", &format!("{user}-task"), &json!({"id":format!("{user}-task"),"user_id":user,"name":"t","status":"active","task_type":"interval","interval_sec":60,"next_run_at":crate::scheduler::stamp_for_tests(now + 60_000),"task":"x"}), PutOptions::default()).unwrap();
    }
    for user in ["u1", "u2"] {
        let queue = crate::scheduler::queue_for_tests(&app, user);
        crate::scheduler::sync_for_tests(&app, &queue, user);
        let schedules = queue.list_schedules(50, None).unwrap();
        let names: Vec<&str> = schedules["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), 1, "{user}: {names:?}");
        assert!(
            names[0].starts_with(&format!("cron.{user}-task.")),
            "{names:?}"
        );
    }
}

#[tokio::test]
async fn websocket_sessions_follow_the_token_and_a_user_cannot_open_anothers() {
    let (_home, app) = two_users();
    let user_token = issue(&app, "u2");
    let admin = app.token.clone();
    let (base, server) = serve(app.clone()).await;
    let url = |path: &str, query: &str| format!("{}{path}?{query}", base.replacen("http", "ws", 1));
    let welcome = |ws_url: String| async move {
        let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await.ok()?;
        let first = frame(&mut ws).await;
        Some(first["type"] == "welcome")
    };
    // A user's own session, with or without naming it.
    assert_eq!(
        welcome(url("/ws", &format!("session=u2&token={user_token}"))).await,
        Some(true)
    );
    assert_eq!(
        welcome(url("/ws", &format!("token={user_token}"))).await,
        Some(true)
    );
    // Someone else's session, the realtime and meeting streams, bad and missing tokens are refused.
    for refused in [
        url("/ws", &format!("session=u1&token={user_token}")),
        url("/realtime", &format!("session=u2&token={user_token}")),
        url(
            "/meeting-stream",
            &format!("meetingSessionId=m&token={user_token}"),
        ),
        url("/ws", "session=u2&token=hvu_wrong"),
        url("/ws", "session=u2"),
    ] {
        assert_eq!(welcome(refused.clone()).await, None, "{refused}");
    }
    // The administrator token still reaches any user's session, as before.
    assert_eq!(
        welcome(url("/ws", &format!("session=u2&token={admin}"))).await,
        Some(true)
    );
    assert_eq!(
        welcome(url("/ws", &format!("session=u1&token={admin}"))).await,
        Some(true)
    );
    // After revocation the user's token no longer opens anything.
    let revoked = call(
        &app,
        Some(&admin),
        "DELETE",
        "/api/users/u2/token",
        json!({}),
    );
    assert_eq!(revoked.0, 200);
    assert_eq!(
        welcome(url("/ws", &format!("session=u2&token={user_token}"))).await,
        None
    );
    server.abort();
}

// ---- tool policy and approvals -----------------------------------------------------

/// Asks for one panel tool, then answers once it has the tool's result. Keeps every request
/// so a test can read what the model was told about the call.
#[derive(Clone)]
struct OneToolFixture {
    step: Arc<Mutex<usize>>,
    seen: Arc<Mutex<Vec<String>>>,
}
impl OneToolFixture {
    fn new() -> Self {
        Self {
            step: Arc::new(Mutex::new(0)),
            seen: Arc::new(Mutex::new(vec![])),
        }
    }
}
impl TurnProvider for OneToolFixture {
    type Stream = FixtureStream;
    type Child = Self;
    fn child(&self, _: NativeProvider) -> Self {
        self.clone()
    }
    fn start(&self, request: ModelRequest) -> Self::Stream {
        self.seen
            .lock()
            .unwrap()
            .push(format!("{:?}", request.messages));
        let mut step = self.step.lock().unwrap();
        let first = *step == 0;
        *step += 1;
        let reply = if first {
            json!({"content":null,"tool_calls":[{"id":"create","name":"a2ui_create_surface","arguments":{"surfaceId":"form"}}],"usage":{"input_tokens":10,"output_tokens":2}})
        } else {
            json!({"content":"listo","tool_calls":[],"usage":{"input_tokens":10,"output_tokens":2}})
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
fn put_policy(app: &App, policy: Value, timeout_ms: u64) {
    let (status, body) = call(
        app,
        Some(&app.token),
        "PUT",
        "/api/tool-policy",
        json!({"policy":policy,"approval_timeout_ms":timeout_ms}),
    );
    assert_eq!(status, 200, "{body}");
}
async fn run_policy_turn(
    app: &Arc<App>,
    cancel: CancellationToken,
) -> (Result<()>, OneToolFixture, mpsc::Receiver<Value>) {
    let created = call(
        app,
        Some(&app.token),
        "POST",
        "/api/conversations",
        json!({}),
    )
    .1;
    let thread = created["conversation"]["id"].as_str().unwrap().to_owned();
    let selection = app.selection().unwrap();
    let handle = app.session(&thread, &selection).unwrap();
    let (output, receiver) = mpsc::channel(64);
    let fixture = OneToolFixture::new();
    let result = drive_turn(
        app.clone(),
        handle,
        fixture.clone(),
        &thread,
        "Crea un panel".into(),
        output,
        cancel,
    )
    .await;
    (result, fixture, receiver)
}
fn surface_exists(app: &App) -> bool {
    app.db
        .col_get("hive_rust_a2ui", "u1:form")
        .unwrap()
        .is_some()
}

#[tokio::test]
async fn a_denied_tool_is_not_run_and_the_model_is_told_why() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    put_policy(
        &app,
        json!({"default":"allow","rules":[{"tools":["a2ui_*"],"decision":"deny","reason":"panels are off"}]}),
        5000,
    );
    let (result, fixture, _rx) = run_policy_turn(&app, CancellationToken::new()).await;
    result.unwrap();
    assert!(!surface_exists(&app), "the denied tool did not run");
    {
        let seen = fixture.seen.lock().unwrap();
        assert!(
            seen.last()
                .unwrap()
                .contains("POLICY_DENIED: panels are off"),
            "{seen:?}"
        );
    }
    // Without a policy the very same turn runs the tool.
    assert_eq!(
        call(
            &app,
            Some(&app.token),
            "DELETE",
            "/api/tool-policy",
            json!({})
        )
        .0,
        200
    );
    run_policy_turn(&app, CancellationToken::new())
        .await
        .0
        .unwrap();
    assert!(surface_exists(&app));
}

#[tokio::test]
async fn an_ask_waits_for_the_person_and_runs_only_if_they_approve() {
    for (approve, ran) in [(true, true), (false, false)] {
        let home = tempfile::tempdir().unwrap();
        seed(home.path(), "http://127.0.0.1:9/v1");
        let app = Arc::new(App::open(home.path().to_owned()).unwrap());
        put_policy(
            &app,
            json!({"rules":[{"tools":["a2ui_create_surface"],"decision":"ask","reason":"creates UI"}]}),
            5000,
        );
        let thread = call(
            &app,
            Some(&app.token),
            "POST",
            "/api/conversations",
            json!({}),
        )
        .1["conversation"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let handle = app.session(&thread, &app.selection().unwrap()).unwrap();
        let (output, mut receiver) = mpsc::channel(64);
        let fixture = OneToolFixture::new();
        let turn_app = app.clone();
        let turn_thread = thread.clone();
        let task = tokio::spawn({
            let fixture = fixture.clone();
            async move {
                drive_turn(
                    turn_app,
                    handle,
                    fixture,
                    &turn_thread,
                    "Crea".into(),
                    output,
                    CancellationToken::new(),
                )
                .await
            }
        });
        // The request reaches the person whose turn it is, with what is being asked.
        let request = loop {
            let event = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            if event["type"] == "approval_request" {
                break event;
            }
        };
        assert_eq!(
            (
                request["tool"].clone(),
                request["reason"].clone(),
                request["arguments"]["surfaceId"].clone()
            ),
            (
                json!("a2ui_create_surface"),
                json!("creates UI"),
                json!("form")
            )
        );
        assert!(!surface_exists(&app), "nothing ran while waiting");
        let id = request["approvalId"].as_str().unwrap().to_owned();
        // Someone else cannot answer; the owner of the turn can, once.
        let stranger = crate::config::Principal {
            user: "u2".into(),
            admin: false,
        };
        assert!(!app.resolve_approval(&id, &stranger, true).unwrap());
        let owner = crate::config::Principal {
            user: "u1".into(),
            admin: false,
        };
        assert!(app.resolve_approval(&id, &owner, approve).unwrap());
        assert!(
            !app.resolve_approval(&id, &owner, approve).unwrap(),
            "an approval can only be answered once"
        );
        task.await.unwrap().unwrap();
        assert_eq!(surface_exists(&app), ran, "approve={approve}");
        let seen = fixture.seen.lock().unwrap().last().unwrap().clone();
        assert_eq!(seen.contains("approval refused"), !ran, "{seen}");
        assert!(
            app.approvals.lock().unwrap().is_empty(),
            "no pending approval is left behind"
        );
    }
}

/// The action a client sends back when the person presses one of the two buttons of an
/// approval surface, built from the components the gateway drew.
fn pressed(frames: &[Value], button: &str) -> Value {
    let surface = frames
        .iter()
        .find(|f| f["type"] == "a2ui:createSurface")
        .expect("the surface was drawn")["data"]["surfaceId"]
        .as_str()
        .unwrap()
        .to_owned();
    let components = frames
        .iter()
        .find(|f| f["type"] == "a2ui:updateComponents")
        .expect("with its components")["data"]["components"]
        .as_array()
        .unwrap()
        .clone();
    let source = components
        .iter()
        .find(|c| c["id"] == button)
        .expect("the button exists");
    json!({"name":source["action"]["event"]["name"],"surfaceId":surface,"sourceComponentId":button,
        "context":source["action"]["event"]["context"]})
}

#[tokio::test]
async fn an_ask_also_draws_an_a2ui_surface_whose_buttons_answer_it_and_which_goes_away_after() {
    for (button, ran) in [("approve", true), ("deny", false)] {
        let home = tempfile::tempdir().unwrap();
        seed(home.path(), "http://127.0.0.1:9/v1");
        let app = Arc::new(App::open(home.path().to_owned()).unwrap());
        put_policy(
            &app,
            json!({"rules":[{"tools":["a2ui_create_surface"],"decision":"ask","reason":"creates UI"}]}),
            5000,
        );
        let thread = call(
            &app,
            Some(&app.token),
            "POST",
            "/api/conversations",
            json!({}),
        )
        .1["conversation"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let handle = app.session(&thread, &app.selection().unwrap()).unwrap();
        let (output, mut receiver) = mpsc::channel(64);
        let fixture = OneToolFixture::new();
        let task = tokio::spawn({
            let (app, fixture, thread) = (app.clone(), fixture.clone(), thread.clone());
            async move {
                drive_turn(
                    app,
                    handle,
                    fixture,
                    &thread,
                    "Crea".into(),
                    output,
                    CancellationToken::new(),
                )
                .await
            }
        });
        // The legacy request and the three A2UI frames arrive, in this order, for this thread.
        let mut seen: Vec<Value> = vec![];
        while !seen.iter().any(|f| f["type"] == "a2ui:updateDataModel") {
            let event = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
                .await
                .unwrap()
                .unwrap();
            if [
                "approval_request",
                "a2ui:createSurface",
                "a2ui:updateComponents",
                "a2ui:updateDataModel",
            ]
            .contains(&event["type"].as_str().unwrap_or_default())
            {
                seen.push(event);
            }
        }
        let types: Vec<&str> = seen.iter().map(|f| f["type"].as_str().unwrap()).collect();
        assert_eq!(
            types,
            [
                "approval_request",
                "a2ui:createSurface",
                "a2ui:updateComponents",
                "a2ui:updateDataModel"
            ]
        );
        let id = seen[0]["approvalId"].as_str().unwrap().to_owned();
        assert!(
            seen[1..].iter().all(|f| f["threadId"] == json!(thread)),
            "drawn in the thread that is asking"
        );
        assert_eq!(
            seen[1]["data"]["surfaceId"],
            json!(format!("hive-approval-{id}"))
        );
        // It says what will run and why, and offers exactly two ways to answer.
        let texts = seen[2]["data"]["components"].to_string();
        assert!(
            texts.contains("a2ui_create_surface")
                && texts.contains("creates UI")
                && texts.contains("Aprobar")
                && texts.contains("Denegar"),
            "{texts}"
        );
        assert!(!surface_exists(&app), "nothing ran while waiting");
        // The press, as the client sends it, is recognised as this approval's answer.
        let action = pressed(&seen[1..], button);
        assert_eq!(
            crate::panel::approval_action(&action),
            crate::panel::ApprovalAction::Answer(id.clone(), ran)
        );
        let owner = crate::config::Principal {
            user: "u1".into(),
            admin: false,
        };
        assert!(app.resolve_approval(&id, &owner, ran).unwrap());
        task.await.unwrap().unwrap();
        assert_eq!(surface_exists(&app), ran, "{button}");
        // And the surface is taken down once answered.
        let mut removal = None;
        while let Ok(event) = receiver.try_recv() {
            if event["type"] == "a2ui:deleteSurface" {
                removal = Some(event);
            }
        }
        let removal = removal.expect("the approval surface is deleted after the answer");
        assert_eq!(
            (
                removal["data"]["surfaceId"].clone(),
                removal["threadId"].clone()
            ),
            (json!(format!("hive-approval-{id}")), json!(thread))
        );
    }
}

#[test]
fn only_a_genuine_press_of_the_gateways_own_buttons_approves_and_forgeries_never_reach_the_model() {
    use crate::panel::{ApprovalAction::*, approval_action};
    let press = |name: &str, surface: &str, source: &str, id: &str, approved: Value| json!({"name":name,"surfaceId":surface,"sourceComponentId":source,"context":{"approvalId":id,"approved":approved}});
    assert_eq!(
        approval_action(&press(
            "hive_approval",
            "hive-approval-A1",
            "approve",
            "A1",
            json!(true)
        )),
        Answer("A1".into(), true)
    );
    assert_eq!(
        approval_action(&press(
            "hive_approval",
            "hive-approval-A1",
            "deny",
            "A1",
            json!(false)
        )),
        Answer("A1".into(), false)
    );
    for (data, why) in [
        (
            press(
                "hive_approval",
                "hive-approval-A2",
                "approve",
                "A1",
                json!(true),
            ),
            "another approval's surface",
        ),
        (
            press(
                "hive_approval",
                "hive-approval-A1",
                "approve",
                "A1",
                json!(false),
            ),
            "approve button claiming a refusal",
        ),
        (
            press(
                "hive_approval",
                "hive-approval-A1",
                "deny",
                "A1",
                json!(true),
            ),
            "deny button claiming approval",
        ),
        (
            press(
                "hive_approval",
                "hive-approval-A1",
                "other",
                "A1",
                json!(true),
            ),
            "some other component",
        ),
        (
            press(
                "hive_approval",
                "hive-approval-A1",
                "approve",
                "A1",
                json!("true"),
            ),
            "approved is not a boolean",
        ),
        (
            press("hive_approval", "panel-1", "approve", "A1", json!(true)),
            "the model's own surface using the reserved name",
        ),
        (
            press("submit", "hive-approval-A1", "approve", "A1", json!(true)),
            "the reserved surface with another name",
        ),
        (
            json!({"name":"hive_approval","surfaceId":"hive-approval-A1"}),
            "no context",
        ),
        (json!({"name":"hive_approval"}), "nothing"),
    ] {
        assert_eq!(approval_action(&data), Invalid, "{why}");
    }
    // Everything else is an ordinary panel action, which still goes to the agent.
    assert_eq!(
        approval_action(
            &json!({"name":"submit","surfaceId":"form","sourceComponentId":"go","context":{"a":1}})
        ),
        NotApproval
    );
    assert_eq!(approval_action(&json!({})), NotApproval);
}

#[test]
fn the_model_cannot_create_change_or_delete_a_surface_named_like_an_approval() {
    let (_home, app) = two_users();
    for tool in [
        "a2ui_create_surface",
        "a2ui_update_components",
        "a2ui_update_data_model",
        "a2ui_delete_surface",
    ] {
        let args = json!({"surfaceId":"hive-approval-anything","components":[],"value":"{}"});
        let result = crate::panel::execute(
            &app,
            "u1",
            "u1/webchat/t",
            &json!({"id":"a"}),
            tool,
            &args,
            "exec",
        );
        assert!(result.is_err(), "{tool} must refuse the reserved prefix");
    }
    // An ordinary id still works.
    assert!(
        crate::panel::execute(
            &app,
            "u1",
            "u1/webchat/t",
            &json!({"id":"a"}),
            "a2ui_create_surface",
            &json!({"surfaceId":"form"}),
            "exec"
        )
        .is_ok()
    );
}

#[tokio::test]
async fn pressing_the_button_over_the_real_websocket_resolves_the_approval_and_a_forgery_is_refused()
 {
    let (_home, app) = two_users();
    let (base, server) = serve(app.clone()).await;
    let url = format!(
        "{}/ws?session=u1&token={}",
        base.replacen("http", "ws", 1),
        app.token
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    assert_eq!(frame(&mut ws).await["type"], "welcome");
    let (answer, mut answered) = tokio::sync::oneshot::channel();
    app.approvals.lock().unwrap().insert(
        "ap-1".into(),
        crate::config::PendingApproval {
            user: "u1".into(),
            answer,
        },
    );
    // An error comes as two frames: the error itself and the closing `process` status.
    async fn error_of<S: AsyncRead + AsyncWrite + Unpin>(ws: &mut WebSocketStream<S>) -> Value {
        let error = frame(ws).await;
        assert_eq!(error["type"], "error", "{error}");
        assert_eq!(frame(ws).await["processStatus"], "error");
        error
    }
    let action = |name: &str, surface: &str, source: &str, id: &str, approved: bool| {
        json!({"type":"a2ui:action","data":{"name":name,"surfaceId":surface,"sourceComponentId":source,"context":{"approvalId":id,"approved":approved}}}).to_string()
    };
    // A forged press (the reserved name on a surface the model could have made) is an error and resolves nothing.
    ws.send(Message::Text(action(
        "hive_approval",
        "panel-1",
        "approve",
        "ap-1",
        true,
    )))
    .await
    .unwrap();
    error_of(&mut ws).await;
    assert!(answered.try_recv().is_err(), "nothing was answered");
    // The genuine press resolves it as approved, without producing a model turn.
    ws.send(Message::Text(action(
        "hive_approval",
        "hive-approval-ap-1",
        "approve",
        "ap-1",
        true,
    )))
    .await
    .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), &mut answered)
            .await
            .unwrap()
            .unwrap(),
        "the press approved it"
    );
    // Pressing again is refused: an approval is answered once.
    ws.send(Message::Text(action(
        "hive_approval",
        "hive-approval-ap-1",
        "approve",
        "ap-1",
        true,
    )))
    .await
    .unwrap();
    let again = error_of(&mut ws).await;
    assert!(
        again["error"].as_str().unwrap().contains("ya resuelta"),
        "{again}"
    );
    // Someone else's pending approval cannot be answered from this session.
    let (other, mut other_answer) = tokio::sync::oneshot::channel();
    app.approvals.lock().unwrap().insert(
        "ap-2".into(),
        crate::config::PendingApproval {
            user: "u2".into(),
            answer: other,
        },
    );
    ws.send(Message::Text(action(
        "hive_approval",
        "hive-approval-ap-2",
        "approve",
        "ap-2",
        true,
    )))
    .await
    .unwrap();
    error_of(&mut ws).await;
    assert!(other_answer.try_recv().is_err());
    server.abort();
}

#[tokio::test]
async fn an_unanswered_ask_times_out_and_denies_and_cancelling_the_turn_clears_it() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    put_policy(&app, json!({"default":"ask"}), 1000);
    let started = std::time::Instant::now();
    let (result, fixture, _rx) = run_policy_turn(&app, CancellationToken::new()).await;
    result.unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "it really waited"
    );
    assert!(!surface_exists(&app));
    assert!(
        fixture
            .seen
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("APPROVAL_TIMEOUT")
    );
    assert!(app.approvals.lock().unwrap().is_empty());
    // Cancelling while waiting ends the turn and leaves no pending approval.
    put_policy(&app, json!({"default":"ask"}), 60_000);
    let cancel = CancellationToken::new();
    let stopper = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        stopper.cancel();
    });
    let (result, _fixture, _rx) = run_policy_turn(&app, cancel).await;
    assert!(result.is_err());
    assert!(!surface_exists(&app));
    assert!(app.approvals.lock().unwrap().is_empty());
    // A turn nobody is connected to cannot be approved: denied at once, not after the timeout.
    let thread = call(
        &app,
        Some(&app.token),
        "POST",
        "/api/conversations",
        json!({}),
    )
    .1["conversation"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let handle = app.session(&thread, &app.selection().unwrap()).unwrap();
    let (output, receiver) = mpsc::channel(64);
    drop(receiver);
    let fixture = OneToolFixture::new();
    let began = std::time::Instant::now();
    let _ = drive_turn(
        app.clone(),
        handle,
        fixture.clone(),
        &thread,
        "x".into(),
        output,
        CancellationToken::new(),
    )
    .await;
    assert!(began.elapsed() < Duration::from_secs(5));
    assert!(!surface_exists(&app));
}

#[tokio::test]
async fn an_unreadable_policy_blocks_tools_instead_of_opening_them() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    app.db
        .col_put(
            "toolPolicy",
            "default",
            &json!({"id":"default","policy":{"rules":[{"tools":[],"decision":"allow"}]}}),
            PutOptions::default(),
        )
        .unwrap();
    let (result, fixture, _rx) = run_policy_turn(&app, CancellationToken::new()).await;
    result.unwrap();
    assert!(!surface_exists(&app));
    assert!(
        fixture
            .seen
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("POLICY_UNAVAILABLE")
    );
}

#[test]
fn the_policy_api_validates_is_admin_only_and_approvals_have_a_rest_twin() {
    let (_home, app) = two_users();
    let user = issue(&app, "u2");
    assert_eq!(
        call(&app, Some(&user), "GET", "/api/tool-policy", json!({})).0,
        403
    );
    assert_eq!(
        call(
            &app,
            Some(&user),
            "PUT",
            "/api/tool-policy",
            json!({"policy":{}})
        )
        .0,
        403
    );
    assert_eq!(
        call(&app, None, "GET", "/api/tool-policy", json!({})).0,
        401
    );
    assert_eq!(
        call(&app, Some(&app.token), "GET", "/api/tool-policy", json!({})).1["policy"],
        Value::Null
    );
    for bad in [
        json!({"policy":{"default":"maybe"}}),
        json!({"policy":{"rules":[{"tools":[],"decision":"deny"}]}}),
        json!({"policy":{},"approval_timeout_ms":5}),
        json!({"policy":{},"approval_timeout_ms":99999999}),
    ] {
        assert_eq!(
            call(
                &app,
                Some(&app.token),
                "PUT",
                "/api/tool-policy",
                bad.clone()
            )
            .0,
            400,
            "{bad}"
        );
    }
    assert!(
        app.db.col_get("toolPolicy", "default").unwrap().is_none(),
        "an invalid policy is never stored"
    );
    put_policy(
        &app,
        json!({"default":"deny","default_reason":"closed"}),
        2500,
    );
    let shown = call(&app, Some(&app.token), "GET", "/api/tool-policy", json!({})).1;
    assert_eq!(
        (
            shown["policy"]["default"].clone(),
            shown["approval_timeout_ms"].clone()
        ),
        (json!("deny"), json!(2500))
    );
    // Replacing it works (versioned write), clearing it too.
    put_policy(&app, json!({"default":"allow"}), 1000);
    assert_eq!(
        call(&app, Some(&app.token), "GET", "/api/tool-policy", json!({})).1["policy"]["default"],
        "allow"
    );
    assert_eq!(
        call(
            &app,
            Some(&app.token),
            "DELETE",
            "/api/tool-policy",
            json!({})
        )
        .0,
        200
    );
    assert!(app.tool_policy().unwrap().is_none());
    // REST approvals: a user answers their own, not another's, and unknown ids are 404.
    let (answer, mut response) = tokio::sync::oneshot::channel();
    app.approvals.lock().unwrap().insert(
        "ap1".into(),
        crate::config::PendingApproval {
            user: "u1".into(),
            answer,
        },
    );
    assert_eq!(
        call(
            &app,
            Some(&user),
            "POST",
            "/api/approvals/ap1",
            json!({"approved":true})
        )
        .0,
        404,
        "u2 cannot answer u1's approval"
    );
    assert_eq!(
        call(
            &app,
            Some(&user),
            "POST",
            "/api/approvals/ghost",
            json!({"approved":true})
        )
        .0,
        404
    );
    assert!(response.try_recv().is_err(), "still pending");
    assert_eq!(
        call(
            &app,
            Some(&app.token),
            "POST",
            "/api/approvals/ap1",
            json!({"approved":true})
        )
        .0,
        200
    );
    assert!(response.try_recv().unwrap());
    assert_eq!(
        call(
            &app,
            Some(&app.token),
            "GET",
            "/api/approvals/ap1",
            json!({})
        )
        .0,
        405
    );
}

// ---- Telegram ----------------------------------------------------------------------

/// A fake Telegram Bot API: serves the scripted updates to `getUpdates` (honouring `offset`)
/// and records every request, including `sendMessage`.
/// Files a fake platform serves, by path: `(content type, bytes)`; and who asked for which.
#[derive(Clone, Default)]
struct FileStore {
    files: Arc<Mutex<SlackFiles>>,
    asked: Arc<Mutex<Vec<String>>>,
}
impl FileStore {
    fn put(&self, path: &str, content_type: &str, bytes: Vec<u8>) {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_owned(), (content_type.to_owned(), bytes));
    }
    /// `path:authorization` of every file request, in order.
    fn requests(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
    /// Serve `path` if it is under `prefix`; true when the request was a file request.
    async fn serve(&self, socket: &mut TcpStream, prefix: &str, path: &str, auth: &str) -> bool {
        let Some(name) = path.strip_prefix(prefix) else {
            return false;
        };
        self.asked.lock().unwrap().push(format!("{name}:{auth}"));
        let found = self.files.lock().unwrap().get(name).cloned();
        let (status, content_type, bytes) = match found {
            Some((content_type, bytes)) => ("200 OK", content_type, bytes),
            None => ("404 Not Found", "text/plain".to_owned(), vec![]),
        };
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            bytes.len()
        );
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(&bytes).await;
        true
    }
}

struct FakeTelegram {
    base: String,
    updates: Arc<Mutex<Vec<Value>>>,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
    /// `file_id → file_path` for `getFile`, and the files themselves under `/file/bot<token>/`.
    file_ids: Arc<Mutex<std::collections::HashMap<String, String>>>,
    store: FileStore,
    task: tokio::task::JoinHandle<()>,
}
async fn fake_telegram() -> FakeTelegram {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let updates: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
    let requests: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(vec![]));
    let file_ids: Arc<Mutex<std::collections::HashMap<String, String>>> = Arc::default();
    let store = FileStore::default();
    let (served, seen, ids, files) = (
        updates.clone(),
        requests.clone(),
        file_ids.clone(),
        store.clone(),
    );
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (served, seen, ids, files) =
                (served.clone(), seen.clone(), ids.clone(), files.clone());
            tokio::spawn(async move {
                let Ok(request) = read_request(&mut socket).await else {
                    return;
                };
                let path = request.uri().path().to_owned();
                if let Some(rest) = path.strip_prefix("/file/bot") {
                    // `/file/bot<token>/<file_path>`: the token is the first segment.
                    let name = rest.split_once('/').map_or("", |(_, name)| name);
                    files
                        .serve(
                            &mut socket,
                            "",
                            name,
                            &format!("token-in-url={}", rest.starts_with("123:SECRET")),
                        )
                        .await;
                    return;
                }
                let body: Value = serde_json::from_slice(request.body()).unwrap_or(Value::Null);
                seen.lock().unwrap().push((path.clone(), body.clone()));
                let reply = if path.ends_with("/getFile") {
                    match ids
                        .lock()
                        .unwrap()
                        .get(body["file_id"].as_str().unwrap_or_default())
                    {
                        Some(file_path) => {
                            json!({"ok":true,"result":{"file_id":body["file_id"],"file_path":file_path}})
                        }
                        None => json!({"ok":false,"description":"Bad Request: file not found"}),
                    }
                } else if path.ends_with("/getUpdates") {
                    let offset = body["offset"].as_i64().unwrap_or(0);
                    let ready: Vec<Value> = served
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|u| u["update_id"].as_i64().unwrap_or(0) >= offset)
                        .cloned()
                        .collect();
                    if ready.is_empty() {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    json!({"ok":true,"result":ready})
                } else {
                    json!({"ok":true,"result":{"message_id":1}})
                };
                let _ = respond(&mut socket, 200, reply).await;
            });
        }
    });
    FakeTelegram {
        base,
        updates,
        requests,
        file_ids,
        store,
        task,
    }
}
impl FakeTelegram {
    /// A file Telegram would serve for `file_id` at `file_path`.
    fn file(&self, file_id: &str, file_path: &str, content_type: &str, bytes: Vec<u8>) {
        self.file_ids
            .lock()
            .unwrap()
            .insert(file_id.to_owned(), file_path.to_owned());
        self.store.put(file_path, content_type, bytes);
    }
    /// An update whose message is `extra` (photo, document, caption…) merged into a basic message.
    fn push_media(&self, update_id: i64, chat: i64, extra: Value) {
        let mut message = json!({"message_id":update_id,"chat":{"id":chat,"type":"private"},"from":{"id":chat,"is_bot":false}});
        for (key, value) in extra.as_object().unwrap() {
            message[key] = value.clone();
        }
        self.updates
            .lock()
            .unwrap()
            .push(json!({"update_id":update_id,"message":message}));
    }
    fn push(&self, update_id: i64, chat: i64, text: &str, bot: bool) {
        self.updates.lock().unwrap().push(json!({"update_id":update_id,"message":{"message_id":update_id,"chat":{"id":chat,"type":"private"},"from":{"id":chat,"is_bot":bot},"text":text}}));
    }
    fn sent(&self) -> Vec<(i64, String)> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path.ends_with("/sendMessage"))
            .map(|(_, body)| {
                (
                    body["chat_id"].as_i64().unwrap(),
                    body["text"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }
    fn polls(&self, token: &str) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == &format!("/bot{token}/getUpdates"))
            .map(|(_, body)| body.clone())
            .collect()
    }
}
async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while !check() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for: {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
fn add_telegram(app: &App, id: &str, user: &str, config: Value) {
    app.db.col_put("channels", id, &json!({"id":id,"type":"telegram","user_id":user,"enabled":true,"active":true,"config":"{}"}), PutOptions::default()).unwrap();
    config::store_secret(app, &format!("channel:{id}:config"), &config.to_string()).unwrap();
    app.channel_wakeup.notify_one();
}

/// A provider that asks for the `a2ui_create_surface` tool on the first request and says
/// "Listo" once it has the tool's result.
async fn tool_provider() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let Ok(request) = read_request(&mut socket).await else {
                    return;
                };
                let body: Value = serde_json::from_slice(request.body()).unwrap();
                let has_result = body["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|m| m["role"] == "tool");
                let chunks = if has_result {
                    vec![
                        json!({"choices":[{"index":0,"delta":{"content":"Listo"},"finish_reason":null}]}),
                        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                    ]
                } else {
                    vec![
                        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"a2ui_create_surface","arguments":"{\"surfaceId\":\"form\"}"}}]},"finish_reason":null}]}),
                        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
                    ]
                };
                let mut sse = String::new();
                for value in chunks.into_iter().chain([
                    json!({"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
                ]) {
                    sse.push_str(&format!("data: {value}\n\n"));
                }
                sse.push_str("data: [DONE]\n\n");
                let headers = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    sse.len()
                );
                let _ = socket.write_all(headers.as_bytes()).await;
                let _ = socket.write_all(sse.as_bytes()).await;
            });
        }
    });
    (base, task)
}

#[test]
fn only_a_plain_yes_or_no_answers_an_approval_and_a_sentence_that_contains_one_does_not() {
    use crate::channels::verdict;
    for yes in [
        "sí", "Sí", "si", " SI ", "s", "yes", "Yes!", "ok", "OK.", "aprobar", "Apruebo", "aprobado",
    ] {
        assert_eq!(verdict(yes), Some(true), "{yes:?}");
    }
    for no in [
        "no", "No", "NO.", "n", "denegar", "deniego", "rechazar", "cancelar", "denegado",
    ] {
        assert_eq!(verdict(no), Some(false), "{no:?}");
    }
    for other in [
        "",
        "  ",
        "sí, pero antes dime qué hace",
        "no lo sé",
        "claro",
        "dale",
        "yes please",
        "ya",
        "tal vez",
        "no no no",
        "sí\nno",
        "aprobar todo",
        "👍",
    ] {
        assert_eq!(verdict(other), None, "{other:?} is not a plain yes or no");
    }
}

#[tokio::test]
async fn a_telegram_chat_is_asked_for_an_approval_and_its_next_yes_or_no_decides_without_the_model()
{
    for (answer, ran) in [("sí", true), ("No", false)] {
        let (llm_url, llm) = tool_provider().await;
        let home = tempfile::tempdir().unwrap();
        seed(home.path(), &llm_url);
        let app = Arc::new(App::open(home.path().to_owned()).unwrap());
        put_policy(
            &app,
            json!({"rules":[{"tools":["a2ui_create_surface"],"decision":"ask","reason":"crea una interfaz"}]}),
            30_000,
        );
        let telegram = fake_telegram().await;
        add_telegram(
            &app,
            "tg1",
            "u1",
            json!({"bot_token":"123:SECRET","allowed_chat_ids":[42],"api_base":telegram.base,"poll_timeout_s":1}),
        );
        let supervisor = tokio::spawn(crate::telegram::run(app.clone()));
        telegram.push(10, 42, "crea el formulario", false);
        // The chat is asked, in plain text, what is about to run and why.
        eventually("the chat is asked", || {
            telegram
                .sent()
                .iter()
                .any(|(_, text)| text.contains("Responde «sí»"))
        })
        .await;
        let question = telegram.sent()[0].1.clone();
        assert!(
            question.contains("a2ui_create_surface")
                && question.contains("crea una interfaz")
                && question.contains("\"surfaceId\": \"form\""),
            "{question}"
        );
        assert!(!surface_exists(&app), "nothing ran while it waits");
        assert_eq!(app.approvals.lock().unwrap().len(), 1);
        // A message that is not a plain yes or no is a reminder, not an answer and not a new turn.
        telegram.push(11, 42, "sí, pero primero dime algo", false);
        eventually("the chat is reminded", || {
            telegram
                .sent()
                .iter()
                .any(|(_, text)| text.contains("esperando tu respuesta"))
        })
        .await;
        assert!(
            !surface_exists(&app) && app.approvals.lock().unwrap().len() == 1,
            "still waiting"
        );
        // Another chat's yes cannot answer it (it is not even allowed to talk).
        telegram.push(12, 99, "sí", false);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            app.approvals.lock().unwrap().len(),
            1,
            "a stranger decides nothing"
        );
        // The person decides.
        telegram.push(13, 42, answer, false);
        eventually("the turn finishes", || {
            telegram.sent().iter().any(|(_, text)| text == "Listo")
        })
        .await;
        let sent: Vec<String> = telegram.sent().into_iter().map(|(_, text)| text).collect();
        assert!(
            sent.iter().any(|t| t
                == if ran {
                    "Aprobado. Continúo."
                } else {
                    "Denegado."
                }),
            "{sent:?}"
        );
        assert_eq!(surface_exists(&app), ran, "answer={answer}");
        assert!(
            app.approvals.lock().unwrap().is_empty()
                && app.chat_approvals.lock().unwrap().is_empty(),
            "nothing is left pending"
        );
        // Afterwards the same words are ordinary messages again.
        telegram.push(14, 42, "sí", false);
        eventually("an ordinary sí is a normal turn", || {
            telegram
                .sent()
                .iter()
                .filter(|(_, text)| text == "Listo")
                .count()
                == 2
        })
        .await;
        supervisor.abort();
        telegram.task.abort();
        llm.abort();
    }
}

#[tokio::test]
async fn every_channel_shares_the_approval_question_and_an_expired_one_stops_catching_answers() {
    use crate::channels::{Answer, ask_agent};
    let (llm_url, llm) = tool_provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    put_policy(
        &app,
        json!({"rules":[{"tools":["a2ui_create_surface"],"decision":"ask"}]}),
        30_000,
    );
    for kind in ["discord", "slack", "whatsapp", "whatsapp_qr"] {
        let _ = std::fs::remove_dir_all(home.path().join("x"));
        let asked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
        let first = tokio::spawn({
            let (app, asked) = (app.clone(), asked.clone());
            async move {
                ask_agent(
                    &app,
                    "u1",
                    kind,
                    "room-1",
                    "t",
                    "crea",
                    |question: String| {
                        let asked = asked.clone();
                        async move { asked.lock().unwrap().push(question) }
                    },
                )
                .await
            }
        });
        eventually("the question is relayed to the chat", || {
            !asked.lock().unwrap().is_empty()
        })
        .await;
        assert!(
            asked.lock().unwrap()[0].contains("a2ui_create_surface"),
            "{kind}"
        );
        // A different chat's yes (same kind, other peer) has no question waiting for it.
        assert!(
            crate::channels::answer_pending(&app, "u1", kind, "room-2", "sí").is_none(),
            "{kind}: another room cannot approve"
        );
        assert_eq!(
            app.approvals.lock().unwrap().len(),
            1,
            "{kind}: still waiting for room-1"
        );
        // This chat's yes decides it, and the model never sees that message.
        let reply = ask_agent(&app, "u1", kind, "room-1", "t", "sí", |_: String| async {}).await;
        assert!(
            matches!(&reply, Answer::Text(text) if text == "Aprobado. Continúo."),
            "{kind}"
        );
        let done = first.await.unwrap();
        assert!(
            matches!(&done, Answer::Text(text) if text == "Listo"),
            "{kind}"
        );
        assert!(app.chat_approvals.lock().unwrap().is_empty());
        // Nothing is waiting any more: the same word is an ordinary message again.
        assert!(crate::channels::answer_pending(&app, "u1", kind, "room-1", "sí").is_none());
        let _ = app
            .db
            .col_delete("conversationThreads", &format!("u1/{kind}/room-1"));
        let _ = app
            .db
            .col_delete("conversationThreads", &format!("u1/{kind}/room-2"));
    }
    // A question nobody answered in time is denied by the gateway, and the stale entry is dropped
    // so a later "sí" is not swallowed as an answer.
    put_policy(
        &app,
        json!({"rules":[{"tools":["a2ui_create_surface"],"decision":"ask"}]}),
        1000,
    );
    let asked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(vec![]));
    let done = ask_agent(
        &app,
        "u1",
        "discord",
        "room-9",
        "t",
        "crea",
        |question: String| {
            let asked = asked.clone();
            async move { asked.lock().unwrap().push(question) }
        },
    )
    .await;
    assert!(matches!(&done, Answer::Text(text) if text == "Listo"));
    assert_eq!(asked.lock().unwrap().len(), 1, "it was asked once");
    assert!(
        app.chat_approvals.lock().unwrap().is_empty(),
        "the finished turn leaves no pending question"
    );
    app.chat_approvals
        .lock()
        .unwrap()
        .insert("discord:room-9".into(), "gone".into());
    assert!(
        crate::channels::answer_pending(&app, "u1", "discord", "room-9", "sí").is_none(),
        "a stale question is not an answer"
    );
    assert!(
        app.chat_approvals.lock().unwrap().is_empty(),
        "and it is cleaned up"
    );
    llm.abort();
}

#[tokio::test]
async fn telegram_answers_only_allowed_chats_through_the_agent_and_never_twice() {
    let (llm_url, llm_seen, llm) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let telegram = fake_telegram().await;
    telegram.push(10, 42, "hola desde Telegram", false);
    telegram.push(11, 99, "soy un intruso", false);
    telegram.push(12, 42, "mensaje de otro bot", true);
    add_telegram(
        &app,
        "tg1",
        "u1",
        json!({"bot_token":"123:SECRET","allowed_chat_ids":[42],"api_base":telegram.base,"poll_timeout_s":1}),
    );
    let supervisor = tokio::spawn(crate::telegram::run(app.clone()));
    eventually("the allowed chat gets the agent's answer", || {
        telegram
            .sent()
            .iter()
            .any(|(chat, text)| *chat == 42 && text == "Hola Rust")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        telegram.sent(),
        [(42, "Hola Rust".to_owned())],
        "nobody else is answered, bots included"
    );
    assert_eq!(
        llm_seen.lock().unwrap().len(),
        1,
        "the model was asked once: strangers and bots never reach it"
    );
    assert!(
        llm_seen.lock().unwrap()[0]["messages"]
            .to_string()
            .contains("hola desde Telegram")
    );
    // The conversation lives under the channel's owner, labelled as Telegram.
    let thread = app
        .db
        .col_get("conversationThreads", "u1/telegram/42")
        .unwrap()
        .unwrap()
        .doc;
    assert_eq!(
        (thread["channel"].clone(), thread["user_id"].clone()),
        (json!("telegram"), json!("u1"))
    );
    assert!(
        app.db
            .col_get("conversationThreads", "u1/telegram/99")
            .unwrap()
            .is_none()
    );
    // At most once: the offset moved past every update before they were handled, and persisted.
    assert_eq!(
        app.db.col_get("channelState", "tg1").unwrap().unwrap().doc["telegram_offset"],
        13
    );
    eventually("the poller asks from the new offset", || {
        telegram
            .polls("123:SECRET")
            .iter()
            .any(|body| body["offset"] == 13)
    })
    .await;
    // A second message is a second turn in the same conversation.
    telegram.push(13, 42, "otra vez", false);
    eventually("the second message is answered", || {
        telegram.sent().len() == 2
    })
    .await;
    assert_eq!(llm_seen.lock().unwrap().len(), 2);
    // Status shows the connection and never the token.
    let status = app
        .channel_status
        .lock()
        .unwrap()
        .get("tg1")
        .cloned()
        .unwrap();
    assert_eq!(status["status"], "connected");
    assert!(!status.to_string().contains("SECRET"));
    // Disabling the channel stops the poller.
    app.db.col_put("channels", "tg1", &json!({"id":"tg1","type":"telegram","user_id":"u1","enabled":false,"active":true,"config":"{}"}), PutOptions::default()).unwrap();
    app.channel_wakeup.notify_one();
    eventually("the channel is disconnected", || {
        app.channel_status
            .lock()
            .unwrap()
            .get("tg1")
            .is_some_and(|s| s["status"] == "disconnected")
    })
    .await;
    let after = telegram.polls("123:SECRET").len();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        telegram.polls("123:SECRET").len() <= after + 1,
        "no longer polling"
    );
    app.shutdown.cancel();
    let _ = supervisor.await;
    telegram.task.abort();
    llm.abort();
}

#[tokio::test]
async fn a_channel_without_an_allowlist_never_polls_and_errors_never_leak_to_the_chat_or_the_status()
 {
    let (llm_url, _llm_seen, llm) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let telegram = fake_telegram().await;
    add_telegram(
        &app,
        "open",
        "u1",
        json!({"bot_token":"999:OPEN","api_base":telegram.base}),
    );
    telegram.push(1, 7, "fail", false);
    add_telegram(
        &app,
        "tg2",
        "u1",
        json!({"bot_token":"456:FAILS","allowed_chat_ids":[7],"api_base":telegram.base,"poll_timeout_s":1}),
    );
    let supervisor = tokio::spawn(crate::telegram::run(app.clone()));
    eventually("the misconfigured channel reports its problem", || {
        app.channel_status
            .lock()
            .unwrap()
            .get("open")
            .is_some_and(|s| s["status"] == "error")
    })
    .await;
    assert!(
        app.channel_status.lock().unwrap().get("open").unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("allowed_chat_ids")
    );
    assert!(
        telegram.polls("999:OPEN").is_empty(),
        "without an allowlist the bot never polls"
    );
    // The model failing ("fail" makes the fake provider answer 429): the chat gets a plain apology.
    eventually("the failure is reported to the chat in plain words", || {
        !telegram.sent().is_empty()
    })
    .await;
    let sent = telegram.sent();
    assert_eq!(sent[0].0, 7);
    assert!(
        sent[0].1.starts_with("No pude completar la respuesta"),
        "{sent:?}"
    );
    assert!(
        !sent[0].1.contains("429")
            && !sent[0].1.contains("fixture")
            && !sent[0].1.contains("FAILS")
    );
    // A wrong token: the API says 401 and the status says so without the token.
    app.shutdown.cancel();
    let _ = supervisor.await;
    telegram.task.abort();
    llm.abort();
}

#[tokio::test]
async fn polling_resumes_from_the_saved_offset_and_a_rejected_token_is_reported_without_it() {
    let (llm_url, _seen, llm) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    app.db
        .col_put(
            "channelState",
            "resume",
            &json!({"id":"resume","telegram_offset":500}),
            PutOptions::default(),
        )
        .unwrap();
    let telegram = fake_telegram().await;
    add_telegram(
        &app,
        "resume",
        "u1",
        json!({"bot_token":"777:RESUME","allowed_chat_ids":[1],"api_base":telegram.base,"poll_timeout_s":1}),
    );
    let supervisor = tokio::spawn(crate::telegram::run(app.clone()));
    eventually("the first poll", || {
        !telegram.polls("777:RESUME").is_empty()
    })
    .await;
    assert_eq!(
        telegram.polls("777:RESUME")[0]["offset"],
        500,
        "a restart continues where it left off instead of replaying old messages"
    );
    // A server that rejects the token.
    let rejecting = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", rejecting.local_addr().unwrap());
    let reject = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = rejecting.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let _ = read_request(&mut socket).await;
                let _ = respond(
                    &mut socket,
                    401,
                    json!({"ok":false,"error_code":401,"description":"Unauthorized"}),
                )
                .await;
            });
        }
    });
    add_telegram(
        &app,
        "bad",
        "u1",
        json!({"bot_token":"000:REJECTED","allowed_chat_ids":[1],"api_base":base,"poll_timeout_s":1}),
    );
    eventually("the rejection is reported", || {
        app.channel_status
            .lock()
            .unwrap()
            .get("bad")
            .is_some_and(|s| s["status"] == "error")
    })
    .await;
    let status = app
        .channel_status
        .lock()
        .unwrap()
        .get("bad")
        .cloned()
        .unwrap();
    assert!(
        status["error"].as_str().unwrap().contains("rechazó"),
        "{status}"
    );
    assert!(
        !status.to_string().contains("REJECTED"),
        "the token never appears in the status"
    );
    app.shutdown.cancel();
    let _ = supervisor.await;
    reject.abort();
    telegram.task.abort();
    llm.abort();
}

// ---- Discord -----------------------------------------------------------------------

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, frame::coding::CloseCode};

/// A fake Discord: a Gateway WebSocket (Hello, Identify, Ready, heartbeats) and a REST API
/// that records every message the bot posts.
struct FakeDiscord {
    gateway: String,
    api: String,
    identifies: Arc<Mutex<Vec<Value>>>,
    posts: Arc<Mutex<Vec<(String, Value, String)>>>,
    connections: Arc<AtomicUsize>,
    heartbeats: Arc<AtomicUsize>,
    push: tokio::sync::broadcast::Sender<Value>,
    drop_first: Arc<AtomicBool>,
    close_code: Arc<Mutex<Option<u16>>>,
    mute_acks: Arc<AtomicBool>,
    rate_limit_first: Arc<AtomicBool>,
    /// Files served at `/cdn/<name>` on the REST address.
    store: FileStore,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
async fn fake_discord() -> FakeDiscord {
    let gateway = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut fake = FakeDiscord {
        gateway: format!("ws://{}/?v=10&encoding=json", gateway.local_addr().unwrap()),
        api: format!("http://{}", rest.local_addr().unwrap()),
        identifies: Arc::default(),
        posts: Arc::default(),
        connections: Arc::default(),
        heartbeats: Arc::default(),
        push: tokio::sync::broadcast::channel(64).0,
        drop_first: Arc::default(),
        close_code: Arc::default(),
        mute_acks: Arc::default(),
        rate_limit_first: Arc::default(),
        store: FileStore::default(),
        tasks: vec![],
    };
    let (identifies, connections, heartbeats, push) = (
        fake.identifies.clone(),
        fake.connections.clone(),
        fake.heartbeats.clone(),
        fake.push.clone(),
    );
    let (drop_first, close_code, mute_acks) = (
        fake.drop_first.clone(),
        fake.close_code.clone(),
        fake.mute_acks.clone(),
    );
    fake.tasks.push(tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = gateway.accept().await else { return };
            let number = connections.fetch_add(1, AtomicOrdering::SeqCst);
            let (identifies, heartbeats, mut events) = (identifies.clone(), heartbeats.clone(), push.subscribe());
            let (drop_first, close_code, mute_acks) = (drop_first.clone(), close_code.clone(), mute_acks.clone());
            tokio::spawn(async move {
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else { return };
                if number == 0 && drop_first.load(AtomicOrdering::SeqCst) {
                    return;
                }
                let _ = socket.send(Message::Text(json!({"op":10,"d":{"heartbeat_interval":100}}).to_string())).await;
                let mut sequence = 0;
                loop {
                    tokio::select! {
                        frame = socket.next() => {
                            let Some(Ok(Message::Text(text))) = frame else { return };
                            let value: Value = serde_json::from_str(&text).unwrap();
                            match value["op"].as_u64() {
                                Some(2) => {
                                    identifies.lock().unwrap().push(value["d"].clone());
                                    let closing = *close_code.lock().unwrap();
                                    if let Some(code) = closing {
                                        let _ = socket.send(Message::Close(Some(CloseFrame { code: CloseCode::from(code), reason: "fixture".into() }))).await;
                                        return;
                                    }
                                    sequence += 1;
                                    let _ = socket.send(Message::Text(json!({"op":0,"s":sequence,"t":"READY","d":{"user":{"id":"999"}}}).to_string())).await;
                                }
                                Some(1) => {
                                    heartbeats.fetch_add(1, AtomicOrdering::SeqCst);
                                    if !mute_acks.load(AtomicOrdering::SeqCst) {
                                        let _ = socket.send(Message::Text(json!({"op":11}).to_string())).await;
                                    }
                                }
                                _ => {}
                            }
                        }
                        Ok(event) = events.recv() => {
                            sequence += 1;
                            let _ = socket.send(Message::Text(json!({"op":0,"s":sequence,"t":"MESSAGE_CREATE","d":event}).to_string())).await;
                        }
                    }
                }
            });
        }
    }));
    let (posts, rate_limit_first, store) = (
        fake.posts.clone(),
        fake.rate_limit_first.clone(),
        fake.store.clone(),
    );
    fake.tasks.push(tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = rest.accept().await else {
                return;
            };
            let (posts, rate_limit_first, store) =
                (posts.clone(), rate_limit_first.clone(), store.clone());
            tokio::spawn(async move {
                let Ok(request) = read_request(&mut socket).await else {
                    return;
                };
                let auth = request
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                if store
                    .serve(&mut socket, "/cdn/", request.uri().path(), &auth)
                    .await
                {
                    return;
                }
                let body: Value = serde_json::from_slice(request.body()).unwrap_or(Value::Null);
                if rate_limit_first.swap(false, AtomicOrdering::SeqCst) {
                    let _ = respond(
                        &mut socket,
                        429,
                        json!({"message":"rate limited","retry_after":0.1}),
                    )
                    .await;
                    return;
                }
                posts
                    .lock()
                    .unwrap()
                    .push((request.uri().path().to_owned(), body, auth));
                let _ = respond(&mut socket, 200, json!({"id":"1"})).await;
            });
        }
    }));
    fake
}
impl FakeDiscord {
    /// A message with files attached: `files` are `(name, content type, declared size)`; each is
    /// served at `/cdn/<name>` on the REST address.
    fn message_with(&self, channel: &str, content: &str, files: &[(&str, &str, u64)]) {
        let attachments: Vec<Value> = files.iter().map(|(name, content_type, size)| json!({"id":"a","filename":name,"content_type":content_type,"size":size,"url":format!("{}/cdn/{name}", self.api)})).collect();
        let _ = self.push.send(json!({"id":"m","channel_id":channel,"author":{"id":"5"},"content":content,"type":0,"attachments":attachments}));
    }
    fn message(&self, channel: &str, author: &str, content: &str) {
        let _ = self.push.send(json!({"id":"m","channel_id":channel,"author":{"id":author},"content":content,"type":0}));
    }
    fn config(&self, allowed: &[&str]) -> Value {
        json!({"bot_token":"DISCORDTOKEN","allowed_channel_ids":allowed,"gateway_url":self.gateway,"api_base":self.api})
    }
}
impl Drop for FakeDiscord {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
fn add_channel(app: &App, kind: &str, id: &str, config: Value) {
    app.db
        .col_put(
            "channels",
            id,
            &json!({"id":id,"type":kind,"user_id":"u1","enabled":true,"active":true,"config":"{}"}),
            PutOptions::default(),
        )
        .unwrap();
    config::store_secret(app, &format!("channel:{id}:config"), &config.to_string()).unwrap();
    app.channel_wakeup.notify_one();
}
fn status_of(app: &App, id: &str) -> Value {
    app.channel_status
        .lock()
        .unwrap()
        .get(id)
        .cloned()
        .unwrap_or(Value::Null)
}

#[tokio::test]
async fn discord_answers_only_allowed_channels_without_mentions_and_heartbeats() {
    let (llm_url, llm_seen, llm) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let discord = fake_discord().await;
    add_channel(&app, "discord", "dc1", discord.config(&["111"]));
    let supervisor = tokio::spawn(crate::discord::run(app.clone()));
    eventually("the bot is connected", || {
        status_of(&app, "dc1")["status"] == "connected"
    })
    .await;
    let identify = discord.identifies.lock().unwrap()[0].clone();
    assert_eq!(identify["token"], "DISCORDTOKEN");
    assert_eq!(identify["intents"], (1u64 << 9) | (1 << 12) | (1 << 15));
    eventually("heartbeats flow", || {
        discord.heartbeats.load(AtomicOrdering::SeqCst) >= 2
    })
    .await;
    discord.message("111", "5", "hola desde Discord");
    discord.message("222", "5", "canal no permitido");
    let _ = discord.push.send(
        json!({"channel_id":"111","author":{"id":"6","bot":true},"content":"otro bot","type":0}),
    );
    let _ = discord
        .push
        .send(json!({"channel_id":"111","author":{"id":"999"},"content":"yo mismo","type":0}));
    let _ = discord.push.send(
        json!({"channel_id":"111","author":{"id":"5"},"content":"un aviso del sistema","type":7}),
    );
    let _ = discord
        .push
        .send(json!({"channel_id":"111","author":{"id":"5"},"content":"","type":0}));
    eventually("the allowed channel is answered", || {
        !discord.posts.lock().unwrap().is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let posts = discord.posts.lock().unwrap().clone();
    assert_eq!(posts.len(), 1, "{posts:?}");
    let (path, body, auth) = &posts[0];
    assert_eq!(
        (path.as_str(), body["content"].clone(), auth.as_str()),
        (
            "/channels/111/messages",
            json!("Hola Rust"),
            "Bot DISCORDTOKEN"
        )
    );
    assert_eq!(
        body["allowed_mentions"],
        json!({"parse":[]}),
        "nothing the model writes can ping anyone"
    );
    assert_eq!(
        llm_seen.lock().unwrap().len(),
        1,
        "strangers, bots, itself, system messages and empty content never reach the model"
    );
    let thread = app
        .db
        .col_get("conversationThreads", "u1/discord/111")
        .unwrap()
        .unwrap()
        .doc;
    assert_eq!(
        (thread["channel"].clone(), thread["user_id"].clone()),
        (json!("discord"), json!("u1"))
    );
    assert!(
        app.db
            .col_get("conversationThreads", "u1/discord/222")
            .unwrap()
            .is_none()
    );
    assert!(!status_of(&app, "dc1").to_string().contains("DISCORDTOKEN"));
    // The web history never lists channel threads.
    assert_eq!(
        call(
            &app,
            Some(&app.token),
            "GET",
            "/api/chat/history?threadId=u1/discord/111",
            json!({})
        )
        .0,
        404
    );
    app.shutdown.cancel();
    let _ = supervisor.await;
    llm.abort();
}

#[tokio::test]
async fn discord_reconnects_after_a_lost_connection_or_a_missed_heartbeat_but_never_after_a_fatal_close()
 {
    let (llm_url, _seen, llm) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    // 1. The first connection is dropped: the bot connects again and identifies again.
    let discord = fake_discord().await;
    discord.drop_first.store(true, AtomicOrdering::SeqCst);
    add_channel(&app, "discord", "dc1", discord.config(&["1"]));
    let supervisor = tokio::spawn(crate::discord::run(app.clone()));
    eventually("it reconnects and identifies", || {
        discord.identifies.lock().unwrap().len() == 1
            && status_of(&app, "dc1")["status"] == "connected"
    })
    .await;
    assert!(discord.connections.load(AtomicOrdering::SeqCst) >= 2);
    // 2. The server stops acknowledging heartbeats: a zombie connection is replaced.
    let before = discord.connections.load(AtomicOrdering::SeqCst);
    discord.mute_acks.store(true, AtomicOrdering::SeqCst);
    eventually("a connection without heartbeat acks is replaced", || {
        discord.connections.load(AtomicOrdering::SeqCst) > before
    })
    .await;
    // 3. A reply that hits a rate limit is retried once, so it arrives exactly once.
    discord.mute_acks.store(false, AtomicOrdering::SeqCst);
    eventually("connected again", || {
        status_of(&app, "dc1")["status"] == "connected"
    })
    .await;
    discord.rate_limit_first.store(true, AtomicOrdering::SeqCst);
    discord.message("1", "5", "hola");
    eventually("the reply is delivered after the rate limit", || {
        discord.posts.lock().unwrap().len() == 1
    })
    .await;
    app.shutdown.cancel();
    let _ = supervisor.await;
    drop(discord);
    // 4. Fatal closes (bad token, intents refused) are reported and never retried.
    for (code, expected) in [(4004u16, "rechazó el bot_token"), (4014, "Message Content")] {
        let app = Arc::new(
            App::open(
                tempfile::tempdir()
                    .map(|d| {
                        seed(d.path(), &llm_url);
                        d.keep()
                    })
                    .unwrap(),
            )
            .unwrap(),
        );
        let fatal = fake_discord().await;
        *fatal.close_code.lock().unwrap() = Some(code);
        add_channel(&app, "discord", "dc", fatal.config(&["1"]));
        let supervisor = tokio::spawn(crate::discord::run(app.clone()));
        eventually("the fatal close is reported", || {
            status_of(&app, "dc")["status"] == "error"
        })
        .await;
        assert!(
            status_of(&app, "dc")["error"]
                .as_str()
                .unwrap()
                .contains(expected),
            "{code}: {}",
            status_of(&app, "dc")
        );
        assert!(!status_of(&app, "dc").to_string().contains("DISCORDTOKEN"));
        tokio::time::sleep(Duration::from_millis(3500)).await;
        assert_eq!(
            fatal.connections.load(AtomicOrdering::SeqCst),
            1,
            "{code}: a fatal close is never retried"
        );
        app.shutdown.cancel();
        let _ = supervisor.await;
    }
    llm.abort();
}

#[tokio::test]
async fn a_discord_channel_without_an_allowlist_never_connects() {
    let (llm_url, _seen, llm) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let discord = fake_discord().await;
    add_channel(
        &app,
        "discord",
        "open",
        json!({"bot_token":"DISCORDTOKEN","gateway_url":discord.gateway,"api_base":discord.api}),
    );
    let supervisor = tokio::spawn(crate::discord::run(app.clone()));
    eventually("the problem is reported", || {
        status_of(&app, "open")["status"] == "error"
    })
    .await;
    assert!(
        status_of(&app, "open")["error"]
            .as_str()
            .unwrap()
            .contains("allowed_channel_ids")
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        discord.connections.load(AtomicOrdering::SeqCst),
        0,
        "without an allowlist the bot never connects"
    );
    app.shutdown.cancel();
    let _ = supervisor.await;
    llm.abort();
}

// ---- Slack -------------------------------------------------------------------------

type SlackFiles = std::collections::HashMap<String, (String, Vec<u8>)>;
/// A fake Slack: the Web API (`apps.connections.open`, `chat.postMessage`) and the Socket
/// Mode WebSocket, which records every envelope the bot acknowledges.
struct FakeSlack {
    api: String,
    posts: Arc<Mutex<Vec<(Value, String)>>>,
    opens: Arc<Mutex<Vec<String>>>,
    connections: Arc<AtomicUsize>,
    acks: Arc<Mutex<Vec<String>>>,
    /// What was posted to a `response_url` (`/response/<name>`): `(name, body)`.
    responses: Arc<Mutex<Vec<(String, Value)>>>,
    /// Files the fake serves at `/files/<name>`: `(content type, bytes)`; and who asked for which.
    files: Arc<Mutex<SlackFiles>>,
    file_auth: Arc<Mutex<Vec<String>>>,
    push: tokio::sync::broadcast::Sender<Value>,
    open_error: Arc<Mutex<Option<String>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
async fn fake_slack() -> FakeSlack {
    let socket_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let socket_url = format!(
        "ws://{}/link/?ticket=t",
        socket_listener.local_addr().unwrap()
    );
    let mut fake = FakeSlack {
        api: format!("http://{}", rest.local_addr().unwrap()),
        posts: Arc::default(),
        opens: Arc::default(),
        connections: Arc::default(),
        acks: Arc::default(),
        responses: Arc::default(),
        files: Arc::default(),
        file_auth: Arc::default(),
        push: tokio::sync::broadcast::channel(64).0,
        open_error: Arc::default(),
        tasks: vec![],
    };
    let (connections, acks, push) = (
        fake.connections.clone(),
        fake.acks.clone(),
        fake.push.clone(),
    );
    fake.tasks.push(tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = socket_listener.accept().await else { return };
            connections.fetch_add(1, AtomicOrdering::SeqCst);
            let (acks, mut events) = (acks.clone(), push.subscribe());
            tokio::spawn(async move {
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else { return };
                let _ = socket.send(Message::Text(json!({"type":"hello","num_connections":1}).to_string())).await;
                loop {
                    tokio::select! {
                        frame = socket.next() => {
                            let Some(Ok(Message::Text(text))) = frame else { return };
                            if let Some(envelope) = serde_json::from_str::<Value>(&text).unwrap()["envelope_id"].as_str() {
                                acks.lock().unwrap().push(envelope.to_owned());
                            }
                        }
                        Ok(envelope) = events.recv() => {
                            let _ = socket.send(Message::Text(envelope.to_string())).await;
                        }
                    }
                }
            });
        }
    }));
    let (posts, opens, open_error, responses, files, file_auth) = (
        fake.posts.clone(),
        fake.opens.clone(),
        fake.open_error.clone(),
        fake.responses.clone(),
        fake.files.clone(),
        fake.file_auth.clone(),
    );
    fake.tasks.push(tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = rest.accept().await else {
                return;
            };
            let (posts, opens, open_error, socket_url, responses, files, file_auth) = (
                posts.clone(),
                opens.clone(),
                open_error.clone(),
                socket_url.clone(),
                responses.clone(),
                files.clone(),
                file_auth.clone(),
            );
            tokio::spawn(async move {
                let Ok(request) = read_request(&mut socket).await else {
                    return;
                };
                let auth = request
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                if let Some(name) = request.uri().path().strip_prefix("/files/") {
                    file_auth.lock().unwrap().push(format!("{name}:{auth}"));
                    let found = files.lock().unwrap().get(name).cloned();
                    let (status, content_type, bytes) = match found {
                        Some((content_type, bytes)) => ("200 OK", content_type, bytes),
                        None => ("404 Not Found", "text/plain".to_owned(), vec![]),
                    };
                    let head = format!("HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len());
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&bytes).await;
                    return;
                }
                let body: Value = serde_json::from_slice(request.body()).unwrap_or(Value::Null);
                let reply = match request.uri().path() {
                    "/apps.connections.open" => {
                        opens.lock().unwrap().push(auth);
                        let failure = open_error.lock().unwrap().clone();
                        match failure {
                            Some(code) => json!({"ok":false,"error":code}),
                            None => json!({"ok":true,"url":socket_url}),
                        }
                    }
                    "/chat.postMessage" => {
                        posts.lock().unwrap().push((body, auth));
                        json!({"ok":true})
                    }
                    path if path.starts_with("/response/") => {
                        responses
                            .lock()
                            .unwrap()
                            .push((path["/response/".len()..].to_owned(), body));
                        json!({"ok":true})
                    }
                    _ => json!({"ok":false,"error":"unknown_method"}),
                };
                let _ = respond(&mut socket, 200, reply).await;
            });
        }
    }));
    fake
}
impl FakeSlack {
    fn event(&self, envelope: &str, event_id: &str, event: Value) {
        let _ = self.push.send(json!({"type":"events_api","envelope_id":envelope,"payload":{"event_id":event_id,"event":event}}));
    }
    fn message(&self, envelope: &str, event_id: &str, channel: &str, text: &str) {
        self.event(
            envelope,
            event_id,
            json!({"type":"message","channel":channel,"user":"U1","text":text,"ts":"1"}),
        );
    }
    fn envelope(&self, kind: &str, envelope: &str, payload: Value) {
        let _ = self
            .push
            .send(json!({"type":kind,"envelope_id":envelope,"payload":payload}));
    }
    fn posted_texts(&self) -> Vec<String> {
        self.posts
            .lock()
            .unwrap()
            .iter()
            .map(|(body, _)| body["text"].as_str().unwrap_or_default().to_owned())
            .collect()
    }
    fn config(&self, allowed: &[&str]) -> Value {
        json!({"bot_token":"xoxb-SECRETBOT","app_token":"xapp-SECRETAPP","allowed_channel_ids":allowed,"api_base":self.api})
    }
}
impl Drop for FakeSlack {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

#[tokio::test]
async fn slack_answers_in_the_thread_it_was_written_in_and_to_slash_commands_through_their_response_url()
 {
    let (llm_url, llm_seen, llm) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let slack = fake_slack().await;
    add_channel(&app, "slack", "sl1", slack.config(&["D0123"]));
    let supervisor = tokio::spawn(crate::slack::run(app.clone()));
    eventually("connected", || {
        slack.connections.load(AtomicOrdering::SeqCst) == 1
    })
    .await;
    // A message in a thread is answered in that thread; one outside any thread is not.
    slack.event("e1", "Ev1", json!({"type":"message","channel":"D0123","user":"U1","text":"en hilo","ts":"1700000001.000200","thread_ts":"1700000000.000100"}));
    eventually("the threaded reply", || {
        slack.posts.lock().unwrap().len() == 1
    })
    .await;
    assert_eq!(
        slack.posts.lock().unwrap()[0].0["thread_ts"],
        "1700000000.000100"
    );
    slack.message("e2", "Ev2", "D0123", "sin hilo");
    eventually("the plain reply", || slack.posts.lock().unwrap().len() == 2).await;
    assert!(slack.posts.lock().unwrap()[1].0.get("thread_ts").is_none());
    // A thread id that is not a Slack timestamp is never forwarded.
    slack.event("e3", "Ev3", json!({"type":"message","channel":"D0123","user":"U1","text":"raro","ts":"1","thread_ts":"../../x"}));
    eventually("the odd one is answered", || {
        slack.posts.lock().unwrap().len() == 3
    })
    .await;
    assert!(slack.posts.lock().unwrap()[2].0.get("thread_ts").is_none());
    // A slash command in an allowed channel is acknowledged and answered through its response_url.
    let url = |name: &str| format!("{}/response/{name}", slack.api);
    slack.envelope("slash_commands", "s1", json!({"command":"/hive","text":"hola desde el comando","channel_id":"D0123","user_id":"U1","response_url":url("r1")}));
    eventually("the command is answered", || {
        slack
            .responses
            .lock()
            .unwrap()
            .iter()
            .any(|(name, _)| name == "r1")
    })
    .await;
    let answered = slack
        .responses
        .lock()
        .unwrap()
        .iter()
        .find(|(name, _)| name == "r1")
        .unwrap()
        .1
        .clone();
    assert_eq!(
        (answered["response_type"].clone(), answered["text"].clone()),
        (json!("in_channel"), json!("Hola Rust"))
    );
    assert!(
        llm_seen
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .to_string()
            .contains("hola desde el comando")
    );
    // A redelivered command envelope, an empty command (a hint, privately), a stranger channel and a
    // response_url that is not Slack's never reach the model or the network.
    let before = llm_seen.lock().unwrap().len();
    slack.envelope(
        "slash_commands",
        "s1",
        json!({"command":"/hive","text":"otra vez","channel_id":"D0123","response_url":url("dup")}),
    );
    slack.envelope(
        "slash_commands",
        "s2",
        json!({"command":"/hive","text":"  ","channel_id":"D0123","response_url":url("empty")}),
    );
    slack.envelope("slash_commands", "s3", json!({"command":"/hive","text":"intruso","channel_id":"C9999","response_url":url("stranger")}));
    slack.envelope("slash_commands", "s4", json!({"command":"/hive","text":"malo","channel_id":"D0123","response_url":"https://evil.example/steal"}));
    slack.envelope(
        "slash_commands",
        "s5",
        json!({"command":"/hive","text":"sin url","channel_id":"D0123"}),
    );
    eventually("the empty command gets its hint", || {
        slack
            .responses
            .lock()
            .unwrap()
            .iter()
            .any(|(name, _)| name == "empty")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let responses = slack.responses.lock().unwrap().clone();
    let hint = responses.iter().find(|(name, _)| name == "empty").unwrap();
    assert_eq!(hint.1["response_type"], "ephemeral");
    assert!(
        hint.1["text"].as_str().unwrap().contains("/hive"),
        "{hint:?}"
    );
    assert!(
        responses
            .iter()
            .all(|(name, _)| !["dup", "stranger"].contains(&name.as_str())),
        "{responses:?}"
    );
    assert_eq!(
        llm_seen.lock().unwrap().len(),
        before,
        "none of them ran a turn"
    );
    let acked = slack.acks.lock().unwrap().clone();
    for envelope in ["s1", "s2", "s3", "s4", "s5"] {
        assert!(
            acked.contains(&envelope.to_owned()),
            "{envelope} is acknowledged even when ignored: {acked:?}"
        );
    }
    app.shutdown.cancel();
    let _ = supervisor.await;
    llm.abort();
}

#[tokio::test]
async fn slack_asks_for_an_approval_with_buttons_and_only_the_current_question_can_be_answered_by_them()
 {
    for (action, ran) in [("hive_approve", true), ("hive_deny", false)] {
        let (llm_url, llm) = tool_provider().await;
        let home = tempfile::tempdir().unwrap();
        seed(home.path(), &llm_url);
        let app = Arc::new(App::open(home.path().to_owned()).unwrap());
        put_policy(
            &app,
            json!({"rules":[{"tools":["a2ui_create_surface"],"decision":"ask","reason":"crea una interfaz"}]}),
            30_000,
        );
        let slack = fake_slack().await;
        add_channel(&app, "slack", "sl1", slack.config(&["D0123"]));
        let supervisor = tokio::spawn(crate::slack::run(app.clone()));
        eventually("connected", || {
            slack.connections.load(AtomicOrdering::SeqCst) == 1
        })
        .await;
        slack.message("e1", "Ev1", "D0123", "crea el formulario");
        eventually("the question with buttons", || {
            slack
                .posts
                .lock()
                .unwrap()
                .iter()
                .any(|(body, _)| body.get("blocks").is_some())
        })
        .await;
        let question = slack
            .posts
            .lock()
            .unwrap()
            .iter()
            .find(|(body, _)| body.get("blocks").is_some())
            .unwrap()
            .0
            .clone();
        assert!(
            question["text"]
                .as_str()
                .unwrap()
                .contains("a2ui_create_surface"),
            "{question}"
        );
        let buttons = question["blocks"][1]["elements"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(
            buttons
                .iter()
                .map(|b| b["action_id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["hive_approve", "hive_deny"]
        );
        assert_eq!(
            question["blocks"][0]["text"]["type"], "plain_text",
            "never markup"
        );
        let id = buttons[0]["value"].as_str().unwrap().to_owned();
        assert_eq!(buttons[1]["value"], json!(id));
        let press = |envelope: &str, channel: &str, value: &str, action: &str, name: &str| {
            slack.envelope("interactive", envelope, json!({"type":"block_actions","channel":{"id":channel},"user":{"id":"U1"},
                "actions":[{"action_id":action,"value":value}],"response_url":format!("{}/response/{name}", slack.api)}));
        };
        // A button of an old question, one pressed from another channel and an unknown action decide nothing.
        press("i1", "D0123", "not-this-one", action, "old");
        press("i2", "C9999", &id, action, "stranger");
        press("i3", "D0123", &id, "something_else", "odd");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            !surface_exists(&app) && app.approvals.lock().unwrap().len() == 1,
            "still waiting"
        );
        assert!(slack.responses.lock().unwrap().is_empty());
        // The right press decides, the message loses its buttons, and the turn goes on.
        press("i4", "D0123", &id, action, "done");
        eventually("the turn finishes", || {
            slack.posted_texts().iter().any(|t| t == "Listo")
        })
        .await;
        assert_eq!(surface_exists(&app), ran, "{action}");
        let replaced = slack
            .responses
            .lock()
            .unwrap()
            .iter()
            .find(|(name, _)| name == "done")
            .unwrap()
            .1
            .clone();
        assert_eq!(replaced["replace_original"], true);
        assert_eq!(
            replaced["text"],
            if ran {
                "Aprobado. Continúo."
            } else {
                "Denegado."
            }
        );
        // Pressing it again changes nothing.
        press("i5", "D0123", &id, action, "again");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            slack
                .responses
                .lock()
                .unwrap()
                .iter()
                .all(|(name, _)| name != "again")
        );
        assert!(
            app.approvals.lock().unwrap().is_empty()
                && app.chat_approvals.lock().unwrap().is_empty()
        );
        for envelope in ["i1", "i2", "i3", "i4", "i5"] {
            assert!(
                slack.acks.lock().unwrap().contains(&envelope.to_owned()),
                "{envelope}"
            );
        }
        app.shutdown.cancel();
        let _ = supervisor.await;
        llm.abort();
    }
}

#[tokio::test]
async fn slack_acknowledges_every_envelope_and_answers_only_people_in_allowed_channels_once() {
    let (llm_url, llm_seen, llm) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let slack = fake_slack().await;
    add_channel(&app, "slack", "sl1", slack.config(&["D0123"]));
    let supervisor = tokio::spawn(crate::slack::run(app.clone()));
    eventually("connected", || {
        status_of(&app, "sl1")["status"] == "connected"
    })
    .await;
    assert_eq!(
        slack.opens.lock().unwrap()[0],
        "Bearer xapp-SECRETAPP",
        "the connection is opened with the app token"
    );
    slack.message("e1", "Ev1", "D0123", "hola desde Slack");
    slack.message("e2", "Ev1", "D0123", "hola desde Slack");
    slack.message("e3", "Ev3", "C9999", "canal no permitido");
    slack.event(
        "e4",
        "Ev4",
        json!({"type":"message","channel":"D0123","bot_id":"B1","text":"soy un bot"}),
    );
    slack.event("e5", "Ev5", json!({"type":"message","channel":"D0123","user":"U1","subtype":"message_changed","text":"editado"}));
    slack.event(
        "e6",
        "Ev6",
        json!({"type":"reaction_added","channel":"D0123","user":"U1"}),
    );
    slack.message("e7", "Ev7", "D0123", "   ");
    eventually("the allowed DM is answered", || {
        !slack.posts.lock().unwrap().is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let posts = slack.posts.lock().unwrap().clone();
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert_eq!(
        (
            posts[0].0["channel"].clone(),
            posts[0].0["text"].clone(),
            posts[0].1.as_str()
        ),
        (json!("D0123"), json!("Hola Rust"), "Bearer xoxb-SECRETBOT")
    );
    assert_eq!(
        llm_seen.lock().unwrap().len(),
        1,
        "the redelivered envelope, a stranger channel, bots, edits, other events and blank text never reach the model"
    );
    let acked = slack.acks.lock().unwrap().clone();
    for envelope in ["e1", "e2", "e3", "e4", "e5", "e6", "e7"] {
        assert!(
            acked.contains(&envelope.to_owned()),
            "{envelope} must be acknowledged even when it is ignored: {acked:?}"
        );
    }
    let thread = app
        .db
        .col_get("conversationThreads", "u1/slack/D0123")
        .unwrap()
        .unwrap()
        .doc;
    assert_eq!(
        (thread["channel"].clone(), thread["user_id"].clone()),
        (json!("slack"), json!("u1"))
    );
    assert!(
        app.db
            .col_get("conversationThreads", "u1/slack/C9999")
            .unwrap()
            .is_none()
    );
    let status = status_of(&app, "sl1").to_string();
    assert!(!status.contains("SECRET"), "{status}");
    app.shutdown.cancel();
    let _ = supervisor.await;
    llm.abort();
}

#[tokio::test]
async fn slack_reconnects_on_request_and_never_retries_a_refused_token() {
    let (llm_url, _seen, llm) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let slack = fake_slack().await;
    add_channel(&app, "slack", "sl1", slack.config(&["D1"]));
    let supervisor = tokio::spawn(crate::slack::run(app.clone()));
    eventually("connected", || {
        status_of(&app, "sl1")["status"] == "connected"
    })
    .await;
    // Slack asks the app to reconnect (it does this regularly): a new connection is opened.
    let _ = slack
        .push
        .send(json!({"type":"disconnect","reason":"refresh_requested"}));
    eventually("a new connection after the disconnect request", || {
        slack.connections.load(AtomicOrdering::SeqCst) >= 2
    })
    .await;
    eventually("connected again", || {
        status_of(&app, "sl1")["status"] == "connected"
    })
    .await;
    app.shutdown.cancel();
    let _ = supervisor.await;
    drop(slack);
    // A refused token is reported (without the token) and not retried.
    for code in ["invalid_auth", "token_revoked"] {
        let app = Arc::new(
            App::open(
                tempfile::tempdir()
                    .map(|d| {
                        seed(d.path(), &llm_url);
                        d.keep()
                    })
                    .unwrap(),
            )
            .unwrap(),
        );
        let refusing = fake_slack().await;
        *refusing.open_error.lock().unwrap() = Some(code.to_owned());
        add_channel(&app, "slack", "sl", refusing.config(&["D1"]));
        let supervisor = tokio::spawn(crate::slack::run(app.clone()));
        eventually("the refusal is reported", || {
            status_of(&app, "sl")["status"] == "error"
        })
        .await;
        assert!(
            status_of(&app, "sl")["error"]
                .as_str()
                .unwrap()
                .contains(code)
        );
        assert!(!status_of(&app, "sl").to_string().contains("SECRET"));
        tokio::time::sleep(Duration::from_millis(3500)).await;
        assert_eq!(
            refusing.opens.lock().unwrap().len(),
            1,
            "{code}: never retried"
        );
        assert_eq!(refusing.connections.load(AtomicOrdering::SeqCst), 0);
        app.shutdown.cancel();
        let _ = supervisor.await;
    }
    // A transient error is retried with a pause.
    let app = Arc::new(
        App::open(
            tempfile::tempdir()
                .map(|d| {
                    seed(d.path(), &llm_url);
                    d.keep()
                })
                .unwrap(),
        )
        .unwrap(),
    );
    let flaky = fake_slack().await;
    *flaky.open_error.lock().unwrap() = Some("internal_error".into());
    add_channel(&app, "slack", "sl", flaky.config(&["D1"]));
    let supervisor = tokio::spawn(crate::slack::run(app.clone()));
    eventually("a transient failure is retried", || {
        flaky.opens.lock().unwrap().len() >= 2
    })
    .await;
    *flaky.open_error.lock().unwrap() = None;
    eventually("and recovers", || {
        status_of(&app, "sl")["status"] == "connected"
    })
    .await;
    app.shutdown.cancel();
    let _ = supervisor.await;
    llm.abort();
}

#[tokio::test]
async fn a_slack_channel_without_an_allowlist_never_connects() {
    let (llm_url, _seen, llm) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let slack = fake_slack().await;
    add_channel(
        &app,
        "slack",
        "open",
        json!({"bot_token":"xoxb-SECRETBOT","app_token":"xapp-SECRETAPP","api_base":slack.api}),
    );
    let supervisor = tokio::spawn(crate::slack::run(app.clone()));
    eventually("the problem is reported", || {
        status_of(&app, "open")["status"] == "error"
    })
    .await;
    assert!(
        status_of(&app, "open")["error"]
            .as_str()
            .unwrap()
            .contains("allowed_channel_ids")
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        slack.opens.lock().unwrap().is_empty()
            && slack.connections.load(AtomicOrdering::SeqCst) == 0
    );
    app.shutdown.cancel();
    let _ = supervisor.await;
    llm.abort();
}

// ---- WhatsApp Cloud ----------------------------------------------------------------

/// A fake Graph API that records the messages the channel sends.
async fn fake_graph() -> (
    String,
    Arc<Mutex<Vec<(String, Value, String)>>>,
    tokio::task::JoinHandle<()>,
) {
    let (base, posts, task, _) = fake_graph_with_media().await;
    (base, posts, task)
}
/// The Graph API fake plus a media store: `GET /<media id>` answers `{url, mime_type, file_size}` for
/// a stored file, and the bytes are served at `/media/<name>` on the same address.
async fn fake_graph_with_media() -> (
    String,
    Arc<Mutex<Vec<(String, Value, String)>>>,
    tokio::task::JoinHandle<()>,
    FileStore,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let posts: Arc<Mutex<Vec<(String, Value, String)>>> = Arc::default();
    let seen = posts.clone();
    let store = FileStore::default();
    let media = store.clone();
    let origin = base.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (seen, media, origin) = (seen.clone(), media.clone(), origin.clone());
            tokio::spawn(async move {
                let Ok(request) = read_request(&mut socket).await else {
                    return;
                };
                let auth = request
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                if media
                    .serve(&mut socket, "/media/", request.uri().path(), &auth)
                    .await
                {
                    return;
                }
                seen.lock().unwrap().push((
                    request.uri().path().to_owned(),
                    serde_json::from_slice(request.body()).unwrap_or(Value::Null),
                    auth,
                ));
                // `GET /<media id>`: where the file is, for ids the test stored as `media-<id>`.
                let id = request.uri().path().trim_start_matches('/');
                let stored = media
                    .files
                    .lock()
                    .unwrap()
                    .get(&format!("info-{id}"))
                    .cloned();
                let reply = match (request.method().as_str(), stored) {
                    ("GET", Some((mime, bytes))) => {
                        json!({"id":id,"url":format!("{origin}/media/{id}"),"mime_type":mime,"file_size":bytes.len()})
                    }
                    _ => json!({"messages":[{"id":"wamid.out"}]}),
                };
                let _ = respond(&mut socket, 200, reply).await;
            });
        }
    });
    (base, posts, task, store)
}
fn webhook_body(phone_id: &str, messages: Value) -> Vec<u8> {
    json!({"object":"whatsapp_business_account","entry":[{"id":"1","changes":[{"field":"messages","value":{"messaging_product":"whatsapp","metadata":{"phone_number_id":phone_id,"display_phone_number":"1"},"messages":messages}}]}]}).to_string().into_bytes()
}
fn text_message(id: &str, from: &str, text: &str) -> Value {
    json!({"id":id,"from":from,"timestamp":"1","type":"text","text":{"body":text}})
}
fn sign(secret: &str, body: &[u8]) -> String {
    format!(
        "sha256={}",
        hex::encode(
            ring::hmac::sign(
                &ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes()),
                body
            )
            .as_ref()
        )
    )
}
async fn post_webhook(
    base: &str,
    channel: &str,
    body: Vec<u8>,
    signature: Option<String>,
) -> reqwest::Response {
    let mut request = reqwest::Client::new()
        .post(format!("{base}/webhook/whatsapp/{channel}"))
        .body(body);
    if let Some(signature) = signature {
        request = request.header("X-Hub-Signature-256", signature);
    }
    request.send().await.unwrap()
}

#[tokio::test]
async fn whatsapp_verifies_meta_answers_only_signed_text_from_allowed_numbers_and_never_twice() {
    let (llm_url, llm_seen, llm) = provider().await;
    let (graph, sent, graph_task) = fake_graph().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    // Credentials on, so every ordinary route needs the bearer token.
    app.db.col_put("users", "u1", &json!({"id":"u1","name":"Uno","email":"uno@example.com","password_hash":"$2b$04$abcdefghijklmnopqrstuuxDugxrpcpW3csMl3JkYMLtX7yxJF.2."}), PutOptions::default()).unwrap();
    add_channel(
        &app,
        "whatsapp",
        "wa1",
        json!({"access_token":"EAABSECRET","phone_number_id":"1055","app_secret":"shh","verify_token":"vt","allowed_numbers":["+57 300 111 2222"],"api_base":graph}),
    );
    let (base, server) = serve(app.clone()).await;
    let client = reqwest::Client::new();
    // Meta's verification handshake: the challenge comes back as plain text, exactly.
    let verified = client.get(format!("{base}/webhook/whatsapp/wa1?hub.mode=subscribe&hub.verify_token=vt&hub.challenge=1158201444")).send().await.unwrap();
    assert_eq!(verified.status(), 200);
    assert!(
        verified.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );
    assert_eq!(verified.text().await.unwrap(), "1158201444");
    for query in [
        "hub.mode=subscribe&hub.verify_token=wrong&hub.challenge=1",
        "hub.mode=unsubscribe&hub.verify_token=vt&hub.challenge=1",
        "hub.mode=subscribe&hub.verify_token=vt",
    ] {
        assert_eq!(
            client
                .get(format!("{base}/webhook/whatsapp/wa1?{query}"))
                .send()
                .await
                .unwrap()
                .status(),
            403,
            "{query}"
        );
    }
    // The webhook is public but nothing else is: ordinary routes still need the bearer token.
    assert_eq!(
        client
            .get(format!("{base}/api/users"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    // A signed message from an allowed number is answered by the agent, to that number.
    let body = webhook_body(
        "1055",
        json!([text_message(
            "wamid.1",
            "573001112222",
            "hola desde WhatsApp"
        )]),
    );
    let signature = sign("shh", &body);
    assert_eq!(
        post_webhook(&base, "wa1", body.clone(), Some(signature.clone()))
            .await
            .status(),
        200
    );
    eventually("the allowed number gets the agent's answer", || {
        !sent.lock().unwrap().is_empty()
    })
    .await;
    let first = sent.lock().unwrap()[0].clone();
    assert_eq!(first.0, "/1055/messages");
    assert_eq!(
        (
            first.1["to"].clone(),
            first.1["text"]["body"].clone(),
            first.1["messaging_product"].clone(),
            first.2.as_str()
        ),
        (
            json!("573001112222"),
            json!("Hola Rust"),
            json!("whatsapp"),
            "Bearer EAABSECRET"
        )
    );
    assert!(
        llm_seen.lock().unwrap()[0]["messages"]
            .to_string()
            .contains("hola desde WhatsApp")
    );
    let thread = app
        .db
        .col_get("conversationThreads", "u1/whatsapp/573001112222")
        .unwrap()
        .unwrap()
        .doc;
    assert_eq!(
        (thread["channel"].clone(), thread["user_id"].clone()),
        (json!("whatsapp"), json!("u1"))
    );
    // Meta redelivers a webhook it thinks failed: the same message id runs only once, even after a restart.
    assert_eq!(
        post_webhook(&base, "wa1", body.clone(), Some(signature.clone()))
            .await
            .status(),
        200
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        (sent.lock().unwrap().len(), llm_seen.lock().unwrap().len()),
        (1, 1),
        "a redelivery is dropped"
    );
    assert!(
        app.db
            .col_get("channelState", "whatsapp:wa1")
            .unwrap()
            .unwrap()
            .doc["seen"]
            .to_string()
            .contains("wamid.1"),
        "seen ids are persisted"
    );
    // Everything that must not reach the model: bad or missing signatures (never even parsed), strangers,
    // another phone number's traffic, non-text messages, blank text and delivery receipts.
    let attack = webhook_body(
        "1055",
        json!([text_message("wamid.x", "573001112222", "firma falsa")]),
    );
    assert_eq!(
        post_webhook(
            &base,
            "wa1",
            attack.clone(),
            Some(sign("not-the-secret", &attack))
        )
        .await
        .status(),
        401
    );
    assert_eq!(
        post_webhook(&base, "wa1", attack.clone(), None)
            .await
            .status(),
        401
    );
    assert_eq!(
        post_webhook(&base, "wa1", attack.clone(), Some("sha256=zz".into()))
            .await
            .status(),
        401
    );
    let tampered = webhook_body(
        "1055",
        json!([text_message("wamid.y", "573001112222", "cuerpo alterado")]),
    );
    assert_eq!(
        post_webhook(&base, "wa1", tampered, Some(sign("shh", &attack)))
            .await
            .status(),
        401,
        "a signature of a different body"
    );
    for (index, messages, phone) in [
        (
            1,
            json!([text_message("wamid.s", "573009999999", "soy un extraño")]),
            "1055",
        ),
        (
            2,
            json!([text_message(
                "wamid.o",
                "573001112222",
                "otro número de teléfono"
            )]),
            "9999",
        ),
        (
            3,
            json!([{"id":"wamid.i","from":"573001112222","type":"image","image":{"id":"1"}}]),
            "1055",
        ),
        (
            4,
            json!([text_message("wamid.b", "573001112222", "   ")]),
            "1055",
        ),
        (5, json!([]), "1055"),
    ] {
        let body = webhook_body(phone, messages);
        assert_eq!(
            post_webhook(&base, "wa1", body.clone(), Some(sign("shh", &body)))
                .await
                .status(),
            200,
            "case {index}"
        );
    }
    let status_only = json!({"object":"whatsapp_business_account","entry":[{"changes":[{"value":{"metadata":{"phone_number_id":"1055"},"statuses":[{"id":"x","status":"delivered"}]}}]}]}).to_string().into_bytes();
    assert_eq!(
        post_webhook(
            &base,
            "wa1",
            status_only.clone(),
            Some(sign("shh", &status_only))
        )
        .await
        .status(),
        200
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Neither the stranger, the other phone, the blank text nor the receipts reached the model. A
    // picture from the allowed number was looked up (and could not be used: this Graph API fake has
    // no media), and the person was told so instead of the model being asked.
    assert_eq!(
        llm_seen.lock().unwrap().len(),
        1,
        "only the first question reached the model"
    );
    {
        let sent = sent.lock().unwrap();
        assert_eq!(
            sent.len(),
            3,
            "{:?}",
            sent.iter().map(|s| s.0.clone()).collect::<Vec<_>>()
        );
        assert_eq!(sent[1].0, "/1", "the media lookup");
        assert!(
            sent[2].1["text"]["body"]
                .as_str()
                .is_some_and(|t| t.contains("Solo puedo leer imágenes")),
            "{:?}",
            sent[2].1
        );
    }
    // Unknown, disabled and non-WhatsApp channels are a plain 404; other methods 405.
    assert_eq!(
        post_webhook(&base, "nope", body.clone(), Some(signature.clone()))
            .await
            .status(),
        404
    );
    app.db
        .col_put(
            "channels",
            "tg",
            &json!({"id":"tg","type":"telegram","enabled":true,"active":true}),
            PutOptions::default(),
        )
        .unwrap();
    assert_eq!(
        post_webhook(&base, "tg", body.clone(), Some(signature.clone()))
            .await
            .status(),
        404
    );
    assert_eq!(
        client
            .put(format!("{base}/webhook/whatsapp/wa1"))
            .send()
            .await
            .unwrap()
            .status(),
        405
    );
    app.db.col_put("channels", "wa1", &json!({"id":"wa1","type":"whatsapp","user_id":"u1","enabled":false,"active":true,"config":"{}"}), PutOptions::default()).unwrap();
    assert_eq!(
        post_webhook(&base, "wa1", body, Some(signature))
            .await
            .status(),
        404,
        "a disabled channel is not listening"
    );
    server.abort();
    graph_task.abort();
    llm.abort();
}

#[tokio::test]
async fn a_whatsapp_channel_without_numbers_or_a_secret_never_answers_and_its_status_never_leaks_secrets()
 {
    let (llm_url, llm_seen, llm) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    add_channel(
        &app,
        "whatsapp",
        "nonum",
        json!({"access_token":"EAABSECRET","phone_number_id":"1055","app_secret":"shh","verify_token":"vt"}),
    );
    add_channel(
        &app,
        "whatsapp",
        "nosecret",
        json!({"access_token":"EAABSECRET","phone_number_id":"1055","verify_token":"vt","allowed_numbers":["15551234567"]}),
    );
    add_channel(
        &app,
        "whatsapp",
        "good",
        json!({"access_token":"EAABSECRET","phone_number_id":"1055","app_secret":"shh","verify_token":"vt","allowed_numbers":["15551234567"]}),
    );
    let (base, server) = serve(app.clone()).await;
    for id in ["nonum", "nosecret"] {
        let body = webhook_body("1055", json!([text_message("w1", "15551234567", "hola")]));
        assert_eq!(
            post_webhook(&base, id, body.clone(), Some(sign("shh", &body)))
                .await
                .status(),
            404,
            "{id}: a misconfigured channel does not exist as far as Meta can tell"
        );
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(llm_seen.lock().unwrap().is_empty());
    // The channel status route reports `listening` or the problem, without any secret.
    let status = |id: &str| {
        call(
            &app,
            Some(&app.token),
            "GET",
            &format!("/api/channels/x/{id}/status"),
            json!({}),
        )
    };
    let (code, good) = status("good");
    assert_eq!(code, 200);
    assert_eq!(
        (good["status"].clone(), good["webhook"].clone()),
        (json!("listening"), json!("/webhook/whatsapp/good"))
    );
    for id in ["nonum", "nosecret"] {
        let (_, bad) = status(id);
        assert_eq!(bad["status"], "error", "{id}");
        assert!(!bad.to_string().contains("SECRET") && !bad.to_string().contains("shh"));
    }
    assert!(
        status("nonum").1["error"]
            .as_str()
            .unwrap()
            .contains("allowed_numbers")
    );
    assert!(
        status("nosecret").1["error"]
            .as_str()
            .unwrap()
            .contains("app_secret")
    );
    server.abort();
    llm.abort();
}

// ---- per-user tool policy and the audit log ----------------------------------------

async fn run_turn_as(app: &Arc<App>, user: &str, token: &str) -> (Result<()>, OneToolFixture) {
    let created = call(app, Some(token), "POST", "/api/conversations", json!({})).1;
    let thread = created["conversation"]["id"].as_str().unwrap().to_owned();
    let handle = app
        .session(&thread, &app.selection_for_user(user).unwrap())
        .unwrap();
    let (output, _receiver) = mpsc::channel(64);
    let fixture = OneToolFixture::new();
    let result = drive_turn(
        app.clone(),
        handle,
        fixture.clone(),
        &thread,
        "Crea un panel".into(),
        output,
        CancellationToken::new(),
    )
    .await;
    (result, fixture)
}
fn surface_of(app: &App, user: &str) -> bool {
    app.db
        .col_get("hive_rust_a2ui", &format!("{user}:form"))
        .unwrap()
        .is_some()
}

#[tokio::test]
async fn a_users_own_policy_replaces_the_global_one_and_every_decision_is_audited_without_arguments()
 {
    let (_home, app) = two_users();
    let admin = app.token.clone();
    let user = issue(&app, "u2");
    put_policy(
        &app,
        json!({"default":"allow","default_reason":"abierto"}),
        5000,
    );
    let put = |query: &str, body: Value| {
        call(
            &app,
            Some(&admin),
            "PUT",
            &format!("/api/tool-policy{query}"),
            body,
        )
    };
    assert_eq!(put("?userId=u2", json!({"policy":{"rules":[{"tools":["a2ui_*"],"decision":"deny","reason":"sin paneles para dos"}]}})).0, 200);
    assert_eq!(put("?userId=ghost", json!({"policy":{}})).0, 404);
    assert_eq!(
        put("?userId=u2", json!({"policy":{"default":"maybe"}})).0,
        400
    );
    assert_eq!(
        call(
            &app,
            Some(&user),
            "PUT",
            "/api/tool-policy?userId=u2",
            json!({"policy":{}})
        )
        .0,
        403,
        "a user cannot loosen their own policy"
    );
    assert_eq!(
        call(
            &app,
            Some(&admin),
            "GET",
            "/api/tool-policy?userId=u2",
            json!({})
        )
        .1["policy"]["rules"][0]["reason"],
        "sin paneles para dos"
    );
    assert_eq!(
        call(&app, Some(&admin), "GET", "/api/tool-policy", json!({})).1["policy"]["default"],
        "allow",
        "the global policy is untouched"
    );
    // The same tool call: allowed for u1 (global policy), denied for u2 (their own).
    let (first, _) = run_turn_as(&app, "u1", &admin).await;
    first.unwrap();
    let (second, fixture) = run_turn_as(&app, "u2", &user).await;
    second.unwrap();
    assert!(surface_of(&app, "u1") && !surface_of(&app, "u2"));
    assert!(
        fixture
            .seen
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("POLICY_DENIED: sin paneles para dos")
    );
    // Every decision is in the log, newest first, with the policy that decided and a hash of the arguments.
    let log = call(&app, Some(&admin), "GET", "/api/tool-audit", json!({})).1;
    let entries = log["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "{log}");
    assert_eq!(
        (
            entries[0]["user"].clone(),
            entries[0]["decision"].clone(),
            entries[0]["policy"].clone(),
            entries[0]["rule"].clone()
        ),
        (json!("u2"), json!("deny"), json!("user"), json!(0))
    );
    assert_eq!(
        (
            entries[1]["user"].clone(),
            entries[1]["decision"].clone(),
            entries[1]["policy"].clone()
        ),
        (json!("u1"), json!("allow"), json!("default"))
    );
    assert_eq!(entries[0]["tool"], "a2ui_create_surface");
    assert_eq!(
        entries[0]["arguments_sha256"],
        App::hash_token(&json!({"surfaceId":"form"}).to_string())
    );
    assert!(
        !log.to_string().contains("surfaceId"),
        "the arguments themselves are never stored"
    );
    assert!(
        entries[0]["thread"]
            .as_str()
            .unwrap()
            .starts_with("u2/webchat/")
    );
    // A user reads only their own entries, whatever they ask for; the administrator can filter.
    for query in ["", "?userId=u1"] {
        let mine = call(
            &app,
            Some(&user),
            "GET",
            &format!("/api/tool-audit{query}"),
            json!({}),
        )
        .1;
        assert!(
            mine["entries"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["user"] == "u2")
                && mine["entries"].as_array().unwrap().len() == 1,
            "{query}: {mine}"
        );
    }
    let only_one = call(
        &app,
        Some(&admin),
        "GET",
        "/api/tool-audit?userId=u1",
        json!({}),
    )
    .1;
    assert!(
        only_one["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["user"] == "u1")
            && only_one["entries"].as_array().unwrap().len() == 1
    );
    assert_eq!(call(&app, None, "GET", "/api/tool-audit", json!({})).0, 401);
    assert_eq!(
        call(&app, Some(&admin), "POST", "/api/tool-audit", json!({})).0,
        405
    );
    assert_eq!(
        call(
            &app,
            Some(&admin),
            "GET",
            "/api/tool-audit?limit=0",
            json!({})
        )
        .0,
        400
    );
    // Pagination: one at a time, newest first, resuming from the cursor.
    let page = call(
        &app,
        Some(&admin),
        "GET",
        "/api/tool-audit?limit=1",
        json!({}),
    )
    .1;
    assert_eq!(
        (
            page["entries"].as_array().unwrap().len(),
            page["entries"][0]["user"].clone()
        ),
        (1, json!("u2"))
    );
    let cursor = page["next"].as_str().unwrap().to_owned();
    let older = call(
        &app,
        Some(&admin),
        "GET",
        &format!("/api/tool-audit?limit=1&before={cursor}"),
        json!({}),
    )
    .1;
    assert_eq!(older["entries"][0]["user"], "u1");
    // Removing the user's own policy returns them to the global one.
    assert_eq!(
        call(
            &app,
            Some(&admin),
            "DELETE",
            "/api/tool-policy?userId=u2",
            json!({})
        )
        .0,
        200
    );
    run_turn_as(&app, "u2", &user).await.0.unwrap();
    assert!(
        surface_of(&app, "u2"),
        "back on the global policy, the same call is allowed"
    );
}

#[tokio::test]
async fn approvals_are_audited_and_the_log_keeps_only_the_newest_entries() {
    let (_home, app) = two_users();
    put_policy(&app, json!({"default":"ask"}), 1000);
    let admin = app.token.clone();
    run_turn_as(&app, "u1", &admin).await.0.unwrap();
    let log = call(&app, Some(&admin), "GET", "/api/tool-audit", json!({})).1;
    assert_eq!(
        log["entries"][0]["decision"], "ask_timeout",
        "the request was shown but nobody answered in time: {log}"
    );
    // Retention keeps the newest and drops the oldest.
    for n in 0..30 {
        app.audit_tool(&crate::config::AuditEntry {
            user: "u1",
            thread: "t",
            tool: "tool",
            arguments: &json!({"n":n}),
            decision: "allow",
            rule: None,
            reason: &format!("entrada {n}"),
            source: "default",
        })
        .unwrap();
    }
    assert_eq!(app.prune_audit(10).unwrap(), 21);
    let kept = app.list_audit(None, 100, None).unwrap();
    assert_eq!(kept.len(), 10);
    assert_eq!(kept[0]["reason"], "entrada 29", "the newest survives");
    assert_eq!(kept[9]["reason"], "entrada 20");
    assert_eq!(app.prune_audit(10).unwrap(), 0, "nothing more to prune");
}

// ---- scheduled tasks delivered to channels -----------------------------------------

/// Everything a scheduled task can be delivered to, all fake: a Telegram bot with two allowed
/// chats, a Discord bot, a Slack app and a WhatsApp number, plus a disabled Telegram channel and
/// another user's, neither of which may ever be written to.
struct Delivery {
    app: Arc<App>,
    _home: tempfile::TempDir,
    telegram: FakeTelegram,
    discord: FakeDiscord,
    slack: FakeSlack,
    graph_posts: Arc<Mutex<Vec<(String, Value, String)>>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}
async fn delivery_setup() -> Delivery {
    let (llm_url, _seen, llm) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    app.db
        .col_put(
            "users",
            "u2",
            &json!({"id":"u2","name":"Dos","email":null,"password_hash":null}),
            PutOptions::default(),
        )
        .unwrap();
    let telegram = fake_telegram().await;
    let discord = fake_discord().await;
    let slack = fake_slack().await;
    let (graph, graph_posts, graph_task) = fake_graph().await;
    add_channel(
        &app,
        "telegram",
        "tg",
        json!({"bot_token":"111:TG","allowed_chat_ids":[42,43],"api_base":telegram.base}),
    );
    add_channel(
        &app,
        "discord",
        "dc",
        json!({"bot_token":"DCTOKEN","allowed_channel_ids":["111"],"api_base":discord.api,"gateway_url":discord.gateway}),
    );
    add_channel(
        &app,
        "slack",
        "sl",
        json!({"bot_token":"xoxb-SLK","app_token":"xapp-SLK","allowed_channel_ids":["D1"],"api_base":slack.api}),
    );
    add_channel(
        &app,
        "whatsapp",
        "wa",
        json!({"access_token":"WATOKEN","phone_number_id":"1055","app_secret":"shh","verify_token":"vt","allowed_numbers":["57300111"],"api_base":graph}),
    );
    add_channel(
        &app,
        "telegram",
        "tg_off",
        json!({"bot_token":"222:OFF","allowed_chat_ids":[7],"api_base":telegram.base}),
    );
    app.db.col_put("channels", "tg_off", &json!({"id":"tg_off","type":"telegram","user_id":"u1","enabled":false,"active":true,"config":"{}"}), PutOptions::default()).unwrap();
    add_channel(
        &app,
        "telegram",
        "tg_u2",
        json!({"bot_token":"333:U2","allowed_chat_ids":[9],"api_base":telegram.base}),
    );
    app.db.col_put("channels", "tg_u2", &json!({"id":"tg_u2","type":"telegram","user_id":"u2","enabled":true,"active":true,"config":"{}"}), PutOptions::default()).unwrap();
    Delivery {
        app,
        _home: home,
        telegram,
        discord,
        slack,
        graph_posts,
        tasks: vec![llm, graph_task],
    }
}
impl Drop for Delivery {
    fn drop(&mut self) {
        self.app.shutdown.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}
fn due_task(app: &App, channel: Option<&str>, text: &str) {
    let due = crate::scheduler::stamp_for_tests(now() - 1000);
    let mut row = json!({"id":"t","user_id":"u1","name":"Resumen","status":"active","task_type":"one_shot","task":text,"fire_at":due,"next_run_at":due});
    if let Some(channel) = channel {
        row["channel"] = json!(channel);
    }
    app.db
        .col_put("cronJobs", "t", &row, PutOptions::default())
        .unwrap();
}
fn texts_to(fake: &FakeTelegram) -> Vec<(i64, String)> {
    fake.sent()
}
fn run_of(app: &App) -> Value {
    config::docs(&app.db, "taskRuns")
        .unwrap()
        .into_iter()
        .find(|run| run["task_id"] == "t")
        .unwrap_or(Value::Null)
}

#[tokio::test]
async fn a_scheduled_task_can_deliver_its_result_to_every_active_channel_so_it_is_not_lost() {
    let d = delivery_setup().await;
    due_task(&d.app, Some("all"), "Resume las noticias");
    let scheduler = tokio::spawn(crate::scheduler::run(d.app.clone()));
    eventually("every active channel gets the result", || {
        texts_to(&d.telegram).len() == 2
            && d.discord.posts.lock().unwrap().len() == 1
            && d.slack.posts.lock().unwrap().len() == 1
            && d.graph_posts.lock().unwrap().len() == 1
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    // The text names the task and carries the agent's answer, in each channel's own format.
    let telegram = texts_to(&d.telegram);
    assert_eq!(
        telegram
            .iter()
            .map(|(chat, _)| *chat)
            .collect::<std::collections::BTreeSet<_>>(),
        [42, 43].into()
    );
    assert!(
        telegram
            .iter()
            .all(|(_, text)| text.starts_with("⏰ Resumen") && text.contains("Hola Rust")),
        "{telegram:?}"
    );
    let discord = d.discord.posts.lock().unwrap()[0].clone();
    assert_eq!(
        (
            discord.0.as_str(),
            discord.2.as_str(),
            discord.1["allowed_mentions"].clone()
        ),
        ("/channels/111/messages", "Bot DCTOKEN", json!({"parse":[]}))
    );
    assert!(discord.1["content"].as_str().unwrap().contains("Hola Rust"));
    let slack = d.slack.posts.lock().unwrap()[0].clone();
    assert_eq!(
        (slack.0["channel"].clone(), slack.1.as_str()),
        (json!("D1"), "Bearer xoxb-SLK")
    );
    assert!(slack.0["text"].as_str().unwrap().contains("Hola Rust"));
    let whatsapp = d.graph_posts.lock().unwrap()[0].clone();
    assert_eq!(
        (
            whatsapp.0.as_str(),
            whatsapp.1["to"].clone(),
            whatsapp.2.as_str()
        ),
        ("/1055/messages", json!("57300111"), "Bearer WATOKEN")
    );
    // Only this user's active channels were written to: not the disabled one, not another user's.
    let paths: Vec<String> = d
        .telegram
        .requests
        .lock()
        .unwrap()
        .iter()
        .map(|(path, _)| path.clone())
        .collect();
    assert!(
        paths.iter().all(|path| path == "/bot111:TG/sendMessage"),
        "{paths:?}"
    );
    // The run keeps its result and a report of where it went, with no secrets in it.
    eventually("the run is recorded as completed", || {
        run_of(&d.app)["status"] == "completed"
    })
    .await;
    let run = run_of(&d.app);
    let sent: Vec<&str> = run["delivery"]["sent"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert_eq!(sent.len(), 4, "{run}");
    assert!(run["delivery"]["failed"].as_array().unwrap().is_empty());
    assert_eq!(
        run["delivery"]["sent"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == "telegram")
            .map(|e| e["destinations"].as_u64().unwrap())
            .sum::<u64>(),
        2
    );
    assert!(
        run["result"].as_str().unwrap().contains("Hola Rust"),
        "the result is still stored with the run"
    );
    for secret in ["TG", "DCTOKEN", "SLK", "WATOKEN"] {
        assert!(
            !run.to_string().contains(&format!("{secret}\""))
                && !run["delivery"].to_string().contains(secret),
            "{secret}"
        );
    }
    assert!(
        d.app
            .db
            .col_get("conversationThreads", "u1/webchat/cron-t")
            .unwrap()
            .is_some(),
        "and the conversation of the task"
    );
    d.app.shutdown.cancel();
    let _ = scheduler.await;
}

#[tokio::test]
async fn a_task_can_target_one_kind_of_channel_and_without_a_target_nothing_is_sent() {
    let d = delivery_setup().await;
    due_task(&d.app, Some("discord"), "Resume");
    let scheduler = tokio::spawn(crate::scheduler::run(d.app.clone()));
    eventually("discord gets it", || {
        d.discord.posts.lock().unwrap().len() == 1
    })
    .await;
    eventually("the run completes", || {
        run_of(&d.app)["status"] == "completed"
    })
    .await;
    assert!(
        d.telegram.sent().is_empty()
            && d.slack.posts.lock().unwrap().is_empty()
            && d.graph_posts.lock().unwrap().is_empty(),
        "only the chosen kind"
    );
    assert_eq!(run_of(&d.app)["delivery"]["sent"][0]["type"], "discord");
    d.app.shutdown.cancel();
    let _ = scheduler.await;
    // The default (and `auto`/`webchat`) keeps the result in the task's conversation.
    for channel in [None, Some("auto"), Some("webchat")] {
        let quiet = delivery_setup().await;
        due_task(&quiet.app, channel, "Resume");
        let scheduler = tokio::spawn(crate::scheduler::run(quiet.app.clone()));
        eventually("the run completes", || {
            run_of(&quiet.app)["status"] == "completed"
        })
        .await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            quiet.telegram.sent().is_empty()
                && quiet.discord.posts.lock().unwrap().is_empty()
                && quiet.slack.posts.lock().unwrap().is_empty()
                && quiet.graph_posts.lock().unwrap().is_empty(),
            "{channel:?}"
        );
        assert!(run_of(&quiet.app)["delivery"].is_null());
        quiet.app.shutdown.cancel();
        let _ = scheduler.await;
    }
}

#[tokio::test]
async fn a_failed_run_is_reported_in_plain_words_and_delivery_problems_are_visible_not_fatal() {
    // A failing model (the fake answers 429 to "fail"): every channel is told, without internals.
    let d = delivery_setup().await;
    due_task(&d.app, Some("all"), "fail");
    let scheduler = tokio::spawn(crate::scheduler::run(d.app.clone()));
    eventually("the failure alert reaches the channels", || {
        texts_to(&d.telegram).len() == 2 && d.discord.posts.lock().unwrap().len() == 1
    })
    .await;
    for (_, text) in texts_to(&d.telegram) {
        assert!(
            text.starts_with("⚠️ La tarea programada «Resumen» no se pudo completar"),
            "{text}"
        );
        assert!(!text.contains("429") && !text.contains("fixture") && !text.contains("TG"));
    }
    eventually("the run is recorded as not completed", || {
        matches!(
            run_of(&d.app)["status"].as_str(),
            Some("uncertain" | "failed")
        )
    })
    .await;
    d.app.shutdown.cancel();
    let _ = scheduler.await;
    // One channel unreachable: the others still get the result and the report says which failed.
    let partial = delivery_setup().await;
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_base = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    config::store_secret(
        &partial.app,
        "channel:tg:config",
        &json!({"bot_token":"111:TG","allowed_chat_ids":[42],"api_base":dead_base}).to_string(),
    )
    .unwrap();
    due_task(&partial.app, Some("all"), "Resume");
    let scheduler = tokio::spawn(crate::scheduler::run(partial.app.clone()));
    eventually("the reachable channels still get it", || {
        partial.discord.posts.lock().unwrap().len() == 1
            && partial.slack.posts.lock().unwrap().len() == 1
    })
    .await;
    eventually("the run records both outcomes", || {
        run_of(&partial.app)["status"] == "completed"
    })
    .await;
    let run = run_of(&partial.app);
    let failed = run["delivery"]["failed"].as_array().unwrap();
    assert_eq!(
        (failed.len(), failed[0]["type"].clone()),
        (1, json!("telegram")),
        "{run}"
    );
    assert!(
        !failed[0]["error"].as_str().unwrap().contains("111:TG"),
        "the token is not in the error: {failed:?}"
    );
    assert_eq!(run["delivery"]["sent"].as_array().unwrap().len(), 3);
    partial.app.shutdown.cancel();
    let _ = scheduler.await;
    // No active channel of the requested kind: the run still completes and says so.
    let none = delivery_setup().await;
    due_task(&none.app, Some("slack"), "Resume");
    none.app.db.col_put("channels", "sl", &json!({"id":"sl","type":"slack","user_id":"u1","enabled":false,"active":true,"config":"{}"}), PutOptions::default()).unwrap();
    let scheduler = tokio::spawn(crate::scheduler::run(none.app.clone()));
    eventually("the run completes", || {
        run_of(&none.app)["status"] == "completed"
    })
    .await;
    let report = run_of(&none.app)["delivery"].clone();
    assert!(
        report["sent"].as_array().unwrap().is_empty()
            && report["failed"][0]["error"]
                .as_str()
                .unwrap()
                .contains("no hay canales activos"),
        "{report}"
    );
    assert!(
        run_of(&none.app)["result"]
            .as_str()
            .unwrap()
            .contains("Hola Rust"),
        "the result is not lost: it is stored with the run"
    );
    none.app.shutdown.cancel();
    let _ = scheduler.await;
}

#[test]
fn the_channel_of_a_task_is_validated_when_it_is_created_or_edited() {
    let (_home, app) = two_users();
    let create = |channel: &str| {
        call(
            &app,
            Some(&app.token),
            "POST",
            "/api/cron",
            json!({"name":"T","task":"Haz algo","task_type":"interval","interval_sec":3600,"channel":channel}),
        )
    };
    for good in [
        "all", "telegram", "discord", "slack", "whatsapp", "auto", "webchat",
    ] {
        assert_eq!(create(good).0, 201, "{good}");
    }
    for bad in ["email", "ALL", "telegram,slack", "sms"] {
        let (status, body) = create(bad);
        assert_eq!(status, 400, "{bad}: {body}");
        assert!(body["error"].as_str().unwrap().contains("all"), "{body}");
    }
}

// ---- rate limits -------------------------------------------------------------------

fn set_limits(app: &App, limits: Value) {
    let (status, body) = call(app, Some(&app.token), "PUT", "/api/rate-limits", limits);
    assert_eq!(status, 200, "{body}");
}

#[test]
fn the_limits_are_off_until_set_validated_admin_only_and_effective_at_once() {
    let (_home, app) = two_users();
    let user = issue(&app, "u2");
    // Nothing is limited by default, however many requests.
    for _ in 0..200 {
        assert_eq!(
            call(&app, Some(&user), "GET", "/api/users", json!({})).0,
            200
        );
    }
    let shown = call(&app, Some(&app.token), "GET", "/api/rate-limits", json!({})).1;
    assert_eq!(
        shown,
        json!({"api_per_minute":null,"chat_per_minute":null,"channel_per_minute":null,"webhook_per_minute":null,"auth_failures_per_minute":null})
    );
    assert_eq!(
        call(&app, Some(&user), "GET", "/api/rate-limits", json!({})).0,
        403
    );
    assert_eq!(
        call(
            &app,
            Some(&user),
            "PUT",
            "/api/rate-limits",
            json!({"api_per_minute":1})
        )
        .0,
        403,
        "a user cannot lift their own limit"
    );
    assert_eq!(
        call(&app, None, "GET", "/api/rate-limits", json!({})).0,
        401
    );
    for bad in [
        json!({"api_per_minute":0}),
        json!({"api_per_minute":"60"}),
        json!({"api_per_minute":1_000_000}),
        json!({"bogus":1}),
        json!([1]),
    ] {
        assert_eq!(
            call(
                &app,
                Some(&app.token),
                "PUT",
                "/api/rate-limits",
                bad.clone()
            )
            .0,
            400,
            "{bad}"
        );
    }
    set_limits(&app, json!({"api_per_minute":50,"channel_per_minute":6}));
    let shown = call(&app, Some(&app.token), "GET", "/api/rate-limits", json!({})).1;
    assert_eq!(
        (
            shown["api_per_minute"].clone(),
            shown["channel_per_minute"].clone(),
            shown["chat_per_minute"].clone()
        ),
        (json!(50), json!(6), Value::Null)
    );
    assert_eq!(
        call(
            &app,
            Some(&app.token),
            "DELETE",
            "/api/rate-limits",
            json!({})
        )
        .0,
        200
    );
    assert_eq!(
        call(&app, Some(&app.token), "GET", "/api/rate-limits", json!({})).1["api_per_minute"],
        Value::Null
    );
}

#[test]
fn rest_requests_are_limited_per_user_and_lifting_the_limit_applies_at_once() {
    let (_home, app) = two_users();
    let user = issue(&app, "u2");
    set_limits(&app, json!({"api_per_minute":5}));
    let statuses: Vec<u16> = (0..7)
        .map(|_| call(&app, Some(&user), "GET", "/api/users", json!({})).0)
        .collect();
    assert_eq!(
        statuses,
        [200, 200, 200, 200, 200, 429, 429],
        "a burst up to the limit, then refused"
    );
    let refused = call(&app, Some(&user), "GET", "/api/users", json!({})).1;
    assert!(
        refused["retryAfterMs"].as_u64().unwrap() > 0
            && refused["error"].as_str().unwrap().contains("Demasiadas"),
        "{refused}"
    );
    // Another user's bucket is separate: the administrator is unaffected by u2's noise.
    assert_eq!(
        call(&app, Some(&app.token), "GET", "/api/users", json!({})).0,
        200
    );
    // /health is not counted, and removing the limit frees the user straight away.
    assert_eq!(call(&app, None, "GET", "/health", json!({})).0, 200);
    assert_eq!(
        call(
            &app,
            Some(&app.token),
            "DELETE",
            "/api/rate-limits",
            json!({})
        )
        .0,
        200
    );
    assert_eq!(
        call(&app, Some(&user), "GET", "/api/users", json!({})).0,
        200
    );
}

#[test]
fn a_password_login_gives_the_owner_the_shared_token_and_everyone_else_a_token_of_their_own() {
    let (_home, app) = two_users();
    app.db
        .col_put(
            "users",
            "u3",
            &json!({"id":"u3","name":"Tres","email":"tres@example.com","password_hash":"$2b$04$abcdefghijklmnopqrstuuxDugxrpcpW3csMl3JkYMLtX7yxJF.2."}),
            PutOptions::default(),
        )
        .unwrap();
    let login = |email: &str, password: &str| {
        call(
            &app,
            None,
            "POST",
            "/api/auth/login",
            json!({"email":email,"password":password}),
        )
    };
    // The owner is the administrator: unchanged, the shared token (the desktop relies on it).
    let owner = login("uno@example.com", "fixture-password");
    assert_eq!(
        (
            owner.0,
            owner.1["authToken"].clone(),
            owner.1["userId"].clone()
        ),
        (200, json!(app.token), json!("u1"))
    );
    // Anyone else gets a token of their own, never the administrator's.
    let first = login("tres@example.com", "fixture-password");
    assert_eq!((first.0, first.1["userId"].clone()), (200, json!("u3")));
    let token = first.1["authToken"].as_str().unwrap().to_owned();
    assert!(token.starts_with("hvu_") && token != app.token, "{token}");
    let principal = app.authenticate(Some(&token)).unwrap().unwrap();
    assert_eq!((principal.user.as_str(), principal.admin), ("u3", false));
    assert_eq!(
        call(&app, Some(&token), "GET", "/api/conversations", json!({})).0,
        200,
        "their own things"
    );
    assert_eq!(
        call(&app, Some(&token), "GET", "/api/tool-policy", json!({})).0,
        403,
        "never the administrator's"
    );
    assert_eq!(
        call(&app, Some(&token), "GET", "/api/channels", json!({})).0,
        403
    );
    // A second login (another device) does not end the first.
    let second = login("tres@example.com", "fixture-password").1["authToken"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(second, token);
    for t in [&token, &second] {
        assert_eq!(
            app.authenticate(Some(t)).unwrap().map(|p| p.user),
            Some("u3".to_owned())
        );
    }
    // Only hashes are stored.
    let stored = app
        .db
        .col_get("users", "u3")
        .unwrap()
        .unwrap()
        .doc
        .to_string();
    assert!(
        !stored.contains(&token)
            && !stored.contains(&second)
            && stored.contains("session_token_hashes")
    );
    // A wrong password gets nothing and mints nothing.
    let before =
        app.db.col_get("users", "u3").unwrap().unwrap().doc["session_token_hashes"].clone();
    assert_eq!(login("tres@example.com", "wrong").0, 401);
    assert_eq!(
        app.db.col_get("users", "u3").unwrap().unwrap().doc["session_token_hashes"],
        before
    );
    // At most five at once: the oldest is dropped.
    let mut later = vec![];
    for _ in 0..4 {
        later.push(
            login("tres@example.com", "fixture-password").1["authToken"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    assert!(
        app.authenticate(Some(&token)).unwrap().is_none(),
        "the oldest of six was dropped"
    );
    assert!(
        app.authenticate(Some(&second)).unwrap().is_some()
            && later
                .iter()
                .all(|t| app.authenticate(Some(t)).unwrap().is_some())
    );
    // Revoking the user's token also ends every login session.
    assert_eq!(
        call(
            &app,
            Some(&app.token),
            "DELETE",
            "/api/users/u3/token",
            json!({})
        )
        .0,
        200
    );
    assert!(
        later
            .iter()
            .chain([&second])
            .all(|t| app.authenticate(Some(t)).unwrap().is_none())
    );
    // And issuing a new one replaces them all.
    let session = login("tres@example.com", "fixture-password").1["authToken"]
        .as_str()
        .unwrap()
        .to_owned();
    let issued = call(
        &app,
        Some(&app.token),
        "POST",
        "/api/users/u3/token",
        json!({}),
    )
    .1["token"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        app.authenticate(Some(&session)).unwrap().is_none()
            && app.authenticate(Some(&issued)).unwrap().is_some()
    );
}

#[test]
fn failing_to_authenticate_is_slowed_down_but_a_valid_token_never_is() {
    let (_home, app) = two_users();
    let user = issue(&app, "u2");
    set_limits(&app, json!({"auth_failures_per_minute":3}));
    let guesses: Vec<u16> = (0..5)
        .map(|n| {
            call(
                &app,
                Some(&format!("hvu_guess{n}")),
                "GET",
                "/api/users",
                json!({}),
            )
            .0
        })
        .collect();
    assert_eq!(
        guesses,
        [401, 401, 401, 429, 429],
        "a guesser is cut off after a few wrong tokens"
    );
    assert_eq!(
        call(&app, None, "GET", "/api/users", json!({})).0,
        429,
        "so is a missing token"
    );
    // Legitimate callers are not affected: only failures use that bucket.
    for _ in 0..10 {
        assert_eq!(
            call(&app, Some(&user), "GET", "/api/users", json!({})).0,
            200
        );
        assert_eq!(
            call(&app, Some(&app.token), "GET", "/api/users", json!({})).0,
            200
        );
    }
}

#[test]
fn password_guessing_is_cut_off_per_account_without_locking_out_a_correct_password_or_other_accounts()
 {
    let (_home, app) = two_users();
    set_limits(&app, json!({"auth_failures_per_minute":3}));
    let login = |email: &str, password: &str| {
        call(
            &app,
            None,
            "POST",
            "/api/auth/login",
            json!({"email":email,"password":password}),
        )
    };
    // Correct logins never count, however many.
    for _ in 0..10 {
        assert_eq!(login("uno@example.com", "fixture-password").0, 200);
    }
    let guesses: Vec<u16> = (0..5)
        .map(|n| login("uno@example.com", &format!("guess{n}")).0)
        .collect();
    assert_eq!(
        guesses,
        [401, 401, 401, 429, 429],
        "a guesser is refused after a few wrong passwords"
    );
    let (status, body) = login("uno@example.com", "fixture-password");
    assert_eq!(
        (status, body["retryAfterMs"].is_u64()),
        (429, true),
        "while the account's bucket is empty not even the right password is checked, and it says when to retry"
    );
    // Another account is unaffected, and so are unknown emails (they cost the guesser too).
    assert_eq!(login("otro@example.com", "x").0, 401);
    for _ in 0..3 {
        login("nadie@example.com", "x");
    }
    assert_eq!(
        login("nadie@example.com", "x").0,
        429,
        "unknown accounts get the same treatment"
    );
    // Token guessers fill the shared bucket, which must never block a real login.
    for n in 0..6 {
        call(
            &app,
            Some(&format!("hvu_guess{n}")),
            "GET",
            "/api/users",
            json!({}),
        );
    }
    assert_eq!(call(&app, None, "GET", "/api/users", json!({})).0, 429);
    let (_h2, app2) = two_users();
    set_limits(&app2, json!({"auth_failures_per_minute":3}));
    for n in 0..6 {
        call(
            &app2,
            Some(&format!("hvu_guess{n}")),
            "GET",
            "/api/users",
            json!({}),
        );
    }
    assert_eq!(
        call(
            &app2,
            None,
            "POST",
            "/api/auth/login",
            json!({"email":"uno@example.com","password":"fixture-password"})
        )
        .0,
        200,
        "a legitimate login is not locked out by token guessing"
    );
    // Not configured: nothing is limited.
    let (_h3, open) = two_users();
    for n in 0..20 {
        assert_eq!(
            call(
                &open,
                None,
                "POST",
                "/api/auth/login",
                json!({"email":"uno@example.com","password":format!("g{n}")})
            )
            .0,
            401
        );
    }
}

#[tokio::test]
async fn a_chat_over_its_limit_gets_a_polite_refusal_without_a_model_call_and_other_chats_are_unaffected()
 {
    let (llm_url, llm_seen, llm) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let telegram = fake_telegram().await;
    telegram.push(1, 42, "uno", false);
    telegram.push(2, 42, "dos", false);
    telegram.push(3, 42, "tres", false);
    telegram.push(4, 43, "otro chat", false);
    set_limits(&app, json!({"channel_per_minute":2}));
    add_telegram(
        &app,
        "tg1",
        "u1",
        json!({"bot_token":"123:SECRET","allowed_chat_ids":[42,43],"api_base":telegram.base,"poll_timeout_s":1}),
    );
    let supervisor = tokio::spawn(crate::telegram::run(app.clone()));
    eventually("all four messages are answered", || {
        telegram.sent().len() == 4
    })
    .await;
    let sent = telegram.sent();
    let to_42: Vec<&String> = sent
        .iter()
        .filter(|(chat, _)| *chat == 42)
        .map(|(_, text)| text)
        .collect();
    assert_eq!(
        to_42
            .iter()
            .filter(|text| text.as_str() == "Hola Rust")
            .count(),
        2,
        "the first two went through: {sent:?}"
    );
    assert_eq!(
        to_42
            .iter()
            .filter(|text| text.starts_with("Estás enviando mensajes muy rápido"))
            .count(),
        1,
        "the third was refused politely: {sent:?}"
    );
    assert!(
        sent.iter()
            .any(|(chat, text)| *chat == 43 && text == "Hola Rust"),
        "another chat has its own allowance"
    );
    assert_eq!(
        llm_seen.lock().unwrap().len(),
        3,
        "the refused message never reached the model"
    );
    app.shutdown.cancel();
    let _ = supervisor.await;
    llm.abort();
}

#[tokio::test]
async fn web_chat_messages_and_a_public_webhook_are_limited_too() {
    let (llm_url, llm_seen, llm) = provider().await;
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), &llm_url);
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    add_channel(
        &app,
        "whatsapp",
        "wa1",
        json!({"access_token":"EAAB","phone_number_id":"1055","app_secret":"shh","verify_token":"vt","allowed_numbers":["57300111"]}),
    );
    add_channel(
        &app,
        "whatsapp",
        "wa2",
        json!({"access_token":"EAAB","phone_number_id":"1055","app_secret":"shh","verify_token":"vt","allowed_numbers":["57300111"]}),
    );
    set_limits(&app, json!({"chat_per_minute":1,"webhook_per_minute":2}));
    let (base, server) = serve(app.clone()).await;
    // The web chat: the first message is a turn, the second is refused before it is run.
    let thread = conversation(&base).await;
    let mut ws = connect_ws(&base).await;
    let first = message(&mut ws, &thread, "hola").await;
    assert!(
        first.iter().any(|event| event["type"] == "message"),
        "{first:?}"
    );
    ws.send(Message::Text(
        json!({"type":"message","threadId":thread,"content":"otra vez"}).to_string(),
    ))
    .await
    .unwrap();
    let refusal = frame(&mut ws).await;
    assert!(refusal.to_string().contains("muy rápido"), "{refusal}");
    assert_eq!(
        llm_seen.lock().unwrap().len(),
        1,
        "the refused message was not run"
    );
    // The webhook counts every request before reading it, per channel.
    let client = reqwest::Client::new();
    let verify = |id: &str| {
        client.get(format!("{base}/webhook/whatsapp/{id}?hub.mode=subscribe&hub.verify_token=vt&hub.challenge=7")).send()
    };
    assert_eq!(verify("wa1").await.unwrap().status(), 200);
    assert_eq!(verify("wa1").await.unwrap().status(), 200);
    let limited = verify("wa1").await.unwrap();
    assert_eq!(limited.status(), 429);
    assert!(limited.text().await.unwrap().contains("retryAfterMs"));
    assert_eq!(
        verify("wa2").await.unwrap().status(),
        200,
        "another channel has its own allowance"
    );
    // Unsigned traffic is limited as well, so flooding the public route costs the gateway little.
    for _ in 0..5 {
        let _ = client
            .post(format!("{base}/webhook/whatsapp/wa1"))
            .body("{}")
            .send()
            .await
            .unwrap();
    }
    assert_eq!(
        client
            .post(format!("{base}/webhook/whatsapp/wa1"))
            .body("{}")
            .send()
            .await
            .unwrap()
            .status(),
        429
    );
    server.abort();
    llm.abort();
}

#[test]
fn configuration_reads_run_side_by_side_and_a_change_waits_for_them() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = Arc::new(App::open(home.path().to_owned()).unwrap());
    let get = |app: Arc<App>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (status, _) = call(&app, Some(&app.token), "GET", "/api/providers", json!({}));
            let _ = tx.send(status);
        });
        rx
    };
    // A read in progress does not stop another read (it used to: one lock serialized every request).
    let reading = app.activity_lock.read().unwrap();
    assert_eq!(
        get(app.clone())
            .recv_timeout(Duration::from_secs(5))
            .expect("a read must not wait for another read"),
        200
    );
    // But a change is exclusive: it waits for the read to finish, so a read never sees a change half made.
    let (tx, change) = std::sync::mpsc::channel();
    let writer = app.clone();
    std::thread::spawn(move || {
        let (status, _) = call(
            &writer,
            Some(&writer.token),
            "POST",
            "/api/providers/openrouter/toggle",
            json!({"active":true}),
        );
        let _ = tx.send(status);
    });
    assert!(
        change.recv_timeout(Duration::from_millis(400)).is_err(),
        "a change must wait while a read is in progress"
    );
    drop(reading);
    assert_eq!(
        change
            .recv_timeout(Duration::from_secs(5))
            .expect("the change runs once the read ends"),
        200
    );
    // And while a change holds the lock, a read waits for it.
    let changing = app.activity_lock.write().unwrap();
    let blocked = get(app.clone());
    assert!(
        blocked.recv_timeout(Duration::from_millis(400)).is_err(),
        "a read must wait while a change is in progress"
    );
    drop(changing);
    assert_eq!(blocked.recv_timeout(Duration::from_secs(5)).unwrap(), 200);
}

#[test]
fn the_master_key_is_looked_up_once_per_profile_and_a_missing_one_is_looked_up_again() {
    let _ = std::env::var("HIVE_MASTER_KEY").map(|_| panic!("run without HIVE_MASTER_KEY"));
    let home = tempfile::tempdir().unwrap();
    let db = HiveDB::open(home.path().join("data/hivedb"))
        .or_else(|_| {
            std::fs::create_dir_all(home.path().join("data")).unwrap();
            HiveDB::open(home.path().join("data/hivedb"))
        })
        .unwrap();
    let app_home = home.path().to_owned();
    // No key anywhere: nothing is found (and nothing is remembered).
    assert!(config::secret(&db, &app_home, "nada").unwrap().is_none());
    let key = app_home.join(".master.key");
    std::fs::write(&key, hex::encode([7u8; 32])).unwrap();
    let sealed = hive_agent_core::services::secrets::seal_bytes(&[7u8; 32], &[], b"valor").unwrap();
    let split = sealed.ciphertext.len() - 32;
    db.col_put(
        "secrets",
        "prueba",
        &json!({"ciphertext":format!("{}:{}", &sealed.ciphertext[..split], &sealed.ciphertext[split..]),"iv":sealed.nonce}),
        PutOptions::default(),
    )
    .unwrap();
    assert_eq!(
        config::secret(&db, &app_home, "prueba").unwrap().as_deref(),
        Some("valor"),
        "a key that appears later is found"
    );
    // Found once, it is kept: the file can go away and the profile still opens its secrets.
    std::fs::remove_file(&key).unwrap();
    assert_eq!(
        config::secret(&db, &app_home, "prueba").unwrap().as_deref(),
        Some("valor"),
        "kept for the life of the process"
    );
}

#[test]
fn the_cheap_setup_check_gives_the_same_verdict_and_reason_as_the_full_selection() {
    let verdicts = |app: &App| {
        (
            app.selection().map(|_| ()).map_err(|e| e.to_string()),
            app.configured().map_err(|e| e.to_string()),
        )
    };
    let home = tempfile::tempdir().unwrap();
    seed(home.path(), "http://127.0.0.1:9/v1");
    let app = App::open(home.path().to_owned()).unwrap();
    let put = |collection: &str, id: &str, edit: &dyn Fn(&mut Value)| {
        let mut doc = app.db.col_get(collection, id).unwrap().unwrap().doc;
        edit(&mut doc);
        app.db
            .col_put(collection, id, &doc, PutOptions::default())
            .unwrap();
    };
    let (full, cheap) = verdicts(&app);
    assert!(full.is_ok() && cheap.is_ok(), "{full:?} {cheap:?}");
    // Every way the configuration can be broken: the same answer, with the same words.
    type Step<'a> = (&'a str, Box<dyn Fn() + 'a>);
    let breaks: Vec<Step> = vec![
        (
            "coordinator disabled",
            Box::new(|| put("agents", "a1", &|d| d["enabled"] = json!(false))),
        ),
        (
            "coordinator restored",
            Box::new(|| put("agents", "a1", &|d| d["enabled"] = json!(true))),
        ),
        (
            "provider inactive",
            Box::new(|| put("providers", "openrouter", &|d| d["active"] = json!(false))),
        ),
        (
            "provider restored",
            Box::new(|| put("providers", "openrouter", &|d| d["active"] = json!(true))),
        ),
        (
            "model disabled",
            Box::new(|| {
                put("models", "openrouter/vendor/model", &|d| {
                    d["enabled"] = json!(false)
                })
            }),
        ),
        (
            "model restored",
            Box::new(|| {
                put("models", "openrouter/vendor/model", &|d| {
                    d["enabled"] = json!(true)
                })
            }),
        ),
        (
            "no address",
            Box::new(|| {
                put("providers", "openrouter", &|d| {
                    d.as_object_mut().unwrap().remove("base_url");
                })
            }),
        ),
        (
            "address restored",
            Box::new(|| {
                put("providers", "openrouter", &|d| {
                    d["base_url"] = json!("http://127.0.0.1:9/v1")
                })
            }),
        ),
        (
            "key that cannot be read",
            Box::new(|| {
                put("secrets", "provider:openrouter:api_key", &|d| {
                    d["ciphertext"] = json!("00:11")
                })
            }),
        ),
    ];
    for (what, apply) in breaks {
        apply();
        let (full, cheap) = verdicts(&app);
        assert_eq!(
            full, cheap,
            "{what}: the full check and the cheap one disagree"
        );
        if what.contains("restored") {
            assert!(cheap.is_ok(), "{what}: {cheap:?}");
        } else {
            assert!(cheap.is_err(), "{what} must be refused");
        }
    }
    // And the route says so in the same words.
    let (_, body) = call(&app, None, "GET", "/api/setup/status", json!({}));
    assert_eq!(body["configured"], json!(false));
    assert!(
        body["error"].as_str().is_some_and(|e| !e.is_empty()),
        "{body}"
    );
}
