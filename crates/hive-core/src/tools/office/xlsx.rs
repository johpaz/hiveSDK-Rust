//! Excel (.xlsx): sheets as JSON rows out, JSON rows in.
//!
//! Values come back as stored: numbers, booleans and strings. Dates are numbers
//! because deciding that a number is a date needs the cell's number format, which
//! is not interpreted here. Formulas are returned as their cached value, never
//! evaluated, and cells written from input are always plain values, so text that
//! starts with `=` is never executed by a spreadsheet application.
use super::{
    Deadline, OfficePolicy, invalid,
    xml::{self, Event, attr, escape},
    zip::{Archive, Writer},
};
use crate::Result;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

const MAX_SHEET_NAME: usize = 31;
const MAX_CELL_CHARS: usize = 32_767;
const MAX_COLUMNS: usize = 1_000;
const MAX_CELLS: usize = 2_000_000;
const MAX_SHARED_STRINGS: usize = 1_000_000;

fn column_name(mut index: usize) -> String {
    let mut name = Vec::new();
    loop {
        name.push(b'A' + (index % 26) as u8);
        if index < 26 {
            break;
        }
        index = index / 26 - 1;
    }
    name.reverse();
    String::from_utf8(name).unwrap()
}

/// `A1` → (row, column), both zero-based.
fn cell_ref(reference: &str) -> Option<(usize, usize)> {
    let letters = reference
        .chars()
        .take_while(char::is_ascii_alphabetic)
        .count();
    let (column, row) = reference.split_at(letters);
    if column.is_empty()
        || column.len() > 3
        || row.is_empty()
        || !row.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let column = column.bytes().fold(0usize, |acc, b| {
        acc * 26 + (b.to_ascii_uppercase() - b'A' + 1) as usize
    }) - 1;
    let row: usize = row.parse().ok()?;
    (row >= 1 && column < 16_384).then(|| (row - 1, column))
}

struct Range {
    rows: (usize, usize),
    columns: (usize, usize),
}

fn parse_range(text: &str) -> Result<Range> {
    let (first, last) = text.split_once(':').unwrap_or((text, text));
    let (a, b) = (cell_ref(first), cell_ref(last));
    let (Some(a), Some(b)) = (a, b) else {
        return Err(invalid("range must be in A1 notation such as A1:D10"));
    };
    Ok(Range {
        rows: (a.0.min(b.0), a.0.max(b.0)),
        columns: (a.1.min(b.1), a.1.max(b.1)),
    })
}

fn number(text: &str) -> Value {
    match text.trim().parse::<f64>() {
        Ok(value)
            if value.is_finite()
                && value.fract() == 0.0
                && value.abs() < 9_007_199_254_740_992.0 =>
        {
            json!(value as i64)
        }
        Ok(value) if value.is_finite() => json!(value),
        _ => json!(text),
    }
}

fn shared_strings(archive: &Archive, deadline: &Deadline) -> Result<Vec<String>> {
    if !archive.contains("xl/sharedStrings.xml") {
        return Ok(vec![]);
    }
    let mut strings = Vec::new();
    let (mut current, mut in_item, mut in_text, mut skip) = (String::new(), false, false, 0usize);
    for (index, event) in xml::parse(&archive.read_text("xl/sharedStrings.xml")?)?
        .into_iter()
        .enumerate()
    {
        if index % 4096 == 0 {
            deadline.check("reading shared strings")?;
        }
        match event {
            Event::Start { name, .. } if name == "si" => (in_item, current) = (true, String::new()),
            Event::End { name } if name == "si" => {
                if strings.len() >= MAX_SHARED_STRINGS {
                    return Err(invalid("workbook has too many shared strings"));
                }
                strings.push(std::mem::take(&mut current));
                in_item = false;
            }
            // Phonetic guides are not part of the visible text.
            Event::Start { name, .. } if name == "rPh" => skip += 1,
            Event::End { name } if name == "rPh" => skip = skip.saturating_sub(1),
            Event::Start { name, .. } if name == "t" && in_item && skip == 0 => in_text = true,
            Event::End { name } if name == "t" => in_text = false,
            Event::Text(text) if in_text => current.push_str(&text),
            _ => {}
        }
    }
    Ok(strings)
}

