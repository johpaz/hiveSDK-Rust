//! What every chat channel (Telegram, Discord, Slack) shares: one supervisor that starts,
//! restarts and stops an adapter as its `channels` row and secret change, a live status,
//! the conversation a chat maps to, and how a message reaches the agent.
//!
//! The rules are the same for all of them, and they are the ones that matter for a bot
//! anybody can message: only an explicit allowlist is ever answered, a message is handled
//! at most once, secrets never reach statuses, errors or the chat, and the turn runs as
//! the channel's owner through the durable driver (budget, user isolation, tool policy).
use crate::{
    Result,
    config::{self, App},
    server,
};
use hive_agent_core::provider::CancellationToken;
use hivedb_core::PutOptions;
use serde_json::json;
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    sync::Arc,
    time::Duration,
};

pub(crate) const BUSY_TEXT: &str = "Sigo trabajando en tu mensaje anterior; espera a que termine.";
pub(crate) const LIMITED_TEXT: &str =
    "Estás enviando mensajes muy rápido; espera un momento antes de escribir de nuevo.";
pub(crate) const FAILED_TEXT: &str = "No pude completar la respuesta. Inténtalo de nuevo.";
pub(crate) const TURN_TIMEOUT: Duration = Duration::from_secs(900);
const MAX_REPLY_CHUNKS: usize = 8;
/// Most destinations one channel is written to by a single proactive delivery.
pub(crate) const MAX_BROADCAST: usize = 50;
/// The channel types that can receive a task's result.
pub(crate) const DELIVERABLE: [&str; 5] = [
    "telegram",
    "discord",
    "slack",
    "whatsapp",
    crate::whatsapp_qr::KIND,
];

pub(crate) fn set_status(app: &App, id: &str, status: &str, error: Option<&str>) {
    if let Ok(mut all) = app.channel_status.lock() {
        all.insert(id.to_owned(), json!({"status":status,"error":error}));
    }
}

/// Split a reply into messages of at most `max` characters, preferring line breaks and never
/// cutting a character; at most 8 parts, so an enormous reply cannot flood a chat.
pub(crate) fn chunks(text: &str, max: usize) -> Vec<String> {
    let mut out = vec![];
    let mut rest = text.trim();
    while !rest.is_empty() && out.len() < MAX_REPLY_CHUNKS {
        if rest.chars().count() <= max {
            out.push(rest.to_owned());
            break;
        }
        let limit = rest
            .char_indices()
            .nth(max)
            .map_or(rest.len(), |(at, _)| at);
        let cut = rest[..limit]
            .rfind('\n')
            .filter(|at| *at > limit / 2)
            .unwrap_or(limit);
        out.push(rest[..cut].trim_end().to_owned());
        rest = rest[cut..].trim_start();
    }
    if out.is_empty() {
        out.push("(sin respuesta)".into());
    }
    out
}

/// How a message left the agent.
pub(crate) enum Answer {
    Text(String),
    Busy,
    Failed,
    /// The chat is over its rate limit; the message was not run.
    Limited,
}

/// The meaning of a reply to an approval question: `Some(true)` for yes, `Some(false)` for no,
/// `None` for anything else. Only a plain yes or no counts, never a sentence that happens to
/// contain one: what the person writes decides, so it has to be unambiguous.
pub(crate) fn verdict(text: &str) -> Option<bool> {
    let word: String = text
        .trim()
        .trim_end_matches(['.', '!', '¡', ',', ' '])
        .to_lowercase();
    match word.as_str() {
        "sí" | "si" | "s" | "yes" | "y" | "ok" | "aprobar" | "apruebo" | "aprobado" => Some(true),
        "no" | "n" | "denegar" | "deniego" | "denegado" | "rechazar" | "cancelar" => Some(false),
        _ => None,
    }
}

const MAX_APPROVAL_ARGS: usize = 400;

