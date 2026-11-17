//! Excel (.xlsx): sheets as JSON rows out, JSON rows in.
//!
//! Values come back as stored: numbers, booleans and strings. A number whose cell has a date or
//! time number format is returned as ISO text (`2025-03-01`, `2025-03-01T14:30:00`, `14:30:00`)
//! unless `dates` is `serial`, which keeps Excel's day number. Formulas are returned as their
//! cached value, never evaluated, and cells written from input are always plain values, so text
//! that starts with `=` is never executed by a spreadsheet application.
//!
//! Writing takes optional per-column formats (dates, percentages, currency, any Excel format
//! code), widths, alignment, a header style, a frozen header row and an autofilter; ISO strings in
//! a date column are stored as real dates.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DateKind {
    Date,
    Time,
    DateTime,
}

/// What a built-in number format id is, if it is a date or time.
fn builtin_kind(id: u32) -> Option<DateKind> {
    match id {
        14..=17 | 27..=31 | 34..=36 | 50..=58 => Some(DateKind::Date),
        18..=21 | 32 | 33 | 45..=47 => Some(DateKind::Time),
        22 => Some(DateKind::DateTime),
        _ => None,
    }
}

/// Is this custom format code a date or time? Quoted text, escaped characters, colors, conditions
/// and locale tags are not tokens; only the first (positive-number) section is looked at.
fn code_kind(code: &str) -> Option<DateKind> {
    let first = code.split(';').next().unwrap_or_default();
    let mut text = String::new();
    let mut chars = first.chars().peekable();
    let (mut in_quote, mut elapsed_time) = (false, false);
    while let Some(c) = chars.next() {
        match c {
            '"' => in_quote = !in_quote,
            _ if in_quote => {}
            '\\' | '_' | '*' => {
                chars.next();
            }
            '[' => {
                let inner: String = chars.by_ref().take_while(|c| *c != ']').collect();
                if matches!(
                    inner.to_ascii_lowercase().as_str(),
                    "h" | "hh" | "m" | "mm" | "s" | "ss"
                ) {
                    elapsed_time = true;
                    text.push(inner.chars().next().unwrap_or('h').to_ascii_lowercase());
                }
            }
            c => text.push(c.to_ascii_lowercase()),
        }
    }
    let text = text.replace("am/pm", "").replace("a/p", "");
    let date = text.contains('y')
        || text.contains('d')
        || (text.contains('m') && !text.contains('h') && !text.contains('s') && !elapsed_time);
    let time = text.contains('h') || text.contains('s');
    match (date, time) {
        (true, true) => Some(DateKind::DateTime),
        (true, false) => Some(DateKind::Date),
        (false, true) => Some(DateKind::Time),
        _ => None,
    }
}

/// Days since 1970-01-01 of a civil date, and the inverse (Howard Hinnant's algorithms).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Excel's 1900 system counts a 29 February 1900 that never existed: serial 60. Before it the
/// epoch is 1899-12-31, after it 1899-12-30. The 1904 system has no such quirk.
const EPOCH_1900_AFTER_BUG: i64 = -25_569; // 1899-12-30 as days since 1970
const EPOCH_1900_BEFORE_BUG: i64 = -25_568; // 1899-12-31
const EPOCH_1904: i64 = -24_107; // 1904-01-01

/// An Excel serial number as ISO text, or `None` when it is out of range (the number is kept).
fn serial_to_iso(serial: f64, kind: DateKind, date1904: bool) -> Option<String> {
    if !serial.is_finite() || !(0.0..2_958_466.0).contains(&serial) {
        return None;
    }
    let mut day = serial.floor() as i64;
    let mut seconds = ((serial - serial.floor()) * 86_400.0).round() as i64;
    if seconds >= 86_400 {
        day += 1;
        seconds -= 86_400;
    }
    let hms = |seconds: i64| {
        format!(
            "{:02}:{:02}:{:02}",
            seconds / 3600,
            seconds % 3600 / 60,
            seconds % 60
        )
    };
    if kind == DateKind::Time {
        // Hours past 24 are elapsed time, as in `[h]:mm:ss`.
        let total = day * 86_400 + seconds;
        return Some(format!(
            "{:02}:{:02}:{:02}",
            total / 3600,
            total % 3600 / 60,
            total % 60
        ));
    }
    let (year, month, date) = if date1904 {
        civil_from_days(day + EPOCH_1904)
    } else if day == 60 {
        (1900, 2, 29)
    } else if day < 60 {
        if day < 1 {
            return None; // "day zero" is a time-only value
        }
        civil_from_days(day + EPOCH_1900_BEFORE_BUG)
    } else {
        civil_from_days(day + EPOCH_1900_AFTER_BUG)
    };
    let date_part = format!("{year:04}-{month:02}-{date:02}");
    Some(match kind {
        DateKind::DateTime => format!("{date_part}T{}", hms(seconds)),
        _ => date_part,
    })
}

