#![cfg(feature = "tools")]
use hive_agent_core::{
    Error,
    provider::CancellationToken,
    tools::{NativeTools, OfficePolicy, ToolsConfig},
};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/office");

fn tools(dir: &Path, office: Value) -> NativeTools {
    NativeTools::from_json(json!({"root":dir,"office":office})).unwrap()
}
async fn call(tools: &NativeTools, name: &str, arguments: Value) -> Result<Value, Error> {
    tools
        .execute_tool(name, arguments, &CancellationToken::new())
        .await
}
fn message(error: Error) -> String {
    match error {
        Error::Input(message) | Error::State(message) => message,
        other => other.to_string(),
    }
}
fn fixture(dir: &Path, name: &str) {
    std::fs::copy(PathBuf::from(FIXTURES).join(name), dir.join(name)).unwrap();
}

// ---- a tiny hostile-ZIP builder -------------------------------------------------

struct Part<'a> {
    name: &'a str,
    data: Vec<u8>,
    deflate: bool,
    crc: Option<u32>,
    flags: u16,
    method: Option<u16>,
    size: Option<u32>,
}
fn part<'a>(name: &'a str, data: &[u8]) -> Part<'a> {
    Part {
        name,
        data: data.to_vec(),
        deflate: false,
        crc: None,
        flags: 0,
        method: None,
        size: None,
    }
}
fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}
fn build(parts: &[Part]) -> Vec<u8> {
    let (mut body, mut directory) = (Vec::new(), Vec::new());
    for part in parts {
        let stored = if part.deflate {
            miniz_oxide::deflate::compress_to_vec(&part.data, 6)
        } else {
            part.data.clone()
        };
        let method = part.method.unwrap_or(if part.deflate { 8 } else { 0 });
        let crc = part.crc.unwrap_or_else(|| crc32(&part.data));
        let size = part.size.unwrap_or(part.data.len() as u32);
        let offset = body.len() as u32;
        for central in [false, true] {
            let out = if central { &mut directory } else { &mut body };
            out.extend_from_slice(if central {
                b"PK\x01\x02\x14\0"
            } else {
                b"PK\x03\x04"
            });
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&part.flags.to_le_bytes());
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&[0, 0, 0x21, 0]);
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(stored.len() as u32).to_le_bytes());
            out.extend_from_slice(&size.to_le_bytes());
            out.extend_from_slice(&(part.name.len() as u16).to_le_bytes());
            out.extend_from_slice(&[0, 0]);
            if central {
                out.extend_from_slice(&[0; 10]);
                out.extend_from_slice(&offset.to_le_bytes());
            }
            out.extend_from_slice(part.name.as_bytes());
            if !central {
                out.extend_from_slice(&stored);
            }
        }
    }
    let start = body.len() as u32;
    let size = directory.len() as u32;
    body.extend(directory);
    body.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    body.extend_from_slice(&(parts.len() as u16).to_le_bytes());
    body.extend_from_slice(&(parts.len() as u16).to_le_bytes());
    body.extend_from_slice(&size.to_le_bytes());
    body.extend_from_slice(&start.to_le_bytes());
    body.extend_from_slice(&[0, 0]);
    body
}
fn docx_xml(body: &str) -> Vec<u8> {
    let xml = format!(
        "<?xml version=\"1.0\"?><w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body>{body}</w:body></w:document>"
    );
    build(&[part("word/document.xml", xml.as_bytes())])
}

// ---- round trips ----------------------------------------------------------------

#[tokio::test]
async fn word_documents_round_trip_text_styles_lists_and_tables() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    let written = call(&tools, "office_write_docx", json!({
        "path":"out/report.docx","create_dirs":true,"title":"Informe & <resumen>",
        "paragraphs":[
            {"text":"Capítulo","type":"title1"},{"text":"Sección","type":"title2"},{"text":"Sub","type":"title3"},
            {"text":"Hola 🦀 \"mundo\" & <tags>","bold":true,"italic":true},
            {"text":"línea uno\nlínea dos\tcon tab"},
            {"text":"viñeta","type":"list"},
            {"text":"con\u{1}control\u{b}chars"}],
        "tables":[{"header":true,"rows":[{"cells":["Nombre","Valor"]},{"cells":["alfa","1"]},{"cells":["corta"]}]}]
    })).await.unwrap();
    assert_eq!(written["path"], "out/report.docx");
    assert!(written["bytes"].as_u64().unwrap() > 1000);

    let read = call(
        &tools,
        "office_read_docx",
        json!({"path":"out/report.docx"}),
    )
    .await
    .unwrap();
    let styles: Vec<_> = read["paragraphs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["text"].as_str().unwrap(), p["style"].as_str()))
        .collect();
    assert_eq!(styles[0], ("Informe & <resumen>", Some("Title")));
    assert_eq!(
        &styles[1..4],
        [
            ("Capítulo", Some("Heading1")),
            ("Sección", Some("Heading2")),
            ("Sub", Some("Heading3"))
        ]
    );
    assert_eq!(styles[4], ("Hola 🦀 \"mundo\" & <tags>", None));
    assert_eq!(styles[5], ("línea uno\nlínea dos\tcon tab", None));
    assert_eq!(styles[6], ("viñeta", Some("ListParagraph")));
    assert_eq!(
        styles[7].0, "concontrolchars",
        "characters XML cannot carry are dropped"
    );
    assert_eq!(
        read["tables"],
        json!([[["Nombre", "Valor"], ["alfa", "1"], ["corta", ""]]]),
        "short rows are padded"
    );
    assert!(read["text"].as_str().unwrap().contains("alfa\t1"));

    let without = call(
        &tools,
        "office_read_docx",
        json!({"path":"out/report.docx","include_tables":false}),
    )
    .await
    .unwrap();
    assert_eq!(without["tables"], json!([]));
    assert!(!without["text"].as_str().unwrap().contains("alfa"));

    let empty = call(&tools, "office_write_docx", json!({"path":"empty.docx"}))
        .await
        .unwrap();
    assert!(empty["bytes"].as_u64().unwrap() > 0);
    assert_eq!(
        call(&tools, "office_read_docx", json!({"path":"empty.docx"}))
            .await
            .unwrap()["text"],
        ""
    );
}

