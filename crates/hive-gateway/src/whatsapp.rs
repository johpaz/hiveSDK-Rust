//! WhatsApp Cloud API channel, by webhook.
//!
//! Meta calls `GET/POST /webhook/whatsapp/{channel id}` on a public HTTPS address that the
//! operator exposes (the gateway itself listens on localhost only; put a reverse proxy or
//! tunnel in front of that one path). This is the one route served **before** the bearer
//! token, so it carries its own protection:
//!
//! - **Every POST must be signed.** `X-Hub-Signature-256` is checked as an HMAC-SHA256 of
//!   the exact body with the app secret, in constant time; anything unsigned or wrongly
//!   signed is a 401 and is never parsed or acted upon. The app secret is mandatory.
//! - **Verification** (`hub.verify_token`) is compared in constant time too.
//! - **An unknown, disabled or misconfigured channel is a plain 404**, revealing nothing.
//! - Only **text from numbers in `allowed_numbers`** (mandatory) is answered, and only
//!   messages for this channel's `phone_number_id`.
//! - **At most once.** WhatsApp redelivers a webhook it thinks failed, so message ids are
//!   recorded (persistently, the last 200) before anything runs and a seen id is dropped.
//!   The webhook answers 200 at once; the turn runs afterwards.
//! - Secrets never appear in statuses, errors or responses; the chat only gets the generic
//!   apology when something fails.
use crate::{
    channels,
    config::{self, App},
};
use http::Request;
use ring::hmac;
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc, time::Duration};
use subtle::ConstantTimeEq;

const MAX_TEXT_CHARS: usize = 4096;
const REMEMBERED: usize = 200;
const DEFAULT_API: &str = "https://graph.facebook.com/v21.0";

pub(crate) struct Settings {
    access_token: String,
    phone_number_id: String,
    app_secret: String,
    verify_token: String,
    allowed: HashSet<String>,
    api_base: String,
}

fn digits(number: &str) -> String {
    number.chars().filter(char::is_ascii_digit).collect()
}

pub(crate) fn parse(raw: &str) -> std::result::Result<Settings, String> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| "la configuración del canal no es JSON".to_owned())?;
    let text = |key: &str| {
        value[key]
            .as_str()
            .filter(|t| !t.is_empty() && t.len() <= 600 && !t.chars().any(char::is_whitespace))
            .map(str::to_owned)
    };
    let (Some(access_token), Some(phone_number_id), Some(app_secret), Some(verify_token)) = (
        text("access_token"),
        text("phone_number_id"),
        text("app_secret"),
        text("verify_token"),
    ) else {
        return Err(
            "faltan access_token, phone_number_id, app_secret (para firmar) y verify_token".into(),
        );
    };
    if !phone_number_id.bytes().all(|b| b.is_ascii_digit()) {
        return Err("phone_number_id debe ser numérico".into());
    }
    let allowed: HashSet<String> = value["allowed_numbers"]
        .as_array()
        .map(|numbers| {
            numbers
                .iter()
                .filter_map(Value::as_str)
                .map(digits)
                .filter(|n| (6..=15).contains(&n.len()))
                .collect()
        })
        .unwrap_or_default();
    if allowed.is_empty()
        || value["allowed_numbers"]
            .as_array()
            .is_some_and(|n| n.len() > 1000)
    {
        return Err("allowed_numbers es obligatorio (números con prefijo de país): sin una lista de números permitidos el canal no responde".into());
    }
    let api_base = value["api_base"]
        .as_str()
        .unwrap_or(DEFAULT_API)
        .trim_end_matches('/')
        .to_owned();
    channels::safe_url(&api_base, "https", "http", false)
        .map_err(|reason| format!("api_base: {reason}"))?;
    Ok(Settings {
        access_token,
        phone_number_id,
        app_secret,
        verify_token,
        allowed,
        api_base,
    })
}

/// The channel's live status: it is a webhook, so it is `listening` when its configuration is usable.
pub(crate) fn status(app: &App, id: &str) -> Value {
    match config::secret(&app.db, &app.home, &format!("channel:{id}:config")) {
        Ok(Some(raw)) => match parse(&raw) {
            Ok(_) => {
                json!({"status":"listening","error":null,"webhook":format!("/webhook/whatsapp/{id}")})
            }
            Err(reason) => json!({"status":"error","error":reason}),
        },
        _ => json!({"status":"error","error":"el canal no tiene configuración"}),
    }
}

fn channel(app: &App, id: &str) -> Option<(String, Settings)> {
    let row = app.db.col_get("channels", id).ok().flatten()?.doc;
    if row["type"] != "whatsapp" || row["enabled"] != true || row["active"] != true {
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
    Some((user, parse(&raw).ok()?))
}

fn signed(secret: &str, body: &[u8], header: Option<&str>) -> bool {
    let Some(sent) = header
        .and_then(|h| h.strip_prefix("sha256="))
        .and_then(|hex| hex::decode(hex).ok())
    else {
        return false;
    };
    hmac::verify(
        &hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes()),
        body,
        &sent,
    )
    .is_ok()
}