/// What the person is told when a tool is waiting for them: what, why, with what, and how to answer.
pub(crate) fn approval_text(request: &serde_json::Value) -> String {
    let tool = request["tool"].as_str().unwrap_or("una herramienta");
    let reason = request["reason"].as_str().unwrap_or_default().trim();
    let arguments = &request["arguments"];
    let shown = if arguments.is_null() {
        String::new()
    } else {
        let pretty = serde_json::to_string_pretty(arguments).unwrap_or_default();
        match pretty.char_indices().nth(MAX_APPROVAL_ARGS) {
            Some((at, _)) => format!("{}…", &pretty[..at]),
            None => pretty,
        }
    };
    let seconds = request["expiresInMs"]
        .as_u64()
        .unwrap_or(120_000)
        .div_ceil(1000);
    let mut text = format!("Hive quiere ejecutar «{tool}».");
    if !reason.is_empty() {
        text.push_str(&format!("\n{reason}"));
    }
    if !shown.is_empty() {
        text.push_str(&format!("\n\n{shown}"));
    }
    text.push_str(&format!(
        "\n\nResponde «sí» para aprobarla o «no» para denegarla. Si no respondes en {seconds} s se denegará."
    ));
    text
}

/// If this chat has a question waiting, its next message is the answer: a plain yes or no
/// resolves it here (the model never sees it), anything else is a reminder. `None` means no
/// question is waiting, so the message is an ordinary one.
pub(crate) fn answer_pending(
    app: &Arc<App>,
    user: &str,
    kind: &str,
    peer: &str,
    text: &str,
) -> Option<String> {
    let key = format!("{kind}:{peer}");
    let id = app.chat_approvals.lock().ok()?.get(&key).cloned()?;
    // A question whose time ran out is gone: the message is an ordinary one again.
    if !app.approvals.lock().ok()?.contains_key(&id) {
        app.chat_approvals.lock().ok()?.remove(&key);
        return None;
    }
    let Some(approved) = verdict(text) else {
        return Some("Hay una acción esperando tu respuesta: escribe «sí» para aprobarla o «no» para denegarla.".into());
    };
    Some(decide(app, user, &key, &id, approved))
}

/// Resolve the approval `id` that `key` (`kind:peer`) was asked, and forget the question.
fn decide(app: &Arc<App>, user: &str, key: &str, id: &str, approved: bool) -> String {
    if let Ok(mut pending) = app.chat_approvals.lock() {
        pending.remove(key);
    }
    let principal = config::Principal {
        user: user.to_owned(),
        admin: false,
    };
    if !app
        .resolve_approval(id, &principal, approved)
        .unwrap_or(false)
    {
        "Ya no hay nada pendiente de aprobar.".into()
    } else if approved {
        "Aprobado. Continúo.".into()
    } else {
        "Denegado.".into()
    }
}

/// The answer to an approval question as a button press (Slack): valid only for the question that
/// chat was asked, so an old or foreign button decides nothing. `None` when it does not match.
pub(crate) fn answer_button(
    app: &Arc<App>,
    user: &str,
    kind: &str,
    peer: &str,
    approval_id: &str,
    approved: bool,
) -> Option<String> {
    let key = format!("{kind}:{peer}");
    let pending = app.chat_approvals.lock().ok()?.get(&key).cloned()?;
    if pending != approval_id || !app.approvals.lock().ok()?.contains_key(&pending) {
        return None;
    }
    Some(decide(app, user, &key, approval_id, approved))
}

/// The approval id this chat is being asked about right now, if any.
pub(crate) fn pending_approval(app: &Arc<App>, kind: &str, peer: &str) -> Option<String> {
    app.chat_approvals
        .lock()
        .ok()?
        .get(&format!("{kind}:{peer}"))
        .cloned()
}

