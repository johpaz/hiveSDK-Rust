//! Rust HTTP transport. Credentials never enter serialized engine state.
use crate::{AttachmentKind, Error, Message, ModelReply, ModelRequest, Result, ToolCall, Usage};
pub use profile::{OpenAiProfile, Preset, presets};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, mpsc};
pub use tokio_util::sync::CancellationToken;

pub mod anthropic;
pub mod gemini;
pub mod hiveagents;
pub mod profile;
mod resilience;
pub mod streaming;

#[derive(Clone, Deserialize)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "default_response_limit")]
    pub max_response_bytes: usize,
    #[serde(default)]
    pub output_token_parameter: OutputTokenParameter,
}
#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputTokenParameter {
    #[default]
    MaxCompletionTokens,
    MaxTokens,
}
fn default_timeout() -> u64 {
    60_000
}
fn default_response_limit() -> usize {
    16 * 1024 * 1024
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelEvent {
    TextDelta {
        text: String,
    },
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    Usage {
        usage: Usage,
    },
    Reply {
        reply: ModelReply,
    },
}

pub struct ModelStream {
    receiver: Mutex<mpsc::Receiver<Result<ModelEvent>>>,
    cancellation: CancellationToken,
}
impl ModelStream {
    pub fn cancel(&self) {
        self.cancellation.cancel();
    }
    pub async fn next(&self) -> Result<Option<ModelEvent>> {
        let mut receiver = self
            .receiver
            .try_lock()
            .map_err(|_| Error::State("only one stream read can be pending".into()))?;
        tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => Err(Error::Cancelled),
            event = receiver.recv() => event.transpose(),
        }
    }
}
impl Drop for ModelStream {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub struct OpenAiCompatible {
    inner: Arc<HttpProvider>,
}
impl OpenAiCompatible {
    pub fn new(config: OpenAiConfig) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(HttpProvider::new(config, Protocol::OpenAi)?),
        })
    }
    pub fn from_json(config: &str) -> Result<Self> {
        Self::new(serde_json::from_str(config)?)
    }
    /// Same transport with a provider profile (tool-name rules, schema
    /// adjustments, headers). See [`profile`].
    pub fn with_profile(config: OpenAiConfig, profile: OpenAiProfile) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(HttpProvider::new(config, Protocol::OpenAi)?.with_profile(profile)?),
        })
    }
    pub fn start(self: &Arc<Self>, request: ModelRequest, stream: bool) -> ModelStream {
        self.inner.start(request, stream)
    }
}

