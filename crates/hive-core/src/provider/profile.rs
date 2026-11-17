//! Provider presets and OpenAI-compatible wire profiles.
//!
//! A preset names a hosted provider and fills in its protocol, base URL and
//! profile; a profile lists the small body adjustments that provider needs.
//! Every adjustment is a deterministic rewrite of one request that can be undone
//! on the reply (tool names) or only narrows a schema. Nothing here retries with
//! a different request or drops tools silently: a model that cannot call tools is
//! rejected up front instead.
use crate::{Error, ModelReply, ModelRequest, Result};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// Wire name to original name, only for names the profile changed.
pub(crate) type NameMap = BTreeMap<String, String>;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OpenAiProfile {
    /// Rewrite tool names to `[a-zA-Z0-9_-]{1,64}`; replies are mapped back.
    pub normalize_tool_names: bool,
    /// Give every `{"type":"array"}` schema an `items` schema.
    pub ensure_array_items: bool,
    /// Drop `additionalProperties: false` from tool schemas.
    pub strip_additional_properties: bool,
    /// Sent as `parallel_tool_calls` when tools are present.
    pub parallel_tool_calls: Option<bool>,
    /// Sent as `tool_choice` when tools are present.
    pub tool_choice: Option<String>,
    /// Models (case-insensitive, exact) that cannot call tools.
    pub no_tool_models: Vec<String>,
    /// Also treat names containing `-r1` or `/r1` as models without tools.
    pub no_tool_r1_models: bool,
    /// Read tool calls a model wrote as text (`<tool_call>{"name":…,"arguments":…}</tool_call>`, or the
    /// whole reply being such a JSON object, optionally in a code fence) when the reply has no real
    /// ones. Only a call to a tool of this request counts; anything else stays text. Off by default:
    /// a model that merely quotes a call must not run it.
    pub text_tool_calls: bool,
    /// Send the `reasoning_content` a reply carried back with its assistant message (DeepSeek, Kimi
    /// K2 thinking need it with their tool calls). Everyone else never sees it.
    pub round_trip_reasoning: bool,
    /// Turn thinking on through `chat_template_kwargs`, chosen by model (NVIDIA): the first rule
    /// whose `models` has a name contained in the model id applies. Off unless `thinking` is true.
    pub thinking: bool,
    pub thinking_kwargs: Vec<ThinkingKwargs>,
    /// Header that carries a stable per-provider session identifier.
    pub session_header: Option<String>,
    pub user_agent: Option<String>,
}

/// One family of models and the template switch that turns its thinking on.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThinkingKwargs {
    /// Case-insensitive fragments of the model id.
    pub models: Vec<String>,
    pub kwargs: Map<String, Value>,
}

impl OpenAiProfile {
    pub(crate) fn validate(&self) -> Result<()> {
        if let Some(name) = &self.session_header {
            reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| Error::Input("invalid profile session_header".into()))?;
        }
        if let Some(agent) = &self.user_agent {
            reqwest::header::HeaderValue::from_str(agent)
                .map_err(|_| Error::Input("invalid profile user_agent".into()))?;
        }
        if self.tool_choice.as_deref().is_some_and(str::is_empty) {
            return Err(Error::Input("profile tool_choice must not be empty".into()));
        }
        Ok(())
    }
}

fn valid_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// `[a-zA-Z0-9_-]{1,64}` names pass through; anything else has each invalid
/// character replaced by `__`, runs of `__` collapsed, a leading `_` added when
/// the first character is not a letter or underscore, and is cut to 64.
pub(crate) fn normalize_tool_name(name: &str) -> String {
    if (1..=64).contains(&name.len()) && name.chars().all(valid_char) {
        return name.to_owned();
    }
    let mut out: String = name
        .chars()
        .map(|c| {
            if valid_char(c) {
                c.to_string()
            } else {
                "__".into()
            }
        })
        .collect();
    while out.contains("____") {
        out = out.replace("____", "__");
    }
    if !out.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        out.insert(0, '_');
    }
    out.chars().take(64).collect()
}

fn ensure_array_items(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(ensure_array_items),
        Value::Object(map) => {
            map.values_mut().for_each(ensure_array_items);
            if map.get("type") == Some(&json!("array")) && !map.contains_key("items") {
                map.insert("items".into(), json!({}));
            }
        }
        _ => {}
    }
}

