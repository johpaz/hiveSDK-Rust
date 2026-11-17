//! Desktop control for the model: screenshots, pointer and keyboard.
//!
//! Controlling the desktop acts on every open application and account, so this module is built
//! around authorization rather than around capability:
//! - **Off unless configured.** `ToolsConfig.computer` is absent by default and no `computer_*`
//!   tool is advertised without it.
//! - **Two explicit grants.** `allow_screenshot` (default true) and `allow_input` (default
//!   **false**); a tool of a scope that was not granted is neither advertised nor executable.
//! - **A budget.** At most `max_actions` actions until the host calls `computer_reset`.
//! - **A stop switch.** `computer_stop` (from the host, at any moment) refuses every later action
//!   and releases anything held down; only the host's `computer_resume` lifts it.
//! - **Nothing replayed.** An input action that times out or is cancelled has an unknown outcome:
//!   everything held down is released, the error says so, and the action is not repeated.
//!
//! The per-action approval a person may want (`ask`) belongs to the host's tool policy, which sees
//! these as ordinary tools named `computer_*`.
//!
//! Backends: X11 (`x11rb`, XTEST) and, on Wayland, the xdg desktop portals (RemoteDesktop,
//! ScreenCast and Screenshot over D-Bus), where the desktop itself asks the person for consent.
//! Coordinates the model uses are those of the screenshot it was given: the screen scaled to fit
//! `max_dimension`, so one screenshot and the clicks that follow it always agree.
use super::{Inner, fs, invalid};
use crate::{Error, Result, image_transform, media, provider::CancellationToken};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