/// Run one chat message as a turn of the channel owner's conversation `{user}/{kind}/{peer}`
/// and wait for the answer. The model's or the gateway's error text is for the operator, so
/// a failure is only ever reported as `Failed`.
///
/// When a policy asks for an approval during the turn, `notify` sends the question to the
/// chat, and that chat's next plain "sí" or "no" answers it (see [`answer_pending`]); a
/// channel has nobody on a WebSocket to ask.
#[cfg_attr(not(any(test, feature = "whatsapp-qr")), allow(dead_code))]
pub(crate) async fn ask_agent<N, F>(
    app: &Arc<App>,
    user: &str,
    kind: &str,
    peer: &str,
    title: &str,
    text: &str,
    notify: N,
) -> Answer
where
    N: Fn(String) -> F,
    F: Future<Output = ()>,
{
    ask_agent_with(app, user, kind, peer, title, text, vec![], notify).await
}

/// Like [`ask_agent`], with pictures that came with the message (attachment objects, inline
/// base64). They go to the model only if it can see; otherwise the model is told they existed.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn ask_agent_with<N, F>(
    app: &Arc<App>,
    user: &str,
    kind: &str,
    peer: &str,
    title: &str,
    text: &str,
    attachments: Vec<serde_json::Value>,
    notify: N,
) -> Answer
where
    N: Fn(String) -> F,
    F: Future<Output = ()>,
{
    // The answer to a question this chat was asked is not a new turn.
    if let Some(reply) = answer_pending(app, user, kind, peer, text) {
        return Answer::Text(reply);
    }
    // Before anything is created or paid for.
    if app
        .limit(crate::ratelimit::Kind::Channel, &format!("{kind}:{peer}"))
        .is_err()
    {
        return Answer::Limited;
    }
    let thread = format!("{user}/{kind}/{peer}");
    let prepare = {
        let app = app.clone();
        let (thread, user, kind, peer, title) = (
            thread.clone(),
            user.to_owned(),
            kind.to_owned(),
            peer.to_owned(),
            title.to_owned(),
        );
        move || -> Result<()> {
            if app.db.col_get("conversationThreads", &thread)?.is_none() {
                let now = server::now();
                app.db.col_put("conversationThreads", &thread, &json!({"id":thread,"user_id":user,"channel":kind,"title":title,"peer_id":peer,"peer_kind":"direct","archived":false,"created_at":now,"last_message_at":now,"message_count":0}), PutOptions { expected_version: Some(0) })?;
            }
            Ok(())
        }
    };
    if tokio::task::spawn_blocking(prepare)
        .await
        .map(|done| done.is_err())
        .unwrap_or(true)
    {
        return Answer::Failed;
    }
    if !attachments.is_empty()
        && let Ok(mut waiting) = app.turn_attachments.lock()
    {
        waiting.insert(thread.clone(), attachments);
    }
    let (delivery, mut results) = tokio::sync::mpsc::channel(8);
    if let Err(error) = server::start_live_work(
        app.clone(),
        user.to_owned(),
        thread,
        text.to_owned(),
        delivery,
        true,
    ) {
        // The turn that would have taken them did not start.
        if let Ok(mut waiting) = app.turn_attachments.lock() {
            waiting.remove(&format!("{user}/{kind}/{peer}"));
        }
        return if error.to_string().contains("turno activo") {
            Answer::Busy
        } else {
            Answer::Failed
        };
    }
    let key = format!("{kind}:{peer}");
    let deadline = tokio::time::Instant::now() + TURN_TIMEOUT;
    let answer = loop {
        match tokio::time::timeout_at(deadline, results.recv()).await {
            Ok(Some(event)) if event["type"] == "approval_request" => {
                if let Some(id) = event["approvalId"].as_str()
                    && let Ok(mut pending) = app.chat_approvals.lock()
                {
                    pending.insert(key.clone(), id.to_owned());
                }
                notify(approval_text(&event)).await;
            }
            Ok(Some(result)) if result["ok"] == true => {
                break Answer::Text(result["content"].as_str().unwrap_or_default().to_owned());
            }
            Ok(Some(result)) if result["type"] == "live_result" => break Answer::Failed,
            Ok(Some(_)) => {}
            _ => break Answer::Failed,
        }
    };
    if let Ok(mut pending) = app.chat_approvals.lock() {
        pending.remove(&key);
    }
    answer
}

