//! Bounded local HTTP/1.1 adapter. One request per connection; no browser origins.
use crate::{
    Result,
    activity::{self, Progress},
    config::{self, App},
};
use futures_util::{SinkExt, StreamExt};
use hive_agent_core::{
    Action, ModelRequest,
    durable_handle::DurableEngineHandle,
    provider::{CancellationToken, ModelEvent, ModelStream, NativeProvider},
};
use hivedb_core::{ColOp, PutOptions};
use http::{Request, StatusCode};
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::mpsc,
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        protocol::{Role, WebSocketConfig},
    },
};

pub const CAPABILITIES: &[&str] = &[
    "chat",
    "streaming",
    "cancel",
    "sessions",
    "narration",
    "office",
    "delegation",
    "dashboard",
    "conversations",
    "hivelive",
    "settings",
    "agents",
    "channels",
    "providers",
    "model-catalog",
    "multimodal-settings",
    "piper",
    "panel",
    "meetings",
    "http-client",
    "notes",
    "cron",
];
const MAX_BODY: usize = 1024 * 1024;

pub async fn connection<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    app: Arc<App>,
    mut socket: S,
) -> Result<()> {
    let request =
        match tokio::time::timeout(Duration::from_secs(10), read_request(&mut socket)).await {
            Ok(Ok(request)) => request,
            _ => {
                return respond(
                    &mut socket,
                    400,
                    json!({"error":"Petición HTTP inválida o demasiado grande"}),
                )
                .await;
            }
        };
    // Native Rune sends no Origin. Refuse browser-origin requests, including WebSocket upgrades.
    if request.headers().contains_key("origin") {
        return respond(
            &mut socket,
            403,
            json!({"error":"Solo cliente nativo local"}),
        )
        .await;
    }
    if matches!(
        request.uri().path(),
        "/ws" | "/realtime" | "/meeting-stream"
    ) {
        let realtime = request.uri().path() == "/realtime";
        let meeting = request.uri().path() == "/meeting-stream";
        let query = query(request.uri().query().unwrap_or_default())?;
        let token = query.get("token").map(String::as_str);
        let requested_user = query.get("session").cloned().unwrap_or_default();
        let check = app.clone();
        let token = token.map(str::to_owned);
        let user = tokio::task::spawn_blocking(move || -> Result<Option<String>> {
            let Some(principal) = check.authenticate(token.as_deref())? else {
                return Ok(None);
            };
            // A per-user token is that user and no one else, whatever `session` asks for.
            if !principal.admin {
                let own = requested_user.is_empty() || requested_user == principal.user;
                return Ok((own && !meeting && !realtime).then_some(principal.user));
            }
            let users = config::docs(&check.db, "users")?;
            Ok(users
                .iter()
                .find(|u| u["id"] == requested_user || (meeting && requested_user.is_empty()))
                .and_then(|u| u["id"].as_str())
                .map(str::to_owned))
        })
        .await??;
        let Some(user) = user else {
            return respond(&mut socket, 401, json!({"error":"Acceso requerido"})).await;
        };
        if meeting
            && !crate::meetings::allowed(
                &app,
                query
                    .get("meetingSessionId")
                    .map(String::as_str)
                    .unwrap_or_default(),
                &user,
            )?
        {
            return respond(&mut socket, 404, json!({"error":"Reunión desconocida"})).await;
        }
        let (parts, _) = request.into_parts();
        let response = match tokio_tungstenite::tungstenite::handshake::server::create_response(
            &Request::from_parts(parts, ()),
        ) {
            Ok(response) => response,
            Err(_) => {
                return respond(&mut socket, 400, json!({"error":"WebSocket inválido"})).await;
            }
        };
        let mut headers = "HTTP/1.1 101 Switching Protocols\r\n".to_string();
        for (name, value) in response.headers() {
            headers.push_str(&format!("{}: {}\r\n", name, value.to_str()?));
        }
        headers.push_str("\r\n");
        socket.write_all(headers.as_bytes()).await?;
        let config = WebSocketConfig {
            max_message_size: Some(MAX_BODY),
            max_frame_size: Some(MAX_BODY),
            ..Default::default()
        };
        let upgraded = WebSocketStream::from_raw_socket(socket, Role::Server, Some(config)).await;
        if meeting {
            return crate::meetings::serve(
                app,
                user,
                query.get("meetingSessionId").cloned().unwrap_or_default(),
                upgraded,
            )
            .await;
        }
        if realtime {
            return crate::realtime::serve(app, user, query, upgraded).await;
        }
        return websocket(app, user, upgraded).await;
    }
    // The public routes: the WhatsApp and Slack webhooks, each protected by its own signature check.
    if request.uri().path().starts_with("/webhook/whatsapp/") {
        let (code, value) = crate::whatsapp::handle(app, request).await;
        return respond(&mut socket, code, value).await;
    }
    if request.uri().path().starts_with("/webhook/slack/") {
        let (code, value) = crate::slack::handle(app, request).await;
        return respond(&mut socket, code, value).await;
    }
    let result = tokio::task::spawn_blocking(move || route(&app, request)).await?;
    let (code, value) = result.unwrap_or_else(|e| (500, json!({"error":e.to_string()})));
    respond(&mut socket, code, value).await
}

async fn read_request<S: AsyncRead + Unpin>(socket: &mut S) -> Result<Request<Vec<u8>>> {
    // Read exactly through the header so WebSocket frames cannot be consumed by HTTP.
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        if bytes.len() >= 32768 {
            return Err("HTTP headers demasiado grandes".into());
        }
        bytes.push(socket.read_u8().await?);
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    if !parsed.parse(&bytes)?.is_complete() {
        return Err("HTTP incompleto".into());
    }
    let mut builder = Request::builder()
        .method(parsed.method.ok_or("Falta método")?)
        .uri(parsed.path.ok_or("Falta ruta")?);
    let mut content_length = None;
    for header in parsed.headers {
        if header.name.eq_ignore_ascii_case("transfer-encoding") {
            return Err("Transfer-Encoding no soportado".into());
        }
        if header.name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err("Content-Length duplicado".into());
            }
            content_length = Some(std::str::from_utf8(header.value)?.parse::<usize>()?);
        }
        builder = builder.header(header.name, header.value);
    }
    let size = content_length.unwrap_or(0);
    if size > MAX_BODY {
        return Err("Petición demasiado grande".into());
    }
    let mut body = vec![0; size];
    socket.read_exact(&mut body).await?;
    Ok(builder.body(body)?)
}

async fn respond<S: AsyncWrite + Unpin>(socket: &mut S, code: u16, value: Value) -> Result<()> {
    // A string body is sent as plain text (WhatsApp's verification must echo its challenge as is).
    let (body, kind) = match &value {
        Value::String(text) => (text.clone(), "text/plain; charset=utf-8"),
        other => (other.to_string(), "application/json"),
    };
    let reason = StatusCode::from_u16(code)?
        .canonical_reason()
        .unwrap_or("Error");
    let headers = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    // One write for head and body: two small writes invite a stall on the second one.
    let mut reply = Vec::with_capacity(headers.len() + body.len());
    reply.extend_from_slice(headers.as_bytes());
    reply.extend_from_slice(body.as_bytes());
    tokio::time::timeout(Duration::from_secs(10), async {
        socket.write_all(&reply).await?;
        socket.shutdown().await
    })
    .await??;
    Ok(())
}