fn strip_additional_properties(value: &mut Value) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(strip_additional_properties),
        Value::Object(map) => {
            if map.get("additionalProperties") == Some(&json!(false)) {
                map.remove("additionalProperties");
            }
            map.values_mut().for_each(strip_additional_properties);
        }
        _ => {}
    }
}

/// `\b` after `r1`, case-insensitive: `deepseek-r1:free` matches, `model-r10` does not.
fn names_an_r1_model(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    ["-r1", "/r1"].iter().any(|needle| {
        lower.match_indices(needle).any(|(at, _)| {
            lower[at + needle.len()..]
                .chars()
                .next()
                .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_'))
        })
    })
}

pub(crate) fn supports_tools(profile: &OpenAiProfile, model: &str) -> bool {
    !(profile
        .no_tool_models
        .iter()
        .any(|name| name.eq_ignore_ascii_case(model))
        || (profile.no_tool_r1_models && names_an_r1_model(model)))
}

fn rename(value: &mut Value, names: &BTreeMap<String, String>) {
    if let Some(name) = value.as_str().and_then(|name| names.get(name)) {
        *value = json!(name);
    }
}

/// Apply the profile to an OpenAI chat body built from `request`.
pub(crate) fn apply(
    profile: &OpenAiProfile,
    request: &ModelRequest,
    body: &mut Value,
) -> Result<NameMap> {
    let mut restore = NameMap::new();
    if !profile.round_trip_reasoning
        && let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut)
    {
        for message in messages {
            if let Some(object) = message.as_object_mut() {
                object.remove("reasoning_content");
            }
        }
    }
    if profile.thinking {
        let model = request.model.to_lowercase();
        if let Some(rule) = profile.thinking_kwargs.iter().find(|rule| {
            rule.models
                .iter()
                .any(|name| !name.is_empty() && model.contains(&name.to_lowercase()))
        }) {
            let mut kwargs = body
                .get("chat_template_kwargs")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            kwargs.extend(rule.kwargs.clone());
            body["chat_template_kwargs"] = Value::Object(kwargs);
        }
    }
    if profile.normalize_tool_names {
        let history = request
            .messages
            .iter()
            .flat_map(|message| message.tool_calls.iter().map(|call| call.name.as_str()));
        let originals = request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .chain(history);
        let mut to_wire = BTreeMap::new();
        let mut by_wire: BTreeMap<String, &str> = BTreeMap::new();
        for original in originals {
            if to_wire.contains_key(original) {
                continue;
            }
            let wire = normalize_tool_name(original);
            // Two different names must never share a wire name, or a reply
            // could not be mapped back to the tool the model meant.
            if let Some(other) = by_wire.insert(wire.clone(), original)
                && other != original
            {
                return Err(Error::Input(format!(
                    "tool names `{other}` and `{original}` collide after normalization"
                )));
            }
            if wire != original {
                restore.insert(wire.clone(), original.to_owned());
            }
            to_wire.insert(original.to_owned(), wire);
        }
        if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
            for tool in tools {
                if let Some(name) = tool.pointer_mut("/function/name") {
                    rename(name, &to_wire);
                }
            }
        }
        if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
            for call in messages
                .iter_mut()
                .filter_map(|message| message.get_mut("tool_calls")?.as_array_mut())
                .flatten()
            {
                if let Some(name) = call.pointer_mut("/function/name") {
                    rename(name, &to_wire);
                }
            }
        }
    }
    if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
        for tool in tools.iter_mut() {
            if let Some(schema) = tool.pointer_mut("/function/parameters") {
                if profile.ensure_array_items {
                    ensure_array_items(schema);
                }
                if profile.strip_additional_properties {
                    strip_additional_properties(schema);
                }
            }
        }
        if !tools.is_empty() {
            if let Some(parallel) = profile.parallel_tool_calls {
                body["parallel_tool_calls"] = json!(parallel);
            }
            if let Some(choice) = &profile.tool_choice {
                body["tool_choice"] = json!(choice);
            }
        }
    }
    Ok(restore)
}

/// What happens to every reply: tool names back to the original, then (if the profile asks) tool
/// calls written as text become real ones.
pub(crate) fn finish_reply(
    profile: &OpenAiProfile,
    names: &NameMap,
    tools: &[String],
    reply: &mut ModelReply,
) {
    restore_names(reply, names);
    if profile.text_tool_calls && reply.tool_calls.is_empty() {
        extract_text_tool_calls(reply, tools);
    }
}

