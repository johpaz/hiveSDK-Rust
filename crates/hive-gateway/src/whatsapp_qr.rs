//! WhatsApp channel linked to a phone by QR (the Baileys way of the TypeScript Hive), on top of
//! the `whatsapp-rust` client. Type `whatsapp_qr`; the official Cloud API is type `whatsapp`.
//!
//! It is an **unofficial** client: using it may break WhatsApp's terms and get the number
//! suspended, so it is compiled only with the `whatsapp-qr` feature and a channel has to be
//! created on purpose. Without the feature the channel reports why it does not start.
//!
//! Same rules as every other channel, because a linked number is reachable by anybody:
//! - **Only numbers in `allowed_numbers` are ever answered**, and the list is mandatory. A
//!   sender is matched by phone number only (never by a LID, which is not a number); a
//!   message whose sender has no visible number is dropped.
//! - Groups, our own messages, messages that are not text and messages older than two minutes
//!   (the backlog delivered when the gateway reconnects) are never answered, and a message id
//!   is handled at most once.
//! - The link (device keys) lives in `<home>/whatsapp-qr/<channel id>.db`, readable by the
//!   owner only. The QR and the keys never appear in logs or errors; the QR is only in the
//!   channel's status (what the owner's client renders) and lasts about twenty seconds.
//! - Turns run as the channel's owner through the same durable driver as the chat.
// Without the feature only the (tested) filter and configuration code is compiled.
#![cfg_attr(not(feature = "whatsapp-qr"), allow(dead_code))]
use crate::{channels, config::App};
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
};

/// The channel row's `type`.
pub(crate) const KIND: &str = "whatsapp_qr";
/// A message older than this when it reaches us is backlog, not a conversation.
const STALE_SECS: i64 = 120;
/// WhatsApp accepts long messages; this keeps replies readable.
const MAX_TEXT_CHARS: usize = 3500;
/// Message ids remembered to handle each one at most once.
const SEEN_CAP: usize = 4096;

pub(crate) struct Settings {
    pub allowed: HashSet<String>,
}

pub(crate) fn parse(raw: &str) -> std::result::Result<Settings, String> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|_| "la configuración del canal no es JSON".to_owned())?;
    let list = value["allowed_numbers"].as_array();
    let allowed: HashSet<String> = list
        .map(|numbers| {
            numbers
                .iter()
                .filter_map(Value::as_str)
                .map(|number| number.trim().trim_start_matches('+').to_owned())
                .collect()
        })
        .unwrap_or_default();
    if allowed.is_empty() || list.is_some_and(|numbers| numbers.len() > 1000) {
        return Err("allowed_numbers es obligatorio: sin una lista de números permitidos el canal no arranca".into());
    }
    if let Some(bad) = allowed
        .iter()
        .find(|n| !(6..=15).contains(&n.len()) || !n.chars().all(|c| c.is_ascii_digit()))
    {
        return Err(format!(
            "allowed_numbers: «{bad}» no es un número internacional (6 a 15 dígitos, sin espacios)"
        ));
    }
    Ok(Settings { allowed })
}

