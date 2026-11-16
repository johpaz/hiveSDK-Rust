use hive_agent_core::Error;
use hive_agent_core::artifacts::{
    ArtifactStore, DEFAULT_RETENTION_MS, MAX_ARTIFACT_BYTES, NewArtifact,
};
use hive_agent_core::image;
use hive_agent_core::sessions::SessionStore;
use serde_json::{Value, json};

const DAY_2023_11_14: u64 = 1_700_000_000_000;

fn png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    bytes
}
fn jpeg(width: u16, height: u16) -> Vec<u8> {
    // SOI, an APP0 segment to skip, then SOF0.
    let mut bytes = vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x4a, 0x46];
    bytes.extend_from_slice(&[0xff, 0xc0, 0x00, 0x0b, 0x08]);
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
    bytes
}
fn gif(width: u16, height: u16) -> Vec<u8> {
    let mut bytes = b"GIF89a".to_vec();
    bytes.extend_from_slice(&width.to_le_bytes());
    bytes.extend_from_slice(&height.to_le_bytes());
    bytes
}
fn webp(chunk: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
    bytes.extend_from_slice(chunk);
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(body);
    bytes
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    store: SessionStore,
    artifacts: ArtifactStore,
}
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("files");
    let store = SessionStore::open(dir.path().join("db"), "tenant".into()).unwrap();
    let artifacts = store.artifacts(&root).unwrap();
    Fixture {
        _dir: dir,
        root,
        store,
        artifacts,
    }
}
fn new(kind: &str, mime: &str) -> NewArtifact {
    NewArtifact {
        kind: kind.into(),
        mime_type: mime.into(),
        ..Default::default()
    }
}
fn state(error: Error) -> String {
    match error {
        Error::State(message) | Error::Input(message) => message,
        other => other.to_string(),
    }
}

#[test]
fn image_headers_give_format_and_size_without_decoding_pixels() {
    let size =
        |bytes: &[u8]| image::info(bytes).map(|info| (info.mime_type, info.width, info.height));
    assert_eq!(size(&png(640, 480)), Some(("image/png", 640, 480)));
    assert_eq!(size(&jpeg(1920, 1080)), Some(("image/jpeg", 1920, 1080)));
    assert_eq!(size(&gif(32, 16)), Some(("image/gif", 32, 16)));
    // Lossy, lossless and extended WebP.
    let mut lossy = vec![0u8; 3];
    lossy.extend_from_slice(&[0x9d, 0x01, 0x2a]);
    lossy.extend_from_slice(&300u16.to_le_bytes());
    lossy.extend_from_slice(&200u16.to_le_bytes());
    assert_eq!(size(&webp(b"VP8 ", &lossy)), Some(("image/webp", 300, 200)));
    let packed: u32 = (99) | (49 << 14);
    let mut lossless = vec![0x2f];
    lossless.extend_from_slice(&packed.to_le_bytes());
    assert_eq!(
        size(&webp(b"VP8L", &lossless)),
        Some(("image/webp", 100, 50))
    );
    let mut extended = vec![0u8; 4];
    extended.extend_from_slice(&[0x7f, 0x02, 0x00, 0xdf, 0x01, 0x00]);
    assert_eq!(
        size(&webp(b"VP8X", &extended)),
        Some(("image/webp", 640, 480))
    );

    assert_eq!(size(&png(0, 10)), None, "zero dimensions are malformed");
    assert_eq!(size(&png(10, 0)), None);
    assert_eq!(size(&png(1, 1)[..20]), None, "truncated header");
    assert_eq!(size(b"not an image at all"), None);
    assert_eq!(size(&[]), None);
    assert_eq!(
        size(&[0xff, 0xd8, 0xff, 0xda, 0, 2]),
        None,
        "scan marker before any frame header"
    );
    let mut endless = vec![0xff, 0xd8];
    endless.extend(std::iter::repeat_n([0xff, 0xe0, 0x00, 0x02], 300).flatten());
    assert_eq!(size(&endless), None, "marker walk is bounded");
    assert_eq!(image::detect_mime(b"%PDF-1.7 ..."), Some("application/pdf"));
    assert_eq!(image::detect_mime(b"plain"), None);
}

