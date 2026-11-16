#![cfg(all(feature = "tools", unix))]
use hive_agent_core::{Error, provider::CancellationToken, tools::NativeTools};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};

/// A fake `obscura` executable (see fixtures/fake_obscura.py) in `dir`, plus its call log.
struct Fake {
    executable: String,
    log: std::path::PathBuf,
}
fn fake(dir: &Path) -> Fake {
    let executable = dir.join("obscura");
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/fake_obscura.py"
        ),
        &executable,
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    let log = dir.join("obscura.log");
    Fake {
        executable: executable.display().to_string(),
        log,
    }
}
impl Fake {
    fn lines(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn starts(&self) -> Vec<Vec<String>> {
        self.lines()
            .iter()
            .filter_map(|line| {
                line["argv"].as_array().map(|argv| {
                    argv.iter()
                        .map(|a| a.as_str().unwrap().to_owned())
                        .collect()
                })
            })
            .collect()
    }
    fn calls(&self) -> Vec<(String, Value)> {
        self.lines()
            .iter()
            .filter_map(|line| {
                line["tool"]
                    .as_str()
                    .map(|tool| (tool.to_owned(), line["arguments"].clone()))
            })
            .collect()
    }
}
fn tools(dir: &Path, fake: &Fake, extra: Value) -> NativeTools {
    let mut policy = json!({"executable":fake.executable});
    policy
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().cloned().unwrap_or_default());
    NativeTools::from_json(json!({"root":dir,"obscura":policy})).unwrap()
}
async fn call(tools: &NativeTools, name: &str, arguments: Value) -> Result<Value, Error> {
    tools
        .execute_tool(name, arguments, &CancellationToken::new())
        .await
}
fn message(error: Error) -> String {
    match error {
        Error::Input(message) | Error::State(message) => message,
        other => other.to_string(),
    }
}
fn names(tools: &NativeTools) -> Vec<String> {
    tools
        .tool_definitions()
        .into_iter()
        .map(|tool| tool.name)
        .filter(|name| name.starts_with("obscura_"))
        .collect()
}

#[tokio::test]
async fn the_tools_exist_only_when_configured_and_sensitive_ones_are_opt_in() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake(dir.path());
    assert!(names(&NativeTools::from_json(json!({"root":dir.path()})).unwrap()).is_empty());
    let base = names(&tools(dir.path(), &fake, json!({})));
    assert_eq!(base.len(), 31);
    for tool in [
        "obscura_navigate",
        "obscura_snapshot",
        "obscura_markdown",
        "obscura_links",
        "obscura_interactive_elements",
        "obscura_detect_forms",
        "obscura_extract",
        "obscura_get_attribute",
        "obscura_count",
        "obscura_search",
        "obscura_click",
        "obscura_fill",
        "obscura_type",
        "obscura_fill_form",
        "obscura_press_key",
        "obscura_select_option",
        "obscura_scroll",
        "obscura_wait_for",
        "obscura_wait_for_text",
        "obscura_network_requests",
        "obscura_console_messages",
        "obscura_screenshot",
        "obscura_pdf",
        "obscura_tab_new",
        "obscura_tab_list",
        "obscura_tab_switch",
        "obscura_tab_close",
        "obscura_forward",
        "obscura_reload",
        "obscura_back",
        "obscura_close",
    ] {
        assert!(base.contains(&tool.to_owned()), "{tool}");
    }
    for hidden in [
        "obscura_evaluate",
        "obscura_get_cookies",
        "obscura_set_cookie",
        "obscura_clear_cookies",
        "obscura_storage_state",
        "obscura_set_storage_state",
    ] {
        assert!(
            !base.contains(&hidden.to_owned()),
            "{hidden} must be opt-in"
        );
    }
    assert!(
        names(&tools(dir.path(), &fake, json!({"allow_evaluate":true})))
            .contains(&"obscura_evaluate".to_owned())
    );
    let credentials = names(&tools(dir.path(), &fake, json!({"allow_credentials":true})));
    assert!(
        [
            "obscura_get_cookies",
            "obscura_set_cookie",
            "obscura_clear_cookies",
            "obscura_storage_state",
            "obscura_set_storage_state"
        ]
        .iter()
        .all(|tool| credentials.contains(&(*tool).to_owned()))
    );
    assert!(
        call(
            &tools(dir.path(), &fake, json!({})),
            "obscura_evaluate",
            json!({"expression":"1"})
        )
        .await
        .is_err(),
        "a hidden tool cannot be called"
    );
    // No Chromium, Playwright or Puppeteer anywhere in the tool surface.
    let everything = serde_json::to_string(
        &tools(
            dir.path(),
            &fake,
            json!({"allow_evaluate":true,"allow_credentials":true}),
        )
        .tool_definitions(),
    )
    .unwrap()
    .to_lowercase();
    for banned in ["chromium", "playwright", "puppeteer", "chrome"] {
        assert!(
            !everything.contains(banned),
            "tool descriptions must not mention {banned}"
        );
    }
    for bad in [
        json!({"startup_timeout_ms":0}),
        json!({"call_timeout_ms":999_999}),
        json!({"proxy":"has space"}),
        json!({"user_agent":""}),
        json!({"executable":""}),
        json!({"bogus":1}),
    ] {
        assert!(
            NativeTools::from_json(json!({"root":dir.path(),"obscura":bad})).is_err(),
            "{bad}"
        );
    }
}