/// The file name of a channel's link. Ids come from the gateway; this still refuses anything
/// that could leave the directory.
pub(crate) fn store_name(id: &str) -> std::result::Result<String, String> {
    (!id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    .then(|| format!("{id}.db"))
    .ok_or_else(|| "id de canal no válido".to_owned())
}

/// What the adapter extracts from an inbound message.
pub(crate) struct Incoming {
    pub id: String,
    /// Phone numbers (digits) the sender is known by; a LID is not included.
    pub senders: Vec<String>,
    pub from_me: bool,
    pub group: bool,
    pub age_secs: i64,
    pub text: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Handle(String),
    Ignore(&'static str),
}

/// Decides, per message, whether it is answered. Pure, so it is tested without WhatsApp.
pub(crate) struct Gate {
    allowed: HashSet<String>,
    order: VecDeque<String>,
    seen: HashSet<String>,
}

impl Gate {
    pub(crate) fn new(allowed: HashSet<String>) -> Self {
        Self {
            allowed,
            order: VecDeque::new(),
            seen: HashSet::new(),
        }
    }

    pub(crate) fn admit(&mut self, message: &Incoming) -> Verdict {
        if message.from_me {
            return Verdict::Ignore("own message");
        }
        if message.group {
            return Verdict::Ignore("group");
        }
        // Before remembering anything: strangers must not be able to fill the memory.
        if !message.senders.iter().any(|n| self.allowed.contains(n)) {
            return Verdict::Ignore("sender not allowed");
        }
        if message.age_secs > STALE_SECS {
            return Verdict::Ignore("stale");
        }
        if !self.seen.insert(message.id.clone()) {
            return Verdict::Ignore("duplicate");
        }
        self.order.push_back(message.id.clone());
        if self.order.len() > SEEN_CAP
            && let Some(oldest) = self.order.pop_front()
        {
            self.seen.remove(&oldest);
        }
        match message.text.as_deref().map(str::trim) {
            Some(text) if !text.is_empty() => Verdict::Handle(text.to_owned()),
            _ => Verdict::Ignore("not text"),
        }
    }
}

fn put_status(app: &App, id: &str, value: Value) {
    if let Ok(mut all) = app.channel_status.lock() {
        all.insert(id.to_owned(), value);
    }
}

/// The QR to pair, as the owner's client shows it. Replaced by `connected` once linked.
fn set_qr(app: &App, id: &str, code: &str, seconds: u64) {
    put_status(
        app,
        id,
        json!({"status":"qr","error":null,"qr":code,"expires_in_s":seconds}),
    );
}

#[cfg(not(feature = "whatsapp-qr"))]
pub(crate) async fn run(app: Arc<App>) {
    channels::supervise(
        app,
        KIND,
        |_app, _id, _user, raw, _cancel| -> std::result::Result<(), String> {
            parse(&raw)?;
            Err("este gateway se compiló sin la feature `whatsapp-qr`; usa el canal `whatsapp` (Cloud API) o recompila con ella".into())
        },
    )
    .await;
}

/// Without the feature nothing is connected, so nothing is sent.
#[cfg(not(feature = "whatsapp-qr"))]
pub(crate) async fn broadcast(_id: &str, _raw: &str, _text: &str) -> channels::Sent {
    channels::Sent::failed("el gateway se compiló sin la feature whatsapp-qr".into())
}

#[cfg(feature = "whatsapp-qr")]
pub(crate) use live::{broadcast, run};

#[cfg(feature = "whatsapp-qr")]
mod live {
    use super::*;
    use std::{
        collections::HashMap,
        path::PathBuf,
        sync::{LazyLock, Mutex},
        time::Duration,
    };
    use whatsapp_rust::{Jid, prelude::*, waproto::whatsapp as wa};

    /// The connected client of each running channel, for sending on our own initiative.
    static CLIENTS: LazyLock<Mutex<HashMap<String, Arc<Client>>>> = LazyLock::new(Default::default);

    pub(crate) async fn run(app: Arc<App>) {
        channels::supervise(app, KIND, |app, id, user, raw, cancel| {
            let settings = parse(&raw)?;
            let file = store_name(&id)?;
            let dir = app.home.join("whatsapp-qr");
            tokio::spawn(session(app, id, user, settings, dir.join(file), cancel));
            Ok(())
        })
        .await;
    }

    pub(super) fn private_dir(path: &std::path::Path) -> std::io::Result<()> {
        let dir = path.parent().unwrap_or(path);
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    async fn session(
        app: Arc<App>,
        id: String,
        user: String,
        settings: Settings,
        store: PathBuf,
        cancel: hive_agent_core::provider::CancellationToken,
    ) {
        let gate = Arc::new(Mutex::new(Gate::new(settings.allowed.clone())));
        loop {
            channels::set_status(&app, &id, "connecting", None);
            if private_dir(&store).is_err() {
                channels::set_status(
                    &app,
                    &id,
                    "error",
                    Some("no se pudo preparar el almacén del enlace"),
                );
                if channels::pause(&cancel, Duration::from_secs(30)).await {
                    return;
                }
                continue;
            }
            let logged_out = Arc::new(tokio::sync::Notify::new());
            let built = async {
                let backend = SqliteStore::new(store.to_string_lossy().as_ref())
                    .await
                    .map_err(|_| "no se pudo abrir el almacén del enlace".to_owned())?;
                Bot::builder()
                    .with_backend(backend)
                    .on_qr_code({
                        let (app, id) = (app.clone(), id.clone());
                        move |code, timeout| {
                            let (app, id) = (app.clone(), id.clone());
                            async move { set_qr(&app, &id, &code, timeout.as_secs()) }
                        }
                    })
                    .on_connected({
                        let (app, id) = (app.clone(), id.clone());
                        move |client| {
                            let (app, id) = (app.clone(), id.clone());
                            async move {
                                if let Ok(mut all) = CLIENTS.lock() {
                                    all.insert(id.clone(), client);
                                }
                                channels::set_status(&app, &id, "connected", None);
                            }
                        }
                    })
                    .on_logged_out({
                        let logged_out = logged_out.clone();
                        move |_| {
                            let logged_out = logged_out.clone();
                            async move { logged_out.notify_one() }
                        }
                    })
                    .on_message({
                        let (app, user, id, gate) =
                            (app.clone(), user.clone(), id.clone(), gate.clone());
                        move |ctx| {
                            let (app, user, id, gate) =
                                (app.clone(), user.clone(), id.clone(), gate.clone());
                            async move { inbound(app, user, id, gate, ctx) }
                        }
                    })
                    .build()
                    .await
                    .map_err(|_| "no se pudo iniciar el cliente de WhatsApp".to_owned())
            }
            .await;
            let bot = match built {
                Ok(bot) => bot,
                Err(reason) => {
                    channels::set_status(&app, &id, "error", Some(&reason));
                    if channels::pause(&cancel, Duration::from_secs(30)).await {
                        return;
                    }
                    continue;
                }
            };
            let mut handle = bot.spawn();
            let relink = tokio::select! {
                _ = cancel.cancelled() => {
                    handle.shutdown().await;
                    forget(&id);
                    channels::set_status(&app, &id, "disconnected", None);
                    return;
                }
                _ = logged_out.notified() => true,
                _ = &mut handle => false,
            };
            forget(&id);
            if relink {
                // The phone unlinked this device: the keys are useless, so start from a new QR.
                let _ = std::fs::remove_file(&store);
                channels::set_status(
                    &app,
                    &id,
                    "disconnected",
                    Some("el teléfono desvinculó el dispositivo; se genera un QR nuevo"),
                );
            }
            if channels::pause(&cancel, Duration::from_secs(if relink { 5 } else { 10 })).await {
                return;
            }
        }
    }

    fn forget(id: &str) {
        if let Ok(mut all) = CLIENTS.lock() {
            all.remove(id);
        }
    }

    /// Map the library's message to what the gate needs, then answer in its own task: a turn
    /// can last minutes and must not hold up the event loop.
    fn inbound(
        app: Arc<App>,
        user: String,
        id: String,
        gate: Arc<Mutex<Gate>>,
        ctx: MessageContext,
    ) {
        let source = &ctx.info.source;
        let numbers = |jid: &Jid| jid.is_pn().then(|| jid.user.to_string());
        let message = Incoming {
            id: ctx.info.id.to_string(),
            senders: [Some(&source.sender), source.sender_alt.as_ref()]
                .into_iter()
                .flatten()
                .filter_map(numbers)
                .collect(),
            from_me: source.is_from_me,
            group: source.is_group,
            age_secs: (chrono::Utc::now() - ctx.info.timestamp).num_seconds(),
            text: ctx.message.text_content().map(str::to_owned),
        };
        let verdict = match gate.lock() {
            Ok(mut gate) => gate.admit(&message),
            Err(_) => return,
        };
        let Verdict::Handle(text) = verdict else {
            return;
        };
        let peer = message.senders.first().cloned().unwrap_or_default();
        let title = if ctx.info.push_name.is_empty() {
            peer.clone()
        } else {
            ctx.info.push_name.clone()
        };
        tokio::spawn(async move {
            let ctx_ref = &ctx;
            let notify = |question: String| async move {
                let _ = ctx_ref.reply(question).await;
            };
            let answer = channels::ask_agent(&app, &user, KIND, &peer, &title, &text, notify).await;
            let _ = &id;
            for part in channels::chunks(&channels::reply_text(answer), MAX_TEXT_CHARS) {
                if ctx.reply(part).await.is_err() {
                    break;
                }
            }
        });
    }

    /// Send `text` on our own initiative to every allowed number through the connected client.
    pub(crate) async fn broadcast(id: &str, raw: &str, text: &str) -> channels::Sent {
        let settings = match parse(raw) {
            Ok(settings) => settings,
            Err(reason) => return channels::Sent::failed(reason),
        };
        let client = CLIENTS.lock().ok().and_then(|all| all.get(id).cloned());
        let Some(client) = client else {
            return channels::Sent::failed("el canal no está conectado".into());
        };
        let mut sent = channels::Sent::default();
        let mut numbers: Vec<_> = settings.allowed.into_iter().collect();
        numbers.sort();
        for number in numbers.into_iter().take(channels::MAX_BROADCAST) {
            for part in channels::chunks(text, MAX_TEXT_CHARS) {
                let outcome = client
                    .send_message(&Jid::pn(number.as_str()), wa::Message::text(part))
                    .await
                    .map(|_| ())
                    .map_err(|_| "no se pudo enviar por WhatsApp".to_owned());
                let failed = outcome.is_err();
                sent.record(outcome);
                if failed {
                    break;
                }
            }
        }
        sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed(numbers: &[&str]) -> HashSet<String> {
        numbers.iter().map(|n| (*n).to_owned()).collect()
    }
    fn message(id: &str, sender: &str, text: Option<&str>) -> Incoming {
        Incoming {
            id: id.into(),
            senders: vec![sender.into()],
            from_me: false,
            group: false,
            age_secs: 1,
            text: text.map(str::to_owned),
        }
    }

    #[test]
    fn the_configuration_needs_an_allowlist_of_real_numbers() {
        let ok = parse(r#"{"allowed_numbers":["+573001112233","14155550123"]}"#).unwrap();
        assert_eq!(ok.allowed, allowed(&["573001112233", "14155550123"]));
        for (raw, why) in [
            (r#"{}"#, "no list"),
            (r#"{"allowed_numbers":[]}"#, "empty"),
            (r#"{"allowed_numbers":["abc"]}"#, "not digits"),
            (r#"{"allowed_numbers":["123"]}"#, "too short"),
            (r#"{"allowed_numbers":["1234567890123456"]}"#, "too long"),
            (r#"{"allowed_numbers":["57 300 111"]}"#, "spaces"),
            (
                r#"{"allowed_numbers":[573001112233]}"#,
                "numbers are strings",
            ),
            ("not json", "not json"),
        ] {
            assert!(parse(raw).is_err(), "{why}");
        }
        let many = format!(
            r#"{{"allowed_numbers":[{}]}}"#,
            (0..1001)
                .map(|n| format!("\"{}\"", 570000000 + n))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(parse(&many).is_err(), "at most 1000");
        let error = parse(r#"{"allowed_numbers":["abc"]}"#).err().unwrap();
        assert!(!error.contains("token"), "{error}");
    }

    #[test]
    fn a_link_file_name_cannot_leave_its_directory() {
        assert_eq!(store_name("abc-123_x").unwrap(), "abc-123_x.db");
        for bad in ["", "../x", "a/b", "a\\b", "a.b", "a b", &"x".repeat(65)] {
            assert!(store_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_allowed_numbers_get_an_answer_and_everything_else_is_dropped_silently() {
        let mut gate = Gate::new(allowed(&["573001112233"]));
        assert_eq!(
            gate.admit(&message("1", "573001112233", Some(" hola "))),
            Verdict::Handle("hola".into())
        );
        assert_eq!(
            gate.admit(&message("2", "573009998877", Some("hola"))),
            Verdict::Ignore("sender not allowed")
        );
        let mut group = message("3", "573001112233", Some("hola"));
        group.group = true;
        assert_eq!(gate.admit(&group), Verdict::Ignore("group"));
        let mut own = message("4", "573001112233", Some("hola"));
        own.from_me = true;
        assert_eq!(gate.admit(&own), Verdict::Ignore("own message"));
        let mut hidden = message("5", "573001112233", Some("hola"));
        hidden.senders.clear();
        assert_eq!(
            gate.admit(&hidden),
            Verdict::Ignore("sender not allowed"),
            "a sender with no number (a LID) is never matched"
        );
        assert_eq!(
            gate.admit(&message("6", "573001112233", None)),
            Verdict::Ignore("not text")
        );
        assert_eq!(
            gate.admit(&message("7", "573001112233", Some("  "))),
            Verdict::Ignore("not text")
        );
        let mut either = message("8", "999", Some("hola"));
        either.senders.push("573001112233".into());
        assert_eq!(
            gate.admit(&either),
            Verdict::Handle("hola".into()),
            "any known number of the sender counts"
        );
    }

    #[test]
    fn backlog_is_ignored_and_each_message_is_handled_at_most_once() {
        let mut gate = Gate::new(allowed(&["573001112233"]));
        let mut old = message("a", "573001112233", Some("hola"));
        old.age_secs = STALE_SECS + 1;
        assert_eq!(gate.admit(&old), Verdict::Ignore("stale"));
        let mut fresh = message("b", "573001112233", Some("hola"));
        fresh.age_secs = STALE_SECS;
        assert_eq!(gate.admit(&fresh), Verdict::Handle("hola".into()));
        assert_eq!(
            gate.admit(&message("b", "573001112233", Some("hola"))),
            Verdict::Ignore("duplicate")
        );
        // A stale delivery that is retried fresh is still handled once: it was never recorded.
        assert_eq!(
            gate.admit(&message("a", "573001112233", Some("hola"))),
            Verdict::Handle("hola".into())
        );
    }

    #[test]
    fn the_memory_of_handled_messages_is_bounded_and_strangers_cannot_fill_it() {
        let mut gate = Gate::new(allowed(&["573001112233"]));
        for n in 0..(SEEN_CAP * 2) {
            assert_eq!(
                gate.admit(&message(&format!("stranger{n}"), "573009998877", Some("x"))),
                Verdict::Ignore("sender not allowed")
            );
        }
        assert!(gate.seen.is_empty(), "strangers are never remembered");
        for n in 0..(SEEN_CAP + 10) {
            gate.admit(&message(&format!("m{n}"), "573001112233", Some("x")));
        }
        assert_eq!(gate.seen.len(), SEEN_CAP);
        assert_eq!(gate.order.len(), SEEN_CAP);
        assert_eq!(
            gate.admit(&message("m0", "573001112233", Some("x"))),
            Verdict::Handle("x".into()),
            "the oldest was forgotten"
        );
    }

    #[cfg(not(feature = "whatsapp-qr"))]
    #[tokio::test]
    async fn without_the_feature_the_channel_says_why_and_sends_nothing() {
        let sent = broadcast("id", r#"{"allowed_numbers":["573001112233"]}"#, "hola").await;
        assert_eq!(sent.destinations, 0);
        assert!(sent.error.unwrap().contains("whatsapp-qr"));
    }

    #[cfg(feature = "whatsapp-qr")]
    #[tokio::test]
    async fn the_link_directory_is_private_and_sending_without_a_connection_fails_cleanly() {
        let home = tempfile::tempdir().unwrap();
        let file = home
            .path()
            .join("whatsapp-qr")
            .join(store_name("c1").unwrap());
        live::private_dir(&file).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(file.parent().unwrap())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o700,
                "the device keys are for the owner only"
            );
        }
        let sent = broadcast(
            "not-connected",
            r#"{"allowed_numbers":["573001112233"]}"#,
            "hola",
        )
        .await;
        assert_eq!(sent.destinations, 0);
        assert!(sent.error.unwrap().contains("no está conectado"));
        let bad = broadcast("x", r#"{"allowed_numbers":[]}"#, "hola").await;
        assert!(bad.error.unwrap().contains("allowed_numbers"));
    }
}
