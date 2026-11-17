//! Ollama's native chat API (`POST {base_url}/chat`, newline-delimited JSON when streaming).
//!
//! Used instead of Ollama's OpenAI-compatible endpoint when a host wants what only the native API
//! gives: the context size (`num_ctx`, which Ollama otherwise cuts to a small default and silently
//! truncates the prompt to), `keep_alive`, `think` and structured output as a plain schema. The base
//! URL ends in `/api` (`http://localhost:11434/api`).
//!
//! Tool calls arrive whole (not in fragments) and carry no stable id on older servers, so ids are
//! made here; a tool result is matched to its call by position and sent with the tool's name.
//! Images travel as inline base64 on user messages; audio, documents and image URLs are refused
//! rather than dropped. The model's `thinking` text is not kept or replayed.
use super::{HttpProvider, ModelEvent, ModelStream, OpenAiConfig, Protocol, send};
use crate::{Error, Message, ModelReply, ModelRequest, Result, ToolCall, Usage};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OllamaConfig {
    /// For example `http://localhost:11434/api`.
    pub base_url: String,
    /// Only for hosted Ollama; a local server needs none.
    pub api_key: Option<String>,
    #[serde(default = "super::default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "super::default_response_limit")]
    pub max_response_bytes: usize,
    /// Output limit for a request that names none. Without it, one is derived from
    /// the window it asks for (`num_ctx`, else `context_window`; 15% of it), else 4096.
    #[serde(default)]
    pub default_max_output_tokens: Option<u64>,
    /// The model's context window, for deriving the output limit when `num_ctx` (the window actually
    /// asked for, which may be smaller) is absent.
    #[serde(default)]
    pub context_window: Option<u64>,
    /// Context window to ask for (`options.num_ctx`). Without it Ollama uses its own default.
    #[serde(default)]
    pub num_ctx: Option<u64>,
    /// How long the model stays loaded after the call, such as `"5m"` or `"0"`.
    #[serde(default)]
    pub keep_alive: Option<String>,
    /// `false` turns thinking off for models that think; `true` turns it on.
    #[serde(default)]
    pub think: Option<bool>,
}

/// What the request builder needs from the configuration.
#[derive(Clone, Debug, Default)]
pub(super) struct Options {
    pub default_max_output_tokens: u64,
    pub num_ctx: Option<u64>,
    pub keep_alive: Option<String>,
    pub think: Option<bool>,
}

