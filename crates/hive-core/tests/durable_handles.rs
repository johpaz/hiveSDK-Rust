use hive_agent_core::{
    Error,
    durable_handle::DurableEngineHandle,
    sessions::{OperationResult, SessionStore},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
fn config() -> String {
    json!({"op":"create","model":"test","tools":[{"name":"read","description":"Read","parameters":{"type":"object"}}]}).to_string()
}
fn open(store: &Arc<Mutex<Option<SessionStore>>>, resume: bool) -> DurableEngineHandle {
    DurableEngineHandle::open(store.clone(), "a".into(), "s".into(), &config(), resume).unwrap()
}
fn call(handle: &DurableEngineHandle, input: Value) -> Value {
    serde_json::from_str(&handle.dispatch(&input.to_string()).unwrap()).unwrap()
}
fn advance(handle: &DurableEngineHandle, frame: &Value, reply: Value) -> Value {
    call(
        handle,
        json!({"op":"advance","execution_id":frame["operation"]["execution_id"],"outcome":{"type":"model","reply":reply}}),
    )
}
fn done() -> Value {
    json!({"content":"Known 🦀","usage":{"input_tokens":3,"output_tokens":2}})
}
fn shared(path: &std::path::Path) -> Arc<Mutex<Option<SessionStore>>> {
    Arc::new(Mutex::new(Some(
        SessionStore::open(path, "default".into()).unwrap(),
    )))
}
fn finish(handle: &DurableEngineHandle) {
    call(handle, json!({"op":"begin_run","prompt":"Hello"}));
    let frame = call(handle, json!({"op":"claim"}));
    advance(handle, &frame, done());
    call(handle, json!({"op":"commit"}));
}

#[test]
fn durable_commands_return_metadata_and_keep_history_in_rust() {
    let dir = tempfile::tempdir().unwrap();
    let store = shared(dir.path());
    let h = open(&store, false);
    for cmd in [
        json!({"op":"begin_run","prompt":"Hello"}),
        json!({"op":"claim"}),
    ] {
        let f = call(&h, cmd);
        assert_eq!(f["state"].as_object().unwrap().len(), 2);
        assert!(f["state"].get("messages").is_none());
    }
    let frame = call(&h, json!({"op":"frame"}));
    let f = advance(&h, &frame, done());
    assert_eq!(f["action"]["stats"]["model_calls"], 1);
    assert_eq!(call(&h, json!({"op":"messages"})), json!([]));
    call(&h, json!({"op":"commit"}));
    assert_eq!(call(&h, json!({"op":"messages"}))[1]["content"], "Known 🦀");
    assert!(
        h.dispatch(r#"{"op":"begin_run","prompt":"No","state":{}}"#)
            .is_err()
    );
    assert!(h.dispatch(r#"{"op":"claim","revision":999}"#).is_err());
}

#[test]
fn uncertain_tool_requires_resolution_and_reopens_without_replay() {
    let dir = tempfile::tempdir().unwrap();
    let store = shared(dir.path());
    let h = open(&store, false);
    call(&h, json!({"op":"begin_run","prompt":"Read"}));
    let f = call(&h, json!({"op":"claim"}));
    advance(
        &h,
        &f,
        json!({"tool_calls":[{"id":"c","name":"read","arguments":{}}],"usage":{"input_tokens":3,"output_tokens":2}}),
    );
    let tool = call(&h, json!({"op":"claim"}));
    drop(h);
    assert!(matches!(
        DurableEngineHandle::open(store.clone(), "a".into(), "s".into(), &config(), true),
        Err(Error::SessionOperationUncertain)
    ));
    store
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .advance(
            "a",
            "s",
            tool["revision"].as_u64().unwrap(),
            tool["operation"]["execution_id"].as_str().unwrap(),
            OperationResult::Tool { result: json!(5) },
        )
        .unwrap();
    let h = open(&store, true);
    let f = call(&h, json!({"op":"resume_run"}));
    assert_eq!(f["action"]["type"], "model");
    assert_eq!(
        call(&h, json!({"op":"messages"})).as_array().unwrap().len(),
        3
    );
    let f = call(&h, json!({"op":"claim"}));
    let f = advance(&h, &f, done());
    assert_eq!(f["action"]["stats"]["tool_calls"], 1);
    assert_eq!(f["action"]["stats"]["model_calls"], 2);
    call(&h, json!({"op":"commit"}));
    assert_eq!(
        call(&h, json!({"op":"messages"})).as_array().unwrap().len(),
        4
    );
}

#[test]
fn stale_handle_and_execution_ids_cannot_advance_a_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let store = shared(dir.path());
    let a = open(&store, false);
    let b = open(&store, false);
    call(&a, json!({"op":"begin_run","prompt":"Hello"}));
    assert!(a.dispatch(r#"{"op":"commit"}"#).is_err());
    let frame = call(&a, json!({"op":"claim"}));
    assert!(matches!(a.dispatch(&json!({"op":"advance","execution_id":"stale","outcome":{"type":"model","reply":done()}}).to_string()),Err(Error::SessionConflict)));
    advance(&a, &frame, done());
    call(&a, json!({"op":"commit"}));
    assert!(matches!(
        b.dispatch(r#"{"op":"begin_run","prompt":"Stale"}"#),
        Err(Error::SessionConflict)
    ));
    assert_eq!(call(&b, json!({"op":"messages"})), json!([]));
}

#[test]
fn closing_store_invalidates_transitions_but_preserves_visible_history() {
    let dir = tempfile::tempdir().unwrap();
    let store = shared(dir.path());
    let h = open(&store, false);
    finish(&h);
    store.lock().unwrap().take();
    assert_eq!(call(&h, json!({"op":"messages"}))[1]["content"], "Known 🦀");
    assert!(matches!(
        h.dispatch(r#"{"op":"begin_run","prompt":"Closed"}"#),
        Err(Error::StoreClosed)
    ));
}

#[test]
fn visible_getters_do_not_wait_for_storage_held_by_a_worker() {
    let dir = tempfile::tempdir().unwrap();
    let store = shared(dir.path());
    let h = Arc::new(open(&store, false));
    finish(&h);
    let guard = store.lock().unwrap();
    let worker = h.clone();
    let (started, ready) = std::sync::mpsc::channel();
    let task = std::thread::spawn(move || {
        started.send(()).unwrap();
        worker.dispatch(r#"{"op":"begin_run","prompt":"Second"}"#)
    });
    ready.recv().unwrap();
    let reader = h.clone();
    let (sent, received) = std::sync::mpsc::channel();
    let read =
        std::thread::spawn(move || sent.send(reader.dispatch(r#"{"op":"messages"}"#)).unwrap());
    let result = received.recv_timeout(std::time::Duration::from_secs(2));
    drop(guard);
    task.join().unwrap().unwrap();
    read.join().unwrap();
    let messages: Value =
        serde_json::from_str(&result.expect("getter waited for storage").unwrap()).unwrap();
    assert_eq!(messages.as_array().unwrap().len(), 2);
}

#[test]
fn context_plan_updates_use_cas_and_keep_the_visible_copy_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let store = shared(dir.path());
    let first = open(&store, false);
    finish(&first);
    let a = open(&store, false);
    let b = open(&store, false);
    let source = call(&a, json!({"op":"capture_context","start":0,"end":2}));
    let plan = json!({"summary":{"id":"s","source":source,"content":"Known"},"retrieved":[]});
    call(&a, json!({"op":"set_context_plan","plan":plan}));
    assert!(matches!(
        b.dispatch(&json!({"op":"set_context_plan","plan":null}).to_string()),
        Err(Error::SessionConflict)
    ));
    assert_eq!(call(&b, json!({"op":"context_plan"})), Value::Null);
    assert_eq!(
        call(&open(&store, false), json!({"op":"context_plan"})),
        plan
    );
}
