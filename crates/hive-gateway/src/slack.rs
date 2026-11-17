//! Slack channel in Socket Mode: the app opens an outbound WebSocket (no public webhook
//! to expose), receives events, and answers an allowlist of channels or DMs with the
//! owner's coordinator agent. The shared rules of [`crate::channels`] apply.
//!
//! Every event envelope is **acknowledged before it is handled**, and event ids already
//! seen are dropped, so Slack redelivering an envelope never runs a user message twice.
//! Only plain messages from people are answered: bots (including this one), edits,
//! deletions and every other subtype are ignored. Replies cannot ping anyone: Slack's
//! `<!channel>`, `<!here>`, `<@user>` and `<#channel>` markers are defused before sending.
use crate::{
    channels,
    config::{self, App},
};
use futures_util::{SinkExt, StreamExt};
use hive_agent_core::provider::CancellationToken;
use http::Request;
use ring::hmac;
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};
use tokio_tungstenite::tungstenite::Message;

const MAX_MESSAGE_CHARS: usize = 3500;
const DEFAULT_API: &str = "https://slack.com/api";
const REMEMBERED_EVENTS: usize = 500;

struct Settings {
    bot_token: String,
    /// Socket Mode only; empty in webhook mode.
    app_token: String,
    /// The Events API over HTTP instead of Socket Mode: Slack calls `/webhook/slack/{id}`, signed
    /// with this secret.
    signing_secret: Option<String>,
    allowed: HashSet<String>,
    api_base: String,
}

impl Settings {
    fn webhook(&self) -> bool {
        self.signing_secret.is_some()
    }
}

fn parse(raw: &str) -> std::result::Result<Settings, String> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| "la configuración del canal no es JSON".to_owned())?;
    let token = |key: &str| {
        value[key]
            .as_str()
            .filter(|t| !t.is_empty() && t.len() <= 300 && !t.chars().any(char::is_whitespace))
            .map(str::to_owned)
    };
    let webhook = match value["mode"].as_str() {
        None | Some("socket") => false,
        Some("webhook") => true,
        Some(_) => return Err("mode debe ser socket o webhook".into()),
    };
    let (bot_token, app_token, signing_secret) = if webhook {
        let secret = value["signing_secret"]
            .as_str()
            .filter(|t| (8..=128).contains(&t.len()) && !t.chars().any(char::is_whitespace))
            .map(str::to_owned);
        match (token("bot_token"), secret) {
            (Some(bot), Some(secret)) => (bot, String::new(), Some(secret)),
            _ => return Err("el modo webhook necesita bot_token (xoxb-…) y signing_secret".into()),
        }
    } else {
        match (token("bot_token"), token("app_token")) {
            (Some(bot), Some(app)) => (bot, app, None),
            _ => return Err("faltan bot_token (xoxb-…) y app_token (xapp-…)".into()),
        }
    };
    let allowed: HashSet<String> = value["allowed_channel_ids"]
        .as_array()
        .map(|ids| {
            ids.iter()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .filter(|id| {
                    !id.is_empty()
                        && id.len() <= 32
                        && id.bytes().all(|b| b.is_ascii_alphanumeric())
                })
                .collect()
        })
        .unwrap_or_default();
    if allowed.is_empty()
        || value["allowed_channel_ids"]
            .as_array()
            .is_some_and(|ids| ids.len() > 1000)
    {
        return Err("allowed_channel_ids es obligatorio (ids como C0123 o D0123): sin una lista de canales permitidos el bot no arranca".into());
    }
    let api_base = value["api_base"]
        .as_str()
        .unwrap_or(DEFAULT_API)
        .trim_end_matches('/')
        .to_owned();
    channels::safe_url(&api_base, "https", "http", false)
        .map_err(|reason| format!("api_base: {reason}"))?;
    Ok(Settings {
        bot_token,
        app_token,
        signing_secret,
        allowed,
        api_base,
    })
}

/// Slack's special markers turn text into notifications; put a zero-width space inside them.
fn defuse(text: &str) -> String {
    text.replace("<!", "<\u{200b}!")
        .replace("<@", "<\u{200b}@")
        .replace("<#", "<\u{200b}#")
}

fn network_error(error: reqwest::Error) -> String {
    let error = error.without_url();
    if error.is_timeout() {
        "tiempo de espera agotado".into()
    } else {
        "error de red con Slack".into()
    }
}