/// Binding entry point; untagged historical config retains Chat Completions behavior.
pub struct NativeProvider {
    inner: Arc<HttpProvider>,
}
impl NativeProvider {
    /// Host-supplied provider headers (for example HiveDB's encrypted headers).
    pub fn with_headers(
        mut self,
        headers: std::collections::HashMap<String, String>,
    ) -> Result<Self> {
        let mut parsed = reqwest::header::HeaderMap::new();
        for (name, value) in headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| Error::Input("invalid provider header name".into()))?;
            if self.inner.managed.is_some() && hiveagents::blocked_header(name.as_str()) {
                continue;
            }
            if matches!(
                name.as_str(),
                "host" | "content-length" | "transfer-encoding" | "connection"
            ) {
                return Err(Error::Input(
                    "transport headers cannot be overridden".into(),
                ));
            }
            let value = reqwest::header::HeaderValue::from_str(&value)
                .map_err(|_| Error::Input("invalid provider header value".into()))?;
            parsed.insert(name, value);
        }
        Arc::get_mut(&mut self.inner)
            .ok_or_else(|| Error::State("provider already shared".into()))?
            .headers = parsed;
        Ok(self)
    }
    pub fn from_json(config: &str) -> Result<Self> {
        let mut config: Value = serde_json::from_str(config)?;
        let object = config
            .as_object_mut()
            .ok_or_else(|| Error::Input("provider config must be an object".into()))?;
        if let Some(preset) = object.remove("preset") {
            let id = preset
                .as_str()
                .ok_or_else(|| Error::Input("preset must be a string".into()))?;
            profile::expand(object, id)?;
        }
        let kind = object.remove("kind");
        let profile = object.remove("profile");
        let resilience = object.remove("resilience").filter(|value| !value.is_null());
        let is_openai =
            kind.is_none() || kind.as_ref().and_then(Value::as_str) == Some("openai_compatible");
        if profile.is_some() && !is_openai {
            return Err(Error::Input(
                "profile applies only to OpenAI-compatible providers".into(),
            ));
        }
        let inner = match kind.as_ref().and_then(Value::as_str) {
            None if kind.is_none() => {
                OpenAiCompatible::with_profile(
                    serde_json::from_value(config)?,
                    parse_profile(profile)?,
                )?
                .inner
            }
            Some("openai_compatible") => {
                OpenAiCompatible::with_profile(
                    serde_json::from_value(config)?,
                    parse_profile(profile)?,
                )?
                .inner
            }
            Some("anthropic") => anthropic::Anthropic::new(serde_json::from_value(config)?)?.inner,
            Some("gemini") => gemini::Gemini::new(serde_json::from_value(config)?)?.inner,
            Some("hiveagents") => hiveagents::create(serde_json::from_value(config)?)?.inner,
            _ => return Err(Error::Input("unknown native provider kind".into())),
        };
        let mut inner = inner;
        if let Some(resilience) = resilience {
            // HiveAgents manages its own backend lifecycle and is never retried.
            if kind.as_ref().and_then(Value::as_str) == Some("hiveagents") {
                return Err(Error::Input(
                    "resilience does not apply to the HiveAgents provider".into(),
                ));
            }
            Arc::get_mut(&mut inner)
                .ok_or_else(|| Error::State("provider already shared".into()))?
                .resilience = Some(resilience::Resilience::from_value(resilience)?);
        }
        Ok(Self { inner })
    }
    pub fn start(&self, request: ModelRequest, stream: bool) -> ModelStream {
        self.inner.start(request, stream)
    }
}

enum Protocol {
    OpenAi,
    Anthropic {
        version: String,
        default_max_output_tokens: u64,
    },
    Gemini {
        default_max_output_tokens: u64,
    },
}
struct HttpProvider {
    managed: Option<hiveagents::Managed>,
    client: reqwest::Client,
    config: OpenAiConfig,
    protocol: Protocol,
    headers: reqwest::header::HeaderMap,
    profile: OpenAiProfile,
    /// Profile-owned headers; host headers from `with_headers` take precedence.
    profile_headers: reqwest::header::HeaderMap,
    /// Explicit retries and circuit breaker; absent unless configured.
    resilience: Option<resilience::Resilience>,
}
impl HttpProvider {
    fn new(config: OpenAiConfig, protocol: Protocol) -> Result<Self> {
        if config.timeout_ms == 0 || config.max_response_bytes == 0 {
            return Err(Error::Input(
                "timeout_ms and max_response_bytes must be positive".into(),
            ));
        }
        let url = reqwest::Url::parse(&config.base_url)
            .map_err(|_| Error::Input("invalid provider base_url".into()))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::Input(
                "base_url must be HTTP(S) without credentials, query, or fragment".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| Error::Provider("cannot initialize HTTP client".into()))?;
        Ok(Self {
            managed: None,
            client,
            config,
            protocol,
            headers: reqwest::header::HeaderMap::new(),
            profile: OpenAiProfile::default(),
            profile_headers: reqwest::header::HeaderMap::new(),
            resilience: None,
        })
    }