fn target(base: &str, target: &str) -> String {
    match target.strip_prefix('/') {
        Some(absolute) => absolute.to_owned(),
        None => format!("{base}{target}"),
    }
}

fn sheet_list(archive: &Archive) -> Result<Vec<(String, String)>> {
    let relationships: BTreeMap<String, String> = if archive.contains("xl/_rels/workbook.xml.rels")
    {
        xml::parse(&archive.read_text("xl/_rels/workbook.xml.rels")?)?
            .into_iter()
            .filter_map(|event| match event {
                Event::Empty { name, attrs } | Event::Start { name, attrs }
                    if name == "Relationship" =>
                {
                    Some((
                        attr(&attrs, "Id")?.to_owned(),
                        target("xl/", attr(&attrs, "Target")?),
                    ))
                }
                _ => None,
            })
            .collect()
    } else {
        BTreeMap::new()
    };
    let mut sheets = Vec::new();
    for event in xml::parse(&archive.read_text("xl/workbook.xml")?)? {
        if let Event::Empty { name, attrs } | Event::Start { name, attrs } = event
            && name == "sheet"
        {
            let title = attr(&attrs, "name")
                .ok_or_else(|| invalid("sheet without a name"))?
                .to_owned();
            let part = attr(&attrs, "r:id")
                .and_then(|id| relationships.get(id))
                .ok_or_else(|| invalid(&format!("sheet `{title}` has no worksheet part")))?;
            sheets.push((title, part.clone()));
        }
    }
    Ok(sheets)
}

/// Rows of cell values, sparse rows kept at their position.
fn read_sheet(
    archive: &Archive,
    part: &str,
    strings: &[String],
    row_cap: usize,
    deadline: &Deadline,
) -> Result<BTreeMap<usize, BTreeMap<usize, Value>>> {
    let mut rows: BTreeMap<usize, BTreeMap<usize, Value>> = BTreeMap::new();
    let (mut cells, mut next_row) = (0usize, 0usize);
    let mut cell: Option<(usize, usize, String)> = None;
    let (mut value, mut in_value, mut in_inline) = (String::new(), false, false);
    for (index, event) in xml::parse(&archive.read_text(part)?)?
        .into_iter()
        .enumerate()
    {
        if index % 4096 == 0 {
            deadline.check("reading the sheet")?;
        }
        match event {
            Event::Start { name, attrs } if name == "row" => {
                next_row = attr(&attrs, "r")
                    .and_then(|r| r.parse::<usize>().ok())
                    .map_or(next_row, |r| r.saturating_sub(1));
                if next_row >= row_cap {
                    break;
                }
            }
            Event::End { name } if name == "row" => next_row += 1,
            Event::Start { name, attrs } if name == "c" => {
                let (row, column) = match attr(&attrs, "r").and_then(cell_ref) {
                    Some(position) => position,
                    None => (
                        next_row,
                        rows.get(&next_row)
                            .and_then(|r| r.keys().next_back())
                            .map_or(0, |c| c + 1),
                    ),
                };
                cell = Some((row, column, attr(&attrs, "t").unwrap_or("n").to_owned()));
                value.clear();
            }
            Event::Start { name, .. } if name == "v" => in_value = true,
            Event::Start { name, .. } if name == "is" => in_inline = true,
            Event::End { name } if name == "is" => in_inline = false,
            Event::Text(text) if in_value || (in_inline && cell.is_some()) => value.push_str(&text),
            Event::End { name } if name == "v" => in_value = false,
            Event::End { name } if name == "c" => {
                let Some((row, column, kind)) = cell.take() else {
                    continue;
                };
                if row >= row_cap || column >= MAX_COLUMNS * 16 {
                    continue;
                }
                let parsed = match kind.as_str() {
                    "s" => value
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .and_then(|at| strings.get(at))
                        .map_or(Value::Null, |text| json!(text)),
                    "b" => json!(value.trim() == "1"),
                    "str" | "inlineStr" | "e" => json!(value),
                    _ if value.trim().is_empty() => Value::Null,
                    _ => number(&value),
                };
                if !parsed.is_null() {
                    cells += 1;
                    if cells > MAX_CELLS {
                        return Err(invalid("sheet has too many cells"));
                    }
                    rows.entry(row).or_default().insert(column, parsed);
                }
            }
            _ => {}
        }
    }
    Ok(rows)
}