pub(crate) mod fake;
mod keys;
#[cfg(target_os = "macos")]
mod mac;
#[cfg(target_os = "linux")]
mod portal;
#[cfg(target_os = "windows")]
mod win;
#[cfg(target_os = "linux")]
mod x11;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ComputerBackend {
    /// Wayland session → portals; otherwise X11.
    #[default]
    Auto,
    X11,
    Portal,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ComputerPolicy {
    pub backend: ComputerBackend,
    /// Offer `computer_screenshot`.
    #[serde(default = "yes")]
    pub allow_screenshot: bool,
    /// Offer the pointer and keyboard tools. Off by default.
    pub allow_input: bool,
    /// Longest side of the screenshot the model sees (and so of its coordinate space).
    pub max_dimension: u32,
    /// Actions allowed until `computer_reset`.
    pub max_actions: u32,
    pub action_timeout_ms: u64,
    /// How long to wait for the connection, which on Wayland includes the person answering the
    /// desktop's consent dialog.
    pub start_timeout_ms: u64,
    /// Longest `computer_type` text, in characters.
    pub max_text_chars: usize,
    /// X11 display (`:0`); `$DISPLAY` when absent.
    pub display: Option<String>,
    /// Absolute file where the portal's restore token is kept, so the next start does not ask
    /// again. Created with owner-only permissions.
    pub restore_token_path: Option<PathBuf>,
}

impl Default for ComputerPolicy {
    fn default() -> Self {
        Self {
            backend: ComputerBackend::Auto,
            allow_screenshot: true,
            allow_input: false,
            max_dimension: 1280,
            max_actions: 200,
            action_timeout_ms: 15_000,
            start_timeout_ms: 120_000,
            max_text_chars: 2_000,
            display: None,
            restore_token_path: None,
        }
    }
}

impl ComputerPolicy {
    pub(super) fn validate(&self) -> Result<()> {
        if !(320..=4096).contains(&self.max_dimension) {
            return Err(invalid(
                "computer.max_dimension must be between 320 and 4096",
            ));
        }
        if !(1..=10_000).contains(&self.max_actions) {
            return Err(invalid("computer.max_actions must be between 1 and 10000"));
        }
        if !(100..=120_000).contains(&self.action_timeout_ms)
            || !(1_000..=600_000).contains(&self.start_timeout_ms)
        {
            return Err(invalid("computer timeouts are out of range"));
        }
        if !(1..=20_000).contains(&self.max_text_chars) {
            return Err(invalid(
                "computer.max_text_chars must be between 1 and 20000",
            ));
        }
        if !self.allow_screenshot && !self.allow_input {
            return Err(invalid("computer needs allow_screenshot or allow_input"));
        }
        if self
            .restore_token_path
            .as_ref()
            .is_some_and(|p| !p.is_absolute())
        {
            return Err(invalid("computer.restore_token_path must be absolute"));
        }
        if self
            .display
            .as_ref()
            .is_some_and(|d| d.len() > 64 || d.contains(['\0', '/']))
        {
            return Err(invalid("computer.display is not a display name"));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Button {
    Left,
    Middle,
    Right,
}

/// PNG of 8-bit RGB pixels (the form every backend converts its capture to).
pub(crate) fn encode_png(rgb: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    use image_codec::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
    let mut png = vec![];
    PngEncoder::new(&mut png)
        .write_image(rgb, width, height, ExtendedColorType::Rgb8)
        .map_err(|_| Error::State("could not encode the screenshot".into()))?;
    Ok(png)
}

/// A captured screen, as PNG at the backend's own resolution.
pub(crate) struct Shot {
    pub png: Vec<u8>,
}

/// The connected desktop. An enum rather than a trait object so the async methods need no boxing.
pub(crate) enum Backend {
    Fake(Arc<fake::Fake>),
    #[cfg(target_os = "linux")]
    X11(x11::X11),
    #[cfg(target_os = "linux")]
    Portal(portal::Portal),
    #[cfg(target_os = "windows")]
    Windows(win::Windows),
    #[cfg(target_os = "macos")]
    Mac(mac::Mac),
}

impl Backend {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Fake(_) => "fake",
            #[cfg(target_os = "linux")]
            Self::X11(_) => "x11",
            #[cfg(target_os = "linux")]
            Self::Portal(_) => "portal",
            #[cfg(target_os = "windows")]
            Self::Windows(_) => "windows",
            #[cfg(target_os = "macos")]
            Self::Mac(_) => "macos",
        }
    }
    /// Size of the space pointer coordinates are in.
    pub(crate) async fn size(&self) -> Result<(u32, u32)> {
        match self {
            Self::Fake(b) => b.size(),
            #[cfg(target_os = "linux")]
            Self::X11(b) => b.size(),
            #[cfg(target_os = "linux")]
            Self::Portal(b) => b.size(),
            #[cfg(target_os = "windows")]
            Self::Windows(b) => b.size(),
            #[cfg(target_os = "macos")]
            Self::Mac(b) => b.size(),
        }
    }
    pub(crate) async fn screenshot(&self) -> Result<Shot> {
        match self {
            Self::Fake(b) => b.screenshot(),
            #[cfg(target_os = "linux")]
            Self::X11(b) => b.screenshot().await,
            #[cfg(target_os = "linux")]
            Self::Portal(b) => b.screenshot().await,
            #[cfg(target_os = "windows")]
            Self::Windows(b) => b.screenshot().await,
            #[cfg(target_os = "macos")]
            Self::Mac(b) => b.screenshot().await,
        }
    }
    pub(crate) async fn motion(&self, x: u32, y: u32) -> Result<()> {
        match self {
            Self::Fake(b) => b.motion(x, y),
            #[cfg(target_os = "linux")]
            Self::X11(b) => b.motion(x, y).await,
            #[cfg(target_os = "linux")]
            Self::Portal(b) => b.motion(x, y).await,
            #[cfg(target_os = "windows")]
            Self::Windows(b) => b.motion(x, y).await,
            #[cfg(target_os = "macos")]
            Self::Mac(b) => b.motion(x, y).await,
        }
    }
    pub(crate) async fn button(&self, button: Button, down: bool) -> Result<()> {
        match self {
            Self::Fake(b) => b.button(button, down),
            #[cfg(target_os = "linux")]
            Self::X11(b) => b.button(button, down).await,
            #[cfg(target_os = "linux")]
            Self::Portal(b) => b.button(button, down).await,
            #[cfg(target_os = "windows")]
            Self::Windows(b) => b.button(button, down).await,
            #[cfg(target_os = "macos")]
            Self::Mac(b) => b.button(button, down).await,
        }
    }
    /// Wheel notches; positive `dy` scrolls down, positive `dx` right.
    pub(crate) async fn scroll(&self, dx: i32, dy: i32) -> Result<()> {
        match self {
            Self::Fake(b) => b.scroll(dx, dy),
            #[cfg(target_os = "linux")]
            Self::X11(b) => b.scroll(dx, dy).await,
            #[cfg(target_os = "linux")]
            Self::Portal(b) => b.scroll(dx, dy).await,
            #[cfg(target_os = "windows")]
            Self::Windows(b) => b.scroll(dx, dy).await,
            #[cfg(target_os = "macos")]
            Self::Mac(b) => b.scroll(dx, dy).await,
        }
    }
    pub(crate) async fn key(&self, keysym: u32, down: bool) -> Result<()> {
        match self {
            Self::Fake(b) => b.key(keysym, down),
            #[cfg(target_os = "linux")]
            Self::X11(b) => b.key(keysym, down).await,
            #[cfg(target_os = "linux")]
            Self::Portal(b) => b.key(keysym, down).await,
            #[cfg(target_os = "windows")]
            Self::Windows(b) => b.key(keysym, down).await,
            #[cfg(target_os = "macos")]
            Self::Mac(b) => b.key(keysym, down).await,
        }
    }
    pub(crate) async fn close(&self) {
        match self {
            #[cfg(target_os = "linux")]
            Self::Portal(b) => b.close().await,
            _ => {}
        }
    }
}

/// A desktop that does nothing but remember what it was asked, for the tests of a host that embeds
/// these tools (no screen, pointer or keyboard is involved). Not part of the supported API.
#[doc(hidden)]
#[derive(Clone)]
pub struct ScriptedDesktop(Arc<fake::Fake>);

impl ScriptedDesktop {
    /// What was done so far, such as `move 10 20`, `button Left down`, `key 0x61 down`.
    pub fn events(&self) -> Vec<String> {
        self.0.events()
    }
}

/// What is held down right now, so a failed or cancelled action can let go of it.
#[derive(Default)]
struct Held {
    keys: Vec<u32>,
    buttons: Vec<Button>,
}

pub(crate) struct State {
    backend: tokio::sync::Mutex<Option<Arc<Backend>>>,
    actions: AtomicU32,
    stopped: AtomicBool,
    /// Wakes an action already running when the host stops.
    stop_signal: tokio::sync::watch::Sender<bool>,
    held: Mutex<Held>,
    /// Tests inject a backend here instead of connecting to a desktop.
    pub(crate) preset: Mutex<Option<Arc<Backend>>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            backend: Default::default(),
            actions: Default::default(),
            stopped: Default::default(),
            stop_signal: tokio::sync::watch::channel(false).0,
            held: Default::default(),
            preset: Default::default(),
        }
    }
}