/// Call a Web API method. `Ok` is the parsed body of an `ok: true` answer.
async fn api(
    client: &reqwest::Client,
    settings: &Settings,
    token: &str,
    method: &str,
    body: Value,
) -> std::result::Result<Value, String> {
    for attempt in 0..2 {
        let response = client
            .post(format!("{}/{method}", settings.api_base))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .map_err(network_error)?;
        if response.status().as_u16() == 429 && attempt == 0 {
            let wait = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(1)
                .clamp(1, 5);
            tokio::time::sleep(Duration::from_secs(wait)).await;
            continue;
        }
        let status = response.status();
        let reply: Value = response
            .json()
            .await
            .map_err(|_| "respuesta de Slack ilegible".to_owned())?;
        if status.is_success() && reply["ok"] == true {
            return Ok(reply);
        }
        return Err(reply["error"]
            .as_str()
            .unwrap_or("error desconocido")
            .chars()
            .take(80)
            .collect());
    }
    Err("Slack limitó las peticiones".into())
}

/// A Slack timestamp (`1700000000.000100`): the only thing allowed as a thread id.
fn valid_ts(ts: &str) -> bool {
    (1..=32).contains(&ts.len()) && ts.chars().all(|c| c.is_ascii_digit() || c == '.')
}

async fn send(
    client: &reqwest::Client,
    settings: &Settings,
    channel: &str,
    text: &str,
) -> std::result::Result<(), String> {
    send_in(client, settings, channel, None, text).await
}

/// Like [`send`], inside the thread `thread` when there is one.
async fn send_in(
    client: &reqwest::Client,
    settings: &Settings,
    channel: &str,
    thread: Option<&str>,
    text: &str,
) -> std::result::Result<(), String> {
    for part in channels::chunks(&defuse(text), MAX_MESSAGE_CHARS) {
        let mut body = json!({"channel":channel,"text":part,"link_names":false,"unfurl_links":false,"unfurl_media":false});
        if let Some(thread) = thread.filter(|ts| valid_ts(ts)) {
            body["thread_ts"] = json!(thread);
        }
        api(
            client,
            settings,
            &settings.bot_token,
            "chat.postMessage",
            body,
        )
        .await?;
    }
    Ok(())
}

/// The approval question with its two buttons (Block Kit). The text is plain text, never
/// markup, and the buttons carry only the approval id.
async fn send_approval_buttons(
    client: &reqwest::Client,
    settings: &Settings,
    channel: &str,
    thread: Option<&str>,
    text: &str,
    approval_id: &str,
) -> std::result::Result<(), String> {
    let shown: String = defuse(text).chars().take(2900).collect();
    let mut body = json!({"channel":channel,"text":shown,"link_names":false,"unfurl_links":false,"unfurl_media":false,
    "blocks":[
        {"type":"section","text":{"type":"plain_text","text":shown}},
        {"type":"actions","elements":[
            {"type":"button","style":"primary","action_id":"hive_approve","value":approval_id,"text":{"type":"plain_text","text":"Aprobar"}},
            {"type":"button","style":"danger","action_id":"hive_deny","value":approval_id,"text":{"type":"plain_text","text":"Denegar"}}
        ]}]});
    if let Some(thread) = thread.filter(|ts| valid_ts(ts)) {
        body["thread_ts"] = json!(thread);
    }
    api(
        client,
        settings,
        &settings.bot_token,
        "chat.postMessage",
        body,
    )
    .await?;
    Ok(())
}

/// A `response_url` is where Slack wants the answer to a slash command or a button. Only Slack's own
/// host (or this machine, for tests) is ever posted to.
fn response_url(raw: &str) -> Option<reqwest::Url> {
    let url = channels::safe_url(raw, "https", "http", true).ok()?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    (local || url.host_str() == Some("hooks.slack.com")).then_some(url)
}

async fn respond(client: &reqwest::Client, url: &reqwest::Url, body: Value) {
    let _ = client.post(url.clone()).json(&body).send().await;
}