fn call_from_json(value: &Value, tools: &[String], at: usize) -> Option<crate::ToolCall> {
    let object = value.as_object()?;
    let name = object
        .get("name")
        .or_else(|| object.get("tool"))
        .or_else(|| value.pointer("/function/name"))?
        .as_str()?;
    if !tools.iter().any(|tool| tool == name) {
        return None;
    }
    let arguments = ["arguments", "parameters", "args", "input"]
        .iter()
        .find_map(|key| object.get(*key))
        .cloned()
        .unwrap_or_else(|| json!({}));
    // Some models send the arguments as a JSON string.
    let arguments = match arguments {
        Value::String(text) => serde_json::from_str(&text).ok()?,
        other => other,
    };
    arguments.is_object().then(|| crate::ToolCall {
        id: format!("text-call-{at}"),
        name: name.to_owned(),
        arguments,
    })
}

pub(crate) fn extract_text_tool_calls(reply: &mut ModelReply, tools: &[String]) {
    let Some(content) = reply.content.clone() else {
        return;
    };
    let mut calls = vec![];
    let mut rest = String::new();
    let mut from = 0;
    // `<tool_call>…</tool_call>` blocks anywhere in the text.
    while let Some(open) = content[from..].find("<tool_call>").map(|at| at + from) {
        let body_start = open + "<tool_call>".len();
        let Some(close) = content[body_start..]
            .find("</tool_call>")
            .map(|at| at + body_start)
        else {
            break;
        };
        match serde_json::from_str::<Value>(content[body_start..close].trim())
            .ok()
            .and_then(|value| call_from_json(&value, tools, calls.len()))
        {
            Some(call) => {
                rest.push_str(&content[from..open]);
                calls.push(call);
            }
            // Not a call to one of our tools: it stays in the text as it was.
            None => rest.push_str(&content[from..close + "</tool_call>".len()]),
        }
        from = close + "</tool_call>".len();
    }
    rest.push_str(&content[from..]);
    if calls.is_empty() {
        // Or the whole reply is the call (or a list of them), maybe fenced.
        let body = content.trim();
        let body = body
            .strip_prefix("```json")
            .or_else(|| body.strip_prefix("```"))
            .map(|inner| inner.strip_suffix("```").unwrap_or(inner).trim())
            .unwrap_or(body);
        let parsed: Option<Vec<Value>> = match serde_json::from_str::<Value>(body).ok() {
            Some(Value::Array(items)) => Some(items),
            Some(object @ Value::Object(_)) => Some(vec![object]),
            _ => None,
        };
        let Some(items) = parsed else { return };
        let found: Vec<_> = items
            .iter()
            .enumerate()
            .map(|(at, item)| call_from_json(item, tools, at))
            .collect();
        // Every item has to be a call, or this is just JSON the model was asked to write.
        if found.is_empty() || found.iter().any(Option::is_none) {
            return;
        }
        reply.tool_calls = found.into_iter().flatten().collect();
        reply.content = None;
        return;
    }
    reply.tool_calls = calls;
    let rest = rest.trim();
    reply.content = (!rest.is_empty()).then(|| rest.to_owned());
}

pub(crate) fn restore_names(reply: &mut ModelReply, names: &NameMap) {
    if names.is_empty() {
        return;
    }
    for call in &mut reply.tool_calls {
        if let Some(original) = names.get(&call.name) {
            call.name = original.clone();
        }
    }
}

/// One entry of the provider catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preset {
    pub id: &'static str,
    /// `openai_compatible`, `anthropic` or `gemini`.
    pub kind: &'static str,
    pub base_url: &'static str,
}

struct Entry {
    preset: Preset,
    profile: fn() -> Value,
}

fn common() -> Value {
    json!({"normalize_tool_names":true,"ensure_array_items":true,"tool_choice":"auto"})
}
fn with(extra: Value) -> Value {
    let mut base = common();
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().cloned().unwrap_or_default());
    base
}
fn none() -> Value {
    Value::Null
}

