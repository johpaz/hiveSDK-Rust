//! Native Messages API subset: text and client-side tools. No automatic continuations.
use super::{HttpProvider, ModelEvent, ModelStream, OpenAiConfig, Protocol};
use crate::{Attachment, AttachmentKind, Error, ModelReply, ModelRequest, Result, ToolCall, Usage};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Arc};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnthropicConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    #[serde(default = "super::default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "super::default_response_limit")]
    pub max_response_bytes: usize,
    #[serde(default = "default_version")]
    pub api_version: String,
    /// Output limit for a request that names none. Without it, one is derived from
    /// `context_window` (15%), else 4096.
    #[serde(default)]
    pub default_max_output_tokens: Option<u64>,
    #[serde(default)]
    pub context_window: Option<u64>,
}
fn default_version() -> String {
    "2026-10-21".into()
}

pub struct Anthropic {
    pub(super) inner: Arc<HttpProvider>,
}
impl Anthropic {
    pub fn new(config: AnthropicConfig) -> Result<Self> {
        let default_max_output_tokens =
            super::resolve_output_cap(config.default_max_output_tokens, config.context_window)?;
        if config.api_version.trim().is_empty()
            || reqwest::header::HeaderValue::from_str(&config.api_version).is_err()
        {
            return Err(Error::Input(
                "Anthropic requires positive default_max_output_tokens and a valid api_version"
                    .into(),
            ));
        }
        let protocol = Protocol::Anthropic {
            version: config.api_version,
            default_max_output_tokens,
        };
        let common = OpenAiConfig {
            base_url: config.base_url,
            api_key: config.api_key,
            timeout_ms: config.timeout_ms,
            max_response_bytes: config.max_response_bytes,
            output_token_parameter: Default::default(),
            context_window: None,
        };
        Ok(Self {
            inner: Arc::new(HttpProvider::new(common, protocol)?),
        })
    }
    pub fn from_json(config: &str) -> Result<Self> {
        Self::new(serde_json::from_str(config)?)
    }
    pub fn start(self: &Arc<Self>, request: ModelRequest, stream: bool) -> ModelStream {
        self.inner.start(request, stream)
    }
}

fn invalid(message: &str) -> Error {
    Error::Provider(format!("Anthropic: {message}"))
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| invalid("missing or invalid string field"))
}
fn append(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if let Some(last) = messages.last_mut().filter(|last| last["role"] == role) {
        last["content"]
            .as_array_mut()
            .expect("constructed content array")
            .extend(blocks);
    } else {
        messages.push(json!({"role":role,"content":blocks}));
    }
}

/// Images take base64 or URL sources; documents are limited to PDF. The Messages
/// API has no audio input, so audio is rejected rather than approximated.
fn media_block(attachment: &Attachment) -> Result<Value> {
    let source = match (&attachment.data, &attachment.url) {
        (Some(data), _) => {
            json!({"type":"base64","media_type":attachment.media_type,"data":data})
        }
        (None, Some(url)) => json!({"type":"url","url":url}),
        _ => return Err(Error::Input("attachment needs data or url".into())),
    };
    match attachment.kind {
        AttachmentKind::Image => {
            if !matches!(
                attachment.media_type.as_str(),
                "image/jpeg" | "image/png" | "image/gif" | "image/webp"
            ) {
                return Err(Error::Input(
                    "Anthropic images must be jpeg, png, gif or webp".into(),
                ));
            }
            Ok(json!({"type":"image","source":source}))
        }
        AttachmentKind::Document => {
            if attachment.media_type != "application/pdf" {
                return Err(Error::Input(
                    "Anthropic documents must be application/pdf".into(),
                ));
            }
            let mut block = json!({"type":"document","source":source});
            if let Some(name) = &attachment.name {
                block["title"] = json!(name);
            }
            Ok(block)
        }
        AttachmentKind::Audio => Err(Error::Input(
            "Anthropic Messages does not accept audio".into(),
        )),
    }
}