/// Send `text` on its own initiative to every allowed channel (at most `channels::MAX_BROADCAST`).
pub(crate) async fn broadcast(raw: &str, text: &str) -> channels::Sent {
    let settings = match parse(raw) {
        Ok(settings) => settings,
        Err(reason) => return channels::Sent::failed(reason),
    };
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
    else {
        return channels::Sent::failed("no se pudo crear el cliente HTTP".into());
    };
    let mut sent = channels::Sent::default();
    for channel in settings.allowed.iter().take(channels::MAX_BROADCAST) {
        sent.record(send(&client, &settings, channel, text).await);
    }
    sent
}

/// Everything an event handler needs; cheap to clone.
#[derive(Clone)]
struct Ctx {
    app: Arc<App>,
    client: reqwest::Client,
    settings: Arc<Settings>,
    user: String,
}

const MAX_FILES: usize = channels::MAX_IMAGES;

/// Where the bot's token may be sent to fetch a file: Slack's own hosts over https, or the
/// configured API host itself (a bridge or a test).
fn file_url_ok(settings: &Settings, raw: &str) -> Option<reqwest::Url> {
    channels::trusted_file_url(raw, &["slack.com"], &settings.api_base)
}

/// The pictures among a message's files, downloaded and checked: type, size and that the bytes
/// really are that image. Anything else is skipped, never guessed at.
async fn download_images(ctx: &Ctx, files: &[Value]) -> Vec<Value> {
    let mut out = vec![];
    for file in files.iter().take(MAX_FILES) {
        let declared = file["mimetype"].as_str().unwrap_or_default();
        if !channels::IMAGE_TYPES.contains(&declared)
            || file["size"]
                .as_u64()
                .is_some_and(|size| size > channels::MAX_IMAGE_BYTES as u64)
        {
            continue;
        }
        let Some(url) = file["url_private"]
            .as_str()
            .and_then(|raw| file_url_ok(&ctx.settings, raw))
        else {
            continue;
        };
        let token = ctx.settings.bot_token.clone();
        if let Some(image) = channels::fetch_image(
            |client| client.get(url).bearer_auth(token),
            file["name"].as_str(),
        )
        .await
        {
            out.push(image);
        }
    }
    out
}

/// One message (or slash command) of an allowed channel, answered in `thread` when it came from one.
/// `command` is the `response_url` of a slash command, which gets the answer instead of a message.
/// `files` are what the message carried: pictures go to the model, which sees them if it can.
async fn handle_message(
    ctx: Ctx,
    channel: String,
    thread: Option<String>,
    text: String,
    files: Vec<Value>,
    command: Option<reqwest::Url>,
) {
    let Ctx {
        app,
        client,
        settings,
        user,
    } = ctx.clone();
    let mut text = text;
    let mut attachments = vec![];
    if !files.is_empty() {
        attachments = download_images(&ctx, &files).await;
        if attachments.is_empty() && text.is_empty() {
            let _ = send_in(
                &client,
                &settings,
                &channel,
                thread.as_deref(),
                channels::NOT_AN_IMAGE_TEXT,
            )
            .await;
            return;
        }
        if text.is_empty() {
            text = "Mira la imagen adjunta.".into();
        }
    }
    let title = format!("Slack {channel}");
    let (app_ref, client_ref, settings_ref) = (&app, &client, &settings);
    let (channel_ref, thread_ref) = (&channel, thread.as_deref());
    // A question about a tool carries Aprobar / Denegar buttons; the typed "sí"/"no" still works.
    let notify = |question: String| async move {
        match channels::pending_approval(app_ref, "slack", channel_ref) {
            Some(id) => {
                let _ = send_approval_buttons(
                    client_ref,
                    settings_ref,
                    channel_ref,
                    thread_ref,
                    &question,
                    &id,
                )
                .await;
            }
            None => {
                let _ = send_in(client_ref, settings_ref, channel_ref, thread_ref, &question).await;
            }
        }
    };
    let answer = channels::ask_agent_with(
        &app,
        &user,
        "slack",
        &channel,
        &title,
        &text,
        attachments,
        notify,
    )
    .await;
    let reply = channels::reply_text(answer);
    match &command {
        Some(url) => {
            for part in channels::chunks(&defuse(&reply), MAX_MESSAGE_CHARS) {
                respond(
                    &client,
                    url,
                    json!({"response_type":"in_channel","text":part}),
                )
                .await;
            }
        }
        None => {
            let _ = send_in(&client, &settings, &channel, thread.as_deref(), &reply).await;
        }
    }
}

