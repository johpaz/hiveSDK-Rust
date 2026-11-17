//! Windows backend: `SendInput` for the pointer and keyboard and GDI for screenshots.
//!
//! **Theoretical.** Written from the Win32 documentation and type-checked for
//! `x86_64-pc-windows-msvc`, but never run on Windows: treat every behavior below as unverified until
//! it has been tried on a real machine (see docs/external-testing.md).
//!
//! The pointer space and the screenshot are the whole **virtual desktop** (every monitor), in
//! physical pixels (the process declares itself DPI aware). Input to a window running as
//! administrator is ignored by Windows (UIPI) unless this process is elevated too.
use super::{Button, Shot, encode_png};
use crate::{Error, Result};
use std::mem::{size_of, zeroed};
use windows_sys::Win32::{
    Foundation::HWND,
    Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleBitmap,
        CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, ReleaseDC,
        SRCCOPY, SelectObject,
    },
    UI::{
        Input::KeyboardAndMouse::{
            INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
            KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
            MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
            MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
            MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput, VK_BACK, VK_CAPITAL, VK_CONTROL, VK_DELETE,
            VK_DOWN, VK_END, VK_ESCAPE, VK_F1, VK_HOME, VK_INSERT, VK_LEFT, VK_LWIN, VK_MENU,
            VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SNAPSHOT, VK_TAB, VK_UP,
            VkKeyScanW,
        },
        WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN, SetProcessDPIAware,
        },
    },
};

fn err(message: impl std::fmt::Display) -> Error {
    Error::State(format!("windows: {message}"))
}

pub(crate) struct Windows {
    /// Top-left of the virtual desktop (negative when a monitor sits left of or above the primary).
    origin: (i32, i32),
}

impl Windows {
    pub(super) fn connect() -> Result<Self> {
        // Without this Windows scales coordinates and captures for the display's DPI setting.
        // SAFETY: no arguments; it only changes this process's DPI awareness.
        unsafe { SetProcessDPIAware() };
        let this = Self {
            // SAFETY: GetSystemMetrics has no preconditions.
            origin: unsafe {
                (
                    GetSystemMetrics(SM_XVIRTUALSCREEN),
                    GetSystemMetrics(SM_YVIRTUALSCREEN),
                )
            },
        };
        this.size()?;
        Ok(this)
    }

    pub(super) fn size(&self) -> Result<(u32, u32)> {
        // SAFETY: GetSystemMetrics has no preconditions.
        let (w, h) = unsafe {
            (
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        if w <= 0 || h <= 0 {
            return Err(err(
                "no desktop to capture (is this a service without a session?)",
            ));
        }
        Ok((w as u32, h as u32))
    }

    pub(super) async fn screenshot(&self) -> Result<Shot> {
        let (w, h) = self.size()?;
        let origin = self.origin;
        tokio::task::spawn_blocking(move || capture(origin, w as i32, h as i32))
            .await
            .map_err(|_| err("screenshot task failed"))?
    }

    fn send(&self, inputs: &[INPUT]) -> Result<()> {
        // SAFETY: `inputs` is a valid slice of INPUT structures and the size is that of one.
        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                size_of::<INPUT>() as i32,
            )
        };
        if sent as usize == inputs.len() {
            Ok(())
        } else {
            Err(err(
                "SendInput was blocked (a window of higher integrity, or a locked desktop)",
            ))
        }
    }