#[tokio::test]
async fn the_process_is_started_with_the_policy_flags_and_a_legacy_handshake_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake(dir.path());
    let configured = tools(
        dir.path(),
        &fake,
        json!({"stealth":true,"proxy":"http://proxy.local:8080","user_agent":"Hive Test/1.0","allow_private_network":true}),
    );
    assert_eq!(
        call(&configured, "obscura_tab_list", json!({}))
            .await
            .unwrap()["text"],
        "browser_tab_list ok {}"
    );
    assert_eq!(
        fake.starts(),
        [[
            "mcp",
            "--stealth",
            "--proxy",
            "http://proxy.local:8080",
            "--user-agent",
            "Hive Test/1.0",
            "--allow-private-network"
        ]]
    );
    assert_eq!(
        fake.lines()
            .iter()
            .filter(|line| line["notification"] == "notifications/initialized")
            .count(),
        1,
        "the client completes the handshake"
    );
    configured.close().await;

    let plain_dir = tempfile::tempdir().unwrap();
    let plain_fake = self::fake(plain_dir.path());
    call(
        &tools(plain_dir.path(), &plain_fake, json!({})),
        "obscura_tab_list",
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!(
        plain_fake.starts(),
        [["mcp"]],
        "no flags unless the policy asks for them"
    );

    // The reply must be a revision this client knows; anything else is refused.
    let odd_dir = tempfile::tempdir().unwrap();
    let odd = self::fake(odd_dir.path());
    let error = message(
        call(
            &tools(odd_dir.path(), &odd, json!({"user_agent":"bad-revision"})),
            "obscura_tab_list",
            json!({}),
        )
        .await
        .unwrap_err(),
    );
    assert!(error.contains("unsupported MCP revision"), "{error}");
    let missing = tools(
        dir.path(),
        &fake,
        json!({"executable":"/nonexistent/obscura"}),
    );
    assert!(
        message(
            call(&missing, "obscura_tab_list", json!({}))
                .await
                .unwrap_err()
        )
        .contains("does not exist")
    );
}