/// A `message` event: only plain messages from people (a file share is one), in an allowed channel.
fn on_event(ctx: &Ctx, event: &Value) {
    let Some(channel) = event["channel"].as_str() else {
        return;
    };
    let text = event["text"].as_str().unwrap_or_default().trim();
    let files: Vec<Value> = event["files"]
        .as_array()
        .map(|files| files.iter().take(MAX_FILES).cloned().collect())
        .unwrap_or_default();
    if event["type"] != "message"
        || event["subtype"]
            .as_str()
            .is_some_and(|kind| kind != "file_share")
        || event["bot_id"].is_string()
        || !event["user"].is_string()
        || !ctx.settings.allowed.contains(channel)
        || (text.is_empty() && files.is_empty())
        || text.chars().count() > 4000
    {
        return;
    }
    // Answered in the thread the person wrote in.
    let thread = event["thread_ts"]
        .as_str()
        .filter(|ts| valid_ts(ts))
        .map(str::to_owned);
    tokio::spawn(handle_message(
        ctx.clone(),
        channel.to_owned(),
        thread,
        text.to_owned(),
        files,
        None,
    ));
}

/// `/hive <question>`: answered through its `response_url`. Like every message, only allowed
/// channels, and a stranger is not answered at all.
fn on_slash(ctx: &Ctx, payload: &Value) {
    let (Some(channel), Some(url)) = (
        payload["channel_id"].as_str(),
        payload["response_url"].as_str().and_then(response_url),
    ) else {
        return;
    };
    if !ctx.settings.allowed.contains(channel) {
        return;
    }
    let text = payload["text"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if text.is_empty() {
        let client = ctx.client.clone();
        tokio::spawn(async move {
            respond(&client, &url, json!({"response_type":"ephemeral","text":"Escribe tu pregunta después del comando, por ejemplo: /hive ¿qué puedes hacer?"})).await;
        });
        return;
    }
    if text.chars().count() > 4000 {
        return;
    }
    tokio::spawn(handle_message(
        ctx.clone(),
        channel.to_owned(),
        None,
        text,
        vec![],
        Some(url),
    ));
}

/// A press of Aprobar / Denegar on an approval question.
fn on_interactive(ctx: &Ctx, payload: &Value) {
    let (Some(channel), Some(action)) = (
        payload["channel"]["id"].as_str(),
        payload["actions"]
            .as_array()
            .and_then(|actions| actions.first()),
    ) else {
        return;
    };
    let approved = match action["action_id"].as_str() {
        Some("hive_approve") => true,
        Some("hive_deny") => false,
        _ => return,
    };
    if payload["type"] != "block_actions" || !ctx.settings.allowed.contains(channel) {
        return;
    }
    let Some(id) = action["value"].as_str() else {
        return;
    };
    // Only the question this channel was asked: an old or foreign button decides nothing.
    let Some(result) = channels::answer_button(&ctx.app, &ctx.user, "slack", channel, id, approved)
    else {
        return;
    };
    // The message loses its buttons so it cannot be pressed twice.
    if let Some(url) = payload["response_url"].as_str().and_then(response_url) {
        let client = ctx.client.clone();
        tokio::spawn(async move {
            respond(
                &client,
                &url,
                json!({"replace_original":true,"text":result}),
            )
            .await;
        });
    }
}

enum Ending {
    Stop,
    Reconnect(String),
    /// A refused token cannot be fixed by retrying; the configuration must change.
    Fatal(String),
}

fn fatal_error(code: &str) -> bool {
    matches!(
        code,
        "invalid_auth"
            | "not_authed"
            | "token_revoked"
            | "token_expired"
            | "account_inactive"
            | "missing_scope"
            | "invalid_app_token"
            | "not_allowed_token_type"
    )
}

async fn session(
    app: &Arc<App>,
    id: &str,
    user: &str,
    settings: &Arc<Settings>,
    client: &reqwest::Client,
    cancel: &CancellationToken,
    seen: &mut VecDeque<String>,
) -> Ending {
    let opened = match channels::unless_cancelled(
        cancel,
        api(
            client,
            settings,
            &settings.app_token,
            "apps.connections.open",
            json!({}),
        ),
    )
    .await
    {
        None => return Ending::Stop,
        Some(Err(code)) if fatal_error(&code) => {
            return Ending::Fatal(format!("Slack rechazó el token ({code})"));
        }
        Some(Err(error)) => return Ending::Reconnect(error),
        Some(Ok(body)) => body,
    };
    let Some(url) = opened["url"]
        .as_str()
        .filter(|url| channels::safe_url(url, "wss", "ws", true).is_ok())
    else {
        return Ending::Reconnect("Slack no entregó una dirección de conexión válida".into());
    };
    let connect = tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::connect_async(url),
    );
    let Some(connected) = channels::unless_cancelled(cancel, connect).await else {
        return Ending::Stop;
    };
    let Ok(Ok((socket, _))) = connected else {
        return Ending::Reconnect("no se pudo conectar con Slack".into());
    };
    let (mut sink, mut stream) = socket.split();
    let ctx = Ctx {
        app: app.clone(),
        client: client.clone(),
        settings: settings.clone(),
        user: user.to_owned(),
    };
    loop {
        let frame = tokio::select! {
            _ = cancel.cancelled() => { let _ = sink.close().await; return Ending::Stop; }
            frame = stream.next() => frame,
        };
        let text = match frame {
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(Message::Ping(payload))) => {
                let _ = sink.send(Message::Pong(payload)).await;
                continue;
            }
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                return Ending::Reconnect("se perdió la conexión".into());
            }
            Some(Ok(_)) => continue,
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        // Acknowledge first, always: an unacknowledged envelope is delivered again.
        if let Some(envelope) = value["envelope_id"].as_str()
            && sink
                .send(Message::Text(json!({"envelope_id":envelope}).to_string()))
                .await
                .is_err()
        {
            return Ending::Reconnect("se perdió la conexión".into());
        }
        match value["type"].as_str() {
            Some("hello") => channels::set_status(app, id, "connected", None),
            Some("disconnect") => return Ending::Reconnect("Slack pidió reconectar".into()),
            Some("events_api") => {
                let event_id = value["payload"]["event_id"].as_str().unwrap_or_default();
                if !event_id.is_empty() {
                    if seen.contains(&event_id.to_owned()) {
                        continue;
                    }
                    seen.push_back(event_id.to_owned());
                    if seen.len() > REMEMBERED_EVENTS {
                        seen.pop_front();
                    }
                }
                on_event(&ctx, &value["payload"]["event"]);
            }
            Some("slash_commands") => {
                let envelope = value["envelope_id"].as_str().unwrap_or_default();
                if !envelope.is_empty() {
                    let key = format!("slash:{envelope}");
                    if seen.contains(&key) {
                        continue;
                    }
                    seen.push_back(key);
                    if seen.len() > REMEMBERED_EVENTS {
                        seen.pop_front();
                    }
                }
                on_slash(&ctx, &value["payload"]);
            }
            Some("interactive") => on_interactive(&ctx, &value["payload"]),
            _ => {}
        }
    }
}

