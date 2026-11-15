use hive_agent_core::{Error, mcp::McpClient};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/mcp_stdio.py")
}

#[tokio::test]
async fn resources_and_prompts_work_without_a_tools_capability() {
    let dir = tempfile::tempdir().unwrap();
    let client = connect("catalog", &dir.path().join("calls"), json!({}))
        .await
        .unwrap();
    assert!(client.tools().is_empty());
    let resources = client
        .start_catalog_request("resources/list", json!({}))
        .unwrap()
        .result()
        .await
        .unwrap();
    assert_eq!(resources["resources"][0]["uri"], "test://guide");
    assert_eq!(resources["nextCursor"], "second");
    let page = client
        .start_catalog_request("resources/list", json!({"cursor":"second"}))
        .unwrap()
        .result()
        .await
        .unwrap();
    assert_eq!(page["resources"], json!([]));
    let resource = client
        .start_catalog_request("resources/read", json!({"uri":"test://guide"}))
        .unwrap()
        .result()
        .await
        .unwrap();
    assert_eq!(resource["contents"][0]["text"], "Use the SDK");
    let prompts = client
        .start_catalog_request("prompts/list", json!({}))
        .unwrap()
        .result()
        .await
        .unwrap();
    assert_eq!(prompts["prompts"][0]["name"], "review");
    let prompt = client
        .start_catalog_request(
            "prompts/get",
            json!({"name":"review","arguments":{"topic":"Rust"}}),
        )
        .unwrap()
        .result()
        .await
        .unwrap();
    assert_eq!(prompt["messages"][0]["content"]["text"], "Review Rust");
    assert!(
        client
            .start_catalog_request("tools/call", json!({}))
            .is_err()
    );
    assert!(
        client
            .start_catalog_request(
                "prompts/get",
                json!({"name":"review","arguments":{"topic":42}})
            )
            .is_err()
    );
    client.close().await;
}

#[tokio::test]
async fn unsupported_capabilities_and_malformed_resource_results_do_not_replay() {
    let dir = tempfile::tempdir().unwrap();
    let client = connect("normal", &dir.path().join("calls"), json!({}))
        .await
        .unwrap();
    assert!(
        client
            .start_catalog_request("resources/list", json!({}))
            .is_err()
    );
    assert!(!client.is_closed());
    client.close().await;
    let client = connect("bad_resource", &dir.path().join("calls"), json!({}))
        .await
        .unwrap();
    let result = client
        .start_catalog_request("resources/read", json!({"uri":"test://guide"}))
        .unwrap()
        .result()
        .await;
    assert!(matches!(result, Err(Error::McpOperationUncertain)));
    assert!(client.is_closed());
    assert!(
        client
            .start_catalog_request("resources/read", json!({"uri":"test://guide"}))
            .is_err()
    );
    client.close().await;
}
async fn connect(
    mode: &str,
    receipt: &Path,
    extra: Value,
) -> std::result::Result<std::sync::Arc<McpClient>, Error> {
    let mut config = json!({"command":"python3","args":[fixture(),mode,receipt],"timeout_ms":1500});
    for (k, v) in extra.as_object().unwrap() {
        config[k] = v.clone();
    }
    McpClient::connect_json(&config.to_string()).await
}
fn rows(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
async fn wait_receipt(path: &Path) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while rows(path).is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn stdio_negotiation_pagination_schemas_context_and_structured_result() {
    let dir = tempfile::tempdir().unwrap();
    let receipt = dir.path().join("calls");
    let client = connect("pages", &receipt, json!({})).await.unwrap();
    assert_eq!(
        client
            .tools()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["add", "echo"]
    );
    assert_eq!(client.info()["protocolVersion"], "2025-11-25");
    let result = client
        .start_call(
            "add".into(),
            json!({"a":2,"b":3}),
            json!({"execution_id":"receipt-key"}),
        )
        .unwrap()
        .result()
        .await
        .unwrap();
    assert_eq!(result["structuredContent"]["answer"], 5);
    assert_eq!(rows(&receipt).len(), 1);
    assert_eq!(
        rows(&receipt)[0]["meta"]["hive/context"]["execution_id"],
        "receipt-key"
    );
    client.close().await;
    assert!(client.is_closed());
    client.close().await;
}

#[tokio::test]
async fn invalid_arguments_unknown_tools_and_oversized_requests_never_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let receipt = dir.path().join("calls");
    let client = connect("normal", &receipt, json!({"max_message_bytes":2048}))
        .await
        .unwrap();
    assert!(
        client
            .start_call("add".into(), json!({"a":"2","b":3}), json!({}))
            .is_err()
    );
    assert!(
        client
            .start_call("missing".into(), json!({}), json!({}))
            .is_err()
    );
    assert!(
        client
            .start_call("echo".into(), json!({"value":"x".repeat(3000)}), json!({}))
            .is_err()
    );
    assert!(rows(&receipt).is_empty());
    let result = client
        .start_call("add".into(), json!({"a":2,"b":3}), json!({}))
        .unwrap()
        .result()
        .await
        .unwrap();
    assert_eq!(result["structuredContent"]["answer"], 5);
    client.close().await;
}

