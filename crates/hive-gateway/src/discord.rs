//! Discord channel: a bot on the Gateway WebSocket that answers an allowlist of channels
//! (server channels and DMs) with the owner's coordinator agent. The shared rules of
//! [`crate::channels`] apply: an allowlist is mandatory, a message is handled at most once
//! (the connection does not resume, so messages sent while it is down are lost, never
//! replayed), the token never reaches statuses or errors, and the chat only ever receives
//! a generic apology when something fails.
//!
//! Replies carry `allowed_mentions: {parse: []}`, so nothing the model writes can ping
//! `@everyone`, a role or a user. The privileged *Message Content* intent must be enabled
//! for the bot in the Discord developer portal; without it Discord sends empty content in
//! server channels and the bot has nothing to answer.
use crate::{channels, config::App};
use futures_util::{SinkExt, StreamExt};
use hive_agent_core::provider::CancellationToken;
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tokio_tungstenite::tungstenite::Message;

const MAX_MESSAGE_CHARS: usize = 2000;
/// GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT.
const INTENTS: u64 = (1 << 9) | (1 << 12) | (1 << 15);
const DEFAULT_GATEWAY: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
const DEFAULT_API: &str = "https://discord.com/api/v10";

struct Settings {
    token: String,
    allowed: HashSet<String>,
    gateway_url: String,
    api_base: String,
}

fn parse(raw: &str) -> std::result::Result<Settings, String> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| "la configuración del canal no es JSON".to_owned())?;
    let token = value["bot_token"]
        .as_str()
        .filter(|t| !t.is_empty() && t.len() <= 200 && !t.chars().any(char::is_whitespace))
        .ok_or("falta bot_token")?;
    let allowed: HashSet<String> = value["allowed_channel_ids"]
        .as_array()
        .map(|ids| {
            ids.iter()
                .filter_map(|id| id.as_str().map(str::to_owned))
                .filter(|id| {
                    !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_digit())
                })
                .collect()
        })
        .unwrap_or_default();
    if allowed.is_empty()
        || value["allowed_channel_ids"]
            .as_array()
            .is_some_and(|ids| ids.len() > 1000)
    {
        return Err("allowed_channel_ids es obligatorio (ids de canal como texto): sin una lista de canales permitidos el bot no arranca".into());
    }
    let gateway_url = value["gateway_url"]
        .as_str()
        .unwrap_or(DEFAULT_GATEWAY)
        .to_owned();
    channels::safe_url(&gateway_url, "wss", "ws", true)
        .map_err(|reason| format!("gateway_url: {reason}"))?;
    let api_base = value["api_base"]
        .as_str()
        .unwrap_or(DEFAULT_API)
        .trim_end_matches('/')
        .to_owned();
    channels::safe_url(&api_base, "https", "http", false)
        .map_err(|reason| format!("api_base: {reason}"))?;
    Ok(Settings {
        token: token.to_owned(),
        allowed,
        gateway_url,
        api_base,
    })
}

fn network_error(error: reqwest::Error) -> String {
    let error = error.without_url();
    if error.is_timeout() {
        "tiempo de espera agotado".into()
    } else {
        "error de red con Discord".into()
    }
}

