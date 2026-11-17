//! An in-memory desktop for tests (also offered to hosts, hidden, as `ScriptedDesktop`): records
//! what it was asked to do and shows a generated screen.
use super::{Button, Shot};
use crate::{Error, Result};
use std::{sync::Mutex, time::Duration};

pub(crate) struct Fake {
    pub size: (u32, u32),
    pub events: Mutex<Vec<String>>,
    /// Make the next input hang this long (to test timeouts and cancellation).
    pub stall: Mutex<Option<Duration>>,
    /// Size of the PNG it returns (a HiDPI capture is bigger than the pointer space).
    pub capture: (u32, u32),
}

impl Fake {
    pub fn new(size: (u32, u32)) -> Self {
        Self {
            size,
            events: Mutex::default(),
            stall: Mutex::default(),
            capture: size,
        }
    }
    pub fn log(&self, text: String) {
        self.events.lock().unwrap().push(text);
    }
    pub fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
    pub fn size(&self) -> Result<(u32, u32)> {
        Ok(self.size)
    }
    pub fn screenshot(&self) -> Result<Shot> {
        use image_codec::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
        let (w, h) = self.capture;
        // A gradient so a JPEG is not trivially tiny and crops can be told apart.
        let mut pixels = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                pixels.extend([
                    (x * 255 / w.max(1)) as u8,
                    (y * 255 / h.max(1)) as u8,
                    ((x ^ y) & 0xff) as u8,
                ]);
            }
        }
        let mut png = vec![];
        PngEncoder::new(&mut png)
            .write_image(&pixels, w, h, ExtendedColorType::Rgb8)
            .map_err(|_| Error::State("fake encode".into()))?;
        Ok(Shot { png })
    }
    fn maybe_stall(&self) -> Option<Duration> {
        *self.stall.lock().unwrap()
    }
    pub fn motion(&self, x: u32, y: u32) -> Result<()> {
        self.log(format!("move {x} {y}"));
        Ok(())
    }
    pub fn button(&self, button: Button, down: bool) -> Result<()> {
        self.log(format!(
            "button {button:?} {}",
            if down { "down" } else { "up" }
        ));
        Ok(())
    }
    pub fn scroll(&self, dx: i32, dy: i32) -> Result<()> {
        self.log(format!("scroll {dx} {dy}"));
        Ok(())
    }
    pub fn key(&self, keysym: u32, down: bool) -> Result<()> {
        self.log(format!(
            "key {keysym:#x} {}",
            if down { "down" } else { "up" }
        ));
        let _ = self.maybe_stall();
        Ok(())
    }
}
