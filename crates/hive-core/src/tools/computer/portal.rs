//! Wayland backend: the xdg desktop portals over D-Bus.
//!
//! A Wayland client cannot capture the screen or inject input on its own; the desktop offers
//! `RemoteDesktop` (input) and `ScreenCast` (the output the pointer coordinates refer to) and asks
//! the person for consent when the session starts. Screenshots use the `Screenshot` portal.
//! Declining the dialog is an error, never a retry.
use super::{Button, ComputerPolicy, Shot};
use crate::{Error, Result};
use futures_util::StreamExt;
use std::{collections::HashMap, path::PathBuf};
use zbus::{
    Connection, Proxy,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";

fn err(error: impl std::fmt::Display) -> Error {
    Error::State(format!("portal: {error}"))
}

type Options<'a> = HashMap<&'a str, Value<'a>>;
type Results = HashMap<String, OwnedValue>;

pub(crate) struct Portal {
    conn: Connection,
    session: OwnedObjectPath,
    stream: u32,
    size: (u32, u32),
}

impl Portal {
    pub(super) async fn connect(policy: &ComputerPolicy) -> Result<Self> {
        let conn = Connection::session()
            .await
            .map_err(|e| err(format!("no desktop session bus ({e})")))?;
        let restore = policy
            .restore_token_path
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .map(|text| text.trim().to_owned())
            .filter(|token| !token.is_empty() && token.len() < 256);

        let mut options = Options::new();
        options.insert("session_handle_token", Value::from(token("s")));
        let created = request(
            &conn,
            "RemoteDesktop",
            "CreateSession",
            |opts| ((opts,), ()),
            options,
        )
        .await?;
        let session: OwnedObjectPath = created
            .get("session_handle")
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
            .and_then(|p| OwnedObjectPath::try_from(p).ok())
            .ok_or_else(|| err("CreateSession returned no session"))?;

        // Keyboard and pointer, and remember the grant when a token file was configured.
        let mut devices = Options::new();
        devices.insert("types", Value::from(3u32));
        devices.insert(
            "persist_mode",
            Value::from(
                if restore.is_some() || policy.restore_token_path.is_some() {
                    2u32
                } else {
                    0u32
                },
            ),
        );
        if let Some(token) = &restore {
            devices.insert("restore_token", Value::from(token.as_str()));
        }
        request(
            &conn,
            "RemoteDesktop",
            "SelectDevices",
            |opts| ((&session, opts), ()),
            devices,
        )
        .await?;

        let mut sources = Options::new();
        sources.insert("types", Value::from(1u32)); // monitors
        sources.insert("multiple", Value::from(false));
        sources.insert("cursor_mode", Value::from(2u32)); // pointer shown in the stream
        request(
            &conn,
            "ScreenCast",
            "SelectSources",
            |opts| ((&session, opts), ()),
            sources,
        )
        .await?;

        let started = request(
            &conn,
            "RemoteDesktop",
            "Start",
            |opts| ((&session, "", opts), ()),
            Options::new(),
        )
        .await?;
        let (stream, size) = first_stream(&started)?;
        if let (Some(path), Some(token)) = (
            &policy.restore_token_path,
            started
                .get("restore_token")
                .and_then(|v| String::try_from(v.try_clone().ok()?).ok()),
        ) {
            save_token(path, &token);
        }
        Ok(Self {
            conn,
            session,
            stream,
            size,
        })
    }

    pub(super) fn size(&self) -> Result<(u32, u32)> {
        Ok(self.size)
    }

    pub(super) async fn close(&self) {
        let _ = self
            .conn
            .call_method(
                Some(DEST),
                self.session.as_str(),
                Some("org.freedesktop.portal.Session"),
                "Close",
                &(),
            )
            .await;
    }

    pub(super) async fn screenshot(&self) -> Result<Shot> {
        let mut options = Options::new();
        options.insert("interactive", Value::from(false));
        let results = request(
            &self.conn,
            "Screenshot",
            "Screenshot",
            |opts| (("", opts), ()),
            options,
        )
        .await?;
        let uri = results
            .get("uri")
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
            .ok_or_else(|| err("the Screenshot portal returned no file"))?;
        let path = file_uri_path(&uri)
            .ok_or_else(|| err("the Screenshot portal returned an unusable location"))?;
        tokio::task::spawn_blocking(move || {
            let png = std::fs::read(&path)
                .map_err(|e| err(format!("could not read the screenshot ({e})")))?;
            // The portal made this file for us; do not leave screenshots lying around.
            if path.extension().is_some_and(|e| e == "png") {
                let _ = std::fs::remove_file(&path);
            }
            Ok(Shot { png })
        })
        .await
        .map_err(|_| err("screenshot task failed"))?
    }

    async fn notify(
        &self,
        method: &str,
        body: &(impl serde::Serialize + zbus::zvariant::DynamicType),
    ) -> Result<()> {
        self.conn
            .call_method(
                Some(DEST),
                PATH,
                Some("org.freedesktop.portal.RemoteDesktop"),
                method,
                body,
            )
            .await
            .map(|_| ())
            .map_err(|e| err(format!("{method}: {e}")))
    }

    pub(super) async fn motion(&self, x: u32, y: u32) -> Result<()> {
        self.notify(
            "NotifyPointerMotionAbsolute",
            &(
                &self.session,
                Options::new(),
                self.stream,
                f64::from(x),
                f64::from(y),
            ),
        )
        .await
    }

    pub(super) async fn button(&self, button: Button, down: bool) -> Result<()> {
        let code: i32 = match button {
            Button::Left => 0x110,
            Button::Right => 0x111,
            Button::Middle => 0x112,
        };
        self.notify(
            "NotifyPointerButton",
            &(&self.session, Options::new(), code, u32::from(down)),
        )
        .await
    }

    pub(super) async fn scroll(&self, dx: i32, dy: i32) -> Result<()> {
        for (axis, steps) in [(0u32, dy), (1u32, dx)] {
            if steps != 0 {
                self.notify(
                    "NotifyPointerAxisDiscrete",
                    &(&self.session, Options::new(), axis, steps),
                )
                .await?;
            }
        }
        Ok(())
    }

    pub(super) async fn key(&self, keysym: u32, down: bool) -> Result<()> {
        self.notify(
            "NotifyKeyboardKeysym",
            &(
                &self.session,
                Options::new(),
                keysym as i32,
                u32::from(down),
            ),
        )
        .await
    }
}

fn token(prefix: &str) -> String {
    format!("hive_{prefix}_{}", uuid::Uuid::new_v4().simple())
}

/// Call a portal method that answers through a `Request` object, and wait for its `Response`.
/// The signal is subscribed before the call: the answer can arrive before the call returns.
async fn request<'a, B>(
    conn: &Connection,
    interface: &str,
    method: &str,
    body: impl FnOnce(Options<'a>) -> (B, ()),
    mut options: Options<'a>,
) -> Result<Results>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let handle_token = token("r");
    options.insert("handle_token", Value::from(handle_token.clone()));
    let sender = conn
        .unique_name()
        .map(|n| n.as_str().trim_start_matches(':').replace('.', "_"))
        .ok_or_else(|| err("no unique name on the bus"))?;
    let request_path = format!("{PATH}/request/{sender}/{handle_token}");
    let proxy = Proxy::new(conn, DEST, request_path, "org.freedesktop.portal.Request")
        .await
        .map_err(err)?;
    let mut responses = proxy.receive_signal("Response").await.map_err(err)?;
    let (body, ()) = body(options);
    conn.call_method(
        Some(DEST),
        PATH,
        Some(format!("org.freedesktop.portal.{interface}").as_str()),
        method,
        &body,
    )
    .await
    .map_err(|e| err(format!("{interface}.{method}: {e}")))?;
    let message = responses
        .next()
        .await
        .ok_or_else(|| err("the portal closed without answering"))?;
    let (code, results): (u32, Results) = message.body().deserialize().map_err(err)?;
    match code {
        0 => Ok(results),
        1 => Err(Error::State(
            "the person declined the desktop's permission request".into(),
        )),
        _ => Err(err(format!("{interface}.{method} failed"))),
    }
}