#[tokio::test]
async fn excel_workbooks_round_trip_values_and_never_execute_formulas() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    call(&tools, "office_write_xlsx", json!({"path":"book.xlsx","sheets":[
        {"name":"Ventas","data":[{"producto":"café","unidades":12,"activo":true,"precio":3.5,"nota":null},{"producto":"=SUM(A1:A2)","unidades":-7,"activo":false,"precio":1e21,"nota":"x"},{"producto":"café","unidades":0,"precio":0.1,"extra":{"a":[1,2]}}]},
        {"name":"Filas","data":[["a",1,true],["b",null,"c"],[]]},
        {"name":"Orden","headers":["z","a"],"data":[{"a":1,"z":2}]}]})).await.unwrap();

    let read = call(&tools, "office_read_xlsx", json!({"path":"book.xlsx"}))
        .await
        .unwrap();
    assert_eq!(read["sheets"], json!(["Ventas", "Filas", "Orden"]));
    let rows = read["data"]["Ventas"].as_array().unwrap();
    assert_eq!(
        rows[0],
        json!({"activo":true,"extra":"","nota":"","precio":3.5,"producto":"café","unidades":12})
    );
    assert_eq!(
        rows[1]["producto"], "=SUM(A1:A2)",
        "formula-looking text stays text"
    );
    assert_eq!(
        (rows[1]["unidades"].clone(), rows[1]["activo"].clone()),
        (json!(-7), json!(false))
    );
    assert_eq!(rows[1]["precio"].as_f64(), Some(1e21));
    assert_eq!(
        rows[2]["extra"], "{\"a\":[1,2]}",
        "nested values are stored as JSON text"
    );
    assert_eq!(
        read["data"]["Filas"],
        json!([{"a":"b","1":"","true":"c"}]),
        "first row became the header"
    );
    assert_eq!(
        read["data"]["Orden"],
        json!([{"z":2,"a":1}]),
        "explicit headers order the columns"
    );
    assert_eq!(read["total_rows"], 3 + 1 + 1);

    let arrays = call(
        &tools,
        "office_read_xlsx",
        json!({"path":"book.xlsx","sheet":"Filas","include_headers":false}),
    )
    .await
    .unwrap();
    assert_eq!(
        arrays["data"]["Filas"],
        json!([["a", 1, true], ["b", "", "c"]]),
        "blank rows are skipped"
    );

    // The written XML really uses plain string cells.
    let bytes = std::fs::read(dir.path().join("book.xlsx")).unwrap();
    let archive = zip_entries(&bytes);
    assert!(
        archive.contains(&"xl/sharedStrings.xml".to_owned())
            && archive.contains(&"xl/worksheets/sheet3.xml".to_owned())
    );
}
fn zip_entries(bytes: &[u8]) -> Vec<String> {
    // Central directory names, read independently of the crate's own reader.
    let mut names = vec![];
    let mut at = 0;
    while let Some(found) = bytes[at..].windows(4).position(|w| w == b"PK\x01\x02") {
        let start = at + found;
        let name_len = u16::from_le_bytes([bytes[start + 28], bytes[start + 29]]) as usize;
        names.push(String::from_utf8_lossy(&bytes[start + 46..start + 46 + name_len]).into_owned());
        at = start + 46 + name_len;
    }
    names
}

#[tokio::test]
async fn presentations_round_trip_titles_text_and_bullets() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    call(
        &tools,
        "office_write_pptx",
        json!({"path":"deck.pptx","presentation_title":"Resumen 🦀","slides":[
        {"title":"Puntos","points":["uno & dos","tres <3"]},
        {"title":"Texto","content":"línea A\nlínea B"},
        {"content":"sin título"}]}),
    )
    .await
    .unwrap();
    let read = call(&tools, "office_read_pptx", json!({"path":"deck.pptx"}))
        .await
        .unwrap();
    assert_eq!(read["total_slides"], 4);
    let slides = read["slides"].as_array().unwrap();
    assert_eq!(
        (slides[0]["title"].as_str(), slides[0]["text"].as_str()),
        (Some("Resumen 🦀"), Some("Resumen 🦀"))
    );
    assert_eq!(
        slides[1]["paragraphs"],
        json!(["Puntos", "uno & dos", "tres <3"])
    );
    assert_eq!(
        (slides[1]["title"].as_str(), slides[1]["text"].as_str()),
        (Some("Puntos"), Some("Puntos uno & dos tres <3"))
    );
    assert_eq!(
        slides[2]["paragraphs"],
        json!(["Texto", "línea A", "línea B"])
    );
    assert_eq!(
        slides[3]["title"], "sin título",
        "falls back to the first paragraph"
    );
    let one = call(
        &tools,
        "office_read_pptx",
        json!({"path":"deck.pptx","only_slide":2}),
    )
    .await
    .unwrap();
    assert_eq!(
        (
            one["slides"].as_array().unwrap().len(),
            one["slides"][0]["number"].clone()
        ),
        (1, json!(2))
    );
    assert!(
        message(
            call(
                &tools,
                "office_read_pptx",
                json!({"path":"deck.pptx","only_slide":9})
            )
            .await
            .unwrap_err()
        )
        .contains("slide 9 not found")
    );
}

// ---- documents written by real Office libraries ----------------------------------

#[tokio::test]
async fn reads_a_document_written_by_python_docx() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), "word-reference.docx");
    let read = call(
        &tools(dir.path(), json!({})),
        "office_read_docx",
        json!({"path":"word-reference.docx"}),
    )
    .await
    .unwrap();
    let paragraphs: Vec<_> = read["paragraphs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["text"].as_str().unwrap().to_owned(),
                p["style"].as_str().map(str::to_owned),
            )
        })
        .collect();
    assert_eq!(
        paragraphs[0],
        ("Informe trimestral".into(), Some("Heading1".into()))
    );
    assert_eq!(
        paragraphs[1].0, "Texto con acentos: canción, niño 🦀 & <tags> \"citas\".",
        "entities are decoded"
    );
    assert_eq!(paragraphs[2].0, "negrita y cursiva", "runs are joined");
    assert_eq!(
        paragraphs[3],
        ("Primer punto".into(), Some("ListBullet".into()))
    );
    assert_eq!(paragraphs[4].0, "Línea uno\nLínea dos\tcon tab");
    assert_eq!(paragraphs.last().unwrap().0, "Después de la tabla");
    assert_eq!(
        read["tables"],
        json!([[["Nombre", "Valor"], ["alfa", "1"]]])
    );
    assert!(
        !paragraphs.iter().any(|(text, _)| text == "alfa"),
        "table cells are not top-level paragraphs"
    );
}