fn route(app: &App, req: Request<Vec<u8>>) -> Result<(u16, Value)> {
    let path = req.uri().path();
    let method = req.method().as_str();
    if method == "GET" && path == "/health" {
        return Ok((
            200,
            json!({"status":"ok", "backend":"hive-gateway-rust", "protocol":2,
            "home":app.home, "dbPath":app.db_path, "desktopContract":4, "capabilities":CAPABILITIES}),
        ));
    }
    if method == "GET" && path == "/api/setup/status" {
        return Ok((
            200,
            match app.configured() {
                Ok(()) => json!({"configured":true}),
                Err(e) => json!({"configured":false,"error":e.to_string()}),
            },
        ));
    }
    if method == "GET" && path == "/api/auth/status" {
        return Ok((200, json!({"hasCredentials":app.has_credentials()?})));
    }
    if method == "POST" && path == "/api/auth/login" {
        let body: Value = serde_json::from_slice(req.body())?;
        let email = body["email"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        let password = body["password"].as_str().unwrap_or_default();
        // Failed logins are limited per account. Asking first means a guesser is refused before
        // any password is checked, and a correct password still works once the bucket refills.
        // Only the account's own bucket is asked: the shared one is filled by token guessers and
        // must never lock a real person out.
        let account = format!("login:{email}");
        if let Err(wait) = app.limit_open(crate::ratelimit::Kind::AuthFailure, &account) {
            return Ok(too_many(wait));
        }
        for user in config::docs(&app.db, "users")? {
            if user["email"] == email
                && !email.is_empty()
                && let Some(hash) = user["password_hash"].as_str()
                && config::verify_password(password, hash)?
            {
                let id = config::string(&user, "id")?;
                // The owner is the administrator and keeps the shared token (the desktop uses it);
                // anyone else gets a token of their own, which reaches only what is theirs.
                if id == app.owner_id()? {
                    return Ok((200, json!({"authToken":app.token,"userId":id})));
                }
                return Ok((
                    200,
                    json!({"authToken":mint_session_token(app, id)?,"userId":id}),
                ));
            }
        }
        // A failure is what is counted; success never is.
        for key in [account.as_str(), "all"] {
            let _ = app.limit(crate::ratelimit::Kind::AuthFailure, key);
        }
        return Ok((401, json!({"error":"Correo o contraseña incorrectos"})));
    }
    let token = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    let Some(principal) = app.authenticate(token)? else {
        // Failing to authenticate is what a guesser does; slow it down (never a valid token).
        if let Err(wait) = app.limit(crate::ratelimit::Kind::AuthFailure, "all") {
            return Ok(too_many(wait));
        }
        return Ok((401, json!({"error":"Acceso requerido"})));
    };
    if let Err(wait) = app.limit(crate::ratelimit::Kind::Api, &principal.user) {
        return Ok(too_many(wait));
    }
    let user = principal.user.as_str();
    if !principal.admin {
        return user_route(app, user, method, path, &req);
    }
    if let Some(response) = user_token_route(app, method, path)? {
        return Ok(response);
    }
    if let Some(response) = rate_limit_route(app, method, path, req.body())? {
        return Ok(response);
    }
    if let Some(response) = tool_policy_route(
        app,
        method,
        path,
        req.uri().query().unwrap_or_default(),
        req.body(),
    )? {
        return Ok(response);
    }
    if let Some(response) = crate::computer::route(app, method, path, req.body())? {
        return Ok(response);
    }
    if let Some(response) = crate::oracle::route(app, method, path, req.body())? {
        return Ok(response);
    }
    if let Some(response) = crate::ocr::route(app, method, path, req.body())? {
        return Ok(response);
    }
    if let Some(response) = approval_route(app, &principal, method, path, req.body())? {
        return Ok(response);
    }
    if let Some(response) = audit_route(
        app,
        &principal,
        method,
        path,
        req.uri().query().unwrap_or_default(),
    )? {
        return Ok(response);
    }
    if let Some(response) = crate::integrations::route(
        app,
        method,
        path,
        req.uri().query().unwrap_or_default(),
        req.body(),
        user,
    )? {
        return Ok(response);
    }
    if let Some(response) = crate::meetings::route(app, method, path, req.body())? {
        return Ok(response);
    }
    if let Some(response) = crate::piper::route(app, method, path, req.body())? {
        return Ok(response);
    }
    if let Some(response) = crate::management::route(
        app,
        method,
        path,
        req.uri().query().unwrap_or_default(),
        req.body(),
        user,
    )? {
        return Ok(response);
    }
    if let Some(response) = crate::realtime::route(app, method, path, req.body())? {
        return Ok(response);
    }
    if method == "GET"
        && let Some(response) =
            crate::conversations::route(app, path, &query(req.uri().query().unwrap_or(""))?, user)?
    {
        return Ok(response);
    }
    if method == "GET"
        && let Some(response) =
            crate::dashboard::route(app, path, req.uri().query().unwrap_or(""), now())?
    {
        return Ok(response);
    }
    if method == "GET" && matches!(path, "/api/agents" | "/api/office") {
        return Ok((
            200,
            if path == "/api/agents" {
                json!({"agents":activity::agents(&app.db,user)?})
            } else {
                let _guard = app
                    .activity_lock
                    .write()
                    .map_err(|_| "Actividad bloqueada")?;
                activity::snapshot(&app.db, user)?
            },
        ));
    }
    if method == "GET" && path == "/api/users" {
        let users: Vec<Value> = config::docs(&app.db, "users")?
            .into_iter()
            .map(|u| json!({"id":u["id"],"name":u["name"]}))
            .collect();
        return Ok((200, json!(users)));
    }
    if method == "POST" && path == "/api/conversations" {
        return create_conversation(app, user, req.body());
    }
    Ok((
        501,
        json!({"error":"Función no disponible en esta primera integración Rust","capabilities":CAPABILITIES}),
    ))
}

fn create_conversation(app: &App, user: &str, raw: &[u8]) -> Result<(u16, Value)> {
    let selected = app.selection_for_user(user)?;
    let peer = uuid::Uuid::new_v4().to_string();
    let id = format!("{}/webchat/{peer}", selected.user);
    let body: Value = serde_json::from_slice(raw)?;
    let title = body["title"]
        .as_str()
        .map(str::trim)
        .filter(|title| !title.is_empty());
    if title.is_some_and(|title| title.chars().count() > 160) {
        return Ok((
            400,
            json!({"error":"El título admite hasta 160 caracteres"}),
        ));
    }
    let row = json!({"id":id,"user_id":selected.user,"channel":"webchat","peer_id":peer,
        "peer_kind":"direct","title":title,"archived":false,"created_at":now(),"last_message_at":now(),"message_count":0});
    app.db.col_put(
        "conversationThreads",
        &id,
        &row,
        PutOptions {
            expected_version: Some(0),
        },
    )?;
    Ok((
        201,
        json!({"conversation":crate::conversations::wire(&row)}),
    ))
}

fn too_many(wait_ms: u64) -> (u16, Value) {
    (
        429,
        json!({"error":"Demasiadas peticiones; espera un momento","retryAfterMs":wait_ms}),
    )
}

/// Administrator-only: read, set or clear the rate limits. `PUT` takes
/// `{"api_per_minute":120,"chat_per_minute":20,"channel_per_minute":6,"webhook_per_minute":120,"auth_failures_per_minute":30}`
/// (each 1..=100000 or null for unlimited) and replaces the whole set; `DELETE` removes every limit.
fn rate_limit_route(
    app: &App,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<Option<(u16, Value)>> {
    if path != "/api/rate-limits" {
        return Ok(None);
    }
    let existing = app.db.col_get("rateLimits", "default")?;
    Ok(Some(match method {
        "GET" => (
            200,
            existing.map_or_else(
                || crate::ratelimit::Config::default().to_json(),
                |row| row.doc["limits"].clone(),
            ),
        ),
        "PUT" => {
            let limits: Value = serde_json::from_slice(body)?;
            if let Err(reason) = crate::ratelimit::Config::from_json(&limits) {
                return Ok(Some((400, json!({"error":reason}))));
            }
            app.db.col_put(
                "rateLimits",
                "default",
                &json!({"id":"default","limits":limits}),
                PutOptions {
                    expected_version: Some(existing.map_or(0, |row| row.version)),
                },
            )?;
            app.reload_rate_limits();
            (200, json!({"ok":true}))
        }
        "DELETE" => {
            if existing.is_some() {
                app.db.col_batch(&[hivedb_core::ColOp::Delete {
                    collection: "rateLimits".into(),
                    id: "default".into(),
                }])?;
            }
            app.reload_rate_limits();
            (200, json!({"ok":true}))
        }
        _ => (405, json!({"error":"Método no permitido"})),
    }))
}

/// Administrator-only: read, set or clear the tool policy applied to every turn.
/// Body of PUT: `{"policy": {...}, "approval_timeout_ms": 120000}`; the policy is
/// validated before it is stored, so an invalid one can never block or open anything.
fn tool_policy_route(
    app: &App,
    method: &str,
    path: &str,
    query: &str,
    body: &[u8],
) -> Result<Option<(u16, Value)>> {
    if path != "/api/tool-policy" {
        return Ok(None);
    }
    // `?userId=` targets one user's own policy, which replaces the global one for them.
    let target = query_user(query)?;
    let key = match &target {
        Some(user) => {
            if !config::docs(&app.db, "users")?
                .iter()
                .any(|row| row["id"] == user.as_str())
            {
                return Ok(Some((404, json!({"error":"Usuario desconocido"}))));
            }
            format!("user:{user}")
        }
        None => "default".to_owned(),
    };
    let existing = app.db.col_get("toolPolicy", &key)?;
    Ok(Some(match method {
        "GET" => (
            200,
            json!({"policy":existing.as_ref().map(|row| row.doc["policy"].clone()),
            "approval_timeout_ms":existing.as_ref().and_then(|row| row.doc["approval_timeout_ms"].as_u64()).unwrap_or(config::DEFAULT_APPROVAL_TIMEOUT_MS)}),
        ),
        "PUT" => {
            let body: Value = serde_json::from_slice(body)?;
            if let Err(error) =
                hive_agent_core::policy::ToolPolicy::from_value(body["policy"].clone())
            {
                return Ok(Some((400, json!({"error":error.to_string()}))));
            }
            let timeout = body["approval_timeout_ms"]
                .as_u64()
                .unwrap_or(config::DEFAULT_APPROVAL_TIMEOUT_MS);
            if !(1_000..=3_600_000).contains(&timeout) {
                return Ok(Some((
                    400,
                    json!({"error":"approval_timeout_ms debe estar entre 1000 y 3600000"}),
                )));
            }
            app.db.col_put(
                "toolPolicy",
                &key,
                &json!({"id":key,"policy":body["policy"],"approval_timeout_ms":timeout}),
                PutOptions {
                    expected_version: Some(existing.map_or(0, |row| row.version)),
                },
            )?;
            (200, json!({"ok":true}))
        }
        "DELETE" => {
            if existing.is_some() {
                app.db.col_batch(&[hivedb_core::ColOp::Delete {
                    collection: "toolPolicy".into(),
                    id: key,
                }])?;
            }
            (200, json!({"ok":true}))
        }
        _ => (405, json!({"error":"Método no permitido"})),
    }))
}

fn query_user(query: &str) -> Result<Option<String>> {
    Ok(self::query(query)?
        .get("userId")
        .filter(|id| !id.is_empty())
        .cloned())
}

/// `GET /api/tool-audit?limit=&before=&userId=`: the audit log of tool decisions, newest first.
/// The administrator sees everyone's (and may filter by `userId`); a user sees only their own,
/// whatever `userId` says. Arguments are never in it, only their hash.
fn audit_route(
    app: &App,
    principal: &config::Principal,
    method: &str,
    path: &str,
    query: &str,
) -> Result<Option<(u16, Value)>> {
    if path != "/api/tool-audit" {
        return Ok(None);
    }
    if method != "GET" {
        return Ok(Some((405, json!({"error":"Método no permitido"}))));
    }
    let params = self::query(query)?;
    let limit = match params.get("limit").map(|v| v.parse::<usize>()) {
        None => 50,
        Some(Ok(value)) if (1..=500).contains(&value) => value,
        _ => {
            return Ok(Some((
                400,
                json!({"error":"limit debe estar entre 1 y 500"}),
            )));
        }
    };
    let user = if principal.admin {
        query_user(query)?
    } else {
        Some(principal.user.clone())
    };
    let entries = app.list_audit(
        user.as_deref(),
        limit,
        params.get("before").map(String::as_str),
    )?;
    let next = (entries.len() == limit)
        .then(|| {
            entries
                .last()
                .and_then(|e| e["id"].as_str().map(str::to_owned))
        })
        .flatten();
    Ok(Some((200, json!({"entries":entries,"next":next}))))
}

/// `POST /api/approvals/{id}` with `{"approved": true|false}`: the REST twin of the
/// WebSocket `approval` message.
fn approval_route(
    app: &App,
    principal: &config::Principal,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<Option<(u16, Value)>> {
    let Some(id) = path.strip_prefix("/api/approvals/") else {
        return Ok(None);
    };
    if method != "POST" {
        return Ok(Some((405, json!({"error":"Método no permitido"}))));
    }
    let body: Value = serde_json::from_slice(body)?;
    Ok(Some(
        if app.resolve_approval(&decode(id)?, principal, body["approved"] == true)? {
            (200, json!({"ok":true}))
        } else {
            (404, json!({"error":"Aprobación desconocida o ya resuelta"}))
        },
    ))
}

/// Administrator-only: issue or revoke a user's own token. A token is shown once and only
/// its SHA-256 is stored (`users.api_token_hash`); issuing again replaces the old one.
/// How many session tokens a user may hold at once; the oldest is dropped past that.
const MAX_SESSION_TOKENS: usize = 5;

/// A new token for `user`, added to the ones they already hold (a login on another device must
/// not end this one). Only its hash is stored.
fn mint_session_token(app: &App, user: &str) -> Result<String> {
    let token = format!(
        "hvu_{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let row = app
        .db
        .col_get("users", user)?
        .ok_or("Usuario desconocido")?;
    let mut doc = row.doc;
    let mut hashes: Vec<Value> = doc["session_token_hashes"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    hashes.push(json!(App::hash_token(&token)));
    let extra = hashes.len().saturating_sub(MAX_SESSION_TOKENS);
    hashes.drain(..extra);
    doc["session_token_hashes"] = json!(hashes);
    app.db.col_put(
        "users",
        user,
        &doc,
        PutOptions {
            expected_version: Some(row.version),
        },
    )?;
    Ok(token)
}

fn user_token_route(app: &App, method: &str, path: &str) -> Result<Option<(u16, Value)>> {
    let Some(id) = path
        .strip_prefix("/api/users/")
        .and_then(|rest| rest.strip_suffix("/token"))
    else {
        return Ok(None);
    };
    if !matches!(method, "POST" | "DELETE") {
        return Ok(Some((405, json!({"error":"Método no permitido"}))));
    }
    let id = decode(id)?;
    let Some(row) = app.db.col_get("users", &id)? else {
        return Ok(Some((404, json!({"error":"Usuario desconocido"}))));
    };
    let mut doc = row.doc;
    let issued = (method == "POST").then(|| {
        format!(
            "hvu_{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        )
    });
    doc["api_token_hash"] = match &issued {
        Some(token) => json!(App::hash_token(token)),
        None => json!(null),
    };
    // Replacing or revoking the token also ends every session a password login started.
    doc["session_token_hashes"] = json!([]);
    app.db.col_put(
        "users",
        &id,
        &doc,
        PutOptions {
            expected_version: Some(row.version),
        },
    )?;
    Ok(Some(match issued {
        Some(token) => (200, json!({"userId":id,"token":token})),
        None => (200, json!({"userId":id,"revoked":true})),
    }))
}

/// What a per-user token may reach: that user's own conversations, scheduled tasks, notes,
/// canvas surfaces, agents and activity. Anything else (providers, channels, tools, skills,
/// MCP, ethics, settings, other users, HTTP egress, voice, dashboards, meetings) needs the
/// administrator token and is refused, not silently scoped.
fn user_route(
    app: &App,
    user: &str,
    method: &str,
    path: &str,
    req: &Request<Vec<u8>>,
) -> Result<(u16, Value)> {
    let own = path == "/api/conversations"
        || path == "/api/chat/history"
        || path == "/api/notes"
        || path.starts_with("/api/cron")
        || path.starts_with("/api/approvals/")
        || path == "/api/tool-audit"
        || path.starts_with("/api/a2ui/surfaces")
        || (method == "GET" && matches!(path, "/api/agents" | "/api/office" | "/api/users"));
    if !own {
        return Ok((403, json!({"error":"Requiere el token de administrador"})));
    }
    let query_text = req.uri().query().unwrap_or_default();
    if let Some(response) =
        crate::integrations::route(app, method, path, query_text, req.body(), user)?
    {
        return Ok(response);
    }
    if method == "GET"
        && let Some(response) = crate::conversations::route(app, path, &query(query_text)?, user)?
    {
        return Ok(response);
    }
    let principal = config::Principal {
        user: user.to_owned(),
        admin: false,
    };
    if let Some(response) = approval_route(app, &principal, method, path, req.body())? {
        return Ok(response);
    }
    if let Some(response) = audit_route(app, &principal, method, path, query_text)? {
        return Ok(response);
    }
    // The same handlers the administrator uses, acting as this user: the user's own
    // profile and agents come from `management`, which filters by the acting user.
    if method == "GET"
        && matches!(path, "/api/users" | "/api/agents")
        && let Some(response) =
            crate::management::route(app, method, path, query_text, req.body(), user)?
    {
        return Ok(response);
    }
    match (method, path) {
        ("GET", "/api/office") => {
            let _guard = app
                .activity_lock
                .write()
                .map_err(|_| "Actividad bloqueada")?;
            Ok((200, activity::snapshot(&app.db, user)?))
        }
        ("POST", "/api/conversations") => create_conversation(app, user, req.body()),
        _ => Ok((404, json!({"error":"Ruta desconocida"}))),
    }
}

pub(crate) fn query(raw: &str) -> Result<std::collections::HashMap<String, String>> {
    raw.split('&')
        .filter(|s| !s.is_empty())
        .map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            Ok((decode(key)?, decode(value)?))
        })
        .collect()
}
pub(crate) fn decode(s: &str) -> Result<String> {
    let mut bytes = Vec::new();
    let mut i = 0;
    while i < s.len() {
        match s.as_bytes()[i] {
            b'%' if i + 2 < s.len() => {
                bytes.push(hex::decode(&s.as_bytes()[i + 1..i + 3])?[0]);
                i += 3;
            }
            b'%' => return Err("Escape de URL inválido".into()),
            b'+' => {
                bytes.push(b' ');
                i += 1;
            }
            b => {
                bytes.push(b);
                i += 1;
            }
        }
    }
    Ok(String::from_utf8(bytes)?)
}
pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

async fn websocket<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    app: Arc<App>,
    user: String,
    socket: WebSocketStream<S>,
) -> Result<()> {
    let _connection = app.dashboard.connect();
    let (mut sink, mut input) = socket.split();
    let (output, mut receiver) = mpsc::channel::<Value>(32);
    let mut office = app.office_events.subscribe();
    let mut subscribed = false;
    let cancel = app.shutdown.child_token();
    let _cancel_on_drop = CancelOnDrop(cancel.clone());
    let writer_cancel = cancel.clone();
    let mut writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = writer_cancel.cancelled() => break,
                value = receiver.recv() => {
                    let Some(value) = value else { break };
                    if !matches!(tokio::time::timeout(Duration::from_secs(10), sink.send(Message::Text(value.to_string()))).await, Ok(Ok(()))) { break; }
                }
            }
        }
    });
    let coordinator = config::docs(&app.db, "agents")?
        .into_iter()
        .find(|agent| agent["user_id"] == user && agent["role"] == "coordinator");
    output
        .send(json!({"type":"welcome","agent":coordinator.map(|agent|json!({"id":agent["id"],"name":agent["name"]})).unwrap_or_else(||json!({"name":"Hive · Rust"}))}))
        .await?;
    loop {
        tokio::select! {
            _ = &mut writer => break,
            _ = cancel.cancelled() => break,
            event = office.recv(), if subscribed => {
                match event {
                    Ok(event) if event["userId"]==user => { output.send(event).await?; },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        output.send(activity::current(app.clone(),user.clone()).await?).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    _ => {},
                }
            }
            frame = input.next() => {
                let text = match frame {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(_))) => { let _ = output.send(json!({"type":"pong"})).await; continue; },
                    Some(Ok(Message::Pong(_))) => continue,
                    _ => break,
                };
                let mut value: Value = match serde_json::from_str(&text) { Ok(v) => v, Err(_) => continue };
                if value["type"]=="a2ui:action" {
                    // An answer to an approval is the gateway's to resolve; it never becomes a prompt.
                    match crate::panel::approval_action(&value["data"]) {
                        crate::panel::ApprovalAction::Answer(id, approved) => {
                            let principal = config::Principal { user: user.clone(), admin: false };
                            if !app.resolve_approval(&id, &principal, approved)? {
                                send_error(&output, "Aprobación desconocida o ya resuelta").await;
                            }
                            continue;
                        }
                        crate::panel::ApprovalAction::Invalid => {
                            send_error(&output, "Respuesta de aprobación no válida").await;
                            continue;
                        }
                        crate::panel::ApprovalAction::NotApproval => {}
                    }
                    match crate::panel::action(&app,&user,&value["data"]) {Ok((thread,prompt))=>value=json!({"type":"message","threadId":thread,"content":prompt}),Err(e)=>{send_error(&output,&e.to_string()).await;continue;}}
                }
                match value["type"].as_str() {
                    Some("message") => {
                        if app.limit(crate::ratelimit::Kind::Chat, &user).is_err() {
                            send_error(&output, "Estás enviando mensajes muy rápido; espera un momento").await;
                            continue;
                        }
                        let thread = value["threadId"].as_str().unwrap_or_default().to_owned();
                        let prompt = value["content"].as_str().unwrap_or_default().to_owned();
                        if prompt.trim().is_empty() { send_thread_error(&output,"Mensaje vacío",Some(&thread)).await; continue; }
                        let turn_cancel = cancel.child_token();
                        let occupied = {
                            let mut active = app.active.lock().map_err(|_| "Turnos bloqueados")?;
                            if active.contains_key(&thread) { true } else { active.insert(thread.clone(),turn_cancel.clone()); false }
                        };
                        if occupied { send_thread_error(&output,"Ya hay un turno activo en esta conversación",Some(&thread)).await; continue; }
                        let app = app.clone(); let output = output.clone(); let user = user.clone();
                        tokio::spawn(async move {
                            let _guard = ActiveTurn { app: app.clone(), thread:thread.clone() };
                            if let Err(e) = turn(app, &user, &thread, prompt, output.clone(), turn_cancel).await { send_thread_error(&output,&e.to_string(),Some(&thread)).await; }
                        });
                    }
                    Some("stop") => {
                        let thread = value["threadId"].as_str().unwrap_or_default();
                        if crate::conversations::owned(&app,thread,&user)?
                            && let Some(token) = app.active.lock().map_err(|_| "Turnos bloqueados")?.get(thread) { token.cancel(); }
                    }
                    Some("approval") => {
                        let principal = config::Principal { user: user.clone(), admin: false };
                        let id = value["approvalId"].as_str().unwrap_or_default();
                        if !app.resolve_approval(id, &principal, value["approved"] == true)? {
                            send_error(&output, "Aprobación desconocida o ya resuelta").await;
                        }
                    }
                    Some("canvas_subscribe") => {
                        while office.try_recv().is_ok() {}
                        subscribed = true;
                        for row in crate::integrations::surfaces(&app,&user)? {for frame in crate::panel::frames(&row){output.send(frame).await?;}}
                        output.send(activity::current(app.clone(),user.clone()).await?).await?;
                    }
                    Some("notification_sync" | "notification_ack" | "pong") => {},
                    _ => send_error(&output,"Mensaje no soportado por el gateway Rust").await,
                }
            }
        }
    }
    cancel.cancel();
    writer.abort();
    Ok(())
}
struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
struct ActiveTurn {
    app: Arc<App>,
    thread: String,
}