#[test]
fn artifacts_roundtrip_with_detected_type_hash_and_a_generated_path() {
    let fx = fixture();
    let bytes = png(640, 480);
    let artifact = fx
        .artifacts
        .create_at(
            &bytes,
            NewArtifact {
                run_id: Some("run-1".into()),
                task_id: Some("t1".into()),
                ..new("screenshot", "application/octet-stream")
            },
            DAY_2023_11_14,
        )
        .unwrap();
    assert_eq!(
        (artifact.mime_type.as_str(), artifact.width, artifact.height),
        ("image/png", Some(640), Some(480)),
        "magic bytes beat the declared type"
    );
    assert_eq!(
        (artifact.size, artifact.status.as_str(), artifact.created_at),
        (bytes.len() as u64, "active", DAY_2023_11_14)
    );
    assert_eq!(
        artifact.expires_at,
        Some(DAY_2023_11_14 + DEFAULT_RETENTION_MS)
    );
    assert_eq!(artifact.sha256.len(), 64);
    assert_eq!(fx.artifacts.get(&artifact.id).unwrap().unwrap(), artifact);
    assert_eq!(fx.artifacts.read_bytes(&artifact.id).unwrap(), bytes);

    let file = fx
        .root
        .join("artifacts/2023-11-14")
        .join(format!("{}.png", artifact.id));
    assert_eq!(
        std::fs::read(&file).unwrap(),
        bytes,
        "stored under <root>/artifacts/<UTC day>/<id>.<ext>"
    );
    assert!(uuid_like(&artifact.id));
    let again = fx
        .artifacts
        .create_at(&bytes, new("screenshot", "image/png"), DAY_2023_11_14)
        .unwrap();
    assert_ne!(again.id, artifact.id, "no content deduplication");
    assert_eq!(again.sha256, artifact.sha256);

    // Reopening through a new store on the same root and database sees the same record.
    let reopened = fx.store.artifacts(&fx.root).unwrap();
    assert_eq!(reopened.get(&artifact.id).unwrap().unwrap(), artifact);
    assert_eq!(
        fx.artifacts
            .get("00000000-0000-4000-8000-000000000000")
            .unwrap(),
        None
    );
}
fn uuid_like(id: &str) -> bool {
    id.len() == 36 && id.matches('-').count() == 4
}

#[test]
fn inputs_are_validated_before_anything_is_written() {
    let fx = fixture();
    let bad = |bytes: &[u8], input: NewArtifact| {
        fx.artifacts
            .create_at(bytes, input, DAY_2023_11_14)
            .unwrap_err()
    };
    assert!(matches!(bad(b"", new("k", "text/plain")), Error::Input(_)));
    assert!(matches!(
        bad(&vec![1u8; MAX_ARTIFACT_BYTES + 1], new("k", "text/plain")),
        Error::Input(_)
    ));
    assert!(matches!(bad(b"x", new("", "text/plain")), Error::Input(_)));
    assert!(matches!(
        bad(b"x", new("k\n", "text/plain")),
        Error::Input(_)
    ));
    assert!(matches!(bad(b"x", new("k", "Text/Plain")), Error::Input(_)));
    assert!(matches!(
        bad(
            b"x",
            NewArtifact {
                run_id: Some("a\nb".into()),
                ..new("k", "text/plain")
            }
        ),
        Error::Input(_)
    ));
    assert!(matches!(
        bad(
            b"x",
            NewArtifact {
                task_id: Some(String::new()),
                ..new("k", "text/plain")
            }
        ),
        Error::Input(_)
    ));
    assert!(fx.artifacts.list(10, None).unwrap().items.is_empty());
    assert!(
        !fx.root.join("artifacts").exists()
            || std::fs::read_dir(fx.root.join("artifacts"))
                .unwrap()
                .count()
                == 0
    );
    assert!(
        fx.artifacts
            .create_at(
                &vec![1u8; MAX_ARTIFACT_BYTES],
                new("k", "application/octet-stream"),
                DAY_2023_11_14
            )
            .is_ok(),
        "the limit itself is allowed"
    );
}

