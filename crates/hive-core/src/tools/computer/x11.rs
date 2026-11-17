//! X11 backend: screenshots with `GetImage` and input with the XTEST extension.
//!
//! This sees and drives the X server it connects to. Under a Wayland session that is XWayland,
//! which only holds the X11 applications, so Wayland sessions use the portal backend instead.
use super::{Button, ComputerPolicy, Shot, encode_png, keys};
use crate::{Error, Result};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use x11rb::{
    connection::Connection,
    protocol::{
        xproto::{self, ConnectionExt as _, ImageFormat, ImageOrder},
        xtest::ConnectionExt as _,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

fn err(error: impl std::fmt::Display) -> Error {
    Error::State(format!("x11: {error}"))
}

struct Keymap {
    first: u8,
    per: usize,
    syms: Vec<u32>,
}

pub(crate) struct X11 {
    conn: Arc<RustConnection>,
    root: u32,
    keymap: Mutex<Keymap>,
    /// Shift keycodes pressed on behalf of a key that needed them: (keysym, shift keycode).
    shifted: Mutex<Vec<(u32, u8)>>,
}

impl X11 {
    pub(super) fn connect(policy: &ComputerPolicy) -> Result<Self> {
        let (conn, screen) = RustConnection::connect(policy.display.as_deref()).map_err(err)?;
        let root = conn.setup().roots[screen].root;
        conn.xtest_get_version(2, 2)
            .map_err(err)?
            .reply()
            .map_err(|_| err("the server has no XTEST extension, so it cannot be driven"))?;
        let (min, max) = (conn.setup().min_keycode, conn.setup().max_keycode);
        let map = conn
            .get_keyboard_mapping(min, max - min + 1)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        Ok(Self {
            conn: Arc::new(conn),
            root,
            keymap: Mutex::new(Keymap {
                first: min,
                per: usize::from(map.keysyms_per_keycode),
                syms: map.keysyms,
            }),
            shifted: Mutex::default(),
        })
    }

    pub(super) fn size(&self) -> Result<(u32, u32)> {
        let geometry = self
            .conn
            .get_geometry(self.root)
            .map_err(err)?
            .reply()
            .map_err(err)?;
        Ok((u32::from(geometry.width), u32::from(geometry.height)))
    }

    pub(super) async fn screenshot(&self) -> Result<Shot> {
        let conn = self.conn.clone();
        let root = self.root;
        tokio::task::spawn_blocking(move || -> Result<Shot> {
            let geometry = conn.get_geometry(root).map_err(err)?.reply().map_err(err)?;
            let (w, h) = (geometry.width, geometry.height);
            let image = conn
                .get_image(ImageFormat::Z_PIXMAP, root, 0, 0, w, h, !0)
                .map_err(err)?
                .reply()
                .map_err(err)?;
            let setup = conn.setup();
            let bits = setup
                .pixmap_formats
                .iter()
                .find(|f| f.depth == image.depth)
                .map_or(0, |f| f.bits_per_pixel);
            if bits != 32 || setup.image_byte_order != ImageOrder::LSB_FIRST {
                return Err(err(format!(
                    "unsupported pixel format ({bits} bits per pixel)"
                )));
            }
            let (w, h) = (usize::from(w), usize::from(h));
            if image.data.len() < w * h * 4 {
                return Err(err("short image"));
            }
            let mut rgb = Vec::with_capacity(w * h * 3);
            for pixel in image.data.as_chunks::<4>().0.iter().take(w * h) {
                rgb.extend([pixel[2], pixel[1], pixel[0]]);
            }
            Ok(Shot {
                png: encode_png(&rgb, w as u32, h as u32)?,
            })
        })
        .await
        .map_err(|_| err("screenshot task failed"))?
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<()> {
        self.conn
            .xtest_fake_input(kind, detail, 0, self.root, x, y, 0)
            .map_err(err)?;
        self.conn.flush().map_err(err)
    }

    pub(super) async fn motion(&self, x: u32, y: u32) -> Result<()> {
        self.fake(
            xproto::MOTION_NOTIFY_EVENT,
            0,
            x.min(32767) as i16,
            y.min(32767) as i16,
        )
    }

    pub(super) async fn button(&self, button: Button, down: bool) -> Result<()> {
        let detail = match button {
            Button::Left => 1,
            Button::Middle => 2,
            Button::Right => 3,
        };
        self.fake(
            if down {
                xproto::BUTTON_PRESS_EVENT
            } else {
                xproto::BUTTON_RELEASE_EVENT
            },
            detail,
            0,
            0,
        )
    }

    pub(super) async fn scroll(&self, dx: i32, dy: i32) -> Result<()> {
        for (amount, negative, positive) in [(dy, 4u8, 5u8), (dx, 6, 7)] {
            let button = if amount < 0 { negative } else { positive };
            for _ in 0..amount.unsigned_abs() {
                self.fake(xproto::BUTTON_PRESS_EVENT, button, 0, 0)?;
                self.fake(xproto::BUTTON_RELEASE_EVENT, button, 0, 0)?;
            }
        }
        Ok(())
    }

    /// `(keycode, needs_shift)` for a keysym from the current keymap, if any key produces it.
    fn lookup(&self, keysym: u32) -> Option<(u8, bool)> {
        let map = self.keymap.lock().ok()?;
        for (index, row) in map.syms.chunks(map.per.max(1)).enumerate() {
            let code = map.first.checked_add(u8::try_from(index).ok()?)?;
            if row.first() == Some(&keysym) {
                return Some((code, false));
            }
            if row.get(1) == Some(&keysym) {
                return Some((code, true));
            }
        }
        None
    }

    pub(super) async fn key(&self, keysym: u32, down: bool) -> Result<()> {
        let (code, shift) = match self.lookup(keysym) {
            Some(found) => found,
            None => (self.remap_spare(keysym).await?, false),
        };
        if down {
            if shift {
                let (shift_code, _) = self
                    .lookup(keys::SHIFT_L)
                    .ok_or_else(|| err("no Shift key in the keymap"))?;
                self.fake(xproto::KEY_PRESS_EVENT, shift_code, 0, 0)?;
                self.shifted
                    .lock()
                    .map_err(|_| err("state poisoned"))?
                    .push((keysym, shift_code));
            }
            self.fake(xproto::KEY_PRESS_EVENT, code, 0, 0)
        } else {
            self.fake(xproto::KEY_RELEASE_EVENT, code, 0, 0)?;
            let released = {
                let mut shifted = self.shifted.lock().map_err(|_| err("state poisoned"))?;
                shifted
                    .iter()
                    .position(|(sym, _)| *sym == keysym)
                    .map(|i| shifted.remove(i))
            };
            if let Some((_, shift_code)) = released {
                self.fake(xproto::KEY_RELEASE_EVENT, shift_code, 0, 0)?;
            }
            Ok(())
        }
    }

    /// A keysym no key produces (an emoji, an accent the layout lacks): borrow an unused keycode,
    /// map it to the keysym and use it. It stays mapped (harmless, and the next use finds it).
    async fn remap_spare(&self, keysym: u32) -> Result<u8> {
        let (code, per) = {
            let map = self.keymap.lock().map_err(|_| err("state poisoned"))?;
            let index = map
                .syms
                .chunks(map.per.max(1))
                .rposition(|row| row.iter().all(|s| *s == 0))
                .ok_or_else(|| err("no free keycode to type this character"))?;
            (map.first + index as u8, map.per.max(1))
        };
        self.conn
            .change_keyboard_mapping(1, code, per as u8, &vec![keysym; per])
            .map_err(err)?;
        self.conn.sync().map_err(err)?;
        if let Ok(mut map) = self.keymap.lock() {
            let at = usize::from(code - map.first) * per;
            for slot in &mut map.syms[at..at + per] {
                *slot = keysym;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok(code)
    }
}
