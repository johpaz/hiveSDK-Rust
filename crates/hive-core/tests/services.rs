#![cfg(feature = "services")]
use hive_agent_core::{services::EntityKind as K, sessions::SessionStore};
use serde_json::json;

#[test]
fn native_memory_tools_validate_arguments_upsert_and_recreate_after_delete() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let service = store.services();
    let args = json!({"title":"Entrega","content":"Viernes"});
    let first = service.execute_tool("memory_write", args.clone()).unwrap();
    let second = service.execute_tool("memory_write", args.clone()).unwrap();
    assert!(second["revision"].as_u64().unwrap() > first["revision"].as_u64().unwrap());
    assert_eq!(
        service
            .execute_tool("memory_read", json!({"title":"Entrega"}))
            .unwrap()["data"]["content"],
        "Viernes"
    );
    assert!(
        service
            .execute_tool("memory_write", json!({"title":"Bad","content":42}))
            .is_err()
    );
    assert!(service.get(K::Memory, "Bad").unwrap().is_none());
    assert_eq!(
        service
            .execute_tool("memory_delete", json!({"title":"Entrega"}))
            .unwrap()["deleted"],
        true
    );
    assert_eq!(
        service
            .execute_tool("memory_delete", json!({"title":"Entrega"}))
            .unwrap()["deleted"],
        false
    );
    let recreated = service.execute_tool("memory_write", args).unwrap();
    assert!(recreated["revision"].as_u64().unwrap() > second["revision"].as_u64().unwrap());
}

#[test]
fn tenant_scopes_and_delimiter_identifiers_do_not_collide() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "a\",\"memory".into()).unwrap();
    let a = store.services();
    let b = store.for_tenant("b".into()).unwrap().services();
    a.create(
        K::Memory,
        "same",
        json!({"title":"Private A","content":"alpha"}),
    )
    .unwrap();
    assert!(b.get(K::Memory, "same").unwrap().is_none());
    b.create(
        K::Memory,
        "same",
        json!({"title":"Private B","content":"beta"}),
    )
    .unwrap();
    assert_eq!(
        a.get(K::Memory, "same").unwrap().unwrap().data["content"],
        "alpha"
    );
    assert_eq!(b.list(K::Memory, 10, None).unwrap().items.len(), 1);
    assert!(b.search(K::Memory, "alpha", 10).unwrap().is_empty());
    assert!(a.get(K::Skills, "same").unwrap().is_none());
}

