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
            if !check.authorized(token.as_deref())? {
                return Ok(None);
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
    let body = value.to_string();
    let reason = StatusCode::from_u16(code)?
        .canonical_reason()
        .unwrap_or("Error");
    let headers = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
        body.len()
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        socket.write_all(headers.as_bytes()).await?;
        socket.write_all(body.as_bytes()).await?;
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
            match app.selection() {
                Ok(_) => json!({"configured":true}),
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
        for user in config::docs(&app.db, "users")? {
            if user["email"] == email
                && !email.is_empty()
                && let Some(hash) = user["password_hash"].as_str()
                && config::verify_password(password, hash)?
            {
                return Ok((200, json!({"authToken":app.token})));
            }
        }
        return Ok((401, json!({"error":"Correo o contraseña incorrectos"})));
    }
    let token = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    if !app.authorized(token)? {
        return Ok((401, json!({"error":"Acceso requerido"})));
    }
    if let Some(response) = crate::integrations::route(
        app,
        method,
        path,
        req.uri().query().unwrap_or_default(),
        req.body(),
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
    )? {
        return Ok(response);
    }
    if let Some(response) = crate::realtime::route(app, method, path, req.body())? {
        return Ok(response);
    }
    if method == "GET"
        && let Some(response) =
            crate::conversations::route(app, path, &query(req.uri().query().unwrap_or(""))?)?
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
        let users = config::docs(&app.db, "users")?;
        let user = config::string(users.first().ok_or("Sin usuario")?, "id")?;
        return Ok((
            200,
            if path == "/api/agents" {
                json!({"agents":activity::agents(&app.db,user)?})
            } else {
                let _guard = app
                    .activity_lock
                    .lock()
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
        let selected = app.selection()?;
        let peer = uuid::Uuid::new_v4().to_string();
        let id = format!("{}/webchat/{peer}", selected.user);
        let body: Value = serde_json::from_slice(req.body())?;
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
        return Ok((
            201,
            json!({"conversation":crate::conversations::wire(&row)}),
        ));
    }
    Ok((
        501,
        json!({"error":"Función no disponible en esta primera integración Rust","capabilities":CAPABILITIES}),
    ))
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
                    match crate::panel::action(&app,&user,&value["data"]) {Ok((thread,prompt))=>value=json!({"type":"message","threadId":thread,"content":prompt}),Err(e)=>{send_error(&output,&e.to_string()).await;continue;}}
                }
                match value["type"].as_str() {
                    Some("message") => {
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
) -> Result<String> {
    if !crate::conversations::owned(&app, &thread, &user)? {
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
                    Some(frame)=>if frame["type"]=="message" && frame["isChunk"]!=true {answer=frame["content"].as_str().unwrap_or_default().to_owned();},
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
        let selected = host.selection()?;
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
}

async fn drive_turn<P: TurnProvider>(
    app: Arc<App>,
    handle: Arc<DurableEngineHandle>,
    provider: P,
    thread: &str,
    prompt: String,
    output: mpsc::Sender<Value>,
    cancel: CancellationToken,
) -> Result<()> {
    let db = app.clone();
    let (actor, policy, provider_id, model_id) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            let selected = db.selection()?;
            let user = &selected.user;
            let agents = config::docs(&db.db, "agents")?;
            let actor = selected.agent;
            let workers = agents
                .iter()
                .filter(|agent| config::enabled_worker(agent, user))
                .count();
            let policy = config::turn_policy(&actor, workers)?;
            Ok((actor, policy, selected.provider_id, selected.model_id))
        })
        .await??;
    let objective_id = format!("gateway.turn.{}", uuid::Uuid::new_v4());
    let host = app.clone();
    let root_id = objective_id.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let guard = host.store.lock().map_err(|_| "Store bloqueado")?;
        guard
            .as_ref()
            .ok_or("Store cerrado")?
            .open_objective(&root_id, policy)?;
        Ok(())
    })
    .await??;
    let context = RunContext {
        provider_id,
        model_id,
        user: config::string(&actor, "user_id")?.to_owned(),
        actor,
        thread: thread.to_owned(),
        root: None,
        parent: None,
        depth: 0,
        objective_id,
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
            let guard = app.store.lock().map_err(|_| "Store bloqueado")?;
            guard
                .as_ref()
                .ok_or("Store cerrado")?
                .objective_command(&root_id, r#"{"op":"cancel"}"#)?;
            Ok(())
        })
        .await??;
    }
    result.map(|_| ())
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
            json!({"op":"begin_run","prompt":prompt,
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
        let _guard = app.activity_lock.lock().map_err(|_|"Actividad bloqueada")?;
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