#[tokio::test]
async fn reads_a_workbook_written_by_openpyxl() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), "excel-reference.xlsx");
    let tools = tools(dir.path(), json!({}));
    let read = call(
        &tools,
        "office_read_xlsx",
        json!({"path":"excel-reference.xlsx"}),
    )
    .await
    .unwrap();
    assert_eq!(read["sheets"], json!(["Ventas", "Datos", "Vacía"]));
    let ventas = read["data"]["Ventas"].as_array().unwrap();
    assert_eq!(
        ventas[0],
        json!({"producto":"café","unidades":12,"activo":true,"precio":3.5,"__EMPTY":"","nota":"a"})
    );
    assert_eq!(
        ventas[1],
        json!({"producto":"té","unidades":7,"activo":false,"precio":2.25,"__EMPTY":"","nota":"b"})
    );
    // Formula cells carry no cached value when a library wrote them, so they read as empty.
    assert_eq!(ventas[2]["producto"], "");
    assert_eq!(ventas[2]["precio"].as_f64(), Some(1e21));
    assert_eq!(ventas[2]["nota"], "fórmula como texto");
    assert_eq!(read["data"]["Vacía"], json!([]));

    let range = call(&tools, "office_read_xlsx", json!({"path":"excel-reference.xlsx","sheet":"Datos","range":"A2:B3","include_headers":false})).await.unwrap();
    assert_eq!(range["data"]["Datos"], json!([[1, 2], [3, 4]]));
    let header_in_range = call(
        &tools,
        "office_read_xlsx",
        json!({"path":"excel-reference.xlsx","sheet":"Datos","range":"A2:B4"}),
    )
    .await
    .unwrap();
    assert_eq!(
        header_in_range["data"]["Datos"],
        json!([{"1":3,"2":4},{"1":5,"2":6}]),
        "the first row of the range is the header"
    );
    let reversed = call(&tools, "office_read_xlsx", json!({"path":"excel-reference.xlsx","sheet":"Datos","range":"B3:A2","include_headers":false})).await.unwrap();
    assert_eq!(
        reversed["data"]["Datos"],
        json!([[1, 2], [3, 4]]),
        "range corners can come in any order"
    );

    let missing = message(
        call(
            &tools,
            "office_read_xlsx",
            json!({"path":"excel-reference.xlsx","sheet":"Nope"}),
        )
        .await
        .unwrap_err(),
    );
    assert!(
        missing.contains("not found") && missing.contains("Ventas, Datos, Vacía"),
        "{missing}"
    );
    for range in ["A1", "1A:B2", "A1:", "ZZZZ1:B2", "A0:B2", "A1:B2:C3"] {
        let result = call(
            &tools,
            "office_read_xlsx",
            json!({"path":"excel-reference.xlsx","range":range}),
        )
        .await;
        if range == "A1" {
            assert!(result.is_ok(), "a single cell is a valid range");
        } else {
            assert!(
                message(result.unwrap_err()).contains("A1 notation"),
                "{range}"
            );
        }
    }
}

#[tokio::test]
async fn reads_a_presentation_written_by_python_pptx() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), "powerpoint-reference.pptx");
    let read = call(
        &tools(dir.path(), json!({})),
        "office_read_pptx",
        json!({"path":"powerpoint-reference.pptx"}),
    )
    .await
    .unwrap();
    assert_eq!(read["total_slides"], 3);
    let slides = read["slides"].as_array().unwrap();
    assert_eq!(
        (slides[0]["title"].as_str(), slides[0]["paragraphs"].clone()),
        (Some("Portada"), json!(["Portada", "Subtítulo 🦀"]))
    );
    assert_eq!(
        slides[1]["paragraphs"],
        json!(["Puntos", "uno", "dos & tres"])
    );
    assert_eq!(
        slides[2]["paragraphs"],
        json!(["Solo título", "caja de texto"])
    );
    assert_eq!(
        slides[2]["title"], "Solo título",
        "the title placeholder wins over the first text box"
    );
}

// ---- hostile input ---------------------------------------------------------------

#[tokio::test]
async fn hostile_containers_are_rejected_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    let xml = b"<w:document xmlns:w=\"x\"><w:body/></w:document>";
    let big = vec![b'a'; 50 * 1024 * 1024];
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("not a zip", b"hello".to_vec(), "end of central directory"),
        ("empty", vec![], "end of central directory"),
        (
            "bad crc",
            build(&[Part {
                crc: Some(0xdead_beef),
                ..part("word/document.xml", xml)
            }]),
            "CRC-32",
        ),
        (
            "wrong size",
            build(&[Part {
                size: Some(3),
                ..part("word/document.xml", xml)
            }]),
            "recorded size",
        ),
        (
            "encrypted",
            build(&[Part {
                flags: 1,
                ..part("word/document.xml", xml)
            }]),
            "encrypted",
        ),
        (
            "odd method",
            build(&[Part {
                method: Some(12),
                ..part("word/document.xml", xml)
            }]),
            "compression method",
        ),
        (
            "duplicate",
            build(&[
                part("word/document.xml", xml),
                part("word/document.xml", xml),
            ]),
            "duplicate",
        ),
        (
            "zip bomb ratio",
            build(&[Part {
                deflate: true,
                ..part("word/document.xml", &vec![b' '; 31 * 1024 * 1024])
            }]),
            "compression ratio",
        ),
        (
            "entry too large",
            build(&[Part {
                size: Some(40 * 1024 * 1024),
                ..part("word/document.xml", xml)
            }]),
            "too large",
        ),
        (
            "total too large",
            build(&[
                Part {
                    deflate: true,
                    ..part("a", &big)
                },
                Part {
                    deflate: true,
                    ..part("b", &big)
                },
            ]),
            "",
        ),
        (
            "truncated",
            build(&[part("word/document.xml", xml)])[..30].to_vec(),
            "end of central directory",
        ),
        (
            "no document part",
            build(&[part("other.xml", xml)]),
            "word/document.xml",
        ),
        (
            "corrupt deflate",
            build(&[Part {
                method: Some(8),
                ..part("word/document.xml", b"not deflate data at all....")
            }]),
            "",
        ),
    ];
    for (label, bytes, expected) in cases {
        std::fs::write(dir.path().join("x.docx"), bytes).unwrap();
        let error = message(
            call(&tools, "office_read_docx", json!({"path":"x.docx"}))
                .await
                .expect_err(label),
        );
        assert!(error.contains(expected), "{label}: {error}");
    }
}

#[tokio::test]
async fn zip64_and_oversized_directories_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    let mut zip64 = build(&[part("word/document.xml", b"<a/>")]);
    let end = zip64.len() - 22;
    zip64[end + 12..end + 16].copy_from_slice(&0xffff_ffffu32.to_le_bytes());
    std::fs::write(dir.path().join("x.docx"), &zip64).unwrap();
    assert!(
        message(
            call(&tools, "office_read_docx", json!({"path":"x.docx"}))
                .await
                .unwrap_err()
        )
        .contains("ZIP64")
    );

    // Many tiny entries: more than the entry limit.
    let names: Vec<String> = (0..4100).map(|n| format!("p{n}")).collect();
    let many = build(
        &names
            .iter()
            .map(|name| part(name, b"x"))
            .collect::<Vec<_>>(),
    );
    std::fs::write(dir.path().join("x.docx"), many).unwrap();
    assert!(
        message(
            call(&tools, "office_read_docx", json!({"path":"x.docx"}))
                .await
                .unwrap_err()
        )
        .contains("too many entries")
    );
}