fn table() -> Vec<Entry> {
    let compat = |id, base_url, profile: fn() -> Value| Entry {
        preset: Preset {
            id,
            kind: "openai_compatible",
            base_url,
        },
        profile,
    };
    vec![
        compat("openai", "https://api.openai.com/v1", common),
        Entry {
            preset: Preset {
                id: "anthropic",
                kind: "anthropic",
                base_url: "https://api.anthropic.com/v1",
            },
            profile: none,
        },
        Entry {
            preset: Preset {
                id: "gemini",
                kind: "gemini",
                base_url: "https://generativelanguage.googleapis.com/v1beta",
            },
            profile: none,
        },
        compat("groq", "https://api.groq.com/openai/v1", common),
        compat("mistral", "https://api.mistral.ai/v1", || {
            with(json!({"strip_additional_properties":true}))
        }),
        compat("openrouter", "https://openrouter.ai/api/v1", || {
            with(json!({"no_tool_models":["deepseek/deepseek-r1:free"],"no_tool_r1_models":true}))
        }),
        compat("deepseek", "https://api.deepseek.com/v1", || {
            with(
                json!({"no_tool_models":["deepseek-reasoner","deepseek/deepseek-r1:free"],"no_tool_r1_models":true,"round_trip_reasoning":true}),
            )
        }),
        compat("kimi", "https://api.moonshot.ai/v1", || {
            with(json!({"parallel_tool_calls":false,"round_trip_reasoning":true}))
        }),
        compat("nvidia", "https://integrate.api.nvidia.com/v1", || {
            // NIM keeps reasoning off by default; the switch is `chat_template_kwargs`, with a different
            // key per family. Families not listed are left alone on purpose: sending a key their
            // template does not accept is what breaks the call.
            with(json!({"thinking_kwargs":[
                {"models":["glm"],"kwargs":{"enable_thinking":true,"clear_thinking":false}},
                {"models":["minimax"],"kwargs":{"thinking_mode":"enabled"}},
                {"models":["kimi","deepseek"],"kwargs":{"thinking":true}},
                {"models":["qwen","qwq"],"kwargs":{"enable_thinking":true}}
            ]}))
        }),
        compat(
            "qwen",
            "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
            common,
        ),
        compat("minimax", "https://api.minimaxi.com/v1", common),
        compat("opencode-go", "https://opencode.ai/zen/go/v1", || {
            with(json!({"session_header":"x-opencode-session",
                        "user_agent":concat!("hive-sdk-rust/", env!("CARGO_PKG_VERSION"))}))
        }),
        compat(
            "hiveagents",
            "https://llm.hiveagents.io/v1",
            || json!({"ensure_array_items":true,"tool_choice":"auto"}),
        ),
        compat("z-ai", "https://api.z.ai/api/paas/v4", common),
        // `.ai`, not `.cn`: separate platforms with separate accounts and tokens.
        compat(
            "modelscope",
            "https://api-inference.modelscope.ai/v1",
            common,
        ),
        // Ollama's OpenAI-compatible endpoint, not its native /api/chat protocol.
        compat("ollama", "http://localhost:11434/v1", || {
            // Local models often write the call as text instead of using the tool-call field.
            with(json!({"text_tool_calls":true}))
        }),
    ]
}

/// Every known preset, in a stable order.
pub fn presets() -> Vec<Preset> {
    table().into_iter().map(|entry| entry.preset).collect()
}

/// Fill `kind`, `base_url` and `profile` from the named preset. Keys the caller
/// already set win, so any preset can point at a proxy or tune its profile.
pub(crate) fn expand(config: &mut Map<String, Value>, id: &str) -> Result<()> {
    let entry = table()
        .into_iter()
        .find(|entry| entry.preset.id == id)
        .ok_or_else(|| Error::Input(format!("unknown provider preset `{id}`")))?;
    match config.get("kind").and_then(Value::as_str) {
        None => {
            config.insert("kind".into(), json!(entry.preset.kind));
        }
        Some(kind) if kind != entry.preset.kind => {
            return Err(Error::Input(format!(
                "preset `{id}` is a `{}` provider, not `{kind}`",
                entry.preset.kind
            )));
        }
        _ => {}
    }
    config
        .entry("base_url")
        .or_insert_with(|| json!(entry.preset.base_url));
    let profile = (entry.profile)();
    if !profile.is_null() {
        match (config.get_mut("profile"), &profile) {
            (None, _) => {
                config.insert("profile".into(), profile);
            }
            // The caller's keys win, key by key: `{"profile":{"thinking":true}}` turns one switch
            // on without throwing the preset's other adjustments away.
            (Some(Value::Object(own)), Value::Object(base)) => {
                for (key, value) in base {
                    own.entry(key.clone()).or_insert_with(|| value.clone());
                }
            }
            _ => {}
        }
    }
    Ok(())
}