impl State {
    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.stop_signal.send_replace(true);
    }
    pub(crate) fn resume(&self) {
        self.stopped.store(false, Ordering::SeqCst);
        self.stop_signal.send_replace(false);
    }
    pub(crate) fn reset(&self) {
        self.actions.store(0, Ordering::SeqCst);
    }
    pub(crate) fn status(&self) -> Value {
        json!({"stopped":self.stopped.load(Ordering::SeqCst),"actions":self.actions.load(Ordering::SeqCst)})
    }
    pub(crate) async fn shutdown(&self) {
        let backend = self.backend.lock().await.take();
        if let Some(backend) = backend {
            release_all(&self.held, &backend).await;
            backend.close().await;
        }
    }
}

async fn release_all(held: &Mutex<Held>, backend: &Backend) {
    let (keys, buttons) = match held.lock() {
        Ok(mut held) => (
            std::mem::take(&mut held.keys),
            std::mem::take(&mut held.buttons),
        ),
        Err(_) => return,
    };
    for key in keys.into_iter().rev() {
        let _ = backend.key(key, false).await;
    }
    for button in buttons.into_iter().rev() {
        let _ = backend.button(button, false).await;
    }
}

/// Make `state` use a scripted desktop of this size instead of connecting to a real one.
pub(super) fn attach_scripted(state: &State, size: (u32, u32)) -> ScriptedDesktop {
    let fake = Arc::new(fake::Fake::new(size));
    if let Ok(mut preset) = state.preset.lock() {
        *preset = Some(Arc::new(Backend::Fake(fake.clone())));
    }
    ScriptedDesktop(fake)
}