#[tokio::test]
async fn xml_is_strict_about_entities_doctypes_and_structure() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    let read = |xml: String| {
        std::fs::write(dir.path().join("x.docx"), docx_xml(&xml)).unwrap();
        call(&tools, "office_read_docx", json!({"path":"x.docx"}))
    };
    let para = |text: &str| format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>");
    assert_eq!(
        read(para("a &amp; b &#65;&#x42; &lt;&gt;&quot;&apos;"))
            .await
            .unwrap()["text"],
        "a & b AB <>\"'"
    );
    assert_eq!(
        read(format!("<!-- c -->{}<![CDATA[<raw>]]>", para("ok")))
            .await
            .unwrap()["text"],
        "ok"
    );
    for (label, xml, expected) in [
        ("custom entity", para("&xxe;"), "custom entities"),
        ("bad reference", para("&#xD800;"), "out of range"),
        ("nul reference", para("&#0;"), "out of range"),
        (
            "unterminated entity",
            para("a &amp b"),
            "unterminated entity",
        ),
        ("mismatched", "<w:p></w:r>".to_owned(), "mismatched"),
        ("unclosed", "<w:p>".to_owned(), "mismatched"),
        ("unquoted attribute", "<w:p a=1/>".to_owned(), "unquoted"),
        ("no value", "<w:p a/>".to_owned(), "without a value"),
    ] {
        let error = message(read(xml).await.expect_err(label));
        assert!(error.contains(expected), "{label}: {error}");
    }
    // A DOCTYPE with an external entity never gets as far as being resolved.
    std::fs::write(dir.path().join("x.docx"), build(&[part("word/document.xml", b"<?xml version=\"1.0\"?><!DOCTYPE d [<!ENTITY x SYSTEM \"file:///etc/passwd\">]><w:document xmlns:w=\"x\"><w:body/></w:document>")])).unwrap();
    assert!(
        message(
            call(&tools, "office_read_docx", json!({"path":"x.docx"}))
                .await
                .unwrap_err()
        )
        .contains("DOCTYPE")
    );
    let deep = format!("{}{}", "<w:sdt>".repeat(300), "</w:sdt>".repeat(300));
    assert!(message(read(deep).await.unwrap_err()).contains("nested too deeply"));
}

#[tokio::test]
async fn files_are_checked_against_the_sandbox_and_the_limits() {
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("root");
    std::fs::create_dir(&root).unwrap();
    fixture(&root, "word-reference.docx");
    std::fs::create_dir(root.join("folder.docx")).unwrap();
    let tools = tools(
        &root,
        json!({"max_input_bytes":1000,"max_sheets":2,"max_rows_per_sheet":2}),
    );
    let big = tools_with_default(&root);
    for (path, expected) in [
        ("word-reference.docx", "byte limit"),
        ("../x.docx", "relative"),
        ("/etc/x.docx", "relative"),
        ("missing.docx", "does not exist"),
        ("folder.docx", "regular file"),
        ("word-reference.txt", ".docx"),
    ] {
        let error = message(
            call(&tools, "office_read_docx", json!({"path":path}))
                .await
                .expect_err(path),
        );
        assert!(error.contains(expected), "{path}: {error}");
    }
    assert!(
        call(
            &big,
            "office_read_docx",
            json!({"path":"word-reference.docx"})
        )
        .await
        .is_ok(),
        "within the default limit"
    );
    assert!(
        message(
            call(
                &big,
                "office_read_xlsx",
                json!({"path":"word-reference.docx"})
            )
            .await
            .unwrap_err()
        )
        .contains(".xlsx")
    );

    assert!(
        message(
            call(&big, "office_write_docx", json!({"path":"new/out.docx"}))
                .await
                .unwrap_err()
        )
        .contains("create_dirs")
    );
    assert!(
        message(
            call(&big, "office_write_docx", json!({"path":"../out.docx"}))
                .await
                .unwrap_err()
        )
        .contains("relative")
    );
    assert!(!outer.path().join("out.docx").exists());
    call(
        &big,
        "office_write_docx",
        json!({"path":"word-reference.docx","title":"replaced"}),
    )
    .await
    .unwrap();
    let again = call(
        &big,
        "office_read_docx",
        json!({"path":"word-reference.docx"}),
    )
    .await
    .unwrap();
    assert_eq!(
        again["paragraphs"][0]["text"], "replaced",
        "writes replace the file atomically"
    );
    assert!(std::fs::read_dir(&root).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".hive-tmp")
    }));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outer.path(), root.join("link")).unwrap();
        assert!(
            call(
                &big,
                "office_write_docx",
                json!({"path":"link/out.docx","create_dirs":true})
            )
            .await
            .is_err()
        );
        assert!(!outer.path().join("out.docx").exists());
    }
}
fn tools_with_default(root: &Path) -> NativeTools {
    tools(root, json!({}))
}

#[tokio::test]
async fn spreadsheet_limits_and_names_are_enforced_on_read_and_write() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({"max_sheets":2,"max_rows_per_sheet":3}));
    let rows = |n: usize| (0..n).map(|i| json!({"v":i})).collect::<Vec<_>>();
    for (label, sheets, expected) in [
        (
            "too many rows",
            json!([{"name":"S","data":rows(4)}]),
            "3 row limit",
        ),
        (
            "too many sheets",
            json!([{"name":"A","data":[]},{"name":"B","data":[]},{"name":"C","data":[]}]),
            "at most 2 sheets",
        ),
        (
            "duplicate",
            json!([{"name":"A","data":[]},{"name":"a","data":[]}]),
            "duplicate",
        ),
        (
            "bad char",
            json!([{"name":"a/b","data":[]}]),
            "invalid or duplicate",
        ),
        (
            "long name",
            json!([{"name":"x".repeat(32),"data":[]}]),
            "do not match schema",
        ),
        (
            "history",
            json!([{"name":"History","data":[]}]),
            "invalid or duplicate",
        ),
        (
            "mixed rows",
            json!([{"name":"M","data":[["a"],{"b":1}]}]),
            "mix rows",
        ),
        (
            "giant cell",
            json!([{"name":"G","data":[["x".repeat(32_768)]]}]),
            "32767",
        ),
        (
            "too many columns",
            json!([{"name":"C","data":[vec![1; 1001]]}]),
            "columns",
        ),
    ] {
        let error = message(
            call(
                &tools,
                "office_write_xlsx",
                json!({"path":"b.xlsx","sheets":sheets}),
            )
            .await
            .expect_err(label),
        );
        assert!(error.contains(expected), "{label}: {error}");
    }
    // A workbook with more rows than the read limit is reported, not truncated.
    let wide = self::tools(dir.path(), json!({}));
    call(&wide, "office_write_xlsx", json!({"path":"b.xlsx","sheets":[{"name":"S","data":rows(5)},{"name":"T","data":rows(1)},{"name":"U","data":[]}]})).await.unwrap();
    let error = message(
        call(&tools, "office_read_xlsx", json!({"path":"b.xlsx"}))
            .await
            .unwrap_err(),
    );
    assert!(error.contains("more than 2 sheets"), "{error}");
    call(
        &wide,
        "office_write_xlsx",
        json!({"path":"c.xlsx","sheets":[{"name":"S","data":rows(5)},{"name":"T","data":rows(2)}]}),
    )
    .await
    .unwrap();
    let error = message(
        call(&tools, "office_read_xlsx", json!({"path":"c.xlsx"}))
            .await
            .unwrap_err(),
    );
    assert!(
        error.contains("sheet `S` exceeds the 3 row limit"),
        "{error}"
    );
    let only = call(
        &tools,
        "office_read_xlsx",
        json!({"path":"c.xlsx","sheet":"T"}),
    )
    .await
    .unwrap();
    assert_eq!(
        only["data"]["T"].as_array().unwrap().len(),
        2,
        "other sheets are not read when one is requested"
    );
    let ranged = call(
        &tools,
        "office_read_xlsx",
        json!({"path":"c.xlsx","sheet":"S","range":"A1:A3"}),
    )
    .await
    .unwrap();
    assert_eq!(
        ranged["data"]["S"].as_array().unwrap().len(),
        2,
        "a range keeps a big sheet readable"
    );
}