/// The ids of messages not seen before, recorded (persisted) before anything runs.
fn first_time(app: &App, id: &str, ids: Vec<String>) -> Vec<String> {
    channels::first_time(app, &format!("whatsapp:{id}"), ids, REMEMBERED)
}

async fn send(
    client: &reqwest::Client,
    settings: &Settings,
    to: &str,
    text: &str,
) -> std::result::Result<(), String> {
    for part in channels::chunks(text, MAX_TEXT_CHARS) {
        let body = json!({"messaging_product":"whatsapp","to":to,"type":"text","text":{"preview_url":false,"body":part}});
        let sent = client
            .post(format!(
                "{}/{}/messages",
                settings.api_base, settings.phone_number_id
            ))
            .bearer_auth(&settings.access_token)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                if error.without_url().is_timeout() {
                    "tiempo de espera agotado".to_owned()
                } else {
                    "error de red con WhatsApp".to_owned()
                }
            })?;
        if !sent.status().is_success() {
            // Outside the 24-hour customer-service window WhatsApp only accepts approved templates.
            return Err(format!(
                "WhatsApp respondió {} (fuera de la ventana de 24 h solo admite plantillas)",
                sent.status().as_u16()
            ));
        }
    }
    Ok(())
}

/// Send `text` on its own initiative to every allowed number (at most `channels::MAX_BROADCAST`).
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
    for number in settings.allowed.iter().take(channels::MAX_BROADCAST) {
        sent.record(send(&client, &settings, number, text).await);
    }
    sent
}

/// A picture sent to the number: its media id gives a short-lived URL (Graph API), and both calls
/// carry the access token. The file is only fetched from Meta's hosts over https, or the configured
/// API host itself, and the bytes decide what it is.
async fn download_media(settings: &Settings, media_id: &str) -> Option<Value> {
    if media_id.is_empty()
        || media_id.len() > 64
        || !media_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    let token = settings.access_token.clone();
    let info_url = format!("{}/{media_id}", settings.api_base);
    let info = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .build()
        .ok()?
        .get(info_url)
        .bearer_auth(&token)
        .send()
        .await
        .ok()?
        .json::<Value>()
        .await
        .ok()?;
    if info["file_size"]
        .as_u64()
        .is_some_and(|s| s > channels::MAX_IMAGE_BYTES as u64)
        || !info["mime_type"]
            .as_str()
            .is_some_and(|m| channels::IMAGE_TYPES.contains(&m))
    {
        return None;
    }
    let url = channels::trusted_file_url(
        info["url"].as_str()?,
        &["fbsbx.com", "facebook.com", "whatsapp.net", "fbcdn.net"],
        &settings.api_base,
    )?;
    channels::fetch_image(|client| client.get(url).bearer_auth(token), None).await
}

async fn answer(
    app: Arc<App>,
    user: String,
    settings: Arc<Settings>,
    from: String,
    text: String,
    media: Option<String>,
) {
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
    else {
        return;
    };
    let mut text = text;
    let mut attachments = vec![];
    if let Some(media_id) = media {
        match download_media(&settings, &media_id).await {
            Some(image) => attachments.push(image),
            None if text.is_empty() => {
                let _ = send(&client, &settings, &from, channels::NOT_AN_IMAGE_TEXT).await;
                return;
            }
            None => {}
        }
        if text.is_empty() {
            text = "Mira la imagen adjunta.".into();
        }
    }
    let title = format!("WhatsApp {from}");
    let (client_ref, settings_ref, from_ref) = (&client, &settings, &from);
    let notify = |question: String| async move {
        let _ = send(client_ref, settings_ref, from_ref, &question).await;
    };
    let reply = channels::ask_agent_with(
        &app,
        &user,
        "whatsapp",
        &from,
        &title,
        &text,
        attachments,
        notify,
    )
    .await;
    let _ = send(&client, &settings, &from, &channels::reply_text(reply)).await;
}

