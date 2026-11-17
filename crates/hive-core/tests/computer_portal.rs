//! The portal backend against a fake `org.freedesktop.portal.Desktop` on a private D-Bus daemon,
//! so the messages the SDK sends (and how it reads the answers) are checked without a desktop and
//! without anyone clicking a consent dialog. Skipped when `dbus-daemon` is not installed.
#![cfg(all(feature = "computer-use", target_os = "linux"))]
use hive_agent_core::{provider::CancellationToken, tools::NativeTools};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use zbus::{
    Connection, interface,
    message::Header,
    zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value as Variant},
};

type Opts = HashMap<String, OwnedValue>;

fn text(value: &OwnedValue) -> String {
    String::try_from(value.try_clone().unwrap()).unwrap()
}

#[derive(Clone)]
struct Shared {
    log: Arc<Mutex<Vec<String>>>,
    screenshot: Arc<Mutex<std::path::PathBuf>>,
    /// Answer the consent dialog with "cancelled" (code 1).
    decline: Arc<std::sync::atomic::AtomicBool>,
}

fn request_path(header: &Header<'_>, options: &Opts) -> OwnedObjectPath {
    let sender = header
        .sender()
        .unwrap()
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_");
    let token = text(&options["handle_token"]);
    ObjectPath::try_from(format!(
        "/org/freedesktop/portal/desktop/request/{sender}/{token}"
    ))
    .unwrap()
    .into()
}

async fn respond(
    connection: &Connection,
    path: &OwnedObjectPath,
    code: u32,
    results: HashMap<&str, Variant<'_>>,
) {
    connection
        .emit_signal(
            None::<&str>,
            path,
            "org.freedesktop.portal.Request",
            "Response",
            &(code, results),
        )
        .await
        .unwrap();
}

struct RemoteDesktop(Shared);
struct ScreenCast(Shared);
struct Screenshot(Shared);

