//! macOS backend: Quartz events (`CGEvent`) for the pointer and keyboard, and `CGDisplay` for
//! screenshots of the main display.
//!
//! **Theoretical.** Written from the Core Graphics documentation and type-checked for
//! `aarch64-apple-darwin`, but never run on a Mac: treat every behavior below as unverified until it
//! has been tried on a real machine (see docs/external-testing.md).
//!
//! macOS asks the person for two permissions in System Settings → Privacy & Security, granted to the
//! application that hosts this code (a terminal, an editor, your app): **Screen Recording** for
//! screenshots and **Accessibility** for the pointer and keyboard. Without them the system drops the
//! events or returns a blank image *without an error*, so this backend checks both up front and says
//! which one is missing. Coordinates are in points on the main display (a Retina capture has more
//! pixels than points; the tool layer scales it back).
use super::{Button, Shot, encode_png, keys};
use crate::{Error, Result};
use core_graphics::{
    access::ScreenCaptureAccess,
    display::CGDisplay,
    event::{
        CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGMouseButton, ScrollEventUnit,
    },
    event_source::{CGEventSource, CGEventSourceStateID},
    geometry::CGPoint,
};
use std::sync::Mutex;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Whether this process may control the computer (the Accessibility permission).
    fn AXIsProcessTrusted() -> bool;
}

fn err(message: impl std::fmt::Display) -> Error {
    Error::State(format!("macos: {message}"))
}

#[derive(Default)]
struct State {
    position: (f64, f64),
    /// Buttons held down: while one is, a move is a *drag* event, as the system expects.
    left: bool,
    right: bool,
    middle: bool,
    /// Modifier flags held by `key` calls, applied to every event until released.
    flags: u64,
}

pub(crate) struct Mac {
    state: Mutex<State>,
}

impl Mac {
    pub(super) fn connect() -> Result<Self> {
        // SAFETY: no arguments, no side effects.
        if !unsafe { AXIsProcessTrusted() } {
            return Err(err(
                "this application is not allowed to control the computer: enable it under System Settings → Privacy & Security → Accessibility",
            ));
        }
        let this = Self {
            state: Mutex::default(),
        };
        this.size()?;
        Ok(this)
    }

    pub(super) fn size(&self) -> Result<(u32, u32)> {
        let bounds = CGDisplay::main().bounds();
        let (w, h) = (bounds.size.width, bounds.size.height);
        if w < 1.0 || h < 1.0 {
            return Err(err("no main display"));
        }
        Ok((w as u32, h as u32))
    }

    pub(super) async fn screenshot(&self) -> Result<Shot> {
        tokio::task::spawn_blocking(|| {
            let access = ScreenCaptureAccess;
            if !access.preflight() {
                // Asks the system to show its prompt, then says what to do; the answer applies after a restart of the app.
                access.request();
                return Err(err(
                    "screen capture is not allowed: enable it under System Settings → Privacy & Security → Screen Recording, then restart the application",
                ));
            }
            let image = CGDisplay::main().image().ok_or_else(|| err("the display could not be captured"))?;
            let (w, h, bits, stride) = (image.width(), image.height(), image.bits_per_pixel(), image.bytes_per_row());
            if bits != 32 || w == 0 || h == 0 {
                return Err(err(format!("unsupported pixel format ({bits} bits per pixel)")));
            }
            let data = image.data();
            let bytes = data.bytes();
            if bytes.len() < stride * h {
                return Err(err("short image"));
            }
            // Display captures are 32-bit BGRA (little-endian, alpha first): B, G, R, A in memory.
            let mut rgb = Vec::with_capacity(w * h * 3);
            for row in 0..h {
                for pixel in bytes[row * stride..row * stride + w * 4].as_chunks::<4>().0 {
                    rgb.extend([pixel[2], pixel[1], pixel[0]]);
                }
            }
            Ok(Shot { png: encode_png(&rgb, w as u32, h as u32)? })
        })
        .await
        .map_err(|_| err("screenshot task failed"))?
    }