#[tokio::test]
async fn office_tools_exist_only_when_configured_and_validate_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let plain = NativeTools::new(ToolsConfig::new(dir.path())).unwrap();
    let names = |tools: &NativeTools| {
        tools
            .tool_definitions()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>()
    };
    assert!(!names(&plain).iter().any(|name| name.starts_with("office_")));
    assert!(
        call(&plain, "office_read_docx", json!({"path":"a.docx"}))
            .await
            .is_err()
    );
    let office = tools(dir.path(), json!({}));
    assert_eq!(
        names(&office)[6..],
        [
            "office_read_docx",
            "office_write_docx",
            "office_read_xlsx",
            "office_write_xlsx",
            "office_read_pdf",
            "office_write_pdf",
            "office_read_pptx",
            "office_write_pptx"
        ]
    );
    for (name, args) in [
        (
            "office_write_docx",
            json!({"path":"a.docx","paragraphs":[{"text":"x","type":"bogus"}]}),
        ),
        (
            "office_write_docx",
            json!({"path":"a.docx","paragraphs":[{"text":"x","extra":1}]}),
        ),
        (
            "office_write_docx",
            json!({"path":"a.docx","tables":[{"rows":[]}]}),
        ),
        ("office_write_pptx", json!({"path":"a.pptx","slides":[]})),
        (
            "office_write_pptx",
            json!({"path":"a.pptx","slides":[{"bogus":"n"}]}),
        ),
        ("office_read_pptx", json!({"path":"a.pptx","only_slide":0})),
        (
            "office_read_xlsx",
            json!({"path":"a.xlsx","include_headers":"yes"}),
        ),
    ] {
        assert!(
            message(call(&office, name, args.clone()).await.unwrap_err())
                .contains("do not match schema"),
            "{name} {args}"
        );
    }
    for office in [
        json!({"max_input_bytes":0}),
        json!({"max_sheets":0}),
        json!({"timeout_ms":0}),
        json!({"bogus":1}),
        json!({"max_rows_per_sheet":2_000_000}),
    ] {
        assert!(NativeTools::from_json(json!({"root":dir.path(),"office":office})).is_err());
    }
    let _ = OfficePolicy::default();
}

// ---- styled presentations --------------------------------------------------------

/// Text of one part of a generated package, read independently of the crate.
fn part_text(bytes: &[u8], name: &str) -> String {
    let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]) as usize;
    let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
    let mut at = 0;
    while let Some(found) = bytes[at..].windows(4).position(|w| w == b"PK\x01\x02") {
        let start = at + found;
        let (method, size, name_len) = (u16_at(start + 10), u32_at(start + 20), u16_at(start + 28));
        let entry = String::from_utf8_lossy(&bytes[start + 46..start + 46 + name_len]).into_owned();
        if entry == name {
            let local = u32_at(start + 42);
            let data_at = local + 30 + u16_at(local + 26) + u16_at(local + 28);
            let raw = &bytes[data_at..data_at + size];
            let data = if method == 8 {
                miniz_oxide::inflate::decompress_to_vec(raw).unwrap()
            } else {
                raw.to_vec()
            };
            return String::from_utf8(data).unwrap();
        }
        at = start + 46 + name_len;
    }
    panic!("part {name} not found");
}

async fn deck(tools: &NativeTools, dir: &Path, arguments: Value) -> Vec<u8> {
    call(tools, "office_write_pptx", arguments).await.unwrap();
    std::fs::read(dir.join("deck.pptx")).unwrap()
}

#[tokio::test]
async fn slides_are_styled_by_the_master_theme_and_layouts_not_by_each_shape() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    let slides = json!([{"title":"Uno","points":["a","b"]},{"title":"Dos","content":"texto"}]);
    let modern = deck(&tools, dir.path(), json!({"path":"deck.pptx","presentation_title":"Portada","presentation_subtitle":"Sub","slides":slides})).await;

    // Theme: palette and fonts, with the colors text/background mapped to dk1/lt1.
    let theme = part_text(&modern, "ppt/theme/theme1.xml");
    for expected in [
        "<a:dk1><a:srgbClr val=\"334155\"/>",
        "<a:lt1><a:srgbClr val=\"F8FAFC\"/>",
        "<a:accent1><a:srgbClr val=\"2563EB\"/>",
        "<a:accent2><a:srgbClr val=\"F59E0B\"/>",
        "<a:latin typeface=\"Calibri\"/>",
    ] {
        assert!(theme.contains(expected), "{expected}");
    }
    // Master: background, accent bar, title rule, styled title and bullets, two layouts.
    let master = part_text(&modern, "ppt/slideMasters/slideMaster1.xml");
    for expected in [
        "<a:schemeClr val=\"bg1\"/>",
        "name=\"Accent Bar\"",
        "name=\"Title Rule\"",
        "<a:srgbClr val=\"0F172A\"/>",
        "<a:buChar char=\"&#8226;\"/>",
        "r:id=\"rId2\"",
    ] {
        assert!(master.contains(expected), "{expected}");
    }
    let cover = part_text(&modern, "ppt/slideLayouts/slideLayout1.xml");
    assert!(
        cover.contains("showMasterSp=\"0\"")
            && cover.contains("type=\"ctrTitle\"")
            && cover.contains("type=\"subTitle\"")
    );
    assert!(
        cover.contains("<a:srgbClr val=\"0F172A\"/>") && cover.contains("name=\"Cover Band\""),
        "the cover has its own background and band"
    );
    assert!(
        part_text(&modern, "ppt/slideLayouts/slideLayout2.xml")
            .contains("name=\"Title and Content\"")
    );
    // Slides point at the right layout and carry no colors or fonts of their own.
    assert!(part_text(&modern, "ppt/slides/_rels/slide1.xml.rels").contains("slideLayout1.xml"));
    assert!(part_text(&modern, "ppt/slides/_rels/slide2.xml.rels").contains("slideLayout2.xml"));
    for number in 1..=3 {
        let slide = part_text(&modern, &format!("ppt/slides/slide{number}.xml"));
        assert!(slide.contains("<p:ph "), "slide {number} uses placeholders");
        assert!(
            !slide.contains("srgbClr")
                && !slide.contains("latin typeface")
                && !slide.contains("<a:xfrm>"),
            "slide {number} inherits its look: {slide}"
        );
    }
    assert!(part_text(&modern, "ppt/slides/slide1.xml").contains("type=\"subTitle\" idx=\"1\""));

    // The look changes with the preset and with overrides, which are normalized and validated.
    let dark = deck(
        &tools,
        dir.path(),
        json!({"path":"deck.pptx","theme":{"preset":"dark"},"slides":slides}),
    )
    .await;
    let dark_theme = part_text(&dark, "ppt/theme/theme1.xml");
    assert!(
        dark_theme.contains("<a:lt1><a:srgbClr val=\"111827\"/>")
            && dark_theme.contains("<a:dk1><a:srgbClr val=\"D1D5DB\"/>")
    );
    let corporate = deck(
        &tools,
        dir.path(),
        json!({"path":"deck.pptx","theme":{"preset":"corporate"},"slides":slides}),
    )
    .await;
    assert!(
        part_text(&corporate, "ppt/theme/theme1.xml")
            .contains("<a:majorFont><a:latin typeface=\"Georgia\"/>")
    );
    let custom = deck(&tools, dir.path(), json!({"path":"deck.pptx","presentation_title":"T","theme":{"preset":"light","colors":{"accent":"d946ef","cover_background":"4c1d95"},"fonts":{"title":"Segoe UI Semibold","body":"Arial"}},"slides":slides})).await;
    let custom_theme = part_text(&custom, "ppt/theme/theme1.xml");
    assert!(
        custom_theme.contains("<a:accent1><a:srgbClr val=\"D946EF\"/>"),
        "hex is uppercased"
    );
    assert!(
        custom_theme.contains("<a:majorFont><a:latin typeface=\"Segoe UI Semibold\"/>")
            && custom_theme.contains("<a:minorFont><a:latin typeface=\"Arial\"/>")
    );
    assert!(
        part_text(&custom, "ppt/slideLayouts/slideLayout1.xml")
            .contains("<a:srgbClr val=\"4C1D95\"/>")
    );
    assert!(
        part_text(&custom, "ppt/theme/theme1.xml")
            .contains("<a:accent2><a:srgbClr val=\"94A3B8\"/>"),
        "untouched colors keep the preset value"
    );

    for theme in [
        json!({"preset":"neon"}),
        json!({"colors":{"accent":"red"}}),
        json!({"colors":{"accent":"#2563EB"}}),
        json!({"colors":{"accent":"2563EB1"}}),
        json!({"colors":{"shadow":"000000"}}),
        json!({"fonts":{"title":"A<b>"}}),
        json!({"fonts":{"title":""}}),
        json!({"fonts":{"heading":"Arial"}}),
        json!({"extra":1}),
    ] {
        let error = message(
            call(
                &tools,
                "office_write_pptx",
                json!({"path":"bad.pptx","theme":theme,"slides":slides}),
            )
            .await
            .unwrap_err(),
        );
        assert!(error.contains("do not match schema"), "{theme}: {error}");
    }
    assert!(!dir.path().join("bad.pptx").exists());
}