#[tokio::test]
async fn calls_map_to_server_tools_forward_only_declared_arguments_and_keep_one_session() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake(dir.path());
    let tools = tools(dir.path(), &fake, json!({}));
    let page = call(
        &tools,
        "obscura_navigate",
        json!({"url":"https://example.com/a","waitUntil":"domcontentloaded","timeout_ms":5000}),
    )
    .await
    .unwrap();
    assert_eq!(
        page,
        json!({"text":"URL: https://example.com/a\nTitle: Fake page","truncated":false})
    );
    call(&tools, "obscura_fill_form", json!({"fields":[{"selector":"#q","value":"hola 🦀"},{"ref":"e3","value":"1","type":"check"}],"submit_selector":"button"})).await.unwrap();
    call(&tools, "obscura_search", json!({"query":"x","limit":3}))
        .await
        .unwrap();
    let calls = fake.calls();
    assert_eq!(
        calls[0],
        (
            "browser_navigate".to_owned(),
            json!({"url":"https://example.com/a","waitUntil":"domcontentloaded"})
        ),
        "timeout_ms stays on the SDK side and obscura_ becomes browser_"
    );
    assert_eq!(calls[1].0, "browser_fill_form");
    assert_eq!(
        calls[1].1["fields"][0],
        json!({"selector":"#q","value":"hola 🦀"})
    );
    assert_eq!(
        calls[2],
        ("browser_search".to_owned(), json!({"query":"x","limit":3}))
    );
    assert_eq!(fake.starts().len(), 1, "one live session serves every call");
    assert!(
        fake.lines()
            .iter()
            .any(|line| line["client_reply"]["id"] == "srv-1"
                && line["client_reply"]["result"] == json!({})),
        "the server's ping is answered"
    );

    // Server-side errors are known errors: the session stays up.
    let error = message(
        call(
            &tools,
            "obscura_navigate",
            json!({"url":"https://error.example/"}),
        )
        .await
        .unwrap_err(),
    );
    assert_eq!(error, "OBSCURA_ERROR: navigation failed: DNS error");
    assert_eq!(
        call(&tools, "obscura_count", json!({"selector":"a"}))
            .await
            .unwrap()["text"],
        "browser_count ok {\"selector\": \"a\"}"
    );
    assert_eq!(fake.starts().len(), 1);

    // Invalid arguments never reach the server.
    let before = fake.calls().len();
    for (name, arguments) in [
        ("obscura_navigate", json!({})),
        ("obscura_click", json!({"selector":""})),
        ("obscura_wait_for", json!({"selector":"a","timeout":999})),
        ("obscura_scroll", json!({"direction":"sideways"})),
        ("obscura_fill", json!({"value":"x","extra":1})),
    ] {
        assert!(
            message(call(&tools, name, arguments.clone()).await.unwrap_err())
                .contains("do not match schema"),
            "{name} {arguments}"
        );
    }
    assert!(
        call(
            &tools,
            "obscura_navigate",
            json!({"url":"file:///etc/passwd"})
        )
        .await
        .is_err()
    );
    assert!(
        call(
            &tools,
            "obscura_navigate",
            json!({"url":"https://u:p@example.com/"})
        )
        .await
        .is_err()
    );
    assert_eq!(fake.calls().len(), before);
}

#[tokio::test]
async fn text_results_are_bounded_and_noise_or_garbage_from_the_server_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake(dir.path());
    let small = NativeTools::from_json(
        json!({"root":dir.path(),"max_output_bytes":200,"obscura":{"executable":fake.executable}}),
    )
    .unwrap();
    let snapshot = call(&small, "obscura_snapshot", json!({})).await.unwrap();
    assert_eq!(
        (
            snapshot["text"].as_str().unwrap().len(),
            snapshot["truncated"].clone()
        ),
        (200, json!(true))
    );
    // A non-JSON line before the reply is ignored.
    let result = call(&small, "obscura_click", json!({"selector":"partial"}))
        .await
        .unwrap();
    assert!(
        result["text"]
            .as_str()
            .unwrap()
            .starts_with("browser_click ok")
    );
    assert_eq!(fake.starts().len(), 1);
}