fn display(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub(super) fn read(
    bytes: &[u8],
    sheet: Option<&str>,
    include_headers: bool,
    range: Option<&str>,
    policy: &OfficePolicy,
    deadline: &Deadline,
) -> Result<Value> {
    let archive = Archive::open(bytes)?;
    if !archive.contains("xl/workbook.xml") {
        return Err(invalid(
            "not an Excel workbook (xl/workbook.xml is missing)",
        ));
    }
    let sheets = sheet_list(&archive)?;
    if sheets.len() > policy.max_sheets {
        return Err(invalid(&format!(
            "the workbook has more than {} sheets",
            policy.max_sheets
        )));
    }
    let wanted: Vec<&(String, String)> = match sheet {
        Some(name) => {
            let found: Vec<_> = sheets.iter().filter(|(title, _)| title == name).collect();
            if found.is_empty() {
                let names: Vec<_> = sheets.iter().map(|(title, _)| title.as_str()).collect();
                return Err(invalid(&format!(
                    "sheet `{name}` not found; available: {}",
                    names.join(", ")
                )));
            }
            found
        }
        None => sheets.iter().collect(),
    };
    let range = range.map(parse_range).transpose()?;
    let strings = shared_strings(&archive, deadline)?;
    let (mut data, mut total) = (Map::new(), 0usize);
    for (title, part) in wanted {
        deadline.check("reading the workbook")?;
        let first = range.as_ref().map_or(0, |range| range.rows.0);
        // Read one row past the limit so an oversized sheet is reported, not truncated.
        let row_cap = range
            .as_ref()
            .map_or(usize::MAX, |range| range.rows.1.saturating_add(1))
            .min(first + policy.max_rows_per_sheet + 3);
        let cells = read_sheet(&archive, part, &strings, row_cap, deadline)?;
        let columns = range
            .as_ref()
            .map_or((0, usize::MAX), |range| range.columns);
        let mut table: Vec<(usize, Vec<(usize, Value)>)> = cells
            .into_iter()
            .filter(|(row, _)| *row >= first && *row < row_cap)
            .map(|(row, cells)| {
                (
                    row,
                    cells
                        .into_iter()
                        .filter(|(column, _)| *column >= columns.0 && *column <= columns.1)
                        .collect::<Vec<_>>(),
                )
            })
            .filter(|(_, cells)| !cells.is_empty())
            .collect();
        table.sort_by_key(|(row, _)| *row);
        let width = table
            .iter()
            .flat_map(|(_, cells)| cells.iter().map(|(column, _)| *column))
            .max()
            .map_or(0, |max| max + 1);
        let origin = if columns.0 == 0 { 0 } else { columns.0 };
        let dense = |cells: &[(usize, Value)]| -> Vec<Value> {
            let mut row = vec![json!(""); width.saturating_sub(origin)];
            for (column, value) in cells {
                row[column - origin] = value.clone();
            }
            row
        };
        let rows: Vec<Value> = if include_headers {
            let mut iter = table.iter();
            let Some((_, header)) = iter.next() else {
                data.insert(title.clone(), json!([]));
                continue;
            };
            let mut seen: BTreeSet<String> = BTreeSet::new();
            let mut empty = 0usize;
            let keys: Vec<String> = dense(header)
                .iter()
                .map(|cell| {
                    let base = display(cell);
                    let mut key = if base.is_empty() {
                        let key = if empty == 0 {
                            "__EMPTY".to_owned()
                        } else {
                            format!("__EMPTY_{empty}")
                        };
                        empty += 1;
                        key
                    } else {
                        base.clone()
                    };
                    let mut suffix = 0;
                    while !seen.insert(key.clone()) {
                        suffix += 1;
                        key = format!("{base}_{suffix}");
                    }
                    key
                })
                .collect();
            iter.map(|(_, cells)| Value::Object(keys.iter().cloned().zip(dense(cells)).collect()))
                .collect()
        } else {
            table
                .iter()
                .map(|(_, cells)| Value::Array(dense(cells)))
                .collect()
        };
        if rows.len() > policy.max_rows_per_sheet {
            return Err(invalid(&format!(
                "sheet `{title}` exceeds the {} row limit",
                policy.max_rows_per_sheet
            )));
        }
        total += rows.len();
        data.insert(title.clone(), Value::Array(rows));
    }
    Ok(
        json!({"sheets":sheets.iter().map(|(title, _)| title).collect::<Vec<_>>(),"data":data,"total_rows":total}),
    )
}

fn valid_sheet_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_SHEET_NAME
        && !name.contains(['[', ']', ':', '*', '?', '/', '\\'])
        && !name.starts_with('\'')
        && !name.ends_with('\'')
        && !name.eq_ignore_ascii_case("history")
}