/// Post a reply. A 429 is waited out once (up to 5 s); nothing else is retried.
async fn send(
    client: &reqwest::Client,
    settings: &Settings,
    channel: &str,
    text: &str,
) -> std::result::Result<(), String> {
    for part in channels::chunks(text, MAX_MESSAGE_CHARS) {
        for attempt in 0..2 {
            let response = client
                .post(format!("{}/channels/{channel}/messages", settings.api_base))
                .header("Authorization", format!("Bot {}", settings.token))
                .json(&json!({"content":part,"allowed_mentions":{"parse":[]}}))
                .send()
                .await
                .map_err(network_error)?;
            if response.status().as_u16() == 429 && attempt == 0 {
                let wait = response
                    .json::<Value>()
                    .await
                    .ok()
                    .and_then(|body| body["retry_after"].as_f64())
                    .unwrap_or(1.0)
                    .clamp(0.1, 5.0);
                tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                continue;
            }
            if !response.status().is_success() {
                return Err(format!("Discord respondió {}", response.status().as_u16()));
            }
            break;
        }
    }
    Ok(())
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

/// The pictures attached to a message. Discord's files are public (no credential is sent), but they
/// are still only fetched from Discord's own hosts over https (or the configured API host).
async fn download_attachments(settings: &Settings, files: &[Value]) -> Vec<Value> {
    let mut out = vec![];
    for file in files.iter().take(channels::MAX_IMAGES) {
        let declared = file["content_type"].as_str().unwrap_or_default();
        if !channels::IMAGE_TYPES.contains(&declared)
            || file["size"]
                .as_u64()
                .is_some_and(|size| size > channels::MAX_IMAGE_BYTES as u64)
        {
            continue;
        }
        let Some(url) = file["url"].as_str().and_then(|raw| {
            channels::trusted_file_url(
                raw,
                &["discordapp.com", "discordapp.net", "discord.com"],
                &settings.api_base,
            )
        }) else {
            continue;
        };
        if let Some(image) =
            channels::fetch_image(|client| client.get(url), file["filename"].as_str()).await
        {
            out.push(image);
        }
    }
    out
}

async fn handle_message(
    app: Arc<App>,
    client: reqwest::Client,
    settings: Arc<Settings>,
    user: String,
    channel: String,
    text: String,
    files: Vec<Value>,
) {
    let mut text = text;
    let mut attachments = vec![];
    if !files.is_empty() {
        attachments = download_attachments(&settings, &files).await;
        if attachments.is_empty() && text.is_empty() {
            let _ = send(&client, &settings, &channel, channels::NOT_AN_IMAGE_TEXT).await;
            return;
        }
        if text.is_empty() {
            text = "Mira la imagen adjunta.".into();
        }
    }
    let title = format!("Discord {channel}");
    let (client_ref, settings_ref, channel_ref) = (&client, &settings, &channel);
    let notify = |question: String| async move {
        let _ = send(client_ref, settings_ref, channel_ref, &question).await;
    };
    let answer = channels::ask_agent_with(
        &app,
        &user,
        "discord",
        &channel,
        &title,
        &text,
        attachments,
        notify,
    )
    .await;
    let _ = send(&client, &settings, &channel, &channels::reply_text(answer)).await;
}

enum Ending {
    Stop,
    /// Connect again after a pause (network loss, a server request, a missed heartbeat).
    Reconnect(String),
    /// Retrying cannot help until the configuration changes (bad token, intents refused).
    Fatal(String),
}

/// One connection: Hello → Identify → heartbeats and events, until it ends.
async fn session(
    app: &Arc<App>,
    id: &str,
    user: &str,
    settings: &Arc<Settings>,
    client: &reqwest::Client,
    cancel: &CancellationToken,
) -> Ending {
    let connect = tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::connect_async(settings.gateway_url.as_str()),
    );
    let Some(connected) = channels::unless_cancelled(cancel, connect).await else {
        return Ending::Stop;
    };
    let Ok(Ok((socket, _))) = connected else {
        return Ending::Reconnect("no se pudo conectar con la pasarela de Discord".into());
    };
    let (mut sink, mut stream) = socket.split();
    let mut sequence: Option<i64> = None;
    let mut heartbeat: Option<tokio::time::Interval> = None;
    let mut acknowledged = true;
    let mut bot_id = String::new();
    loop {
        let tick = async {
            match heartbeat.as_mut() {
                Some(timer) => {
                    timer.tick().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = cancel.cancelled() => { let _ = sink.close().await; return Ending::Stop; }
            _ = tick => {
                if !acknowledged {
                    return Ending::Reconnect("la pasarela no respondió al latido".into());
                }
                acknowledged = false;
                if sink.send(Message::Text(json!({"op":1,"d":sequence}).to_string())).await.is_err() {
                    return Ending::Reconnect("se perdió la conexión".into());
                }
            }
            frame = stream.next() => {
                let text = match frame {
                    Some(Ok(Message::Text(text))) => text,
                    Some(Ok(Message::Ping(payload))) => { let _ = sink.send(Message::Pong(payload)).await; continue; }
                    Some(Ok(Message::Close(Some(close)))) => {
                        return match u16::from(close.code) {
                            4004 => Ending::Fatal("Discord rechazó el bot_token".into()),
                            4013 | 4014 => Ending::Fatal("Discord rechazó los intents: activa Message Content en el portal de desarrolladores".into()),
                            4010..=4012 => Ending::Fatal("Discord rechazó la configuración de la conexión".into()),
                            code => Ending::Reconnect(format!("Discord cerró la conexión ({code})")),
                        };
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(_)) | None => return Ending::Reconnect("se perdió la conexión".into()),
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                if let Some(seq) = value["s"].as_i64() {
                    sequence = Some(seq);
                }
                match value["op"].as_u64() {
                    Some(10) => {
                        let every = value["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250).clamp(50, 120_000);
                        heartbeat = Some(tokio::time::interval_at(tokio::time::Instant::now() + Duration::from_millis(every), Duration::from_millis(every)));
                        let identify = json!({"op":2,"d":{"token":settings.token,"intents":INTENTS,
                            "properties":{"os":std::env::consts::OS,"browser":"hive-sdk","device":"hive-sdk"}}});
                        if sink.send(Message::Text(identify.to_string())).await.is_err() {
                            return Ending::Reconnect("se perdió la conexión".into());
                        }
                    }
                    Some(11) => acknowledged = true,
                    Some(1) => { let _ = sink.send(Message::Text(json!({"op":1,"d":sequence}).to_string())).await; }
                    Some(7) => return Ending::Reconnect("Discord pidió reconectar".into()),
                    Some(9) => return Ending::Reconnect("Discord invalidó la sesión".into()),
                    Some(0) => match value["t"].as_str() {
                        Some("READY") => {
                            bot_id = value["d"]["user"]["id"].as_str().unwrap_or_default().to_owned();
                            channels::set_status(app, id, "connected", None);
                        }
                        Some("MESSAGE_CREATE") => {
                            let message = &value["d"];
                            let (Some(channel), Some(author), Some(content)) = (message["channel_id"].as_str(), message["author"]["id"].as_str(), message["content"].as_str()) else { continue };
                            let ordinary = matches!(message["type"].as_u64().unwrap_or(0), 0 | 19);
                            if !ordinary || message["author"]["bot"] == true || message["webhook_id"].is_string() || author == bot_id || !settings.allowed.contains(channel) {
                                continue;
                            }
                            let content = content.trim();
                            let files: Vec<Value> = message["attachments"].as_array().map(|f| f.iter().take(channels::MAX_IMAGES).cloned().collect()).unwrap_or_default();
                            if (content.is_empty() && files.is_empty()) || content.chars().count() > 4000 {
                                continue;
                            }
                            tokio::spawn(handle_message(app.clone(), client.clone(), settings.clone(), user.to_owned(), channel.to_owned(), content.to_owned(), files));
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
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
    let mut failures = 0u32;
    while !cancel.is_cancelled() {
        match session(&app, &id, &user, &settings, &client, &cancel).await {
            Ending::Stop => break,
            Ending::Fatal(reason) => {
                channels::set_status(&app, &id, "error", Some(&reason));
                return;
            }
            Ending::Reconnect(reason) => {
                failures = failures.saturating_add(1);
                channels::set_status(&app, &id, "error", Some(&reason));
                // A connection that was up for a while is not a failure streak.
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
    channels::supervise(app, "discord", |app, id, user, raw, cancel| {
        let settings = parse(&raw)?;
        tokio::spawn(run_channel(app, id, user, settings, cancel));
        Ok(())
    })
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configuration_is_validated_and_a_channel_without_an_allowlist_does_not_start() {
        let ok = parse(r#"{"bot_token":"abc.def","allowed_channel_ids":["123","456"]}"#).unwrap();
        assert_eq!(
            (
                ok.allowed.len(),
                ok.gateway_url.as_str(),
                ok.api_base.as_str()
            ),
            (2, DEFAULT_GATEWAY, DEFAULT_API)
        );
        for (config, why) in [
            (r#"{"bot_token":"x"}"#, "no allowlist"),
            (
                r#"{"bot_token":"x","allowed_channel_ids":[]}"#,
                "empty allowlist",
            ),
            (
                r#"{"bot_token":"x","allowed_channel_ids":[123]}"#,
                "ids must be strings, as Discord's are too big for JSON numbers",
            ),
            (
                r#"{"bot_token":"x","allowed_channel_ids":["abc"]}"#,
                "ids are digits",
            ),
            (r#"{"allowed_channel_ids":["1"]}"#, "no token"),
            (
                r#"{"bot_token":"a b","allowed_channel_ids":["1"]}"#,
                "token with spaces",
            ),
            (
                r#"{"bot_token":"x","allowed_channel_ids":["1"],"gateway_url":"ws://evil.example/"}"#,
                "plain ws to a remote host",
            ),
            (
                r#"{"bot_token":"x","allowed_channel_ids":["1"],"gateway_url":"https://gateway.discord.gg"}"#,
                "wrong scheme",
            ),
            (
                r#"{"bot_token":"x","allowed_channel_ids":["1"],"api_base":"http://evil.example"}"#,
                "plain http to a remote host",
            ),
            (
                r#"{"bot_token":"x","allowed_channel_ids":["1"],"api_base":"https://u:p@discord.com/api"}"#,
                "credentials",
            ),
            ("[]", "not an object"),
        ] {
            assert!(parse(config).is_err(), "{why}");
        }
        assert!(parse(r#"{"bot_token":"x","allowed_channel_ids":["1"],"gateway_url":"ws://127.0.0.1:9/?v=10","api_base":"http://127.0.0.1:9"}"#).is_ok());
    }
}