pub struct Ollama {
    pub(super) inner: Arc<HttpProvider>,
}
impl Ollama {
    pub fn new(config: OllamaConfig) -> Result<Self> {
        let default_max_output_tokens = super::resolve_output_cap(
            config.default_max_output_tokens,
            config.num_ctx.or(config.context_window),
        )?;
        if config.num_ctx == Some(0)
            || config.keep_alive.as_ref().is_some_and(|k| {
                k.is_empty()
                    || k.len() > 16
                    || !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
            })
        {
            return Err(Error::Input(
                "Ollama default_max_output_tokens and num_ctx must be positive and keep_alive a duration such as 5m".into(),
            ));
        }
        let protocol = Protocol::Ollama(Options {
            default_max_output_tokens,
            num_ctx: config.num_ctx,
            keep_alive: config.keep_alive,
            think: config.think,
        });
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
    Error::Provider(format!("Ollama: {message}"))
}

pub(super) fn request_body(request: &ModelRequest, options: &Options) -> Result<Value> {
    crate::media::ensure_user_only(&request.messages)?;
    if let Some(output) = &request.structured_output {
        output.validate()?;
    }
    if request.model.is_empty()
        || request.model.len() > 256
        || request.model.chars().any(char::is_control)
    {
        return Err(Error::Input("Ollama model name is not valid".into()));
    }
    let cap = request
        .max_output_tokens
        .unwrap_or(options.default_max_output_tokens);
    if cap == 0 {
        return Err(Error::Input("Ollama output cap must be positive".into()));
    }
    let history: Vec<Message> = super::sanitize_orphan_tool_calls(&request.messages);
    let mut names: HashMap<&str, &str> = HashMap::new();
    let mut messages = vec![];
    for message in &history {
        match message.role.as_str() {
            "system" | "user" => {
                let mut wire = json!({"role":message.role,"content":message.content.clone().unwrap_or_default()});
                let mut images = vec![];
                for attachment in &message.attachments {
                    match (attachment.kind, &attachment.data) {
                        (crate::AttachmentKind::Image, Some(data)) => images.push(json!(data)),
                        (crate::AttachmentKind::Image, None) => {
                            return Err(Error::Input(
                                "Ollama images must be inline base64 data, not URLs".into(),
                            ));
                        }
                        _ => {
                            return Err(Error::Input(
                                "Ollama chat accepts only images as attachments".into(),
                            ));
                        }
                    }
                }
                if !images.is_empty() {
                    wire["images"] = Value::Array(images);
                }
                messages.push(wire);
            }
            "assistant" => {
                let mut wire = json!({"role":"assistant","content":message.content.clone().unwrap_or_default()});
                if !message.tool_calls.is_empty() {
                    wire["tool_calls"] = message
                        .tool_calls
                        .iter()
                        .map(|call| json!({"function":{"name":call.name,"arguments":call.arguments}}))
                        .collect();
                    for call in &message.tool_calls {
                        names.insert(call.id.as_str(), call.name.as_str());
                    }
                }
                messages.push(wire);
            }
            "tool" => {
                let name = message
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| names.get(id))
                    .ok_or_else(|| Error::Input("orphan Ollama tool result".into()))?;
                messages.push(json!({"role":"tool","tool_name":name,"content":message.content.clone().unwrap_or_default()}));
            }
            _ => return Err(Error::Input("unsupported message role".into())),
        }
    }
    let mut wire_options = json!({"num_predict":cap});
    if let Some(ctx) = options.num_ctx {
        wire_options["num_ctx"] = json!(ctx);
    }
    let mut body = json!({"model":request.model,"messages":messages,"options":wire_options});
    if !request.tools.is_empty() {
        body["tools"] = request
            .tools
            .iter()
            .map(|tool| json!({"type":"function","function":{"name":tool.name,"description":tool.description,"parameters":tool.parameters}}))
            .collect();
    }
    if let Some(output) = &request.structured_output {
        body["format"] = output.schema.clone();
    }
    if let Some(think) = options.think {
        body["think"] = json!(think);
    }
    if let Some(keep) = &options.keep_alive {
        body["keep_alive"] = json!(keep);
    }
    Ok(body)
}

fn usage(value: &Value) -> Result<Option<Usage>> {
    let count = |key: &str| -> Result<Option<u64>> {
        match value.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(n) => n
                .as_u64()
                .map(Some)
                .ok_or_else(|| invalid("invalid token count")),
        }
    };
    Ok(match (count("prompt_eval_count")?, count("eval_count")?) {
        (None, None) => None,
        (input, output) => Some(Usage {
            input_tokens: input.unwrap_or(0),
            output_tokens: output.unwrap_or(0),
        }),
    })
}

fn tool_call(value: &Value) -> Result<ToolCall> {
    let function = value
        .get("function")
        .ok_or_else(|| invalid("tool call without a function"))?;
    let name = function["name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| invalid("tool call without a name"))?;
    let arguments = match &function["arguments"] {
        Value::Object(_) => function["arguments"].clone(),
        Value::Null => json!({}),
        // Some servers and models hand the arguments over as JSON text.
        Value::String(text) => serde_json::from_str(text)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| invalid("tool arguments are not a JSON object"))?,
        _ => return Err(invalid("tool arguments are not a JSON object")),
    };
    // Servers that name the call keep their id; the rest get one that cannot repeat in a conversation.
    let id = value["id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_graphic()))
        .map_or_else(
            || format!("call_{}", uuid::Uuid::new_v4().simple()),
            str::to_owned,
        );
    Ok(ToolCall {
        id,
        name: name.into(),
        arguments,
    })
}

/// Only a normal stop is a complete answer: a cut-off one (`length`) or an unload is refused.
fn check_done(value: &Value) -> Result<()> {
    match value.get("done_reason").and_then(Value::as_str) {
        None | Some("stop") => Ok(()),
        Some("length") => Err(invalid("the answer was truncated by the output limit")),
        Some(_) => Err(invalid("unexpected end of the answer")),
    }
}

pub fn parse_reply(body: Value) -> Result<ModelReply> {
    if body.get("error").is_some() {
        // The text is the server's; it is not echoed.
        return Err(invalid("the server reported an error"));
    }
    if body["done"] != true {
        return Err(invalid("incomplete response"));
    }
    check_done(&body)?;
    let message = &body["message"];
    let content = message["content"]
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    let tool_calls = message["tool_calls"]
        .as_array()
        .map(|calls| calls.iter().map(tool_call).collect::<Result<Vec<_>>>())
        .transpose()?
        .unwrap_or_default();
    if content.is_none() && tool_calls.is_empty() {
        return Err(invalid("the answer has neither text nor tool calls"));
    }
    Ok(ModelReply {
        provider_state: None,
        content,
        tool_calls,
        usage: usage(&body)?,
    })
}

/// Splits a byte stream into lines, refusing a line longer than the response limit.
#[derive(Default)]
pub(super) struct LineDecoder {
    buffer: Vec<u8>,
}
impl LineDecoder {
    pub(super) fn feed(&mut self, chunk: &[u8], limit: usize) -> Result<Vec<String>> {
        self.buffer.extend_from_slice(chunk);
        let mut lines = vec![];
        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=end).collect();
            let text = std::str::from_utf8(&line[..line.len() - 1])
                .map_err(|_| invalid("invalid UTF-8 in the stream"))?;
            let text = text.trim_end_matches('\r');
            if !text.trim().is_empty() {
                lines.push(text.to_owned());
            }
        }
        if self.buffer.len() > limit {
            return Err(Error::Provider("response exceeds byte limit".into()));
        }
        Ok(lines)
    }
    pub(super) fn finish(&mut self) -> Result<Option<String>> {
        let rest = std::mem::take(&mut self.buffer);
        let text = std::str::from_utf8(&rest)
            .map_err(|_| invalid("invalid UTF-8 in the stream"))?
            .trim();
        Ok((!text.is_empty()).then(|| text.to_owned()))
    }
}

