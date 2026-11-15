use hive_agent_core::{Error, mcp::McpClient};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

struct Peer {
    port: u16,
    requests: Arc<Mutex<Vec<Value>>>,
    stop: CancellationToken,
    task: JoinHandle<()>,
}
impl Peer {
    async fn close(self) {
        self.stop.cancel();
        self.task.await.unwrap();
    }
    fn calls(&self) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["method"] == "tools/call")
            .count()
    }
    async fn client(&self) -> std::result::Result<Arc<McpClient>, Error> {
        McpClient::connect_json(&json!({"transport":"http","url":format!("http://127.0.0.1:{}/mcp",self.port),"headers":{"Authorization":"Bearer test-only"},"timeout_ms":2000,"max_messages":4}).to_string()).await
    }
}
async fn request(socket: &mut TcpStream) -> (String, String, Value) {
    let mut bytes = vec![];
    loop {
        let mut chunk = [0; 4096];
        let n = socket.read(&mut chunk).await.unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(end) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
            let length: usize = header
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .map(|v| v.trim().parse().unwrap())
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                let value = if length == 0 {
                    Value::Null
                } else {
                    serde_json::from_slice(&bytes[end + 4..end + 4 + length]).unwrap()
                };
                return (header.lines().next().unwrap().to_owned(), header, value);
            }
        }
    }
}
async fn send(socket: &mut TcpStream, status: u16, content_type: &str, extra: &str, body: &str) {
    socket.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",body.len()).as_bytes()).await.unwrap();
}
async fn peer(mode: &'static str) -> Peer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(vec![]));
    let output = requests.clone();
    let stop = CancellationToken::new();
    let cancel = stop.clone();
    let ack = Arc::new(Notify::new());
    let task = tokio::spawn(async move {
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                _=cancel.cancelled()=>{tasks.abort_all();while let Some(result)=tasks.join_next().await{if let Err(error)=result{assert!(error.is_cancelled());}}break;},
                result=tasks.join_next(),if !tasks.is_empty()=>{result.unwrap().unwrap();},
                accepted=listener.accept()=>{let(mut socket,_)=accepted.unwrap();let requests=output.clone();let ack=ack.clone();tasks.spawn(async move{
                    let (route,headers,value)=request(&mut socket).await;assert!(headers.contains("authorization: bearer test-only"));
                    if route.starts_with("delete "){assert!(headers.contains("mcp-session-id: session-1"));send(&mut socket,204,"application/json","","").await;return;}
                    assert!(route.starts_with("post /mcp "));assert!(headers.contains("application/json, text/event-stream"));
                    if value["method"]!="initialize"{assert!(headers.contains("mcp-session-id: session-1"));assert!(headers.contains("mcp-protocol-version: 2025-11-25"));}
                    requests.lock().unwrap().push(value.clone());
                    if value.get("method").is_none(){ack.notify_one();send(&mut socket,202,"application/json","","").await;return;}
                    if value["method"]=="notifications/initialized"{send(&mut socket,202,"application/json","","").await;return;}
                    let id=value["id"].clone();
                    if mode=="redirect"{send(&mut socket,302,"application/json","Location: /elsewhere\r\n","").await;return;}
                    let result=match value["method"].as_str().unwrap(){
                        "initialize"=>{assert_eq!(value["params"]["capabilities"],json!({}));json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"http-test","version":"1"}})},
                        "tools/list"=>json!({"tools":[{"name":"add","description":"Add","inputSchema":{"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}}]}),
                        "tools/call"=>json!({"content":[{"type":"text","text":"5"}],"structuredContent":{"answer":5}}),
                        other=>panic!("unexpected {other}"),
                    };
                    if value["method"]=="tools/call"{
                        if mode=="lost"{return;}
                        if mode=="http_error"{send(&mut socket,500,"application/json","","{}").await;return;}
                        if mode=="remote_error"{send(&mut socket,200,"application/json","",&json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"known failure"}}).to_string()).await;return;}
                        if mode=="sse"{
                            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").await.unwrap();
                            for(method,id)in[("ping","server-ping"),("sampling/createMessage","server-sampling")]{socket.write_all(format!("data: {}\n\n",json!({"jsonrpc":"2.0","id":id,"method":method})).as_bytes()).await.unwrap();tokio::time::timeout(Duration::from_secs(1),ack.notified()).await.unwrap();}
                            socket.write_all(format!("data: {}\n\n",json!({"jsonrpc":"2.0","id":id,"result":result})).as_bytes()).await.unwrap();return;
                        }
                    }
                    let wrong=mode=="wrong_id"&&value["method"]=="tools/call";
                    let extra=if value["method"]=="initialize"{"Mcp-Session-Id: session-1\r\n"}else if mode=="changed_session"&&value["method"]=="tools/call"{"Mcp-Session-Id: different\r\n"}else{""};
                    send(&mut socket,200,"application/json",extra,&json!({"jsonrpc":"2.0","id":if wrong{json!(999)}else{id},"result":result}).to_string()).await;
                });}
            }
        }
    });
    Peer {
        port,
        requests,
        stop,
        task,
    }
}