#[tokio::test]
async fn stdout_loss_timeout_and_malformed_responses_remain_uncertain_without_replay() {
    for mode in [
        "exit",
        "wait",
        "invalid_json",
        "wrong_id",
        "bad_result",
        "flood",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let receipt = dir.path().join("calls");
        let client = connect(mode, &receipt, json!({"max_messages":4}))
            .await
            .unwrap();
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
        assert_eq!(rows(&receipt).len(), 1);
        assert!(
            client
                .start_call("add".into(), json!({"a":2,"b":3}), json!({}))
                .is_err()
        );
        client.close().await;
    }
}

#[tokio::test]
async fn confirmed_tool_and_output_schema_errors_are_known_results() {
    for (mode, code) in [
        ("tool_error", "MCP_TOOL_ERROR"),
        ("bad_output", "INVALID_MCP_OUTPUT"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let receipt = dir.path().join("calls");
        let client = connect(mode, &receipt, json!({})).await.unwrap();
        let result = client
            .start_call("add".into(), json!({"a":2,"b":3}), json!({}))
            .unwrap()
            .result()
            .await
            .unwrap();
        assert_eq!(result["error"]["code"], code);
        assert!(!client.is_closed());
        assert_eq!(rows(&receipt).len(), 1);
        client.close().await;
    }
}

#[tokio::test]
async fn rpc_errors_are_confirmed_and_client_never_retries_on_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let receipt = dir.path().join("calls");
    let client = connect("remote_error", &receipt, json!({})).await.unwrap();
    for expected in 1..=2 {
        let result = client
            .start_call("add".into(), json!({"a":2,"b":3}), json!({}))
            .unwrap()
            .result()
            .await;
        assert!(matches!(result, Err(Error::McpRemote(_))));
        assert!(!client.is_closed());
        assert_eq!(rows(&receipt).len(), expected);
    }
    client.close().await;
}

#[tokio::test]
async fn setup_rejects_unsupported_versions_invalid_schemas_catalogs_and_cursor_cycles() {
    for mode in [
        "wrong_version",
        "no_tools",
        "bad_schema",
        "external_schema",
        "duplicate",
        "cycle",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let receipt = dir.path().join("calls");
        assert!(connect(mode, &receipt, json!({})).await.is_err(), "{mode}");
        assert!(rows(&receipt).is_empty());
    }
    let dir = tempfile::tempdir().unwrap();
    assert!(
        connect("pages", &dir.path().join("calls"), json!({"max_pages":1}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn server_requests_are_answered_without_enabling_sampling_and_stderr_is_discarded() {
    for mode in ["server_requests", "noisy_stderr"] {
        let dir = tempfile::tempdir().unwrap();
        let client = connect(mode, &dir.path().join("calls"), json!({}))
            .await
            .unwrap();
        let result = client
            .start_call("add".into(), json!({"a":2,"b":3}), json!({}))
            .unwrap()
            .result()
            .await
            .unwrap();
        assert_eq!(result["structuredContent"]["answer"], 5);
        client.close().await;
    }
}

#[tokio::test]
async fn cancellation_and_busy_checks_do_not_replay_and_direct_child_is_reaped() {
    let dir = tempfile::tempdir().unwrap();
    let receipt = dir.path().join("calls");
    let client = connect("wait", &receipt, json!({"timeout_ms":5000}))
        .await
        .unwrap();
    let pending = client
        .start_call("add".into(), json!({"a":2,"b":3}), json!({}))
        .unwrap();
    wait_receipt(&receipt).await;
    assert!(
        client
            .start_call("add".into(), json!({"a":2,"b":3}), json!({}))
            .is_err()
    );
    pending.cancel();
    assert!(matches!(
        pending.result().await,
        Err(Error::McpOperationUncertain)
    ));
    assert_eq!(rows(&receipt).len(), 1);
    client.close().await;
    #[cfg(target_os = "linux")]
    assert!(!Path::new(&format!("/proc/{}", rows(&receipt)[0]["pid"])).exists());
}

#[tokio::test]
async fn subprocess_environment_is_limited_and_explicit_overrides_are_supported() {
    let dir = tempfile::tempdir().unwrap();
    let client = connect(
        "env",
        &dir.path().join("calls"),
        json!({"env":{"VISIBLE":"yes"},"protocol_version":"2025-06-18"}),
    )
    .await
    .unwrap();
    let result = client
        .start_call("add".into(), json!({"a":2,"b":3}), json!({}))
        .unwrap()
        .result()
        .await
        .unwrap();
    let allowed = [
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
        "VISIBLE",
        "LC_CTYPE",
    ];
    for key in result["structuredContent"]["env_keys"].as_array().unwrap() {
        assert!(allowed.contains(&key.as_str().unwrap()));
    }
    client.close().await;
}