/// Voice acknowledges immediately; the SDK job keeps draining even after hangup.
pub(crate) fn start_live_work(
    app: Arc<App>,
    user: String,
    thread: String,
    prompt: String,
    delivery: mpsc::Sender<Value>,
    forward_approvals: bool,
) -> Result<String> {
    if !crate::conversations::runnable(&app, &thread, &user)? {
        return Err("Conversación desconocida".into());
    }
    let cancel = app.shutdown.child_token();
    {
        let mut active = app.active.lock().map_err(|_| "Turnos bloqueados")?;
        if active.contains_key(&thread) {
            return Err("Ya hay un turno activo en esta conversación; consulta su estado".into());
        }
        active.insert(thread.clone(), cancel.clone());
    }
    let job = uuid::Uuid::new_v4().to_string();
    let task = job.clone();
    tokio::spawn(async move {
        let _guard = ActiveTurn {
            app: app.clone(),
            thread: thread.clone(),
        };
        let (output, mut receiver) = mpsc::channel(64);
        let execution = turn(app, &user, &thread, prompt, output, cancel);
        tokio::pin!(execution);
        let mut answer = String::new();
        let mut open = true;
        let result = loop {
            tokio::select! {
                frame=receiver.recv(),if open=>match frame {
                    Some(frame)=>if frame["type"]=="message" && frame["isChunk"]!=true {
                        answer=frame["content"].as_str().unwrap_or_default().to_owned();
                    } else if forward_approvals && frame["type"]=="approval_request" {
                        // A channel has no WebSocket: whoever asked for the turn relays the question
                        // to the person (see `channels::ask_agent`).
                        let _ = tokio::time::timeout(Duration::from_secs(2), delivery.send(frame)).await;
                    },
                    None=>open=false,
                },
                result=&mut execution=>break result,
            }
        };
        while let Ok(frame) = receiver.try_recv() {
            if frame["type"] == "message" && frame["isChunk"] != true {
                answer = frame["content"].as_str().unwrap_or_default().to_owned();
            }
        }
        let event = match result {
            Ok(()) => json!({"type":"live_result","jobId":task,"ok":true,"content":answer}),
            Err(error) => {
                json!({"type":"live_result","jobId":task,"ok":false,"error":error.to_string()})
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(2), delivery.send(event)).await;
    });
    Ok(job)
}
impl Drop for ActiveTurn {
    fn drop(&mut self) {
        if let Ok(mut active) = self.app.active.lock() {
            active.remove(&self.thread);
        }
    }
}

async fn dispatch(handle: &Arc<DurableEngineHandle>, command: Value) -> Result<Value> {
    let handle = handle.clone();
    let result =
        tokio::task::spawn_blocking(move || handle.dispatch(&command.to_string())).await??;
    Ok(serde_json::from_str(&result)?)
}

async fn turn(
    app: Arc<App>,
    user: &str,
    thread: &str,
    prompt: String,
    output: mpsc::Sender<Value>,
    cancel: CancellationToken,
) -> Result<()> {
    let host = app.clone();
    let thread_id = thread.to_owned();
    let user_id = user.to_owned();
    let (selected, handle) = tokio::task::spawn_blocking(move || -> Result<_> {
        let selected = host.selection_for_user(&user_id)?;
        if selected.user != user_id {
            return Err("Usuario no accesible".into());
        }
        let handle = host.session(&thread_id, &selected)?;
        Ok((selected, handle))
    })
    .await??;
    drive_turn(
        app,
        handle,
        selected.provider,
        thread,
        prompt,
        output,
        cancel,
    )
    .await
}

trait TurnStream {
    fn next(&self) -> impl std::future::Future<Output = Result<Option<ModelEvent>>> + Send;
    fn cancel(&self);
}
impl TurnStream for ModelStream {
    async fn next(&self) -> Result<Option<ModelEvent>> {
        Ok(ModelStream::next(self).await?)
    }
    fn cancel(&self) {
        ModelStream::cancel(self);
    }
}
trait TurnProvider: Send + Sync + 'static {
    type Stream: TurnStream + Send;
    type Child: TurnProvider;
    fn start(&self, request: ModelRequest) -> Self::Stream;
    fn child(&self, provider: NativeProvider) -> Self::Child;
}
impl TurnProvider for NativeProvider {
    type Stream = ModelStream;
    type Child = NativeProvider;
    fn start(&self, request: ModelRequest) -> ModelStream {
        NativeProvider::start(self, request, true)
    }
    fn child(&self, provider: NativeProvider) -> NativeProvider {
        provider
    }
}