    fn with_profile(mut self, profile: OpenAiProfile) -> Result<Self> {
        profile.validate()?;
        if let Some(name) = &profile.session_header {
            // One stable identifier per provider instance, never derived from user data.
            self.profile_headers.insert(
                reqwest::header::HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| Error::Input("invalid profile session_header".into()))?,
                reqwest::header::HeaderValue::from_str(&uuid::Uuid::new_v4().simple().to_string())
                    .map_err(|_| Error::Input("invalid session identifier".into()))?,
            );
        }
        if let Some(agent) = &profile.user_agent {
            self.profile_headers.insert(
                reqwest::header::USER_AGENT,
                reqwest::header::HeaderValue::from_str(agent)
                    .map_err(|_| Error::Input("invalid profile user_agent".into()))?,
            );
        }
        self.profile = profile;
        Ok(self)
    }

    /// Must be called inside a Tokio runtime. The receiver applies bounded backpressure.
    pub fn start(self: &Arc<Self>, request: ModelRequest, stream: bool) -> ModelStream {
        let (sender, receiver) = mpsc::channel(32);
        let cancellation = CancellationToken::new();
        let token = cancellation.clone();
        let provider = self.clone();
        tokio::spawn(async move {
            let remaining = request
                .deadline_unix_ms
                .map(|deadline| crate::budget::now_ms().map(|now| deadline.saturating_sub(now)))
                .transpose();
            let remaining = match remaining {
                Ok(value) => value,
                Err(error) => {
                    let _ = sender.send(Err(error)).await;
                    return;
                }
            };
            if remaining == Some(0) {
                let _ = sender.send(Err(Error::DeadlineExceeded)).await;
                return;
            }
            let global_first = remaining.is_some_and(|left| left <= provider.config.timeout_ms);
            let timeout_ms = remaining.map_or(provider.config.timeout_ms, |left| {
                left.min(provider.config.timeout_ms)
            });
            let result = tokio::select! {
                biased;
                _ = token.cancelled() => return,
                result = tokio::time::timeout(Duration::from_millis(timeout_ms),
                    provider.execute(&request, stream, &sender)) => match result {
                        Ok(result) => result,
                        Err(_) => Err(if global_first { Error::DeadlineExceeded } else { Error::Timeout }),
                    }
            };
            if let Err(error) = result {
                tokio::select! { biased;
                    _ = token.cancelled() => {},
                    _ = sender.send(Err(error)) => {},
                }
            }
        });
        ModelStream {
            receiver: Mutex::new(receiver),
            cancellation,
        }
    }

    async fn execute(
        &self,
        request: &ModelRequest,
        stream: bool,
        sender: &mpsc::Sender<Result<ModelEvent>>,
    ) -> Result<()> {
        if let Some(output) = &request.structured_output {
            output.validate()?;
        }
        if let Some(estimate) = request
            .context
            .as_ref()
            .and_then(|context| context.token_estimate.as_ref())
        {
            use crate::InputTokenFraming;
            let expected = match self.protocol {
                Protocol::OpenAi => InputTokenFraming::OpenaiChatJson,
                Protocol::Anthropic { .. } => InputTokenFraming::AnthropicMessagesJson,
                Protocol::Gemini { .. } => InputTokenFraming::GeminiGenerateContentJson,
            };
            if estimate.framing != InputTokenFraming::NeutralJson && estimate.framing != expected {
                return Err(Error::Input(
                    "input token framing does not match the provider protocol".into(),
                ));
            }
        }
        if !matches!(self.protocol, Protocol::Gemini { .. })
            && request
                .messages
                .iter()
                .any(|message| message.provider_state.is_some())
        {
            return Err(Error::Input(
                "provider state cannot be replayed through a different protocol".into(),
            ));
        }
        if matches!(self.protocol, Protocol::OpenAi)
            && !request.tools.is_empty()
            && !profile::supports_tools(&self.profile, &request.model)
        {
            return Err(Error::Input(format!(
                "model `{}` does not support tools for this provider",
                request.model
            )));
        }
        let mut restore = profile::NameMap::new();
        let mut body = match &self.protocol {
            Protocol::OpenAi => {
                let mut body = request_body(request)?;
                restore = profile::apply(&self.profile, request, &mut body)?;
                body
            }
            Protocol::Anthropic {
                default_max_output_tokens,
                ..
            } => anthropic::request_body(request, *default_max_output_tokens)?,
            Protocol::Gemini {
                default_max_output_tokens,
            } => gemini::request_body(request, *default_max_output_tokens)?,
        };
        if request.max_output_tokens == Some(0) {
            return Err(Error::Input("output token limit must be positive".into()));
        }
        if matches!(self.protocol, Protocol::OpenAi)
            && matches!(
                self.config.output_token_parameter,
                OutputTokenParameter::MaxTokens
            )
            && let Some(cap) = body
                .as_object_mut()
                .and_then(|body| body.remove("max_completion_tokens"))
        {
            body["max_tokens"] = cap;
        }
        if !matches!(self.protocol, Protocol::Gemini { .. }) {
            body["stream"] = json!(stream);
        }
        if stream && matches!(self.protocol, Protocol::OpenAi) {
            body["stream_options"] = json!({"include_usage":true});
        }
        let _backend_lease = if let Some(managed) = &self.managed {
            let lease = managed.lease.lock().await;
            hiveagents::prepare(self, managed, &request.model).await?;
            body["model"] = json!("local");
            Some(lease)
        } else {
            None
        };
        let route = match self.protocol {
            Protocol::OpenAi => "chat/completions".into(),
            Protocol::Anthropic { .. } => "messages".into(),
            Protocol::Gemini { .. } => gemini::route(&request.model, stream)?,
        };
        let endpoint = format!("{}/{route}", self.config.base_url.trim_end_matches('/'));
        let build = || {
            let mut call = self.client.post(&endpoint).json(&body);
            if let Protocol::Anthropic { version, .. } = &self.protocol {
                call = call.header("anthropic-version", version);
            }
            if let Some(key) = &self.config.api_key {
                call = if matches!(self.protocol, Protocol::Gemini { .. }) {
                    call.header("x-goog-api-key", key)
                } else {
                    call.bearer_auth(key)
                };
            }
            call.headers(self.profile_headers.clone())
                .headers(self.headers.clone())
        };
        // Only a request that provably was not processed is ever repeated: a
        // connection that never opened, or the server's own 429/503. Everything else
        // (timeouts, other errors, anything that started to arrive) may have been
        // processed and is reported as it happened.
        let resilience = self.resilience.as_ref();
        let mut retries = 0u32;
        let mut response = loop {
            if let Some(resilience) = resilience {
                resilience.admit()?;
            }
            let sent = build().send().await;
            let Some(resilience) = resilience else {
                break sent.map_err(http_error)?;
            };
            let (health, refusal) = match &sent {
                Ok(response) if matches!(response.status().as_u16(), 429 | 503) => (
                    resilience::Health::Failing,
                    Some(resilience::retry_after(response.headers())),
                ),
                Ok(response) if response.status().is_server_error() => {
                    (resilience::Health::Failing, None)
                }
                Ok(_) => (resilience::Health::Reachable, None),
                Err(error) if error.is_connect() => (resilience::Health::Failing, Some(None)),
                Err(_) => (resilience::Health::Failing, None),
            };
            resilience.record(health);
            if let Some(asked) = refusal
                && retries + 1 < resilience.retry.max_attempts
                && let Some(wait) = resilience.delay(retries, asked)
            {
                retries += 1;
                tokio::time::sleep(wait).await;
                continue;
            }
            break sent.map_err(http_error)?;
        };
        if !response.status().is_success() {
            if self.managed.is_some() {
                return Err(Error::Provider(format!(
                    "HiveAgents inference for model {} returned HTTP {}; check model readiness and backend availability (request not retried)",
                    request.model,
                    response.status().as_u16()
                )));
            }
            return Err(Error::Provider(format!(
                "HTTP {}",
                response.status().as_u16()
            )));
        }
        if !stream {
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(http_error)? {
                if bytes.len().saturating_add(chunk.len()) > self.config.max_response_bytes {
                    return Err(Error::Provider("response exceeds byte limit".into()));
                }
                bytes.extend_from_slice(&chunk);
            }
            let body = serde_json::from_slice(&bytes)
                .map_err(|_| Error::Provider("invalid response JSON".into()))?;
            let mut reply = match self.protocol {
                Protocol::OpenAi => parse_reply(body)?,
                Protocol::Anthropic { .. } => anthropic::parse_reply(body)?,
                Protocol::Gemini { .. } => gemini::parse_reply(body)?,
            };
            profile::restore_names(&mut reply, &restore);
            return send(sender, ModelEvent::Reply { reply }).await;
        }
        let mut decoder = streaming::SseDecoder::default();
        let mut accumulator = streaming::Accumulator::default();
        let mut anthropic_accumulator = anthropic::Accumulator::default();
        let mut gemini_accumulator = gemini::Accumulator::default();
        let mut received = 0usize;
        while let Some(chunk) = response.chunk().await.map_err(http_error)? {
            received = received.saturating_add(chunk.len());
            if received > self.config.max_response_bytes {
                return Err(Error::Provider("response exceeds byte limit".into()));
            }
            for frame in decoder.feed(&chunk)? {
                if matches!(self.protocol, Protocol::OpenAi) && frame.trim() == "[DONE]" {
                    let mut reply = accumulator.finish()?;
                    profile::restore_names(&mut reply, &restore);
                    send(sender, ModelEvent::Reply { reply }).await?;
                    return Ok(());
                }
                let value = serde_json::from_str(&frame)
                    .map_err(|_| Error::Provider("invalid SSE JSON".into()))?;
                let events = match self.protocol {
                    Protocol::OpenAi => accumulator.push(value)?,
                    Protocol::Anthropic { .. } => anthropic_accumulator.push(value)?,
                    Protocol::Gemini { .. } => gemini_accumulator.push(value)?,
                };
                let complete = events
                    .iter()
                    .any(|event| matches!(event, ModelEvent::Reply { .. }));
                for mut event in events {
                    // Fragments in ToolCallDelta keep the wire name; only the
                    // completed reply is mapped back to the original tool name.
                    if let ModelEvent::Reply { reply } = &mut event {
                        profile::restore_names(reply, &restore);
                    }
                    send(sender, event).await?;
                }
                if complete {
                    return Ok(());
                }
            }
        }
        if matches!(self.protocol, Protocol::Gemini { .. }) {
            decoder.finish()?;
            return send(
                sender,
                ModelEvent::Reply {
                    reply: gemini_accumulator.finish()?,
                },
            )
            .await;
        }
        Err(Error::Provider(match self.protocol {
            Protocol::OpenAi => "stream ended before [DONE]".into(),
            Protocol::Anthropic { .. } => "stream ended before message_stop".into(),
            Protocol::Gemini { .. } => unreachable!(),
        }))
    }
}

