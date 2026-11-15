//! Explicit, sequential MCP stdio client. No automatic replay, sampling or reconnect.
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, oneshot},
};
use tokio_util::sync::CancellationToken;
pub mod http;

fn version() -> String {
    "2025-11-25".into()
}
fn timeout() -> u64 {
    30_000
}
fn bytes() -> usize {
    8 * 1024 * 1024
}
fn tools() -> usize {
    256
}
fn pages() -> usize {
    16
}
fn messages() -> usize {
    128
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StdioConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub cwd: Option<String>,
    #[serde(default = "version")]
    pub protocol_version: String,
    #[serde(default = "timeout")]
    pub timeout_ms: u64,
    #[serde(default = "bytes")]
    pub max_message_bytes: usize,
    #[serde(default = "tools")]
    pub max_tools: usize,
    #[serde(default = "pages")]
    pub max_pages: usize,
    #[serde(default = "messages")]
    pub max_messages: usize,
}
#[derive(Clone, Serialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
    #[serde(rename = "outputSchema", skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Value>,
    #[serde(skip)]
    input: Arc<jsonschema::Validator>,
    #[serde(skip)]
    output: Option<Arc<jsonschema::Validator>>,
}
struct StdioSession {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    id: u64,
    max_bytes: usize,
    max_messages: usize,
}
fn error(message: &str) -> Error {
    Error::Mcp(message.into())
}
impl StdioSession {
    async fn write(&mut self, value: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(value)?;
        if bytes.len() + 1 > self.max_bytes {
            return Err(error("outgoing message exceeds byte limit"));
        }
        bytes.push(b'\n');
        self.input
            .write_all(&bytes)
            .await
            .map_err(|_| error("cannot write to MCP process"))?;
        self.input
            .flush()
            .await
            .map_err(|_| error("cannot flush MCP message"))
    }
    async fn read(&mut self) -> Result<Value> {
        let mut line = vec![];
        loop {
            let buf = self
                .output
                .fill_buf()
                .await
                .map_err(|_| error("cannot read MCP stdout"))?;
            if buf.is_empty() {
                return Err(error("MCP stdout ended before response"));
            }
            let length = buf
                .iter()
                .position(|b| *b == b'\n')
                .map_or(buf.len(), |i| i + 1);
            if line.len().saturating_add(length) > self.max_bytes {
                return Err(error("incoming message exceeds byte limit"));
            }
            line.extend_from_slice(&buf[..length]);
            self.output.consume(length);
            if line.last() == Some(&b'\n') {
                break;
            }
        }
        let value: Value =
            serde_json::from_slice(&line).map_err(|_| error("invalid MCP JSON line"))?;
        if value["jsonrpc"] != "2.0" || !value.is_object() {
            return Err(error("invalid JSON-RPC envelope"));
        }
        Ok(value)
    }
    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        self.id = self
            .id
            .checked_add(1)
            .filter(|id| *id <= 9_007_199_254_740_991)
            .ok_or_else(|| error("request ID exhausted"))?;
        let id = self.id;
        self.write(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        for _ in 0..self.max_messages {
            let value = self.read().await?;
            if let Some(method) = value.get("method") {
                if !method.is_string()
                    || value.get("result").is_some()
                    || value.get("error").is_some()
                {
                    return Err(error("invalid server request"));
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
                continue;
            }
            if value["id"].as_u64() != Some(id) {
                return Err(error("unexpected response ID"));
            }
            match (value.get("result"), value.get("error")) {
                (Some(result), None) => return Ok(result.clone()),
                (None, Some(remote))
                    if remote["code"].as_i64().is_some() && remote["message"].is_string() =>
                {
                    return Err(Error::McpRemote(
                        remote["message"]
                            .as_str()
                            .unwrap()
                            .chars()
                            .take(512)
                            .collect(),
                    ));
                }
                _ => return Err(error("invalid response envelope")),
            }
        }
        Err(error("too many messages before response"))
    }
    async fn shutdown(&mut self) {
        let _ = self.input.shutdown().await;
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(1), self.child.wait()).await;
    }
}
enum Session {
    Stdio(StdioSession),
    Http(http::HttpSession),
}
impl Session {
    async fn write(&mut self, value: &Value) -> Result<()> {
        match self {
            Self::Stdio(session) => session.write(value).await,
            Self::Http(session) => session.write(value).await,
        }
    }
    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        match self {
            Self::Stdio(session) => session.rpc(method, params).await,
            Self::Http(session) => session.rpc(method, params).await,
        }
    }
    async fn shutdown(&mut self) {
        match self {
            Self::Stdio(session) => session.shutdown().await,
            Self::Http(session) => session.shutdown().await,
        }
    }
}
pub struct McpClient {
    session: Mutex<Session>,
    catalog: Vec<McpTool>,
    info: Value,
    timeout_ms: u64,
    max_bytes: usize,
    busy: AtomicBool,
    closed: AtomicBool,
    cancellation: CancellationToken,
}
pub struct McpRequest {
    receiver: Mutex<Option<oneshot::Receiver<Result<Value>>>>,
    cancel: CancellationToken,
}
impl McpRequest {
    pub async fn result(&self) -> Result<Value> {
        let receiver = self
            .receiver
            .try_lock()
            .map_err(|_| error("only one result reader is allowed"))?
            .take()
            .ok_or_else(|| error("result was already consumed"))?;
        receiver.await.unwrap_or(Err(Error::McpOperationUncertain))
    }
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}
impl Drop for McpRequest {
    fn drop(&mut self) {
        self.cancel();
    }
}
impl McpClient {
    pub async fn connect_json(config: &str) -> Result<Arc<Self>> {
        let mut config: Value = serde_json::from_str(config)?;
        let transport = config
            .as_object_mut()
            .ok_or_else(|| error("MCP config must be an object"))?
            .remove("transport");
        match transport.as_ref().and_then(Value::as_str) {
            None if transport.is_none() => Self::connect(serde_json::from_value(config)?).await,
            Some("stdio") => Self::connect(serde_json::from_value(config)?).await,
            Some("http") => Self::connect_http(serde_json::from_value(config)?).await,
            _ => Err(error("unsupported MCP transport")),
        }
    }
    pub async fn connect(config: StdioConfig) -> Result<Arc<Self>> {
        Self::validate_config(&config)?;
        Self::connect_stdio(config).await
    }
    fn validate_config(config: &StdioConfig) -> Result<()> {
        if config.command.trim().is_empty()
            || config.timeout_ms == 0
            || !(1024..=64 * 1024 * 1024).contains(&config.max_message_bytes)
            || !(1..=4096).contains(&config.max_tools)
            || !(1..=128).contains(&config.max_pages)
            || !(1..=4096).contains(&config.max_messages)
            || !matches!(
                config.protocol_version.as_str(),
                "2025-06-18" | "2025-11-25"
            )
        {
            return Err(error(
                "invalid stdio configuration or unsupported protocol version",
            ));
        }
        Ok(())
    }
    async fn connect_stdio(config: StdioConfig) -> Result<Arc<Self>> {
        let mut command = Command::new(&config.command);
        command.args(&config.args).env_clear();
        for key in [
            "PATH",
            "HOME",
            "USER",
            "TMPDIR",
            "TMP",
            "TEMP",
            "SystemRoot",
            "WINDIR",
            "COMSPEC",
            "PATHEXT",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command.envs(&config.env);
        if let Some(cwd) = &config.cwd {
            command.current_dir(cwd);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| error("cannot spawn MCP process"))?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| error("missing child stdin"))?;
        let output = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| error("missing child stdout"))?,
        );
        let session = Session::Stdio(StdioSession {
            child,
            input,
            output,
            id: 0,
            max_bytes: config.max_message_bytes,
            max_messages: config.max_messages,
        });
        Self::setup(session, config).await
    }
    pub async fn connect_http(config: http::HttpConfig) -> Result<Arc<Self>> {
        let common = StdioConfig {
            command: "http".into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
            protocol_version: config.protocol_version.clone(),
            timeout_ms: config.timeout_ms,
            max_message_bytes: config.max_message_bytes,
            max_tools: config.max_tools,
            max_pages: config.max_pages,
            max_messages: config.max_messages,
        };
        Self::validate_config(&common)?;
        let session = Session::Http(http::HttpSession::new(config)?);
        Self::setup(session, common).await
    }
    async fn setup(mut session: Session, config: StdioConfig) -> Result<Arc<Self>> {
        let setup = async {
            let info=session.rpc("initialize",json!({"protocolVersion":config.protocol_version,"capabilities":{},"clientInfo":{"name":"hive-sdk-rust","version":env!("CARGO_PKG_VERSION")}})).await?;
            if info["protocolVersion"].as_str() != Some(&config.protocol_version)
                || !["tools", "resources", "prompts"]
                    .iter()
                    .any(|key| info["capabilities"][*key].is_object())
                || !info["serverInfo"]["name"].is_string()
                || !info["serverInfo"]["version"].is_string()
            {
                return Err(error(
                    "server negotiation is incompatible or supported capabilities are missing",
                ));
            }
            session
                .write(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
                .await?;
            if !info["capabilities"]["tools"].is_object() {
                return Ok((vec![], info));
            }
            let mut catalog = vec![];
            let mut names = HashSet::new();
            let mut cursors = HashSet::new();
            let mut cursor = None::<String>;
            let mut size = 0usize;
            for _ in 0..config.max_pages {
                let params = cursor
                    .as_ref()
                    .map_or_else(|| json!({}), |cursor| json!({"cursor":cursor}));
                let result = session.rpc("tools/list", params).await?;
                for raw in result["tools"]
                    .as_array()
                    .ok_or_else(|| error("tools/list has no tool array"))?
                {
                    size = size.saturating_add(serde_json::to_vec(raw)?.len());
                    if size > config.max_message_bytes || catalog.len() >= config.max_tools {
                        return Err(error("tool catalog exceeds configured limits"));
                    }
                    let name = raw["name"]
                        .as_str()
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| error("invalid tool name"))?
                        .to_owned();
                    if !names.insert(name.clone()) || raw["inputSchema"]["type"] != "object" {
                        return Err(error("duplicate tool or non-object inputSchema"));
                    }
                    let description = match raw.get("description") {
                        None => String::new(),
                        Some(value) => value
                            .as_str()
                            .ok_or_else(|| error("invalid tool description"))?
                            .to_owned(),
                    };
                    let input_schema = raw["inputSchema"].clone();
                    let input = crate::validation::compile(&input_schema)?;
                    let output_schema = raw.get("outputSchema").cloned();
                    if output_schema
                        .as_ref()
                        .is_some_and(|schema| schema["type"] != "object")
                    {
                        return Err(error("outputSchema must describe an object"));
                    }
                    let output = output_schema
                        .as_ref()
                        .map(crate::validation::compile)
                        .transpose()?;
                    catalog.push(McpTool {
                        name,
                        description,
                        input_schema,
                        output_schema,
                        annotations: raw.get("annotations").cloned(),
                        input,
                        output,
                    });
                }
                cursor = match result.get("nextCursor").filter(|v| !v.is_null()) {
                    None => return Ok((catalog, info)),
                    Some(value) => Some(
                        value
                            .as_str()
                            .filter(|v| !v.is_empty())
                            .ok_or_else(|| error("invalid pagination cursor"))?
                            .to_owned(),
                    ),
                };
                if !cursors.insert(cursor.clone().unwrap()) {
                    return Err(error("repeated pagination cursor"));
                }
            }
            Err(error("too many catalog pages"))
        };
        let (catalog, info) =
            match tokio::time::timeout(Duration::from_millis(config.timeout_ms), setup).await {
                Ok(Ok(value)) => value,
                Ok(Err(error)) => {
                    session.shutdown().await;
                    return Err(error);
                }
                Err(_) => {
                    session.shutdown().await;
                    return Err(error("MCP connection setup timed out"));
                }
            };
        Ok(Arc::new(Self {
            session: Mutex::new(session),
            catalog,
            info,
            timeout_ms: config.timeout_ms,
            max_bytes: config.max_message_bytes,
            busy: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            cancellation: CancellationToken::new(),
        }))
    }
    pub fn tools(&self) -> &[McpTool] {
        &self.catalog
    }
    pub fn info(&self) -> &Value {
        &self.info
    }
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
    pub fn cancel(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.cancellation.cancel();
    }
    pub async fn close(&self) {
        self.cancel();
        self.session.lock().await.shutdown().await;
    }
    pub fn start_call(
        self: &Arc<Self>,
        name: String,
        arguments: Value,
        context: Value,
    ) -> Result<McpRequest> {
        if self.is_closed() {
            return Err(error("client is closed"));
        }
        let tool = self
            .catalog
            .iter()
            .find(|tool| tool.name == name)
            .ok_or_else(|| error("unknown MCP tool"))?
            .clone();
        if !arguments.is_object() || !tool.input.is_valid(&arguments) {
            return Err(Error::Input(
                "MCP tool arguments do not match its schema".into(),
            ));
        }
        if !context.is_object() {
            return Err(Error::Input("MCP context must be an object".into()));
        }
        let params = json!({"name":name,"arguments":arguments,"_meta":{"hive/context":context}});
        self.start_rpc("tools/call", params, ResponseKind::Tool(tool))
    }

    /// Explicit resource/prompt operations; never injected into agent context.
    pub fn start_catalog_request(
        self: &Arc<Self>,
        method: &str,
        params: Value,
    ) -> Result<McpRequest> {
        if !params.is_object() {
            return Err(Error::Input("MCP params must be an object".into()));
        }
        let (method, capability, response) = match method {
            "resources/list" => ("resources/list", "resources", ResponseKind::Resources),
            "resources/read" => ("resources/read", "resources", ResponseKind::Resource),
            "prompts/list" => ("prompts/list", "prompts", ResponseKind::Prompts),
            "prompts/get" => ("prompts/get", "prompts", ResponseKind::Prompt),
            _ => return Err(Error::Input("unsupported MCP catalog operation".into())),
        };
        if !self.info["capabilities"][capability].is_object() {
            return Err(error("server does not advertise this capability"));
        }
        let schema = match method {
            "resources/read" => {
                json!({"type":"object","properties":{"uri":{"type":"string","minLength":1}},"required":["uri"],"additionalProperties":false})
            }
            "prompts/get" => {
                json!({"type":"object","properties":{"name":{"type":"string","minLength":1},"arguments":{"type":"object","additionalProperties":{"type":"string"}}},"required":["name"],"additionalProperties":false})
            }
            _ => {
                json!({"type":"object","properties":{"cursor":{"type":"string","minLength":1}},"additionalProperties":false})
            }
        };
        if !crate::validation::compile(&schema)?.is_valid(&params) {
            return Err(Error::Input("invalid MCP catalog parameters".into()));
        }
        self.start_rpc(method, params, response)
    }

    fn start_rpc(
        self: &Arc<Self>,
        method: &'static str,
        params: Value,
        response: ResponseKind,
    ) -> Result<McpRequest> {
        if self.is_closed() {
            return Err(error("client is closed"));
        }
        if serde_json::to_vec(
            &json!({"jsonrpc":"2.0","id":9_007_199_254_740_991u64,"method":method,"params":params}),
        )?
        .len()
            + 1
            > self.max_bytes
        {
            return Err(error("MCP request exceeds byte limit"));
        }
        if self
            .busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(error("client already has an active request"));
        }
        let (sender, receiver) = oneshot::channel();
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let client = self.clone();
        tokio::spawn(async move {
            let mut session = client.session.lock().await;
            let result = tokio::select! {biased;
                _=client.cancellation.cancelled()=>Err(Error::McpOperationUncertain),
                _=token.cancelled()=>Err(Error::McpOperationUncertain),
                result=tokio::time::timeout(Duration::from_millis(client.timeout_ms),session.rpc(method,params))=>match result {
                    Ok(Ok(result))=>response.normalize(result),
                    Ok(Err(Error::McpRemote(message)))=>Err(Error::McpRemote(message)),
                    _=>Err(Error::McpOperationUncertain),
                }
            };
            if matches!(result, Err(Error::McpOperationUncertain)) {
                client.cancel();
                session.shutdown().await;
            }
            client.busy.store(false, Ordering::SeqCst);
            let _ = sender.send(result);
        });
        Ok(McpRequest {
            receiver: Mutex::new(Some(receiver)),
            cancel,
        })
    }
}
enum ResponseKind {
    Tool(McpTool),
    Resources,
    Resource,
    Prompts,
    Prompt,
}
fn optional_text(value: &Value, key: &str) -> bool {
    value.get(key).is_none_or(Value::is_string)
}
impl ResponseKind {
    fn normalize(self, result: Value) -> Result<Value> {
        if let Self::Tool(tool) = self {
            return normalize_result(result, &tool);
        }
        let valid = match self {
            Self::Resources => result["resources"].as_array().is_some_and(|items| {
                items.iter().all(|item| {
                    item["uri"].is_string()
                        && item["name"].is_string()
                        && optional_text(item, "description")
                        && optional_text(item, "mimeType")
                })
            }),
            Self::Resource => result["contents"].as_array().is_some_and(|items| {
                items.iter().all(|item| {
                    item["uri"].is_string()
                        && (item["text"].is_string() ^ item["blob"].is_string())
                        && optional_text(item, "mimeType")
                })
            }),
            Self::Prompts => result["prompts"].as_array().is_some_and(|items| {
                items.iter().all(|item| {
                    item["name"].is_string()
                        && optional_text(item, "description")
                        && item.get("arguments").is_none_or(|arguments| {
                            arguments.as_array().is_some_and(|args| {
                                args.iter().all(|arg| {
                                    arg["name"].is_string()
                                        && optional_text(arg, "description")
                                        && arg.get("required").is_none_or(Value::is_boolean)
                                })
                            })
                        })
                })
            }),
            Self::Prompt => result["messages"].as_array().is_some_and(|items| {
                items.iter().all(|item| {
                    matches!(item["role"].as_str(), Some("user" | "assistant"))
                        && valid_content(&item["content"])
                })
            }),
            Self::Tool(_) => unreachable!(),
        };
        let cursor = result
            .get("nextCursor")
            .is_none_or(|value| value.as_str().is_some_and(|s| !s.is_empty()));
        if !result.is_object() || !valid || !cursor || !optional_text(&result, "description") {
            return Err(Error::McpOperationUncertain);
        }
        Ok(result)
    }
}
fn normalize_result(result: Value, tool: &McpTool) -> Result<Value> {
    if !result["content"].is_array()
        || result["content"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| !valid_content(item)))
        || result.get("isError").is_some_and(|v| !v.is_boolean())
        || result
            .get("structuredContent")
            .is_some_and(|v| !v.is_object())
    {
        return Err(Error::McpOperationUncertain);
    }
    if result["isError"].as_bool() == Some(true) {
        return Ok(
            json!({"error":{"code":"MCP_TOOL_ERROR","message":"MCP tool reported an execution error","result":result}}),
        );
    }
    if let Some(output) = &tool.output
        && result
            .get("structuredContent")
            .is_none_or(|value| !output.is_valid(value))
    {
        return Ok(
            json!({"error":{"code":"INVALID_MCP_OUTPUT","message":"MCP structured result does not match outputSchema","result":result}}),
        );
    }
    Ok(result)
}
fn valid_content(item: &Value) -> bool {
    match item["type"].as_str() {
        Some("text") => item["text"].is_string(),
        Some("image" | "audio") => item["data"].is_string() && item["mimeType"].is_string(),
        Some("resource_link") => item["uri"].is_string() && item["name"].is_string(),
        Some("resource") => {
            item["resource"]["uri"].is_string()
                && (item["resource"]["text"].is_string() || item["resource"]["blob"].is_string())
        }
        _ => false,
    }
}