#[derive(Clone)]
struct RunContext {
    provider_id: String,
    model_id: String,
    user: String,
    actor: Value,
    thread: String,
    root: Option<String>,
    parent: Option<String>,
    depth: u32,
    objective_id: String,
    /// Pictures that came with the message (from a channel), as attachment objects.
    attachments: Vec<Value>,
}

async fn drive_turn<P: TurnProvider>(
    app: Arc<App>,
    handle: Arc<DurableEngineHandle>,
    provider: P,
    thread: &str,
    mut prompt: String,
    output: mpsc::Sender<Value>,
    cancel: CancellationToken,
) -> Result<()> {
    let db = app.clone();
    let thread_id = thread.to_owned();
    let (actor, policy, provider_id, model_id, attachments, unseen) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            // Pictures a channel left for this turn: taken first, so they never wait for a later one.
            let taken = db
                .turn_attachments
                .lock()
                .ok()
                .and_then(|mut all| all.remove(&thread_id))
                .unwrap_or_default();
            // The turn runs as the conversation's owner, never as "the first user".
            let owner = db
                .db
                .col_get("conversationThreads", &thread_id)?
                .and_then(|row| row.doc["user_id"].as_str().map(str::to_owned))
                .ok_or("Conversación inexistente")?;
            let selected = db.selection_for_user(&owner)?;
            let user = &selected.user;
            let agents = config::docs(&db.db, "agents")?;
            let actor = selected.agent;
            let workers = agents
                .iter()
                .filter(|agent| config::enabled_worker(agent, user))
                .count();
            let policy = crate::oracle::with_review_budget(
                config::with_computer_budget(
                    config::turn_policy(&actor, workers)?,
                    db.computer_extra_calls(),
                ),
                db.oracle_corrections(),
            );
            // Only a model that can see is given pictures; the others are told there were some.
            let sees = config::docs(&db.db, "models")?
                .iter()
                .find(|m| m["id"] == selected.model_id.as_str())
                .is_some_and(|row| crate::model_catalog::supports(row, "vision"));
            let (attachments, unseen) = if sees {
                (taken, 0)
            } else {
                (vec![], taken.len())
            };
            Ok((
                actor,
                policy,
                selected.provider_id,
                selected.model_id,
                attachments,
                unseen,
            ))
        })
        .await??;
    if unseen > 0 {
        prompt.push_str(&format!(
            "\n\n(La persona adjuntó {unseen} imagen(es), pero el modelo actual no puede verlas: dilo y pide que las describa.)"
        ));
    }
    let objective_id = format!("gateway.turn.{}", uuid::Uuid::new_v4());
    let host = app.clone();
    let root_id = objective_id.clone();
    let turn_user = config::string(&actor, "user_id")?.to_owned();
    let store_user = turn_user.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        host.store_handle(&store_user)?
            .open_objective(&root_id, policy)?;
        Ok(())
    })
    .await??;
    let context = RunContext {
        provider_id,
        model_id,
        user: turn_user.clone(),
        actor,
        thread: thread.to_owned(),
        root: None,
        parent: None,
        depth: 0,
        objective_id,
        attachments,
    };
    let root_id = context.objective_id.clone();
    let result = run_engine(
        app.clone(),
        handle,
        provider,
        context,
        prompt,
        output,
        cancel.clone(),
    )
    .await;
    if cancel.is_cancelled() {
        tokio::task::spawn_blocking(move || -> Result<()> {
            app.store_handle(&turn_user)?
                .objective_command(&root_id, r#"{"op":"cancel"}"#)?;
            Ok(())
        })
        .await??;
    }
    result.map(|_| ())
}