fn parse_profile(profile: Option<Value>) -> Result<OpenAiProfile> {
    profile
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| Error::Input(format!("invalid provider profile: {error}")))
        .map(Option::unwrap_or_default)
}

fn http_error(error: reqwest::Error) -> Error {
    if error.is_timeout() {
        Error::Timeout
    } else {
        Error::Provider("HTTP request failed".into())
    }
}
async fn send(sender: &mpsc::Sender<Result<ModelEvent>>, event: ModelEvent) -> Result<()> {
    sender.send(Ok(event)).await.map_err(|_| Error::Cancelled)
}

/// Chat Completions content array for a user message with attachments. Inline
/// audio and documents need base64; audio is limited to the formats the API names.
fn user_parts(message: &Message) -> Result<Value> {
    let unsupported = |what: &str| Error::Input(format!("OpenAI chat does not support {what}"));
    let mut parts = vec![json!({"type":"text","text":message.content})];
    for attachment in &message.attachments {
        parts.push(match attachment.kind {
            AttachmentKind::Image => json!({"type":"image_url","image_url":{
                "url": attachment.url.clone().or_else(|| attachment.data_url())}}),
            AttachmentKind::Audio => {
                let format = match attachment.media_type.as_str() {
                    "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
                    "audio/mpeg" | "audio/mp3" => "mp3",
                    _ => return Err(unsupported("this audio media type (use wav or mp3)")),
                };
                let data = attachment
                    .data
                    .as_ref()
                    .ok_or_else(|| unsupported("audio by URL"))?;
                json!({"type":"input_audio","input_audio":{"data":data,"format":format}})
            }
            AttachmentKind::Document => {
                let file_data = attachment
                    .data_url()
                    .ok_or_else(|| unsupported("documents by URL"))?;
                json!({"type":"file","file":{
                    "filename": attachment.name.as_deref().unwrap_or("document"),
                    "file_data": file_data}})
            }
        });
    }
    Ok(Value::Array(parts))
}

