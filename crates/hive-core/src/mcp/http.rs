//! Streamable HTTP for explicitly selected MCP 2025 lifecycle revisions. No replay.
use super::error;
use crate::{Error, Result};
use reqwest::{
    Response,
    header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    pub url: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default = "super::version")]
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
pub(super) struct HttpSession {
    client: reqwest::Client,
    url: reqwest::Url,
    headers: HeaderMap,
    version: String,
    session_id: Option<HeaderValue>,
    initialized: bool,
    id: u64,
    max_bytes: usize,
    max_messages: usize,
}
impl HttpSession {
    pub(super) fn new(config: HttpConfig) -> Result<Self> {
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
                "accept"
                    | "content-type"
                    | "content-length"
                    | "host"
                    | "mcp-session-id"
                    | "mcp-protocol-version"
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
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(config.timeout_ms))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| error("cannot initialize MCP HTTP client"))?;
        Ok(Self {
            client,
            url,
            headers,
            version: config.protocol_version,
            session_id: None,
            initialized: false,
            id: 0,
            max_bytes: config.max_message_bytes,
            max_messages: config.max_messages,
        })
    }
    fn headers(&self) -> HeaderMap {
        let mut headers = self.headers.clone();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        if self.initialized {
            headers.insert(
                "mcp-protocol-version",
                HeaderValue::from_str(&self.version).expect("validated protocol version"),
            );
        }
        if let Some(session) = &self.session_id {
            headers.insert("mcp-session-id", session.clone());
        }
        headers
    }
    async fn send(&self, value: &Value) -> Result<Response> {
        if serde_json::to_vec(value)?.len() > self.max_bytes {
            return Err(error("outgoing MCP HTTP body exceeds byte limit"));
        }
        self.client
            .post(self.url.clone())
            .headers(self.headers())
            .json(value)
            .send()
            .await
            .map_err(|_| error("MCP HTTP request failed"))
    }
    pub(super) async fn write(&mut self, value: &Value) -> Result<()> {
        let mut response = self.send(value).await?;
        if response.status() != reqwest::StatusCode::ACCEPTED {
            return Err(error("MCP HTTP notification/response requires 202"));
        }
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| error("MCP HTTP acknowledgement failed"))?
        {
            if !chunk.is_empty() {
                return Err(error("MCP HTTP acknowledgement must have no body"));
            }
        }
        Ok(())
    }
    async fn read_json(&self, mut response: Response) -> Result<Value> {
        let mut bytes = vec![];
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| error("MCP HTTP body failed"))?
        {
            if bytes.len().saturating_add(chunk.len()) > self.max_bytes {
                return Err(error("MCP HTTP response exceeds byte limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| error("invalid MCP HTTP JSON"))
    }
    async fn accept(&mut self, value: Value, id: u64) -> Result<Option<Value>> {
        if value["jsonrpc"] != "2.0" || !value.is_object() {
            return Err(error("invalid HTTP JSON-RPC envelope"));
        }
        if let Some(method) = value.get("method") {
            if !method.is_string() || value.get("result").is_some() || value.get("error").is_some()
            {
                return Err(error("invalid MCP server request"));
            }
            if let Some(id) = value.get("id") {
                if !id.is_string() && !id.is_number() {
                    return Err(error("invalid server request ID"));
                }
                let response = if method == "ping" {
                    json!({"jsonrpc":"2.0","id":id,"result":{}})
                } else {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Client capability is not enabled"}})
                };
                self.write(&response).await?;
            }
            return Ok(None);
        }
        if value["id"].as_u64() != Some(id) {
            return Err(error("unexpected MCP HTTP response ID"));
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
            _ => Err(error("invalid MCP HTTP response envelope")),
        }
    }
    pub(super) async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        self.id = self
            .id
            .checked_add(1)
            .filter(|id| *id <= 9_007_199_254_740_991)
            .ok_or_else(|| error("request ID exhausted"))?;
        let id = self.id;
        let mut response = self
            .send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        if !response.status().is_success() || response.status() == reqwest::StatusCode::ACCEPTED {
            return Err(error("MCP HTTP request returned no confirmed result"));
        }
        if method == "initialize" {
            self.session_id = response.headers().get("mcp-session-id").cloned();
        } else if response
            .headers()
            .get("mcp-session-id")
            .is_some_and(|id| Some(id) != self.session_id.as_ref())
        {
            return Err(error("MCP HTTP session ID changed"));
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap()
            .trim()
            .to_ascii_lowercase();
        let result = if content_type == "application/json" {
            let value = self.read_json(response).await?;
            self.accept(value, id)
                .await?
                .ok_or_else(|| error("MCP HTTP JSON response contains no result"))?
        } else if content_type == "text/event-stream" {
            let mut decoder = crate::provider::streaming::SseDecoder::default();
            let mut size = 0usize;
            let mut messages = 0usize;
            let mut result = None;
            'chunks: while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| error("MCP HTTP stream failed"))?
            {
                size = size.saturating_add(chunk.len());
                if size > self.max_bytes {
                    return Err(error("MCP HTTP stream exceeds byte limit"));
                }
                for frame in decoder.feed(&chunk)? {
                    messages += 1;
                    if messages > self.max_messages {
                        return Err(error("too many MCP HTTP stream messages"));
                    }
                    let value = serde_json::from_str(&frame)
                        .map_err(|_| error("invalid MCP HTTP SSE JSON"))?;
                    if let Some(value) = self.accept(value, id).await? {
                        result = Some(value);
                        break 'chunks;
                    }
                }
            }
            result.ok_or_else(|| error("MCP HTTP stream ended before response"))?
        } else {
            return Err(error("unsupported MCP HTTP content type"));
        };
        if method == "initialize" {
            self.initialized = true;
        }
        Ok(result)
    }
    pub(super) async fn shutdown(&mut self) {
        if self.session_id.is_some() {
            let request = self.client.delete(self.url.clone()).headers(self.headers());
            self.session_id = None;
            let _ = tokio::time::timeout(Duration::from_secs(1), request.send()).await;
        }
    }
}
