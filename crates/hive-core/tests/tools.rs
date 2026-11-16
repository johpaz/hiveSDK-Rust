#![cfg(feature = "tools")]
use hive_agent_core::{
    Error,
    provider::CancellationToken,
    tools::{CommandPolicy, NativeTools, SearchProvider, ToolsConfig, WebPolicy},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn tools_in(dir: &tempfile::TempDir) -> NativeTools {
    NativeTools::new(ToolsConfig::new(dir.path())).unwrap()
}

async fn call(tools: &NativeTools, name: &str, arguments: Value) -> Result<Value, Error> {
    tools
        .execute_tool(name, arguments, &CancellationToken::new())
        .await
}

fn command_policy(programs: &[&str]) -> CommandPolicy {
    CommandPolicy {
        allowed_programs: programs.iter().map(|name| name.to_string()).collect(),
        env: BTreeMap::new(),
        max_timeout_ms: 5_000,
    }
}

/// One-shot HTTP server answering every connection with `response`; returns its URL
/// and a channel of raw requests.
async fn serve(response: &'static str) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let sender = sender.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0u8; 8192];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let _ = sender.send(String::from_utf8_lossy(&buffer[..read]).into_owned());
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (url, receiver)
}

#[tokio::test]
async fn only_enabled_tools_are_advertised() {
    let dir = tempfile::tempdir().unwrap();
    let names = |tools: &NativeTools| -> Vec<String> {
        tools
            .tool_definitions()
            .into_iter()
            .map(|tool| tool.name)
            .collect()
    };
    let base = names(&tools_in(&dir));
    assert_eq!(
        base,
        [
            "fs_read", "fs_write", "fs_edit", "fs_list", "fs_glob", "fs_grep"
        ]
    );

    let mut config = ToolsConfig::new(dir.path());
    config.command = Some(command_policy(&["echo"]));
    config.web = Some(WebPolicy {
        allow_private_hosts: false,
        allowed_hosts: vec![],
        timeout_ms: 1000,
        search: None,
    });
    let with_web = names(&NativeTools::new(config).unwrap());
    assert!(with_web.contains(&"command_run".into()) && with_web.contains(&"web_fetch".into()));
    assert!(!with_web.contains(&"web_search".into()));
    assert!(
        call(&tools_in(&dir), "command_run", json!({"program":"echo"}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn invalid_configuration_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    assert!(NativeTools::new(ToolsConfig::new(dir.path().join("missing"))).is_err());
    let mut config = ToolsConfig::new(dir.path());
    config.max_read_bytes = 0;
    assert!(NativeTools::new(config).is_err());
    let mut config = ToolsConfig::new(dir.path());
    config.command = Some(command_policy(&["/bin/echo"]));
    assert!(NativeTools::new(config).is_err());
    assert!(NativeTools::from_json(json!({"root":dir.path(),"unknown":1})).is_err());
}

#[tokio::test]
async fn paths_cannot_escape_the_root() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(outer.path().join("secret.txt"), "secret").unwrap();
    let tools = NativeTools::new(ToolsConfig::new(&root)).unwrap();

    for path in ["../secret.txt", "a/../../secret.txt", "/etc/passwd"] {
        let error = call(&tools, "fs_read", json!({"path":path}))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Input(_)), "{path}: {error:?}");
    }
    assert!(
        call(&tools, "fs_write", json!({"path":"../x.txt","content":"x"}))
            .await
            .is_err()
    );
    assert!(!outer.path().join("x.txt").exists());

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outer.path().join("secret.txt"), root.join("link")).unwrap();
        std::os::unix::fs::symlink(outer.path(), root.join("dirlink")).unwrap();
        assert!(
            call(&tools, "fs_read", json!({"path":"link"}))
                .await
                .is_err()
        );
        assert!(
            call(
                &tools,
                "fs_write",
                json!({"path":"dirlink/new.txt","content":"x","create_dirs":true})
            )
            .await
            .is_err()
        );
        assert!(!outer.path().join("new.txt").exists());
        let found = call(&tools, "fs_glob", json!({"pattern":"*"}))
            .await
            .unwrap();
        assert_eq!(found["matches"], json!([]), "symlinks must not be followed");
    }
}