pub fn request_body(request: &ModelRequest, default_max_output_tokens: u64) -> Result<Value> {
    crate::media::ensure_user_only(&request.messages)?;
    if let Some(output) = &request.structured_output {
        output.validate()?;
    }
    if request
        .messages
        .iter()
        .any(|message| message.provider_state.is_some())
    {
        return Err(Error::Input(
            "provider state cannot be replayed through Anthropic".into(),
        ));
    }
    let cap = request
        .max_output_tokens
        .unwrap_or(default_max_output_tokens);
    if cap == 0 {
        return Err(Error::Input("Anthropic max_tokens must be positive".into()));
    }
    let mut system = vec![];
    let mut messages = vec![];
    let mut pending = HashSet::new();
    for message in &request.messages {
        match message.role.as_str() {
            "system" => {
                if !messages.is_empty()
                    || !message.tool_calls.is_empty()
                    || message.tool_call_id.is_some()
                {
                    return Err(Error::Input(
                        "Anthropic system messages must be a text-only prefix".into(),
                    ));
                }
                system.push(json!({"type":"text","text":message.content.as_deref().ok_or_else(||Error::Input("system text is required".into()))?}));
            }
            "user" | "assistant" => {
                if !pending.is_empty()
                    || message.tool_call_id.is_some()
                    || (message.role == "user" && !message.tool_calls.is_empty())
                {
                    return Err(Error::Input(
                        "Anthropic requires all tool results before the next message".into(),
                    ));
                }
                let mut blocks = vec![];
                if !message.attachments.is_empty() && message.role != "user" {
                    return Err(Error::Input(
                        "only user messages can carry attachments".into(),
                    ));
                }
                // Media blocks lead the text, which is the order the API recommends.
                for attachment in &message.attachments {
                    blocks.push(media_block(attachment)?);
                }
                if let Some(text) = &message.content {
                    blocks.push(json!({"type":"text","text":text}));
                }
                for call in &message.tool_calls {
                    if call.id.is_empty()
                        || call.name.is_empty()
                        || !call.arguments.is_object()
                        || !pending.insert(call.id.clone())
                    {
                        return Err(Error::Input("invalid Anthropic tool call".into()));
                    }
                    blocks.push(json!({"type":"tool_use","id":call.id,"name":call.name,"input":call.arguments}));
                }
                if blocks.is_empty() {
                    return Err(Error::Input("Anthropic message has no content".into()));
                }
                append(&mut messages, &message.role, blocks);
            }
            "tool" => {
                let id = message
                    .tool_call_id
                    .as_ref()
                    .ok_or_else(|| Error::Input("tool result needs an ID".into()))?;
                if !pending.remove(id) || !message.tool_calls.is_empty() {
                    return Err(Error::Input(
                        "orphan or duplicate Anthropic tool result".into(),
                    ));
                }
                let content = message
                    .content
                    .as_deref()
                    .ok_or_else(|| Error::Input("tool result needs content".into()))?;
                let mut block = json!({"type":"tool_result","tool_use_id":id,"content":content});
                if serde_json::from_str::<Value>(content)
                    .is_ok_and(|value| value.get("error").is_some())
                {
                    block["is_error"] = json!(true);
                }
                append(&mut messages, "user", vec![block]);
            }
            _ => return Err(Error::Input("unsupported Anthropic message role".into())),
        }
    }
    if !pending.is_empty()
        || messages
            .first()
            .is_none_or(|message| message["role"] != "user")
    {
        return Err(Error::Input(
            "Anthropic conversation must start with user and contain all tool results".into(),
        ));
    }
    let mut body =
        json!({"model":request.model,"messages":messages,"max_tokens":cap,"stream":false});
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    if !request.tools.is_empty() {
        body["tools"] = json!(request.tools.iter().map(|tool|json!({"name":tool.name,"description":tool.description,"input_schema":tool.parameters})).collect::<Vec<_>>());
    }
    if let Some(output) = &request.structured_output {
        body["output_config"] = json!({"format":{"type":"json_schema","schema":output.schema}});
    }
    Ok(body)
}