/// Apply the administrator's tool policy to one call, before it is claimed. `None` means
/// go ahead. A denial, a refused or unanswered approval, or an unreadable policy all say
/// no, with a reason the model can read. An `ask` is shown to the person whose turn this
/// is (over their WebSocket) and waits for their answer, the turn's cancellation or the
/// timeout, whichever comes first.
/// Record a tool decision in the audit log. Failing to write it never changes the decision.
async fn audit(
    app: &Arc<App>,
    context: &RunContext,
    call: &hive_agent_core::ToolCall,
    decision: &'static str,
    rule: Option<usize>,
    reason: &str,
    source: &'static str,
) {
    let (host, user, thread, tool, arguments, reason) = (
        app.clone(),
        context.user.clone(),
        context.thread.clone(),
        call.name.clone(),
        call.arguments.clone(),
        reason.to_owned(),
    );
    let _ = tokio::task::spawn_blocking(move || {
        host.audit_tool(&config::AuditEntry {
            user: &user,
            thread: &thread,
            tool: &tool,
            arguments: &arguments,
            decision,
            rule,
            reason: &reason,
            source,
        })
    })
    .await;
}

/// Pointer and keyboard actions: the policy's `deny` rules still apply, and otherwise a person
/// approves once per conversation, which then holds for the configured minutes. An `ask` rule is
/// satisfied by that approval too, so nobody is asked for every click.
/// What one kind of tool that needs a once-per-conversation approval says about itself.
struct SessionApproval {
    /// Has this conversation already been approved, and is it still valid?
    live: bool,
    /// What the person is told they are approving.
    reason: String,
    /// Remember the approval for this conversation.
    grant: Box<dyn FnOnce() + Send>,
}

fn session_approval(
    app: &Arc<App>,
    context: &RunContext,
    call: &hive_agent_core::ToolCall,
) -> Option<SessionApproval> {
    let (user, thread) = (context.user.clone(), context.thread.clone());
    if crate::computer::is_input(&call.name) {
        let minutes = app.computer_settings().map_or(10, |s| s.session_minutes);
        let host = app.clone();
        return Some(SessionApproval {
            live: app.computer_grant_live(&user, &thread),
            reason: format!(
                "Control del escritorio: al aprobar, el asistente podrá usar el ratón y el teclado en esta conversación durante {minutes} minutos"
            ),
            grant: Box::new(move || host.computer_grant(&user, &thread)),
        });
    }
    if call.name == crate::ocr::TOOL {
        let settings = app.ocr_settings().ok()?;
        if !settings.ask {
            return None;
        }
        let host = app.clone();
        let file = call.arguments["path"]
            .as_str()
            .unwrap_or("un documento")
            .chars()
            .take(120)
            .collect::<String>();
        return Some(SessionApproval {
            live: app.ocr_grant_live(&user, &thread),
            reason: format!(
                "Lectura de documentos (OCR): al aprobar, «{file}» y los documentos que lea después en esta conversación se enviarán a {} durante {} minutos",
                crate::ocr::provider_label(app),
                settings.session_minutes
            ),
            grant: Box::new(move || host.ocr_grant(&user, &thread)),
        });
    }
    None
}