#[tokio::test]
async fn read_write_edit_list_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools_in(&dir);
    assert!(
        call(&tools, "fs_write", json!({"path":"a/b.txt","content":"x"}))
            .await
            .is_err()
    );
    let written = call(
        &tools,
        "fs_write",
        json!({"path":"a/b.txt","content":"one\ntwo\nthree\n","create_dirs":true}),
    )
    .await
    .unwrap();
    assert_eq!(written["bytes"], 14);
    assert!(
        std::fs::read_dir(dir.path().join("a")).unwrap().count() == 1,
        "no temp file left behind"
    );

    let window = call(
        &tools,
        "fs_read",
        json!({"path":"a/b.txt","offset":1,"limit":1}),
    )
    .await
    .unwrap();
    assert_eq!(
        (
            window["content"].as_str(),
            window["start_line"].as_u64(),
            window["truncated"].as_bool()
        ),
        (Some("two\n"), Some(2), Some(true))
    );

    let ambiguous = call(
        &tools,
        "fs_edit",
        json!({"path":"a/b.txt","old":"e","new":"E"}),
    )
    .await
    .unwrap_err();
    assert!(ambiguous.to_string().contains("matches"));
    assert!(
        call(
            &tools,
            "fs_edit",
            json!({"path":"a/b.txt","old":"zzz","new":"E"})
        )
        .await
        .is_err()
    );
    call(
        &tools,
        "fs_edit",
        json!({"path":"a/b.txt","old":"two","new":"2"}),
    )
    .await
    .unwrap();
    let all = call(
        &tools,
        "fs_edit",
        json!({"path":"a/b.txt","old":"e","new":"E","replace_all":true}),
    )
    .await
    .unwrap();
    assert_eq!(all["replacements"], 3);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a/b.txt")).unwrap(),
        "onE\n2\nthrEE\n"
    );

    let listing = call(&tools, "fs_list", json!({})).await.unwrap();
    assert_eq!(
        listing["entries"],
        json!([{"name":"a","type":"dir","size":0}])
    );
    assert!(
        call(&tools, "fs_list", json!({"path":"a/b.txt"}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn size_limits_and_binary_files_are_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = ToolsConfig::new(dir.path());
    config.max_read_bytes = 16;
    config.max_write_bytes = 8;
    let tools = NativeTools::new(config).unwrap();
    assert!(
        call(
            &tools,
            "fs_write",
            json!({"path":"big.txt","content":"123456789"})
        )
        .await
        .is_err()
    );
    std::fs::write(dir.path().join("lines.txt"), "aaaaaaa\nbbbbbbb\nccccccc\n").unwrap();
    let read = call(&tools, "fs_read", json!({"path":"lines.txt"}))
        .await
        .unwrap();
    assert_eq!(
        (read["content"].as_str(), read["truncated"].as_bool()),
        (Some("aaaaaaa\nbbbbbbb\n"), Some(true))
    );
    std::fs::write(dir.path().join("bin"), [0xff, 0xfe, 0x00]).unwrap();
    assert!(
        call(&tools, "fs_read", json!({"path":"bin"}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn glob_and_grep_search_the_tree() {
    let dir = tempfile::tempdir().unwrap();
    for (path, body) in [
        ("src/main.rs", "fn main() {}\n// TODO: later\n"),
        ("src/lib/mod.rs", "pub fn todo() {}\n"),
        ("docs/readme.md", "Todo list\n"),
        ("src/skip.bin", "TODO\n"),
    ] {
        let path = dir.path().join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    let tools = tools_in(&dir);
    let rust = call(&tools, "fs_glob", json!({"pattern":"**/*.rs"}))
        .await
        .unwrap();
    assert_eq!(rust["matches"], json!(["src/main.rs", "src/lib/mod.rs"]));
    let by_name = call(&tools, "fs_glob", json!({"pattern":"readme.?d"}))
        .await
        .unwrap();
    assert_eq!(by_name["matches"], json!(["docs/readme.md"]));
    let limited = call(&tools, "fs_glob", json!({"pattern":"*","limit":2}))
        .await
        .unwrap();
    assert_eq!(
        (
            limited["matches"].as_array().unwrap().len(),
            limited["truncated"].as_bool()
        ),
        (2, Some(true))
    );

    let exact = call(&tools, "fs_grep", json!({"pattern":"TODO"}))
        .await
        .unwrap();
    assert_eq!(exact["matches"].as_array().unwrap().len(), 2);
    let relaxed = call(
        &tools,
        "fs_grep",
        json!({"pattern":"todo","case_insensitive":true,"glob":"**/*.rs"}),
    )
    .await
    .unwrap();
    let paths: Vec<_> = relaxed["matches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        ["src/main.rs", "src/lib/mod.rs"],
        "files before subdirectories, name-sorted"
    );
    let one_file = call(
        &tools,
        "fs_grep",
        json!({"pattern":"main","path":"src/main.rs"}),
    )
    .await
    .unwrap();
    assert_eq!(one_file["matches"][0]["line"], 1);
}

#[cfg(unix)]
#[tokio::test]
async fn commands_run_only_allowlisted_programs_without_a_shell() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("sub")).unwrap();
    let mut config = ToolsConfig::new(dir.path());
    let mut policy = command_policy(&["echo", "sh", "cat", "sleep", "pwd", "env", "yes"]);
    policy.env.insert("HIVE_TEST".into(), "visible".into());
    config.command = Some(policy);
    config.max_output_bytes = 64;
    let tools = NativeTools::new(config).unwrap();

    let echoed = call(
        &tools,
        "command_run",
        json!({"program":"echo","args":["a;b","$HOME"]}),
    )
    .await
    .unwrap();
    assert_eq!(
        (echoed["stdout"].as_str(), echoed["exit_code"].as_i64()),
        (Some("a;b $HOME\n"), Some(0))
    );
    assert!(
        call(
            &tools,
            "command_run",
            json!({"program":"rm","args":["-rf","x"]})
        )
        .await
        .is_err()
    );
    assert!(
        call(&tools, "command_run", json!({"program":"/bin/echo"}))
            .await
            .is_err()
    );

    let cwd = call(&tools, "command_run", json!({"program":"pwd","cwd":"sub"}))
        .await
        .unwrap();
    assert!(cwd["stdout"].as_str().unwrap().trim_end().ends_with("/sub"));
    assert!(
        call(&tools, "command_run", json!({"program":"pwd","cwd":".."}))
            .await
            .is_err()
    );

    let env = call(
        &tools,
        "command_run",
        json!({"program":"sh","args":["-c","echo $HIVE_TEST:${HOME:-unset}"]}),
    )
    .await
    .unwrap();
    assert_eq!(
        env["stdout"], "visible:unset\n",
        "environment is cleared except policy values"
    );
    let piped = call(
        &tools,
        "command_run",
        json!({"program":"cat","stdin":"from stdin"}),
    )
    .await
    .unwrap();
    assert_eq!(piped["stdout"], "from stdin");
    let failed = call(
        &tools,
        "command_run",
        json!({"program":"sh","args":["-c","echo oops >&2; exit 3"]}),
    )
    .await
    .unwrap();
    assert_eq!(
        (failed["exit_code"].as_i64(), failed["stderr"].as_str()),
        (Some(3), Some("oops\n"))
    );

    let noisy = call(
        &tools,
        "command_run",
        json!({"program":"yes","timeout_ms":300}),
    )
    .await
    .unwrap();
    assert_eq!(noisy["stdout"].as_str().unwrap().len(), 64);
    assert_eq!(noisy["truncated"], true);
}

#[cfg(unix)]
#[tokio::test]
async fn commands_time_out_and_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = ToolsConfig::new(dir.path());
    config.command = Some(command_policy(&["sleep"]));
    let tools = NativeTools::new(config).unwrap();

    let started = std::time::Instant::now();
    let slow = call(
        &tools,
        "command_run",
        json!({"program":"sleep","args":["30"],"timeout_ms":200}),
    )
    .await
    .unwrap();
    assert_eq!(
        (slow["timed_out"].as_bool(), slow["exit_code"].is_null()),
        (Some(true), true)
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        call(
            &tools,
            "command_run",
            json!({"program":"sleep","args":["1"],"timeout_ms":6000})
        )
        .await
        .is_err(),
        "above policy maximum"
    );

    let cancel = CancellationToken::new();
    let stopper = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        stopper.cancel();
    });
    let error = tools
        .execute_tool(
            "command_run",
            json!({"program":"sleep","args":["30"]}),
            &cancel,
        )
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled));
}