/// ISO text (`2025-03-01`, `2025-03-01T14:30[:00]`, or a time `14:30[:00]`) as an Excel serial number.
fn iso_to_serial(text: &str, kind: DateKind) -> Option<f64> {
    let text = text.trim();
    let time = |part: &str| -> Option<f64> {
        let mut fields = part.split(':');
        let (h, m, s) = (fields.next()?, fields.next()?, fields.next().unwrap_or("0"));
        if fields.next().is_some()
            || [h, m, s]
                .iter()
                .any(|f| f.is_empty() || !f.bytes().all(|b| b.is_ascii_digit()))
        {
            return None;
        }
        let (h, m, s): (u32, u32, u32) = (h.parse().ok()?, m.parse().ok()?, s.parse().ok()?);
        (h < 24 && m < 60 && s < 60).then(|| f64::from(h * 3600 + m * 60 + s) / 86_400.0)
    };
    if kind == DateKind::Time {
        return time(text);
    }
    let (date, clock) = match text.split_once(['T', ' ']) {
        Some((date, clock)) => (date, Some(clock)),
        None => (text, None),
    };
    let mut fields = date.split('-');
    let (y, m, d) = (fields.next()?, fields.next()?, fields.next()?);
    if fields.next().is_some()
        || y.len() != 4
        || m.len() != 2
        || d.len() != 2
        || ![y, m, d]
            .iter()
            .all(|f| f.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let longest = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => 28 + i64::from(leap),
        _ => return None,
    };
    if !(1..=longest).contains(&d) || !(1900..=9999).contains(&y) {
        return None;
    }
    let unix = days_from_civil(y, m, d);
    let day = if unix >= EPOCH_1900_AFTER_BUG + 61 {
        unix - EPOCH_1900_AFTER_BUG
    } else {
        unix - EPOCH_1900_BEFORE_BUG
    };
    if day < 1 {
        return None;
    }
    let fraction = match clock {
        Some(clock) if kind == DateKind::DateTime => time(clock)?,
        Some(_) => return None,
        None => 0.0,
    };
    Some(day as f64 + fraction)
}

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

/// Which cell styles are dates or times, and the workbook's date system.
struct DateStyles {
    kinds: Vec<Option<DateKind>>,
    date1904: bool,
}