#[test]
fn ids_cannot_reach_other_paths_and_tenants_are_isolated() {
    let fx = fixture();
    for id in ["../x", "a/b", "", "not-a-uuid", "..\\x"] {
        assert!(matches!(fx.artifacts.get(id), Err(Error::Input(_))), "{id}");
        assert!(fx.artifacts.read_bytes(id).is_err());
        assert!(fx.artifacts.delete(id).is_err());
    }
    let artifact = fx
        .artifacts
        .create(b"hello", new("note", "text/plain"))
        .unwrap();
    let other = fx
        .store
        .for_tenant("other".into())
        .unwrap()
        .artifacts(&fx.root)
        .unwrap();
    assert_eq!(
        other.get(&artifact.id).unwrap(),
        None,
        "another tenant cannot see it"
    );
    assert!(state(other.read_bytes(&artifact.id).unwrap_err()).contains("ARTIFACT_NOT_FOUND"));
    assert!(
        !other.delete(&artifact.id).unwrap(),
        "another tenant cannot delete it"
    );
    assert_eq!(
        fx.artifacts.get(&artifact.id).unwrap().unwrap().status,
        "active"
    );
    assert!(other.list(10, None).unwrap().items.is_empty());
    assert_eq!(fx.artifacts.list(10, None).unwrap().items.len(), 1);
    assert!(
        other.list(10, Some("[\"tenant\",")).is_err(),
        "a cursor from another tenant is refused"
    );
}

#[test]
fn reads_verify_the_recorded_hash_and_report_missing_files() {
    let fx = fixture();
    let artifact = fx
        .artifacts
        .create_at(b"important text", new("note", "text/plain"), DAY_2023_11_14)
        .unwrap();
    let file = fx
        .root
        .join("artifacts/2023-11-14")
        .join(format!("{}.txt", artifact.id));
    assert_eq!(
        fx.artifacts.read_text(&artifact.id).unwrap(),
        "important text"
    );
    std::fs::write(&file, b"tampered text!").unwrap();
    assert!(
        state(fx.artifacts.read_bytes(&artifact.id).unwrap_err()).contains("ARTIFACT_CORRUPT"),
        "same size, different content"
    );
    std::fs::write(&file, b"short").unwrap();
    assert!(state(fx.artifacts.read_text(&artifact.id).unwrap_err()).contains("ARTIFACT_CORRUPT"));
    std::fs::remove_file(&file).unwrap();
    assert!(state(fx.artifacts.read_bytes(&artifact.id).unwrap_err()).contains("ARTIFACT_MISSING"));
}

#[test]
fn text_reads_refuse_binary_and_invalid_utf8() {
    let fx = fixture();
    let text = fx
        .artifacts
        .create(b"hola \xf0\x9f\xa6\x80", new("note", "text/plain"))
        .unwrap();
    assert_eq!(fx.artifacts.read_text(&text.id).unwrap(), "hola 🦀");
    for (bytes, mime) in [
        (png(2, 2), "image/png"),
        (b"%PDF-1.4 x".to_vec(), "application/pdf"),
        (b"a\0b".to_vec(), "text/plain"),
        (b"x".to_vec(), "application/octet-stream"),
    ] {
        let artifact = fx.artifacts.create(&bytes, new("blob", mime)).unwrap();
        assert!(
            state(fx.artifacts.read_text(&artifact.id).unwrap_err()).contains("binary"),
            "{mime}"
        );
        assert_eq!(
            fx.artifacts.read_bytes(&artifact.id).unwrap(),
            bytes,
            "raw bytes stay readable"
        );
    }
    let broken = fx
        .artifacts
        .create(b"\xff\xfe text", new("note", "text/plain"))
        .unwrap();
    assert!(state(fx.artifacts.read_text(&broken.id).unwrap_err()).contains("UTF-8"));
}