pub(super) fn specs(
    policy: &ComputerPolicy,
) -> Vec<(&'static str, &'static str, Value, Vec<&'static str>)> {
    let coord = json!({"type":"integer","minimum":0,"maximum":8192});
    let mut tools = vec![];
    if policy.allow_screenshot {
        tools.push((
            "computer_screenshot",
            "Capture the screen. Coordinates in every other computer_* tool are pixels of THIS image (width and height are in the result). With region {x,y,width,height} (pixels of a previous screenshot) returns that area at higher detail. With path the image is saved inside the sandbox root instead of returned inline.",
            json!({"region":{"type":"object","additionalProperties":false,"required":["x","y","width","height"],
                    "properties":{"x":coord,"y":coord,"width":{"type":"integer","minimum":1,"maximum":8192},"height":{"type":"integer","minimum":1,"maximum":8192}}},
                "path":{"type":"string","maxLength":4096},"create_dirs":{"type":"boolean"}}),
            vec![],
        ));
    }
    if policy.allow_input {
        let button = json!({"type":"string","enum":["left","middle","right"]});
        tools.extend([
            ("computer_mouse_move", "Move the pointer to (x, y) of the last screenshot.",
                json!({"x":coord,"y":coord}), vec!["x", "y"]),
            ("computer_click", "Click at (x, y) of the last screenshot, or at the pointer when they are omitted. count 2 is a double click.",
                json!({"x":coord,"y":coord,"button":button,"count":{"type":"integer","minimum":1,"maximum":3}}), vec![]),
            ("computer_drag", "Press at the start point, move to the end point and release.",
                json!({"from_x":coord,"from_y":coord,"to_x":coord,"to_y":coord,"button":button}),
                vec!["from_x", "from_y", "to_x", "to_y"]),
            ("computer_scroll", "Scroll the wheel by notches (positive dy scrolls down, positive dx right), at (x, y) when given.",
                json!({"x":coord,"y":coord,"dx":{"type":"integer","minimum":-50,"maximum":50},"dy":{"type":"integer","minimum":-50,"maximum":50}}), vec![]),
            ("computer_type", "Type text as keyboard input into the focused window.",
                json!({"text":{"type":"string","minLength":1,"maxLength":20000}}), vec!["text"]),
            ("computer_key", "Press a key or a combination such as \"Return\", \"ctrl+c\", \"alt+Tab\", \"ctrl+shift+t\", \"F5\". Modifiers are held while the last key is pressed.",
                json!({"keys":{"type":"string","minLength":1,"maxLength":128},"count":{"type":"integer","minimum":1,"maximum":20}}), vec!["keys"]),
        ]);
    }
    tools.push((
        "computer_wait",
        "Wait for the screen to settle (at most 10 seconds).",
        json!({"ms":{"type":"integer","minimum":1,"maximum":10000}}),
        vec!["ms"],
    ));
    tools
}

/// Screen size → the size the model sees: scaled down to fit `max_dimension`, never up.
pub(crate) fn view_size(screen: (u32, u32), max_dimension: u32) -> (u32, u32) {
    let longest = screen.0.max(screen.1).max(1);
    if longest <= max_dimension {
        return screen;
    }
    let scale = f64::from(max_dimension) / f64::from(longest);
    (
        ((f64::from(screen.0) * scale).round() as u32).max(1),
        ((f64::from(screen.1) * scale).round() as u32).max(1),
    )
}

/// A point of the model's view → the screen. Rejects points outside the view.
pub(crate) fn to_screen(
    point: (u32, u32),
    view: (u32, u32),
    screen: (u32, u32),
) -> Result<(u32, u32)> {
    if point.0 >= view.0 || point.1 >= view.1 {
        return Err(invalid(format!(
            "({}, {}) is outside the screenshot, which is {}x{}",
            point.0, point.1, view.0, view.1
        )));
    }
    let scale = |value: u32, from: u32, to: u32| -> u32 {
        ((u64::from(value) * 2 + 1) * u64::from(to) / (u64::from(from) * 2)).min(u64::from(to) - 1)
            as u32
    };
    Ok((
        scale(point.0, view.0, screen.0),
        scale(point.1, view.1, screen.1),
    ))
}