async fn run_channel(
    app: Arc<App>,
    id: String,
    user: String,
    settings: Settings,
    cancel: CancellationToken,
) {
    let settings = Arc::new(settings);
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
    else {
        return channels::set_status(&app, &id, "error", Some("no se pudo crear el cliente HTTP"));
    };
    channels::set_status(&app, &id, "connecting", None);
    let mut seen = VecDeque::new();
    let mut failures = 0u32;
    while !cancel.is_cancelled() {
        match session(&app, &id, &user, &settings, &client, &cancel, &mut seen).await {
            Ending::Stop => break,
            Ending::Fatal(reason) => {
                channels::set_status(&app, &id, "error", Some(&reason));
                return;
            }
            Ending::Reconnect(reason) => {
                failures = failures.saturating_add(1);
                channels::set_status(&app, &id, "error", Some(&reason));
                if channels::pause(
                    &cancel,
                    Duration::from_secs(2u64.saturating_pow(failures.min(5)).min(30)),
                )
                .await
                {
                    break;
                }
            }
        }
    }
    channels::set_status(&app, &id, "disconnected", None);
}

pub(crate) async fn run(app: Arc<App>) {
    channels::supervise(app, "slack", |app, id, user, raw, cancel| {
        let settings = parse(&raw)?;
        if settings.webhook() {
            // Nothing to keep connected: Slack calls `/webhook/slack/{id}`.
            channels::set_status(&app, &id, "listening", None);
            return Ok(());
        }
        tokio::spawn(run_channel(app, id, user, settings, cancel));
        Ok(())
    })
    .await;
}