fn web_tools(
    dir: &tempfile::TempDir,
    allow_private: bool,
    search: Option<SearchProvider>,
) -> NativeTools {
    let mut config = ToolsConfig::new(dir.path());
    config.max_output_bytes = 32;
    config.web = Some(WebPolicy {
        allow_private_hosts: allow_private,
        allowed_hosts: vec![],
        timeout_ms: 5000,
        search,
    });
    NativeTools::new(config).unwrap()
}

#[tokio::test]
async fn web_fetch_blocks_non_public_targets_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let (url, mut requests) = serve("HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok").await;
    let guarded = web_tools(&dir, false, None);
    for target in [
        url.as_str(),
        "http://localhost:1/",
        "http://169.254.169.254/latest",
        "http://10.0.0.1/",
        "http://[::1]/",
        "http://user:pw@example.com/",
        "ftp://example.com/",
        "file:///etc/passwd",
    ] {
        let error = call(&guarded, "web_fetch", json!({"url":target}))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Input(_)), "{target}: {error:?}");
    }
    assert!(
        requests.try_recv().is_err(),
        "blocked targets must never be contacted"
    );

    let open = web_tools(&dir, true, None);
    let fetched = call(&open, "web_fetch", json!({"url":url})).await.unwrap();
    assert_eq!(
        (
            fetched["status"].as_u64(),
            fetched["body"].as_str(),
            fetched["truncated"].as_bool()
        ),
        (Some(200), Some("ok"), Some(false))
    );
    assert!(requests.recv().await.unwrap().starts_with("GET / HTTP/1.1"));
}