#[interface(name = "org.freedesktop.portal.RemoteDesktop")]
impl RemoteDesktop {
    async fn create_session(
        &self,
        options: Opts,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> OwnedObjectPath {
        let path = request_path(&header, &options);
        let token = text(&options["session_handle_token"]);
        self.0.log.lock().unwrap().push("CreateSession".into());
        let session = format!("/org/freedesktop/portal/desktop/session/test/{token}");
        // Each session answers `Session.Close`, as the real portal does.
        connection
            .object_server()
            .at(session.as_str(), Session(self.0.clone()))
            .await
            .unwrap();
        let spawner = connection.clone();
        let (connection, reply) = (connection.clone(), path.clone());
        spawner
            .executor()
            .spawn(
                async move {
                    respond(
                        &connection,
                        &reply,
                        0,
                        HashMap::from([("session_handle", Variant::from(session))]),
                    )
                    .await
                },
                "respond",
            )
            .detach();
        path
    }
    async fn select_devices(
        &self,
        _session: OwnedObjectPath,
        options: Opts,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> OwnedObjectPath {
        let path = request_path(&header, &options);
        let mut keys: Vec<&String> = options.keys().collect();
        keys.sort();
        let restore = options.get("restore_token").map(text);
        self.0.log.lock().unwrap().push(format!(
            "SelectDevices {keys:?} restore={restore:?} persist={:?}",
            options
                .get("persist_mode")
                .map(|v| u32::try_from(v).unwrap())
        ));
        let spawner = connection.clone();
        let (connection, reply) = (connection.clone(), path.clone());
        spawner
            .executor()
            .spawn(
                async move { respond(&connection, &reply, 0, HashMap::new()).await },
                "respond",
            )
            .detach();
        path
    }
    async fn start(
        &self,
        _session: OwnedObjectPath,
        _parent: String,
        options: Opts,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> OwnedObjectPath {
        let path = request_path(&header, &options);
        self.0.log.lock().unwrap().push("Start".into());
        let spawner = connection.clone();
        let (connection, reply, decline) = (
            connection.clone(),
            path.clone(),
            self.0.decline.load(std::sync::atomic::Ordering::SeqCst),
        );
        spawner
            .executor()
            .spawn(
                async move {
                    if decline {
                        return respond(&connection, &reply, 1, HashMap::new()).await;
                    }
                    let props: HashMap<&str, Variant<'_>> =
                        HashMap::from([("size", Variant::from((1920i32, 1080i32)))]);
                    let streams = Variant::new(vec![(42u32, props)]);
                    respond(
                        &connection,
                        &reply,
                        0,
                        HashMap::from([
                            ("devices", Variant::from(3u32)),
                            ("streams", streams),
                            ("restore_token", Variant::from("token-from-portal")),
                        ]),
                    )
                    .await
                },
                "respond",
            )
            .detach();
        path
    }
    async fn notify_pointer_motion_absolute(
        &self,
        _s: OwnedObjectPath,
        _o: Opts,
        stream: u32,
        x: f64,
        y: f64,
    ) {
        self.0
            .log
            .lock()
            .unwrap()
            .push(format!("motion {stream} {x} {y}"));
    }
    async fn notify_pointer_button(&self, _s: OwnedObjectPath, _o: Opts, button: i32, state: u32) {
        self.0
            .log
            .lock()
            .unwrap()
            .push(format!("button {button:#x} {state}"));
    }
    async fn notify_pointer_axis_discrete(
        &self,
        _s: OwnedObjectPath,
        _o: Opts,
        axis: u32,
        steps: i32,
    ) {
        self.0
            .log
            .lock()
            .unwrap()
            .push(format!("axis {axis} {steps}"));
    }
    async fn notify_keyboard_keysym(&self, _s: OwnedObjectPath, _o: Opts, keysym: i32, state: u32) {
        self.0
            .log
            .lock()
            .unwrap()
            .push(format!("key {keysym:#x} {state}"));
    }
}

#[interface(name = "org.freedesktop.portal.ScreenCast")]
impl ScreenCast {
    async fn select_sources(
        &self,
        _session: OwnedObjectPath,
        options: Opts,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> OwnedObjectPath {
        let path = request_path(&header, &options);
        self.0.log.lock().unwrap().push("SelectSources".into());
        let spawner = connection.clone();
        let (connection, reply) = (connection.clone(), path.clone());
        spawner
            .executor()
            .spawn(
                async move { respond(&connection, &reply, 0, HashMap::new()).await },
                "respond",
            )
            .detach();
        path
    }
}

#[interface(name = "org.freedesktop.portal.Screenshot")]
impl Screenshot {
    async fn screenshot(
        &self,
        _parent: String,
        options: Opts,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> OwnedObjectPath {
        let path = request_path(&header, &options);
        let interactive = bool::try_from(options["interactive"].try_clone().unwrap()).unwrap();
        self.0
            .log
            .lock()
            .unwrap()
            .push(format!("Screenshot interactive={interactive}"));
        let uri = format!("file://{}", self.0.screenshot.lock().unwrap().display());
        let spawner = connection.clone();
        let (connection, reply) = (connection.clone(), path.clone());
        spawner
            .executor()
            .spawn(
                async move {
                    respond(
                        &connection,
                        &reply,
                        0,
                        HashMap::from([("uri", Variant::from(uri))]),
                    )
                    .await
                },
                "respond",
            )
            .detach();
        path
    }
}

/// `org.freedesktop.portal.Session.Close` is served on each session path.
struct Session(Shared);
#[interface(name = "org.freedesktop.portal.Session")]
impl Session {
    async fn close(&self) {
        self.0.log.lock().unwrap().push("Close".into());
    }
}

/// Kills the private bus when the test ends, however it ends.
struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn dbus() -> Option<(Daemon, String)> {
    let mut child = Command::new("dbus-daemon")
        .args(["--session", "--nofork", "--print-address=1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut address = String::new();
    BufReader::new(child.stdout.take()?)
        .read_line(&mut address)
        .ok()?;
    Some((Daemon(child), address.trim().to_owned()))
}

fn png(width: u32, height: u32) -> Vec<u8> {
    use image_codec::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
    let pixels = vec![200u8; (width * height * 3) as usize];
    let mut out = vec![];
    PngEncoder::new(&mut out)
        .write_image(&pixels, width, height, ExtendedColorType::Rgb8)
        .unwrap();
    out
}

async fn tools(policy: Value, dir: &std::path::Path) -> NativeTools {
    NativeTools::from_json(json!({"root":dir,"computer":policy})).unwrap()
}
async fn run(tools: &NativeTools, name: &str, arguments: Value) -> hive_agent_core::Result<Value> {
    tokio::time::timeout(
        Duration::from_secs(15),
        tools.execute_tool(name, arguments, &CancellationToken::new()),
    )
    .await
    .unwrap_or_else(|_| panic!("{name} hung"))
}

#[tokio::test]
async fn the_portal_flow_grants_input_maps_coordinates_and_remembers_consent() {
    let Some((_daemon, address)) = dbus() else {
        eprintln!("dbus-daemon is not installed: skipped");
        return;
    };
    // This test process owns its environment (it is the only test in this file).
    eprintln!("[t] daemon at {address}");
    unsafe { std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &address) };
    let dir = tempfile::tempdir().unwrap();
    let shot_path = dir.path().join("portal-shot.png");
    std::fs::write(&shot_path, png(3840, 2160)).unwrap(); // a HiDPI capture of a 1920x1080 stream
    let shared = Shared {
        log: Arc::default(),
        screenshot: Arc::new(Mutex::new(shot_path.clone())),
        decline: Arc::default(),
    };
    let service = zbus::connection::Builder::session()
        .unwrap()
        .name("org.freedesktop.portal.Desktop")
        .unwrap()
        .serve_at(
            "/org/freedesktop/portal/desktop",
            RemoteDesktop(shared.clone()),
        )
        .unwrap()
        .serve_at(
            "/org/freedesktop/portal/desktop",
            ScreenCast(shared.clone()),
        )
        .unwrap()
        .serve_at(
            "/org/freedesktop/portal/desktop",
            Screenshot(shared.clone()),
        )
        .unwrap()
        .build()
        .await
        .unwrap();
    // Sessions are created with a token the fake derives the path from; serve Close on any of them.
    let session_object =
        |token: &str| format!("/org/freedesktop/portal/desktop/session/test/{token}");
    let _ = (&service, session_object);

    eprintln!("[t] fake portal up");
    let token_file = dir.path().join("restore-token");
    let policy = json!({"backend":"portal","allow_input":true,"start_timeout_ms":8000,"max_dimension":960,"restore_token_path":token_file});
    let first = tools(policy.clone(), dir.path()).await;

    // Pointer: the model's view (960x540) maps onto the stream (1920x1080), by stream id.
    run(
        &first,
        "computer_click",
        json!({"x":480,"y":270,"button":"right"}),
    )
    .await
    .unwrap();
    run(&first, "computer_scroll", json!({"dy":-2,"dx":3}))
        .await
        .unwrap();
    run(&first, "computer_key", json!({"keys":"ctrl+c"}))
        .await
        .unwrap();
    let log = shared.log.lock().unwrap().clone();
    let at = |prefix: &str| {
        log.iter()
            .position(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("{prefix} not in {log:?}"))
    };
    assert!(
        at("CreateSession") < at("SelectDevices")
            && at("SelectDevices") < at("SelectSources")
            && at("SelectSources") < at("Start")
    );
    assert!(
        log[at("SelectDevices")].contains("persist=Some(2)"),
        "the grant is to be remembered: {log:?}"
    );
    assert!(
        log[at("SelectDevices")].contains("restore=None"),
        "first time there is no token yet"
    );
    let input: Vec<&String> = log.iter().skip(at("Start") + 1).collect();
    assert_eq!(
        input,
        [
            "motion 42 961 541",
            "button 0x111 1",
            "button 0x111 0",
            "axis 0 -2",
            "axis 1 3",
            "key 0xffe3 1",
            "key 0x63 1",
            "key 0x63 0",
            "key 0xffe3 0"
        ],
    );

    // Consent was remembered in an owner-only file.
    assert_eq!(
        std::fs::read_to_string(&token_file).unwrap(),
        "token-from-portal"
    );
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&token_file).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // Screenshot: through the Screenshot portal (never interactive), brought to the view, and the
    // file the portal made is removed.
    let shot = run(&first, "computer_screenshot", json!({})).await.unwrap();
    assert_eq!(
        (shot["width"].as_u64(), shot["height"].as_u64()),
        (Some(960), Some(540))
    );
    assert_eq!(shot["backend"], "portal");
    assert!(
        shared
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|l| l == "Screenshot interactive=false")
    );
    assert!(
        !shot_path.exists(),
        "the portal's temporary screenshot is deleted"
    );
    first.close().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        shared.log.lock().unwrap().iter().any(|l| l == "Close"),
        "closing the tools ends the portal session"
    );

    // A new start presents the remembered token.
    std::fs::write(&shot_path, png(10, 10)).unwrap();
    shared.log.lock().unwrap().clear();
    let second = tools(policy, dir.path()).await;
    run(&second, "computer_mouse_move", json!({"x":0,"y":0}))
        .await
        .unwrap();
    let log = shared.log.lock().unwrap().clone();
    assert!(
        log.iter()
            .any(|l| l.contains("restore=Some(\"token-from-portal\")")),
        "{log:?}"
    );
    second.close().await;

    // The person says no to the desktop's dialog: a clear error, nothing is retried or sent.
    shared
        .decline
        .store(true, std::sync::atomic::Ordering::SeqCst);
    shared.log.lock().unwrap().clear();
    let third = tools(
        json!({"backend":"portal","allow_input":true,"start_timeout_ms":8000}),
        dir.path(),
    )
    .await;
    let error = run(&third, "computer_click", json!({"x":1,"y":1}))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("declined"), "{error}");
    assert!(
        !shared
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("motion") || l.starts_with("button"))
    );
}