#[test]
fn only_one_concurrent_writer_wins_and_tombstones_preserve_versions() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let services = store.services();
    let row = services
        .create(
            K::Memory,
            "m",
            json!({"title":"Memory","content":"original"}),
        )
        .unwrap();
    let handles: Vec<_> = ["one", "two"]
        .into_iter()
        .map(|content| {
            let service = services.clone();
            std::thread::spawn(move || {
                service.update(K::Memory, "m", row.revision, json!({"content":content}))
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    let current = services.get(K::Memory, "m").unwrap().unwrap();
    assert!(services.delete(K::Memory, "m", 1).is_err());
    services.delete(K::Memory, "m", current.revision).unwrap();
    assert!(services.get(K::Memory, "m").unwrap().is_none());
    drop(services);
    drop(store);
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    assert!(store.services().get(K::Memory, "m").unwrap().is_none());
    assert!(
        store
            .services()
            .list(K::Memory, 10, None)
            .unwrap()
            .items
            .is_empty()
    );
    let recreated = store
        .services()
        .create(K::Memory, "m", json!({"title":"Again","content":"x"}))
        .unwrap();
    assert!(recreated.revision > current.revision);
    assert!(
        store
            .services()
            .update(K::Memory, "m", current.revision, json!({"content":"stale"}))
            .is_err()
    );
}

#[test]
fn catalog_relations_are_validated_in_the_same_tenant() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let s = store.services();
    s.create(K::Providers, "p", json!({"name":"Provider","protocol":"openai_compatible","base_url":"http://localhost:1234/v1"})).unwrap();
    s.create(
        K::Models,
        "m",
        json!({"name":"Model","wire_id":"model","provider_id":"p"}),
    )
    .unwrap();
    assert!(
        s.create(K::Agents, "bad", json!({"name":"Agent","provider_id":"p"}))
            .is_err()
    );
    assert!(
        s.create(
            K::Skills,
            "bad",
            json!({"name":"Skill","description":"s","content":"Use tool","tools":["missing"]})
        )
        .is_err()
    );
    s.create(
        K::Tools,
        "sum",
        json!({"name":"sum","description":"Add numbers","parameters":{"type":"object"}}),
    )
    .unwrap();
    s.create(
        K::Skills,
        "math",
        json!({"name":"Math","description":"Sum numbers","content":"Use sum","tools":["sum"]}),
    )
    .unwrap();
    s.create(
        K::Agents,
        "a",
        json!({"name":"Agent","provider_id":"p","model_id":"m","tools":["sum"],"skills":["math"]}),
    )
    .unwrap();
    assert!(
        store
            .for_tenant("other".into())
            .unwrap()
            .services()
            .create(
                K::Agents,
                "a",
                json!({"name":"Agent","provider_id":"p","model_id":"m"})
            )
            .is_err()
    );
}

#[test]
fn rejects_unknown_fields_credentials_projects_and_invalid_protocols() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let s = store.services();
    for extra in [
        json!({"api_key":"secret"}),
        json!({"project_id":"p"}),
        json!({"user_id":"other"}),
    ] {
        let mut data = json!({"name":"Agent"});
        data.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(s.create(K::Agents, "bad", data).is_err());
    }
    assert!(s.dispatch(json!({"op":"list","kind":"projects"})).is_err());
    assert!(
        s.create(
            K::McpServers,
            "bad",
            json!({"name":"Server","transport":"http"})
        )
        .is_err()
    );
    assert!(
        s.create(
            K::Providers,
            "bad",
            json!({"name":"p","protocol":"openai_compatible","base_url":"https://host/?key=secret"})
        )
        .is_err()
    );
    assert!(
        s.create(
            K::Tools,
            "bad",
            json!({"name":"bad","description":"x","parameters":{"type":"bogus"}})
        )
        .is_err()
    );
    assert!(
        s.create(
            K::Memory,
            "large",
            json!({"title":"large","content":"a".repeat(262145)})
        )
        .is_err()
    );
}

#[test]
fn pagination_crosses_scan_batches_tombstones_and_scopes() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let s = store.services();
    for i in 0..205 {
        let id = format!("{i:03}");
        let row = s
            .create(K::Memory, &id, json!({"title":id,"content":"value"}))
            .unwrap();
        if i % 2 == 0 {
            s.delete(K::Memory, &id, row.revision).unwrap();
        }
    }
    let mut ids = vec![];
    let mut cursor = None;
    loop {
        let page = s.list(K::Memory, 7, cursor.as_deref()).unwrap();
        ids.extend(page.items.into_iter().map(|row| row.id));
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(ids.len(), 102);
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(ids, unique);
    let page = s.list(K::Memory, 2, None).unwrap();
    assert!(
        store
            .for_tenant("other".into())
            .unwrap()
            .services()
            .list(K::Memory, 2, page.next_cursor.as_deref())
            .is_err()
    );
    assert!(s.list(K::Skills, 2, page.next_cursor.as_deref()).is_err());
}

#[test]
fn bm25_search_is_ranked_and_disabled_records_are_excluded() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path(), "t".into()).unwrap();
    let s = store.services();
    s.create(K::Skills, "mail", json!({"name":"Correo","description":"Redactar correo","content":"Escribe un correo claro"})).unwrap();
    s.create(
        K::Skills,
        "math",
        json!({"name":"Math","description":"Sumar","content":"Suma números"}),
    )
    .unwrap();
    s.create(
        K::Skills,
        "disabled",
        json!({"name":"Correo","description":"Correo","content":"Correo","enabled":false}),
    )
    .unwrap();
    let results = s.search(K::Skills, "CORREO", 10).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].record.id, "mail");
    assert!(results[0].score > 0.0);
    assert!(s.search(K::Skills, "   ", 10).unwrap().is_empty());
    let query = (0..65)
        .map(|i| format!("word{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    assert!(s.search(K::Skills, &query, 10).is_err());
}