async fn connect(inner: &Inner, policy: &ComputerPolicy) -> Result<Arc<Backend>> {
    if let Some(backend) = inner
        .computer_state
        .preset
        .lock()
        .ok()
        .and_then(|p| p.clone())
    {
        return Ok(backend);
    }
    let _ = inner;
    #[cfg(target_os = "linux")]
    {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty());
        let backend = match policy.backend {
            ComputerBackend::X11 => ComputerBackend::X11,
            ComputerBackend::Portal => ComputerBackend::Portal,
            ComputerBackend::Auto if wayland => ComputerBackend::Portal,
            ComputerBackend::Auto => ComputerBackend::X11,
        };
        return Ok(Arc::new(match backend {
            ComputerBackend::Portal => Backend::Portal(portal::Portal::connect(policy).await?),
            _ => Backend::X11(x11::X11::connect(policy)?),
        }));
    }
    #[cfg(target_os = "windows")]
    {
        let _ = policy;
        return Ok(Arc::new(Backend::Windows(win::Windows::connect()?)));
    }
    #[cfg(target_os = "macos")]
    {
        let _ = policy;
        return Ok(Arc::new(Backend::Mac(mac::Mac::connect()?)));
    }
    #[allow(unreachable_code)]
    {
        let _ = policy;
        Err(invalid("computer use is not available on this platform"))
    }
}

fn int(arguments: &Value, key: &str) -> Option<u32> {
    arguments[key]
        .as_u64()
        .map(|v| v.min(u64::from(u32::MAX)) as u32)
}

pub(super) async fn run(
    inner: &Arc<Inner>,
    name: &str,
    arguments: &Value,
    cancel: &CancellationToken,
) -> Result<Value> {
    let policy = inner
        .computer
        .as_ref()
        .ok_or_else(|| invalid("computer tools are disabled"))?;
    let state = &inner.computer_state;
    if state.stopped.load(Ordering::SeqCst) {
        return Err(Error::State(
            "computer use was stopped by the host; no action was performed".into(),
        ));
    }
    if name == "computer_wait" {
        let ms = int(arguments, "ms").unwrap_or(1).min(10_000);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Error::Cancelled),
            _ = tokio::time::sleep(Duration::from_millis(u64::from(ms))) => {}
        }
        return Ok(json!({"ok":true,"waited_ms":ms}));
    }
    let is_input = name != "computer_screenshot";
    if (is_input && !policy.allow_input) || (!is_input && !policy.allow_screenshot) {
        return Err(invalid("that computer tool was not granted"));
    }
    // The budget is charged when the action starts: an action that fails midway may still have had effects.
    if state.actions.fetch_add(1, Ordering::SeqCst) >= policy.max_actions {
        state.actions.fetch_sub(1, Ordering::SeqCst);
        return Err(Error::State(format!(
            "the computer action budget ({}) is exhausted; the host must reset it",
            policy.max_actions
        )));
    }
    let backend = {
        let mut slot = state.backend.lock().await;
        match slot.as_ref() {
            Some(backend) => backend.clone(),
            None => {
                let connected = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(Error::Cancelled),
                    r = tokio::time::timeout(Duration::from_millis(policy.start_timeout_ms), connect(inner, policy)) => r.map_err(|_| Error::Timeout)??,
                };
                *slot = Some(connected.clone());
                connected
            }
        }
    };
    let mut stopped = state.stop_signal.subscribe();
    let outcome = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Error::Cancelled),
        _ = stopped.wait_for(|stopped| *stopped) => Err(Error::Cancelled),
        r = tokio::time::timeout(Duration::from_millis(policy.action_timeout_ms), act(inner, &backend, name, arguments)) => r.unwrap_or(Err(Error::Timeout)),
    };
    if outcome.is_err() {
        // Whatever was held down when it stopped is let go; an input action's effect is unknown.
        release_all(&state.held, &backend).await;
        if is_input && matches!(outcome, Err(Error::Timeout | Error::Cancelled)) {
            return Err(Error::State(
                "COMPUTER_ACTION_UNCERTAIN: the action was interrupted and may have been partly performed; check the screen before continuing, and do not repeat it blindly".into(),
            ));
        }
    }
    outcome
}