/// The text to send back for an answer.
pub(crate) fn reply_text(answer: Answer) -> String {
    match answer {
        Answer::Text(text) => text,
        Answer::Busy => BUSY_TEXT.into(),
        Answer::Failed => FAILED_TEXT.into(),
        Answer::Limited => LIMITED_TEXT.into(),
    }
}

/// Is this URL one an adapter may talk to? `secure` (https or wss) anywhere; `plain`
/// (http or ws) only to the local machine, for bridges and tests. Never credentials, and
/// a query only where the protocol needs one (`allow_query`).
pub(crate) fn safe_url(
    raw: &str,
    secure: &str,
    plain: &str,
    allow_query: bool,
) -> std::result::Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw).map_err(|_| "no es una URL".to_owned())?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    let scheme_ok = url.scheme() == secure || (url.scheme() == plain && local);
    let query_ok = allow_query || url.query().is_none();
    if !scheme_ok
        || !query_ok
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(format!(
            "la URL debe ser {secure}:// (o {plain}:// solo en local), sin credenciales{}",
            if allow_query { "" } else { " ni parámetros" }
        ));
    }
    Ok(url)
}

struct Running {
    fingerprint: String,
    cancel: CancellationToken,
}

/// Starts, restarts and stops one adapter per enabled channel of `kind`. `start` gets the
/// channel id, its owner, the raw secret configuration and a cancellation token, spawns the
/// adapter, and returns an error (shown as the channel's status) if the configuration is
/// unusable. `channel_wakeup` makes a change take effect now.
pub(crate) async fn supervise<S>(app: Arc<App>, kind: &'static str, start: S)
where
    S: Fn(Arc<App>, String, String, String, CancellationToken) -> std::result::Result<(), String>
        + Send
        + Sync
        + 'static,
{
    let mut running: HashMap<String, Running> = HashMap::new();
    loop {
        let host = app.clone();
        let desired = tokio::task::spawn_blocking(
            move || -> Result<Vec<(String, String, Option<String>)>> {
                let owner = host.owner_id()?;
                let mut out = vec![];
                for row in config::docs(&host.db, "channels")? {
                    if row["type"] != kind || row["enabled"] != true || row["active"] != true {
                        continue;
                    }
                    let id = config::string(&row, "id")?.to_owned();
                    let user = row["user_id"]
                        .as_str()
                        .filter(|u| !u.is_empty())
                        .map_or(owner.clone(), str::to_owned);
                    out.push((
                        id.clone(),
                        user,
                        config::secret(&host.db, &host.home, &format!("channel:{id}:config"))?,
                    ));
                }
                Ok(out)
            },
        )
        .await;
        let desired = desired
            .ok()
            .and_then(|result| result.ok())
            .unwrap_or_default();
        let wanted: HashSet<&str> = desired.iter().map(|(id, _, _)| id.as_str()).collect();
        running.retain(|id, entry| {
            let keep = wanted.contains(id.as_str());
            if !keep {
                entry.cancel.cancel();
            }
            keep
        });
        for (id, user, raw) in desired {
            let Some(raw) = raw else {
                set_status(&app, &id, "error", Some("el canal no tiene configuración"));
                continue;
            };
            let fingerprint = config::App::hash_token(&format!("{user}\n{raw}"));
            if running
                .get(&id)
                .is_some_and(|entry| entry.fingerprint == fingerprint)
            {
                continue;
            }
            if let Some(old) = running.remove(&id) {
                old.cancel.cancel();
            }
            let cancel = app.shutdown.child_token();
            match start(app.clone(), id.clone(), user, raw, cancel.clone()) {
                Err(reason) => set_status(&app, &id, "error", Some(&reason)),
                Ok(()) => {
                    running.insert(
                        id,
                        Running {
                            fingerprint,
                            cancel,
                        },
                    );
                }
            }
        }
        tokio::select! {
            _ = app.shutdown.cancelled() => break,
            _ = app.channel_wakeup.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }
    for entry in running.values() {
        entry.cancel.cancel();
    }
}

/// Wait `wait` unless cancelled; `true` means cancelled.
pub(crate) async fn pause(cancel: &CancellationToken, wait: Duration) -> bool {
    tokio::select! { _ = cancel.cancelled() => true, _ = tokio::time::sleep(wait) => false }
}

/// Run `future` unless cancelled first.
pub(crate) async fn unless_cancelled<T>(
    cancel: &CancellationToken,
    future: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! { _ = cancel.cancelled() => None, value = future => Some(value) }
}

/// What one channel managed to send on its own initiative.
#[derive(Default)]
pub(crate) struct Sent {
    pub destinations: usize,
    pub error: Option<String>,
}
impl Sent {
    pub(crate) fn failed(reason: String) -> Self {
        Self {
            destinations: 0,
            error: Some(reason),
        }
    }
    pub(crate) fn record(&mut self, outcome: std::result::Result<(), String>) {
        match outcome {
            Ok(()) => self.destinations += 1,
            Err(error) => {
                self.error.get_or_insert(error);
            }
        }
    }
}

/// Send `text` to the active channels of `user`: every one (`target` is `"all"`) or only those
/// of one type (`"telegram"`, `"discord"`, `"slack"`, `"whatsapp"`, `"whatsapp_qr"`). A channel is used when its
/// row is enabled and active and its configuration is usable; the text goes to each of its
/// allowed chats. A channel that fails does not stop the others, nothing is retried (a retry
/// could deliver twice), and the report says what was sent and what failed, without secrets.
pub(crate) async fn deliver(
    app: &Arc<App>,
    user: &str,
    target: &str,
    text: &str,
) -> serde_json::Value {
    let host = app.clone();
    let (owner, wanted) = (user.to_owned(), target.to_owned());
    let found =
        tokio::task::spawn_blocking(move || -> Result<Vec<(String, String, Option<String>)>> {
            let first = host.owner_id()?;
            let mut out = vec![];
            for row in config::docs(&host.db, "channels")? {
                let kind = row["type"].as_str().unwrap_or_default();
                if !DELIVERABLE.contains(&kind)
                    || row["enabled"] != true
                    || row["active"] != true
                    || (wanted != "all" && wanted != kind)
                {
                    continue;
                }
                if row["user_id"]
                    .as_str()
                    .filter(|u| !u.is_empty())
                    .unwrap_or(&first)
                    != owner
                {
                    continue;
                }
                let id = config::string(&row, "id")?.to_owned();
                out.push((
                    id.clone(),
                    kind.to_owned(),
                    config::secret(&host.db, &host.home, &format!("channel:{id}:config"))?,
                ));
            }
            Ok(out)
        })
        .await
        .ok()
        .and_then(|result| result.ok())
        .unwrap_or_default();
    let mut sent = vec![];
    let mut failed = vec![];
    if found.is_empty() {
        failed.push(
            json!({"channel":null,"type":target,"error":"no hay canales activos de este tipo"}),
        );
    }
    for (id, kind, raw) in found {
        let outcome = match raw {
            None => Sent::failed("el canal no tiene configuración".into()),
            Some(raw) => match kind.as_str() {
                "telegram" => crate::telegram::broadcast(&raw, text).await,
                "discord" => crate::discord::broadcast(&raw, text).await,
                "slack" => crate::slack::broadcast(&raw, text).await,
                "whatsapp_qr" => crate::whatsapp_qr::broadcast(&id, &raw, text).await,
                _ => crate::whatsapp::broadcast(&raw, text).await,
            },
        };
        if outcome.destinations > 0 {
            sent.push(json!({"channel":id,"type":kind,"destinations":outcome.destinations}));
        }
        if let Some(error) = outcome.error {
            failed.push(json!({"channel":id,"type":kind,"error":error}));
        }
    }
    json!({"target":target,"sent":sent,"failed":failed})
}

/// Record which message ids are new for a channel, persisted under `channelState/{key}`: done under
/// the activity lock, so two concurrent deliveries or a restart cannot both run the same message.
/// If the ids cannot be recorded nothing is returned (at most once beats at least once).
pub(crate) fn first_time(app: &App, key: &str, ids: Vec<String>, remembered: usize) -> Vec<String> {
    let Ok(_guard) = app.activity_lock.write() else {
        return vec![];
    };
    let existing = app.db.col_get("channelState", key).ok().flatten();
    let mut seen: Vec<String> = existing
        .as_ref()
        .and_then(|row| row.doc["seen"].as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let mut fresh: Vec<String> = vec![];
    for candidate in ids {
        if !seen.contains(&candidate) && !fresh.contains(&candidate) {
            fresh.push(candidate);
        }
    }
    if fresh.is_empty() {
        return fresh;
    }
    seen.extend(fresh.iter().cloned());
    let excess = seen.len().saturating_sub(remembered);
    seen.drain(..excess);
    let saved = app.db.col_put(
        "channelState",
        key,
        &json!({"id":key,"seen":seen}),
        PutOptions {
            expected_version: Some(existing.map_or(0, |row| row.version)),
        },
    );
    if saved.is_ok() { fresh } else { vec![] }
}

/// Pictures a message may carry into a turn: how many, how big, and which types. The bytes decide
/// the type, never the name or what the sender declared.
pub(crate) const MAX_IMAGES: usize = 4;
pub(crate) const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
pub(crate) const IMAGE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
pub(crate) const NOT_AN_IMAGE_TEXT: &str =
    "Solo puedo leer imágenes (png, jpeg, gif o webp) de hasta 5 MB; ese archivo no lo pude usar.";

/// May a credential be sent to this URL to fetch a file? Only over https to a host in `suffixes`
/// (`slack.com` allows `files.slack.com`), or to the same origin as `api_base` (a bridge or a test).
/// Never to credentials-in-URL or fragments, and never somewhere a message could point.
pub(crate) fn trusted_file_url(
    raw: &str,
    suffixes: &[&str],
    api_base: &str,
) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(raw).ok()?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return None;
    }
    let host = url.host_str()?;
    let listed = url.scheme() == "https"
        && suffixes
            .iter()
            .any(|s| host == *s || host.ends_with(&format!(".{s}")));
    let api = reqwest::Url::parse(api_base).ok()?;
    let same_origin = url.scheme() == api.scheme()
        && url.host_str() == api.host_str()
        && url.port_or_known_default() == api.port_or_known_default();
    (listed || same_origin).then_some(url)
}

/// Download one picture and return it as an attachment object, or `None` if anything is off: a
/// failed request, a redirect, more than [`MAX_IMAGE_BYTES`], or bytes that are not a real
/// png/jpeg/gif/webp (a login page is HTML). `request` carries the URL and any credential.
pub(crate) async fn fetch_image(
    request: impl FnOnce(&reqwest::Client) -> reqwest::RequestBuilder,
    name: Option<&str>,
) -> Option<serde_json::Value> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .ok()?;
    let mut response = request(&client).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut bytes: Vec<u8> = vec![];
    while let Ok(Some(chunk)) = response.chunk().await {
        if bytes.len() + chunk.len() > MAX_IMAGE_BYTES {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    let real = hive_agent_core::image::detect_mime(&bytes).filter(|m| IMAGE_TYPES.contains(m))?;
    let mut attachment = json!({"kind":"image","media_type":real,"data":STANDARD.encode(&bytes)});
    if let Some(name) =
        name.filter(|n| !n.is_empty() && n.len() <= 128 && !n.chars().any(char::is_control))
    {
        attachment["name"] = json!(name);
    }
    Some(attachment)
}
