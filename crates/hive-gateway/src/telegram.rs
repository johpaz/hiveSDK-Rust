//! Telegram channel: a long-polling bot that answers an allowed list of chats with the
//! user's coordinator agent.
//!
//! Safety rules, because a bot is reachable by anyone who finds it:
//! - **Only chats in `allowed_chat_ids` are ever answered**, and the list is mandatory:
//!   a channel without one does not start. Anything else is dropped without a reply.
//! - **At most once.** The update offset is saved *before* the messages are handled, so a
//!   crash can lose a message but never runs the same user message (and its tools) twice.
//! - The bot token never appears in status, errors or logs; network errors are reported
//!   without their URL.
//! - Turns run as the channel's owner through the same durable driver as the chat, so the
//!   tool policy, budgets and isolation between users apply unchanged. There is nobody to
//!   approve a tool that needs approval, so those are denied when the approval times out.
use crate::{Result, channels, config::App};
use hive_agent_core::provider::CancellationToken;
use hivedb_core::PutOptions;
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc, time::Duration};

const MAX_TEXT_CHARS: usize = 4096;
/// Messages of one chat waiting behind a running turn.
const WORKER_QUEUE: usize = 16;

struct Settings {
    token: String,
    allowed: HashSet<i64>,
    api_base: String,
    poll_seconds: u64,
}

fn parse(raw: &str) -> std::result::Result<Settings, String> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| "la configuración del canal no es JSON".to_owned())?;
    let token = value["bot_token"]
        .as_str()
        .filter(|t| !t.is_empty() && t.len() <= 200 && !t.chars().any(char::is_whitespace))
        .ok_or("falta bot_token")?;
    let allowed: HashSet<i64> = value["allowed_chat_ids"]
        .as_array()
        .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default();
    if allowed.is_empty()
        || value["allowed_chat_ids"]
            .as_array()
            .is_some_and(|ids| ids.len() > 1000)
    {
        return Err(
            "allowed_chat_ids es obligatorio: sin una lista de chats permitidos el bot no arranca"
                .into(),
        );
    }
    let api_base = value["api_base"]
        .as_str()
        .unwrap_or("https://api.telegram.org")
        .trim_end_matches('/')
        .to_owned();
    channels::safe_url(&api_base, "https", "http", false)
        .map_err(|reason| format!("api_base: {reason}"))?;
    let poll_seconds = value["poll_timeout_s"].as_u64().unwrap_or(25);
    if !(1..=50).contains(&poll_seconds) {
        return Err("poll_timeout_s debe estar entre 1 y 50".into());
    }
    Ok(Settings {
        token: token.to_owned(),
        allowed,
        api_base,
        poll_seconds,
    })
}

/// Network failures without their URL (which contains the bot token).
fn network_error(error: reqwest::Error) -> String {
    let error = error.without_url();
    if error.is_timeout() {
        "tiempo de espera agotado".into()
    } else if error.is_connect() {
        "no se pudo conectar con Telegram".into()
    } else {
        "error de red con Telegram".into()
    }
}

async fn call(
    client: &reqwest::Client,
    settings: &Settings,
    method: &str,
    body: Value,
) -> std::result::Result<Value, String> {
    let url = format!("{}/bot{}/{method}", settings.api_base, settings.token);
    let response = client
        .post(url)
        .json(&body)
        .send()
        .await
        .map_err(network_error)?;
    let status = response.status();
    let reply: Value = response
        .json()
        .await
        .map_err(|_| "respuesta de Telegram ilegible".to_owned())?;
    if status.is_success() && reply["ok"] == true {
        return Ok(reply["result"].clone());
    }
    let description = reply["description"].as_str().unwrap_or("error desconocido");
    Err(match status.as_u16() {
        401 => "Telegram rechazó el bot_token".to_owned(),
        409 => "otro proceso está usando este bot (conflicto de getUpdates)".to_owned(),
        code => format!(
            "Telegram respondió {code}: {}",
            description.chars().take(200).collect::<String>()
        ),
    })
}

async fn send(
    client: &reqwest::Client,
    settings: &Settings,
    chat: i64,
    text: &str,
) -> std::result::Result<(), String> {
    for part in channels::chunks(text, MAX_TEXT_CHARS) {
        call(
            client,
            settings,
            "sendMessage",
            json!({"chat_id":chat,"text":part}),
        )
        .await?;
    }
    Ok(())
}

