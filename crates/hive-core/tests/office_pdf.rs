#![cfg(feature = "tools")]
use hive_agent_core::{Error, provider::CancellationToken, tools::NativeTools};
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
async fn text_of(tools: &NativeTools, name: &str) -> Value {
    call(tools, "office_read_pdf", json!({"path":name}))
        .await
        .unwrap()
}
fn lines(value: &Value) -> Vec<String> {
    value["text"]
        .as_str()
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

// ---- a minimal hand-made PDF builder ---------------------------------------------

#[derive(Default)]
struct Pdf {
    objects: Vec<(u32, Vec<u8>)>,
    trailer: String,
}
impl Pdf {
    fn object(mut self, number: u32, body: &str) -> Self {
        self.objects.push((number, body.as_bytes().to_vec()));
        self
    }
    fn stream(mut self, number: u32, dictionary: &str, data: &[u8]) -> Self {
        let mut body = format!("<< {dictionary} /Length {} >>\nstream\n", data.len()).into_bytes();
        body.extend_from_slice(data);
        body.extend_from_slice(b"\nendstream");
        self.objects.push((number, body));
        self
    }
    fn trailer(mut self, trailer: &str) -> Self {
        self.trailer = trailer.to_owned();
        self
    }
    fn build(self) -> Vec<u8> {
        let mut out = b"%PDF-1.4\n".to_vec();
        for (number, body) in self.objects {
            out.extend_from_slice(format!("{number} 0 obj\n").as_bytes());
            out.extend(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let trailer = if self.trailer.is_empty() {
            "<< /Root 1 0 R >>"
        } else {
            &self.trailer
        };
        out.extend_from_slice(format!("trailer\n{trailer}\n%%EOF\n").as_bytes());
        out
    }
}
/// One page with a font dictionary (object 4) and an uncompressed content stream.
fn page(font: &str, content: &str) -> Pdf {
    Pdf::default()
        .object(1, "<< /Type /Catalog /Pages 2 0 R >>")
        .object(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>")
        .object(3, "<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>")
        .object(4, font)
        .stream(5, "", content.as_bytes())
}
const HELVETICA: &str = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>";

async fn read_bytes(dir: &Path, bytes: Vec<u8>) -> Result<Value, Error> {
    std::fs::write(dir.join("x.pdf"), bytes).unwrap();
    call(
        &tools(dir, json!({})),
        "office_read_pdf",
        json!({"path":"x.pdf"}),
    )
    .await
}
async fn extract(content: &str, font: &str) -> Value {
    let dir = tempfile::tempdir().unwrap();
    read_bytes(dir.path(), page(font, content).build())
        .await
        .unwrap()
}

// ---- writing ---------------------------------------------------------------------

#[tokio::test]
async fn pdfs_are_written_wrapped_and_read_back() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    let long = "palabra ".repeat(200);
    let written = call(&tools, "office_write_pdf", json!({"path":"out/a.pdf","create_dirs":true,"title":"Informe 🦀 ñandú",
        "content":format!("Canción, niño, €100 “comillas” – guiones\n\n{long}\nTabulado\tcon tab y emoji 🦀")})).await.unwrap();
    assert_eq!(
        (
            written["path"].as_str(),
            written["pages"].as_u64(),
            written["replaced_characters"].as_u64()
        ),
        (Some("out/a.pdf"), Some(1), Some(2)),
        "two crabs became ?"
    );
    let read = text_of(&tools, "out/a.pdf").await;
    let text = read["text"].as_str().unwrap();
    assert!(text.starts_with("Informe ? ñandú\n"), "{text}");
    assert!(text.contains("Canción, niño, €100 “comillas” – guiones"));
    assert!(text.contains("Tabulado con tab y emoji ?"));
    assert_eq!(
        read["metadata"]["title"], "Informe 🦀 ñandú",
        "the title keeps its Unicode in the metadata"
    );
    assert_eq!(read["metadata"]["producer"], "Hive SDK");
    let wrapped = lines(&read)
        .iter()
        .filter(|line| line.starts_with("palabra"))
        .count();
    assert!(
        wrapped > 5,
        "a 1600-character paragraph wraps into many lines, got {wrapped}"
    );
    assert!(
        lines(&read).iter().all(|line| line.chars().count() < 120),
        "no line runs past the margin"
    );

    // Many lines make many pages, and the page size is honoured.
    let many = (0..300)
        .map(|n| format!("linea {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let big = call(
        &tools,
        "office_write_pdf",
        json!({"path":"b.pdf","content":many,"page_size":"Letter","margin":72,"font_size":14}),
    )
    .await
    .unwrap();
    assert!(big["pages"].as_u64().unwrap() > 3);
    let bytes = std::fs::read(dir.path().join("b.pdf")).unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("/MediaBox [0 0 612.00 792.00]"));
    let again = text_of(&tools, "b.pdf").await;
    assert_eq!(again["total_pages"], big["pages"]);
    assert!(again["text"].as_str().unwrap().contains("linea 299"));
    let range = call(
        &tools,
        "office_read_pdf",
        json!({"path":"b.pdf","start_page":2,"end_page":2}),
    )
    .await
    .unwrap();
    assert_eq!(range["pages"].as_array().unwrap().len(), 1);
    assert_eq!(range["pages"][0]["number"], 2);
    assert!(!range["text"].as_str().unwrap().contains("linea 0\n"));
}

#[tokio::test]
async fn pdf_write_options_are_validated() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    for (arguments, expected) in [
        (json!({"path":"a.pdf"}), "exactly one of"),
        (
            json!({"path":"a.pdf","content":"x","blocks":[]}),
            "exactly one of",
        ),
        (
            json!({"path":"a.pdf","content":"x","page_size":"A3"}),
            "do not match schema",
        ),
        (
            json!({"path":"a.pdf","content":"x","margin":-1}),
            "do not match schema",
        ),
        (
            json!({"path":"a.pdf","content":"x","font_size":3}),
            "do not match schema",
        ),
        (json!({"path":"a.txt","content":"x"}), ".pdf"),
        (json!({"path":"../a.pdf","content":"x"}), "relative"),
    ] {
        let error = message(
            call(&tools, "office_write_pdf", arguments.clone())
                .await
                .unwrap_err(),
        );
        assert!(error.contains(expected), "{arguments}: {error}");
    }
    let lots = "x\n".repeat(40_000);
    assert!(
        message(
            call(
                &tools,
                "office_write_pdf",
                json!({"path":"a.pdf","content":lots})
            )
            .await
            .unwrap_err()
        )
        .contains("500 pages")
    );
    assert!(!dir.path().join("a.pdf").exists());
    let empty = call(
        &tools,
        "office_write_pdf",
        json!({"path":"e.pdf","content":""}),
    )
    .await
    .unwrap();
    assert_eq!(empty["pages"], 1, "an empty document still has a page");
}

/// Poppler is an independent reader: it must see what this crate wrote.
#[tokio::test]
async fn poppler_reads_the_pdfs_this_crate_writes() {
    if std::process::Command::new("pdftotext")
        .arg("-v")
        .output()
        .is_err()
    {
        eprintln!("skipped: pdftotext (poppler) is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    call(&tools, "office_write_pdf", json!({"path":"p.pdf","title":"Título","content":"Canción, niño, €100 “comillas”\nSegunda línea con (paréntesis) y \\ barra\n\nTercer párrafo"})).await.unwrap();
    let output = std::process::Command::new("pdftotext")
        .args(["-layout", "p.pdf", "-"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    for expected in [
        "Título",
        "Canción, niño, €100 “comillas”",
        "Segunda línea con (paréntesis) y \\ barra",
        "Tercer párrafo",
    ] {
        assert!(
            text.contains(expected),
            "poppler did not see `{expected}` in:\n{text}"
        );
    }
}

// ---- real producers --------------------------------------------------------------

#[tokio::test]
async fn reads_pdfs_from_reportlab_chrome_libreoffice_and_ghostscript() {
    let dir = tempfile::tempdir().unwrap();
    for name in [
        "pdf-reportlab.pdf",
        "pdf-chrome.pdf",
        "pdf-lo.pdf",
        "pdf-gs.pdf",
    ] {
        fixture(dir.path(), name);
    }
    let tools = tools(dir.path(), json!({}));

    let reportlab = text_of(&tools, "pdf-reportlab.pdf").await;
    assert_eq!(reportlab["total_pages"], 2);
    assert_eq!(
        reportlab["pages"][0]["text"],
        "Informe trimestral\nIngresos: 1.234,50 € y margen del 24%\nCanción, niño, acentuación: áéíóú ñ ü\n“Comillas” y – guiones — raros\nÚltima línea de la página uno"
    );
    assert_eq!(
        reportlab["pages"][1]["text"],
        "Segunda página con texto\nFin."
    );
    assert_eq!(
        (
            reportlab["metadata"]["title"].as_str(),
            reportlab["metadata"]["author"].as_str()
        ),
        (Some("Informe reportlab"), Some("Equipo"))
    );
    assert_eq!(reportlab["warnings"], json!([]));

    // Chrome splits words across runs positioned exactly where the last one ended.
    let chrome = text_of(&tools, "pdf-chrome.pdf").await;
    assert_eq!(
        chrome["text"],
        "Título en Chrome\nUn párrafo con acentos: canción, niño, ñandú. Y emoji 🦀 aparte.\npunto uno\npunto dos\na b\n1 2\nNegrita y cursiva en serif."
    );
    assert!(
        chrome["metadata"]["producer"]
            .as_str()
            .unwrap()
            .starts_with("Skia/PDF")
    );

    for name in ["pdf-lo.pdf", "pdf-gs.pdf"] {
        let lines = lines(&text_of(&tools, name).await);
        for expected in [
            "Informe trimestral",
            "negrita y cursiva",
            "• Primer punto",
            "Línea uno",
            "Línea dos con tab",
            "Nombre Valor",
            "alfa 1",
            "Después de la tabla",
        ] {
            assert!(
                lines.iter().any(|line| line == expected),
                "{name}: missing `{expected}` in {lines:?}"
            );
        }
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("Texto con acentos: canción, niño")
                    && line.contains("& <tags> \"citas\".")),
            "{name}: {lines:?}"
        );
    }
    let gs = text_of(&tools, "pdf-gs.pdf").await;
    assert!(
        gs["metadata"]["producer"]
            .as_str()
            .unwrap()
            .contains("Ghostscript")
    );
    assert!(
        std::fs::read(dir.path().join("pdf-gs.pdf"))
            .unwrap()
            .windows(6)
            .any(|w| w == b"ObjStm"),
        "this fixture really uses object streams"
    );
}

/// Same characters as poppler, ignoring order within a page and glyphs drawn with a
/// fallback font (emoji), which poppler places by geometry and this reader by stream order.
#[tokio::test]
async fn extracted_words_match_poppler_on_real_pdfs() {
    if std::process::Command::new("pdftotext")
        .arg("-v")
        .output()
        .is_err()
    {
        eprintln!("skipped: pdftotext (poppler) is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    // Word boundaries differ between readers (poppler's raw mode glues table cells), so
    // compare every non-space character, sorted: lost or invented text still shows up.
    let words = |text: &str| {
        let mut chars: Vec<char> = text
            .chars()
            .filter(|c| {
                !c.is_whitespace()
                    && (*c as u32) < 0xffff
                    && !('\u{e000}'..='\u{f8ff}').contains(c)
                    && *c != '•'
            })
            .collect();
        chars.sort();
        chars
    };
    for name in [
        "pdf-reportlab.pdf",
        "pdf-chrome.pdf",
        "pdf-lo.pdf",
        "pdf-gs.pdf",
    ] {
        fixture(dir.path(), name);
        let ours = words(text_of(&tools, name).await["text"].as_str().unwrap());
        let output = std::process::Command::new("pdftotext")
            .arg(name)
            .arg("-")
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_eq!(
            ours,
            words(&String::from_utf8(output.stdout).unwrap()),
            "{name}"
        );
    }
}

// ---- hand-made documents: encodings, operators, filters ---------------------------

#[tokio::test]
async fn simple_font_encodings_are_applied() {
    assert_eq!(
        extract(
            "BT /F1 12 Tf (caf\\351 \\(ok\\) \\\\ \\101) Tj ET",
            HELVETICA
        )
        .await["text"],
        "café (ok) \\ A",
        "WinAnsi by default, with escapes and octal"
    );
    assert_eq!(
        extract("BT /F1 12 Tf <E9 41 4> Tj ET", HELVETICA).await["text"],
        "éA@",
        "hex strings, odd digit counts pad with zero"
    );
    let mac = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /MacRomanEncoding >>";
    assert_eq!(
        extract("BT /F1 12 Tf <8E8A41> Tj ET", mac).await["text"],
        "éäA"
    );
    let differences = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding << /Type /Encoding /Differences [65 /eacute /uni00E1 /Euro 90 /Z] >> >>";
    assert_eq!(
        extract("BT /F1 12 Tf (ABCZ) Tj ET", differences).await["text"],
        "éá€Z"
    );
    let named = extract(
        "BT /F1 12 Tf (x) Tj ET",
        "<< /Type /Font /Subtype /Type1 /Encoding << /BaseEncoding /MacRomanEncoding >> >>",
    )
    .await;
    assert_eq!(named["text"], "x");
}

#[tokio::test]
async fn text_operators_decide_lines_words_and_kerning() {
    let content = "BT /F1 12 Tf 14 TL 72 700 Td (Linea1) Tj 0 -14 Td (Linea2) Tj T* (Linea3) Tj (Linea4) ' 1 0 0 1 72 600 Tm (Tm) Tj ET";
    assert_eq!(
        extract(content, HELVETICA).await["text"],
        "Linea1\nLinea2\nLinea3\nLinea4\nTm"
    );
    assert_eq!(
        extract(
            "BT /F1 12 Tf [(Hello) -350 (world) 20 (!) -50 (x)] TJ ET",
            HELVETICA
        )
        .await["text"],
        "Hello world!x",
        "only wide kerning is a space"
    );
    // Default glyph width is half an em: "AB" advances 12, so a 12-unit move continues the word.
    assert_eq!(
        extract(
            "BT /F1 12 Tf 72 700 Td (AB) Tj 12 0 Td (C) Tj 50 0 Td (D) Tj ET",
            HELVETICA
        )
        .await["text"],
        "ABC D"
    );
    assert_eq!(extract("BT /F1 12 Tf 72 700 Td (A) Tj ET BT /F1 12 Tf 100 700 Td (B) Tj ET BT /F1 12 Tf 72 680 Td (C) Tj ET", HELVETICA).await["text"], "A B\nC", "positions carry across BT blocks");
    let widths = "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /FirstChar 65 /LastChar 66 /Widths [1000 1000] >>";
    assert_eq!(
        extract("BT /F1 10 Tf 0 0 Td (AB) Tj 20 0 Td (C) Tj ET", widths).await["text"],
        "ABC",
        "declared widths are used"
    );
    assert_eq!(
        extract("BT /F1 10 Tf 0 0 Td (AB) Tj 30 0 Td (C) Tj ET", widths).await["text"],
        "AB C"
    );
    assert_eq!(
        extract(
            "q 1 0 0 1 0 0 cm BI /W 1 /H 1 ID \u{1}\u{2}\u{3} EI Q BT /F1 12 Tf (after) Tj ET",
            HELVETICA
        )
        .await["text"],
        "after",
        "inline image data is skipped"
    );
}

#[tokio::test]
async fn composite_fonts_use_tounicode_and_widths_or_are_reported() {
    let cmap = "/CIDInit /ProcSet findresource begin 12 dict begin begincmap\n1 begincodespacerange <0000> <FFFF> endcodespacerange\n2 beginbfchar <0041> <0058> <0042> <D83DDE00> endbfchar\n2 beginbfrange <0043> <0045> <0061> <0046> <0047> [<0067> <0068>] endbfrange\nendcmap end end";
    let pdf = Pdf::default()
        .object(1, "<< /Type /Catalog /Pages 2 0 R >>")
        .object(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>")
        .object(3, "<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 4 0 R /F2 7 0 R >> >> /Contents 5 0 R >>")
        .object(4, "<< /Type /Font /Subtype /Type0 /Encoding /Identity-H /DescendantFonts [6 0 R] /ToUnicode 8 0 R >>")
        .stream(5, "", b"BT /F1 12 Tf 0 0 Td <004100420043004400450046> Tj 72 0 Td <0047> Tj ET BT /F2 12 Tf 0 -20 Td <00410042> Tj ET BT /F1 12 Tf 0 -40 Td <00410041> Tj 24 0 Td <0042> Tj ET")
        .object(6, "<< /Type /Font /Subtype /CIDFontType2 /BaseFont /X /DW 1000 /W [66 [500]] >>")
        .object(7, "<< /Type /Font /Subtype /Type0 /Encoding /Identity-H /DescendantFonts [6 0 R] >>")
        .stream(8, "", cmap.as_bytes());
    let dir = tempfile::tempdir().unwrap();
    let read = read_bytes(dir.path(), pdf.build()).await.unwrap();
    let lines = lines(&read);
    assert_eq!(
        lines[0], "X😀abcg h",
        "{lines:?}: a bfchar, a surrogate pair, a range and an array range, then a gap after 66 points"
    );
    assert!(read["warnings"].as_array().unwrap().iter().any(|w| {
        w.as_str()
            .unwrap()
            .contains("font `F2` has no Unicode mapping")
    }));
    assert_eq!(
        lines[1], "XX😀",
        "two glyphs advance 24 points, so a move of 24 continues the run"
    );
    assert_eq!(lines.len(), 2);
}

#[tokio::test]
async fn stream_filters_and_failures_are_handled_per_stream() {
    fn hex(bytes: &[u8]) -> Vec<u8> {
        let mut out: Vec<u8> = bytes
            .iter()
            .flat_map(|b| format!("{b:02x}").into_bytes())
            .collect();
        out.push(b'>');
        out
    }
    fn ascii85(bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for chunk in bytes.chunks(4) {
            let mut group = [0u8; 4];
            group[..chunk.len()].copy_from_slice(chunk);
            let mut value = u32::from_be_bytes(group);
            let mut digits = [0u8; 5];
            for digit in digits.iter_mut().rev() {
                *digit = (value % 85) as u8 + b'!';
                value /= 85;
            }
            out.extend_from_slice(&digits[..chunk.len() + 1]);
        }
        out.extend_from_slice(b"~>");
        out
    }
    let text = b"BT /F1 12 Tf (filtered text) Tj ET";
    let flate = miniz_oxide::deflate::compress_to_vec_zlib(text, 6);
    let base = |stream: Pdf| {
        stream.object(1, "<< /Type /Catalog /Pages 2 0 R >>").object(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>").object(3, "<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>").object(4, HELVETICA)
    };
    let dir = tempfile::tempdir().unwrap();
    for (label, dictionary, data) in [
        ("flate", "/Filter /FlateDecode", flate.clone()),
        ("flate abbreviated", "/Filter [/Fl]", flate.clone()),
        ("ascii hex", "/Filter /ASCIIHexDecode", hex(text)),
        ("ascii85", "/Filter /ASCII85Decode", ascii85(text)),
        (
            "hex then flate",
            "/Filter [/ASCIIHexDecode /FlateDecode]",
            hex(&flate),
        ),
    ] {
        let read = read_bytes(
            dir.path(),
            base(Pdf::default().stream(5, dictionary, &data)).build(),
        )
        .await
        .unwrap();
        assert_eq!(read["text"], "filtered text", "{label}");
    }

    let unsupported = read_bytes(
        dir.path(),
        base(Pdf::default().stream(5, "/Filter /LZWDecode", b"xx")).build(),
    )
    .await
    .unwrap();
    assert_eq!(unsupported["text"], "");
    let warnings = unsupported["warnings"].to_string();
    assert!(
        warnings.contains("unsupported stream filter `LZWDecode`") && warnings.contains("scanned"),
        "{warnings}"
    );
    let predictor = read_bytes(
        dir.path(),
        base(Pdf::default().stream(
            5,
            "/Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns 4 >>",
            &flate,
        ))
        .build(),
    )
    .await
    .unwrap();
    assert!(
        predictor["warnings"]
            .to_string()
            .contains("predictors are not supported")
    );

    // A Flate stream that inflates far past the limit is an error for that page only.
    let bomb = miniz_oxide::deflate::compress_to_vec_zlib(&vec![b' '; 40 * 1024 * 1024], 6);
    let read = read_bytes(
        dir.path(),
        base(Pdf::default().stream(5, "/Filter /FlateDecode", &bomb)).build(),
    )
    .await
    .unwrap();
    assert!(
        read["warnings"]
            .to_string()
            .contains("corrupt or oversized Flate stream")
    );
}

#[tokio::test]
async fn documents_without_text_or_with_odd_structure_are_reported_not_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let scanned = Pdf::default()
        .object(1, "<< /Type /Catalog /Pages 2 0 R >>")
        .object(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>")
        .object(3, "<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>")
        .stream(4, "", b"q 600 0 0 800 0 0 cm /Im0 Do Q");
    let read = read_bytes(dir.path(), scanned.build()).await.unwrap();
    assert_eq!(read["text"], "");
    assert!(
        read["warnings"]
            .to_string()
            .contains("no extractable text on pages [1]")
    );

    // A page tree that points back at itself terminates, and a missing /Contents is an empty page.
    let cycle = Pdf::default()
        .object(1, "<< /Type /Catalog /Pages 2 0 R >>")
        .object(2, "<< /Type /Pages /Kids [2 0 R 3 0 R] /Count 1 >>")
        .object(3, "<< /Type /Page /Parent 2 0 R >>");
    assert_eq!(
        read_bytes(dir.path(), cycle.build()).await.unwrap()["total_pages"],
        1
    );

    // Resources are inherited from the page tree.
    let inherited = Pdf::default()
        .object(1, "<< /Type /Catalog /Pages 2 0 R >>")
        .object(
            2,
            "<< /Type /Pages /Kids [3 0 R] /Count 1 /Resources << /Font << /F1 4 0 R >> >> >>",
        )
        .object(3, "<< /Type /Page /Parent 2 0 R /Contents 5 0 R >>")
        .object(4, HELVETICA)
        .stream(5, "", b"BT /F1 12 Tf (inherited) Tj ET");
    assert_eq!(
        read_bytes(dir.path(), inherited.build()).await.unwrap()["text"],
        "inherited"
    );

    // Several content streams on one page read in order; a later definition of an object wins.
    let split = Pdf::default()
        .object(1, "<< /Type /Catalog /Pages 2 0 R >>")
        .object(2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>")
        .object(3, "<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 4 0 R >> >> /Contents [5 0 R 6 0 R] >>")
        .object(4, HELVETICA)
        .stream(5, "", b"BT /F1 12 Tf (old) Tj ET")
        .stream(6, "", b"BT /F1 12 Tf 0 -20 Td (second) Tj ET")
        .stream(5, "", b"BT /F1 12 Tf (first) Tj ET");
    assert_eq!(
        read_bytes(dir.path(), split.build()).await.unwrap()["text"],
        "first\nsecond"
    );

    let titled = page(HELVETICA, "BT /F1 12 Tf (x) Tj ET")
        .object(
            6,
            "<< /Title <FEFF00480069D83DDE00> /Author (Ana) /Producer (caf\\351) >>",
        )
        .trailer("<< /Root 1 0 R /Info 6 0 R >>");
    let read = read_bytes(dir.path(), titled.build()).await.unwrap();
    assert_eq!(
        (
            read["metadata"]["title"].as_str(),
            read["metadata"]["author"].as_str(),
            read["metadata"]["producer"].as_str()
        ),
        (Some("Hi😀"), Some("Ana"), Some("café"))
    );
}

#[tokio::test]
async fn hostile_and_broken_pdfs_fail_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    for (label, bytes, expected) in [
        ("not a pdf", b"hello world".to_vec(), "not a PDF"),
        ("empty", vec![], "not a PDF"),
        (
            "encrypted",
            page(HELVETICA, "BT ET")
                .object(9, "<< /Filter /Standard /V 1 >>")
                .trailer("<< /Root 1 0 R /Encrypt 9 0 R >>")
                .build(),
            "encrypted",
        ),
        (
            "no catalog",
            Pdf::default()
                .object(1, "<< /Foo /Bar >>")
                .trailer("<< /Size 2 >>")
                .build(),
            "no document catalog",
        ),
        (
            "root is not a catalog",
            Pdf::default().object(1, "<< /Foo /Bar >>").build(),
            "no pages",
        ),
        (
            "no pages",
            Pdf::default()
                .object(1, "<< /Type /Catalog /Pages 2 0 R >>")
                .object(2, "<< /Type /Pages /Kids [] /Count 0 >>")
                .build(),
            "no pages",
        ),
    ] {
        let error = message(read_bytes(dir.path(), bytes).await.expect_err(label));
        assert!(error.contains(expected), "{label}: {error}");
    }
    // Truncated or corrupted real files never panic: they read what they can or fail.
    let whole = std::fs::read(PathBuf::from(FIXTURES).join("pdf-reportlab.pdf")).unwrap();
    for cut in [10, 200, whole.len() / 2, whole.len() - 30] {
        let _ = read_bytes(dir.path(), whole[..cut].to_vec()).await;
    }
    let mut damaged = whole.clone();
    for at in (100..damaged.len()).step_by(37) {
        damaged[at] ^= 0x5a;
    }
    let _ = read_bytes(dir.path(), damaged).await;
    let deep = format!("{}{}", "[".repeat(5000), "]".repeat(5000));
    let nested = Pdf::default()
        .object(1, "<< /Type /Catalog /Pages 2 0 R >>")
        .object(2, &format!("<< /Type /Pages /Kids [] /X {deep} >>"));
    assert!(read_bytes(dir.path(), nested.build()).await.is_err());
    let loop_pages = Pdf::default()
        .object(1, "<< /Type /Catalog /Pages 2 0 R >>")
        .object(2, "<< /Type /Pages /Kids [3 0 R] >>")
        .object(3, "<< /Type /Pages /Kids [2 0 R] >>");
    assert!(
        message(
            read_bytes(dir.path(), loop_pages.build())
                .await
                .unwrap_err()
        )
        .contains("no pages")
    );
}

#[tokio::test]
async fn page_ranges_and_input_limits_are_enforced() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({"max_input_bytes":3000}));
    fixture(dir.path(), "pdf-reportlab.pdf");
    fixture(dir.path(), "pdf-chrome.pdf");
    let second = call(
        &tools,
        "office_read_pdf",
        json!({"path":"pdf-reportlab.pdf","start_page":2}),
    )
    .await
    .unwrap();
    assert_eq!(
        (
            second["pages"].as_array().unwrap().len(),
            second["total_pages"].clone()
        ),
        (1, json!(2))
    );
    let clamped = call(
        &tools,
        "office_read_pdf",
        json!({"path":"pdf-reportlab.pdf","end_page":99}),
    )
    .await
    .unwrap();
    assert_eq!(
        clamped["pages"].as_array().unwrap().len(),
        2,
        "an end past the last page is clamped"
    );
    assert!(
        message(
            call(
                &tools,
                "office_read_pdf",
                json!({"path":"pdf-reportlab.pdf","start_page":3})
            )
            .await
            .unwrap_err()
        )
        .contains("page range is empty")
    );
    assert!(
        message(
            call(
                &tools,
                "office_read_pdf",
                json!({"path":"pdf-reportlab.pdf","start_page":2,"end_page":1})
            )
            .await
            .unwrap_err()
        )
        .contains("page range is empty")
    );
    assert!(
        call(
            &tools,
            "office_read_pdf",
            json!({"path":"pdf-reportlab.pdf","start_page":0})
        )
        .await
        .is_err(),
        "pages are 1-indexed"
    );
    assert!(
        message(
            call(&tools, "office_read_pdf", json!({"path":"pdf-chrome.pdf"}))
                .await
                .unwrap_err()
        )
        .contains("byte limit")
    );

    // More than 200 pages in one request is refused; 200 are fine.
    let many = (0..1700)
        .map(|n| format!("linea {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let wide = self::tools(dir.path(), json!({}));
    call(
        &wide,
        "office_write_pdf",
        json!({"path":"many.pdf","content":many,"font_size":72,"margin":10}),
    )
    .await
    .unwrap();
    let all = call(&wide, "office_read_pdf", json!({"path":"many.pdf"})).await;
    assert!(message(all.unwrap_err()).contains("at most 200 pages"));
    let first = call(
        &wide,
        "office_read_pdf",
        json!({"path":"many.pdf","end_page":200}),
    )
    .await
    .unwrap();
    assert_eq!(first["pages"].as_array().unwrap().len(), 200);
}

// ---- styled generation -----------------------------------------------------------

fn tool_ok(name: &str) -> bool {
    std::process::Command::new(name).arg("-v").output().is_ok()
}
fn run_tool(dir: &Path, tool: &str, args: &[&str]) -> String {
    let output = std::process::Command::new(tool)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{tool}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}
fn long_report() -> Value {
    let rows: Vec<Value> = std::iter::once(json!(["Concepto", "Unidades", "Importe"]))
        .chain((1..=80).map(|n| json!([format!("Artículo número {n} con una descripción bastante larga que obliga a envolver la celda"), n.to_string(), format!("{}.50 €", n * 3)])))
        .collect();
    json!({"path":"r.pdf","title":"Informe trimestral","author":"Equipo Hive","theme":{"preset":"corporate"},
        "header":{"left":"{title}","right":"Confidencial","rule":true},"footer":{"center":"Página {page} de {pages}"},
        "blocks":[
            {"type":"heading","level":1,"text":"Resumen"},
            {"type":"paragraph","align":"justify","runs":[{"text":"Las ventas crecieron "},{"text":"un 12%","bold":true,"color":"B91C1C"},{"text":" gracias a "},{"text":"la documentación","italic":true},{"text":" en "},{"text":"hive.dev","link":"https://hive.dev/docs"},{"text":". "},{"text":"Texto de relleno que se repite para que el párrafo ocupe varias líneas y compruebe el ajuste. ".repeat(6)}]},
            {"type":"list","style":"numbered","items":["Primero",{"text":"Segundo","items":["Detalle A","Detalle B"]},"Tercero"]},
            {"type":"list","items":["Viñeta uno","Viñeta dos"]},
            {"type":"quote","text":"La simplicidad es la sofisticación suprema.","author":"Leonardo"},
            {"type":"code","text":"fn main() {\n    println!(\"hola\");\n}"},
            {"type":"rule"},
            {"type":"heading","level":2,"text":"Detalle"},
            {"type":"table","zebra":true,"align":["left","right","right"],"columns":[6,1,1.4],"rows":rows},
            {"type":"page_break"},
            {"type":"heading","level":2,"text":"Anexo"},
            {"type":"paragraph","text":"Fin del documento ✓ con símbolo fuera de WinAnsi."}]})
}

#[tokio::test]
async fn styled_documents_paginate_repeat_table_headers_and_number_pages() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    let written = call(&tools, "office_write_pdf", long_report())
        .await
        .unwrap();
    let pages = written["pages"].as_u64().unwrap();
    assert!(pages >= 4, "a long table spans pages: {pages}");
    assert_eq!(
        written["replaced_characters"], 1,
        "the check mark is the only non-WinAnsi character"
    );
    let read = text_of(&tools, "r.pdf").await;
    let all = read["pages"].as_array().unwrap();
    assert_eq!(all.len() as u64, pages);
    for (index, page) in all.iter().enumerate() {
        let text = page["text"].as_str().unwrap();
        assert!(
            text.contains(&format!("Página {} de {}", index + 1, pages)),
            "page {} footer in:\n{text}",
            index + 1
        );
        assert!(
            text.contains("Informe trimestral") && text.contains("Confidencial"),
            "header on page {}",
            index + 1
        );
    }
    let table_pages: Vec<&Value> = all
        .iter()
        .filter(|page| page["text"].as_str().unwrap().contains("Artículo número"))
        .collect();
    assert!(table_pages.len() >= 2);
    for page in &table_pages {
        assert!(
            page["text"].as_str().unwrap().contains("Concepto"),
            "the header row repeats on every table page"
        );
    }
    let joined = read["text"].as_str().unwrap();
    for expected in [
        "Resumen",
        "un 12%",
        "Detalle A",
        "Viñeta dos",
        "Leonardo",
        "println!",
        "Anexo",
        "con símbolo fuera de WinAnsi.",
    ] {
        assert!(joined.contains(expected), "missing `{expected}`");
    }
    assert!(
        joined.contains("1.") && joined.contains("\u{2022}") && joined.contains("\u{2013}"),
        "list markers are real bullets and dashes, not `?`: {joined}"
    );
    assert_eq!(read["metadata"]["title"], "Informe trimestral");
    assert_eq!(read["metadata"]["author"], "Equipo Hive");
    let last = joined
        .rfind("Artículo número 80")
        .expect("the last row is present");
    assert!(
        joined[..last].contains("Artículo número 79"),
        "rows stay in order"
    );
}

#[tokio::test]
async fn poppler_agrees_on_fonts_links_layout_and_rendering() {
    if !tool_ok("pdftotext") || !tool_ok("pdffonts") {
        eprintln!("skipped: poppler is not installed");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    let written = call(&tools, "office_write_pdf", long_report())
        .await
        .unwrap();
    let info = run_tool(dir.path(), "pdfinfo", &["r.pdf"]);
    assert!(
        info.contains(&format!("Pages:           {}", written["pages"]))
            && info.contains("Title:           Informe trimestral"),
        "{info}"
    );
    let fonts = run_tool(dir.path(), "pdffonts", &["r.pdf"]);
    for face in [
        "Helvetica",
        "Helvetica-Bold",
        "Helvetica-Oblique",
        "Times-Bold",
        "Courier",
    ] {
        assert!(fonts.contains(face), "{face} missing from:\n{fonts}");
    }
    let text = run_tool(dir.path(), "pdftotext", &["-layout", "r.pdf", "-"]);
    for expected in [
        "Informe trimestral",
        "Las ventas crecieron un 12%",
        "Página 1 de",
        "Concepto",
        "Artículo número 80",
        "La simplicidad es la sofisticación suprema.",
    ] {
        assert!(text.contains(expected), "poppler did not see `{expected}`");
    }
    // Wrapping uses real font metrics: no line is drawn past the right margin.
    let boxes = run_tool(dir.path(), "pdftotext", &["-bbox", "r.pdf", "-"]);
    let right_edge = 595.28 - 50.0 + 1.0;
    for word in boxes.lines().filter(|line| line.contains("<word")) {
        let x_max: f64 = word
            .split("xMax=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert!(x_max <= right_edge, "text crosses the margin: {word}");
    }
    // The hyperlink is a real annotation.
    let raw = std::fs::read(dir.path().join("r.pdf")).unwrap();
    assert!(String::from_utf8_lossy(&raw).contains("/URI (https://hive.dev/docs)"));
    if tool_ok("pdftoppm") {
        run_tool(
            dir.path(),
            "pdftoppm",
            &["-r", "40", "-png", "-f", "1", "-l", "1", "r.pdf", "page"],
        );
        let png = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .find(|e| e.file_name().to_string_lossy().starts_with("page"))
            .expect("a rendered page");
        let bytes = std::fs::read(png.path()).unwrap();
        assert!(
            bytes.starts_with(b"\x89PNG") && bytes.len() > 2000,
            "the page renders to a non-blank image"
        );
    }
}

#[tokio::test]
async fn themes_fonts_landscape_and_bad_blocks_are_handled() {
    let dir = tempfile::tempdir().unwrap();
    let tools = tools(dir.path(), json!({}));
    let small = |extra: Value| {
        let mut base = json!({"path":"t.pdf","blocks":[{"type":"paragraph","text":"Hola mundo"}]});
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().cloned().unwrap());
        base
    };
    for preset in ["modern", "classic", "corporate", "minimal"] {
        call(
            &tools,
            "office_write_pdf",
            small(json!({"theme":{"preset":preset}})),
        )
        .await
        .unwrap();
        assert!(
            text_of(&tools, "t.pdf").await["text"]
                .as_str()
                .unwrap()
                .contains("Hola mundo"),
            "{preset}"
        );
    }
    call(&tools, "office_write_pdf", small(json!({"landscape":true,"page_size":"Letter","theme":{"fonts":{"body":"mono"},"colors":{"text":"112233"}}}))).await.unwrap();
    let raw = std::fs::read(dir.path().join("t.pdf")).unwrap();
    let raw = String::from_utf8_lossy(&raw);
    assert!(raw.contains("/MediaBox [0 0 792.00 612.00]") && raw.contains("/BaseFont /Courier"));
    let wide = call(&tools, "office_write_pdf", json!({"path":"w.pdf","blocks":[{"type":"paragraph","text":"x".repeat(5000)},{"type":"table","rows":[["a","b"],["c".repeat(300),"d"]]}]})).await.unwrap();
    assert!(
        wide["pages"].as_u64().unwrap() >= 1,
        "overlong words are broken instead of overflowing"
    );
    for (extra, expected) in [
        (json!({"blocks":[{"type":"bogus"}]}), "do not match schema"),
        (json!({"theme":{"preset":"neon"}}), "do not match schema"),
        (
            json!({"theme":{"colors":{"text":"red"}}}),
            "do not match schema",
        ),
        (
            json!({"blocks":[{"type":"paragraph","runs":[{"text":"x","link":"javascript:alert(1)"}]}]}),
            "http(s)",
        ),
        (json!({"blocks":[{"type":"table","rows":[]}]}), "rows"),
        (
            json!({"blocks":[{"type":"table","columns":[1],"rows":[["a","b"]]}]}),
            "one weight per column",
        ),
        (
            json!({"header":{"left":"x"},"margin":5}),
            "margin is too small",
        ),
        (
            json!({"blocks":vec![json!({"type":"page_break"}); 600]}),
            "500 pages",
        ),
    ] {
        let error = message(
            call(&tools, "office_write_pdf", small(extra.clone()))
                .await
                .unwrap_err(),
        );
        assert!(error.contains(expected), "{extra}: {error}");
    }
    assert!(!dir.path().join("bad.pdf").exists());
}