#[test]
fn retention_is_explicit_and_expiry_removes_only_elapsed_files() {
    let fx = fixture();
    let default = fx
        .artifacts
        .create_at(b"a", new("k", "text/plain"), DAY_2023_11_14)
        .unwrap();
    let short = fx
        .artifacts
        .create_at(
            b"b",
            NewArtifact {
                expires_in_ms: Some(Some(1000)),
                ..new("k", "text/plain")
            },
            DAY_2023_11_14,
        )
        .unwrap();
    let forever = fx
        .artifacts
        .create_at(
            b"c",
            NewArtifact {
                expires_in_ms: Some(None),
                ..new("k", "text/plain")
            },
            DAY_2023_11_14,
        )
        .unwrap();
    assert_eq!(
        (short.expires_at, forever.expires_at),
        (Some(DAY_2023_11_14 + 1000), None)
    );

    assert!(
        fx.artifacts
            .expire(DAY_2023_11_14 + 999)
            .unwrap()
            .is_empty(),
        "nothing is deleted before its time"
    );
    assert_eq!(
        fx.artifacts.expire(DAY_2023_11_14 + 1000).unwrap(),
        std::slice::from_ref(&short.id)
    );
    let expired = fx.artifacts.get(&short.id).unwrap().unwrap();
    assert_eq!(
        (expired.status.as_str(), expired.expired_at),
        ("expired", Some(DAY_2023_11_14 + 1000))
    );
    assert!(state(fx.artifacts.read_bytes(&short.id).unwrap_err()).contains("ARTIFACT_EXPIRED"));
    assert!(
        !fx.root
            .join("artifacts/2023-11-14")
            .join(format!("{}.txt", short.id))
            .exists(),
        "the file is gone"
    );
    assert!(
        fx.artifacts
            .expire(DAY_2023_11_14 + 1000)
            .unwrap()
            .is_empty(),
        "idempotent"
    );

    let swept = fx
        .artifacts
        .expire(DAY_2023_11_14 + DEFAULT_RETENTION_MS)
        .unwrap();
    assert_eq!(swept, std::slice::from_ref(&default.id));
    assert_eq!(
        fx.artifacts.read_text(&forever.id).unwrap(),
        "c",
        "kept forever"
    );
}

#[test]
fn deleting_removes_the_content_once() {
    let fx = fixture();
    let artifact = fx
        .artifacts
        .create_at(b"x", new("k", "text/plain"), DAY_2023_11_14)
        .unwrap();
    assert!(fx.artifacts.delete(&artifact.id).unwrap());
    assert!(
        !fx.artifacts.delete(&artifact.id).unwrap(),
        "already deleted"
    );
    assert_eq!(
        fx.artifacts.get(&artifact.id).unwrap().unwrap().status,
        "deleted"
    );
    assert!(state(fx.artifacts.read_bytes(&artifact.id).unwrap_err()).contains("ARTIFACT_DELETED"));
    assert!(
        !fx.root
            .join("artifacts/2023-11-14")
            .join(format!("{}.txt", artifact.id))
            .exists()
    );
}