/// How far a webhook's timestamp may be from now; older ones are replays.
const MAX_SKEW_SECS: i64 = 300;
const REMEMBERED_WEBHOOK_EVENTS: usize = 200;

/// Slack's signature: `v0=` and the hex HMAC-SHA256 of `v0:{timestamp}:{body}`, checked in constant time.
fn signed(
    secret: &str,
    timestamp: Option<&str>,
    body: &[u8],
    header: Option<&str>,
    now: i64,
) -> bool {
    let (Some(timestamp), Some(sent)) = (
        timestamp,
        header
            .and_then(|h| h.strip_prefix("v0="))
            .and_then(|hex| hex::decode(hex).ok()),
    ) else {
        return false;
    };
    let Ok(at) = timestamp.parse::<i64>() else {
        return false;
    };
    if (now - at).abs() > MAX_SKEW_SECS {
        return false;
    }
    let mut signed = format!("v0:{timestamp}:").into_bytes();
    signed.extend_from_slice(body);
    hmac::verify(
        &hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes()),
        &signed,
        &sent,
    )
    .is_ok()
}

fn webhook_channel(app: &Arc<App>, id: &str) -> Option<Ctx> {
    let row = app.db.col_get("channels", id).ok().flatten()?.doc;
    if row["type"] != "slack" || row["enabled"] != true || row["active"] != true {
        return None;
    }
    let owner = app.owner_id().ok()?;
    let user = row["user_id"]
        .as_str()
        .filter(|u| !u.is_empty())
        .map_or(owner, str::to_owned);
    let raw = config::secret(&app.db, &app.home, &format!("channel:{id}:config"))
        .ok()
        .flatten()?;
    let settings = parse(&raw).ok().filter(Settings::webhook)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .ok()?;
    Some(Ctx {
        app: app.clone(),
        client,
        settings: Arc::new(settings),
        user,
    })
}