fn date_styles(archive: &Archive) -> Result<DateStyles> {
    let date1904 = xml::parse(&archive.read_text("xl/workbook.xml")?)?
        .into_iter()
        .any(|event| {
            matches!(event, Event::Empty { name, attrs } | Event::Start { name, attrs }
            if name == "workbookPr" && matches!(attr(&attrs, "date1904"), Some("1" | "true")))
        });
    let mut kinds = vec![];
    if archive.contains("xl/styles.xml") {
        let mut custom: BTreeMap<u32, Option<DateKind>> = BTreeMap::new();
        let mut in_cell_xfs = false;
        for event in xml::parse(&archive.read_text("xl/styles.xml")?)? {
            match event {
                Event::Empty { name, attrs } | Event::Start { name, attrs } if name == "numFmt" => {
                    if let (Some(id), Some(code)) = (
                        attr(&attrs, "numFmtId").and_then(|v| v.parse().ok()),
                        attr(&attrs, "formatCode"),
                    ) {
                        custom.insert(id, code_kind(code));
                    }
                }
                Event::Start { name, .. } if name == "cellXfs" => in_cell_xfs = true,
                Event::End { name } if name == "cellXfs" => in_cell_xfs = false,
                Event::Empty { name, attrs } | Event::Start { name, attrs }
                    if name == "xf" && in_cell_xfs =>
                {
                    let id: u32 = attr(&attrs, "numFmtId")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    kinds.push(custom.get(&id).copied().unwrap_or_else(|| builtin_kind(id)));
                    if kinds.len() > 64_000 {
                        return Err(invalid("workbook has too many cell styles"));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(DateStyles { kinds, date1904 })
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
    styles: Option<&DateStyles>,
    row_cap: usize,
    deadline: &Deadline,
) -> Result<BTreeMap<usize, BTreeMap<usize, Value>>> {
    let mut rows: BTreeMap<usize, BTreeMap<usize, Value>> = BTreeMap::new();
    let (mut cells, mut next_row) = (0usize, 0usize);
    let mut cell: Option<(usize, usize, String, Option<usize>)> = None;
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
                cell = Some((
                    row,
                    column,
                    attr(&attrs, "t").unwrap_or("n").to_owned(),
                    attr(&attrs, "s").and_then(|s| s.parse().ok()),
                ));
                value.clear();
            }
            Event::Start { name, .. } if name == "v" => in_value = true,
            Event::Start { name, .. } if name == "is" => in_inline = true,
            Event::End { name } if name == "is" => in_inline = false,
            Event::Text(text) if in_value || (in_inline && cell.is_some()) => value.push_str(&text),
            Event::End { name } if name == "v" => in_value = false,
            Event::End { name } if name == "c" => {
                let Some((row, column, kind, style)) = cell.take() else {
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
                    "str" | "inlineStr" | "e" | "d" => json!(value),
                    _ if value.trim().is_empty() => Value::Null,
                    _ => {
                        let plain = number(&value);
                        // A number in a date or time format is shown as the date or time it is.
                        styles
                            .and_then(|styles| {
                                let kind = styles.kinds.get(style?).copied().flatten()?;
                                serial_to_iso(value.trim().parse().ok()?, kind, styles.date1904)
                            })
                            .map_or(plain, Value::String)
                    }
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
    dates_as_text: bool,
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
    let styles = if dates_as_text {
        Some(date_styles(&archive)?)
    } else {
        None
    };
    let (mut data, mut total) = (Map::new(), 0usize);
    for (title, part) in wanted {
        deadline.check("reading the workbook")?;
        let first = range.as_ref().map_or(0, |range| range.rows.0);
        // Read one row past the limit so an oversized sheet is reported, not truncated.
        let row_cap = range
            .as_ref()
            .map_or(usize::MAX, |range| range.rows.1.saturating_add(1))
            .min(first + policy.max_rows_per_sheet + 3);
        let cells = read_sheet(&archive, part, &strings, styles.as_ref(), row_cap, deadline)?;
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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum NumFmt {
    Builtin(u32),
    Custom(String),
}

/// What a cell looks like: number format, font, fill and alignment.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
struct Style {
    num_fmt: Option<NumFmt>,
    bold: bool,
    color: Option<String>,
    fill: Option<String>,
    align: Option<&'static str>,
    wrap: bool,
}

const MAX_STYLES: usize = 4000;

/// The `cellXfs` of a workbook, deduplicated; index 0 is the plain default.
struct StyleBook {
    styles: Vec<Style>,
}
impl StyleBook {
    fn new() -> Self {
        Self {
            styles: vec![Style::default()],
        }
    }
    fn index(&mut self, style: Style) -> Result<usize> {
        if let Some(at) = self.styles.iter().position(|known| *known == style) {
            return Ok(at);
        }
        if self.styles.len() >= MAX_STYLES {
            return Err(invalid("too many different cell styles"));
        }
        self.styles.push(style);
        Ok(self.styles.len() - 1)
    }

    fn xml(&self, header: &str) -> String {
        let mut fonts = vec![(false, None::<String>)];
        let mut fills: Vec<String> = vec![];
        let mut formats: Vec<String> = vec![];
        let mut xfs = String::new();
        for style in &self.styles {
            let font = (style.bold, style.color.clone());
            let font_id = fonts
                .iter()
                .position(|known| *known == font)
                .unwrap_or_else(|| {
                    fonts.push(font);
                    fonts.len() - 1
                });
            let fill_id = style.fill.as_ref().map_or(0, |color| {
                2 + fills
                    .iter()
                    .position(|known| known == color)
                    .unwrap_or_else(|| {
                        fills.push(color.clone());
                        fills.len() - 1
                    })
            });
            let format_id = match &style.num_fmt {
                None => 0,
                Some(NumFmt::Builtin(id)) => *id as usize,
                Some(NumFmt::Custom(code)) => {
                    164 + formats
                        .iter()
                        .position(|known| known == code)
                        .unwrap_or_else(|| {
                            formats.push(code.clone());
                            formats.len() - 1
                        })
                }
            };
            let alignment = match (style.align, style.wrap) {
                (None, false) => String::new(),
                (align, wrap) => format!(
                    "<alignment{}{}/>",
                    align.map_or(String::new(), |a| format!(" horizontal=\"{a}\"")),
                    if wrap { " wrapText=\"1\"" } else { "" }
                ),
            };
            xfs.push_str(&format!(
                "<xf numFmtId=\"{format_id}\" fontId=\"{font_id}\" fillId=\"{fill_id}\" borderId=\"0\" xfId=\"0\" applyNumberFormat=\"1\" applyFont=\"1\" applyFill=\"1\" applyAlignment=\"1\"{}",
                if alignment.is_empty() { "/>".to_owned() } else { format!(">{alignment}</xf>") }
            ));
        }
        let num_fmts = if formats.is_empty() {
            String::new()
        } else {
            format!(
                "<numFmts count=\"{}\">{}</numFmts>",
                formats.len(),
                formats
                    .iter()
                    .enumerate()
                    .map(|(n, code)| format!(
                        "<numFmt numFmtId=\"{}\" formatCode=\"{}\"/>",
                        164 + n,
                        escape(code)
                    ))
                    .collect::<String>()
            )
        };
        let fonts_xml: String = fonts
            .iter()
            .map(|(bold, color)| {
                format!(
                    "<font>{}<sz val=\"11\"/>{}<name val=\"Calibri\"/></font>",
                    if *bold { "<b/>" } else { "" },
                    color
                        .as_ref()
                        .map_or(String::new(), |c| format!("<color rgb=\"{c}\"/>"))
                )
            })
            .collect();
        let fills_xml: String = fills
            .iter()
            .map(|color| format!("<fill><patternFill patternType=\"solid\"><fgColor rgb=\"{color}\"/><bgColor indexed=\"64\"/></patternFill></fill>"))
            .collect();
        format!(
            "{header}<styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">{num_fmts}<fonts count=\"{}\">{fonts_xml}</fonts><fills count=\"{}\"><fill><patternFill patternType=\"none\"/></fill><fill><patternFill patternType=\"gray125\"/></fill>{fills_xml}</fills><borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders><cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs><cellXfs count=\"{}\">{xfs}</cellXfs><cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles></styleSheet>",
            fonts.len(),
            2 + fills.len(),
            self.styles.len()
        )
    }
}

/// `#RRGGBB` as Excel's `AARRGGBB`.
fn argb(color: &str) -> Result<String> {
    let hex = color.strip_prefix('#').unwrap_or_default();
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("colors are written #RRGGBB"));
    }
    Ok(format!("FF{}", hex.to_ascii_uppercase()))
}

fn alignment(value: Option<&Value>) -> Result<Option<&'static str>> {
    Ok(match value.and_then(Value::as_str) {
        None => None,
        Some("left") => Some("left"),
        Some("center") => Some("center"),
        Some("right") => Some("right"),
        Some(_) => return Err(invalid("align must be left, center or right")),
    })
}

/// A column format by name or as an Excel format code, and whether it holds dates or times.
fn number_format(name: &str) -> Result<(NumFmt, Option<DateKind>)> {
    Ok(match name {
        "date" => (NumFmt::Custom("yyyy-mm-dd".into()), Some(DateKind::Date)),
        "datetime" => (
            NumFmt::Custom("yyyy-mm-dd hh:mm:ss".into()),
            Some(DateKind::DateTime),
        ),
        "time" => (NumFmt::Custom("hh:mm:ss".into()), Some(DateKind::Time)),
        "integer" => (NumFmt::Builtin(1), None),
        "decimal" => (NumFmt::Builtin(2), None),
        "thousands" => (NumFmt::Builtin(3), None),
        "percent" => (NumFmt::Builtin(10), None),
        "text" => (NumFmt::Builtin(49), None),
        "currency" => (NumFmt::Custom("$#,##0.00".into()), None),
        code if !code.is_empty()
            && code.chars().count() <= 64
            && !code.chars().any(char::is_control) =>
        {
            (NumFmt::Custom(code.to_owned()), code_kind(code))
        }
        _ => {
            return Err(invalid(
                "a column format is a name (date, datetime, time, integer, decimal, thousands, percent, currency, text) or an Excel format code of at most 64 characters",
            ));
        }
    })
}

struct Column {
    index: usize,
    kind: Option<DateKind>,
    width: Option<f64>,
    xf: usize,
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
    let mut book = StyleBook::new();
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
        let mut header_names: Vec<String> = vec![];
        if data.first().is_some_and(Value::is_array) {
            for row in data {
                grid.push(
                    row.as_array()
                        .ok_or_else(|| invalid("a sheet cannot mix rows of arrays and objects"))?
                        .clone(),
                );
            }
            if let Some(first) = grid.first() {
                header_names = first
                    .iter()
                    .map(|v| v.as_str().unwrap_or_default().to_owned())
                    .collect();
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
            header_names = headers;
        }
        let width = grid.iter().map(Vec::len).max().unwrap_or(0);
        if width > MAX_COLUMNS {
            return Err(invalid(&format!("at most {MAX_COLUMNS} columns")));
        }

        // Column formats, widths and alignment.
        let mut columns: Vec<Column> = vec![];
        for spec in sheet["columns"].as_array().into_iter().flatten() {
            let index = match &spec["column"] {
                Value::Number(n) => n.as_u64().map(|n| n as usize),
                Value::String(key) => header_names.iter().position(|h| h == key),
                _ => None,
            }
            .filter(|index| *index < MAX_COLUMNS)
            .ok_or_else(|| invalid("each column spec names a column by header text or by index"))?;
            if columns.iter().any(|known| known.index == index) {
                return Err(invalid("a column is described twice"));
            }
            let (num_fmt, kind) = match spec["format"].as_str() {
                Some(format) => {
                    let (num_fmt, kind) = number_format(format)?;
                    (Some(num_fmt), kind)
                }
                None => (None, None),
            };
            let width = match spec.get("width").filter(|w| !w.is_null()) {
                None => None,
                Some(w) => Some(
                    w.as_f64()
                        .filter(|w| (1.0..=255.0).contains(w))
                        .ok_or_else(|| invalid("width is between 1 and 255"))?,
                ),
            };
            let style = Style {
                num_fmt,
                bold: spec["bold"].as_bool() == Some(true),
                color: spec["color"].as_str().map(argb).transpose()?,
                fill: spec["fill"].as_str().map(argb).transpose()?,
                align: alignment(spec.get("align"))?,
                wrap: spec["wrap"].as_bool() == Some(true),
            };
            columns.push(Column {
                index,
                kind,
                width,
                xf: book.index(style)?,
            });
        }
        columns.sort_by_key(|column| column.index);
        let header_xf = match sheet.get("header_style").filter(|v| v.is_object()) {
            Some(style) => book.index(Style {
                num_fmt: None,
                bold: style["bold"].as_bool() == Some(true),
                color: style["color"].as_str().map(argb).transpose()?,
                fill: style["fill"].as_str().map(argb).transpose()?,
                align: alignment(style.get("align"))?,
                wrap: false,
            })?,
            None => 0,
        };
        let frozen = sheet["freeze_header"].as_bool() == Some(true);
        let has_header = !grid.is_empty();

        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\">",
        );
        if frozen && has_header {
            xml.push_str("<sheetViews><sheetView workbookViewId=\"0\"><pane ySplit=\"1\" topLeftCell=\"A2\" activePane=\"bottomLeft\" state=\"frozen\"/><selection pane=\"bottomLeft\"/></sheetView></sheetViews>");
        }
        let widths: String = columns
            .iter()
            .filter_map(|column| {
                column.width.map(|w| {
                    format!(
                        "<col min=\"{0}\" max=\"{0}\" width=\"{w}\" customWidth=\"1\"/>",
                        column.index + 1
                    )
                })
            })
            .collect();
        if !widths.is_empty() {
            xml.push_str(&format!("<cols>{widths}</cols>"));
        }
        xml.push_str("<sheetData>");
        for (row_number, row) in grid.iter().enumerate() {
            let mut cells = String::new();
            for (column, value) in row.iter().enumerate() {
                let reference = format!("{}{}", column_name(column), row_number + 1);
                let spec = columns.iter().find(|spec| spec.index == column);
                // The header row takes the header style; the rest, the column's.
                let xf = if row_number == 0 && header_xf != 0 {
                    header_xf
                } else {
                    spec.map_or(0, |spec| spec.xf)
                };
                let style = if xf == 0 {
                    String::new()
                } else {
                    format!(" s=\"{xf}\"")
                };
                // A date column accepts ISO text and stores a real date (the header row stays text).
                let converted;
                let value = match (spec.and_then(|spec| spec.kind), value) {
                    (Some(kind), Value::String(text)) if row_number > 0 || !has_header => {
                        let serial = iso_to_serial(text, kind).ok_or_else(|| invalid(&format!("cell {reference} is not an ISO date or time (2025-03-01, 2025-03-01T14:30:00 or 14:30)")))?;
                        // A whole day is written as 45717, not 45717.0.
                        converted = if serial.fract() == 0.0 {
                            json!(serial as i64)
                        } else {
                            json!(serial)
                        };
                        &converted
                    }
                    _ => value,
                };
                match cell_of(value)? {
                    None if xf != 0 => cells.push_str(&format!("<c r=\"{reference}\"{style}/>")),
                    None => {}
                    Some(Cell::Number(text)) => {
                        cells.push_str(&format!("<c r=\"{reference}\"{style}><v>{text}</v></c>"))
                    }
                    Some(Cell::Bool(flag)) => cells.push_str(&format!(
                        "<c r=\"{reference}\"{style} t=\"b\"><v>{}</v></c>",
                        u8::from(flag)
                    )),
                    Some(Cell::Text(text)) => {
                        let next = shared.len();
                        let at = *shared_index.entry(text.clone()).or_insert_with(|| {
                            shared.push(text);
                            next
                        });
                        cells.push_str(&format!(
                            "<c r=\"{reference}\"{style} t=\"s\"><v>{at}</v></c>"
                        ));
                    }
                }
            }
            if !cells.is_empty() {
                xml.push_str(&format!("<row r=\"{}\">{cells}</row>", row_number + 1));
            }
        }
        xml.push_str("</sheetData>");
        if sheet["autofilter"].as_bool() == Some(true) && has_header && width > 0 {
            xml.push_str(&format!(
                "<autoFilter ref=\"A1:{}{}\"/>",
                column_name(width - 1),
                grid.len()
            ));
        }
        xml.push_str("</worksheet>");
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
    zip.add("xl/styles.xml", book.xml(header).as_bytes())?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_formats_are_dates_times_or_neither() {
        use DateKind::*;
        for (code, expected) in [
            ("yyyy-mm-dd", Some(Date)),
            ("dd/mm/yyyy", Some(Date)),
            ("d-mmm-yy", Some(Date)),
            ("mmmm", Some(Date)),
            ("yyyy-mm-dd hh:mm:ss", Some(DateTime)),
            ("m/d/yyyy h:mm AM/PM", Some(DateTime)),
            ("hh:mm:ss", Some(Time)),
            ("h:mm AM/PM", Some(Time)),
            ("[h]:mm:ss", Some(Time)),
            ("mm:ss", Some(Time)),
            ("[$-409]d-mmm-yy;@", Some(Date)),
            ("[Red]dd/mm/yy", Some(Date)),
            // Not dates: numbers, text, quoted words, escaped characters, colors, conditions.
            ("General", None),
            ("0.00", None),
            ("0.00%", None),
            ("#,##0.00", None),
            ("@", None),
            ("\"día\" 0", None),
            ("0 \"days\"", None),
            ("\\d0", None),
            ("[Red][<0]0.00;0.00", None),
            ("0.00E+00", None),
            ("_-* #,##0_-;-* #,##0_-", None),
            ("", None),
        ] {
            assert_eq!(code_kind(code), expected, "{code}");
        }
        assert_eq!(builtin_kind(14), Some(Date));
        assert_eq!(builtin_kind(22), Some(DateTime));
        assert_eq!(builtin_kind(21), Some(Time));
        assert_eq!(builtin_kind(9), None);
        assert_eq!(builtin_kind(0), None);
    }

    #[test]
    fn serial_numbers_and_iso_text_convert_both_ways_including_excels_1900_quirks() {
        use DateKind::*;
        for (serial, kind, date1904, expected) in [
            (1.0, Date, false, Some("1900-01-01")),
            (59.0, Date, false, Some("1900-02-28")),
            (60.0, Date, false, Some("1900-02-29")), // the day Excel invented
            (61.0, Date, false, Some("1900-03-01")),
            (45717.0, Date, false, Some("2025-03-01")),
            (45717.5, Date, false, Some("2025-03-01")),
            (45717.5, DateTime, false, Some("2025-03-01T12:00:00")),
            (45717.999999, DateTime, false, Some("2025-03-02T00:00:00")), // rounds up into the next day
            (36526.0, Date, false, Some("2000-01-01")),
            (60.0, Date, true, Some("1904-03-01")),
            (43891.0, Date, true, Some("2024-03-02")),
            (0.5, Time, false, Some("12:00:00")),
            (1.5, Time, false, Some("36:00:00")),
            (0.0, Date, false, None),
            (-1.0, Date, false, None),
            (3_000_000.0, Date, false, None),
            (f64::NAN, Date, false, None),
        ] {
            assert_eq!(
                serial_to_iso(serial, kind, date1904).as_deref(),
                expected,
                "{serial} {kind:?} 1904={date1904}"
            );
        }
        // Back again, for every day of two centuries (outside Excel's invented day).
        for day in 61..=(61 + 365 * 200) {
            let iso = serial_to_iso(day as f64, Date, false).unwrap();
            assert_eq!(iso_to_serial(&iso, Date), Some(day as f64), "{iso}");
        }
        for day in 1..60 {
            let iso = serial_to_iso(day as f64, Date, false).unwrap();
            assert_eq!(iso_to_serial(&iso, Date), Some(day as f64), "{iso}");
        }
        assert_eq!(
            iso_to_serial("2025-03-01T14:30:00", DateKind::DateTime)
                .map(|v| (v * 86_400.0).round() as i64),
            Some(45717 * 86_400 + 14 * 3600 + 30 * 60)
        );
        assert_eq!(
            iso_to_serial("2025-03-01 14:30", DateKind::DateTime),
            iso_to_serial("2025-03-01T14:30:00", DateKind::DateTime)
        );
        assert_eq!(
            iso_to_serial("14:30", DateKind::Time),
            Some((14.0 * 3600.0 + 30.0 * 60.0) / 86_400.0)
        );
        for bad in [
            "",
            "2025-3-1",
            "2025-02-29",
            "2025-13-01",
            "2025-04-31",
            "1899-12-31",
            "10000-01-01",
            "2025-03-01T25:00",
            "2025-03-01T10:61",
            "hola",
            "2025/03/01",
            "2025-03-01T",
        ] {
            assert_eq!(iso_to_serial(bad, DateKind::DateTime), None, "{bad}");
        }
        assert!(
            iso_to_serial("2024-02-29", DateKind::Date).is_some(),
            "a leap day"
        );
        assert_eq!(
            iso_to_serial("1900-02-29", DateKind::Date),
            None,
            "1900 was not a leap year"
        );
        assert_eq!(
            iso_to_serial("2025-03-01T10:00", DateKind::Date),
            None,
            "a date column holds no time"
        );
    }

    fn policy() -> OfficePolicy {
        OfficePolicy::default()
    }
    fn part(bytes: &[u8], name: &str) -> String {
        Archive::open(bytes).unwrap().read_text(name).unwrap()
    }

    #[test]
    fn column_formats_widths_header_style_freeze_and_filter_are_written_as_excel_expects() {
        let book = write(
            &json!({"sheets":[{"name":"Ventas","headers":["fecha","monto","tasa","nota"],"data":[
                {"fecha":"2025-03-01","monto":1234.5,"tasa":0.25,"nota":"=1+1"},
                {"fecha":"2025-03-02T10:30:00","monto":10,"tasa":0.5,"nota":null}],
                "columns":[{"column":"fecha","format":"datetime","width":22},{"column":"monto","format":"currency","bold":true,"color":"#CC0000","align":"right"},
                           {"column":2,"format":"percent","fill":"#FFFF00"},{"column":"nota","wrap":true}],
                "header_style":{"bold":true,"fill":"#DDEEFF","color":"#112233","align":"center"},"freeze_header":true,"autofilter":true}]}),
            &policy(),
        )
        .unwrap();
        let sheet = part(&book, "xl/worksheets/sheet1.xml");
        assert!(
            sheet.contains(
                "<pane ySplit=\"1\" topLeftCell=\"A2\" activePane=\"bottomLeft\" state=\"frozen\"/>"
            ),
            "{sheet}"
        );
        assert!(
            sheet
                .contains("<cols><col min=\"1\" max=\"1\" width=\"22\" customWidth=\"1\"/></cols>"),
            "{sheet}"
        );
        assert!(sheet.contains("<autoFilter ref=\"A1:D3\"/>"), "{sheet}");
        // The date strings became real dates (numbers), the percent stayed a number, the formula stayed text.
        assert!(
            sheet.contains("<v>45717</v>")
                && sheet.contains("<v>1234.5</v>")
                && sheet.contains("<v>0.25</v>"),
            "{sheet}"
        );
        assert!(
            !sheet.contains("2025-03-01"),
            "dates are not stored as text: {sheet}"
        );
        let styles = part(&book, "xl/styles.xml");
        assert!(
            styles.contains("formatCode=\"yyyy-mm-dd hh:mm:ss\"")
                && styles.contains("formatCode=\"$#,##0.00\""),
            "{styles}"
        );
        assert!(
            styles.contains("numFmtId=\"10\""),
            "percent is the built-in 0.00%: {styles}"
        );
        assert!(
            styles.contains("<b/>")
                && styles.contains("<color rgb=\"FFCC0000\"/>")
                && styles.contains("<color rgb=\"FF112233\"/>"),
            "{styles}"
        );
        assert!(
            styles.contains("<fgColor rgb=\"FFFFFF00\"/>")
                && styles.contains("<fgColor rgb=\"FFDDEEFF\"/>"),
            "{styles}"
        );
        assert!(
            styles.contains("horizontal=\"center\"")
                && styles.contains("horizontal=\"right\"")
                && styles.contains("wrapText=\"1\""),
            "{styles}"
        );
        // And it reads back as it was meant: ISO dates, numbers, and the formula as plain text.
        let policy = policy();
        let read = read(
            &book,
            None,
            true,
            None,
            true,
            &policy,
            &Deadline::new(policy.timeout_ms),
        )
        .unwrap();
        assert_eq!(
            read["data"]["Ventas"][0],
            json!({"fecha":"2025-03-01T00:00:00","monto":1234.5,"tasa":0.25,"nota":"=1+1"})
        );
        assert_eq!(read["data"]["Ventas"][1]["fecha"], "2025-03-02T10:30:00");
        let serial = read_serial(&book);
        assert_eq!(serial["data"]["Ventas"][0]["fecha"], 45717);
    }
    fn read_serial(book: &[u8]) -> Value {
        let policy = policy();
        read(
            book,
            None,
            true,
            None,
            false,
            &policy,
            &Deadline::new(policy.timeout_ms),
        )
        .unwrap()
    }

    #[test]
    fn a_workbook_without_any_formatting_is_written_as_before() {
        let book = write(
            &json!({"sheets":[{"name":"A","data":[{"k":"v","n":1}]}]}),
            &policy(),
        )
        .unwrap();
        let sheet = part(&book, "xl/worksheets/sheet1.xml");
        assert!(
            !sheet.contains("<cols>")
                && !sheet.contains("<pane")
                && !sheet.contains("autoFilter")
                && !sheet.contains(" s=\""),
            "{sheet}"
        );
        let styles = part(&book, "xl/styles.xml");
        assert!(
            styles.contains("<cellXfs count=\"1\">") && !styles.contains("numFmts"),
            "{styles}"
        );
    }

    #[test]
    fn bad_formatting_requests_are_refused_with_a_reason() {
        let sheet = |extra: Value| {
            let mut sheet =
                json!({"name":"A","headers":["d","n"],"data":[{"d":"2025-03-01","n":1}]});
            for (key, value) in extra.as_object().unwrap() {
                sheet[key] = value.clone();
            }
            write(&json!({"sheets":[sheet]}), &policy())
        };
        let message =
            |result: Result<Vec<u8>>| result.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(message(sheet(json!({"columns":[{"column":"d","format":"date"}],"data":[{"d":"01/03/2025","n":1}]}))).contains("cell A2 is not an ISO date"));
        assert!(message(sheet(json!({"columns":[{"column":"d","format":"date"}],"data":[{"d":"2025-02-30","n":1}]}))).contains("ISO date"));
        assert!(
            message(sheet(json!({"columns":[{"column":"n","color":"red"}]}))).contains("#RRGGBB")
        );
        assert!(
            message(sheet(json!({"columns":[{"column":"n","fill":"#12345"}]}))).contains("#RRGGBB")
        );
        assert!(
            message(sheet(json!({"columns":[{"column":"n","align":"justify"}]}))).contains("align")
        );
        assert!(message(sheet(json!({"columns":[{"column":"n","width":0}]}))).contains("width"));
        assert!(message(sheet(json!({"columns":[{"column":"nope"}]}))).contains("names a column"));
        assert!(message(sheet(json!({"columns":[{"column":5000}]}))).contains("names a column"));
        assert!(message(sheet(json!({"columns":[{"column":"n"},{"column":1}]}))).contains("twice"));
        assert!(
            message(sheet(json!({"columns":[{"column":"n","format":""}]})))
                .contains("column format")
        );
        assert!(
            message(sheet(json!({"columns":[{"column":"n","format":"x\u{7}"}]})))
                .contains("column format")
        );
        // A time column takes a time; a date column does not take a time of day.
        assert!(
            sheet(json!({"columns":[{"column":"d","format":"time"}],"data":[{"d":"14:30","n":1}]}))
                .is_ok()
        );
        assert!(message(sheet(json!({"columns":[{"column":"d","format":"date"}],"data":[{"d":"2025-03-01T10:00","n":1}]}))).contains("ISO date"));
        // A format code that is a date is a date column too.
        assert!(sheet(json!({"columns":[{"column":"d","format":"dd/mm/yyyy"}]})).is_ok());
        // Numbers already serial and empty cells pass through a date column.
        assert!(sheet(json!({"columns":[{"column":"d","format":"date"}],"data":[{"d":45717,"n":1},{"d":null,"n":2}]})).is_ok());
    }
}