fn load_offset(app: &App, id: &str) -> i64 {
    app.db
        .col_get("channelState", id)
        .ok()
        .flatten()
        .and_then(|row| row.doc["telegram_offset"].as_i64())
        .unwrap_or(0)
}
fn save_offset(app: &App, id: &str, offset: i64) -> Result<()> {
    let version = app
        .db
        .col_get("channelState", id)?
        .map_or(0, |row| row.version);
    app.db.col_put(
        "channelState",
        id,
        &json!({"id":id,"telegram_offset":offset}),
        PutOptions {
            expected_version: Some(version),
        },
    )?;
    Ok(())
}

/// Send `text` on its own initiative to every allowed chat (at most `channels::MAX_BROADCAST`).
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
    for chat in settings.allowed.iter().take(channels::MAX_BROADCAST) {
        sent.record(send(&client, &settings, *chat, text).await);
    }
    sent
}

/// The picture a message carries: its best photo size, or an image sent as a file. Fetched through
/// `getFile`, then from Telegram's file URL (which holds the bot token, so no redirect is followed).
async fn download_photos(
    client: &reqwest::Client,
    settings: &Settings,
    message: &Value,
) -> Vec<Value> {
    let fits = |size: &Value| {
        size.as_u64()
            .is_none_or(|s| s <= channels::MAX_IMAGE_BYTES as u64)
    };
    let mut wanted: Vec<(String, Option<String>)> = vec![];
    // The sizes of one photo come smallest to largest: take the largest that is not too big.
    if let Some(sizes) = message["photo"].as_array()
        && let Some(id) = sizes
            .iter()
            .filter(|p| fits(&p["file_size"]))
            .filter_map(|p| p["file_id"].as_str())
            .next_back()
    {
        wanted.push((id.to_owned(), None));
    }
    let document = &message["document"];
    if document["mime_type"]
        .as_str()
        .is_some_and(|m| channels::IMAGE_TYPES.contains(&m))
        && fits(&document["file_size"])
        && let Some(id) = document["file_id"].as_str()
    {
        wanted.push((
            id.to_owned(),
            document["file_name"].as_str().map(str::to_owned),
        ));
    }
    let mut out = vec![];
    for (file_id, name) in wanted.into_iter().take(channels::MAX_IMAGES) {
        let Ok(info) = call(client, settings, "getFile", json!({"file_id":file_id})).await else {
            continue;
        };
        // A relative path of plain characters: nothing that could leave the file server.
        let Some(path) = info["file_path"].as_str().filter(|p| {
            !p.is_empty()
                && p.len() <= 256
                && !p.starts_with('/')
                && !p.split('/').any(|part| part.is_empty() || part == "..")
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
        }) else {
            continue;
        };
        let url = format!("{}/file/bot{}/{path}", settings.api_base, settings.token);
        if let Some(image) = channels::fetch_image(|c| c.get(url), name.as_deref()).await {
            out.push(image);
        }
    }
    out
}

async fn handle(
    app: Arc<App>,
    client: reqwest::Client,
    settings: Arc<Settings>,
    user: String,
    update: Value,
) {
    let (app, client, settings, user, update) = (&app, &client, &*settings, user.as_str(), &update);
    let message = &update["message"];
    // Words, or the caption of a picture.
    let words = message["text"]
        .as_str()
        .or_else(|| message["caption"].as_str());
    let has_media = message["photo"].is_array() || message["document"].is_object();
    let Some(chat) = message["chat"]["id"].as_i64() else {
        return;
    };
    if words.is_none() && !has_media {
        return;
    }
    if !settings.allowed.contains(&chat) || message["from"]["is_bot"] == true {
        return;
    }
    let mut attachments = vec![];
    if has_media {
        attachments = download_photos(client, settings, message).await;
        if attachments.is_empty() && words.is_none_or(|w| w.trim().is_empty()) {
            let _ = send(client, settings, chat, channels::NOT_AN_IMAGE_TEXT).await;
            return;
        }
    }
    let text = match words.map(str::trim) {
        Some(words) if !words.is_empty() => words,
        _ if !attachments.is_empty() => "Mira la imagen adjunta.",
        _ => "",
    };
    let reply = |text: &str| {
        let text = text.to_owned();
        async move {
            let _ = send(client, settings, chat, &text).await;
        }
    };
    if text.trim().is_empty() || text.chars().count() > MAX_TEXT_CHARS {
        return reply("Envíame un mensaje de texto de hasta 4096 caracteres.").await;
    }
    let title = format!("Telegram {chat}");
    let notify = |question: String| async move {
        let _ = send(client, settings, chat, &question).await;
    };
    let answer = channels::ask_agent_with(
        app,
        user,
        "telegram",
        &chat.to_string(),
        &title,
        text,
        attachments,
        notify,
    )
    .await;
    reply(&channels::reply_text(answer)).await
}

