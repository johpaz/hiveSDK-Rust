use hive_agent_core::sessions::SessionStore;
use hive_agent_core::{Action, Engine, Error, ModelReply};

fn initial() -> Engine {
    Engine::new("test".into(), Some("Be brief".into()), vec![], 8).unwrap()
}

fn finished(mut state: Engine) -> Engine {
    state.start("Hello".into()).unwrap();
    state
        .accept_reply(ModelReply {
            provider_state: None,
            content: Some("Hi".into()),
            tool_calls: vec![],
            usage: None,
        })
        .unwrap();
    state
}

#[test]
fn completed_history_survives_database_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let loaded = store.load("agent", "session", &initial()).unwrap();
    assert_eq!(loaded.revision, 0);
    let revision = store
        .begin("agent", "session", loaded.revision, loaded.state.clone())
        .unwrap();
    store
        .commit("agent", "session", revision, finished(loaded.state))
        .unwrap();
    drop(store);
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let loaded = store.load("agent", "session", &initial()).unwrap();
    assert_eq!(loaded.state.messages().len(), 3);
    assert!(matches!(loaded.state.action(), Action::Complete { content, .. } if content == "Hi"));
    let mut state = loaded.state;
    state.start("Follow up".into()).unwrap();
    assert_eq!(state.messages().len(), 4);
}

#[test]
fn interrupted_execution_blocks_replay_until_explicit_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    store.begin("agent", "session", 0, initial()).unwrap();
    drop(store);
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    assert!(matches!(
        store.load("agent", "session", &initial()),
        Err(Error::SessionInterrupted)
    ));
    store.recover("agent", "session").unwrap();
    let loaded = store.load("agent", "session", &initial()).unwrap();
    assert_eq!(loaded.state.messages().len(), 1);
    assert!(
        store
            .begin("agent", "session", loaded.revision, loaded.state)
            .is_ok()
    );
}

#[test]
fn stale_writer_cannot_claim_or_commit() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let token = store.begin("agent", "session", 0, initial()).unwrap();
    assert!(matches!(
        store.begin("agent", "session", 0, initial()),
        Err(Error::SessionConflict)
    ));
    store.recover("agent", "session").unwrap();
    assert!(matches!(
        store.commit("agent", "session", token, finished(initial())),
        Err(Error::SessionConflict)
    ));
}

#[test]
fn tenants_agents_and_sessions_are_separate_even_with_delimiters() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "a/b".into()).unwrap();
    let other = store.for_tenant("a".into()).unwrap();
    let token = store.begin("c", "d", 0, initial()).unwrap();
    store.commit("c", "d", token, finished(initial())).unwrap();
    assert_eq!(other.load("b/c", "d", &initial()).unwrap().revision, 0);
    assert_eq!(store.load("c/d", "other", &initial()).unwrap().revision, 0);
    assert_eq!(store.load("other", "d", &initial()).unwrap().revision, 0);
}

#[test]
fn configuration_change_requires_a_new_session() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let token = store.begin("agent", "session", 0, initial()).unwrap();
    store
        .commit("agent", "session", token, finished(initial()))
        .unwrap();
    let other = Engine::new("different-model".into(), Some("Be brief".into()), vec![], 8).unwrap();
    assert!(matches!(
        store.load("agent", "session", &other),
        Err(Error::SessionConfigMismatch)
    ));
    assert!(store.load("agent", "other-session", &other).is_ok());
}

#[test]
fn recovery_preserves_the_last_completed_conversation() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let token = store.begin("agent", "session", 0, initial()).unwrap();
    store
        .commit("agent", "session", token, finished(initial()))
        .unwrap();
    let loaded = store.load("agent", "session", &initial()).unwrap();
    store
        .begin("agent", "session", loaded.revision, loaded.state)
        .unwrap();
    store.recover("agent", "session").unwrap();
    assert_eq!(
        store
            .load("agent", "session", &initial())
            .unwrap()
            .state
            .messages()
            .len(),
        3
    );
    assert!(store.recover("agent", "session").is_err());
}

#[test]
fn empty_identifiers_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    assert!(SessionStore::open(dir.path(), " ".into()).is_err());
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    assert!(store.load("", "session", &initial()).is_err());
    assert!(store.load("agent", "", &initial()).is_err());
}

#[test]
fn policy_stop_survives_reopen_without_resetting_the_run() {
    use hive_agent_core::{LoopPolicy, Usage};
    let dir = tempfile::tempdir().unwrap();
    let initial = Engine::with_policy(
        "test".into(),
        None,
        vec![],
        8,
        LoopPolicy {
            max_total_tokens: Some(1),
            ..Default::default()
        },
    )
    .unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let revision = store.begin("agent", "session", 0, initial.clone()).unwrap();
    let mut state = initial.clone();
    state.start("Work".into()).unwrap();
    state
        .accept_reply(ModelReply {
            provider_state: None,
            content: Some("Answer".into()),
            tool_calls: vec![],
            usage: Some(Usage {
                input_tokens: 1,
                output_tokens: 1,
            }),
        })
        .unwrap();
    assert!(matches!(state.action(), Action::Stopped { .. }));
    store.commit("agent", "session", revision, state).unwrap();
    drop(store);
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let loaded = store.load("agent", "session", &initial).unwrap();
    assert!(matches!(loaded.state.action(), Action::Stopped { .. }));
    assert!(
        store
            .begin("agent", "session", loaded.revision, loaded.state)
            .is_err()
    );
    assert!(store.recover("agent", "session").is_err());
}