/// `(status, body)` for a request to `/webhook/whatsapp/{id}`. A string body is sent as plain text.
pub(crate) async fn handle(app: Arc<App>, request: Request<Vec<u8>>) -> (u16, Value) {
    let not_found = (404, json!({"error":"Canal desconocido"}));
    let Some(id) = request
        .uri()
        .path()
        .strip_prefix("/webhook/whatsapp/")
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
    let Ok(Some((user, settings))) =
        tokio::task::spawn_blocking(move || channel(&host, &lookup)).await
    else {
        return not_found;
    };
    match request.method().as_str() {
        "GET" => {
            let query =
                crate::server::query(request.uri().query().unwrap_or_default()).unwrap_or_default();
            let token = query
                .get("hub.verify_token")
                .map(String::as_bytes)
                .unwrap_or_default();
            let valid = query
                .get("hub.mode")
                .is_some_and(|mode| mode == "subscribe")
                && bool::from(token.ct_eq(settings.verify_token.as_bytes()));
            match query
                .get("hub.challenge")
                .filter(|c| valid && c.len() <= 200)
            {
                Some(challenge) => (200, Value::String(challenge.clone())),
                None => (403, json!({"error":"Verificación rechazada"})),
            }
        }
        "POST" => {
            let signature = request
                .headers()
                .get("x-hub-signature-256")
                .and_then(|v| v.to_str().ok());
            if !signed(&settings.app_secret, request.body(), signature) {
                return (401, json!({"error":"Firma inválida"}));
            }
            let Ok(payload) = serde_json::from_slice::<Value>(request.body()) else {
                return (200, json!({"ok":true}));
            };
            // (message id, sender, words, media id of a picture)
            let mut wanted: Vec<(String, String, String, Option<String>)> = vec![];
            if payload["object"] == "whatsapp_business_account" {
                for entry in payload["entry"].as_array().into_iter().flatten() {
                    for change in entry["changes"].as_array().into_iter().flatten() {
                        let value = &change["value"];
                        if value["metadata"]["phone_number_id"] != settings.phone_number_id.as_str()
                        {
                            continue;
                        }
                        for message in value["messages"].as_array().into_iter().flatten() {
                            let (Some(mid), Some(from)) =
                                (message["id"].as_str(), message["from"].as_str())
                            else {
                                continue;
                            };
                            // Words, or a picture with its caption; nothing else is answered.
                            let (text, media) = match message["type"].as_str() {
                                Some("text") => (
                                    message["text"]["body"].as_str().unwrap_or_default().trim(),
                                    None,
                                ),
                                Some("image") => (
                                    message["image"]["caption"]
                                        .as_str()
                                        .unwrap_or_default()
                                        .trim(),
                                    message["image"]["id"].as_str().map(str::to_owned),
                                ),
                                _ => continue,
                            };
                            let from = digits(from);
                            if settings.allowed.contains(&from)
                                && (!text.is_empty() || media.is_some())
                                && text.chars().count() <= MAX_TEXT_CHARS
                            {
                                wanted.push((mid.to_owned(), from, text.to_owned(), media));
                            }
                        }
                    }
                }
            }
            let host = app.clone();
            let ids: Vec<String> = wanted.iter().map(|(mid, _, _, _)| mid.clone()).collect();
            let fresh: HashSet<String> =
                tokio::task::spawn_blocking(move || first_time(&host, &id, ids))
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .collect();
            let settings = Arc::new(settings);
            for (mid, from, text, media) in wanted {
                if fresh.contains(&mid) {
                    tokio::spawn(answer(
                        app.clone(),
                        user.clone(),
                        settings.clone(),
                        from,
                        text,
                        media,
                    ));
                }
            }
            (200, json!({"ok":true}))
        }
        _ => (405, json!({"error":"Método no permitido"})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str =
        r#""access_token":"EAAB","phone_number_id":"1055","app_secret":"shh","verify_token":"vt""#;

    #[test]
    fn the_configuration_is_validated_and_a_channel_without_numbers_or_a_secret_is_unusable() {
        let ok = parse(&format!(
            "{{{BASE},\"allowed_numbers\":[\"+57 300 111-2222\",\"15551234567\"]}}"
        ))
        .unwrap();
        assert!(
            ok.allowed.contains("573001112222") && ok.allowed.contains("15551234567"),
            "numbers are normalised to digits"
        );
        for (config, why) in [
            (format!("{{{BASE}}}"), "no numbers"),
            (format!("{{{BASE},\"allowed_numbers\":[]}}"), "empty numbers"),
            (format!("{{{BASE},\"allowed_numbers\":[\"123\"]}}"), "too short to be a number"),
            (r#"{"access_token":"a","phone_number_id":"1","verify_token":"v","allowed_numbers":["15551234567"]}"#.into(), "no app_secret: unsigned webhooks would be accepted"),
            (r#"{"phone_number_id":"1","app_secret":"s","verify_token":"v","allowed_numbers":["15551234567"]}"#.into(), "no access token"),
            (r#"{"access_token":"a","phone_number_id":"abc","app_secret":"s","verify_token":"v","allowed_numbers":["15551234567"]}"#.into(), "non-numeric phone id"),
            (format!("{{{BASE},\"allowed_numbers\":[\"15551234567\"],\"api_base\":\"http://evil.example\"}}"), "plain http to a remote host"),
            ("[]".into(), "not an object"),
        ] {
            assert!(parse(&config).is_err(), "{why}");
        }
    }

    #[test]
    fn signatures_are_checked_over_the_exact_body_and_anything_else_fails() {
        let sign = |secret: &str, body: &[u8]| {
            format!(
                "sha256={}",
                hex::encode(
                    hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes()), body)
                        .as_ref()
                )
            )
        };
        let body = br#"{"object":"whatsapp_business_account"}"#;
        assert!(signed("shh", body, Some(&sign("shh", body))));
        assert!(
            !signed("shh", body, Some(&sign("other", body))),
            "wrong secret"
        );
        assert!(
            !signed("shh", b"{}", Some(&sign("shh", body))),
            "a different body"
        );
        for bad in [
            None,
            Some(""),
            Some("sha256="),
            Some("sha256=zz"),
            Some("sha1=abcd"),
            Some(&sign("shh", body)[7..]),
        ] {
            assert!(!signed("shh", body, bad), "{bad:?}");
        }
    }
}