#[test]
fn listing_pages_with_a_continuation_cursor() {
    let fx = fixture();
    let mut ids: Vec<String> = (0..5)
        .map(|n| {
            fx.artifacts
                .create(format!("n{n}").as_bytes(), new("k", "text/plain"))
                .unwrap()
                .id
        })
        .collect();
    ids.sort();
    let (mut seen, mut cursor) = (vec![], None);
    loop {
        let page = fx.artifacts.list(2, cursor.as_deref()).unwrap();
        assert!(page.items.len() <= 2);
        seen.extend(page.items.into_iter().map(|artifact| artifact.id));
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    seen.sort();
    assert_eq!(seen, ids);
    assert!(fx.artifacts.list(0, None).is_err() && fx.artifacts.list(201, None).is_err());
    assert!(
        fx.artifacts.list(1, Some("[\"someone-else\",")).is_err(),
        "foreign cursor"
    );
}

fn call(store: &ArtifactStore, name: &str, arguments: Value) -> Result<Value, Error> {
    store.execute_tool(name, arguments)
}

#[test]
fn tools_save_read_inspect_list_delete_and_report_image_info() {
    let fx = fixture();
    let names: Vec<_> = fx
        .artifacts
        .tool_definitions()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert_eq!(
        names,
        [
            "artifact_save",
            "artifact_inspect",
            "artifact_read",
            "artifact_list",
            "artifact_delete",
            "image_info"
        ]
    );

    let saved = call(
        &fx.artifacts,
        "artifact_save",
        json!({"kind":"note","mime_type":"text/plain","text":"hola"}),
    )
    .unwrap();
    let id = saved["id"].as_str().unwrap().to_owned();
    assert_eq!(
        call(&fx.artifacts, "artifact_read", json!({"id":id})).unwrap(),
        json!({"encoding":"text","text":"hola"})
    );
    assert_eq!(
        call(&fx.artifacts, "artifact_inspect", json!({"id":id})).unwrap()["size"],
        4
    );

    let bytes = png(7, 9);
    let encoded: String = {
        let saved = call(
            &fx.artifacts,
            "artifact_save",
            json!({"kind":"shot","mime_type":"image/png","data":
            // standard base64 of the PNG header bytes, built independently of the crate
            base64_of(&bytes)}),
        )
        .unwrap();
        assert_eq!(
            (
                saved["mime_type"].as_str(),
                saved["width"].as_u64(),
                saved["height"].as_u64()
            ),
            (Some("image/png"), Some(7), Some(9))
        );
        let image = saved["id"].as_str().unwrap().to_owned();
        assert_eq!(
            call(&fx.artifacts, "image_info", json!({"id":image})).unwrap(),
            json!({"mime_type":"image/png","width":7,"height":9})
        );
        call(
            &fx.artifacts,
            "artifact_read",
            json!({"id":image,"encoding":"base64"}),
        )
        .unwrap()["data"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(encoded, base64_of(&bytes), "base64 read is byte-exact");
    assert!(
        call(&fx.artifacts, "image_info", json!({"id":id})).is_err(),
        "text is not an image"
    );
    assert_eq!(
        call(&fx.artifacts, "artifact_list", json!({})).unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let forever = call(
        &fx.artifacts,
        "artifact_save",
        json!({"kind":"keep","mime_type":"text/plain","text":"x","expires_in_days":null}),
    )
    .unwrap();
    assert!(forever["expires_at"].is_null());
    let week = call(
        &fx.artifacts,
        "artifact_save",
        json!({"kind":"k","mime_type":"text/plain","text":"x","expires_in_days":2}),
    )
    .unwrap();
    assert_eq!(
        week["expires_at"].as_u64().unwrap() - week["created_at"].as_u64().unwrap(),
        2 * 86_400_000
    );
    assert_eq!(
        call(&fx.artifacts, "artifact_delete", json!({"id":id})).unwrap(),
        json!({"deleted":true})
    );

    for bad in [
        (
            "artifact_save",
            json!({"kind":"k","mime_type":"text/plain"}),
        ),
        (
            "artifact_save",
            json!({"kind":"k","mime_type":"text/plain","text":"a","data":"YQ=="}),
        ),
        (
            "artifact_save",
            json!({"kind":"k","mime_type":"text/plain","data":"not base64!"}),
        ),
        (
            "artifact_save",
            json!({"kind":"k","mime_type":"text/plain","text":"a","expires_in_days":0}),
        ),
        (
            "artifact_save",
            json!({"kind":"k","mime_type":"text/plain","text":"a","extra":1}),
        ),
        ("artifact_read", json!({"id":"short"})),
        ("nope", json!({})),
    ] {
        assert!(
            call(&fx.artifacts, bad.0, bad.1.clone()).is_err(),
            "{bad:?}"
        );
    }
}
fn base64_of(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                A[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

#[test]
fn the_store_dispatches_artifact_commands_for_bindings() {
    let fx = fixture();
    let run = |command: Value| -> Result<Value, Error> {
        let reply = fx
            .store
            .dispatch(&json!({"op":"artifacts","root":fx.root,"command":command}).to_string())?;
        Ok(serde_json::from_str(&reply)?)
    };
    assert_eq!(
        run(json!({"op":"tool_definitions"}))
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        6
    );
    let saved = run(json!({"op":"execute_tool","name":"artifact_save","arguments":{"kind":"k","mime_type":"text/plain","text":"hi","expires_in_days":1}})).unwrap();
    assert_eq!(saved["mime_type"], "text/plain");
    let swept = run(json!({"op":"expire","now":saved["expires_at"]})).unwrap();
    assert_eq!(swept["expired"], json!([saved["id"]]));
    assert!(run(json!({"op":"bogus"})).is_err());
}