#[tokio::test]
async fn images_and_pdfs_are_saved_in_the_sandbox_or_returned_when_small() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake(dir.path());
    let tools = tools(dir.path(), &fake, json!({}));
    let inline = call(
        &tools,
        "obscura_screenshot",
        json!({"width":64,"height":48}),
    )
    .await
    .unwrap();
    assert_eq!(
        (
            inline["mime_type"].as_str(),
            inline["width"].as_u64(),
            inline["height"].as_u64()
        ),
        (Some("image/png"), Some(64), Some(48))
    );
    assert!(inline["data"].as_str().unwrap().len() > 100);
    assert_eq!(
        fake.calls().last().unwrap().1,
        json!({"width":64,"height":48}),
        "path stays on the SDK side"
    );

    let saved = call(
        &tools,
        "obscura_screenshot",
        json!({"path":"shots/page.png","create_dirs":true}),
    )
    .await
    .unwrap();
    assert_eq!(saved["path"], "shots/page.png");
    let png = std::fs::read(dir.path().join("shots/page.png")).unwrap();
    assert!(png.starts_with(b"\x89PNG") && png.len() as u64 == saved["bytes"].as_u64().unwrap());
    assert!(saved.get("data").is_none());
    assert!(
        message(
            call(&tools, "obscura_screenshot", json!({"path":"nodir/x.png"}))
                .await
                .unwrap_err()
        )
        .contains("create_dirs")
    );
    assert!(
        call(
            &tools,
            "obscura_screenshot",
            json!({"path":"../escape.png"})
        )
        .await
        .is_err()
    );
    assert!(!dir.path().parent().unwrap().join("escape.png").exists());

    let pdf = call(
        &tools,
        "obscura_pdf",
        json!({"landscape":true,"scale":1.5,"paper_width":8.5,"path":"out.pdf"}),
    )
    .await
    .unwrap();
    assert_eq!(
        (
            pdf["mime_type"].as_str(),
            pdf["text"].as_str(),
            pdf["path"].as_str()
        ),
        (Some("application/pdf"), Some("exported"), Some("out.pdf"))
    );
    assert!(
        std::fs::read(dir.path().join("out.pdf"))
            .unwrap()
            .starts_with(b"%PDF-1.4")
    );
    assert_eq!(
        fake.calls().last().unwrap().1,
        json!({"landscape":true,"scale":1.5,"paper_width":8.5})
    );

    let big = NativeTools::from_json(
        json!({"root":dir.path(),"max_output_bytes":100,"obscura":{"executable":fake.executable}}),
    )
    .unwrap();
    assert!(
        message(
            call(&big, "obscura_screenshot", json!({}))
                .await
                .unwrap_err()
        )
        .contains("pass path")
    );
    // A server that cannot render says so, and that is a plain tool error.
    assert!(
        message(
            call(&tools, "obscura_screenshot", json!({"width":999}))
                .await
                .unwrap_err()
        )
        .contains("not supported")
    );
}

#[tokio::test]
async fn an_allowlist_applies_to_the_requested_url_and_to_where_the_page_ends_up() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake(dir.path());
    let tools = tools(dir.path(), &fake, json!({"allowed_hosts":["example.com"]}));
    let refused = message(
        call(
            &tools,
            "obscura_navigate",
            json!({"url":"https://evil.example/"}),
        )
        .await
        .unwrap_err(),
    );
    assert!(refused.contains("allowed_hosts"), "{refused}");
    assert!(
        fake.calls().is_empty(),
        "a refused URL never reaches the browser"
    );
    assert!(
        call(
            &tools,
            "obscura_tab_new",
            json!({"url":"https://evil.example/"})
        )
        .await
        .is_err()
    );

    call(
        &tools,
        "obscura_navigate",
        json!({"url":"https://example.com/ok"}),
    )
    .await
    .unwrap();
    let redirected = message(
        call(
            &tools,
            "obscura_navigate",
            json!({"url":"https://example.com/redir-bad"}),
        )
        .await
        .unwrap_err(),
    );
    assert!(
        redirected.contains("ended on a blocked address"),
        "{redirected}"
    );
    let names: Vec<String> = fake.calls().into_iter().map(|(tool, _)| tool).collect();
    assert_eq!(
        names.last().unwrap(),
        "browser_close",
        "the blocked page is closed"
    );
    assert_eq!(
        call(
            &tools,
            "obscura_navigate",
            json!({"url":"https://example.com/again"})
        )
        .await
        .unwrap()["text"],
        "URL: https://example.com/again\nTitle: Fake page",
        "the session keeps working"
    );

    let unreadable = message(
        call(
            &tools,
            "obscura_navigate",
            json!({"url":"https://example.com/noaddr"}),
        )
        .await
        .unwrap_err(),
    );
    assert!(
        unreadable.contains("could not be read"),
        "an unreadable address fails closed: {unreadable}"
    );
    // Without an allowlist no extra address check runs.
    let open_dir = tempfile::tempdir().unwrap();
    let open_fake = self::fake(open_dir.path());
    call(
        &self::tools(open_dir.path(), &open_fake, json!({})),
        "obscura_navigate",
        json!({"url":"https://anything.example/redir-bad"}),
    )
    .await
    .unwrap();
    assert_eq!(open_fake.calls().len(), 1);
}

