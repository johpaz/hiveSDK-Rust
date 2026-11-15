use hive_agent_core::sessions::SessionStore;
use hive_agent_core::{Action, Engine, ModelReply, Tool, ToolCall};
use hivedb_core::{HiveDB, PutOptions};
use serde_json::json;

#[test]
fn pending_tool_from_an_older_checkpoint_is_revalidated_before_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let initial = Engine::new(
        "test".into(),
        None,
        vec![Tool {
            name: "save".into(),
            description: "Save".into(),
            parameters: json!({"type":"object",
            "properties":{"age":{"type":"integer"}},"required":["age"]}),
            allow_repeated_results: false,
        }],
        8,
    )
    .unwrap();
    let mut old = initial.clone();
    old.start("Save".into()).unwrap();
    old.accept_reply(ModelReply {
        provider_state: None,
        content: None,
        tool_calls: vec![ToolCall {
            id: "c".into(),
            name: "save".into(),
            arguments: json!({"age":20}),
        }],
        usage: None,
    })
    .unwrap();
    let mut old = serde_json::to_value(old).unwrap();
    // Simulate an older engine that scheduled invalid arguments without checking the schema.
    old["action"]["call"]["arguments"]["age"] = json!("20");
    old["pending"][0]["arguments"]["age"] = json!("20");
    old["messages"][1]["tool_calls"][0]["arguments"]["age"] = json!("20");
    let db = HiveDB::open(dir.path()).unwrap();
    let key = serde_json::to_string(&("t", "a", "s")).unwrap();
    db.col_put(
        "hive_sdk_rust_sessions_v1",
        &key,
        &json!({"schema_version":2,"tenant_id":"t",
            "agent_id":"a","session_id":"s","status":"running","state":initial,
            "run":{"run_id":"older-run","state":old,"in_flight":null,"tools":[],"abandoned":false}
        }),
        PutOptions::default(),
    )
    .unwrap();
    drop(db);
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let loaded = store.load_run("a", "s", &initial).unwrap();
    assert!(store.claim("a", "s", loaded.revision).is_err());
    let resumed = store.resume_run("a", "s", loaded.revision).unwrap();
    assert!(matches!(resumed.action, Action::ToolRejected { .. }));
    let next = store.reject("a", "s", resumed.revision).unwrap();
    assert!(matches!(next.action, Action::Model { .. }));
    let ledger = store.inspect("a", "s", None).unwrap();
    assert_eq!(ledger["run"]["tools"][0]["status"], "rejected");
    assert_eq!(ledger["run"]["state"]["stats"]["tool_calls"], 0);
}
