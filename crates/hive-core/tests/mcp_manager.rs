use hive_agent_core::{Error, mcp::McpManager};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

fn fixture() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/mcp_stdio.py")
        .display()
        .to_string()
}
fn server(mode: &str, receipt: &Path) -> Value {
    json!({"command":"python3","args":[fixture(),mode,receipt],"timeout_ms":3000})
}
fn state(manager: &McpManager, name: &str) -> String {
    manager
        .status()
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .map_or("absent".into(), |row| {
            row["state"].as_str().unwrap().to_owned()
        })
}
fn names(value: &Value, key: &str) -> Vec<String> {
    let mut names: Vec<String> = value[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n.as_str().unwrap().to_owned())
        .collect();
    names.sort();
    names
}
async fn add(
    manager: &McpManager,
    server_name: &str,
    tool: &str,
    a: i64,
    b: i64,
) -> Result<Value, Error> {
    manager
        .start_call(server_name, tool, json!({"a":a,"b":b}), json!({}))
        .unwrap()
        .result()
        .await
}

#[tokio::test]
async fn a_set_of_servers_connects_in_parallel_and_exposes_qualified_tools() {
    let dir = tempfile::tempdir().unwrap();
    let manager = McpManager::new();
    let report = manager.apply(json!({"math":server("normal", &dir.path().join("a")),"pages":server("pages", &dir.path().join("b"))}), 0).await.unwrap();
    assert_eq!(names(&report, "added"), ["math", "pages"]);
    assert_eq!(report["failed"], json!({}));
    assert_eq!(
        (state(&manager, "math"), state(&manager, "pages")),
        ("connected".into(), "connected".into())
    );
    let catalog = manager.catalog();
    let qualified: Vec<&str> = catalog
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(
        qualified.contains(&"math__add")
            && qualified.contains(&"math__echo")
            && qualified.contains(&"pages__add")
            && qualified.contains(&"pages__echo"),
        "{qualified:?}"
    );
    assert!(
        catalog[0]["inputSchema"].is_object()
            && catalog[0]["server"].is_string()
            && catalog[0]["tool"].is_string()
    );
    assert_eq!(
        add(&manager, "math", "add", 2, 3).await.unwrap()["structuredContent"]["answer"],
        5
    );
    assert_eq!(manager.status()[0]["server"]["name"], "hive-test-peer");
    // Calls validate before sending and refuse unknown servers.
    assert!(
        manager
            .start_call("math", "add", json!({"a":"x"}), json!({}))
            .is_err()
    );
    assert!(
        matches!(manager.start_call("ghost", "add", json!({}), json!({})), Err(Error::Mcp(m)) if m.contains("MCP_SERVER_UNAVAILABLE"))
    );
    manager.close_all().await;
    assert_eq!(manager.status(), json!([]));
}

#[tokio::test]
async fn reloading_keeps_what_is_unchanged_restarts_what_changed_and_closes_what_was_removed() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b, c) = (
        dir.path().join("a"),
        dir.path().join("b"),
        dir.path().join("c"),
    );
    let manager = McpManager::new();
    manager.apply(json!({"keep":server("normal", &a),"change":server("normal", &b),"gone":server("normal", &c)}), 0).await.unwrap();
    let pid_of = |path: &Path| -> u64 {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .last()
            .map(|l| {
                serde_json::from_str::<Value>(l).unwrap()["pid"]
                    .as_u64()
                    .unwrap()
            })
            .unwrap()
    };
    for server_name in ["keep", "change", "gone"] {
        add(&manager, server_name, "add", 1, 1).await.unwrap();
    }
    let (keep_pid, change_pid) = (pid_of(&a), pid_of(&b));
    let changed = server("pages", &b);
    let report = manager.apply(json!({"keep":server("normal", &a),"change":changed,"fresh":server("normal", &dir.path().join("d"))}), 0).await.unwrap();
    assert_eq!(
        (
            names(&report, "unchanged"),
            names(&report, "restarted"),
            names(&report, "added"),
            names(&report, "removed")
        ),
        (
            vec!["keep".to_owned()],
            vec!["change".to_owned()],
            vec!["fresh".to_owned()],
            vec!["gone".to_owned()]
        )
    );
    add(&manager, "keep", "add", 2, 2).await.unwrap();
    assert_eq!(
        pid_of(&a),
        keep_pid,
        "an unchanged server keeps its process"
    );
    add(&manager, "change", "add", 2, 2).await.unwrap();
    assert_ne!(
        pid_of(&b),
        change_pid,
        "a changed server runs in a new process"
    );
    assert_eq!(state(&manager, "gone"), "absent");
    assert!(
        manager
            .start_call("gone", "add", json!({"a":1,"b":1}), json!({}))
            .is_err()
    );
    // Applying the same set again changes nothing.
    let again = manager.apply(json!({"keep":server("normal", &a),"change":server("pages", &b),"fresh":server("normal", &dir.path().join("d"))}), 0).await.unwrap();
    assert_eq!(names(&again, "unchanged"), ["change", "fresh", "keep"]);
    assert_eq!(
        (names(&again, "restarted"), names(&again, "added")),
        (vec![] as Vec<String>, vec![] as Vec<String>)
    );
    manager.close_all().await;
}

