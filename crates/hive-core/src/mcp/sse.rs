//! The classic MCP "HTTP+SSE" transport (revision 2024-11-05, still what n8n's MCP Server Trigger
//! and many older servers speak). The client opens a long-lived `GET` that returns a Server-Sent
//! Events stream; the server's first event, `endpoint`, says where to `POST` messages; every
//! response (and server notification) comes back as an `event: message` on that same stream.
//!
//! Nothing is replayed: a stream that ends or a rejected `POST` makes the exchange uncertain, like
//! every other transport. The endpoint the server names must be on the **same origin** as the
//! stream (otherwise a hostile server could point the client, and its headers, anywhere).
use super::error;
use crate::{Error, Result};
use reqwest::{
    Response,
    header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

fn version() -> String {
    "2025-03-26".into()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SseConfig {
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "version")]
    pub protocol_version: String,
    #[serde(default = "super::timeout")]
    pub timeout_ms: u64,
    #[serde(default = "super::bytes")]
    pub max_message_bytes: usize,
    #[serde(default = "super::tools")]
    pub max_tools: usize,
    #[serde(default = "super::pages")]
    pub max_pages: usize,
    #[serde(default = "super::messages")]
    pub max_messages: usize,
}

struct Event {
    name: String,
    data: String,
}

/// Splits the byte stream into events: `event:` / `data:` lines (any number of `data:`), a blank
/// line ends the event, `:` comments (keep-alives) and unknown fields are ignored, LF or CRLF.
#[derive(Default)]
struct Decoder {
    buffer: Vec<u8>,
    name: String,
    data: Vec<String>,
    size: usize,
}
impl Decoder {
    fn feed(&mut self, chunk: &[u8], max: usize) -> Result<Vec<Event>> {
        self.buffer.extend_from_slice(chunk);
        let mut events = vec![];
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8(line).map_err(|_| error("invalid MCP SSE text"))?;
            if line.is_empty() {
                if !self.data.is_empty() {
                    let name = std::mem::take(&mut self.name);
                    events.push(Event {
                        name: if name.is_empty() {
                            "message".into()
                        } else {
                            name
                        },
                        data: self.data.join("\n"),
                    });
                }
                self.name.clear();
                self.data.clear();
                self.size = 0;
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = line.split_once(':').map_or((line.as_str(), ""), |(f, v)| {
                (f, v.strip_prefix(' ').unwrap_or(v))
            });
            match field {
                "event" => self.name = value.chars().take(64).collect(),
                "data" => {
                    self.size = self.size.saturating_add(value.len());
                    if self.size > max {
                        return Err(error("MCP SSE event exceeds byte limit"));
                    }
                    self.data.push(value.to_owned());
                }
                _ => {}
            }
        }
        // A line that never ends must not grow without bound either.
        if self.buffer.len() > max {
            return Err(error("MCP SSE line exceeds byte limit"));
        }
        Ok(events)
    }
}

pub(super) struct SseSession {
    client: reqwest::Client,
    headers: HeaderMap,
    post_url: reqwest::Url,
    stream: Option<Response>,
    decoder: Decoder,
    queue: VecDeque<Event>,
    id: u64,
    max_bytes: usize,
    max_messages: usize,
    timeout: Duration,
    /// Set when the server said `notifications/tools/list_changed`.
    pub(super) changed: Arc<AtomicBool>,
}