#[tokio::test]
async fn web_fetch_truncates_and_reports_redirects() {
    let dir = tempfile::tempdir().unwrap();
    let tools = web_tools(&dir, true, None);
    let (big, _) = serve("HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nconnection: close\r\n\r\n0123456789012345678901234567890123456789").await;
    let body = call(&tools, "web_fetch", json!({"url":big})).await.unwrap();
    assert_eq!(
        (
            body["body"].as_str().unwrap().len(),
            body["truncated"].as_bool(),
            body["content_type"].as_str()
        ),
        (32, Some(true), Some("text/plain"))
    );
    let (moved, mut seen) = serve(
        "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:1/next\r\ncontent-length: 0\r\n\r\n",
    )
    .await;
    let redirect = call(&tools, "web_fetch", json!({"url":moved}))
        .await
        .unwrap();
    assert_eq!(
        (redirect["status"].as_u64(), redirect["location"].as_str()),
        (Some(302), Some("http://127.0.0.1:1/next"))
    );
    seen.recv().await.unwrap();

    let mut config = ToolsConfig::new(dir.path());
    config.web = Some(WebPolicy {
        allow_private_hosts: true,
        allowed_hosts: vec!["example.org".into()],
        timeout_ms: 1000,
        search: None,
    });
    let allowlisted = NativeTools::new(config).unwrap();
    assert!(
        call(
            &allowlisted,
            "web_fetch",
            json!({"url":"http://127.0.0.1:1/"})
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn web_search_normalizes_brave_and_forwards_custom_responses() {
    let dir = tempfile::tempdir().unwrap();
    let (brave, mut seen) = serve("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"web\":{\"results\":[{\"title\":\"T1\",\"url\":\"https://a\",\"description\":\"D1\"},{\"title\":\"T2\",\"url\":\"https://b\",\"description\":\"D2\"}]}}").await;
    let tools = web_tools(
        &dir,
        false,
        Some(SearchProvider::Brave {
            api_key: "k-123".into(),
            endpoint: Some(format!("{brave}/search")),
        }),
    );
    let found = call(
        &tools,
        "web_search",
        json!({"query":"rust & sdk","limit":1}),
    )
    .await
    .unwrap();
    assert_eq!(
        found,
        json!({"results":[{"title":"T1","url":"https://a","snippet":"D1"}]})
    );
    let request = seen.recv().await.unwrap().to_lowercase();
    assert!(
        request.starts_with("get /search?q=rust+%26+sdk&count=1 "),
        "{request}"
    );
    assert!(request.contains("x-subscription-token: k-123"));

    let (custom, mut seen) =
        serve("HTTP/1.1 200 OK\r\nconnection: close\r\n\r\n{\"hits\":[1,2]}").await;
    let tools = web_tools(
        &dir,
        false,
        Some(SearchProvider::Custom {
            endpoint: format!("{custom}/q"),
            query_param: "term".into(),
            count_param: Some("n".into()),
            headers: BTreeMap::from([("authorization".into(), "Bearer t".into())]),
        }),
    );
    let raw = call(&tools, "web_search", json!({"query":"x"}))
        .await
        .unwrap();
    assert_eq!(raw, json!({"response":{"hits":[1,2]}}));
    let request = seen.recv().await.unwrap().to_lowercase();
    assert!(
        request.starts_with("get /q?term=x&n=5 ") && request.contains("authorization: bearer t"),
        "{request}"
    );

    let (broken, _) =
        serve("HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\n\r\n").await;
    let tools = web_tools(
        &dir,
        false,
        Some(SearchProvider::Custom {
            endpoint: broken,
            query_param: "q".into(),
            count_param: None,
            headers: BTreeMap::new(),
        }),
    );
    assert!(
        call(&tools, "web_search", json!({"query":"x"}))
            .await
            .unwrap_err()
            .to_string()
            .contains("HTTP 500")
    );
}

#[tokio::test]
async fn schema_violations_and_dispatch_are_validated() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools_in(&dir);
    assert!(call(&tools, "fs_read", json!({})).await.is_err());
    assert!(
        call(&tools, "fs_read", json!({"path":"x","extra":1}))
            .await
            .is_err()
    );
    assert!(call(&tools, "nope", json!({})).await.is_err());
    let cancel = CancellationToken::new();
    let defs = tools
        .dispatch(json!({"op":"tool_definitions"}), &cancel)
        .await
        .unwrap();
    assert_eq!(defs.as_array().unwrap().len(), 6);
    tools.dispatch(json!({"op":"execute_tool","name":"fs_write","arguments":{"path":"f.txt","content":"hi"}}), &cancel).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        "hi"
    );
}

#[tokio::test]
async fn long_lines_scan_offsets_and_edit_expansion_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("huge.txt"), "x".repeat(1024 * 1024)).unwrap();
    let mut config = ToolsConfig::new(dir.path());
    config.max_read_bytes = 32;
    config.max_write_bytes = 32;
    let tools = NativeTools::new(config).unwrap();
    for offset in [0, 1, 100] {
        let read = call(
            &tools,
            "fs_read",
            json!({"path":"huge.txt","offset":offset}),
        )
        .await
        .unwrap();
        assert_eq!(read["content"], "");
        assert_eq!(read["truncated"], true);
    }
    std::fs::write(dir.path().join("short.txt"), "a".repeat(32)).unwrap();
    assert!(
        call(
            &tools,
            "fs_edit",
            json!({"path":"short.txt","old":"a","new":"z".repeat(1024),"replace_all":true})
        )
        .await
        .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("short.txt")).unwrap(),
        "a".repeat(32)
    );
    assert!(
        call(
            &tools,
            "fs_read",
            json!({"path":"short.txt","offset":u64::MAX})
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn repetitive_globs_and_pre_cancelled_writes_do_not_escape_limits() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a".repeat(80)), "fixture").unwrap();
    let tools = tools_in(&dir);
    let started = std::time::Instant::now();
    let found = call(
        &tools,
        "fs_glob",
        json!({"pattern":format!("{}z", "*a".repeat(120))}),
    )
    .await
    .unwrap();
    assert_eq!(found["matches"], json!([]));
    assert!(started.elapsed() < Duration::from_secs(2));
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        tools
            .execute_tool(
                "fs_write",
                json!({"path":"never.txt","content":"no"}),
                &cancel
            )
            .await,
        Err(Error::Cancelled)
    ));
    assert!(!dir.path().join("never.txt").exists());
}

#[tokio::test]
async fn web_body_reads_obey_cancellation_and_total_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (ready, mut headers) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let ready = ready.clone();
            tokio::spawn(async move {
                let mut request = [0; 4096];
                if socket.read(&mut request).await.unwrap_or(0) == 0 {
                    return;
                }
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nx")
                    .await
                    .unwrap();
                let _ = ready.send(());
                tokio::time::sleep(Duration::from_secs(2)).await;
            });
        }
    });
    let mut config = ToolsConfig::new(dir.path());
    config.web = Some(WebPolicy {
        allow_private_hosts: true,
        allowed_hosts: vec![],
        timeout_ms: 100,
        search: None,
    });
    let tools = NativeTools::new(config).unwrap();
    assert!(matches!(
        call(&tools, "web_fetch", json!({"url":url})).await,
        Err(Error::Timeout)
    ));
    headers.recv().await.unwrap();
    let cancel = CancellationToken::new();
    let task = tools.execute_tool("web_fetch", json!({"url":url}), &cancel);
    let stop = async {
        headers.recv().await.unwrap();
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(task, stop);
    assert!(matches!(result, Err(Error::Cancelled)));
    server.abort();
}