#[tokio::test]
async fn a_lost_call_discards_the_session_and_the_next_call_starts_a_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake(dir.path());
    let tools = tools(dir.path(), &fake, json!({}));
    call(&tools, "obscura_tab_list", json!({})).await.unwrap();
    assert_eq!(fake.starts().len(), 1);

    let started = std::time::Instant::now();
    let error = tools
        .execute_tool(
            "obscura_navigate",
            json!({"url":"https://hang.example/","timeout_ms":400}),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, Error::Timeout) && started.elapsed() < Duration::from_secs(5),
        "{error:?}"
    );
    call(&tools, "obscura_tab_list", json!({})).await.unwrap();
    assert_eq!(
        fake.starts().len(),
        2,
        "a timed-out call is never replayed on the old session"
    );

    let crashed = message(
        call(
            &tools,
            "obscura_navigate",
            json!({"url":"https://crash.example/"}),
        )
        .await
        .unwrap_err(),
    );
    assert!(crashed.contains("closed the connection"), "{crashed}");
    call(&tools, "obscura_tab_list", json!({})).await.unwrap();
    assert_eq!(fake.starts().len(), 3);

    let cancel = CancellationToken::new();
    let stopper = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        stopper.cancel();
    });
    let error = tools
        .execute_tool(
            "obscura_navigate",
            json!({"url":"https://hang.example/"}),
            &cancel,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled));
    call(&tools, "obscura_tab_list", json!({})).await.unwrap();
    assert_eq!(fake.starts().len(), 4);
    assert!(call(&tools, "obscura_tab_list", json!({})).await.is_ok());
    assert_eq!(fake.starts().len(), 4, "a healthy session is reused");

    tools.close().await;
    call(&tools, "obscura_tab_list", json!({})).await.unwrap();
    assert_eq!(fake.starts().len(), 5, "close() ends the session");
    tools.close_now();
    call(&tools, "obscura_tab_list", json!({})).await.unwrap();
    assert_eq!(fake.starts().len(), 6, "close_now() does too");
}

#[tokio::test]
async fn evaluate_and_credential_tools_work_only_when_enabled_and_calls_are_serialized() {
    let dir = tempfile::tempdir().unwrap();
    let fake = fake(dir.path());
    let tools = tools(
        dir.path(),
        &fake,
        json!({"allow_evaluate":true,"allow_credentials":true}),
    );
    assert_eq!(
        call(&tools, "obscura_evaluate", json!({"expression":"1+1"}))
            .await
            .unwrap()["text"],
        "{\"echo\": \"1+1\"}"
    );
    call(
        &tools,
        "obscura_set_cookie",
        json!({"name":"sid","value":"abc","domain":".example.com","secure":true}),
    )
    .await
    .unwrap();
    assert_eq!(
        fake.calls().last().unwrap(),
        &(
            "browser_set_cookie".to_owned(),
            json!({"name":"sid","value":"abc","domain":".example.com","secure":true})
        )
    );
    let (a, b, c) = tokio::join!(
        call(
            &tools,
            "obscura_navigate",
            json!({"url":"https://example.com/1"})
        ),
        call(
            &tools,
            "obscura_navigate",
            json!({"url":"https://example.com/2"})
        ),
        call(&tools, "obscura_snapshot", json!({"max_chars":50})),
    );
    assert!(a.is_ok() && b.is_ok() && c.is_ok());
    assert_eq!(
        fake.starts().len(),
        1,
        "concurrent calls share and serialize on one session"
    );
}

// ---- the real Obscura binary -------------------------------------------------------
//
// Run with `OBSCURA_TEST_EXECUTABLE=/path/to/obscura cargo test --test obscura_tools real_`.
// Without it these tests are skipped and do not count as passed.