#[tokio::test]
async fn an_invalid_set_changes_nothing_and_a_failing_server_does_not_affect_the_others() {
    let dir = tempfile::tempdir().unwrap();
    let manager = McpManager::new();
    manager
        .apply(json!({"good":server("normal", &dir.path().join("a"))}), 0)
        .await
        .unwrap();
    let before = manager.status();
    for bad in [
        json!({"bad name":server("normal", &dir.path().join("x"))}),
        json!({"a__b":server("normal", &dir.path().join("x"))}),
        json!({"":server("normal", &dir.path().join("x"))}),
        json!({"x":"not an object"}),
        json!({"x":{"command":"python3","enabled":"yes"}}),
        json!([1]),
    ] {
        assert!(manager.apply(bad.clone(), 0).await.is_err(), "{bad}");
        assert_eq!(
            manager.status(),
            before,
            "an invalid set must not touch the running one: {bad}"
        );
    }
    let too_many: serde_json::Map<String, Value> = (0..65)
        .map(|n| (format!("s{n}"), json!({"command":"x"})))
        .collect();
    assert!(manager.apply(Value::Object(too_many), 0).await.is_err());
    // One server that cannot start is reported; the rest still connect.
    let report = manager.apply(json!({"good":server("normal", &dir.path().join("a")),"missing":{"command":"/nonexistent/mcp-server"},"wrong":server("wrong_version", &dir.path().join("w"))}), 0).await.unwrap();
    assert_eq!(
        report["failed"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["missing", "wrong"]
    );
    assert_eq!(
        (
            state(&manager, "good"),
            state(&manager, "missing"),
            state(&manager, "wrong")
        ),
        ("connected".into(), "failed".into(), "failed".into())
    );
    assert!(
        manager
            .status()
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "missing" && row["error"].is_string())
    );
    let refused = manager
        .start_call("missing", "add", json!({}), json!({}))
        .err()
        .unwrap()
        .to_string();
    assert!(
        refused.contains("MCP_SERVER_UNAVAILABLE") && refused.contains("reconnect"),
        "{refused}"
    );
    assert_eq!(
        add(&manager, "good", "add", 1, 2).await.unwrap()["structuredContent"]["answer"],
        3
    );
    assert_eq!(
        manager
            .catalog()
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["server"] != "good")
            .count(),
        0,
        "only connected servers appear in the catalog"
    );
    // Fixing the config and applying again brings it up; nothing retried on its own meanwhile.
    let fixed = manager.apply(json!({"good":server("normal", &dir.path().join("a")),"missing":server("normal", &dir.path().join("m")),"wrong":server("wrong_version", &dir.path().join("w"))}), 0).await.unwrap();
    assert_eq!(
        names(&fixed, "restarted"),
        ["missing"],
        "it existed (failed), so bringing it up is a restart"
    );
    assert_eq!(
        fixed["failed"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["wrong"],
        "a server that still fails is retried by apply and reported again"
    );
    assert_eq!(state(&manager, "missing"), "connected");
    manager.close_all().await;
}

