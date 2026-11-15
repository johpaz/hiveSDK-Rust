//! Per-turn durable budgets for a long-lived, otherwise unbound conversation.
use hive_agent_core::{
    Error, durable_handle::DurableEngineHandle, objective::ObjectivePolicy, sessions::SessionStore,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
fn config() -> String {
    json!({"op":"create","model":"test","tools":[]}).to_string()
}
fn call(h: &DurableEngineHandle, value: Value) -> Value {
    serde_json::from_str(&h.dispatch(&value.to_string()).unwrap()).unwrap()
}
fn source(path: &std::path::Path) -> Arc<Mutex<Option<SessionStore>>> {
    Arc::new(Mutex::new(Some(
        SessionStore::open(path, "tenant".into()).unwrap(),
    )))
}
fn root(store: &Arc<Mutex<Option<SessionStore>>>, id: &str) {
    store
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .open_objective(
            id,
            ObjectivePolicy {
                max_model_calls: 1,
                max_output_tokens: 10,
                max_total_output_tokens: 10,
                ..Default::default()
            },
        )
        .unwrap();
}
fn snapshot(store: &Arc<Mutex<Option<SessionStore>>>, id: &str) -> Value {
    store
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .inspect_objective(id)
        .unwrap()
}
fn reply(h: &DurableEngineHandle, frame: &Value) {
    call(
        h,
        json!({"op":"advance","execution_id":frame["operation"]["execution_id"],
        "outcome":{"type":"model","reply":{"content":"receipt","usage":{"input_tokens":2,"output_tokens":3}}}}),
    );
}
#[test]
fn consecutive_turns_keep_history_and_separate_root_ledgers() {
    let dir = tempfile::tempdir().unwrap();
    let store = source(dir.path());
    let h =
        DurableEngineHandle::open(store.clone(), "a".into(), "s".into(), &config(), false).unwrap();
    for id in ["first", "second"] {
        root(&store, id);
        let frame = call(&h, json!({"op":"begin_run","prompt":id,"objective_id":id}));
        assert_eq!(frame["objective_id"], id);
        assert!(frame["state"]["objective_id"].is_null());
        let frame = call(&h, json!({"op":"claim"}));
        assert_eq!(frame["operation"]["objective_permit"], 1);
        assert_eq!(snapshot(&store, id)["reservations"]["1"], 10);
        reply(&h, &frame);
        call(&h, json!({"op":"commit"}));
        assert_eq!(snapshot(&store, id)["model_calls"], 1);
        assert_eq!(snapshot(&store, id)["output_tokens"], 3);
        assert_eq!(snapshot(&store, id)["reservations"], json!({}));
    }
    assert_eq!(
        call(&h, json!({"op":"messages"})).as_array().unwrap().len(),
        4
    );
    drop(h);
    let reopened =
        DurableEngineHandle::open(store, "a".into(), "s".into(), &config(), false).unwrap();
    assert_eq!(
        call(&reopened, json!({"op":"messages"}))
            .as_array()
            .unwrap()
            .len(),
        4
    );
}
#[test]
fn interrupted_turn_cannot_switch_roots_and_reconciliation_settles_original_permit() {
    let dir = tempfile::tempdir().unwrap();
    let store = source(dir.path());
    root(&store, "first");
    root(&store, "second");
    let h =
        DurableEngineHandle::open(store.clone(), "a".into(), "s".into(), &config(), false).unwrap();
    call(
        &h,
        json!({"op":"begin_run","prompt":"work","objective_id":"first"}),
    );
    let frame = call(&h, json!({"op":"claim"}));
    assert!(matches!(
        h.dispatch(
            &json!({"op":"begin_run","prompt":"replace","objective_id":"second"}).to_string()
        ),
        Err(Error::SessionInterrupted)
    ));
    drop(h);
    assert!(
        DurableEngineHandle::open(store.clone(), "a".into(), "s".into(), &config(), false).is_err()
    );
    assert!(
        DurableEngineHandle::open(store.clone(), "a".into(), "s".into(), &config(), true).is_err()
    );
    let source = store.lock().unwrap();
    let source = source.as_ref().unwrap();
    let inspected = source.inspect("a", "s", None).unwrap();
    assert_eq!(inspected["run"]["objective_id"], "first");
    source.dispatch(&json!({"op":"resolve","agent_id":"a","session_id":"s","revision":frame["revision"],
        "execution_id":frame["operation"]["execution_id"],"outcome":{"type":"model","reply":{"content":"known","usage":{"input_tokens":2,"output_tokens":3}}}}).to_string()).unwrap();
    assert_eq!(
        source.inspect_objective("first").unwrap()["output_tokens"],
        3
    );
    assert_eq!(
        source.inspect_objective("second").unwrap()["model_calls"],
        0
    );
}
#[test]
fn bound_sessions_reject_per_turn_rebinding_and_unknown_roots_do_not_claim() {
    let dir = tempfile::tempdir().unwrap();
    let store = source(dir.path());
    root(&store, "first");
    let bound = json!({"op":"create","model":"test","objective_id":"first"}).to_string();
    let h = DurableEngineHandle::open(store.clone(), "a".into(), "bound".into(), &bound, false)
        .unwrap();
    assert!(
        h.dispatch(
            &json!({"op":"begin_run","prompt":"replace","objective_id":"first"}).to_string()
        )
        .is_err()
    );
    let h = DurableEngineHandle::open(
        store.clone(),
        "a".into(),
        "unbound".into(),
        &config(),
        false,
    )
    .unwrap();
    assert!(
        h.dispatch(r#"{"op":"begin_run","prompt":"missing","objective_id":"missing"}"#)
            .is_err()
    );
    assert!(
        store
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .inspect("a", "unbound", None)
            .unwrap()
            .is_null()
    );
    assert_eq!(snapshot(&store, "first")["model_calls"], 0);
}
#[test]
fn stale_scope_claim_is_atomic_and_depth_and_cancel_still_apply() {
    let dir = tempfile::tempdir().unwrap();
    let store = source(dir.path());
    root(&store, "first");
    let a =
        DurableEngineHandle::open(store.clone(), "a".into(), "s".into(), &config(), false).unwrap();
    let b =
        DurableEngineHandle::open(store.clone(), "a".into(), "s".into(), &config(), false).unwrap();
    assert!(
        a.dispatch(
            r#"{"op":"begin_run","prompt":"too deep","objective_id":"first","objective_depth":100}"#
        )
        .is_err()
    );
    call(
        &a,
        json!({"op":"begin_run","prompt":"work","objective_id":"first","objective_depth":1}),
    );
    assert!(
        b.dispatch(r#"{"op":"begin_run","prompt":"stale","objective_id":"first"}"#)
            .is_err()
    );
    call(&a, json!({"op":"claim"}));
    assert!(a.dispatch(r#"{"op":"claim"}"#).is_err());
    assert_eq!(snapshot(&store, "first")["model_calls"], 1);
    store
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .objective_command("first", r#"{"op":"cancel"}"#)
        .unwrap();
    assert_eq!(snapshot(&store, "first")["stopped"], "cancelled");
    assert_eq!(snapshot(&store, "first")["reservations"]["1"], 10);
}