#[tokio::test]
async fn long_titles_and_dense_bodies_get_smaller_type_and_short_ones_inherit_the_master() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    let words = |n: usize| vec!["palabra"; n].join(" ");
    let points =
        |count: usize, length: usize| (0..count).map(|_| "x".repeat(length)).collect::<Vec<_>>();
    let bytes = deck(
        &tools,
        dir.path(),
        json!({"path":"deck.pptx","presentation_title":words(8),"slides":[
        {"title":"corto","points":points(3, 20)},
        {"title":words(9),"points":points(7, 20)},
        {"title":words(12),"points":points(10, 20)},
        {"title":"t","points":points(13, 20)},
        {"title":"t","points":points(3, 300)},
        {"title":"t","content":"x".repeat(1300)}]}),
    )
    .await;
    let sizes = |number: usize| {
        let slide = part_text(&bytes, &format!("ppt/slides/slide{number}.xml"));
        let all: Vec<_> = slide
            .split("sz=\"")
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap().to_owned())
            .collect();
        all
    };
    assert_eq!(
        sizes(1),
        ["3600"],
        "a 63-character cover title shrinks one step"
    );
    assert!(sizes(2).is_empty(), "short slides inherit the master sizes");
    assert_eq!(
        sizes(3),
        [
            "2400", "1600", "1600", "1600", "1600", "1600", "1600", "1600"
        ],
        "a 71-character title and 7 points"
    );
    assert_eq!(sizes(4)[0], "2000", "a 95-character title");
    assert_eq!(&sizes(4)[1..3], ["1400", "1400"], "10 points");
    assert_eq!(&sizes(5)[..2], ["1200", "1200"], "13 points");
    assert_eq!(
        &sizes(6)[..2],
        ["1400", "1400"],
        "900 characters of bullets"
    );
    assert_eq!(&sizes(7)[..1], ["1200"], "1300 characters of free text");
    let read = call(
        &tools,
        "office_read_pptx",
        json!({"path":"deck.pptx","only_slide":1}),
    )
    .await
    .unwrap();
    assert_eq!(
        read["slides"][0]["paragraphs"].as_array().unwrap().len(),
        1,
        "no subtitle was given"
    );
    let with_subtitle = deck(&tools, dir.path(), json!({"path":"deck.pptx","presentation_title":"T","presentation_subtitle":"S","slides":[{"title":"a"}]})).await;
    assert!(part_text(&with_subtitle, "ppt/slides/slide1.xml").contains("<a:t>S</a:t>"));
    let read = call(
        &tools,
        "office_read_pptx",
        json!({"path":"deck.pptx","only_slide":1}),
    )
    .await
    .unwrap();
    assert_eq!(read["slides"][0]["paragraphs"], json!(["T", "S"]));
    assert_eq!(read["slides"][0]["title"], "T");
}

// ---- independent validation ------------------------------------------------------