/// Convert the neutral contract into Chat Completions wire messages.
pub fn request_body(request: &ModelRequest) -> Result<Value> {
    crate::media::ensure_user_only(&request.messages)?;
    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(|m| {
            let mut v = json!({ "role": m.role, "content": m.content });
            if !m.attachments.is_empty() {
                if m.role != "user" {
                    return Err(Error::Input(
                        "only user messages can carry attachments".into(),
                    ));
                }
                v["content"] = user_parts(m)?;
            }
            if let Some(id) = &m.tool_call_id {
                v["tool_call_id"] = json!(id);
            }
            if !m.tool_calls.is_empty() {
                v["tool_calls"] = json!(
                    m.tool_calls
                        .iter()
                        .map(|c| json!({
                            "id": c.id, "type": "function", "function": {
                                "name": c.name, "arguments": c.arguments.to_string()
                            }
                        }))
                        .collect::<Vec<_>>()
                );
            }
            Ok(v)
        })
        .collect::<Result<_>>()?;
    let mut body = json!({ "model": request.model, "messages": messages, "stream": false });
    if let Some(cap) = request.max_output_tokens {
        body["max_completion_tokens"] = json!(cap);
    }
    if let Some(output) = &request.structured_output {
        body["response_format"] = json!({"type":"json_schema","json_schema":{"name":output.name,"schema":output.schema,"strict":true}});
    }
    if !request.tools.is_empty() {
        body["tools"] = json!(
            request
                .tools
                .iter()
                .map(|t| json!({
                    "type": "function", "function": { "name": t.name,
                        "description": t.description, "parameters": t.parameters }
                }))
                .collect::<Vec<_>>()
        );
    }
    Ok(body)
}