impl SseSession {
    pub(super) async fn connect(config: SseConfig) -> Result<Self> {
        let url =
            reqwest::Url::parse(&config.url).map_err(|_| error("invalid MCP endpoint URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(error(
                "MCP URL must be HTTP(S) without credentials or fragment",
            ));
        }
        let mut headers = HeaderMap::new();
        let mut size = 0;
        for (name, value) in config.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| error("invalid MCP header name"))?;
            if matches!(
                name.as_str(),
                "accept" | "content-type" | "content-length" | "host"
            ) {
                return Err(error("MCP protocol headers cannot be overridden"));
            }
            size += name.as_str().len() + value.len();
            if size > 64 * 1024 {
                return Err(error("MCP headers exceed byte limit"));
            }
            headers.insert(
                name,
                HeaderValue::from_str(&value).map_err(|_| error("invalid MCP header value"))?,
            );
        }
        let timeout = Duration::from_millis(config.timeout_ms);
        // No total timeout on the client: the stream is meant to stay open.
        let client = reqwest::Client::builder()
            .connect_timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| error("cannot initialize MCP HTTP client"))?;
        let response = tokio::time::timeout(
            timeout,
            client
                .get(url.clone())
                .headers(headers.clone())
                .header(ACCEPT, "text/event-stream")
                .send(),
        )
        .await
        .map_err(|_| error("MCP SSE connection timed out"))?
        .map_err(|_| error("MCP SSE connection failed"))?;
        if !response.status().is_success() {
            return Err(error("MCP SSE endpoint refused the connection"));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if content_type != "text/event-stream" {
            return Err(error("MCP SSE endpoint did not return an event stream"));
        }
        let mut session = Self {
            client,
            headers,
            post_url: url.clone(),
            stream: Some(response),
            decoder: Decoder::default(),
            queue: VecDeque::new(),
            id: 0,
            max_bytes: config.max_message_bytes,
            max_messages: config.max_messages,
            timeout,
            changed: Arc::new(AtomicBool::new(false)),
        };
        // The first event says where messages go.
        let endpoint = tokio::time::timeout(timeout, async {
            loop {
                let event = session.next_event().await?;
                if event.name == "endpoint" {
                    return Ok::<String, Error>(event.data);
                }
            }
        })
        .await
        .map_err(|_| error("MCP SSE server never announced its message endpoint"))??;
        let post = url
            .join(endpoint.trim())
            .map_err(|_| error("invalid MCP SSE message endpoint"))?;
        let same_origin = post.scheme() == url.scheme()
            && post.host_str() == url.host_str()
            && post.port_or_known_default() == url.port_or_known_default();
        if !same_origin || !post.username().is_empty() || post.password().is_some() {
            return Err(error(
                "MCP SSE message endpoint must be on the same origin as the stream",
            ));
        }
        session.post_url = post;
        Ok(session)
    }

    async fn next_event(&mut self) -> Result<Event> {
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Ok(event);
            }
            let stream = self
                .stream
                .as_mut()
                .ok_or_else(|| error("MCP SSE stream is closed"))?;
            match stream.chunk().await {
                Ok(Some(chunk)) => {
                    for event in self.decoder.feed(&chunk, self.max_bytes)? {
                        self.queue.push_back(event);
                    }
                }
                Ok(None) => return Err(error("MCP SSE stream ended")),
                Err(_) => return Err(error("MCP SSE stream failed")),
            }
        }
    }

    pub(super) async fn write(&mut self, value: &Value) -> Result<()> {
        let body = serde_json::to_vec(value)?;
        if body.len() > self.max_bytes {
            return Err(error("outgoing MCP SSE message exceeds byte limit"));
        }
        let mut response = self
            .client
            .post(self.post_url.clone())
            .headers(self.headers.clone())
            .header(CONTENT_TYPE, "application/json")
            .timeout(self.timeout)
            .body(body)
            .send()
            .await
            .map_err(|_| error("MCP SSE message could not be sent"))?;
        if !response.status().is_success() {
            return Err(error("MCP SSE server rejected the message"));
        }
        // The answer comes on the stream; the POST body, if any, is only drained (bounded).
        let mut drained = 0usize;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| error("MCP SSE acknowledgement failed"))?
        {
            drained = drained.saturating_add(chunk.len());
            if drained > 64 * 1024 {
                return Err(error("MCP SSE acknowledgement is too large"));
            }
        }
        Ok(())
    }

    async fn accept(&mut self, value: Value, id: u64) -> Result<Option<Value>> {
        if value["jsonrpc"] != "2.0" || !value.is_object() {
            return Err(error("invalid SSE JSON-RPC envelope"));
        }
        if let Some(method) = value.get("method") {
            if !method.is_string() || value.get("result").is_some() || value.get("error").is_some()
            {
                return Err(error("invalid MCP server request"));
            }
            if method == "notifications/tools/list_changed" {
                self.changed.store(true, Ordering::SeqCst);
            }
            if let Some(id) = value.get("id") {
                if !id.is_string() && !id.is_number() {
                    return Err(error("invalid server request ID"));
                }
                let reply = if method == "ping" {
                    json!({"jsonrpc":"2.0","id":id,"result":{}})
                } else {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Client capability is not enabled"}})
                };
                self.write(&reply).await?;
            }
            return Ok(None);
        }
        if value["id"].as_u64() != Some(id) {
            return Err(error("unexpected MCP SSE response ID"));
        }
        match (value.get("result"), value.get("error")) {
            (Some(result), None) => Ok(Some(result.clone())),
            (None, Some(remote))
                if remote["code"].as_i64().is_some() && remote["message"].is_string() =>
            {
                Err(Error::McpRemote(
                    remote["message"]
                        .as_str()
                        .unwrap()
                        .chars()
                        .take(512)
                        .collect(),
                ))
            }
            _ => Err(error("invalid MCP SSE response envelope")),
        }
    }

    pub(super) async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        self.id = self
            .id
            .checked_add(1)
            .filter(|id| *id <= 9_007_199_254_740_991)
            .ok_or_else(|| error("request ID exhausted"))?;
        let id = self.id;
        self.write(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        let mut messages = 0usize;
        loop {
            let event = self.next_event().await?;
            // Keep-alive or vendor events are not part of the conversation.
            if event.name != "message" {
                continue;
            }
            messages += 1;
            if messages > self.max_messages {
                return Err(error("too many MCP SSE messages before the response"));
            }
            let value =
                serde_json::from_str(&event.data).map_err(|_| error("invalid MCP SSE JSON"))?;
            if let Some(result) = self.accept(value, id).await? {
                return Ok(result);
            }
        }
    }

    pub(super) async fn shutdown(&mut self) {
        // Dropping the response closes the stream, which ends the server's session.
        self.stream = None;
        self.queue.clear();
    }
}