/// Reads the NDJSON stream of one answer and sends its events. The reply is sent once, when the
/// server says `done`; a stream that ends before that is an error, never a partial answer.
pub(super) async fn stream(
    mut response: reqwest::Response,
    limit: usize,
    sender: &mpsc::Sender<Result<ModelEvent>>,
) -> Result<()> {
    let mut decoder = LineDecoder::default();
    let mut text = String::new();
    let mut calls: Vec<ToolCall> = vec![];
    let mut received = 0usize;
    let handle = |line: &str,
                  text: &mut String,
                  calls: &mut Vec<ToolCall>|
     -> Result<(Vec<ModelEvent>, Option<ModelReply>)> {
        let value: Value =
            serde_json::from_str(line).map_err(|_| invalid("invalid JSON in the stream"))?;
        if value.get("error").is_some() {
            return Err(invalid("the server reported an error"));
        }
        let mut events = vec![];
        let message = &value["message"];
        if let Some(delta) = message["content"].as_str().filter(|d| !d.is_empty()) {
            text.push_str(delta);
            events.push(ModelEvent::TextDelta {
                text: delta.to_owned(),
            });
        }
        if let Some(found) = message["tool_calls"].as_array() {
            for call in found {
                let call = tool_call(call)?;
                events.push(ModelEvent::ToolCallDelta {
                    index: calls.len(),
                    id: Some(call.id.clone()),
                    name: Some(call.name.clone()),
                    arguments: call.arguments.to_string(),
                });
                calls.push(call);
            }
        }
        if value["done"] != true {
            return Ok((events, None));
        }
        check_done(&value)?;
        if text.is_empty() && calls.is_empty() {
            return Err(invalid("the answer has neither text nor tool calls"));
        }
        let usage = usage(&value)?;
        if let Some(usage) = usage.clone() {
            events.push(ModelEvent::Usage { usage });
        }
        let reply = ModelReply {
            provider_state: None,
            content: (!text.is_empty()).then(|| std::mem::take(text)),
            tool_calls: std::mem::take(calls),
            usage,
        };
        Ok((events, Some(reply)))
    };
    while let Some(chunk) = response.chunk().await.map_err(super::http_error)? {
        received = received.saturating_add(chunk.len());
        if received > limit {
            return Err(Error::Provider("response exceeds byte limit".into()));
        }
        for line in decoder.feed(&chunk, limit)? {
            let (events, reply) = handle(&line, &mut text, &mut calls)?;
            for event in events {
                send(sender, event).await?;
            }
            if let Some(reply) = reply {
                return send(sender, ModelEvent::Reply { reply }).await;
            }
        }
    }
    if let Some(line) = decoder.finish()? {
        let (events, reply) = handle(&line, &mut text, &mut calls)?;
        for event in events {
            send(sender, event).await?;
        }
        if let Some(reply) = reply {
            return send(sender, ModelEvent::Reply { reply }).await;
        }
    }
    Err(Error::Provider(
        "stream ended before the answer was complete".into(),
    ))
}
