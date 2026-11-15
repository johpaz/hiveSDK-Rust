//! Optional native CDP transport. Attach to a page WebSocket, without Node/Bun.
//! Owns the connection, not the browser process or its profile.
use crate::{Error, Result, provider::CancellationToken};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpStream, sync::Mutex};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Message, protocol::WebSocketConfig},
};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Command {
    Navigate { url: String },
    Evaluate { expression: String },
    Click { selector: String },
    TypeText { selector: String, text: String },
    Extract { selector: String },
    Screenshot,
}

pub struct Browser {
    socket: Mutex<Option<Socket>>,
    stopped: CancellationToken,
    timeout: Duration,
}

fn error(message: &str) -> Error {
    Error::State(format!("BROWSER_ERROR: {message}"))
}

impl Browser {
    pub async fn dispatch(&self, command: Value) -> Result<Value> {
        match serde_json::from_value::<Command>(command)? {
            Command::Navigate { url } => self.navigate(&url).await,
            Command::Evaluate { expression } => self.evaluate(&expression).await,
            Command::Click { selector } => self.click(&selector).await,
            Command::TypeText { selector, text } => self.type_text(&selector, &text).await,
            Command::Extract { selector } => self.extract(&selector).await,
            Command::Screenshot => self.screenshot().await,
        }
    }
    pub async fn connect(page_websocket_url: &str, timeout_ms: u64) -> Result<Arc<Self>> {
        if !(1..=120_000).contains(&timeout_ms) {
            return Err(Error::Input("browser timeout must be 1..120000 ms".into()));
        }
        let url = reqwest::Url::parse(page_websocket_url)
            .map_err(|_| Error::Input("invalid page WebSocket URL".into()))?;
        if !matches!(url.scheme(), "ws" | "wss")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(Error::Input(
                "browser requires a ws/wss page URL without embedded credentials".into(),
            ));
        }
        let config = WebSocketConfig {
            max_message_size: Some(8 * 1024 * 1024),
            max_frame_size: Some(8 * 1024 * 1024),
            ..Default::default()
        };
        let timeout = Duration::from_millis(timeout_ms);
        let (socket, _) = tokio::time::timeout(
            timeout,
            tokio_tungstenite::connect_async_with_config(page_websocket_url, Some(config), false),
        )
        .await
        .map_err(|_| Error::Timeout)?
        .map_err(|_| error("cannot connect to page"))?;
        Ok(Arc::new(Self {
            socket: Mutex::new(Some(socket)),
            stopped: CancellationToken::new(),
            timeout,
        }))
    }

    /// Serializes CDP commands. A timeout, cancellation, or protocol error drops
    /// the connection: an uncertain browser action is never silently replayed.
    pub async fn command(&self, method: &str, params: Value) -> Result<Value> {
        if method.is_empty()
            || method.len() > 128
            || !params.is_object()
            || serde_json::to_vec(&params)?.len() > 1024 * 1024
        {
            return Err(Error::Input("invalid or oversized browser command".into()));
        }
        let operation = async {
            let mut slot = self.socket.lock().await;
            let mut socket = slot
                .take()
                .ok_or_else(|| error("connection is closed; reconnect explicitly"))?;
            socket
                .send(Message::Text(
                    json!({"id":1,"method":method,"params":params}).to_string(),
                ))
                .await
                .map_err(|_| error("command delivery is uncertain"))?;
            let result = loop {
                let frame = socket
                    .next()
                    .await
                    .ok_or_else(|| error("connection ended during command"))?
                    .map_err(|_| error("connection failed during command"))?;
                match frame {
                    Message::Text(text) => {
                        let value: Value =
                            serde_json::from_str(&text).map_err(|_| error("invalid CDP JSON"))?;
                        if value.get("id").is_none() {
                            continue;
                        }
                        if value["id"] != 1 {
                            return Err(error("unexpected CDP response id"));
                        }
                        if value.get("error").is_some() {
                            return Err(error("CDP rejected command"));
                        }
                        break value
                            .get("result")
                            .cloned()
                            .ok_or_else(|| error("missing CDP result"))?;
                    }
                    Message::Ping(value) => {
                        socket
                            .send(Message::Pong(value))
                            .await
                            .map_err(|_| error("ping failed"))?;
                    }
                    Message::Pong(_) => {}
                    _ => return Err(error("connection closed or invalid frame")),
                }
            };
            *slot = Some(socket);
            Ok(result)
        };
        tokio::select! {
            biased;
            _ = self.stopped.cancelled() => Err(Error::Cancelled),
            result = tokio::time::timeout(self.timeout, operation) => result.map_err(|_| Error::Timeout)?,
        }
    }

    pub async fn navigate(&self, url: &str) -> Result<Value> {
        let parsed =
            reqwest::Url::parse(url).map_err(|_| Error::Input("invalid navigation URL".into()))?;
        if !matches!(parsed.scheme(), "http" | "https" | "about") {
            return Err(Error::Input(
                "navigation requires http, https or about".into(),
            ));
        }
        let result = self.command("Page.navigate", json!({"url":url})).await?;
        if result.get("errorText").is_some() {
            return Err(error("navigation failed"));
        }
        Ok(result)
    }

    pub async fn evaluate(&self, expression: &str) -> Result<Value> {
        let result = self
            .command(
                "Runtime.evaluate",
                json!({"expression":expression,"awaitPromise":true,"returnByValue":true}),
            )
            .await?;
        if result.get("exceptionDetails").is_some() {
            return Err(error("page script threw an exception"));
        }
        Ok(result["result"]["value"].clone())
    }

    pub async fn click(&self, selector: &str) -> Result<Value> {
        let selector = serde_json::to_string(selector)?;
        self.evaluate(&format!("(() => {{const e=document.querySelector({selector});if(!e)throw new Error('element not found');e.click();return true;}})()" )).await
    }

    pub async fn type_text(&self, selector: &str, text: &str) -> Result<Value> {
        let selector = serde_json::to_string(selector)?;
        let text = serde_json::to_string(text)?;
        self.evaluate(&format!("(() => {{const e=document.querySelector({selector});if(!e || !('value' in e))throw new Error('input not found');e.focus();e.value={text};e.dispatchEvent(new Event('input',{{bubbles:true}}));e.dispatchEvent(new Event('change',{{bubbles:true}}));return true;}})()" )).await
    }

    pub async fn extract(&self, selector: &str) -> Result<Value> {
        let selector = serde_json::to_string(selector)?;
        self.evaluate(&format!("(() => {{const e=document.querySelector({selector});if(!e)throw new Error('element not found');return e.innerText ?? e.textContent;}})()" )).await
    }

    pub async fn screenshot(&self) -> Result<Value> {
        self.command("Page.captureScreenshot", json!({"format":"png"}))
            .await
    }

    pub fn cancel(&self) {
        self.stopped.cancel();
    }

    pub async fn close(&self) {
        self.cancel();
        if let Some(mut socket) = self.socket.lock().await.take() {
            let _ = tokio::time::timeout(Duration::from_secs(1), socket.close(None)).await;
        }
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.stopped.cancel();
    }
}
