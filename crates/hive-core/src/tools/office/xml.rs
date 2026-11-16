//! Small, strict XML tokenizer for OOXML parts, plus the escaping writers need.
//!
//! It understands elements, attributes, text, CDATA, comments and processing
//! instructions, decodes the five predefined and numeric character references,
//! and refuses everything that could expand or fetch: DOCTYPE declarations and
//! custom entities. Depth and event counts are bounded.
use crate::{Error, Result};

const MAX_DEPTH: usize = 256;
const MAX_EVENTS: usize = 3_000_000;

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Event {
    Start {
        name: String,
        attrs: Vec<(String, String)>,
    },
    End {
        name: String,
    },
    Empty {
        name: String,
        attrs: Vec<(String, String)>,
    },
    Text(String),
}

fn invalid(message: &str) -> Error {
    Error::Input(format!("invalid XML: {message}"))
}

fn decode(text: &str) -> Result<String> {
    if !text.contains('&') {
        return Ok(text.to_owned());
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 1..];
        let end = tail
            .find(';')
            .ok_or_else(|| invalid("unterminated entity"))?;
        let entity = &tail[..end];
        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => {
                let code = entity
                    .strip_prefix("#x")
                    .or_else(|| entity.strip_prefix("#X"))
                    .map(|hex| u32::from_str_radix(hex, 16))
                    .or_else(|| entity.strip_prefix('#').map(|dec| dec.parse::<u32>()))
                    .ok_or_else(|| invalid("custom entities are not supported"))?
                    .map_err(|_| invalid("bad character reference"))?;
                out.push(
                    char::from_u32(code)
                        .filter(|c| allowed(*c))
                        .ok_or_else(|| invalid("character reference out of range"))?,
                );
            }
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-' | '.')
}

fn tag(inner: &str) -> Result<(String, Vec<(String, String)>)> {
    let inner = inner.trim_start();
    let name_end = inner.find(|c: char| !name_char(c)).unwrap_or(inner.len());
    let name = &inner[..name_end];
    if name.is_empty() {
        return Err(invalid("element without a name"));
    }
    let mut attrs = Vec::new();
    let mut rest = inner[name_end..].trim_start();
    while !rest.is_empty() {
        let key_end = rest.find(|c: char| !name_char(c)).unwrap_or(rest.len());
        let key = &rest[..key_end];
        let after = rest[key_end..]
            .trim_start()
            .strip_prefix('=')
            .ok_or_else(|| invalid("attribute without a value"))?
            .trim_start();
        let quote = after
            .chars()
            .next()
            .filter(|c| matches!(c, '"' | '\''))
            .ok_or_else(|| invalid("unquoted attribute"))?;
        let body = &after[1..];
        let close = body
            .find(quote)
            .ok_or_else(|| invalid("unterminated attribute"))?;
        if key.is_empty() {
            return Err(invalid("attribute without a name"));
        }
        attrs.push((key.to_owned(), decode(&body[..close])?));
        rest = body[close + 1..].trim_start();
    }
    Ok((name.to_owned(), attrs))
}

pub(super) fn parse(source: &str) -> Result<Vec<Event>> {
    let mut events = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut rest = source.strip_prefix('\u{feff}').unwrap_or(source);
    while !rest.is_empty() {
        if events.len() > MAX_EVENTS {
            return Err(invalid("too many nodes"));
        }
        let Some(open) = rest.find('<') else {
            if !stack.is_empty() || !rest.trim().is_empty() {
                events.push(Event::Text(decode(rest)?));
            }
            break;
        };
        if open > 0 {
            let text = &rest[..open];
            if !stack.is_empty() {
                events.push(Event::Text(decode(text)?));
            } else if !text.trim().is_empty() {
                return Err(invalid("text outside the root element"));
            }
            rest = &rest[open..];
        }
        if let Some(body) = rest.strip_prefix("<!--") {
            let end = body
                .find("-->")
                .ok_or_else(|| invalid("unterminated comment"))?;
            rest = &body[end + 3..];
        } else if let Some(body) = rest.strip_prefix("<![CDATA[") {
            let end = body
                .find("]]>")
                .ok_or_else(|| invalid("unterminated CDATA"))?;
            events.push(Event::Text(body[..end].to_owned()));
            rest = &body[end + 3..];
        } else if rest.starts_with("<!") {
            return Err(invalid("DOCTYPE and entity declarations are not supported"));
        } else if let Some(body) = rest.strip_prefix("<?") {
            let end = body
                .find("?>")
                .ok_or_else(|| invalid("unterminated processing instruction"))?;
            rest = &body[end + 2..];
        } else if let Some(body) = rest.strip_prefix("</") {
            let end = body
                .find('>')
                .ok_or_else(|| invalid("unterminated end tag"))?;
            let name = body[..end].trim();
            if stack.pop().as_deref() != Some(name) {
                return Err(invalid("mismatched end tag"));
            }
            events.push(Event::End {
                name: name.to_owned(),
            });
            rest = &body[end + 1..];
        } else {
            let body = &rest[1..];
            let end = body.find('>').ok_or_else(|| invalid("unterminated tag"))?;
            // `>` inside a quoted attribute value would end the tag early; Office never writes it.
            let (inner, empty) = match body[..end].strip_suffix('/') {
                Some(inner) => (inner, true),
                None => (&body[..end], false),
            };
            let (name, attrs) = tag(inner)?;
            if empty {
                events.push(Event::Empty { name, attrs });
            } else {
                if stack.len() >= MAX_DEPTH {
                    return Err(invalid("elements nested too deeply"));
                }
                stack.push(name.clone());
                events.push(Event::Start { name, attrs });
            }
            rest = &body[end + 1..];
        }
    }
    if !stack.is_empty() {
        return Err(invalid("unclosed element"));
    }
    Ok(events)
}

pub(super) fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

/// XML 1.0 character range.
fn allowed(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r') || (c >= ' ' && c != '\u{fffe}' && c != '\u{ffff}')
}

/// Escape text for element content or a double-quoted attribute. Characters XML
/// cannot carry (most C0 controls) are dropped rather than producing a broken part.
pub(super) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars().filter(|c| allowed(*c)) {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            other => out.push(other),
        }
    }
    out
}