#[tokio::test]
async fn an_uncertain_call_closes_its_server_and_only_reconnect_or_apply_brings_it_back() {
    let dir = tempfile::tempdir().unwrap();
    let receipt = dir.path().join("calls");
    let manager = McpManager::new();
    manager.apply(json!({"flaky":server("exit", &receipt),"steady":server("normal", &dir.path().join("s"))}), 0).await.unwrap();
    let outcome = manager
        .start_call("flaky", "add", json!({"a":1,"b":1}), json!({}))
        .unwrap()
        .result()
        .await;
    assert!(
        matches!(outcome, Err(Error::McpOperationUncertain)),
        "{outcome:?}"
    );
    let calls = std::fs::read_to_string(&receipt).unwrap().lines().count();
    assert_eq!(state(&manager, "flaky"), "closed");
    assert!(
        manager
            .start_call("flaky", "add", json!({"a":1,"b":1}), json!({}))
            .err()
            .unwrap()
            .to_string()
            .contains("is closed; reconnect it")
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        std::fs::read_to_string(&receipt).unwrap().lines().count(),
        calls,
        "the failed call was never replayed"
    );
    assert_eq!(
        add(&manager, "steady", "add", 4, 4).await.unwrap()["structuredContent"]["answer"],
        8,
        "the other server is untouched"
    );
    // A new session for future calls, on request.
    assert_eq!(manager.reconnect("flaky").await.unwrap()["connected"], true);
    assert_eq!(state(&manager, "flaky"), "connected");
    assert!(manager.reconnect("ghost").await.is_err());
    // A closed server is also replaced by the next apply (its config is unchanged, but it is not alive).
    let _ = manager
        .start_call("flaky", "add", json!({"a":1,"b":1}), json!({}))
        .unwrap()
        .result()
        .await;
    assert_eq!(state(&manager, "flaky"), "closed");
    let report = manager.apply(json!({"flaky":server("exit", &receipt),"steady":server("normal", &dir.path().join("s"))}), 0).await.unwrap();
    assert_eq!(
        (names(&report, "restarted"), names(&report, "unchanged")),
        (vec!["flaky".to_owned()], vec!["steady".to_owned()])
    );
    manager.close_all().await;
}

#[tokio::test]
async fn disabled_servers_stay_in_the_set_unconnected_and_can_be_enabled_by_a_reload() {
    let dir = tempfile::tempdir().unwrap();
    let manager = McpManager::new();
    let mut config = server("normal", &dir.path().join("a"));
    config["enabled"] = json!(false);
    let report = manager
        .apply(json!({"off":config.clone()}), 0)
        .await
        .unwrap();
    assert_eq!(names(&report, "disabled"), ["off"]);
    assert_eq!(state(&manager, "off"), "disabled");
    assert!(
        manager
            .start_call("off", "add", json!({"a":1,"b":1}), json!({}))
            .err()
            .unwrap()
            .to_string()
            .contains("disabled")
    );
    assert!(manager.reconnect("off").await.is_err());
    assert!(manager.catalog().as_array().unwrap().is_empty());
    config["enabled"] = json!(true);
    let enabled = manager
        .apply(json!({"off":config.clone()}), 0)
        .await
        .unwrap();
    assert_eq!(names(&enabled, "added"), ["off"]);
    assert_eq!(state(&manager, "off"), "connected");
    config["enabled"] = json!(false);
    manager.apply(json!({"off":config}), 0).await.unwrap();
    assert_eq!(
        state(&manager, "off"),
        "disabled",
        "disabling closes a running server"
    );
    manager.close_all().await;
}

#[tokio::test]
async fn a_reload_can_let_a_running_call_finish_before_closing_its_server() {
    let dir = tempfile::tempdir().unwrap();
    for (drain_ms, finishes) in [(3000u64, true), (0, false)] {
        let manager = std::sync::Arc::new(McpManager::new());
        manager
            .apply(
                json!({"slow":server("slow", &dir.path().join(format!("c{drain_ms}")))}),
                0,
            )
            .await
            .unwrap();
        let request = manager
            .start_call("slow", "add", json!({"a":20,"b":22}), json!({}))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let remover = manager.clone();
        let reload = tokio::spawn(async move { remover.apply(json!({}), drain_ms).await });
        let outcome = request.result().await;
        reload.await.unwrap().unwrap();
        if finishes {
            assert_eq!(
                outcome.unwrap()["structuredContent"]["answer"],
                42,
                "drained: the call completed"
            );
        } else {
            assert!(
                matches!(outcome, Err(Error::McpOperationUncertain)),
                "no drain: the call was cut off and is uncertain"
            );
        }
        assert_eq!(manager.status(), json!([]));
    }
}
