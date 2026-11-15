#![cfg(feature = "browser")]
use futures_util::{SinkExt, StreamExt};
use hive_agent_core::browser::Browser;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn matches_cdp_replies_skips_events_and_rejects_script_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let request: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        assert_eq!(request["method"], "Runtime.evaluate");
        ws.send(Message::Text(
            json!({"method":"Page.loadEventFired","params":{}}).to_string(),
        ))
        .await
        .unwrap();
        ws.send(Message::Text(
            json!({"id":request["id"],"result":{"result":{"value":"Hello"}}}).to_string(),
        ))
        .await
        .unwrap();
        let request: Value =
            serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap()).unwrap();
        ws.send(Message::Text(
            json!({"id":request["id"],"result":{"exceptionDetails":{"text":"not echoed"}}})
                .to_string(),
        ))
        .await
        .unwrap();
    });
    let browser = Browser::connect(&format!("ws://{addr}/page"), 1000)
        .await
        .unwrap();
    assert_eq!(browser.evaluate("'Hello'").await.unwrap(), "Hello");
    assert!(
        browser
            .click("#missing")
            .await
            .unwrap_err()
            .to_string()
            .contains("script threw")
    );
    browser.close().await;
    server.await.unwrap();
}

#[tokio::test]
async fn timeout_closes_uncertain_connection_without_replaying_actions() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        assert!(ws.next().await.unwrap().is_ok());
        let next = tokio::time::timeout(Duration::from_secs(1), ws.next())
            .await
            .unwrap();
        assert!(next.is_none() || next.unwrap().is_err());
    });
    let browser = Browser::connect(&format!("ws://{addr}/page"), 100)
        .await
        .unwrap();
    assert!(
        browser
            .click("#submit")
            .await
            .unwrap_err()
            .to_string()
            .contains("TIMEOUT")
    );
    assert!(
        browser
            .click("#submit")
            .await
            .unwrap_err()
            .to_string()
            .contains("closed")
    );
    server.await.unwrap();
}

#[tokio::test]
async fn cancellation_interrupts_pending_cdp_work() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (sent, received) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        ws.next().await.unwrap().unwrap();
        sent.send(()).unwrap();
        let _ = ws.next().await;
    });
    let browser = Browser::connect(&format!("ws://{addr}/page"), 1000)
        .await
        .unwrap();
    let client = browser.clone();
    let request = tokio::spawn(async move { client.evaluate("new Promise(()=>{})").await });
    received.await.unwrap();
    browser.cancel();
    assert!(
        request
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("CANCELLED")
    );
    browser.close().await;
    server.await.unwrap();
}

/// Explicit live validation, using an installed native Chromium executable.
/// No Node/Bun/Playwright process participates in browser control.
#[tokio::test]
#[ignore = "set HIVE_BROWSER_TEST_EXECUTABLE to a native Chromium binary"]
async fn native_chromium_navigate_type_click_extract_and_screenshot() {
    let executable =
        std::env::var("HIVE_BROWSER_TEST_EXECUTABLE").expect("Chromium executable required");
    let profile = tempfile::tempdir().unwrap();
    let mut child = tokio::process::Command::new(executable)
        .args([
            "--headless",
            "--no-sandbox",
            "--disable-gpu",
            "--no-first-run",
            "--no-default-browser-check",
            "--remote-debugging-port=0",
            "--remote-debugging-address=127.0.0.1",
        ])
        .arg(format!("--user-data-dir={}", profile.path().display()))
        .arg("about:blank")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let port = tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if let Some(url) = line.strip_prefix("DevTools listening on ") {
                return reqwest::Url::parse(url).unwrap().port().unwrap();
            }
        }
        panic!("browser did not expose CDP");
    })
    .await
    .unwrap();
    let drain =
        tokio::spawn(async move { while lines.next_line().await.ok().flatten().is_some() {} });
    let page: Value = reqwest::Client::new()
        .put(format!("http://127.0.0.1:{port}/json/new?about:blank"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let browser = Browser::connect(page["webSocketDebuggerUrl"].as_str().unwrap(), 5000)
        .await
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        loop {
            let (mut tcp, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = tcp.read(&mut request).await;
                let body = "<!doctype html><meta charset=utf-8><input id=q><button id=b onclick=\"document.querySelector('#out').innerText=document.querySelector('#q').value\">Save</button><p id=out>Ready</p>";
                let _ = tcp.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
            });
        }
    });
    browser
        .navigate(&format!("http://{address}"))
        .await
        .unwrap();
    let mut ready = false;
    for _ in 0..50 {
        if browser
            .evaluate("Boolean(document.querySelector('#q'))")
            .await
            .ok()
            == Some(json!(true))
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ready);
    browser.type_text("#q", "Entrega viernes 🦀").await.unwrap();
    browser.click("#b").await.unwrap();
    assert_eq!(browser.extract("#out").await.unwrap(), "Entrega viernes 🦀");
    assert!(
        !browser.screenshot().await.unwrap()["data"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    browser.close().await;
    server.abort();
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    drain.await.unwrap();
}