#[derive(Default)]
struct Meter {
    input: Option<u64>,
    output: Option<u64>,
    created: u64,
    cached: u64,
}
impl Meter {
    fn update(&mut self, value: &Value) -> Result<()> {
        if !value.is_object() {
            return Err(invalid("invalid usage"));
        }
        for (key, target) in [
            ("input_tokens", &mut self.input),
            ("output_tokens", &mut self.output),
        ] {
            if let Some(value) = value.get(key) {
                let next = value
                    .as_u64()
                    .ok_or_else(|| invalid("invalid usage count"))?;
                if target.is_some_and(|previous| next < previous) {
                    return Err(invalid("usage counter decreased"));
                }
                *target = Some(next);
            }
        }
        for (key, target) in [
            ("cache_creation_input_tokens", &mut self.created),
            ("cache_read_input_tokens", &mut self.cached),
        ] {
            if let Some(value) = value.get(key) {
                let next = value
                    .as_u64()
                    .ok_or_else(|| invalid("invalid cache usage"))?;
                if next < *target {
                    return Err(invalid("cache usage counter decreased"));
                }
                *target = next;
            }
        }
        Ok(())
    }
    fn usage(&self) -> Result<Option<Usage>> {
        let (Some(input), Some(output)) = (self.input, self.output) else {
            return Ok(None);
        };
        let input_tokens = input
            .checked_add(self.created)
            .and_then(|value| value.checked_add(self.cached))
            .ok_or_else(|| invalid("usage count overflow"))?;
        Ok(Some(Usage {
            input_tokens,
            output_tokens: output,
        }))
    }
}
fn check_stop(reason: &str) -> Result<()> {
    if !matches!(reason, "end_turn" | "tool_use" | "stop_sequence") {
        return Err(invalid("unsupported, truncated or refused stop reason"));
    }
    Ok(())
}
fn refusal(value: &Value) -> Result<()> {
    if value["stop_details"]["type"] == "refusal"
        || value.get("error").is_some_and(|v| !v.is_null())
    {
        return Err(invalid("provider refused or failed the response"));
    }
    Ok(())
}
fn assemble(blocks: &[Value], reason: &str, usage: Option<Usage>) -> Result<ModelReply> {
    check_stop(reason)?;
    let mut content = None::<String>;
    let mut calls = vec![];
    let mut ids = HashSet::new();
    for block in blocks {
        match string(block, "type")? {
            "text" => {
                if block
                    .get("citations")
                    .is_some_and(|v| !v.is_null() && v.as_array().is_none_or(|v| !v.is_empty()))
                {
                    return Err(invalid(
                        "citations are not supported by the neutral text contract",
                    ));
                }
                content
                    .get_or_insert_default()
                    .push_str(string(block, "text")?);
            }
            "tool_use" => {
                let id = string(block, "id")?;
                let name = string(block, "name")?;
                if id.is_empty()
                    || name.is_empty()
                    || !ids.insert(id)
                    || !block["input"].is_object()
                {
                    return Err(invalid("invalid tool_use block"));
                }
                calls.push(ToolCall {
                    id: id.into(),
                    name: name.into(),
                    arguments: block["input"].clone(),
                });
            }
            _ => return Err(invalid("unsupported content block")),
        }
    }
    if (reason == "tool_use") != !calls.is_empty() || (content.is_none() && calls.is_empty()) {
        return Err(invalid("stop reason does not match content"));
    }
    Ok(ModelReply {
        provider_state: None,
        content,
        tool_calls: calls,
        usage,
    })
}
pub fn parse_reply(value: Value) -> Result<ModelReply> {
    refusal(&value)?;
    if value["type"] != "message" || value["role"] != "assistant" {
        return Err(invalid("invalid message envelope"));
    }
    let mut meter = Meter::default();
    if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
        meter.update(usage)?;
    }
    assemble(
        value["content"]
            .as_array()
            .ok_or_else(|| invalid("invalid content"))?,
        string(&value, "stop_reason")?,
        meter.usage()?,
    )
}