async fn authorize_session(
    app: &Arc<App>,
    context: &RunContext,
    call: &hive_agent_core::ToolCall,
    output: &mpsc::Sender<Value>,
    cancel: &CancellationToken,
    session: SessionApproval,
) -> Result<Option<String>> {
    let host = app.clone();
    let owner = context.user.clone();
    let (timeout_ms, source, rule) =
        match tokio::task::spawn_blocking(move || host.tool_policy_for(&owner)).await? {
            Ok(None) => (config::DEFAULT_APPROVAL_TIMEOUT_MS, "default", None),
            Ok(Some((stored, source))) => {
                let verdict = stored.policy.evaluate(&call.name, &call.arguments);
                if matches!(verdict.decision, hive_agent_core::policy::Decision::Deny) {
                    audit(
                        app,
                        context,
                        call,
                        "deny",
                        verdict.rule,
                        &verdict.reason,
                        source,
                    )
                    .await;
                    return Ok(Some(format!("POLICY_DENIED: {}", verdict.reason)));
                }
                (stored.approval_timeout_ms, source, verdict.rule)
            }
            Err(_) => {
                let reason =
                    "POLICY_UNAVAILABLE: the tool policy could not be read, so tools are blocked";
                audit(
                    app,
                    context,
                    call,
                    "policy_unreadable",
                    None,
                    reason,
                    "unknown",
                )
                .await;
                return Ok(Some(reason.into()));
            }
        };
    if session.live {
        audit(
            app,
            context,
            call,
            "allow",
            rule,
            "aprobado para esta conversación",
            source,
        )
        .await;
        return Ok(None);
    }
    let outcome = request_approval(
        app,
        context,
        call,
        output,
        cancel,
        &session.reason,
        rule,
        source,
        timeout_ms,
    )
    .await?;
    if outcome.is_none() {
        (session.grant)();
    }
    Ok(outcome)
}

async fn authorize_tool(
    app: &Arc<App>,
    context: &RunContext,
    call: &hive_agent_core::ToolCall,
    output: &mpsc::Sender<Value>,
    cancel: &CancellationToken,
) -> Result<Option<String>> {
    if let Some(session) = session_approval(app, context, call) {
        return authorize_session(app, context, call, output, cancel, session).await;
    }
    let host = app.clone();
    let owner = context.user.clone();
    let (stored, source) =
        match tokio::task::spawn_blocking(move || host.tool_policy_for(&owner)).await? {
            Ok(None) => return Ok(None),
            Ok(Some(found)) => found,
            Err(_) => {
                let reason =
                    "POLICY_UNAVAILABLE: the tool policy could not be read, so tools are blocked";
                audit(
                    app,
                    context,
                    call,
                    "policy_unreadable",
                    None,
                    reason,
                    "unknown",
                )
                .await;
                return Ok(Some(reason.into()));
            }
        };
    let verdict = stored.policy.evaluate(&call.name, &call.arguments);
    match verdict.decision {
        hive_agent_core::policy::Decision::Allow => {
            audit(
                app,
                context,
                call,
                "allow",
                verdict.rule,
                &verdict.reason,
                source,
            )
            .await;
            return Ok(None);
        }
        hive_agent_core::policy::Decision::Deny => {
            audit(
                app,
                context,
                call,
                "deny",
                verdict.rule,
                &verdict.reason,
                source,
            )
            .await;
            return Ok(Some(format!("POLICY_DENIED: {}", verdict.reason)));
        }
        hive_agent_core::policy::Decision::Ask => {}
    }
    return request_approval(
        app,
        context,
        call,
        output,
        cancel,
        &verdict.reason,
        verdict.rule,
        source,
        stored.approval_timeout_ms,
    )
    .await;
}

/// Ask a person whether a tool call may run, in the same chat (WebSocket request plus an A2UI
/// surface), and wait. `Ok(None)` means approved; `Ok(Some(why))` is the refusal the model is told.
#[allow(clippy::too_many_arguments)]
async fn request_approval(
    app: &Arc<App>,
    context: &RunContext,
    call: &hive_agent_core::ToolCall,
    output: &mpsc::Sender<Value>,
    cancel: &CancellationToken,
    reason: &str,
    rule: Option<usize>,
    source: &'static str,
    timeout_ms: u64,
) -> Result<Option<String>> {
    let id = uuid::Uuid::new_v4().to_string();
    let (answer, response) = tokio::sync::oneshot::channel();
    app.approvals
        .lock()
        .map_err(|_| "Aprobaciones bloqueadas")?
        .insert(
            id.clone(),
            config::PendingApproval {
                user: context.user.clone(),
                answer,
            },
        );
    let forget = |app: &App| {
        if let Ok(mut pending) = app.approvals.lock() {
            pending.remove(&id);
        }
    };
    if output
        .send(json!({"type":"approval_request","threadId":context.thread,"approvalId":id,"tool":call.name,
            "arguments":call.arguments,"reason":reason,"expiresInMs":timeout_ms}))
        .await
        .is_err()
    {
        forget(app);
        audit(app, context, call, "ask_unavailable", rule, reason, source).await;
        return Ok(Some("APPROVAL_UNAVAILABLE: nobody is connected to approve this call".into()));
    }
    // The same question inside the chat, as an A2UI surface with two buttons, for clients that
    // draw A2UI (the answer comes back as an `a2ui:action` handled by the gateway). The
    // request above stays for clients that answer it directly.
    for frame in crate::panel::approval_frames(
        &id,
        &context.thread,
        &call.name,
        &call.arguments,
        reason,
        timeout_ms,
    ) {
        let _ = output.send(frame).await;
    }
    let outcome = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            forget(app);
            let _ = output.send(crate::panel::approval_removal(&id, &context.thread)).await;
            audit(app, context, call, "ask_cancelled", rule, reason, source).await;
            return Err("Turno cancelado esperando una aprobación".into());
        }
        answer = tokio::time::timeout(Duration::from_millis(timeout_ms), response) => answer,
    };
    forget(app);
    // Answered, refused or timed out: the question is over, so the surface goes away.
    let _ = output
        .send(crate::panel::approval_removal(&id, &context.thread))
        .await;
    let label = match &outcome {
        Ok(Ok(true)) => "ask_approved",
        Ok(Ok(false)) => "ask_refused",
        _ => "ask_timeout",
    };
    audit(app, context, call, label, rule, reason, source).await;
    Ok(match outcome {
        Ok(Ok(true)) => None,
        Ok(Ok(false)) => Some(format!("POLICY_DENIED: approval refused ({})", reason)),
        _ => Some(format!(
            "APPROVAL_TIMEOUT: nobody approved in time ({})",
            reason
        )),
    })
}

