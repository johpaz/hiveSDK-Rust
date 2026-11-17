//! Word (.docx): text and tables out, headings, lists, tables, a header and a footer in.
//!
//! A header or footer is one paragraph of text, aligned left, centre or right, in which `{page}`,
//! `{pages}` and `{title}` become the page number, the page count (live fields Word updates) and
//! the document title. Reading returns the header and footer texts with the fields written back as
//! `{page}` and `{pages}`; complex fields and headers of other sections or first pages are not read.
use super::{
    Deadline, invalid,
    xml::{self, Event, attr, escape},
    zip::{Archive, Writer},
};
use crate::Result;
use serde_json::{Value, json};

const MAX_TEXT_CHARS: usize = 4_000_000;
const MAX_PARAGRAPHS: usize = 10_000;
const MAX_TABLES: usize = 200;
const MAX_ROWS: usize = 2_000;
const MAX_COLUMNS: usize = 64;
const MAX_RUN_CHARS: usize = 200_000;

/// A table being read: finished rows, the current row and the current cell's paragraphs.
#[derive(Default)]
struct OpenTable {
    rows: Vec<Vec<String>>,
    row: Vec<String>,
    cell: Vec<String>,
}

struct Paragraph {
    text: String,
    style: Option<String>,
}

fn flush_text(blocks: &mut Vec<String>, total: &mut usize, line: String) -> Result<()> {
    *total = total.saturating_add(line.chars().count() + 1);
    if *total > MAX_TEXT_CHARS {
        return Err(invalid("document text exceeds the size limit"));
    }
    blocks.push(line);
    Ok(())
}

