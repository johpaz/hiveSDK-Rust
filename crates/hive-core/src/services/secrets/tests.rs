use super::*;
use std::sync::{
    Barrier,
    atomic::{AtomicBool, Ordering},
};
fn key(byte: u8) -> SecretKey {
    SecretKey::new(&[byte; 32]).unwrap()
}
fn setup() -> (tempfile::TempDir, SessionStore, SecretVault) {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "tenant".into()).unwrap();
    let vault = store.open_secret_vault("main", "first", key(7)).unwrap();
    (dir, store, vault)
}
#[test]
fn values_are_ciphertext_at_rest_and_metadata_and_debug_are_redacted() {
    let (_dir, store, vault) = setup();
    let value = "private fixture token 🦀";
    let meta = vault.put("api", value, 0).unwrap();
    assert_eq!(meta.revision, 1);
    assert!(!serde_json::to_string(&meta).unwrap().contains(value));
    assert_eq!(vault.reveal("api").unwrap().unwrap().expose(), value);
    assert!(vault.reveal("missing").unwrap().is_none());
    let raw = store
        .db
        .col_get(RECORDS, &vault.record_key("api"))
        .unwrap()
        .unwrap();
    assert!(!raw.doc.to_string().contains(value));
    assert!(
        format!(
            "{vault:?} {:?} {:?}",
            key(7),
            vault.reveal("api").unwrap().unwrap()
        )
        .contains("REDACTED")
    );
    assert!(!format!("{:?}", vault.reveal("api").unwrap().unwrap()).contains(value));
    let original = raw.doc["sealed"]["nonce"].clone();
    vault.put("api", value, 1).unwrap();
    assert_ne!(
        original,
        store
            .db
            .col_get(RECORDS, &vault.record_key("api"))
            .unwrap()
            .unwrap()
            .doc["sealed"]["nonce"]
    );
    for invalid in [
        json!({"op":value}),
        json!({"op":"put","name":"api","value":value,"revision":false}),
    ] {
        assert!(
            !vault
                .dispatch(&invalid.to_string())
                .unwrap_err()
                .to_string()
                .contains(value)
        );
    }
}
#[test]
fn cas_tombstones_and_recreation_prevent_stale_overwrites() {
    let (_dir, _store, vault) = setup();
    vault.put("api", "one", 0).unwrap();
    assert!(
        vault
            .put("api", "stale", 0)
            .unwrap_err()
            .to_string()
            .contains("SECRET_CONFLICT")
    );
    let deleted = vault.delete("api", 1).unwrap();
    assert!(deleted.deleted);
    assert!(vault.reveal("api").unwrap().is_none());
    assert_eq!(vault.inspect("api").unwrap().unwrap().revision, 2);
    assert!(
        vault.list(50, None, false).unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        vault.list(50, None, true).unwrap()["items"][0]["deleted"],
        true
    );
    assert!(vault.put("api", "wrong", 0).is_err());
    assert_eq!(vault.put("api", "new", 2).unwrap().revision, 3);
    assert_eq!(vault.reveal("api").unwrap().unwrap().expose(), "new");
}
#[test]
fn namespaces_tenants_wrong_keys_and_cursors_fail_closed() {
    let (_dir, store, vault) = setup();
    vault.put("a", "first", 0).unwrap();
    vault.put("b", "second", 0).unwrap();
    let other = store
        .for_tenant("other".into())
        .unwrap()
        .open_secret_vault("main", "first", key(7))
        .unwrap();
    assert!(other.reveal("a").unwrap().is_none());
    assert!(store.open_secret_vault("main", "first", key(8)).is_err());
    assert!(
        store
            .open_secret_vault("main", "wrong-label", key(7))
            .is_err()
    );
    let second = store
        .open_secret_vault("other-vault", "first", key(7))
        .unwrap();
    assert!(second.reveal("a").unwrap().is_none());
    let first = vault.list(1, None, false).unwrap();
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        vault.list(1, first["next_cursor"].as_str(), false).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(other.list(1, first["next_cursor"].as_str(), false).is_err());
    other.put("target", "other", 0).unwrap();
    let copied = store
        .db
        .col_get(RECORDS, &vault.record_key("a"))
        .unwrap()
        .unwrap()
        .doc;
    store
        .db
        .col_put(
            RECORDS,
            &other.record_key("target"),
            &copied,
            Default::default(),
        )
        .unwrap();
    assert!(other.inspect("target").is_err());
    assert!(other.reveal("target").is_err());
}
#[test]
fn authenticated_headers_ciphertext_and_nonces_reject_tampering() {
    for field in ["deleted", "updated_at", "ciphertext", "nonce"] {
        let (_dir, store, vault) = setup();
        vault.put("api", "fixture", 0).unwrap();
        let mut record = vault.record("api").unwrap().unwrap();
        record.meta.revision = 2;
        record.sealed = seal_with(&vault.key, &record.aad().unwrap(), b"fixture").unwrap();
        match field {
            "deleted" => record.meta.deleted = true,
            "updated_at" => record.meta.updated_at += 1,
            "ciphertext" => {
                let first = if record.sealed.ciphertext.starts_with('0') {
                    "1"
                } else {
                    "0"
                };
                record.sealed.ciphertext.replace_range(0..1, first);
            }
            "nonce" => record.sealed.nonce = "00".repeat(12),
            _ => unreachable!(),
        }
        store
            .db
            .col_put(
                RECORDS,
                &vault.record_key("api"),
                &serde_json::to_value(record).unwrap(),
                Default::default(),
            )
            .unwrap();
        assert!(vault.inspect("api").is_err(), "{field}");
        assert!(vault.reveal("api").is_err(), "{field}");
        assert!(vault.put("api", "overwrite", 2).is_err(), "{field}");
    }
}
#[test]
fn concurrent_creates_and_independent_writes_use_anchor_cas() {
    let (_dir, store, vault) = setup();
    let writers = (0..8)
        .map(|_| {
            let store = store.clone();
            std::thread::spawn(move || {
                store
                    .open_secret_vault("main", "first", key(7))
                    .unwrap()
                    .put("shared", "one", 0)
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        writers
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(Result::is_ok)
            .count(),
        1
    );
    let writers = (0..8)
        .map(|n| {
            let store = store.clone();
            std::thread::spawn(move || {
                store
                    .open_secret_vault("main", "first", key(7))
                    .unwrap()
                    .put(&format!("own-{n}"), "value", 0)
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();
    for writer in writers {
        writer.join().unwrap();
    }
    assert_eq!(vault.info().unwrap()["retained_records"], 9);
    assert_eq!(vault.info().unwrap()["encryptions"], 19);
}
#[test]
fn atomic_rekey_preserves_values_and_tombstones_and_invalidates_old_handles() {
    let (_dir, store, mut vault) = setup();
    let old = store.open_secret_vault("main", "first", key(7)).unwrap();
    vault.put("api", "private", 0).unwrap();
    vault.put("deleted", "old", 0).unwrap();
    vault.delete("deleted", 1).unwrap();
    assert!(vault.rekey("same-material", key(7)).is_err());
    assert_eq!(vault.info().unwrap()["key_id"], "first");
    let info = vault.rekey("second", key(8)).unwrap();
    assert_eq!(info["encryptions"], 3);
    assert!(old.reveal("api").is_err());
    assert!(old.put("api", "old-writer", 1).is_err());
    assert!(store.open_secret_vault("main", "first", key(7)).is_err());
    assert!(store.open_secret_vault("main", "second", key(7)).is_err());
    let current = store.open_secret_vault("main", "second", key(8)).unwrap();
    assert_eq!(current.reveal("api").unwrap().unwrap().expose(), "private");
    assert_eq!(current.inspect("api").unwrap().unwrap().revision, 2);
    assert!(current.inspect("deleted").unwrap().unwrap().deleted);
    assert_eq!(current.inspect("deleted").unwrap().unwrap().revision, 3);
}
#[test]
fn concurrent_write_forces_full_rekey_retry_without_partial_rewrap() {
    let (_dir, store, mut vault) = setup();
    vault.put("original", "first", 0).unwrap();
    let entered = Arc::new(Barrier::new(2));
    let done = Arc::new(Barrier::new(2));
    let once = Arc::new(AtomicBool::new(false));
    let (a, b, c) = (entered.clone(), done.clone(), once.clone());
    vault.before_rotation_commit = Some(Arc::new(move || {
        if !c.swap(true, Ordering::SeqCst) {
            a.wait();
            b.wait();
        }
    }));
    let rotation = std::thread::spawn(move || vault.rekey("second", key(8)));
    entered.wait();
    let writer = store.open_secret_vault("main", "first", key(7)).unwrap();
    assert_eq!(
        writer.reveal("original").unwrap().unwrap().expose(),
        "first"
    );
    writer.put("late", "second", 0).unwrap();
    done.wait();
    rotation.join().unwrap().unwrap();
    let current = store.open_secret_vault("main", "second", key(8)).unwrap();
    assert_eq!(current.info().unwrap()["retained_records"], 2);
    assert_eq!(current.reveal("late").unwrap().unwrap().expose(), "second");
    assert!(writer.info().is_err());
}
#[test]
fn invalid_keys_bounds_and_closed_handles_do_not_return_values() {
    let (_dir, store, vault) = setup();
    assert!(SecretKey::new(&[7; 31]).is_err());
    assert!(
        vault
            .put("large", &"x".repeat(MAX_SECRET_BYTES + 1), 0)
            .is_err()
    );
    assert!(vault.inspect("\n").is_err());
    assert!(vault.list(201, None, false).is_err());
    let parent = Arc::new(std::sync::Mutex::new(Some(store)));
    let handle = SecretVaultHandle::open(parent.clone(), "main", "first", key(7)).unwrap();
    handle.close().unwrap();
    assert!(handle.dispatch(r#"{"op":"info"}"#.into()).is_err());
    let handle = SecretVaultHandle::open(parent.clone(), "main", "first", key(7)).unwrap();
    parent.lock().unwrap().take();
    assert!(handle.dispatch(r#"{"op":"info"}"#.into()).is_err());
}
#[test]
fn active_key_budget_requires_rekey_before_any_more_mutation() {
    let (_dir, store, mut vault) = setup();
    vault.put("api", "before", 0).unwrap();
    let mut anchor = vault.anchor().unwrap();
    anchor.encryptions = MAX_ENCRYPTIONS;
    anchor.revision += 1;
    anchor.sealed = seal_with(&vault.key, &anchor.aad().unwrap(), MAGIC).unwrap();
    store
        .db
        .col_put(
            ANCHORS,
            &vault.anchor_key(),
            &serde_json::to_value(anchor).unwrap(),
            Default::default(),
        )
        .unwrap();
    assert!(
        vault
            .put("api", "after", 1)
            .unwrap_err()
            .to_string()
            .contains("ROTATION_REQUIRED")
    );
    assert_eq!(vault.reveal("api").unwrap().unwrap().expose(), "before");
    vault.rekey("second", key(8)).unwrap();
    vault.put("api", "after", 2).unwrap();
    assert_eq!(vault.reveal("api").unwrap().unwrap().expose(), "after");
}