fn run_engine<P: TurnProvider>(
    app: Arc<App>,
    handle: Arc<DurableEngineHandle>,
    provider: P,
    context: RunContext,
    prompt: String,
    output: mpsc::Sender<Value>,
    cancel: CancellationToken,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send>> {
    Box::pin(async move {
        if cancel.is_cancelled() {
            return Err("Turno cancelado antes de iniciar".into());
        }
        let frame = dispatch(
            &handle,
            json!({"op":"begin_run","prompt":if context.attachments.is_empty() { json!(prompt) } else { json!({"text":prompt,"attachments":context.attachments}) },
            "objective_id":context.objective_id,"objective_depth":context.depth}),
        )
        .await?;
        let id = config::string(&frame, "run_id")?.to_owned();
        let progress = Progress {
            app: app.clone(),
            run: id.clone(),
            root: context.root.clone().unwrap_or_else(|| id.clone()),
            thread: context.thread.clone(),
            user: context.user.clone(),
            agent: config::string(&context.actor, "id")?.to_owned(),
            name: context.actor["name"]
                .as_str()
                .unwrap_or("Agente")
                .to_owned(),
            task: prompt.chars().take(160).collect(),
            parent: context.parent.clone(),
        };
        let result = run_loop(
            app,
            handle,
            Arc::new(provider),
            &context,
            &progress,
            frame,
            prompt,
            &output,
            &cancel,
        )
        .await;
        if let Err(error) = &result {
            eprintln!(
                "[Hive Agent] {}",
                json!({"event":"run_failed","agent_id":progress.agent,
                "agent_name":progress.name,"run_id":progress.run,"error":error.to_string()})
            );
            let label = if cancel.is_cancelled() {
                "Ejecución cancelada; requiere revisión antes de reanudar"
            } else {
                "La ejecución necesita revisión"
            };
            if let Ok(event) = progress
                .emit(
                    "failed",
                    "failed",
                    if cancel.is_cancelled() {
                        "aborted"
                    } else {
                        "failed"
                    },
                    "error",
                    label,
                    Some(error.to_string()),
                )
                .await
            {
                let _ = output.send(event).await;
            }
        }
        result
    })
}

#[allow(clippy::too_many_arguments)]
async fn run_loop<P: TurnProvider>(
    app: Arc<App>,
    handle: Arc<DurableEngineHandle>,
    provider: Arc<P>,
    context: &RunContext,
    progress: &Progress,
    mut frame: Value,
    prompt: String,
    output: &mpsc::Sender<Value>,
    cancel: &CancellationToken,
) -> Result<String> {
    output
        .send(
            progress
                .emit(
                    "start",
                    "worker_started",
                    "thinking",
                    "running",
                    &format!("{} está revisando la solicitud", progress.name),
                    None,
                )
                .await?,
        )
        .await?;
    let prompt_for_review: String = prompt.chars().take(2000).collect();
    if context.depth == 0 {
        record(
            app.clone(),
            context.thread.clone(),
            format!("{}:user", progress.run),
            "user",
            prompt,
        )
        .await?;
    }
    // What the tools returned this turn and how often the oracle has sent the answer back: both only
    // matter for the coordinator's final answer (see `crate::oracle`).
    let mut evidence: Vec<Value> = vec![];
    let mut corrections = 0u32;
    let mut draft: Option<String> = None;
    let mut request_text = prompt_for_review;
    loop {
        let action: Action = serde_json::from_value(frame["action"].clone())?;
        match action {
            Action::Model { .. } => {
                if cancel.is_cancelled() {
                    return Err("Turno cancelado; inspecciona la sesión antes de reanudar".into());
                }
                frame = dispatch(&handle, json!({"op":"claim"})).await?;
                // SessionStore reserves the root permit atomically with this claim.
                let Action::Model { request } = serde_json::from_value(frame["action"].clone())?
                else {
                    continue;
                };
                let max_output_tokens = request.max_output_tokens.unwrap_or_default();
                let execution_id = config::string(&frame["operation"], "execution_id")?.to_owned();
                let measured_model = if request.model
                    == config::wire_model(&context.provider_id, &context.model_id)
                {
                    context.model_id.clone()
                } else {
                    request.model.clone()
                };
                let stream = provider.start(request);
                let mut reply = None;
                let mut writing = false;
                loop {
                    let event = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {stream.cancel();return Err("Turno cancelado; la operación queda pendiente de reconciliación explícita".into());}
                        event = stream.next() => event.map_err(|error| format!("{error} (max_output_tokens={})", max_output_tokens))?,
                    };
                    match event {
                        Some(ModelEvent::TextDelta { text }) => {
                            if !writing {
                                writing = true;
                                output
                                    .send(
                                        progress
                                            .emit(
                                                &format!("writing:{}", execution_id),
                                                "worker_started",
                                                "writing",
                                                "running",
                                                &format!(
                                                    "{} está preparando la respuesta",
                                                    progress.name
                                                ),
                                                None,
                                            )
                                            .await?,
                                    )
                                    .await?;
                            }
                            if context.depth == 0 {
                                output.send(json!({"type":"message","threadId":context.thread,"id":progress.run,"content":text,"isChunk":true})).await?;
                            }
                        }
                        Some(ModelEvent::Reply { reply: value }) => {
                            reply = Some(value);
                            break;
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
                let reply = reply.ok_or(
                    "El proveedor cerró sin confirmar la respuesta; requiere reconciliación",
                )?;
                let usage = reply.usage.clone();
                let host = app.clone();
                let identity = (context.provider_id.clone(), measured_model);
                let usage_id = execution_id.clone();
                let measured = usage.clone();
                tokio::task::spawn_blocking(move || {
                    crate::dashboard::record_usage(
                        &host,
                        &usage_id,
                        &identity.0,
                        &identity.1,
                        measured.as_ref(),
                        now(),
                    )
                })
                .await??;
                frame = dispatch(&handle,json!({"op":"advance","execution_id":execution_id,"outcome":{"type":"model","reply":reply}})).await?;
            }
            Action::Tool { call } => {
                if cancel.is_cancelled() {
                    return Err("Turno cancelado antes de ejecutar la herramienta".into());
                }
                if let Some(reason) = authorize_tool(&app, context, &call, output, cancel).await? {
                    // Not run. The operation is claimed and closed in one step so the turn
                    // moves on, and the model is told why like any tool error.
                    frame = dispatch(&handle, json!({"op":"claim"})).await?;
                    let execution_id =
                        config::string(&frame["operation"], "execution_id")?.to_owned();
                    frame = dispatch(
                        &handle,
                        json!({"op":"advance","execution_id":execution_id,
                            "outcome":{"type":"tool","result":{"ok":false,"error":reason}}}),
                    )
                    .await?;
                    continue;
                }
                if call.name.starts_with("a2ui_") && context.depth == 0 {
                    frame = dispatch(&handle, json!({"op":"claim"})).await?;
                    let execution_id =
                        config::string(&frame["operation"], "execution_id")?.to_owned();
                    output
                        .send(
                            progress
                                .emit(
                                    &format!("{}:call", call.id),
                                    "tool_start",
                                    "thinking",
                                    "running",
                                    "Actualizando el Panel interactivo",
                                    Some(call.name.clone()),
                                )
                                .await?,
                        )
                        .await?;
                    let result = crate::panel::execute(
                        &app,
                        &context.user,
                        &context.thread,
                        &context.actor,
                        &call.name,
                        &call.arguments,
                        &execution_id,
                    )
                    .unwrap_or_else(|e| json!({"ok":false,"error":e.to_string()}));
                    output
                        .send(
                            progress
                                .emit(
                                    &format!("{}:result", call.id),
                                    "tool_result",
                                    "thinking",
                                    if result["ok"] == true {
                                        "done"
                                    } else {
                                        "error"
                                    },
                                    if result["ok"] == true {
                                        "Panel interactivo actualizado"
                                    } else {
                                        "No se pudo actualizar el panel"
                                    },
                                    result["error"].as_str().map(str::to_owned),
                                )
                                .await?,
                        )
                        .await?;
                    frame=dispatch(&handle,json!({"op":"advance","execution_id":execution_id,"outcome":{"type":"tool","result":result}})).await?;
                    continue;
                }
                // Reading a document (OCR), for the coordinator only: approved above when the settings
                // ask for it, claimed first and run once.
                if call.name == crate::ocr::TOOL && context.depth == 0 {
                    frame = dispatch(&handle, json!({"op":"claim"})).await?;
                    let execution_id =
                        config::string(&frame["operation"], "execution_id")?.to_owned();
                    output
                        .send(
                            progress
                                .emit(
                                    &format!("{}:call", call.id),
                                    "tool_start",
                                    "thinking",
                                    "running",
                                    "Leyendo un documento",
                                    Some(call.name.clone()),
                                )
                                .await?,
                        )
                        .await?;
                    let result = crate::ocr::execute(&app, call.arguments.clone(), cancel).await;
                    evidence.push(crate::oracle::evidence(&call.name, &result));
                    output
                        .send(
                            progress
                                .emit(
                                    &format!("{}:result", call.id),
                                    "tool_result",
                                    "thinking",
                                    if result["ok"] == true {
                                        "done"
                                    } else {
                                        "error"
                                    },
                                    if result["ok"] == true {
                                        "Documento leído"
                                    } else {
                                        "No se pudo leer el documento"
                                    },
                                    result["error"].as_str().map(str::to_owned),
                                )
                                .await?,
                        )
                        .await?;
                    frame = dispatch(
                        &handle,
                        json!({"op":"advance","execution_id":execution_id,"outcome":{"type":"tool","result":result}}),
                    )
                    .await?;
                    continue;
                }
                // Desktop control, for the coordinator only. Approved above (once per conversation for
                // pointer and keyboard); claimed first and never retried, since a click cannot be undone.
                if call.name.starts_with(crate::computer::PREFIX) && context.depth == 0 {
                    frame = dispatch(&handle, json!({"op":"claim"})).await?;
                    let execution_id =
                        config::string(&frame["operation"], "execution_id")?.to_owned();
                    output
                        .send(
                            progress
                                .emit(
                                    &format!("{}:call", call.id),
                                    "tool_start",
                                    "thinking",
                                    "running",
                                    "Usando el escritorio",
                                    Some(call.name.clone()),
                                )
                                .await?,
                        )
                        .await?;
                    let result =
                        crate::computer::execute(&app, &call.name, call.arguments.clone(), cancel)
                            .await;
                    evidence.push(crate::oracle::evidence(&call.name, &result));
                    output
                        .send(
                            progress
                                .emit(
                                    &format!("{}:result", call.id),
                                    "tool_result",
                                    "thinking",
                                    if result["ok"] == true {
                                        "done"
                                    } else {
                                        "error"
                                    },
                                    if result["ok"] == true {
                                        "Acción de escritorio realizada"
                                    } else {
                                        "La acción de escritorio no se completó"
                                    },
                                    result["error"].as_str().map(str::to_owned),
                                )
                                .await?,
                        )
                        .await?;
                    frame = dispatch(
                        &handle,
                        json!({"op":"advance","execution_id":execution_id,"outcome":{"type":"tool","result":result}}),
                    )
                    .await?;
                    continue;
                }
                // A tool of an MCP server (the policy already had its say above). Claimed first, run once,
                // never retried: its outcome may be unknown, and it may have sent an email.
                if call.name.starts_with(crate::mcp_servers::TOOL_PREFIX)
                    && context.depth == 0
                    && let Some((server, tool)) = crate::mcp_servers::resolve(&app, &call.name)
                {
                    frame = dispatch(&handle, json!({"op":"claim"})).await?;
                    let execution_id =
                        config::string(&frame["operation"], "execution_id")?.to_owned();
                    output
                        .send(
                            progress
                                .emit(
                                    &format!("{}:call", call.id),
                                    "tool_start",
                                    "thinking",
                                    "running",
                                    &format!("Consultando el servidor MCP «{server}»"),
                                    Some(tool.clone()),
                                )
                                .await?,
                        )
                        .await?;
                    let result = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err("Turno cancelado durante una herramienta MCP".into()),
                        result = crate::mcp_servers::execute(&app, &server, &tool, call.arguments.clone()) => result,
                    };
                    evidence.push(crate::oracle::evidence(&call.name, &result));
                    output
                        .send(
                            progress
                                .emit(
                                    &format!("{}:result", call.id),
                                    "tool_result",
                                    "thinking",
                                    if result["ok"] == true {
                                        "done"
                                    } else {
                                        "error"
                                    },
                                    if result["ok"] == true {
                                        "El servidor MCP respondió"
                                    } else {
                                        "El servidor MCP no completó la acción"
                                    },
                                    result["error"].as_str().map(str::to_owned),
                                )
                                .await?,
                        )
                        .await?;
                    frame = dispatch(
                        &handle,
                        json!({"op":"advance","execution_id":execution_id,"outcome":{"type":"tool","result":result}}),
                    )
                    .await?;
                    continue;
                }
                if !matches!(call.name.as_str(), "task_delegate" | "task_delegate_group")
                    || context.depth != 0
                {
                    return Err("Herramienta no disponible en esta integración".into());
                }
                if cancel.is_cancelled() {
                    return Err("Turno cancelado antes de delegar".into());
                }
                frame = dispatch(&handle, json!({"op":"claim"})).await?;
                let execution_id = config::string(&frame["operation"], "execution_id")?.to_owned();
                let tool_name = call.name.clone();
                let result = delegation::execute(
                    app.clone(),
                    provider.clone(),
                    context.clone(),
                    progress.clone(),
                    call,
                    execution_id.clone(),
                    output.clone(),
                    cancel.clone(),
                )
                .await?;
                evidence.push(crate::oracle::evidence(&tool_name, &result));
                frame = dispatch(
                    &handle,
                    json!({"op":"advance","execution_id":execution_id,
                    "outcome":{"type":"tool","result":result}}),
                )
                .await?;
            }
            Action::ToolRejected { call, .. } => {
                output
                    .send(
                        progress
                            .emit(
                                &format!("{}:rejected", call.id),
                                "tool_result",
                                "thinking",
                                "error",
                                "La solicitud a la herramienta necesita corrección",
                                None,
                            )
                            .await?,
                    )
                    .await?;
                frame = dispatch(&handle, json!({"op":"reject"})).await?;
            }
            Action::Complete { content, .. } => {
                dispatch(&handle, json!({"op":"commit"})).await?;
                // The coordinator's answer may be sent back to be corrected before the person sees it.
                if context.depth == 0 && !cancel.is_cancelled() {
                    // The oracle is judged by what the model does with its advice: an unchanged
                    // answer after a correction means the advice was not needed.
                    if let Some(previous) = draft.take()
                        && let Ok(Some((oracle, _))) = app.oracle_client()
                    {
                        if previous.trim() == content.trim() {
                            let _ = oracle
                                .record_overruled("the answer did not change after a correction");
                        } else {
                            let _ = oracle.record_agreement();
                        }
                    }
                    if corrections < app.oracle_corrections() {
                        let verdict = crate::oracle::review(
                            &app,
                            &context.user,
                            &context.objective_id,
                            hive_agent_core::oracle::review::ReviewInput {
                                request: request_text.clone(),
                                answer: content.clone(),
                                evidence: evidence.clone(),
                            },
                            cancel,
                        )
                        .await;
                        if let hive_agent_core::oracle::review::Verdict::Revise { reason } = verdict
                        {
                            corrections += 1;
                            draft = Some(content.clone());
                            let correction =
                                hive_agent_core::oracle::review::correction_prompt(&reason);
                            output
                                .send(
                                    progress
                                        .emit(
                                            &format!("review:{corrections}"),
                                            "tool_result",
                                            "thinking",
                                            "running",
                                            "Revisando la respuesta antes de entregarla",
                                            None,
                                        )
                                        .await?,
                                )
                                .await?;
                            frame = dispatch(
                                &handle,
                                json!({"op":"begin_run","prompt":correction,
                                    "objective_id":context.objective_id,"objective_depth":context.depth}),
                            )
                            .await?;
                            request_text = format!("{request_text}\n(Corrección pedida: {reason})");
                            continue;
                        }
                    }
                }
                if context.depth == 0 {
                    record(
                        app.clone(),
                        context.thread.clone(),
                        format!("{}:assistant", progress.run),
                        "assistant",
                        content.clone(),
                    )
                    .await?;
                }
                output
                    .send(
                        progress
                            .emit(
                                "completed",
                                "group_ready",
                                "completed",
                                "done",
                                &format!(
                                    "{} {}",
                                    progress.name,
                                    if context.depth == 0 {
                                        "terminó su respuesta"
                                    } else {
                                        "respondió la consulta"
                                    }
                                ),
                                None,
                            )
                            .await?,
                    )
                    .await?;
                if context.depth == 0 {
                    output.send(json!({"type":"message","threadId":context.thread,"id":progress.run,"content":content})).await?;
                    output.send(json!({"type":"process","threadId":context.thread,"messageId":progress.run,"processStatus":"done","turnDone":true})).await?;
                }
                return Ok(content);
            }
            Action::Stopped { reason, .. } => {
                dispatch(&handle, json!({"op":"commit"})).await?;
                return Err(format!("SDK detuvo el turno: {reason:?}").into());
            }
            Action::Idle => return Err("El SDK no inició el turno".into()),
        }
    }
}

async fn record(
    app: Arc<App>,
    thread: String,
    id: String,
    role: &'static str,
    content: String,
) -> Result<()> {
    record_tagged(app, thread, id, role, content, "message").await
}
pub(crate) async fn record_tagged(
    app: Arc<App>,
    thread: String,
    id: String,
    role: &'static str,
    content: String,
    source: &'static str,
) -> Result<()> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        let _guard = app.activity_lock.write().map_err(|_|"Actividad bloqueada")?;
        if app.db.col_get("hive_sdk_rust_message_links_v1",&id)?.is_some() { return Ok(()); }
        let at = now();
        let counter_id = format!("conversations:{thread}");
        let counter = app.db.col_get("counters", &counter_id)?;
        let sequence = counter.as_ref().and_then(|c| c.doc["value"].as_u64()).unwrap_or(0).checked_add(1).ok_or("Contador de mensajes agotado")?;
        let storage_id = format!("{thread}:{sequence:015}");
        let mut row = app.db.col_get("conversationThreads",&thread)?.ok_or("Conversación inexistente")?;
        row.doc["message_count"] = json!(row.doc["message_count"].as_u64().unwrap_or(0)+1);
        row.doc["last_message_at"] = json!(at);
        if row.doc["title"].is_null() && role == "user" { row.doc["title"] = json!(content.chars().take(80).collect::<String>()); }
        app.db.col_batch(&[
            crate::dashboard::message_activity(&app,at)?,
            ColOp::Put { collection:"counters".into(),id:counter_id,doc:json!({"value":sequence}),expected_version:Some(counter.map_or(0,|c| c.version)) },
            ColOp::Put { collection:"hive_sdk_rust_message_links_v1".into(),id,doc:json!({"message_id":storage_id,"thread_id":thread,"activity_recorded":true}),expected_version:Some(0) },
            ColOp::Put { collection:"conversationThreads".into(),id:thread.clone(),doc:row.doc,expected_version:Some(row.version) },
            ColOp::Put { collection:"conversations".into(),id:storage_id.clone(),expected_version:Some(0),
                doc:json!({"id":storage_id,"thread_id":thread,"channel":"webchat","role":role,"content":content,
                    "content_multimodal":null,"tool_calls_json":null,"tool_call_id":null,"reasoning_content":null,
                    "source":source,"token_count":content.chars().count().div_ceil(4).max(1),"created_at":at,"updated_at":at}) },
        ])?;
        Ok(())
    }).await??;
    Ok(())
}

async fn send_error(output: &mpsc::Sender<Value>, message: &str) {
    send_thread_error(output, message, None).await;
}
async fn send_thread_error(output: &mpsc::Sender<Value>, message: &str, thread: Option<&str>) {
    let _ = output
        .send(json!({"type":"error","error":message,"threadId":thread}))
        .await;
    let _ = output
        .send(json!({"type":"process","processStatus":"error","turnDone":true,"threadId":thread}))
        .await;
}

#[cfg(test)]
mod tests;

mod delegation;