pub(super) fn read(bytes: &[u8], include_tables: bool, deadline: &Deadline) -> Result<Value> {
    let archive = Archive::open(bytes)?;
    if !archive.contains("word/document.xml") {
        return Err(invalid(
            "not a Word document (word/document.xml is missing)",
        ));
    }
    let events = xml::parse(&archive.read_text("word/document.xml")?)?;
    let (mut paragraphs, mut tables) = (Vec::new(), Vec::new());
    let (mut lines, mut total) = (Vec::new(), 0usize);
    // Open tables: each has finished rows, the current row and the current cell's paragraphs.
    let mut open: Vec<OpenTable> = Vec::new();
    let mut current: Option<Paragraph> = None;
    let mut in_text = false;
    for (index, event) in events.iter().enumerate() {
        if index % 4096 == 0 {
            deadline.check("reading the document")?;
        }
        match event {
            Event::Start { name, .. } if name == "w:tbl" => open.push(OpenTable::default()),
            Event::Start { name, .. } if name == "w:tr" => {
                if let Some(table) = open.last_mut() {
                    table.row.clear();
                }
            }
            Event::Start { name, .. } if name == "w:tc" => {
                if let Some(table) = open.last_mut() {
                    table.cell.clear();
                }
            }
            Event::End { name } if name == "w:tc" => {
                if let Some(table) = open.last_mut() {
                    let cell = table.cell.join("\n");
                    table.row.push(cell);
                }
            }
            Event::End { name } if name == "w:tr" => {
                if let Some(table) = open.last_mut() {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            Event::End { name } if name == "w:tbl" => {
                let rows = open.pop().unwrap_or_default().rows;
                if let Some(parent) = open.last_mut() {
                    // A nested table becomes plain text of the enclosing cell.
                    for row in &rows {
                        parent.cell.push(row.join("\t"));
                    }
                } else if include_tables {
                    for row in &rows {
                        flush_text(&mut lines, &mut total, row.join("\t"))?;
                    }
                    tables.push(rows);
                }
            }
            Event::Start { name, .. } if name == "w:p" => {
                current = Some(Paragraph {
                    text: String::new(),
                    style: None,
                })
            }
            Event::Empty { name, attrs } if name == "w:pStyle" => {
                if let Some(paragraph) = &mut current {
                    paragraph.style = attr(attrs, "w:val").map(str::to_owned);
                }
            }
            Event::Start { name, .. } if name == "w:t" => in_text = true,
            Event::End { name } if name == "w:t" => in_text = false,
            Event::Text(text) if in_text => {
                if let Some(paragraph) = &mut current {
                    paragraph.text.push_str(text);
                }
            }
            Event::Empty { name, .. } | Event::Start { name, .. } if name == "w:tab" => {
                if let Some(paragraph) = &mut current {
                    paragraph.text.push('\t');
                }
            }
            Event::Empty { name, .. } | Event::Start { name, .. }
                if name == "w:br" || name == "w:cr" =>
            {
                if let Some(paragraph) = &mut current {
                    paragraph.text.push('\n');
                }
            }
            Event::End { name } if name == "w:p" => {
                if let Some(paragraph) = current.take() {
                    if let Some(table) = open.last_mut() {
                        table.cell.push(paragraph.text);
                    } else {
                        flush_text(&mut lines, &mut total, paragraph.text.clone())?;
                        paragraphs.push(json!({"text":paragraph.text,"style":paragraph.style}));
                    }
                }
            }
            _ => {}
        }
    }
    let mut result = json!({"text":lines.join("\n"),"paragraphs":paragraphs,"tables":tables});
    for (key, prefix) in [("headers", "word/header"), ("footers", "word/footer")] {
        let mut names: Vec<&str> = archive
            .names()
            .filter(|n| n.starts_with(prefix) && n.ends_with(".xml"))
            .collect();
        names.sort_unstable();
        let mut texts = vec![];
        for name in names.into_iter().take(8) {
            let text = part_text(&archive.read_text(name)?)?;
            if !text.is_empty() {
                texts.push(text);
            }
        }
        if !texts.is_empty() {
            result[key] = json!(texts);
        }
    }
    Ok(result)
}

/// The text of a header or footer part: paragraphs joined by newlines, page fields as `{page}` / `{pages}`.
fn part_text(xml_text: &str) -> Result<String> {
    let (mut out, mut in_text, mut skip_field) = (String::new(), false, false);
    for event in xml::parse(xml_text)? {
        match event {
            Event::Start { name, attrs } if name == "w:fldSimple" => {
                let instr = attr(&attrs, "w:instr")
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_uppercase();
                if instr.starts_with("PAGE") {
                    out.push_str("{page}");
                } else if instr.starts_with("NUMPAGES") {
                    out.push_str("{pages}");
                }
                skip_field = true;
            }
            Event::End { name } if name == "w:fldSimple" => skip_field = false,
            Event::Start { name, .. } if name == "w:t" => in_text = true,
            Event::End { name } if name == "w:t" => in_text = false,
            Event::Text(text) if in_text && !skip_field => out.push_str(&text),
            Event::Empty { name, .. } | Event::Start { name, .. } if name == "w:tab" => {
                out.push('\t')
            }
            Event::End { name } if name == "w:p" => out.push('\n'),
            _ => {}
        }
        if out.chars().count() > 20_000 {
            return Err(invalid("header or footer text is too long"));
        }
    }
    Ok(out.trim_matches('\n').to_owned())
}

fn text_runs(text: &str, bold: bool, italic: bool) -> String {
    let properties = format!(
        "{}{}",
        if bold { "<w:b/>" } else { "" },
        if italic { "<w:i/>" } else { "" }
    );
    let properties = if properties.is_empty() {
        String::new()
    } else {
        format!("<w:rPr>{properties}</w:rPr>")
    };
    let mut out = String::new();
    for (line_number, line) in text.split('\n').enumerate() {
        for (tab_number, piece) in line.split('\t').enumerate() {
            out.push_str(&format!("<w:r>{properties}"));
            if line_number > 0 && tab_number == 0 {
                out.push_str("<w:br/>");
            }
            if tab_number > 0 {
                out.push_str("<w:tab/>");
            }
            out.push_str(&format!(
                "<w:t xml:space=\"preserve\">{}</w:t></w:r>",
                escape(piece)
            ));
        }
    }
    out
}

fn paragraph(text: &str, style: Option<&str>, list: bool, bold: bool, italic: bool) -> String {
    let mut properties = String::new();
    if let Some(style) = style {
        properties.push_str(&format!("<w:pStyle w:val=\"{style}\"/>"));
    }
    if list {
        properties.push_str("<w:numPr><w:ilvl w:val=\"0\"/><w:numId w:val=\"1\"/></w:numPr>");
    }
    let properties = if properties.is_empty() {
        String::new()
    } else {
        format!("<w:pPr>{properties}</w:pPr>")
    };
    format!("<w:p>{properties}{}</w:p>", text_runs(text, bold, italic))
}

fn table(input: &Value) -> Result<String> {
    let rows: Vec<Vec<String>> = input["rows"]
        .as_array()
        .ok_or_else(|| invalid("a table needs rows"))?
        .iter()
        .map(|row| {
            row["cells"]
                .as_array()
                .map(|cells| {
                    cells
                        .iter()
                        .map(|cell| cell.as_str().unwrap_or_default().to_owned())
                        .collect()
                })
                .ok_or_else(|| invalid("each table row needs cells"))
        })
        .collect::<Result<_>>()?;
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if rows.is_empty() || columns == 0 {
        return Err(invalid("a table needs at least one row and one cell"));
    }
    if rows.len() > MAX_ROWS || columns > MAX_COLUMNS {
        return Err(invalid("table exceeds the row or column limit"));
    }
    let header = input["header"].as_bool() == Some(true);
    let width = 9360 / columns;
    let mut out = String::from(
        "<w:tbl><w:tblPr><w:tblStyle w:val=\"TableGrid\"/><w:tblW w:w=\"9360\" w:type=\"dxa\"/></w:tblPr><w:tblGrid>",
    );
    out.push_str(&format!("<w:gridCol w:w=\"{width}\"/>").repeat(columns));
    out.push_str("</w:tblGrid>");
    for (index, row) in rows.iter().enumerate() {
        let bold = header && index == 0;
        out.push_str("<w:tr>");
        if bold {
            out.push_str("<w:trPr><w:tblHeader/></w:trPr>");
        }
        for column in 0..columns {
            let text = row.get(column).map_or("", String::as_str);
            out.push_str(&format!(
                "<w:tc><w:tcPr><w:tcW w:w=\"{width}\" w:type=\"dxa\"/></w:tcPr>{}</w:tc>",
                paragraph(text, None, false, bold, false)
            ));
        }
        out.push_str("</w:tr>");
    }
    // Word wants a paragraph after a table, and it keeps neighbouring tables apart.
    out.push_str("</w:tbl><w:p/>");
    Ok(out)
}

const NS: &str = "xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"";
const HEADER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";

/// A header or footer part: one small grey paragraph, with live page fields.
fn running_part(spec: &Value, root: &str, title: &str) -> Result<Option<String>> {
    let Some(text) = spec["text"].as_str().filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    if text.chars().count() > 1000 {
        return Err(invalid("a header or footer is at most 1000 characters"));
    }
    let align = match spec["align"].as_str() {
        None | Some("center") => "center",
        Some("left") => "left",
        Some("right") => "right",
        Some(_) => return Err(invalid("align must be left, center or right")),
    };
    let properties = "<w:rPr><w:sz w:val=\"18\"/><w:color w:val=\"666666\"/></w:rPr>";
    let literal = |piece: &str| {
        if piece.is_empty() {
            String::new()
        } else {
            format!(
                "<w:r>{properties}<w:t xml:space=\"preserve\">{}</w:t></w:r>",
                escape(piece)
            )
        }
    };
    let field = |instr: &str| {
        format!(
            "<w:fldSimple w:instr=\" {instr} \"><w:r>{properties}<w:t>1</w:t></w:r></w:fldSimple>"
        )
    };
    let mut runs = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        let next = ["{page}", "{pages}", "{title}"]
            .iter()
            .filter_map(|t| rest.find(t).map(|at| (at, *t)))
            .min();
        match next {
            None => {
                runs.push_str(&literal(rest));
                break;
            }
            Some((at, token)) => {
                runs.push_str(&literal(&rest[..at]));
                runs.push_str(&match token {
                    "{page}" => field("PAGE"),
                    "{pages}" => field("NUMPAGES"),
                    _ => literal(title),
                });
                rest = &rest[at + token.len()..];
            }
        }
    }
    Ok(Some(format!(
        "{HEADER}<w:{root} {NS}><w:p><w:pPr><w:jc w:val=\"{align}\"/></w:pPr>{runs}</w:p></w:{root}>"
    )))
}

pub(super) fn write(input: &Value) -> Result<Vec<u8>> {
    let mut body = String::new();
    let title = input["title"].as_str().filter(|title| !title.is_empty());
    if let Some(title) = title {
        body.push_str(&paragraph(title, Some("Title"), false, false, false));
    }
    let paragraphs = input["paragraphs"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let tables = input["tables"].as_array().map_or(&[][..], Vec::as_slice);
    if paragraphs.len() > MAX_PARAGRAPHS || tables.len() > MAX_TABLES {
        return Err(invalid("document exceeds the paragraph or table limit"));
    }
    for item in paragraphs {
        let text = item["text"]
            .as_str()
            .ok_or_else(|| invalid("each paragraph needs text"))?;
        if text.chars().count() > MAX_RUN_CHARS {
            return Err(invalid("paragraph text is too long"));
        }
        let kind = item["type"].as_str().unwrap_or("paragraph");
        let (style, list) = match kind {
            "title1" => (Some("Heading1"), false),
            "title2" => (Some("Heading2"), false),
            "title3" => (Some("Heading3"), false),
            "list" => (Some("ListParagraph"), true),
            "paragraph" => (None, false),
            other => return Err(invalid(&format!("unknown paragraph type `{other}`"))),
        };
        body.push_str(&paragraph(
            text,
            style,
            list,
            item["bold"].as_bool() == Some(true),
            item["italic"].as_bool() == Some(true),
        ));
    }
    for item in tables {
        body.push_str(&table(item)?);
    }
    if body.is_empty() {
        body.push_str("<w:p/>");
    }
    let title_text = title.unwrap_or_default();
    let header_part = running_part(&input["header"], "hdr", title_text)?;
    let footer_part = running_part(&input["footer"], "ftr", title_text)?;
    let references = format!(
        "{}{}",
        if header_part.is_some() {
            "<w:headerReference w:type=\"default\" r:id=\"rId3\"/>"
        } else {
            ""
        },
        if footer_part.is_some() {
            "<w:footerReference w:type=\"default\" r:id=\"rId4\"/>"
        } else {
            ""
        }
    );
    let document = format!(
        "{HEADER}<w:document {NS} xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><w:body>{body}<w:sectPr>{references}<w:pgSz w:w=\"12240\" w:h=\"15840\"/><w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\" w:header=\"720\" w:footer=\"720\" w:gutter=\"0\"/></w:sectPr></w:body></w:document>"
    );
    let heading = |id: &str, name: &str, size: u32| {
        format!(
            "<w:style w:type=\"paragraph\" w:styleId=\"{id}\"><w:name w:val=\"{name}\"/><w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:qFormat/><w:pPr><w:keepNext/><w:spacing w:before=\"240\" w:after=\"80\"/></w:pPr><w:rPr><w:b/><w:sz w:val=\"{size}\"/></w:rPr></w:style>"
        )
    };
    let styles = format!(
        "{HEADER}<w:styles {NS}><w:docDefaults><w:rPrDefault><w:rPr><w:rFonts w:ascii=\"Calibri\" w:hAnsi=\"Calibri\" w:cs=\"Calibri\"/><w:sz w:val=\"22\"/></w:rPr></w:rPrDefault><w:pPrDefault><w:pPr><w:spacing w:after=\"120\" w:line=\"264\" w:lineRule=\"auto\"/></w:pPr></w:pPrDefault></w:docDefaults><w:style w:type=\"paragraph\" w:default=\"1\" w:styleId=\"Normal\"><w:name w:val=\"Normal\"/><w:qFormat/></w:style>{}{}{}{}<w:style w:type=\"paragraph\" w:styleId=\"ListParagraph\"><w:name w:val=\"List Paragraph\"/><w:basedOn w:val=\"Normal\"/><w:qFormat/><w:pPr><w:ind w:left=\"720\"/></w:pPr></w:style><w:style w:type=\"table\" w:default=\"1\" w:styleId=\"TableNormal\"><w:name w:val=\"Normal Table\"/><w:tblPr><w:tblInd w:w=\"0\" w:type=\"dxa\"/><w:tblCellMar><w:top w:w=\"0\" w:type=\"dxa\"/><w:left w:w=\"108\" w:type=\"dxa\"/><w:bottom w:w=\"0\" w:type=\"dxa\"/><w:right w:w=\"108\" w:type=\"dxa\"/></w:tblCellMar></w:tblPr></w:style><w:style w:type=\"table\" w:styleId=\"TableGrid\"><w:name w:val=\"Table Grid\"/><w:basedOn w:val=\"TableNormal\"/><w:tblPr><w:tblBorders><w:top w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/><w:left w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/><w:bottom w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/><w:right w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/><w:insideH w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/><w:insideV w:val=\"single\" w:sz=\"4\" w:space=\"0\" w:color=\"auto\"/></w:tblBorders></w:tblPr></w:style></w:styles>",
        "<w:style w:type=\"paragraph\" w:styleId=\"Title\"><w:name w:val=\"Title\"/><w:basedOn w:val=\"Normal\"/><w:next w:val=\"Normal\"/><w:qFormat/><w:pPr><w:spacing w:after=\"240\"/></w:pPr><w:rPr><w:b/><w:sz w:val=\"48\"/></w:rPr></w:style>",
        heading("Heading1", "heading 1", 36),
        heading("Heading2", "heading 2", 30),
        heading("Heading3", "heading 3", 26),
    );
    let numbering = format!(
        "{HEADER}<w:numbering {NS}><w:abstractNum w:abstractNumId=\"0\"><w:multiLevelType w:val=\"hybridMultilevel\"/><w:lvl w:ilvl=\"0\"><w:start w:val=\"1\"/><w:numFmt w:val=\"bullet\"/><w:lvlText w:val=\"&#8226;\"/><w:lvlJc w:val=\"left\"/><w:pPr><w:ind w:left=\"720\" w:hanging=\"360\"/></w:pPr></w:lvl></w:abstractNum><w:num w:numId=\"1\"><w:abstractNumId w:val=\"0\"/></w:num></w:numbering>"
    );
    let core_title = title.map(escape).unwrap_or_default();
    let mut zip = Writer::default();
    zip.add("[Content_Types].xml", format!("{HEADER}<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/><Override PartName=\"/word/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml\"/><Override PartName=\"/word/numbering.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.numbering+xml\"/><Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>{}{}</Types>", if header_part.is_some() { "<Override PartName=\"/word/header1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.header+xml\"/>" } else { "" }, if footer_part.is_some() { "<Override PartName=\"/word/footer1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.footer+xml\"/>" } else { "" }).as_bytes())?;
    zip.add("_rels/.rels", format!("{HEADER}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/><Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/></Relationships>").as_bytes())?;
    zip.add("docProps/core.xml", format!("{HEADER}<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:title>{core_title}</dc:title></cp:coreProperties>").as_bytes())?;
    zip.add("word/_rels/document.xml.rels", format!("{HEADER}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/><Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/numbering\" Target=\"numbering.xml\"/>{}{}</Relationships>", if header_part.is_some() { "<Relationship Id=\"rId3\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/header\" Target=\"header1.xml\"/>" } else { "" }, if footer_part.is_some() { "<Relationship Id=\"rId4\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/footer\" Target=\"footer1.xml\"/>" } else { "" }).as_bytes())?;
    if let Some(part) = &header_part {
        zip.add("word/header1.xml", part.as_bytes())?;
    }
    if let Some(part) = &footer_part {
        zip.add("word/footer1.xml", part.as_bytes())?;
    }
    zip.add("word/document.xml", document.as_bytes())?;
    zip.add("word/styles.xml", styles.as_bytes())?;
    zip.add("word/numbering.xml", numbering.as_bytes())?;
    zip.finish()
}