async fn act(
    inner: &Arc<Inner>,
    backend: &Arc<Backend>,
    name: &str,
    arguments: &Value,
) -> Result<Value> {
    let policy = inner.computer.as_ref().expect("enabled");
    let held = &inner.computer_state.held;
    let screen = backend.size().await?;
    let view = view_size(screen, policy.max_dimension);
    let point = |x: &str, y: &str| -> Result<Option<(u32, u32)>> {
        match (int(arguments, x), int(arguments, y)) {
            (Some(px), Some(py)) => to_screen((px, py), view, screen).map(Some),
            (None, None) => Ok(None),
            _ => Err(invalid("give both x and y, or neither")),
        }
    };
    let button = match arguments["button"].as_str() {
        Some("middle") => Button::Middle,
        Some("right") => Button::Right,
        _ => Button::Left,
    };
    match name {
        "computer_screenshot" => screenshot(inner, backend, arguments, screen, view).await,
        "computer_mouse_move" => {
            let (x, y) = point("x", "y")?.ok_or_else(|| invalid("x and y are required"))?;
            backend.motion(x, y).await?;
            Ok(json!({"ok":true}))
        }
        "computer_click" => {
            if let Some((x, y)) = point("x", "y")? {
                backend.motion(x, y).await?;
            }
            for _ in 0..int(arguments, "count").unwrap_or(1).clamp(1, 3) {
                press(held, backend, button).await?;
                release(held, backend, button).await?;
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
            Ok(json!({"ok":true}))
        }
        "computer_drag" => {
            let from = point("from_x", "from_y")?
                .ok_or_else(|| invalid("from_x and from_y are required"))?;
            let to = point("to_x", "to_y")?.ok_or_else(|| invalid("to_x and to_y are required"))?;
            backend.motion(from.0, from.1).await?;
            press(held, backend, button).await?;
            for step in 1..=8u32 {
                let at = |a: u32, b: u32| {
                    (i64::from(a) + (i64::from(b) - i64::from(a)) * i64::from(step) / 8) as u32
                };
                backend.motion(at(from.0, to.0), at(from.1, to.1)).await?;
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
            release(held, backend, button).await?;
            Ok(json!({"ok":true}))
        }
        "computer_scroll" => {
            if let Some((x, y)) = point("x", "y")? {
                backend.motion(x, y).await?;
            }
            let (dx, dy) = (
                arguments["dx"].as_i64().unwrap_or(0) as i32,
                arguments["dy"].as_i64().unwrap_or(0) as i32,
            );
            if dx == 0 && dy == 0 {
                return Err(invalid("give dx or dy"));
            }
            backend.scroll(dx, dy).await?;
            Ok(json!({"ok":true}))
        }
        "computer_type" => {
            let text = arguments["text"].as_str().unwrap_or_default();
            if text.chars().count() > policy.max_text_chars {
                return Err(invalid(format!(
                    "text is longer than {} characters",
                    policy.max_text_chars
                )));
            }
            for ch in text.chars() {
                let (keysym, shift) = keys::for_char(ch);
                if shift {
                    hold(held, backend, keys::SHIFT_L).await?;
                }
                backend.key(keysym, true).await?;
                backend.key(keysym, false).await?;
                if shift {
                    let _ = lift(held, backend, keys::SHIFT_L).await;
                }
                tokio::time::sleep(Duration::from_millis(4)).await;
            }
            Ok(json!({"ok":true,"typed_chars":text.chars().count()}))
        }
        "computer_key" => {
            let combo = keys::parse_combo(arguments["keys"].as_str().unwrap_or_default())?;
            let (last, modifiers) = combo.split_last().expect("non-empty combo");
            for _ in 0..int(arguments, "count").unwrap_or(1).clamp(1, 20) {
                for modifier in modifiers {
                    hold(held, backend, *modifier).await?;
                }
                backend.key(*last, true).await?;
                backend.key(*last, false).await?;
                for modifier in modifiers.iter().rev() {
                    lift(held, backend, *modifier).await?;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            Ok(json!({"ok":true}))
        }
        _ => Err(invalid("unknown computer tool")),
    }
}

async fn press(held: &Mutex<Held>, backend: &Backend, button: Button) -> Result<()> {
    held.lock()
        .map_err(|_| Error::State("input state poisoned".into()))?
        .buttons
        .push(button);
    backend.button(button, true).await
}
async fn release(held: &Mutex<Held>, backend: &Backend, button: Button) -> Result<()> {
    backend.button(button, false).await?;
    held.lock()
        .map_err(|_| Error::State("input state poisoned".into()))?
        .buttons
        .retain(|b| *b != button);
    Ok(())
}
async fn hold(held: &Mutex<Held>, backend: &Backend, key: u32) -> Result<()> {
    held.lock()
        .map_err(|_| Error::State("input state poisoned".into()))?
        .keys
        .push(key);
    backend.key(key, true).await
}
async fn lift(held: &Mutex<Held>, backend: &Backend, key: u32) -> Result<()> {
    backend.key(key, false).await?;
    held.lock()
        .map_err(|_| Error::State("input state poisoned".into()))?
        .keys
        .retain(|k| *k != key);
    Ok(())
}

async fn screenshot(
    inner: &Arc<Inner>,
    backend: &Backend,
    arguments: &Value,
    screen: (u32, u32),
    view: (u32, u32),
) -> Result<Value> {
    let policy = inner.computer.as_ref().expect("enabled");
    let shot = backend.screenshot().await?;
    // The capture may be at another resolution than the pointer space (HiDPI); bring it to the
    // pointer space first so the view's coordinates and the pixels agree.
    let mut crop = None;
    let (mut max_w, mut max_h) = view;
    if let Some(region) = arguments["region"].as_object() {
        let get = |k: &str| region.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
        let (x, y, w, h) = (get("x"), get("y"), get("width"), get("height"));
        if x.checked_add(w).is_none_or(|v| v > view.0)
            || y.checked_add(h).is_none_or(|v| v > view.1)
        {
            return Err(invalid(format!(
                "region is outside the screenshot, which is {}x{}",
                view.0, view.1
            )));
        }
        let (sx, sy) = (
            f64::from(screen.0) / f64::from(view.0),
            f64::from(screen.1) / f64::from(view.1),
        );
        crop = Some((
            (f64::from(x) * sx).round() as u32,
            (f64::from(y) * sy).round() as u32,
            ((f64::from(w) * sx).round() as u32).max(1),
            ((f64::from(h) * sy).round() as u32).max(1),
        ));
        (max_w, max_h) = (policy.max_dimension, policy.max_dimension);
    }
    let captured = crate::image::info(&shot.png)
        .ok_or_else(|| Error::State("the desktop returned an unreadable image".into()))?;
    let mut base = shot.png;
    if (captured.width, captured.height) != screen {
        base = image_transform::apply(
            &base,
            &image_transform::Transform {
                format: Some(image_transform::Format::Png),
                max_width: Some(screen.0),
                max_height: Some(screen.1),
                ..Default::default()
            },
        )?
        .bytes;
    }
    let max_output = inner.max_output;
    let save_to = arguments["path"].as_str();
    // PNG first (exact text, UI edges); smaller JPEGs and sizes only if it does not fit inline.
    let mut candidates: Vec<(image_transform::Format, u8, f64)> =
        vec![(image_transform::Format::Png, 0, 1.0)];
    for scale in [1.0, 0.75, 0.55] {
        for quality in [80u8, 55] {
            candidates.push((image_transform::Format::Jpeg, quality, scale));
        }
    }
    for (format, quality, scale) in candidates {
        let transformed = image_transform::apply(
            &base,
            &image_transform::Transform {
                crop: crop.map(|(x, y, width, height)| image_transform::Crop {
                    x,
                    y,
                    width,
                    height,
                }),
                max_width: Some(((f64::from(max_w) * scale).round() as u32).max(1)),
                max_height: Some(((f64::from(max_h) * scale).round() as u32).max(1)),
                format: Some(format),
                quality: (quality > 0).then_some(quality),
                ..Default::default()
            },
        )?;
        let mut result = json!({"mime_type":transformed.format.mime_type(),"width":transformed.width,"height":transformed.height,
            "bytes":transformed.bytes.len(),"screen":{"width":view.0,"height":view.1},"backend":backend.name()});
        if crop.is_some() {
            result["region_of_screenshot"] = arguments["region"].clone();
        }
        if let Some(path) = save_to {
            result["path"] = json!(fs::write_bytes(
                inner,
                path,
                &transformed.bytes,
                arguments["create_dirs"].as_bool() == Some(true)
            )?);
            return Ok(result);
        }
        let encoded = media::encode_base64(&transformed.bytes);
        if encoded.len() <= max_output {
            result["data"] = json!(encoded);
            return Ok(result);
        }
    }
    Err(invalid(
        "the screenshot does not fit in max_output_bytes; pass path to save it inside the sandbox root",
    ))
}

#[cfg(test)]
mod tests;
