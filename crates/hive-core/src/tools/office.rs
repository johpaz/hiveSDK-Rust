//! Word, Excel and PowerPoint tools for the filesystem sandbox.
//!
//! Documents are read and written as OOXML containers by this crate alone: a
//! bounded ZIP reader/writer, a strict XML tokenizer and one module per format.
//! Inputs are untrusted files, so size, entry count, expansion ratio, row and
//! sheet counts and processing time are all capped. Legacy binary formats
//! (.doc, .xls, .ppt), macros, embedded objects and PDF are not handled.
mod docx;
mod pdf_crypt;
mod pdf_metrics;
mod pdf_read;
mod pdf_write;
mod pptx;
mod xlsx;
mod xml;
mod zip;

use super::{Inner, fs, invalid as input_error};
use crate::{Error, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

pub(super) fn invalid(message: &str) -> Error {
    input_error(message)
}

fn default_input() -> usize {
    15 * 1024 * 1024
}
fn default_sheets() -> usize {
    50
}
fn default_rows() -> usize {
    10_000
}
fn default_timeout() -> u64 {
    30_000
}

/// Limits for the `office_*` tools; the tools exist only when this is configured.
#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OfficePolicy {
    pub max_input_bytes: usize,
    pub max_sheets: usize,
    pub max_rows_per_sheet: usize,
    pub timeout_ms: u64,
}

impl Default for OfficePolicy {
    fn default() -> Self {
        Self {
            max_input_bytes: default_input(),
            max_sheets: default_sheets(),
            max_rows_per_sheet: default_rows(),
            timeout_ms: default_timeout(),
        }
    }
}

impl OfficePolicy {
    pub(super) fn validate(&self) -> Result<()> {
        if !(1..=64 * 1024 * 1024).contains(&self.max_input_bytes)
            || !(1..=1_000).contains(&self.max_sheets)
            || !(1..=1_000_000).contains(&self.max_rows_per_sheet)
            || !(1..=300_000).contains(&self.timeout_ms)
        {
            return Err(invalid("office limits are out of range"));
        }
        Ok(())
    }
}

/// Cooperative processing deadline checked between large steps.
pub(super) struct Deadline(Instant);

impl Deadline {
    fn new(timeout_ms: u64) -> Self {
        Self(Instant::now() + Duration::from_millis(timeout_ms))
    }
    pub(super) fn check(&self, what: &str) -> Result<()> {
        if Instant::now() > self.0 {
            return Err(invalid(&format!(
                "{what} exceeded the processing time limit"
            )));
        }
        Ok(())
    }
}

/// The embedded picture of each page of a scanned PDF, for a host that shows pages to a model that reads
/// pictures but not PDF. It renders nothing: a page whose content is drawn rather than scanned has no
/// picture and is reported as such.
pub fn pdf_page_images(
    bytes: &[u8],
    first: usize,
    last: usize,
    password: Option<&str>,
    timeout_ms: u64,
) -> Result<PdfPageImages> {
    pdf_read::page_images(bytes, first, last, password, &Deadline::new(timeout_ms))
}
pub use pdf_read::{PageImage as PdfPageImage, PageImages as PdfPageImages};