/// Opens generated files with python-docx, openpyxl and python-pptx. Opt in with
/// HIVE_OFFICE_PYTHON=<python with those packages>; otherwise this test is skipped.
#[tokio::test]
async fn generated_files_open_in_independent_office_libraries() {
    let Ok(python) = std::env::var("HIVE_OFFICE_PYTHON") else {
        eprintln!(
            "skipped: set HIVE_OFFICE_PYTHON to a python with python-docx, openpyxl and python-pptx"
        );
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    call(&tools, "office_write_docx", json!({"path":"d.docx","title":"Título","paragraphs":[{"text":"Capítulo","type":"title1"},{"text":"negrita","bold":true},{"text":"punto","type":"list"}],
        "tables":[{"header":true,"rows":[{"cells":["a","b"]},{"cells":["1","2"]}]}]})).await.unwrap();
    call(&tools, "office_write_xlsx", json!({"path":"x.xlsx","sheets":[{"name":"Hoja","data":[{"k":"v","n":1.5,"b":true}]},{"name":"Dos","data":[["=1+1",2]]}]})).await.unwrap();
    call(&tools, "office_write_pptx", json!({"path":"p.pptx","presentation_title":"Portada","presentation_subtitle":"Sub","slides":[{"title":"Uno","points":["a","b"]},{"title":"Dos","content":"texto"}]})).await.unwrap();
    let script = r#"
import sys, docx, openpyxl, pptx
d = docx.Document('d.docx')
paragraphs = [(p.text, p.style.name) for p in d.paragraphs]
assert ('Título', 'Title') in paragraphs and ('Capítulo', 'Heading 1') in paragraphs, paragraphs
assert any(p.runs and p.runs[0].bold for p in d.paragraphs if p.text == 'negrita')
assert [[c.text for c in r.cells] for r in d.tables[0].rows] == [['a', 'b'], ['1', '2']]
w = openpyxl.load_workbook('x.xlsx')
assert w.sheetnames == ['Hoja', 'Dos'], w.sheetnames
h = w['Hoja']
assert [[c.value for c in row] for row in h.iter_rows()] == [['b', 'k', 'n'], [True, 'v', 1.5]], [[c.value for c in row] for row in h.iter_rows()]
cell = w['Dos']['A1']
assert cell.data_type == 's' and cell.value == '=1+1', (cell.data_type, cell.value)
p = pptx.Presentation('p.pptx')
assert len(p.slides) == 3
assert [s.shapes.title.text for s in p.slides] == ['Portada', 'Uno', 'Dos']
assert [s.slide_layout.name for s in p.slides] == ['Title Slide', 'Title and Content', 'Title and Content'], [s.slide_layout.name for s in p.slides]
assert [pg.text for pg in p.slides[1].placeholders[1].text_frame.paragraphs] == ['a', 'b']
assert p.slides[0].placeholders[1].text == 'Sub'
print('ok')
"#;
    let output = std::process::Command::new(python)
        .arg("-c")
        .arg(script)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn excel_dates_are_read_as_iso_by_their_number_format_and_as_serials_on_request() {
    let dir = tempfile::tempdir().unwrap();
    fixture(dir.path(), "dates-1900.xlsx");
    fixture(dir.path(), "dates-1904.xlsx");
    let tools = tools(dir.path(), json!({}));
    let rows = |read: Value| -> Vec<(String, Value)> {
        read["data"]["Fechas"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["Etiqueta"].as_str().unwrap().to_owned(),
                    r["Valor"].clone(),
                )
            })
            .collect()
    };
    let iso = rows(
        call(
            &tools,
            "office_read_xlsx",
            json!({"path":"dates-1900.xlsx"}),
        )
        .await
        .unwrap(),
    );
    let got = |label: &str| iso.iter().find(|(l, _)| l == label).unwrap().1.clone();
    assert_eq!(got("builtin 14"), "2025-03-01");
    assert_eq!(got("builtin 22 con hora"), "2025-03-01T14:30:00");
    assert_eq!(got("personalizado dd/mm/yyyy"), "2025-03-01");
    assert_eq!(got("hora builtin 21"), "12:00:00");
    assert_eq!(
        got("transcurrido [h]:mm:ss"),
        "36:00:00",
        "elapsed hours do not wrap at 24"
    );
    assert_eq!(
        got("1900-02-29 inexistente"),
        "1900-02-29",
        "Excel's invented day is shown as Excel shows it"
    );
    assert_eq!(got("primer día"), "1900-03-01");
    assert_eq!(got("serie 1"), "1900-01-01");
    assert_eq!(got("mmm-yy"), "2025-03-01");
    assert_eq!(
        got("celda ISO t=d"),
        "2025-03-01T00:00:00",
        "a typed date cell is kept as the text it is"
    );
    assert_eq!(got("mm:ss builtin 20"), "06:00:00");
    // Not dates: a percentage, General, quoted text that happens to contain date letters, a colored
    // conditional format, and a serial beyond Excel's last date.
    assert_eq!(got("porcentaje"), 0.25);
    assert_eq!(got("general"), 45717);
    assert_eq!(got("texto entre comillas"), 5);
    assert_eq!(got("color y condición"), -2.5);
    assert_eq!(got("fuera de rango"), 3_000_000);
    // `dates: serial` keeps Excel's numbers, exactly as stored.
    let serial = rows(
        call(
            &tools,
            "office_read_xlsx",
            json!({"path":"dates-1900.xlsx","dates":"serial"}),
        )
        .await
        .unwrap(),
    );
    let kept = |label: &str| serial.iter().find(|(l, _)| l == label).unwrap().1.clone();
    assert_eq!(
        (
            kept("builtin 14"),
            kept("hora builtin 21"),
            kept("1900-02-29 inexistente")
        ),
        (json!(45717), json!(0.5), json!(60))
    );
    assert_eq!(kept("celda ISO t=d"), "2025-03-01T00:00:00");
    // The 1904 date system (old Mac workbooks) counts from another day.
    let old = rows(
        call(
            &tools,
            "office_read_xlsx",
            json!({"path":"dates-1904.xlsx"}),
        )
        .await
        .unwrap(),
    );
    assert_eq!(old[0].1, "2024-03-02");
    assert_eq!(old[1].1, "18:00:00");
    assert!(
        call(
            &tools,
            "office_read_xlsx",
            json!({"path":"dates-1900.xlsx","dates":"unix"})
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn excel_columns_with_formats_round_trip_and_a_real_spreadsheet_shows_them_formatted() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    call(&tools, "office_write_xlsx", json!({"path":"ventas.xlsx","sheets":[{"name":"Ventas","headers":["fecha","monto","tasa","hora"],
        "data":[{"fecha":"2025-03-01","monto":1234.5,"tasa":0.25,"hora":"14:30"},{"fecha":"2025-12-31","monto":99,"tasa":0.075,"hora":"09:05:30"}],
        "columns":[{"column":"fecha","format":"date","width":14},{"column":"monto","format":"currency","bold":true},{"column":"tasa","format":"percent"},{"column":"hora","format":"time"}],
        "header_style":{"bold":true,"fill":"#DDEEFF"},"freeze_header":true,"autofilter":true}]})).await.unwrap();
    let back = call(&tools, "office_read_xlsx", json!({"path":"ventas.xlsx"}))
        .await
        .unwrap();
    assert_eq!(
        back["data"]["Ventas"],
        json!([
        {"fecha":"2025-03-01","monto":1234.5,"tasa":0.25,"hora":"14:30:00"},
        {"fecha":"2025-12-31","monto":99,"tasa":0.075,"hora":"09:05:30"}])
    );
    // A bad date is refused before anything is written.
    let error = call(&tools, "office_write_xlsx", json!({"path":"mal.xlsx","sheets":[{"name":"A","headers":["f"],"data":[{"f":"1 de marzo"}],"columns":[{"column":"f","format":"date"}]}]})).await.unwrap_err();
    assert!(message(error).contains("ISO date"));
    assert!(!dir.path().join("mal.xlsx").exists());
    // LibreOffice, an independent implementation, must show the cells as formatted (opt in: HIVE_TEST_SOFFICE=soffice).
    let Ok(soffice) = std::env::var("HIVE_TEST_SOFFICE") else {
        eprintln!("skipped: set HIVE_TEST_SOFFICE to soffice to check the file in LibreOffice");
        return;
    };
    let out = std::process::Command::new(soffice)
        .args([
            "--headless",
            "--convert-to",
            "csv:Text - txt - csv (StarCalc):44,34,76,1,,0,false,true,true",
            "--outdir",
        ])
        .arg(dir.path().join("out"))
        .arg(dir.path().join("ventas.xlsx"))
        .env("HOME", dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let csv = std::fs::read_to_string(dir.path().join("out/ventas.csv")).unwrap();
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines[0], "fecha,monto,tasa,hora", "{csv}");
    // The separators follow the machine's locale (1,234.50 or 1.234,50); the formats do not.
    let digits = |line: &str| -> String {
        line.chars()
            .filter(|c| c.is_ascii_digit() || matches!(c, '$' | '%' | '-' | ':'))
            .collect()
    };
    assert_eq!(
        digits(lines[1]),
        digits("2025-03-01,\"$1,234.50\",25.00%,14:30:00"),
        "{csv}"
    );
    assert_eq!(
        digits(lines[2]),
        digits("2025-12-31,$99.00,7.50%,09:05:30"),
        "{csv}"
    );
    assert!(
        lines[1].starts_with("2025-03-01,") && lines[1].ends_with(",14:30:00"),
        "{csv}"
    );
    assert!(
        lines[2].starts_with("2025-12-31,") && lines[2].ends_with(",09:05:30"),
        "{csv}"
    );
}

#[tokio::test]
async fn word_headers_and_footers_repeat_with_live_page_fields_and_are_read_back() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    let long: Vec<Value> = (0..120).map(|n| json!({"text":format!("Párrafo número {n} con bastante texto para que el documento ocupe varias páginas al abrirse en un procesador.")})).collect();
    call(&tools, "office_write_docx", json!({"path":"informe.docx","title":"Informe Q3","paragraphs":long,
        "header":{"text":"Confidencial — {title}","align":"right"},"footer":{"text":"Página {page} de {pages}","align":"center"}})).await.unwrap();
    let read = call(&tools, "office_read_docx", json!({"path":"informe.docx"}))
        .await
        .unwrap();
    assert_eq!(
        read["headers"],
        json!(["Confidencial — Informe Q3"]),
        "{read}"
    );
    assert_eq!(
        read["footers"],
        json!(["Página {page} de {pages}"]),
        "the page fields are read back as fields"
    );
    assert!(
        !read["text"].as_str().unwrap().contains("Confidencial"),
        "running text is not body text"
    );
    // No header or footer: nothing extra, as before.
    call(
        &tools,
        "office_write_docx",
        json!({"path":"plain.docx","paragraphs":[{"text":"hola"}]}),
    )
    .await
    .unwrap();
    let plain = call(&tools, "office_read_docx", json!({"path":"plain.docx"}))
        .await
        .unwrap();
    assert!(plain.get("headers").is_none() && plain.get("footers").is_none());
    // Only a footer, left-aligned, with literal text around and a field twice.
    call(&tools, "office_write_docx", json!({"path":"pie.docx","paragraphs":[{"text":"x"}],"footer":{"text":"{page}/{pages} · {page}","align":"left"}})).await.unwrap();
    let pie = call(&tools, "office_read_docx", json!({"path":"pie.docx"}))
        .await
        .unwrap();
    assert_eq!(
        (pie["footers"].clone(), pie.get("headers").is_none()),
        (json!(["{page}/{pages} · {page}"]), true)
    );
    for bad in [
        json!({"text":"x","align":"justify"}),
        json!({"text":"x".repeat(1001)}),
    ] {
        let error = call(
            &tools,
            "office_write_docx",
            json!({"path":"mal.docx","paragraphs":[{"text":"x"}],"header":bad}),
        )
        .await;
        assert!(error.is_err());
    }
    // LibreOffice lays the pages out: the header is on every page and the fields count them (opt in).
    let (Ok(soffice), true) = (
        std::env::var("HIVE_TEST_SOFFICE"),
        std::process::Command::new("pdftotext")
            .arg("-v")
            .output()
            .is_ok(),
    ) else {
        eprintln!(
            "skipped: set HIVE_TEST_SOFFICE and have pdftotext to check the page layout in LibreOffice"
        );
        return;
    };
    let out = std::process::Command::new(soffice)
        .args(["--headless", "--convert-to", "pdf", "--outdir"])
        .arg(dir.path().join("out"))
        .arg(dir.path().join("informe.docx"))
        .env("HOME", dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::process::Command::new("pdftotext")
        .arg("-layout")
        .arg(dir.path().join("out/informe.pdf"))
        .arg("-")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&text.stdout).to_string();
    let pages = text.matches('\u{c}').count();
    assert!(pages >= 3, "a long document is several pages: {pages}");
    assert_eq!(
        text.matches("Confidencial — Informe Q3").count(),
        pages,
        "the header is on every page"
    );
    for page in 1..=pages {
        assert!(
            text.contains(&format!("Página {page} de {pages}")),
            "page {page} of {pages} is numbered by the live fields"
        );
    }
}

#[tokio::test]
async fn powerpoint_speaker_notes_are_written_with_a_notes_master_and_read_back() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    call(&tools, "office_write_pptx", json!({"path":"deck.pptx","presentation_title":"Portada","slides":[
        {"title":"Uno","points":["a","b"],"notes":"Saluda y presenta el tema.\nMenciona los dos puntos & el cierre <importante>."},
        {"title":"Dos","content":"sin notas"},
        {"title":"Tres","content":"con notas","notes":"Solo una línea."}]})).await.unwrap();
    let read = call(&tools, "office_read_pptx", json!({"path":"deck.pptx"}))
        .await
        .unwrap();
    let slides = read["slides"].as_array().unwrap();
    assert_eq!(slides.len(), 4);
    assert!(slides[0].get("notes").is_none(), "the cover has none");
    assert_eq!(
        slides[1]["notes"],
        "Saluda y presenta el tema.\nMenciona los dos puntos & el cierre <importante>."
    );
    assert!(
        slides[2].get("notes").is_none(),
        "a slide without notes has no notes page"
    );
    assert_eq!(slides[3]["notes"], "Solo una línea.");
    assert!(
        !slides[1]["text"].as_str().unwrap().contains("Saluda"),
        "notes are not slide text"
    );
    // A deck without notes is the deck it always was: no notes master.
    call(
        &tools,
        "office_write_pptx",
        json!({"path":"sin.pptx","slides":[{"title":"A","content":"b"}]}),
    )
    .await
    .unwrap();
    let plain = call(&tools, "office_read_pptx", json!({"path":"sin.pptx"}))
        .await
        .unwrap();
    assert!(plain["slides"][0].get("notes").is_none());
    assert!(
        call(
            &tools,
            "office_write_pptx",
            json!({"path":"x.pptx","slides":[{"title":"A","notes":"x".repeat(100_001)}]})
        )
        .await
        .is_err()
    );
    // LibreOffice must open it and export the notes pages (opt in).
    let (Ok(soffice), true) = (
        std::env::var("HIVE_TEST_SOFFICE"),
        std::process::Command::new("pdftotext")
            .arg("-v")
            .output()
            .is_ok(),
    ) else {
        eprintln!(
            "skipped: set HIVE_TEST_SOFFICE and have pdftotext to check the notes in LibreOffice"
        );
        return;
    };
    let out = std::process::Command::new(soffice)
        .args(["--headless", "--convert-to", "pdf:impress_pdf_Export:{\"ExportNotesPages\":{\"type\":\"boolean\",\"value\":\"true\"}}", "--outdir"])
        .arg(dir.path().join("out")).arg(dir.path().join("deck.pptx")).env("HOME", dir.path()).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(
        &std::process::Command::new("pdftotext")
            .arg(dir.path().join("out/deck.pdf"))
            .arg("-")
            .output()
            .unwrap()
            .stdout,
    )
    .to_string();
    assert!(
        text.contains("Saluda y presenta el tema.") && text.contains("Solo una línea."),
        "{text}"
    );
}