async fn poll(
    app: Arc<App>,
    id: String,
    user: String,
    settings: Settings,
    cancel: CancellationToken,
) {
    let settings = Arc::new(settings);
    let mut workers: std::collections::HashMap<i64, tokio::sync::mpsc::Sender<Value>> =
        std::collections::HashMap::new();
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(settings.poll_seconds + 15))
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            return channels::set_status(
                &app,
                &id,
                "error",
                Some("no se pudo crear el cliente HTTP"),
            );
        }
    };
    let mut offset = load_offset(&app, &id);
    let mut failures = 0u32;
    channels::set_status(&app, &id, "connecting", None);
    while !cancel.is_cancelled() {
        let request =
            json!({"offset":offset,"timeout":settings.poll_seconds,"allowed_updates":["message"]});
        let updates = tokio::select! { _ = cancel.cancelled() => break, result = call(&client, &settings, "getUpdates", request) => result };
        match updates {
            Err(error) => {
                channels::set_status(&app, &id, "error", Some(&error));
                failures = failures.saturating_add(1);
                let wait = Duration::from_secs(2u64.saturating_pow(failures.min(5)).min(30));
                tokio::select! { _ = cancel.cancelled() => break, _ = tokio::time::sleep(wait) => {} }
            }
            Ok(list) => {
                failures = 0;
                channels::set_status(&app, &id, "connected", None);
                let list = list.as_array().cloned().unwrap_or_default();
                if let Some(next) = list
                    .iter()
                    .filter_map(|update| update["update_id"].as_i64())
                    .max()
                    .map(|last| last + 1)
                {
                    // Confirm first: a crash may lose these messages but never runs one twice.
                    let (host, channel) = (app.clone(), id.clone());
                    let saved =
                        tokio::task::spawn_blocking(move || save_offset(&host, &channel, next))
                            .await;
                    if !matches!(saved, Ok(Ok(()))) {
                        channels::set_status(
                            &app,
                            &id,
                            "error",
                            Some("no se pudo guardar el desplazamiento de Telegram"),
                        );
                        tokio::select! { _ = cancel.cancelled() => break, _ = tokio::time::sleep(Duration::from_secs(5)) => {} }
                        continue;
                    }
                    offset = next;
                }
                for update in &list {
                    if cancel.is_cancelled() {
                        break;
                    }
                    let message = &update["message"];
                    let chat = message["chat"]["id"].as_i64();
                    // The answer to an approval this chat was asked is read here, not queued behind
                    // the very turn that is waiting for it.
                    if let (Some(chat), Some(text)) = (chat, message["text"].as_str())
                        && settings.allowed.contains(&chat)
                        && message["from"]["is_bot"] != true
                        && let Some(reply) = channels::answer_pending(
                            &app,
                            &user,
                            "telegram",
                            &chat.to_string(),
                            text,
                        )
                    {
                        let _ = send(&client, &settings, chat, &reply).await;
                        continue;
                    }
                    // One worker per chat: its messages are handled in order, and a turn that is
                    // waiting does not stop the poller from reading the next updates.
                    let queue = workers.entry(chat.unwrap_or(0)).or_insert_with(|| {
                        let (queue, mut inbox) = tokio::sync::mpsc::channel::<Value>(WORKER_QUEUE);
                        let (app, client, settings, user) =
                            (app.clone(), client.clone(), settings.clone(), user.clone());
                        tokio::spawn(async move {
                            while let Some(update) = inbox.recv().await {
                                handle(
                                    app.clone(),
                                    client.clone(),
                                    settings.clone(),
                                    user.clone(),
                                    update,
                                )
                                .await;
                            }
                        });
                        queue
                    });
                    if queue.try_send(update.clone()).is_err()
                        && let Some(chat) = chat
                        && settings.allowed.contains(&chat)
                    {
                        let _ = send(
                            &client,
                            &settings,
                            chat,
                            "Tengo demasiados mensajes tuyos pendientes; espera a que termine.",
                        )
                        .await;
                    }
                }
            }
        }
    }
    channels::set_status(&app, &id, "disconnected", None);
}