pub fn parse_reply(body: Value) -> Result<ModelReply> {
    let choice = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
        .ok_or_else(|| Error::Provider("missing response choice".into()))?;
    let usage = body.get("usage").and_then(|u| {
        Some(Usage {
            input_tokens: u.get("prompt_tokens")?.as_u64()?,
            output_tokens: u.get("completion_tokens")?.as_u64()?,
        })
    });
    if let Some(reason) = choice.get("finish_reason").filter(|value| !value.is_null()) {
        let reason = reason
            .as_str()
            .ok_or_else(|| Error::Provider("invalid finish_reason".into()))?;
        if !matches!(reason, "stop" | "tool_calls") {
            return Err(streaming::terminal_error(reason, usage.as_ref()));
        }
    }
    let message = &choice["message"];
    let mut calls = vec![];
    if let Some(raw) = message.get("tool_calls").filter(|v| !v.is_null()) {
        for c in raw
            .as_array()
            .ok_or_else(|| Error::Provider("invalid tool_calls".into()))?
        {
            let text = |v: &Value| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| Error::Provider("invalid tool call fields".into()))
            };
            calls.push(ToolCall {
                id: text(&c["id"])?,
                name: text(&c["function"]["name"])?,
                arguments: serde_json::from_str(&text(&c["function"]["arguments"])?)?,
            });
        }
    }
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if content.is_none() && calls.is_empty() {
        return Err(Error::Provider(
            "response has neither text nor tool calls".into(),
        ));
    }
    Ok(ModelReply {
        provider_state: None,
        content,
        tool_calls: calls,
        usage,
    })
}
