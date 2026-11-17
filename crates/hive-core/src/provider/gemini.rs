//! Gemini Developer API: text, complete function-call parts and durable signatures.
use super::{HttpProvider, ModelEvent, ModelStream, OpenAiConfig, Protocol};
use crate::{Error, ModelReply, ModelRequest, ProviderState, Result, ToolCall, Usage};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeminiConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    #[serde(default = "super::default_timeout")]
    pub timeout_ms: u64,
    #[serde(default = "super::default_response_limit")]
    pub max_response_bytes: usize,
    /// Output limit for a request that names none. Without it, one is derived from
    /// `context_window` (15%), else 4096.
    #[serde(default)]
    pub default_max_output_tokens: Option<u64>,
    #[serde(default)]
    pub context_window: Option<u64>,
}
pub struct Gemini {
    pub(super) inner: Arc<HttpProvider>,
}
impl Gemini {
    pub fn new(config: GeminiConfig) -> Result<Self> {
        let default_max_output_tokens =
            super::resolve_output_cap(config.default_max_output_tokens, config.context_window)?;
        let protocol = Protocol::Gemini {
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
    Error::Provider(format!("Gemini: {message}"))
}
// Only enum-shaped codes belong in diagnostics; never echo response text or
// free-form finishMessage/blockReasonMessage fields supplied by a proxy.
fn reason_code(value: &Value) -> &str {
    value
        .as_str()
        .filter(|code| {
            !code.is_empty()
                && code.len() <= 64
                && code
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        })
        .unwrap_or("INVALID")
}
fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| invalid("missing or invalid string field"))
}
pub fn route(model: &str, stream: bool) -> Result<String> {
    let model = model.strip_prefix("models/").unwrap_or(model);
    if model.is_empty()
        || !model
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    {
        return Err(Error::Input(
            "Gemini model must be a model ID or models/<ID>".into(),
        ));
    }
    Ok(format!(
        "models/{model}:{}",
        if stream {
            "streamGenerateContent?alt=sse"
        } else {
            "generateContent"
        }
    ))
}