    fn mouse(&self, flags: u32, dx: i32, dy: i32, data: i32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data as u32,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    pub(super) async fn motion(&self, x: u32, y: u32) -> Result<()> {
        let (w, h) = self.size()?;
        // Absolute coordinates are 0..=65535 across the virtual desktop.
        let scale = |value: u32, extent: u32| -> i32 {
            ((u64::from(value) * 65_535 + u64::from(extent) / 2)
                / u64::from(extent.saturating_sub(1).max(1)))
            .min(65_535) as i32
        };
        self.send(&[self.mouse(
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            scale(x, w),
            scale(y, h),
            0,
        )])
    }

    pub(super) async fn button(&self, button: Button, down: bool) -> Result<()> {
        let flags = match (button, down) {
            (Button::Left, true) => MOUSEEVENTF_LEFTDOWN,
            (Button::Left, false) => MOUSEEVENTF_LEFTUP,
            (Button::Right, true) => MOUSEEVENTF_RIGHTDOWN,
            (Button::Right, false) => MOUSEEVENTF_RIGHTUP,
            (Button::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
            (Button::Middle, false) => MOUSEEVENTF_MIDDLEUP,
        };
        self.send(&[self.mouse(flags, 0, 0, 0)])
    }

    pub(super) async fn scroll(&self, dx: i32, dy: i32) -> Result<()> {
        // One notch is 120 units; the wheel reports positive away from the user (up), ours down.
        let mut inputs = vec![];
        if dy != 0 {
            inputs.push(self.mouse(MOUSEEVENTF_WHEEL, 0, 0, -dy * 120));
        }
        if dx != 0 {
            inputs.push(self.mouse(MOUSEEVENTF_HWHEEL, 0, 0, dx * 120));
        }
        self.send(&inputs)
    }

    fn keyboard(&self, vk: u16, scan: u16, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    pub(super) async fn key(&self, keysym: u32, down: bool) -> Result<()> {
        let up = if down { 0 } else { KEYEVENTF_KEYUP };
        if let Some((vk, extended)) = named_key(keysym) {
            return self.send(&[self.keyboard(
                vk,
                0,
                up | if extended { KEYEVENTF_EXTENDEDKEY } else { 0 },
            )]);
        }
        let Some(ch) = super::keys::to_char(keysym) else {
            return Err(err("that key is not supported"));
        };
        let mut utf16 = [0u16; 2];
        let units = ch.encode_utf16(&mut utf16);
        // A character the layout types with a key (letters, digits, punctuation) is sent as that key,
        // so shortcuts such as Ctrl+C work; Shift is added when the layout needs it.
        if units.len() == 1 {
            // SAFETY: VkKeyScanW takes a UTF-16 unit and has no other preconditions.
            let scanned = unsafe { VkKeyScanW(units[0]) };
            if scanned != -1 {
                let (vk, state) = ((scanned & 0xff) as u16, (scanned >> 8) & 0xff);
                let mut inputs = vec![];
                if down && state & 1 != 0 {
                    inputs.push(self.keyboard(VK_SHIFT, 0, 0));
                }
                inputs.push(self.keyboard(vk, 0, up));
                if !down && state & 1 != 0 {
                    inputs.push(self.keyboard(VK_SHIFT, 0, KEYEVENTF_KEYUP));
                }
                return self.send(&inputs);
            }
        }
        // Anything else (an emoji, a letter the layout lacks) is typed as Unicode text.
        let inputs: Vec<INPUT> = units
            .iter()
            .map(|unit| self.keyboard(0, *unit, KEYEVENTF_UNICODE | up))
            .collect();
        self.send(&inputs)
    }
}

/// `(virtual key, extended)` for the named keys of an X keysym.
fn named_key(keysym: u32) -> Option<(u16, bool)> {
    Some(match keysym {
        0xff0d => (VK_RETURN, false),
        0xff09 => (VK_TAB, false),
        0xff1b => (VK_ESCAPE, false),
        0xff08 => (VK_BACK, false),
        0xffff => (VK_DELETE, true),
        0xff63 => (VK_INSERT, true),
        0xff50 => (VK_HOME, true),
        0xff57 => (VK_END, true),
        0xff55 => (VK_PRIOR, true),
        0xff56 => (VK_NEXT, true),
        0xff51 => (VK_LEFT, true),
        0xff52 => (VK_UP, true),
        0xff53 => (VK_RIGHT, true),
        0xff54 => (VK_DOWN, true),
        0xffe1 => (VK_SHIFT, false),
        super::keys::CONTROL_L => (VK_CONTROL, false),
        super::keys::ALT_L => (VK_MENU, false),
        super::keys::SUPER_L => (VK_LWIN, true),
        0xffe5 => (VK_CAPITAL, false),
        0xff61 => (VK_SNAPSHOT, false),
        // F1..F24 are consecutive virtual keys.
        0xffbe..=0xffd5 => (VK_F1 + (keysym - 0xffbe) as u16, false),
        _ => return None,
    })
}

/// A screenshot of the virtual desktop through GDI, as PNG.
fn capture(origin: (i32, i32), width: i32, height: i32) -> Result<Shot> {
    // SAFETY: every handle created below is released on every path; the pixel buffer has room for
    // `width * height` 32-bit pixels, which is what GetDIBits is asked to write.
    unsafe {
        let screen = GetDC(0 as HWND);
        if screen.is_null() {
            return Err(err("could not read the screen"));
        }
        let memory = CreateCompatibleDC(screen);
        let bitmap = CreateCompatibleBitmap(screen, width, height);
        let done = |ok: bool, result: Result<Shot>| {
            let _ = ok;
            result
        };
        if memory.is_null() || bitmap.is_null() {
            if !bitmap.is_null() {
                DeleteObject(bitmap);
            }
            if !memory.is_null() {
                DeleteDC(memory);
            }
            ReleaseDC(0 as HWND, screen);
            return Err(err("could not allocate the capture"));
        }
        let previous = SelectObject(memory, bitmap);
        let copied = BitBlt(
            memory,
            0,
            0,
            width,
            height,
            screen,
            origin.0,
            origin.1,
            SRCCOPY | CAPTUREBLT,
        );
        let mut header: BITMAPINFO = zeroed();
        header.bmiHeader = BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        };
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        let rows = if copied != 0 {
            GetDIBits(
                memory,
                bitmap,
                0,
                height as u32,
                pixels.as_mut_ptr().cast(),
                &mut header,
                DIB_RGB_COLORS,
            )
        } else {
            0
        };
        SelectObject(memory, previous);
        DeleteObject(bitmap);
        DeleteDC(memory);
        ReleaseDC(0 as HWND, screen);
        if copied == 0 || rows != height {
            return done(
                false,
                Err(err(
                    "the screen could not be copied (a secure desktop, such as UAC or the lock screen, cannot be captured)",
                )),
            );
        }
        let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
        for pixel in pixels.as_chunks::<4>().0 {
            rgb.extend([pixel[2], pixel[1], pixel[0]]);
        }
        done(
            true,
            Ok(Shot {
                png: encode_png(&rgb, width as u32, height as u32)?,
            }),
        )
    }
}
