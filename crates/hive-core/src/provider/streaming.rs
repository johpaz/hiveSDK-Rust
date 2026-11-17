//! Incremental SSE framing and Chat Completions delta assembly; never runs a tool.
use super::ModelEvent;
use crate::{Error, ModelReply, Result, ToolCall, Usage};
use serde_json::Value;
use std::collections::BTreeMap;

const MAX_EVENT_BYTES: usize = 1024 * 1024;

pub(super) fn terminal_error(reason: &str, usage: Option<&Usage>) -> Error {
    // Keep only an enum-shaped code, never a response body or free-form message.
    let code = if !reason.is_empty()
        && reason.len() <= 64
        && reason
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        reason
    } else {
        "INVALID"
    };
    let hint = match code {
        "length" => "output token limit reached; response not confirmed",
        "content_filter" => "response filtered; response not confirmed",
        _ => "unsupported terminal reason; response not confirmed",
    };
    Error::Provider(format!(
        "finish_reason={code}; {hint}; completion_tokens={}",
        usage.map_or_else(|| "unknown".into(), |usage| usage.output_tokens.to_string())
    ))
}

#[derive(Default)]
pub struct SseDecoder {
    line: Vec<u8>,
    data: String,
    after_cr: bool,
    started: bool,
}
impl SseDecoder {
    pub fn finish(&self) -> Result<()> {
        if !self.line.is_empty() || !self.data.is_empty() {
            return Err(Error::Provider("stream ended inside an SSE event".into()));
        }
        Ok(())
    }
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<String>> {
        let mut frames = vec![];
        for &byte in bytes {
            match byte {
                b'\r' => {
                    self.flush(&mut frames)?;
                    self.after_cr = true;
                }
                b'\n' => {
                    if !self.after_cr {
                        self.flush(&mut frames)?;
                    }
                    self.after_cr = false;
                }
                _ => {
                    self.after_cr = false;
                    self.line.push(byte);
                    if self.line.len().saturating_add(self.data.len()) > MAX_EVENT_BYTES {
                        return Err(Error::Provider("SSE event exceeds byte limit".into()));
                    }
                }
            }
        }
        Ok(frames)
    }
    fn flush(&mut self, frames: &mut Vec<String>) -> Result<()> {
        let bytes = std::mem::take(&mut self.line);
        let mut line =
            std::str::from_utf8(&bytes).map_err(|_| Error::Provider("invalid SSE UTF-8".into()))?;
        if !self.started {
            line = line.trim_start_matches('\u{feff}');
            self.started = true;
        }
        if line.is_empty() {
            if !self.data.is_empty() {
                self.data.pop();
                frames.push(std::mem::take(&mut self.data));
            }
        } else {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            if field == "data" {
                self.data.push_str(value.strip_prefix(' ').unwrap_or(value));
                self.data.push('\n');
                if self.data.len() > MAX_EVENT_BYTES {
                    return Err(Error::Provider("SSE event exceeds byte limit".into()));
                }
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct PartialTool {
    id: Option<String>,
    name: String,
    arguments: String,
}
#[derive(Default)]
pub struct Accumulator {
    text: Option<String>,
    reasoning: String,
    tools: BTreeMap<usize, PartialTool>,
    usage: Option<Usage>,
    finish: Option<String>,
}
impl Accumulator {
    pub fn push(&mut self, value: Value) -> Result<Vec<ModelEvent>> {
        if value.get("error").is_some_and(|v| !v.is_null()) {
            return Err(Error::Provider("provider returned a stream error".into()));
        }
        let mut events = vec![];
        if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
            let usage = Usage {
                input_tokens: usage["prompt_tokens"]
                    .as_u64()
                    .ok_or_else(|| Error::Provider("invalid stream usage".into()))?,
                output_tokens: usage["completion_tokens"]
                    .as_u64()
                    .ok_or_else(|| Error::Provider("invalid stream usage".into()))?,
            };
            self.usage = Some(usage.clone());
            events.push(ModelEvent::Usage { usage });
        }
        let choices = value["choices"]
            .as_array()
            .ok_or_else(|| Error::Provider("missing stream choices".into()))?;
        if choices.len() > 1 {
            return Err(Error::Provider(
                "only one response choice is supported".into(),
            ));
        }
        for choice in choices {
            if choice.get("index").is_some_and(|v| v.as_u64() != Some(0)) {
                return Err(Error::Provider(
                    "only one response choice is supported".into(),
                ));
            }
            let delta = choice["delta"]
                .as_object()
                .ok_or_else(|| Error::Provider("invalid stream delta".into()))?;
            if delta.get("refusal").is_some_and(|v| !v.is_null()) {
                return Err(Error::Provider("provider refused the response".into()));
            }
            if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str) {
                self.reasoning.push_str(reasoning);
            }
            if let Some(content) = delta.get("content").filter(|v| !v.is_null()) {
                let text = content
                    .as_str()
                    .ok_or_else(|| Error::Provider("invalid text delta".into()))?;
                if self.finish.is_some() {
                    return Err(Error::Provider("content after finish_reason".into()));
                }
                self.text.get_or_insert_default().push_str(text);
                events.push(ModelEvent::TextDelta { text: text.into() });
            }
            if let Some(calls) = delta.get("tool_calls").filter(|v| !v.is_null()) {
                if self.finish.is_some() {
                    return Err(Error::Provider("tool delta after finish_reason".into()));
                }
                for call in calls
                    .as_array()
                    .ok_or_else(|| Error::Provider("invalid tool deltas".into()))?
                {
                    let index = call["index"]
                        .as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .ok_or_else(|| Error::Provider("missing tool delta index".into()))?;
                    if call
                        .get("type")
                        .is_some_and(|v| v.as_str() != Some("function"))
                    {
                        return Err(Error::Provider("unsupported tool delta type".into()));
                    }
                    let optional = |v: Option<&Value>| -> Result<Option<String>> {
                        v.filter(|v| !v.is_null())
                            .map(|v| {
                                v.as_str().map(str::to_owned).ok_or_else(|| {
                                    Error::Provider("invalid tool delta field".into())
                                })
                            })
                            .transpose()
                    };
                    let id = optional(call.get("id"))?;
                    let name = optional(call.get("function").and_then(|f| f.get("name")))?;
                    let arguments =
                        optional(call.get("function").and_then(|f| f.get("arguments")))?
                            .unwrap_or_default();
                    let tool = self.tools.entry(index).or_default();
                    if let Some(id) = &id {
                        if tool.id.as_ref().is_some_and(|old| old != id) {
                            return Err(Error::Provider(
                                "tool call ID changed during streaming".into(),
                            ));
                        }
                        tool.id = Some(id.clone());
                    }
                    if let Some(name) = &name {
                        tool.name.push_str(name);
                    }
                    tool.arguments.push_str(&arguments);
                    events.push(ModelEvent::ToolCallDelta {
                        index,
                        id,
                        name,
                        arguments,
                    });
                }
            }
            if let Some(reason) = choice.get("finish_reason").filter(|v| !v.is_null()) {
                let reason = reason
                    .as_str()
                    .ok_or_else(|| Error::Provider("invalid finish_reason".into()))?;
                if !matches!(reason, "stop" | "tool_calls") {
                    return Err(terminal_error(reason, self.usage.as_ref()));
                }
                if self.finish.is_some() {
                    return Err(Error::Provider("duplicate finish_reason".into()));
                }
                self.finish = Some(reason.into());
            }
        }
        Ok(events)
    }
    pub fn finish(self) -> Result<ModelReply> {
        if self.finish.is_none() {
            return Err(Error::Provider("stream missing finish_reason".into()));
        }
        let mut calls = vec![];
        for (expected, (index, tool)) in self.tools.into_iter().enumerate() {
            if index != expected {
                return Err(Error::Provider("stream missing a tool call index".into()));
            }
            let id = tool
                .id
                .filter(|id| !id.is_empty())
                .ok_or_else(|| Error::Provider("tool call missing ID".into()))?;
            if tool.name.is_empty() {
                return Err(Error::Provider("tool call missing name".into()));
            }
            calls.push(ToolCall {
                id,
                name: tool.name,
                arguments: serde_json::from_str(&tool.arguments)
                    .map_err(|_| Error::Provider("invalid assembled tool arguments JSON".into()))?,
            });
        }
        if self.text.is_none() && calls.is_empty() {
            return Err(Error::Provider("empty streamed response".into()));
        }
        if self.finish.as_deref() == Some("tool_calls") && calls.is_empty() {
            return Err(Error::Provider("missing streamed tool calls".into()));
        }
        Ok(ModelReply {
            provider_state: (!self.reasoning.is_empty()).then_some(
                crate::ProviderState::Reasoning {
                    content: self.reasoning,
                },
            ),
            content: self.text,
            tool_calls: calls,
            usage: self.usage,
        })
    }
}