fn parse_part(part: &Value, call_index: usize) -> Result<(Option<String>, Option<ToolCall>)> {
    let object = part.as_object().ok_or_else(|| invalid("invalid part"))?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "text" | "thought" | "thoughtSignature" | "functionCall"
        )
    }) {
        return Err(invalid("unsupported part field"));
    }
    if let Some(signature) = part.get("thoughtSignature")
        && !signature.is_string()
    {
        return Err(invalid("invalid thought signature"));
    }
    if let Some(thought) = part.get("thought")
        && !thought.is_boolean()
    {
        return Err(invalid("invalid thought flag"));
    }
    match (part.get("text"), part.get("functionCall")) {
        (Some(text), None) => {
            let text = text.as_str().ok_or_else(|| invalid("invalid text part"))?;
            if part["thought"].as_bool() != Some(true) {
                return Ok((Some(text.into()), None));
            }
            Ok((None, None))
        }
        (None, Some(call)) => {
            if part["thought"].as_bool() == Some(true) {
                return Err(invalid("function call marked as thought"));
            }
            if call.as_object().is_none_or(|o| {
                o.keys()
                    .any(|k| !matches!(k.as_str(), "id" | "name" | "args"))
            }) {
                return Err(invalid("unsupported or partial function call"));
            }
            let name = string(call, "name")?;
            let id = match call.get("id") {
                Some(value) => value
                    .as_str()
                    .ok_or_else(|| invalid("invalid function ID"))?
                    .to_owned(),
                None => format!("gemini-call-{call_index}"),
            };
            let arguments = call.get("args").cloned().unwrap_or_else(|| json!({}));
            if id.is_empty() || name.is_empty() || !arguments.is_object() {
                return Err(invalid("invalid or duplicate function call"));
            }
            Ok((
                None,
                Some(ToolCall {
                    id,
                    name: name.into(),
                    arguments,
                }),
            ))
        }
        (None, None) if part.get("thoughtSignature").is_some() => Ok((None, None)),
        _ => Err(invalid("part must contain text or a complete functionCall")),
    }
}
fn parse_parts(parts: &[Value]) -> Result<(Option<String>, Vec<ToolCall>)> {
    let mut content = None::<String>;
    let mut calls = vec![];
    let mut ids = HashSet::new();
    for part in parts {
        let (text, call) = parse_part(part, calls.len())?;
        if let Some(text) = text {
            content.get_or_insert_default().push_str(&text);
        }
        if let Some(call) = call {
            if !ids.insert(call.id.clone()) {
                return Err(invalid("duplicate function call ID"));
            }
            calls.push(call);
        }
    }
    if content.is_none() && calls.is_empty() {
        return Err(invalid("response contains no visible text or tool calls"));
    }
    Ok((content, calls))
}
pub fn validate_state(
    state: &ProviderState,
    content: &Option<String>,
    calls: &[ToolCall],
) -> Result<()> {
    let ProviderState::Gemini { version, parts } = state else {
        return Err(Error::Input(
            "provider state cannot be replayed through a different protocol".into(),
        ));
    };
    if *version != 1 {
        return Err(Error::Input("unsupported Gemini state version".into()));
    }
    let (actual_content, actual_calls) = parse_parts(parts)?;
    if &actual_content != content || actual_calls != calls {
        return Err(Error::Input(
            "Gemini state does not match neutral content and tool calls".into(),
        ));
    }
    Ok(())
}
fn append(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if let Some(last) = contents.last_mut().filter(|last| last["role"] == role) {
        last["parts"]
            .as_array_mut()
            .expect("constructed parts")
            .extend(parts);
    } else {
        contents.push(json!({"role":role,"parts":parts}));
    }
}
struct Pending {
    id: String,
    name: String,
    vendor_id: Option<Value>,
}
pub fn request_body(request: &ModelRequest, default_max_output_tokens: u64) -> Result<Value> {
    crate::media::ensure_user_only(&request.messages)?;
    if let Some(output) = &request.structured_output {
        output.validate()?;
    }
    route(&request.model, false)?;
    let cap = request
        .max_output_tokens
        .unwrap_or(default_max_output_tokens);
    if cap == 0 {
        return Err(Error::Input("Gemini output cap must be positive".into()));
    }
    let mut system = vec![];
    let mut contents = vec![];
    let mut pending = VecDeque::new();
    for message in &request.messages {
        if message.role != "assistant" && message.provider_state.is_some() {
            return Err(Error::Input(
                "provider state is only valid on assistant messages".into(),
            ));
        }
        match message.role.as_str() {
            "system" => {
                if !contents.is_empty()
                    || !message.tool_calls.is_empty()
                    || message.tool_call_id.is_some()
                {
                    return Err(Error::Input(
                        "Gemini system messages must be a text-only prefix".into(),
                    ));
                }
                system.push(json!({"text":message.content.as_ref().ok_or_else(||Error::Input("system text required".into()))?}));
            }
            "user" | "assistant" => {
                if !pending.is_empty()
                    || message.tool_call_id.is_some()
                    || (message.role == "user" && !message.tool_calls.is_empty())
                {
                    return Err(Error::Input(
                        "Gemini requires all tool results before the next message".into(),
                    ));
                }
                let parts = if let Some(state) = &message.provider_state {
                    validate_state(state, &message.content, &message.tool_calls)?;
                    // `validate_state` above already refused any state that is not Gemini's.
                    let ProviderState::Gemini { parts, .. } = state else {
                        unreachable!("validated")
                    };
                    parts.clone()
                } else {
                    let mut parts = vec![];
                    if !message.attachments.is_empty() && message.role != "user" {
                        return Err(Error::Input(
                            "only user messages can carry attachments".into(),
                        ));
                    }
                    // Only inline data is mapped: fileData URIs belong to the Files API.
                    for attachment in &message.attachments {
                        let data = attachment.data.as_ref().ok_or_else(|| {
                            Error::Input("Gemini attachments must be inline base64 data".into())
                        })?;
                        parts.push(
                            json!({"inlineData":{"mimeType":attachment.media_type,"data":data}}),
                        );
                    }
                    if let Some(text) = &message.content {
                        parts.push(json!({"text":text}));
                    }
                    for call in &message.tool_calls {
                        parts.push(json!({"functionCall":{"id":call.id,"name":call.name,"args":call.arguments}}));
                    }
                    parts
                };
                if parts.is_empty() {
                    return Err(Error::Input("Gemini message has no content".into()));
                }
                if message.role == "assistant" {
                    let (content, calls) = parse_parts(&parts)?;
                    if content != message.content || calls != message.tool_calls {
                        return Err(Error::Input("invalid Gemini assistant message".into()));
                    }
                    let mut wire_calls = parts.iter().filter_map(|part| part.get("functionCall"));
                    for call in &message.tool_calls {
                        let wire = wire_calls.next().unwrap();
                        pending.push_back(Pending {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            vendor_id: wire.get("id").cloned(),
                        });
                    }
                }
                append(
                    &mut contents,
                    if message.role == "assistant" {
                        "model"
                    } else {
                        "user"
                    },
                    parts,
                );
            }
            "tool" => {
                let call = pending
                    .pop_front()
                    .ok_or_else(|| Error::Input("orphan Gemini tool result".into()))?;
                if message.tool_call_id.as_deref() != Some(&call.id)
                    || !message.tool_calls.is_empty()
                {
                    return Err(Error::Input(
                        "Gemini tool results must match call order and IDs".into(),
                    ));
                }
                let text = message
                    .content
                    .as_deref()
                    .ok_or_else(|| Error::Input("tool result content required".into()))?;
                let result = serde_json::from_str::<Value>(text).unwrap_or_else(|_| json!(text));
                let result = if result.is_object() {
                    result
                } else {
                    json!({"result":result})
                };
                let mut response = json!({"name":call.name,"response":result});
                if let Some(id) = call.vendor_id {
                    response["id"] = id;
                }
                append(
                    &mut contents,
                    "user",
                    vec![json!({"functionResponse":response})],
                );
            }
            _ => return Err(Error::Input("unsupported Gemini message role".into())),
        }
    }
    if !pending.is_empty()
        || contents
            .first()
            .is_none_or(|content| content["role"] != "user")
    {
        return Err(Error::Input(
            "Gemini conversation needs user input and complete tool results".into(),
        ));
    }
    let mut body =
        json!({"contents":contents,"generationConfig":{"candidateCount":1,"maxOutputTokens":cap}});
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts":system});
    }
    if !request.tools.is_empty() {
        for tool in &request.tools {
            if tool.name.is_empty()
                || tool.name.len() > 128
                || !tool
                    .name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
            {
                return Err(Error::Input("invalid Gemini function name".into()));
            }
        }
        body["tools"] = json!([{"functionDeclarations":request.tools.iter().map(|tool|json!({"name":tool.name,"description":tool.description,"parametersJsonSchema":tool.parameters})).collect::<Vec<_>>()}]);
    }
    if let Some(output) = &request.structured_output {
        body["generationConfig"]["responseMimeType"] = json!("application/json");
        body["generationConfig"]["responseJsonSchema"] = output.schema.clone();
    }
    Ok(body)
}