fn real_binary() -> Option<String> {
    let path = std::env::var("OBSCURA_TEST_EXECUTABLE")
        .ok()
        .filter(|p| !p.is_empty());
    if path.is_none() {
        eprintln!("skipped: set OBSCURA_TEST_EXECUTABLE to run against the real Obscura");
    }
    path
}

/// A tiny HTTP server on loopback serving fixed pages; returns its base URL.
fn serve() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buffer = [0u8; 4096];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let path = request.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let body = match path.as_str() {
                "/second" => "<html><head><title>Segunda</title></head><body><h1>Segunda página</h1><p id='note'>Llegaste aquí</p></body></html>".to_owned(),
                _ => "<html><head><title>Página de prueba</title></head><body><h1>Hola Obscura</h1><p class='intro'>Texto de ejemplo con ñ y acentos áéí.</p>\
                      <a href='/second'>Ir a la segunda</a><ul><li>uno</li><li>dos</li><li>tres</li></ul>\
                      <form action='/second' method='get'><input id='q' name='q' type='text'><button id='go' type='submit'>Buscar</button></form></body></html>".to_owned(),
            };
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    base
}

#[tokio::test]
async fn real_obscura_reads_navigates_and_extracts() {
    let Some(binary) = real_binary() else { return };
    let dir = tempfile::tempdir().unwrap();
    let base = serve();
    let tools = NativeTools::from_json(json!({"root":dir.path(),"obscura":{"executable":binary,"allow_private_network":true,"allow_evaluate":true,"allow_credentials":true}})).unwrap();
    let page = call(
        &tools,
        "obscura_navigate",
        json!({"url":format!("{base}/")}),
    )
    .await
    .unwrap();
    eprintln!("navigate: {page}");
    assert!(
        page["text"].as_str().unwrap().contains("Página de prueba"),
        "{page}"
    );
    let snapshot = call(&tools, "obscura_snapshot", json!({})).await.unwrap();
    eprintln!("snapshot: {snapshot}");
    let text = snapshot["text"].as_str().unwrap();
    assert!(
        text.contains("Hola Obscura") && text.contains("ñ y acentos áéí"),
        "UTF-8 survives the round trip: {text}"
    );
    let markdown = call(&tools, "obscura_markdown", json!({})).await.unwrap();
    eprintln!("markdown: {markdown}");
    assert!(markdown["text"].as_str().unwrap().contains("Hola Obscura"));
    let links = call(&tools, "obscura_links", json!({})).await.unwrap();
    eprintln!("links: {links}");
    assert!(links["text"].as_str().unwrap().contains("second"));
    assert!(
        call(&tools, "obscura_count", json!({"selector":"li"}))
            .await
            .unwrap()["text"]
            .as_str()
            .unwrap()
            .contains('3')
    );
    let extracted = call(
        &tools,
        "obscura_extract",
        json!({"schema":{"title":"h1","items[]":"li","link":"a@href"}}),
    )
    .await
    .unwrap();
    eprintln!("extract: {extracted}");
    let parsed: Value = serde_json::from_str(extracted["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        parsed,
        json!({"title":"Hola Obscura","items":["uno","dos","tres"],"link":"/second"}),
        "`field[]` collects every match and `selector@attr` reads an attribute"
    );
    assert!(
        call(&tools, "obscura_search", json!({"query":"ejemplo"}))
            .await
            .unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("ejemplo")
    );
    let value = call(
        &tools,
        "obscura_evaluate",
        json!({"expression":"document.querySelectorAll('li').length"}),
    )
    .await
    .unwrap();
    eprintln!("evaluate: {value}");
    assert!(value["text"].as_str().unwrap().contains('3'));
    // Acting: fill the form and follow the link.
    let forms = call(&tools, "obscura_detect_forms", json!({}))
        .await
        .unwrap();
    eprintln!("forms: {forms}");
    call(
        &tools,
        "obscura_fill",
        json!({"selector":"#q","value":"hive"}),
    )
    .await
    .unwrap();
    let typed = call(
        &tools,
        "obscura_evaluate",
        json!({"expression":"document.querySelector('#q').value"}),
    )
    .await
    .unwrap();
    assert!(typed["text"].as_str().unwrap().contains("hive"), "{typed}");
    call(&tools, "obscura_click", json!({"selector":"a"}))
        .await
        .unwrap();
    let after = call(&tools, "obscura_snapshot", json!({})).await.unwrap();
    eprintln!("after click: {after}");
    assert!(
        after["text"].as_str().unwrap().contains("Segunda página"),
        "{after}"
    );
    let tabs = call(&tools, "obscura_tab_list", json!({})).await.unwrap();
    eprintln!("tabs: {tabs}");
    tools.close().await;
}

#[tokio::test]
async fn real_obscura_blocks_private_networks_by_default_and_honors_the_allowlist() {
    let Some(binary) = real_binary() else { return };
    let dir = tempfile::tempdir().unwrap();
    let base = serve();
    let blocked =
        NativeTools::from_json(json!({"root":dir.path(),"obscura":{"executable":binary.clone()}}))
            .unwrap();
    let error = message(
        call(
            &blocked,
            "obscura_navigate",
            json!({"url":format!("{base}/")}),
        )
        .await
        .unwrap_err(),
    );
    eprintln!("ssrf: {error}");
    assert!(
        error.contains("OBSCURA_ERROR") && error.to_lowercase().contains("private"),
        "{error}"
    );
    // The session survives a tool error.
    assert!(call(&blocked, "obscura_tab_list", json!({})).await.is_ok());
    blocked.close().await;
    // The allowlist refuses before the browser is asked; an allowed host works.
    let listed = NativeTools::from_json(json!({"root":dir.path(),"obscura":{"executable":binary,"allow_private_network":true,"allowed_hosts":["127.0.0.1"]}})).unwrap();
    assert!(
        call(
            &listed,
            "obscura_navigate",
            json!({"url":"https://example.com/"})
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("allowed_hosts")
    );
    let ok = call(
        &listed,
        "obscura_navigate",
        json!({"url":format!("{base}/")}),
    )
    .await;
    eprintln!("allowed: {ok:?}");
    assert!(ok.is_ok(), "{ok:?}");
    listed.close().await;
}

#[tokio::test]
async fn real_obscura_screenshot_pdf_and_cookies_report_what_this_build_supports() {
    let Some(binary) = real_binary() else { return };
    let dir = tempfile::tempdir().unwrap();
    let base = serve();
    let tools = NativeTools::from_json(json!({"root":dir.path(),"office":{},"obscura":{"executable":binary,"allow_private_network":true,"allow_credentials":true}})).unwrap();
    call(
        &tools,
        "obscura_navigate",
        json!({"url":format!("{base}/")}),
    )
    .await
    .unwrap();
    let shot = call(&tools, "obscura_screenshot", json!({"path":"shot.png"})).await;
    eprintln!("screenshot: {shot:?}");
    let pdf = call(&tools, "obscura_pdf", json!({"path":"page.pdf"})).await;
    eprintln!("pdf: {pdf:?}");
    if shot.is_ok() {
        assert!(
            std::fs::read(dir.path().join("shot.png"))
                .unwrap()
                .starts_with(b"\x89PNG")
        );
    }
    if pdf.is_ok() {
        assert!(
            std::fs::read(dir.path().join("page.pdf"))
                .unwrap()
                .starts_with(b"%PDF")
        );
        // Obscura's PDF is the rendered page as one JPEG on a single page, with no text layer:
        // our reader must say so rather than invent text, exactly like poppler does.
        let read = tools
            .execute_tool(
                "office_read_pdf",
                json!({"path":"page.pdf"}),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        eprintln!("office_read_pdf of Obscura PDF: {read}");
        assert_eq!(read["text"], "");
        assert_eq!(read["pages"].as_array().unwrap().len(), 1);
        assert!(
            !read["warnings"].as_array().unwrap().is_empty(),
            "an image-only PDF is reported: {read}"
        );
    }
    call(
        &tools,
        "obscura_set_cookie",
        json!({"name":"sid","value":"abc","domain":"127.0.0.1"}),
    )
    .await
    .unwrap();
    let cookies = call(&tools, "obscura_get_cookies", json!({}))
        .await
        .unwrap();
    eprintln!("cookies: {cookies}");
    assert!(cookies["text"].as_str().unwrap().contains("sid"));
    let state = call(&tools, "obscura_storage_state", json!({}))
        .await
        .unwrap();
    eprintln!(
        "storage_state: {}",
        &state.to_string()[..state.to_string().len().min(300)]
    );
    tools.close().await;
}

/// Downloads the real latest release (~70 MB). Run with `HIVE_TEST_NETWORK=1`.
#[tokio::test]
async fn real_install_downloads_the_latest_release_verifies_it_and_it_browses() {
    if std::env::var("HIVE_TEST_NETWORK").as_deref() != Ok("1") {
        eprintln!("skipped: set HIVE_TEST_NETWORK=1 to download the real Obscura");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let install_dir = dir.path().join("bin");
    let tools = NativeTools::from_json(json!({"root":dir.path(),"office":{},"obscura":{"install_dir":install_dir,"allow_private_network":true}})).unwrap();
    let report = tools.install_obscura(true).await.unwrap();
    eprintln!("installed: {report}");
    let path = std::path::PathBuf::from(report["path"].as_str().unwrap());
    assert!(path.starts_with(&install_dir) && path.is_file());
    assert_eq!(report["already_current"], false);
    assert!(report["version"].as_str().unwrap().starts_with('v'));
    assert_eq!(report["sha256"].as_str().unwrap().len(), 64);
    // It is a working Obscura: ask it its version, then browse with it through the SDK.
    let version = std::process::Command::new(&path)
        .arg("--version")
        .output()
        .unwrap();
    let version = String::from_utf8_lossy(&version.stdout).to_string();
    eprintln!("version: {version}");
    assert!(version.starts_with("obscura "), "{version}");
    assert!(
        version.contains(report["version"].as_str().unwrap().trim_start_matches('v')),
        "{version} vs {report}"
    );
    // A second install keeps it; an update of the same release is a no-op.
    assert_eq!(
        tools.install_obscura(false).await.unwrap()["already_current"],
        true
    );
    assert_eq!(
        tools.install_obscura(true).await.unwrap()["already_current"],
        true
    );
    let base = serve();
    let browsing = NativeTools::from_json(json!({"root":dir.path(),"office":{},"obscura":{"executable":path,"allow_private_network":true}})).unwrap();
    let page = call(
        &browsing,
        "obscura_navigate",
        json!({"url":format!("{base}/")}),
    )
    .await
    .unwrap();
    assert!(
        page["text"].as_str().unwrap().contains("Página de prueba"),
        "{page}"
    );
    let pdf = call(&browsing, "obscura_pdf", json!({"path":"latest.pdf"}))
        .await
        .unwrap();
    assert_eq!(pdf["mime_type"], "application/pdf");
    assert!(
        std::fs::read(dir.path().join("latest.pdf"))
            .unwrap()
            .starts_with(b"%PDF")
    );
    browsing.close().await;
}

#[tokio::test]
async fn installing_needs_the_obscura_tools_and_a_valid_install_dir() {
    let dir = tempfile::tempdir().unwrap();
    let off = NativeTools::from_json(json!({"root":dir.path()})).unwrap();
    assert!(message(off.install_obscura(false).await.unwrap_err()).contains("disabled"));
    for bad in [
        json!({"install_dir":"relative/bin"}),
        json!({"install_dir":""}),
    ] {
        assert!(
            NativeTools::from_json(json!({"root":dir.path(),"obscura":bad.clone()})).is_err(),
            "{bad}"
        );
    }
    assert!(
        NativeTools::from_json(
            json!({"root":dir.path(),"obscura":{"install_dir":dir.path(),"auto_install":false}})
        )
        .is_ok()
    );
    // With automatic installation off and no Obscura anywhere, a call says how to get one.
    let missing = NativeTools::from_json(json!({"root":dir.path(),"obscura":{"executable":"/nonexistent/obscura","auto_install":false}})).unwrap();
    assert!(
        message(
            call(&missing, "obscura_tab_list", json!({}))
                .await
                .unwrap_err()
        )
        .contains("does not exist"),
        "an explicit path that is missing is an error, never a download"
    );
}