enum Cell {
    Text(String),
    Number(String),
    Bool(bool),
}

fn cell_of(value: &Value) -> Result<Option<Cell>> {
    Ok(match value {
        Value::Null => None,
        Value::Bool(flag) => Some(Cell::Bool(*flag)),
        Value::Number(number) => Some(Cell::Number(number.to_string())),
        Value::String(text) => {
            if text.chars().count() > MAX_CELL_CHARS {
                return Err(invalid("cell text exceeds 32767 characters"));
            }
            Some(Cell::Text(text.clone()))
        }
        other => {
            let text = other.to_string();
            if text.chars().count() > MAX_CELL_CHARS {
                return Err(invalid("cell text exceeds 32767 characters"));
            }
            Some(Cell::Text(text))
        }
    })
}

pub(super) fn write(input: &Value, policy: &OfficePolicy) -> Result<Vec<u8>> {
    let sheets = input["sheets"]
        .as_array()
        .filter(|sheets| !sheets.is_empty())
        .ok_or_else(|| invalid("a workbook needs at least one sheet"))?;
    if sheets.len() > policy.max_sheets {
        return Err(invalid(&format!("at most {} sheets", policy.max_sheets)));
    }
    let (mut shared, mut shared_index) = (Vec::<String>::new(), BTreeMap::<String, usize>::new());
    let (mut names, mut parts) = (Vec::<String>::new(), Vec::<String>::new());
    for sheet in sheets {
        let name = sheet["name"]
            .as_str()
            .ok_or_else(|| invalid("each sheet needs a name"))?;
        if !valid_sheet_name(name) || names.iter().any(|other| other.eq_ignore_ascii_case(name)) {
            return Err(invalid(&format!(
                "invalid or duplicate sheet name `{name}`"
            )));
        }
        names.push(name.to_owned());
        let data = sheet["data"]
            .as_array()
            .ok_or_else(|| invalid("each sheet needs data"))?;
        if data.len() > policy.max_rows_per_sheet {
            return Err(invalid(&format!(
                "sheet `{name}` exceeds the {} row limit",
                policy.max_rows_per_sheet
            )));
        }
        // Rows of arrays are written as they are; rows of objects use `headers` or the sorted keys.
        let mut grid: Vec<Vec<Value>> = Vec::new();
        if data.first().is_some_and(Value::is_array) {
            for row in data {
                grid.push(
                    row.as_array()
                        .ok_or_else(|| invalid("a sheet cannot mix rows of arrays and objects"))?
                        .clone(),
                );
            }
        } else {
            let headers: Vec<String> = match sheet["headers"].as_array() {
                Some(headers) => headers
                    .iter()
                    .map(|header| header.as_str().unwrap_or_default().to_owned())
                    .collect(),
                None => data
                    .iter()
                    .filter_map(Value::as_object)
                    .flat_map(|row| row.keys().cloned())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect(),
            };
            if !headers.is_empty() {
                grid.push(headers.iter().map(|header| json!(header)).collect());
                for row in data {
                    let row = row
                        .as_object()
                        .ok_or_else(|| invalid("a sheet cannot mix rows of arrays and objects"))?;
                    grid.push(
                        headers
                            .iter()
                            .map(|header| row.get(header).cloned().unwrap_or(Value::Null))
                            .collect(),
                    );
                }
            }
        }
        if grid.iter().any(|row| row.len() > MAX_COLUMNS) {
            return Err(invalid(&format!("at most {MAX_COLUMNS} columns")));
        }
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData>",
        );
        for (row_number, row) in grid.iter().enumerate() {
            let mut cells = String::new();
            for (column, value) in row.iter().enumerate() {
                let reference = format!("{}{}", column_name(column), row_number + 1);
                match cell_of(value)? {
                    None => {}
                    Some(Cell::Number(text)) => {
                        cells.push_str(&format!("<c r=\"{reference}\"><v>{text}</v></c>"))
                    }
                    Some(Cell::Bool(flag)) => cells.push_str(&format!(
                        "<c r=\"{reference}\" t=\"b\"><v>{}</v></c>",
                        u8::from(flag)
                    )),
                    Some(Cell::Text(text)) => {
                        let next = shared.len();
                        let at = *shared_index.entry(text.clone()).or_insert_with(|| {
                            shared.push(text);
                            next
                        });
                        cells.push_str(&format!("<c r=\"{reference}\" t=\"s\"><v>{at}</v></c>"));
                    }
                }
            }
            if !cells.is_empty() {
                xml.push_str(&format!("<row r=\"{}\">{cells}</row>", row_number + 1));
            }
        }
        xml.push_str("</sheetData></worksheet>");
        parts.push(xml);
    }
    let header = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";
    let mut zip = Writer::default();
    let overrides: String = (1..=parts.len()).map(|n| format!("<Override PartName=\"/xl/worksheets/sheet{n}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>")).collect();
    zip.add("[Content_Types].xml", format!("{header}<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/><Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/><Override PartName=\"/xl/sharedStrings.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml\"/>{overrides}</Types>").as_bytes())?;
    zip.add("_rels/.rels", format!("{header}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>").as_bytes())?;
    let listing: String = names
        .iter()
        .enumerate()
        .map(|(n, name)| {
            format!(
                "<sheet name=\"{}\" sheetId=\"{}\" r:id=\"rId{}\"/>",
                escape(name),
                n + 1,
                n + 1
            )
        })
        .collect();
    zip.add("xl/workbook.xml", format!("{header}<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets>{listing}</sheets></workbook>").as_bytes())?;
    let count = parts.len();
    let relationships: String = (1..=count).map(|n| format!("<Relationship Id=\"rId{n}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet{n}.xml\"/>")).collect();
    zip.add("xl/_rels/workbook.xml.rels", format!("{header}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{relationships}<Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles\" Target=\"styles.xml\"/><Relationship Id=\"rId{}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings\" Target=\"sharedStrings.xml\"/></Relationships>", count + 1, count + 2).as_bytes())?;
    zip.add("xl/styles.xml", format!("{header}<styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><fonts count=\"1\"><font><sz val=\"11\"/><name val=\"Calibri\"/></font></fonts><fills count=\"2\"><fill><patternFill patternType=\"none\"/></fill><fill><patternFill patternType=\"gray125\"/></fill></fills><borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders><cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs><cellXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/></cellXfs><cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles></styleSheet>").as_bytes())?;
    let strings: String = shared
        .iter()
        .map(|text| format!("<si><t xml:space=\"preserve\">{}</t></si>", escape(text)))
        .collect();
    zip.add("xl/sharedStrings.xml", format!("{header}<sst xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\" count=\"{0}\" uniqueCount=\"{0}\">{strings}</sst>", shared.len()).as_bytes())?;
    for (index, part) in parts.iter().enumerate() {
        zip.add(
            &format!("xl/worksheets/sheet{}.xml", index + 1),
            part.as_bytes(),
        )?;
    }
    zip.finish()
}