struct PartialBlock {
    value: Value,
    arguments: String,
    tool_index: Option<usize>,
}
#[derive(Default)]
pub struct Accumulator {
    started: bool,
    finished: bool,
    blocks: Vec<Value>,
    active: Option<PartialBlock>,
    tools: usize,
    meter: Meter,
    stop: Option<String>,
}
impl Accumulator {
    pub fn push(&mut self, value: Value) -> Result<Vec<ModelEvent>> {
        if self.finished {
            return Err(invalid("event after message_stop"));
        }
        let kind = string(&value, "type")?;
        if kind == "error" {
            return Err(invalid("provider returned a stream error"));
        }
        if kind == "ping" {
            return Ok(vec![]);
        }
        let mut events = vec![];
        if kind == "message_start" {
            let message = &value["message"];
            refusal(message)?;
            if self.started
                || message["type"] != "message"
                || message["role"] != "assistant"
                || message["content"].as_array().is_none_or(|v| !v.is_empty())
                || !message["stop_reason"].is_null()
            {
                return Err(invalid("invalid message_start"));
            }
            self.started = true;
            if let Some(usage) = message.get("usage").filter(|v| !v.is_null()) {
                self.meter.update(usage)?;
            }
            if let Some(usage) = self.meter.usage()? {
                events.push(ModelEvent::Usage { usage });
            }
            return Ok(events);
        }
        if !self.started {
            return Err(invalid("event before message_start"));
        }
        match kind {
            "content_block_start" => {
                if self.active.is_some()
                    || self.stop.is_some()
                    || value["index"].as_u64() != Some(self.blocks.len() as u64)
                {
                    return Err(invalid("invalid content block order"));
                }
                let block = value["content_block"].clone();
                let tool_index = match string(&block, "type")? {
                    "text" => {
                        let text = string(&block, "text")?;
                        if !text.is_empty() {
                            events.push(ModelEvent::TextDelta { text: text.into() });
                        }
                        None
                    }
                    "tool_use" => {
                        let id = string(&block, "id")?;
                        let name = string(&block, "name")?;
                        if id.is_empty() || name.is_empty() || !block["input"].is_object() {
                            return Err(invalid("invalid tool start"));
                        }
                        let index = self.tools;
                        self.tools += 1;
                        let arguments = if block["input"].as_object().is_some_and(|v| v.is_empty())
                        {
                            String::new()
                        } else {
                            block["input"].to_string()
                        };
                        events.push(ModelEvent::ToolCallDelta {
                            index,
                            id: Some(id.into()),
                            name: Some(name.into()),
                            arguments,
                        });
                        Some(index)
                    }
                    _ => return Err(invalid("unsupported streaming content block")),
                };
                self.active = Some(PartialBlock {
                    value: block,
                    arguments: String::new(),
                    tool_index,
                });
            }
            "content_block_delta" => {
                if value["index"].as_u64() != Some(self.blocks.len() as u64) {
                    return Err(invalid("delta index does not match active block"));
                }
                let active = self
                    .active
                    .as_mut()
                    .ok_or_else(|| invalid("delta without active block"))?;
                let delta = &value["delta"];
                match (active.value["type"].as_str(), string(delta, "type")?) {
                    (Some("text"), "text_delta") => {
                        let text = string(delta, "text")?;
                        let Value::String(content) = &mut active.value["text"] else {
                            return Err(invalid("invalid active text block"));
                        };
                        content.push_str(text);
                        events.push(ModelEvent::TextDelta { text: text.into() });
                    }
                    (Some("tool_use"), "input_json_delta") => {
                        if active.value["input"]
                            .as_object()
                            .is_none_or(|v| !v.is_empty())
                        {
                            return Err(invalid("conflicting tool input and JSON deltas"));
                        }
                        let text = string(delta, "partial_json")?;
                        active.arguments.push_str(text);
                        events.push(ModelEvent::ToolCallDelta {
                            index: active.tool_index.unwrap(),
                            id: None,
                            name: None,
                            arguments: text.into(),
                        });
                    }
                    _ => return Err(invalid("unsupported or mismatched delta")),
                }
            }
            "content_block_stop" => {
                if value["index"].as_u64() != Some(self.blocks.len() as u64) {
                    return Err(invalid("invalid block stop index"));
                }
                let mut active = self
                    .active
                    .take()
                    .ok_or_else(|| invalid("stop without active block"))?;
                if let Some(index) = active.tool_index {
                    if !active.arguments.is_empty() {
                        let input: Value = serde_json::from_str(&active.arguments)
                            .map_err(|_| invalid("incomplete tool JSON"))?;
                        if !input.is_object() {
                            return Err(invalid("tool input must be an object"));
                        }
                        active.value["input"] = input;
                    } else if active.value["input"]
                        .as_object()
                        .is_some_and(|v| v.is_empty())
                    {
                        events.push(ModelEvent::ToolCallDelta {
                            index,
                            id: None,
                            name: None,
                            arguments: "{}".into(),
                        });
                    }
                }
                self.blocks.push(active.value);
            }
            "message_delta" => {
                if self.active.is_some() || !value["delta"].is_object() {
                    return Err(invalid("message delta while a block is incomplete"));
                }
                refusal(&value["delta"])?;
                if let Some(reason) = value["delta"].get("stop_reason").filter(|v| !v.is_null()) {
                    let reason = reason
                        .as_str()
                        .ok_or_else(|| invalid("invalid stop reason"))?;
                    check_stop(reason)?;
                    if self.stop.as_ref().is_some_and(|old| old != reason) {
                        return Err(invalid("stop reason changed"));
                    }
                    self.stop = Some(reason.into());
                }
                if let Some(usage) = value.get("usage").filter(|v| !v.is_null()) {
                    self.meter.update(usage)?;
                }
                if let Some(usage) = self.meter.usage()? {
                    events.push(ModelEvent::Usage { usage });
                }
            }
            "message_stop" => {
                if self.active.is_some() {
                    return Err(invalid("message_stop with unfinished block"));
                }
                let reason = self
                    .stop
                    .as_deref()
                    .ok_or_else(|| invalid("missing final stop reason"))?;
                let reply = assemble(&self.blocks, reason, self.meter.usage()?)?;
                self.finished = true;
                events.push(ModelEvent::Reply { reply });
            }
            _ => return Err(invalid("unsupported stream event")),
        }
        Ok(events)
    }
}