pub(super) fn specs() -> Vec<(&'static str, &'static str, Value, Vec<&'static str>)> {
    let path = json!({"type":"string","maxLength":4096});
    let create_dirs = json!({"type":"boolean"});
    let text = |max: u64| json!({"type":"string","maxLength":max});
    let hex = json!({"type":"string","pattern":"^[0-9A-Fa-f]{6}$"});
    let margin = json!({"type":"number","minimum":0,"maximum":200});
    let family = json!({"type":"string","enum":["sans","serif","mono"]});
    let align = json!({"type":"string","enum":["left","center","right","justify"]});
    let band = json!({"type":"object","additionalProperties":false,"properties":{
        "left":text(500),"center":text(500),"right":text(500),"size":{"type":"number","minimum":5,"maximum":24},"color":hex.clone(),"rule":{"type":"boolean"}}});
    let run = json!({"type":"object","additionalProperties":false,"required":["text"],"properties":{
        "text":text(200000),"bold":{"type":"boolean"},"italic":{"type":"boolean"},"underline":{"type":"boolean"},"color":hex.clone(),
        "size":{"type":"number","minimum":4,"maximum":96},"font":family.clone(),"link":{"type":"string","maxLength":2048}}});
    let font = json!({"type":"string","minLength":1,"maxLength":64,"pattern":"^[A-Za-z0-9 .-]+$"});
    let mut pdf = json!({"path":path,"create_dirs":create_dirs,"content":text(2000000),"title":text(1000),"author":text(500),"subject":text(500),
        "page_size":{"type":"string","enum":["A4","Letter"]},"landscape":{"type":"boolean"},
        "margin":margin.clone(),
        "margins":{"type":"object","additionalProperties":false,"properties":{"top":margin.clone(),"right":margin.clone(),"bottom":margin.clone(),"left":margin}},
        "font_size":{"type":"number","minimum":6,"maximum":72},"line_height":{"type":"number","minimum":1,"maximum":3},
        "header":band.clone(),"footer":band});
    pdf["theme"] = json!({"type":"object","additionalProperties":false,"properties":{
        "preset":{"type":"string","enum":["modern","classic","corporate","minimal"]},
        "fonts":{"type":"object","additionalProperties":false,"properties":{"body":family.clone(),"heading":family.clone()}}}});
    pdf["theme"]["properties"]["colors"] = json!({"type":"object","additionalProperties":false,"properties":{
        "text":hex.clone(),"heading":hex.clone(),"accent":hex.clone(),"muted":hex.clone(),"table_header_background":hex.clone(),
        "table_header_text":hex.clone(),"table_border":hex.clone(),"zebra":hex.clone(),"code_background":hex.clone()}});
    let mut block = json!({"type":{"type":"string","enum":["heading","paragraph","list","table","quote","code","rule","spacer","page_break"]},
        "text":text(200000),"runs":{"type":"array","maxItems":2000,"items":run},
        "level":{"type":"integer","minimum":1,"maximum":3},"align":{"anyOf":[align.clone(),{"type":"array","maxItems":32,"items":align}]},"size":{"type":"number","minimum":4,"maximum":96},
        "bold":{"type":"boolean"},"italic":{"type":"boolean"},"color":hex.clone(),"indent":{"type":"number","minimum":0,"maximum":200},
        "space_after":{"type":"number","minimum":0,"maximum":200},"height":{"type":"number","minimum":0,"maximum":400},
        "style":{"type":"string","enum":["bullet","numbered"]},"author":text(500)});
    block["items"] = json!({"type":"array","maxItems":5000,"items":{"type":["string","object"]}});
    block["header"] = json!({"type":"boolean"});
    block["zebra"] = json!({"type":"boolean"});
    block["columns"] =
        json!({"type":"array","maxItems":32,"items":{"type":"number","exclusiveMinimum":0}});
    block["rows"] = json!({"type":"array","maxItems":5000,"items":{"type":"array","maxItems":32,"items":{"type":["string","object","null"]}}});
    pdf["blocks"] = json!({"type":"array","maxItems":10000,"items":{"type":"object","additionalProperties":false,"properties":block}});
    vec![
        (
            "office_read_docx",
            "Read a Word (.docx) file: text, paragraphs with styles, and tables.",
            json!({"path":path,"include_tables":{"type":"boolean"}}),
            vec!["path"],
        ),
        (
            "office_write_docx",
            "Create a Word (.docx) file with a title, headings, paragraphs, bullet lists and tables. `header` and `footer` ({text, align left|center|right}) repeat on every page; their text may contain {page}, {pages} and {title}.",
            json!({"path":path,"create_dirs":create_dirs,"title":text(1000),
                "header":{"type":"object","additionalProperties":false,"properties":{"text":text(1000),"align":{"type":"string","enum":["left","center","right"]}}},
                "footer":{"type":"object","additionalProperties":false,"properties":{"text":text(1000),"align":{"type":"string","enum":["left","center","right"]}}},
                "paragraphs":{"type":"array","maxItems":10000,"items":{"type":"object","additionalProperties":false,"required":["text"],"properties":{
                    "text":text(200000),"type":{"type":"string","enum":["title1","title2","title3","paragraph","list"]},
                    "bold":{"type":"boolean"},"italic":{"type":"boolean"}}}},
                "tables":{"type":"array","maxItems":200,"items":{"type":"object","additionalProperties":false,"required":["rows"],"properties":{
                    "header":{"type":"boolean"},
                    "rows":{"type":"array","minItems":1,"maxItems":2000,"items":{"type":"object","additionalProperties":false,"required":["cells"],"properties":{
                        "cells":{"type":"array","minItems":1,"maxItems":64,"items":text(200000)}}}}}}}}),
            vec!["path"],
        ),
        (
            "office_read_xlsx",
            "Read an Excel (.xlsx) file as JSON rows (first row as keys, or arrays), optionally one sheet or an A1 range. Cells with a date or time format come back as ISO text (2025-03-01, 2025-03-01T14:30:00, 14:30:00); pass dates=serial for Excel's day numbers instead.",
            json!({"path":path,"sheet":text(31),"include_headers":{"type":"boolean"},"range":text(32),"dates":{"type":"string","enum":["iso","serial"]}}),
            vec!["path"],
        ),
        (
            "office_write_xlsx",
            "Create an Excel (.xlsx) file from sheets of objects or arrays. Object columns are the sorted keys unless headers is given. Per sheet, `columns` can set a format (date, datetime, time, integer, decimal, thousands, percent, currency, text or an Excel format code; ISO strings such as 2025-03-01 or 2025-03-01T14:30:00 in a date column are stored as real dates), a width, bold, color, fill, align and wrap for a column named by its header text or index; `header_style` styles the first row; `freeze_header` pins it and `autofilter` adds filter buttons. Colors are #RRGGBB. Cells are always values: text starting with = is never a formula.",
            json!({"path":path,"create_dirs":create_dirs,
                "sheets":{"type":"array","minItems":1,"maxItems":1000,"items":{"type":"object","additionalProperties":false,"required":["name","data"],"properties":{
                    "name":text(31),"data":{"type":"array","items":{"type":["object","array"]}},
                    "headers":{"type":"array","maxItems":1000,"items":{"type":"string","maxLength":32767}},
                    "columns":{"type":"array","maxItems":1000,"items":{"type":"object","additionalProperties":false,"required":["column"],"properties":{
                        "column":{"type":["string","integer"],"minimum":0,"maximum":999},"format":text(64),"width":{"type":"number","minimum":1,"maximum":255},
                        "bold":{"type":"boolean"},"color":text(7),"fill":text(7),"align":{"type":"string","enum":["left","center","right"]},"wrap":{"type":"boolean"}}}},
                    "header_style":{"type":"object","additionalProperties":false,"properties":{"bold":{"type":"boolean"},"color":text(7),"fill":text(7),"align":{"type":"string","enum":["left","center","right"]}}},
                    "freeze_header":{"type":"boolean"},"autofilter":{"type":"boolean"}}}}}),
            vec!["path", "sheets"],
        ),
        (
            "office_read_pdf",
            "Extract text from a PDF by page, with its metadata. Opens encrypted PDFs (RC4, AES-128, AES-256) with the empty password or the user `password` you give; fonts without a Unicode mapping and scanned images are reported in warnings, not guessed, and there is no OCR.",
            json!({"path":path,"start_page":{"type":"integer","minimum":1},"end_page":{"type":"integer","minimum":1},"password":{"type":"string","maxLength":127}}),
            vec!["path"],
        ),
        (
            "office_write_pdf",
            "Create a styled PDF. Give either `content` (plain text) or `blocks`: headings (level 1-3), paragraphs (with `runs` for bold/italic/underline/color/size/font/link), lists (bullet or numbered, one nested level), tables (header row repeated across pages, zebra, column alignment/weights), quotes, code, rules, spacers and page breaks. `theme` picks a preset (modern, classic, corporate, minimal) and may override colors and fonts (sans, serif, mono). `header`/`footer` take left/center/right text with {page}, {pages} and {title}. Fonts are the standard PDF faces measured exactly, so text wraps where the viewer draws it; characters outside Western European text become ? and are reported.",
            pdf,
            vec!["path"],
        ),
        (
            "office_read_pptx",
            "Read a PowerPoint (.pptx) file: title and text of each slide.",
            json!({"path":path,"only_slide":{"type":"integer","minimum":1}}),
            vec!["path"],
        ),
        (
            "office_write_pptx",
            "Create a styled 16:9 PowerPoint (.pptx) deck: an optional cover, then slides with a title and either free text or bullet points, and optional speaker `notes` per slide. Pick a theme preset or override its colors and fonts.",
            json!({"path":path,"create_dirs":create_dirs,"presentation_title":text(1000),"presentation_subtitle":text(1000),
                "theme":{"type":"object","additionalProperties":false,"properties":{
                    "preset":{"type":"string","enum":["modern","light","dark","corporate","warm"]},
                    "colors":{"type":"object","additionalProperties":false,"properties":{
                        "background":hex.clone(),"title":hex.clone(),"text":hex.clone(),"accent":hex.clone(),"accent2":hex.clone(),
                        "cover_background":hex.clone(),"cover_text":hex.clone()}},
                    "fonts":{"type":"object","additionalProperties":false,"properties":{
                        "title":font.clone(),"body":font.clone()}}}},
                "slides":{"type":"array","minItems":1,"maxItems":500,"items":{"type":"object","additionalProperties":false,"properties":{
                    "title":text(100000),"content":text(100000),"points":{"type":"array","maxItems":100,"items":text(100000)},"notes":text(100000)}}}}),
            vec!["path", "slides"],
        ),
    ]
}