#[derive(Default)]
struct Meter {
    prompt: Option<u64>,
    candidates: Option<u64>,
    thoughts: u64,
    tool_prompt: u64,
    cached: u64,
}
impl Meter {
    fn update(&mut self, value: &Value) -> Result<()> {
        if !value.is_object() {
            return Err(invalid("invalid usageMetadata"));
        }
        for (key, target) in [
            ("promptTokenCount", &mut self.prompt),
            ("candidatesTokenCount", &mut self.candidates),
        ] {
            if let Some(value) = value.get(key) {
                let next = value
                    .as_u64()
                    .ok_or_else(|| invalid("invalid token count"))?;
                if target.is_some_and(|old| next < old) {
                    return Err(invalid("usage counter decreased"));
                }
                *target = Some(next);
            }
        }
        for (key, target) in [
            ("thoughtsTokenCount", &mut self.thoughts),
            ("toolUsePromptTokenCount", &mut self.tool_prompt),
            ("cachedContentTokenCount", &mut self.cached),
        ] {
            if let Some(value) = value.get(key) {
                let next = value
                    .as_u64()
                    .ok_or_else(|| invalid("invalid token count"))?;
                if next < *target {
                    return Err(invalid("usage counter decreased"));
                }
                *target = next;
            }
        }
        Ok(())
    }
    fn usage(&self) -> Result<Option<Usage>> {
        let (Some(prompt), Some(candidates)) = (self.prompt, self.candidates) else {
            return Ok(None);
        };
        if self.cached > prompt {
            return Err(invalid("cached token count exceeds total prompt"));
        }
        let input_tokens = prompt
            .checked_add(self.tool_prompt)
            .ok_or_else(|| invalid("input token count overflow"))?;
        let output_tokens = candidates
            .checked_add(self.thoughts)
            .ok_or_else(|| invalid("output token count overflow"))?;
        Ok(Some(Usage {
            input_tokens,
            output_tokens,
        }))
    }
}
#[derive(Default)]
pub struct Accumulator {
    parts: Vec<Value>,
    meter: Meter,
    stopped: bool,
    tool_ids: HashSet<String>,
    tools: usize,
}
impl Accumulator {
    pub fn push(&mut self, value: Value) -> Result<Vec<ModelEvent>> {
        if !value.is_object() || value.get("error").is_some_and(|v| !v.is_null()) {
            return Err(invalid("provider error or invalid response"));
        }
        if let Some(reason) = value["promptFeedback"].get("blockReason")
            && reason.as_str() != Some("BLOCK_REASON_UNSPECIFIED")
        {
            return Err(invalid(&format!(
                "prompt blocked; blockReason={}",
                reason_code(reason)
            )));
        }
        let mut events = vec![];
        if let Some(usage) = value.get("usageMetadata").filter(|v| !v.is_null()) {
            self.meter.update(usage)?;
            if let Some(usage) = self.meter.usage()? {
                events.push(ModelEvent::Usage { usage });
            }
        }
        let candidates = match value.get("candidates") {
            Some(v) => v.as_array().ok_or_else(|| invalid("invalid candidates"))?,
            None if value.get("usageMetadata").is_some() => return Ok(events),
            None => return Err(invalid("missing candidates")),
        };
        if candidates.len() > 1 {
            return Err(invalid("only one candidate is supported"));
        }
        for candidate in candidates {
            if candidate
                .get("index")
                .is_some_and(|v| v.as_u64() != Some(0))
            {
                return Err(invalid("invalid candidate index"));
            }
            if candidate
                .get("citationMetadata")
                .is_some_and(|v| !v.is_null())
                || candidate
                    .get("groundingMetadata")
                    .is_some_and(|v| !v.is_null())
            {
                return Err(invalid("citations and grounding are not supported"));
            }
            if let Some(content) = candidate.get("content").filter(|v| !v.is_null()) {
                if self.stopped {
                    return Err(invalid("content after STOP"));
                }
                if content
                    .get("role")
                    .is_some_and(|v| v.as_str() != Some("model"))
                {
                    return Err(invalid("invalid candidate role"));
                }
                let parts = content["parts"]
                    .as_array()
                    .ok_or_else(|| invalid("invalid candidate parts"))?;
                for part in parts {
                    let (text, call) = parse_part(part, self.tools)?;
                    self.parts.push(part.clone());
                    if let Some(text) = text {
                        events.push(ModelEvent::TextDelta { text });
                    }
                    if let Some(call) = call {
                        if !self.tool_ids.insert(call.id.clone()) {
                            return Err(invalid("duplicate function call ID"));
                        }
                        events.push(ModelEvent::ToolCallDelta {
                            index: self.tools,
                            id: Some(call.id.clone()),
                            name: Some(call.name.clone()),
                            arguments: call.arguments.to_string(),
                        });
                        self.tools += 1;
                    }
                }
            }
            if let Some(reason) = candidate.get("finishReason").filter(|v| !v.is_null()) {
                if self.stopped {
                    return Err(invalid("duplicate finish reason after STOP"));
                }
                if reason.as_str() != Some("STOP") {
                    let code = reason_code(reason);
                    let hint = match code {
                        "MAX_TOKENS" => {
                            "output token limit reached (includes thinking); response not confirmed"
                        }
                        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => {
                            "response blocked; response not confirmed"
                        }
                        "MALFORMED_FUNCTION_CALL"
                        | "UNEXPECTED_TOOL_CALL"
                        | "TOO_MANY_TOOL_CALLS" => {
                            "invalid function call; no tools executed from this response"
                        }
                        _ => "unsupported terminal reason; response not confirmed",
                    };
                    let count = |value: Option<u64>| {
                        value.map_or_else(|| "unknown".into(), |v| v.to_string())
                    };
                    return Err(invalid(&format!(
                        "finishReason={code}; {hint}; tokens: prompt={}, candidates={}, thoughts={}",
                        count(self.meter.prompt),
                        count(self.meter.candidates),
                        self.meter.thoughts
                    )));
                }
                self.stopped = true;
            }
        }
        Ok(events)
    }
    pub fn finish(self) -> Result<ModelReply> {
        if !self.stopped {
            return Err(invalid("stream ended before STOP"));
        }
        let (content, tool_calls) = parse_parts(&self.parts)?;
        Ok(ModelReply {
            provider_state: Some(ProviderState::Gemini {
                version: 1,
                parts: self.parts,
            }),
            content,
            tool_calls,
            usage: self.meter.usage()?,
        })
    }
}
pub fn parse_reply(value: Value) -> Result<ModelReply> {
    let single_candidate = value["candidates"].as_array().is_some_and(|v| v.len() == 1);
    let mut accumulator = Accumulator::default();
    accumulator.push(value)?;
    if !single_candidate {
        return Err(invalid("JSON response requires exactly one candidate"));
    }
    accumulator.finish()
}