fn first_stream(results: &Results) -> Result<(u32, (u32, u32))> {
    let streams = results
        .get("streams")
        .ok_or_else(|| err("no screen was shared"))?;
    let streams: Vec<(u32, HashMap<String, OwnedValue>)> = streams
        .try_clone()
        .map_err(err)?
        .try_into()
        .map_err(|_| err("unreadable stream list"))?;
    let (node, props) = streams
        .into_iter()
        .next()
        .ok_or_else(|| err("no screen was shared"))?;
    let (w, h): (i32, i32) = props
        .get("size")
        .and_then(|v| v.try_clone().ok()?.try_into().ok())
        .ok_or_else(|| err("the shared screen has no size"))?;
    if w <= 0 || h <= 0 {
        return Err(err("the shared screen has an empty size"));
    }
    Ok((node, (w as u32, h as u32)))
}

fn save_token(path: &PathBuf, token: &str) {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    if let Ok(mut file) = options.open(path) {
        let _ = file.write_all(token.as_bytes());
    }
}

/// `file:///home/a%20b/x.png` → `/home/a b/x.png`.
fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let path = PathBuf::from(String::from_utf8(out).ok()?);
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::file_uri_path;
    #[test]
    fn file_uris_are_decoded_and_must_be_absolute_local_paths() {
        assert_eq!(
            file_uri_path("file:///home/a%20b/x.png").unwrap().to_str(),
            Some("/home/a b/x.png")
        );
        assert_eq!(
            file_uri_path("file://localhost/tmp/x.png")
                .unwrap()
                .to_str(),
            Some("/tmp/x.png")
        );
        assert!(file_uri_path("https://example.com/x.png").is_none());
        assert!(file_uri_path("file:///x%zz").is_none());
    }
}