/// `(status, body)` for a request to `/webhook/slack/{id}`: the Events API (JSON), slash commands
/// and interactivity (form-encoded). Public, so it is protected like WhatsApp's: every request must
/// carry a valid signature, or it is a 401 and is never parsed; an unknown or misconfigured channel
/// is a plain 404; event ids are recorded before anything runs, so a redelivery does nothing.
pub(crate) async fn handle(app: Arc<App>, request: Request<Vec<u8>>) -> (u16, Value) {
    let not_found = (404, json!({"error":"Canal desconocido"}));
    let Some(id) = request
        .uri()
        .path()
        .strip_prefix("/webhook/slack/")
        .filter(|id| !id.is_empty() && !id.contains('/'))
        .map(str::to_owned)
    else {
        return not_found;
    };
    // Counted before anything is read or verified: the route is public.
    if let Err(wait) = app.limit(crate::ratelimit::Kind::Webhook, &id) {
        return (
            429,
            json!({"error":"Demasiadas peticiones","retryAfterMs":wait}),
        );
    }
    let host = app.clone();
    let lookup = id.clone();
    let Ok(Some(ctx)) = tokio::task::spawn_blocking(move || webhook_channel(&host, &lookup)).await
    else {
        return not_found;
    };
    if request.method() != "POST" {
        return (405, json!({"error":"Método no permitido"}));
    }
    let header = |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let secret = ctx.settings.signing_secret.clone().unwrap_or_default();
    if !signed(
        &secret,
        header("x-slack-request-timestamp"),
        request.body(),
        header("x-slack-signature"),
        now,
    ) {
        return (401, json!({"error":"Firma inválida"}));
    }
    let form =
        header("content-type").is_some_and(|t| t.starts_with("application/x-www-form-urlencoded"));
    if form {
        let Ok(text) = std::str::from_utf8(request.body()) else {
            return (400, json!({"error":"Cuerpo inválido"}));
        };
        let fields = crate::server::query(text).unwrap_or_default();
        if let Some(payload) = fields.get("payload") {
            if let Ok(payload) = serde_json::from_str::<Value>(payload) {
                on_interactive(&ctx, &payload);
            }
            return (200, json!({}));
        }
        // A slash command: acknowledged at once and answered through its response_url.
        let payload = json!({"channel_id":fields.get("channel_id"),"response_url":fields.get("response_url"),"text":fields.get("text")});
        on_slash(&ctx, &payload);
        return (200, json!({}));
    }
    let Ok(payload) = serde_json::from_slice::<Value>(request.body()) else {
        return (400, json!({"error":"Cuerpo inválido"}));
    };
    match payload["type"].as_str() {
        Some("url_verification") => {
            match payload["challenge"].as_str().filter(|c| c.len() <= 200) {
                Some(challenge) => (200, json!({"challenge":challenge})),
                None => (400, json!({"error":"Desafío inválido"})),
            }
        }
        Some("event_callback") => {
            let event_id = payload["event_id"].as_str().unwrap_or_default().to_owned();
            // An event without an id cannot be told apart from its redelivery, so it is not run.
            if event_id.is_empty() || event_id.len() > 64 {
                return (200, json!({"ok":true}));
            }
            let host = app.clone();
            let key = format!("slack:{id}");
            let fresh = tokio::task::spawn_blocking(move || {
                channels::first_time(&host, &key, vec![event_id], REMEMBERED_WEBHOOK_EVENTS)
            })
            .await
            .unwrap_or_default();
            if !fresh.is_empty() {
                on_event(&ctx, &payload["event"]);
            }
            (200, json!({"ok":true}))
        }
        _ => (200, json!({"ok":true})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configuration_is_validated_and_a_channel_without_an_allowlist_does_not_start() {
        let ok = parse(r#"{"bot_token":"xoxb-1","app_token":"xapp-1","allowed_channel_ids":["D0123","C0456"]}"#).unwrap();
        assert_eq!((ok.allowed.len(), ok.api_base.as_str()), (2, DEFAULT_API));
        for (config, why) in [
            (
                r#"{"bot_token":"xoxb-1","app_token":"xapp-1"}"#,
                "no allowlist",
            ),
            (
                r#"{"bot_token":"xoxb-1","app_token":"xapp-1","allowed_channel_ids":[]}"#,
                "empty allowlist",
            ),
            (
                r#"{"bot_token":"xoxb-1","allowed_channel_ids":["D1"]}"#,
                "no app token",
            ),
            (
                r#"{"app_token":"xapp-1","allowed_channel_ids":["D1"]}"#,
                "no bot token",
            ),
            (
                r#"{"bot_token":"x y","app_token":"xapp-1","allowed_channel_ids":["D1"]}"#,
                "token with spaces",
            ),
            (
                r#"{"bot_token":"xoxb-1","app_token":"xapp-1","allowed_channel_ids":["D-1"]}"#,
                "invalid id",
            ),
            (
                r#"{"bot_token":"xoxb-1","app_token":"xapp-1","allowed_channel_ids":["D1"],"api_base":"http://evil.example"}"#,
                "plain http to a remote host",
            ),
            (
                r#"{"bot_token":"xoxb-1","app_token":"xapp-1","allowed_channel_ids":["D1"],"api_base":"https://u:p@slack.com"}"#,
                "credentials",
            ),
            ("null", "not an object"),
        ] {
            assert!(parse(config).is_err(), "{why}");
        }
    }

    #[test]
    fn markers_that_notify_people_are_defused_and_ordinary_text_is_untouched() {
        let text = defuse(
            "hola <!channel> y <!here> con <@U123> en <#C9|general> y <https://x.example|enlace>",
        );
        assert!(
            !text.contains("<!channel>")
                && !text.contains("<!here>")
                && !text.contains("<@U123>")
                && !text.contains("<#C9")
        );
        assert!(
            text.contains("<https://x.example|enlace>"),
            "links still work"
        );
        assert_eq!(defuse("texto normal ñ"), "texto normal ñ");
    }

    #[test]
    fn only_authentication_failures_are_fatal() {
        for code in [
            "invalid_auth",
            "token_revoked",
            "account_inactive",
            "not_allowed_token_type",
        ] {
            assert!(fatal_error(code), "{code}");
        }
        for code in [
            "ratelimited",
            "internal_error",
            "service_unavailable",
            "fatal_error",
        ] {
            assert!(!fatal_error(code), "{code}");
        }
    }
}
