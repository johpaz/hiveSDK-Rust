//! Neutral image, audio and document attachments for user messages.
//!
//! The contract is deliberately small: an attachment is either inline base64
//! (`data`) or a public `url`, and each provider adapter maps only the
//! combinations its API really supports. Unsupported combinations fail before
//! any request is sent; nothing is converted or approximated.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};

pub const MAX_ATTACHMENTS: usize = 8;
/// Base64 characters per attachment (about 6 MiB of decoded bytes).
pub const MAX_ATTACHMENT_BASE64_BYTES: usize = 8 * 1024 * 1024;
/// Base64 characters across one message.
pub const MAX_MESSAGE_BASE64_BYTES: usize = 16 * 1024 * 1024;
const MAX_URL_BYTES: usize = 4096;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Image,
    Audio,
    Document,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub kind: AttachmentKind,
    /// IANA media type such as `image/png`, `audio/wav` or `application/pdf`.
    pub media_type: String,
    /// Standard-alphabet base64 content. Exactly one of `data` and `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    /// Public http(s) URL the provider fetches itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Optional file name; some providers require one for documents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Input(message.into())
}

pub(crate) fn valid_media_type(value: &str) -> bool {
    let Some((main, sub)) = value.split_once('/') else {
        return false;
    };
    let token = |part: &str| {
        !part.is_empty()
            && part.len() <= 127
            && part
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"+-._".contains(&b))
    };
    token(main) && token(sub)
}

pub(crate) fn valid_base64(data: &str) -> bool {
    let bytes = data.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return false;
    }
    let padding = bytes.iter().rev().take_while(|b| **b == b'=').count();
    padding <= 2
        && bytes[..bytes.len() - padding]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/')
}

impl Attachment {
    pub fn validate(&self) -> Result<()> {
        if !valid_media_type(&self.media_type) {
            return Err(invalid("attachment media_type is not a valid media type"));
        }
        let family = self.media_type.split('/').next().unwrap_or_default();
        let allowed = match self.kind {
            AttachmentKind::Image => family == "image",
            AttachmentKind::Audio => family == "audio",
            AttachmentKind::Document => family != "image" && family != "audio",
        };
        if !allowed {
            return Err(invalid("attachment media_type does not match its kind"));
        }
        match (&self.data, &self.url) {
            (Some(data), None) => {
                if data.len() > MAX_ATTACHMENT_BASE64_BYTES {
                    return Err(invalid("attachment data exceeds the size limit"));
                }
                if !valid_base64(data) {
                    return Err(invalid("attachment data is not valid base64"));
                }
            }
            (None, Some(url)) => {
                let parsed = reqwest::Url::parse(url)
                    .map_err(|_| invalid("attachment url is not a valid URL"))?;
                if url.len() > MAX_URL_BYTES
                    || !matches!(parsed.scheme(), "http" | "https")
                    || !parsed.username().is_empty()
                    || parsed.password().is_some()
                {
                    return Err(invalid(
                        "attachment url must be http(s) without credentials",
                    ));
                }
            }
            _ => return Err(invalid("attachment needs exactly one of data and url")),
        }
        if let Some(name) = &self.name
            && (name.is_empty() || name.len() > 256 || name.chars().any(char::is_control))
        {
            return Err(invalid("attachment name is invalid"));
        }
        Ok(())
    }

    /// `data:` URL form used by providers that take inline media as a URL.
    pub(crate) fn data_url(&self) -> Option<String> {
        self.data
            .as_ref()
            .map(|data| format!("data:{};base64,{data}", self.media_type))
    }
}

/// Providers differ on what a non-user role does with media, and silently dropping
/// it would hide a caller bug, so every adapter rejects it up front.
pub(crate) fn ensure_user_only(messages: &[crate::Message]) -> Result<()> {
    if messages
        .iter()
        .any(|message| !message.attachments.is_empty() && message.role != "user")
    {
        return Err(invalid("only user messages can carry attachments"));
    }
    Ok(())
}

pub fn validate_all(attachments: &[Attachment]) -> Result<()> {
    if attachments.len() > MAX_ATTACHMENTS {
        return Err(invalid(format!(
            "at most {MAX_ATTACHMENTS} attachments per message"
        )));
    }
    let mut total = 0usize;
    for attachment in attachments {
        attachment.validate()?;
        total += attachment.data.as_ref().map_or(0, String::len);
    }
    if total > MAX_MESSAGE_BASE64_BYTES {
        return Err(invalid("attachments exceed the per-message size limit"));
    }
    Ok(())
}

/// A user turn: plain text, or text plus attachments. Plain strings keep their
/// existing wire form, so callers and stored commands that send a string work
/// unchanged.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Prompt {
    Text(String),
    Parts(PromptParts),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PromptParts {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

impl Prompt {
    pub fn into_parts(self) -> (String, Vec<Attachment>) {
        match self {
            Self::Text(text) => (text, vec![]),
            Self::Parts(parts) => (parts.text, parts.attachments),
        }
    }
}
impl From<String> for Prompt {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}
impl From<&str> for Prompt {
    fn from(text: &str) -> Self {
        Self::Text(text.to_owned())
    }
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard-alphabet base64 with padding; `None` for anything `valid_base64` rejects.
pub(crate) fn decode_base64(data: &str) -> Option<Vec<u8>> {
    if !valid_base64(data) {
        return None;
    }
    let value = |byte: u8| ALPHABET.iter().position(|candidate| *candidate == byte).map(|at| at as u32);
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    for chunk in data.as_bytes().chunks(4) {
        let padding = chunk.iter().filter(|byte| **byte == b'=').count();
        let mut acc = 0u32;
        for byte in &chunk[..4 - padding] {
            acc = acc << 6 | value(*byte)?;
        }
        acc <<= 6 * padding as u32;
        let bytes = acc.to_be_bytes();
        out.extend_from_slice(&bytes[1..4 - padding]);
    }
    Some(out)
}

pub(crate) fn encode_base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let acc = chunk.iter().enumerate().fold(0u32, |acc, (at, byte)| acc | (*byte as u32) << (16 - 8 * at));
        for at in 0..4 {
            if at <= chunk.len() {
                out.push(ALPHABET[(acc >> (18 - 6 * at) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
