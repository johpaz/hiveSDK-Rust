//! Bounded image metadata: format and pixel size read from the header only.
//!
//! No pixel data is decoded and nothing is resized or re-encoded; transforms
//! need an image codec and are not part of this module.
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct ImageInfo {
    pub mime_type: &'static str,
    pub width: u32,
    pub height: u32,
}

fn be16(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?) as u32)
}
fn le16(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?) as u32)
}
fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}
fn le24(bytes: &[u8], at: usize) -> Option<u32> {
    let part = bytes.get(at..at + 3)?;
    Some(part[0] as u32 | (part[1] as u32) << 8 | (part[2] as u32) << 16)
}

fn png(bytes: &[u8]) -> Option<ImageInfo> {
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") || bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    Some(ImageInfo {
        mime_type: "image/png",
        width: be32(bytes, 16)?,
        height: be32(bytes, 20)?,
    })
}

fn jpeg(bytes: &[u8]) -> Option<ImageInfo> {
    if !bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return None;
    }
    let mut at = 2;
    // Bounded walk over marker segments; real headers reach a frame marker quickly.
    for _ in 0..256 {
        while *bytes.get(at)? == 0xff && *bytes.get(at + 1)? == 0xff {
            at += 1;
        }
        if *bytes.get(at)? != 0xff {
            return None;
        }
        let marker = *bytes.get(at + 1)?;
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            return Some(ImageInfo {
                mime_type: "image/jpeg",
                height: be16(bytes, at + 5)?,
                width: be16(bytes, at + 7)?,
            });
        }
        if marker == 0xd9 || marker == 0xda {
            return None;
        }
        at += 2 + be16(bytes, at + 2)? as usize;
    }
    None
}

fn gif(bytes: &[u8]) -> Option<ImageInfo> {
    if !bytes.starts_with(b"GIF87a") && !bytes.starts_with(b"GIF89a") {
        return None;
    }
    Some(ImageInfo {
        mime_type: "image/gif",
        width: le16(bytes, 6)?,
        height: le16(bytes, 8)?,
    })
}

fn webp(bytes: &[u8]) -> Option<ImageInfo> {
    if bytes.get(..4)? != b"RIFF" || bytes.get(8..12)? != b"WEBP" {
        return None;
    }
    let (width, height) = match bytes.get(12..16)? {
        b"VP8 " if bytes.get(23..26)? == [0x9d, 0x01, 0x2a] => {
            (le16(bytes, 26)? & 0x3fff, le16(bytes, 28)? & 0x3fff)
        }
        b"VP8L" if *bytes.get(20)? == 0x2f => {
            let packed = u32::from_le_bytes(bytes.get(21..25)?.try_into().ok()?);
            ((packed & 0x3fff) + 1, ((packed >> 14) & 0x3fff) + 1)
        }
        b"VP8X" => (le24(bytes, 24)? + 1, le24(bytes, 27)? + 1),
        _ => return None,
    };
    Some(ImageInfo {
        mime_type: "image/webp",
        width,
        height,
    })
}

/// Format and size from the leading bytes, or `None` when it is not a
/// recognised, well-formed PNG, JPEG, GIF or WebP header. Zero dimensions are
/// rejected as malformed.
pub fn info(bytes: &[u8]) -> Option<ImageInfo> {
    png(bytes)
        .or_else(|| jpeg(bytes))
        .or_else(|| gif(bytes))
        .or_else(|| webp(bytes))
        .filter(|info| info.width > 0 && info.height > 0)
}

/// Media type from magic bytes for the formats artifacts care about.
pub fn detect_mime(bytes: &[u8]) -> Option<&'static str> {
    info(bytes)
        .map(|info| info.mime_type)
        .or_else(|| bytes.starts_with(b"%PDF-").then_some("application/pdf"))
}