fn extension_of(name: &str) -> &str {
    name.strip_prefix("office_read_")
        .or_else(|| name.strip_prefix("office_write_"))
        .unwrap_or_default()
}

pub(super) fn run(inner: &Inner, name: &str, arguments: &Value) -> Result<Value> {
    let policy = inner
        .office
        .as_ref()
        .ok_or_else(|| invalid("office tools are disabled"))?;
    let format = extension_of(name);
    let relative = arguments["path"].as_str().unwrap_or_default();
    if !relative
        .to_ascii_lowercase()
        .ends_with(&format!(".{format}"))
    {
        return Err(invalid(&format!("path must end with .{format}")));
    }
    let deadline = Deadline::new(policy.timeout_ms);
    if name.starts_with("office_read_") {
        let path = fs::resolve(&inner.root, relative, false)?;
        let metadata = std::fs::metadata(&path).map_err(|_| invalid("path is not readable"))?;
        if !metadata.is_file() {
            return Err(invalid("path must be a regular file"));
        }
        if metadata.len() > policy.max_input_bytes as u64 {
            return Err(invalid(&format!(
                "file exceeds the {} byte limit",
                policy.max_input_bytes
            )));
        }
        let bytes = std::fs::read(&path).map_err(|_| invalid("path is not readable"))?;
        let mut result = match format {
            "docx" => docx::read(
                &bytes,
                arguments["include_tables"].as_bool() != Some(false),
                &deadline,
            )?,
            "xlsx" => xlsx::read(
                &bytes,
                arguments["sheet"].as_str(),
                arguments["include_headers"].as_bool() != Some(false),
                arguments["range"].as_str(),
                arguments["dates"].as_str() != Some("serial"),
                policy,
                &deadline,
            )?,
            "pdf" => pdf_read::read(
                &bytes,
                arguments["start_page"].as_u64(),
                arguments["end_page"].as_u64(),
                arguments["password"].as_str(),
                &deadline,
            )?,
            _ => pptx::read(&bytes, arguments["only_slide"].as_u64(), &deadline)?,
        };
        result["path"] = json!(fs::display(&inner.root, &path));
        return Ok(result);
    }
    let mut details = json!({});
    let bytes = match format {
        "docx" => docx::write(arguments)?,
        "xlsx" => xlsx::write(arguments, policy)?,
        "pdf" => {
            let (bytes, info) = pdf_write::write(arguments)?;
            details = info;
            bytes
        }
        _ => pptx::write(arguments)?,
    };
    deadline.check("writing the document")?;
    let written = fs::write_bytes(
        inner,
        relative,
        &bytes,
        arguments["create_dirs"].as_bool() == Some(true),
    )?;
    let mut result = json!({"path":written,"bytes":bytes.len()});
    if let (Some(result), Some(details)) = (result.as_object_mut(), details.as_object()) {
        result.extend(details.clone());
    }
    Ok(result)
}