/// Supervises the Telegram channels: one poller per enabled channel.
pub(crate) async fn run(app: Arc<App>) {
    channels::supervise(app, "telegram", |app, id, user, raw, cancel| {
        let settings = parse(&raw)?;
        tokio::spawn(poll(app, id, user, settings, cancel));
        Ok(())
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configuration_is_validated_and_a_channel_without_an_allowlist_does_not_start() {
        let ok = parse(r#"{"bot_token":"1:abc","allowed_chat_ids":[42,-100]}"#).unwrap();
        assert_eq!(
            (ok.allowed.len(), ok.api_base.as_str(), ok.poll_seconds),
            (2, "https://api.telegram.org", 25)
        );
        for (config, why) in [
            (r#"{"bot_token":"1:abc"}"#, "no allowlist"),
            (
                r#"{"bot_token":"1:abc","allowed_chat_ids":[]}"#,
                "empty allowlist",
            ),
            (
                r#"{"bot_token":"1:abc","allowed_chat_ids":["42"]}"#,
                "ids must be numbers",
            ),
            (r#"{"allowed_chat_ids":[1]}"#, "no token"),
            (
                r#"{"bot_token":"a b","allowed_chat_ids":[1]}"#,
                "token with spaces",
            ),
            (
                r#"{"bot_token":"1:abc","allowed_chat_ids":[1],"api_base":"http://evil.example"}"#,
                "plain http to a remote host",
            ),
            (
                r#"{"bot_token":"1:abc","allowed_chat_ids":[1],"api_base":"https://user:pw@evil.example"}"#,
                "credentials in the URL",
            ),
            (
                r#"{"bot_token":"1:abc","allowed_chat_ids":[1],"api_base":"ftp://x"}"#,
                "wrong scheme",
            ),
            (
                r#"{"bot_token":"1:abc","allowed_chat_ids":[1],"poll_timeout_s":0}"#,
                "poll 0",
            ),
            (
                r#"{"bot_token":"1:abc","allowed_chat_ids":[1],"poll_timeout_s":99}"#,
                "poll too long",
            ),
            ("not json", "not json"),
        ] {
            assert!(parse(config).is_err(), "{why}");
        }
        assert!(
            parse(
                r#"{"bot_token":"1:abc","allowed_chat_ids":[1],"api_base":"http://127.0.0.1:8080"}"#
            )
            .is_ok(),
            "local http is allowed for tests and bridges"
        );
    }

    #[test]
    fn replies_are_split_into_messages_telegram_accepts_without_losing_text() {
        assert_eq!(channels::chunks("hola", MAX_TEXT_CHARS), ["hola"]);
        assert_eq!(channels::chunks("   ", MAX_TEXT_CHARS), ["(sin respuesta)"]);
        let long = format!("{}\n{}", "a".repeat(3000), "b".repeat(3000));
        let parts = channels::chunks(&long, MAX_TEXT_CHARS);
        assert_eq!(parts.len(), 2, "split at the line break, not mid-word");
        assert!(
            parts
                .iter()
                .all(|part| part.chars().count() <= MAX_TEXT_CHARS)
        );
        assert_eq!(parts.join("\n"), long);
        let unbroken = "é".repeat(9000);
        let parts = channels::chunks(&unbroken, MAX_TEXT_CHARS);
        assert!(
            parts
                .iter()
                .all(|part| part.chars().count() <= MAX_TEXT_CHARS),
            "multibyte text is cut on character boundaries"
        );
        assert_eq!(parts.concat(), unbroken);
        assert_eq!(
            channels::chunks(&"x".repeat(MAX_TEXT_CHARS * 20), MAX_TEXT_CHARS).len(),
            8,
            "an enormous reply is capped"
        );
    }
}