#[tokio::test]
async fn streamable_http_lifecycle_session_headers_catalog_and_call() {
    let peer = peer("json").await;
    let client = peer.client().await.unwrap();
    let result = client
        .start_call(
            "add".into(),
            json!({"a":2,"b":3}),
            json!({"execution_id":"durable"}),
        )
        .unwrap()
        .result()
        .await
        .unwrap();
    assert_eq!(result["structuredContent"]["answer"], 5);
    assert_eq!(peer.calls(), 1);
    client.close().await;
    peer.close().await;
}

#[tokio::test]
async fn sse_server_requests_receive_replies_without_enabling_sampling() {
    let peer = peer("sse").await;
    let client = peer.client().await.unwrap();
    let result = client
        .start_call("add".into(), json!({"a":2,"b":3}), json!({}))
        .unwrap()
        .result()
        .await
        .unwrap();
    assert_eq!(result["structuredContent"]["answer"], 5);
    let requests = peer.requests.lock().unwrap().clone();
    assert!(
        requests
            .iter()
            .any(|r| r["id"] == "server-sampling" && r["error"]["code"] == -32601)
    );
    client.close().await;
    peer.close().await;
}

#[tokio::test]
async fn unconfirmed_http_results_never_reconnect_or_retry_tools() {
    for mode in ["lost", "http_error", "wrong_id", "changed_session"] {
        let peer = peer(mode).await;
        let client = peer.client().await.unwrap();
        let result = client
            .start_call("add".into(), json!({"a":2,"b":3}), json!({}))
            .unwrap()
            .result()
            .await;
        assert!(
            matches!(result, Err(Error::McpOperationUncertain)),
            "{mode}"
        );
        assert!(client.is_closed());
        assert_eq!(peer.calls(), 1);
        assert!(
            client
                .start_call("add".into(), json!({"a":2,"b":3}), json!({}))
                .is_err()
        );
        client.close().await;
        peer.close().await;
    }
}

#[tokio::test]
async fn confirmed_http_rpc_errors_keep_the_session_usable() {
    let peer = peer("remote_error").await;
    let client = peer.client().await.unwrap();
    let result = client
        .start_call("add".into(), json!({"a":2,"b":3}), json!({}))
        .unwrap()
        .result()
        .await;
    assert!(matches!(result, Err(Error::McpRemote(_))));
    assert!(!client.is_closed());
    assert_eq!(peer.calls(), 1);
    client.close().await;
    peer.close().await;
}

#[tokio::test]
async fn redirects_and_reserved_http_headers_are_not_followed_or_overridden() {
    let peer = peer("redirect").await;
    assert!(peer.client().await.is_err());
    assert_eq!(peer.requests.lock().unwrap().len(), 1);
    peer.close().await;
    for key in ["Host", "Mcp-Session-Id", "Mcp-Protocol-Version", "Accept"] {
        assert!(McpClient::connect_json(&json!({"transport":"http","url":"http://127.0.0.1:1/mcp","headers":{key:"override"}}).to_string()).await.is_err());
    }
}