    fn source() -> Result<CGEventSource> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .map_err(|()| err("could not create an event source"))
    }

    pub(super) async fn motion(&self, x: u32, y: u32) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| err("state poisoned"))?;
        state.position = (f64::from(x), f64::from(y));
        let kind = if state.left {
            CGEventType::LeftMouseDragged
        } else if state.right {
            CGEventType::RightMouseDragged
        } else if state.middle {
            CGEventType::OtherMouseDragged
        } else {
            CGEventType::MouseMoved
        };
        let button = if state.right {
            CGMouseButton::Right
        } else if state.middle {
            CGMouseButton::Center
        } else {
            CGMouseButton::Left
        };
        let event = CGEvent::new_mouse_event(
            Self::source()?,
            kind,
            CGPoint::new(state.position.0, state.position.1),
            button,
        )
        .map_err(|()| err("could not create a mouse event"))?;
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    pub(super) async fn button(&self, button: Button, down: bool) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| err("state poisoned"))?;
        let (kind, which) = match (button, down) {
            (Button::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
            (Button::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
            (Button::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
            (Button::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
            (Button::Middle, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
            (Button::Middle, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
        };
        match button {
            Button::Left => state.left = down,
            Button::Right => state.right = down,
            Button::Middle => state.middle = down,
        }
        let event = CGEvent::new_mouse_event(
            Self::source()?,
            kind,
            CGPoint::new(state.position.0, state.position.1),
            which,
        )
        .map_err(|()| err("could not create a mouse event"))?;
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    pub(super) async fn scroll(&self, dx: i32, dy: i32) -> Result<()> {
        // Positive wheel values scroll up and left; ours are down and right.
        let event =
            CGEvent::new_scroll_event(Self::source()?, ScrollEventUnit::LINE, 2, -dy, -dx, 0)
                .map_err(|()| err("could not create a scroll event"))?;
        event.post(CGEventTapLocation::HID);
        Ok(())
    }

    pub(super) async fn key(&self, keysym: u32, down: bool) -> Result<()> {
        let mut state = self.state.lock().map_err(|_| err("state poisoned"))?;
        // Modifiers are kept as flags and applied to the events that follow.
        let modifier = match keysym {
            0xffe1 => Some((CGEventFlags::CGEventFlagShift.bits(), 56u16)),
            keys::CONTROL_L => Some((CGEventFlags::CGEventFlagControl.bits(), 59)),
            keys::ALT_L => Some((CGEventFlags::CGEventFlagAlternate.bits(), 58)),
            keys::SUPER_L => Some((CGEventFlags::CGEventFlagCommand.bits(), 55)),
            _ => None,
        };
        if let Some((flag, code)) = modifier {
            if down {
                state.flags |= flag;
            } else {
                state.flags &= !flag;
            }
            let event = CGEvent::new_keyboard_event(Self::source()?, code, down)
                .map_err(|()| err("could not create a key event"))?;
            event.set_flags(CGEventFlags::from_bits_truncate(state.flags));
            event.post(CGEventTapLocation::HID);
            return Ok(());
        }
        let flags = CGEventFlags::from_bits_truncate(state.flags);
        if let Some(code) = keycode(keysym) {
            let event = CGEvent::new_keyboard_event(Self::source()?, code, down)
                .map_err(|()| err("could not create a key event"))?;
            event.set_flags(flags);
            event.post(CGEventTapLocation::HID);
            return Ok(());
        }
        // A character without a fixed key (an accent, an emoji): typed as text, on the press only.
        let Some(ch) = keys::to_char(keysym) else {
            return Err(err("that key is not supported"));
        };
        if down {
            let event = CGEvent::new_keyboard_event(Self::source()?, 0, true)
                .map_err(|()| err("could not create a key event"))?;
            event.set_string(&ch.to_string());
            event.set_flags(flags);
            event.post(CGEventTapLocation::HID);
            let up = CGEvent::new_keyboard_event(Self::source()?, 0, false)
                .map_err(|()| err("could not create a key event"))?;
            up.set_string(&ch.to_string());
            up.post(CGEventTapLocation::HID);
        }
        Ok(())
    }
}

/// The virtual key code (ANSI layout) of an X keysym: named keys, then letters, digits and the
/// punctuation keys. Codes are positions on a US keyboard, not characters.
fn keycode(keysym: u32) -> Option<u16> {
    Some(match keysym {
        0xff0d => 36,  // Return
        0xff09 => 48,  // Tab
        0x20 => 49,    // Space
        0xff08 => 51,  // Delete (backspace)
        0xff1b => 53,  // Escape
        0xffff => 117, // Forward delete
        0xff50 => 115, // Home
        0xff57 => 119, // End
        0xff55 => 116, // Page up
        0xff56 => 121, // Page down
        0xff51 => 123, // Left
        0xff53 => 124, // Right
        0xff54 => 125, // Down
        0xff52 => 126, // Up
        0xffbe..=0xffc9 => {
            [122, 120, 99, 118, 96, 97, 98, 100, 101, 109, 103, 111][(keysym - 0xffbe) as usize]
        } // F1..F12
        _ => {
            let ch = keys::to_char(keysym)?.to_ascii_lowercase();
            match ch {
                'a' => 0,
                's' => 1,
                'd' => 2,
                'f' => 3,
                'h' => 4,
                'g' => 5,
                'z' => 6,
                'x' => 7,
                'c' => 8,
                'v' => 9,
                'b' => 11,
                'q' => 12,
                'w' => 13,
                'e' => 14,
                'r' => 15,
                'y' => 16,
                't' => 17,
                '1' => 18,
                '2' => 19,
                '3' => 20,
                '4' => 21,
                '6' => 22,
                '5' => 23,
                '=' => 24,
                '9' => 25,
                '7' => 26,
                '-' => 27,
                '8' => 28,
                '0' => 29,
                ']' => 30,
                'o' => 31,
                'u' => 32,
                '[' => 33,
                'i' => 34,
                'p' => 35,
                'l' => 37,
                'j' => 38,
                '\'' => 39,
                'k' => 40,
                ';' => 41,
                '\\' => 42,
                ',' => 43,
                '/' => 44,
                'n' => 45,
                'm' => 46,
                '.' => 47,
                '`' => 50,
                _ => return None,
            }
        }
    })
}
