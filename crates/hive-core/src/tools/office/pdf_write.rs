//! Styled PDF generation.
//!
//! Documents are laid out here, not by an external renderer: fonts are the 14
//! standard PDF faces (nothing is embedded) measured with their real AFM widths, so
//! lines wrap where the viewer will draw them. Content is WinAnsi (Western
//! European); characters outside it become `?` and are counted. The look comes from
//! a theme (a preset plus optional color and font overrides) and the content from
//! `blocks`: headings, paragraphs with styled runs and links, lists, tables with a
//! repeating header, quotes, code, rules and page breaks, with optional header and
//! footer text carrying page numbers.
use super::{
    invalid,
    pdf_metrics::{
        HELVETICA, HELVETICA_BOLD, TIMES_BOLD, TIMES_BOLDITALIC, TIMES_ITALIC, TIMES_ROMAN,
    },
};
use crate::Result;
use serde_json::{Value, json};

const MAX_CHARS: usize = 2_000_000;
const MAX_PAGES: usize = 500;
const MAX_BLOCKS: usize = 10_000;
const MAX_ROWS: usize = 5_000;
const MAX_COLUMNS: usize = 32;
const MAX_LIST_ITEMS: usize = 5_000;
const CELL_PAD: f64 = 5.0;

type Rgb = (f64, f64, f64);

fn rgb(hex: &str) -> Result<Rgb> {
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid("colors must be six hex digits such as 2563EB"));
    }
    let channel =
        |at: usize| f64::from(u8::from_str_radix(&hex[at..at + 2], 16).unwrap_or(0)) / 255.0;
    Ok((channel(0), channel(2), channel(4)))
}

// ---- fonts and styles ------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Family {
    Sans,
    Serif,
    Mono,
}

impl Family {
    fn parse(name: &str) -> Result<Self> {
        match name {
            "sans" => Ok(Self::Sans),
            "serif" => Ok(Self::Serif),
            "mono" => Ok(Self::Mono),
            other => Err(invalid(&format!(
                "unknown font family `{other}` (sans, serif or mono)"
            ))),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Face {
    family: Family,
    bold: bool,
    italic: bool,
}

impl Face {
    /// 0..12: the resource name is `F{slot + 1}`.
    fn slot(self) -> usize {
        let family = match self.family {
            Family::Sans => 0,
            Family::Serif => 1,
            Family::Mono => 2,
        };
        family * 4 + usize::from(self.bold) + 2 * usize::from(self.italic)
    }

    fn base_font(self) -> &'static str {
        const NAMES: [&str; 12] = [
            "Helvetica",
            "Helvetica-Bold",
            "Helvetica-Oblique",
            "Helvetica-BoldOblique",
            "Times-Roman",
            "Times-Bold",
            "Times-Italic",
            "Times-BoldItalic",
            "Courier",
            "Courier-Bold",
            "Courier-Oblique",
            "Courier-BoldOblique",
        ];
        NAMES[self.slot()]
    }

    /// Advance of a WinAnsi byte in thousandths of an em.
    fn units(self, byte: u8) -> f64 {
        let table = match (self.family, self.bold, self.italic) {
            (Family::Mono, ..) => return 600.0,
            (Family::Sans, false, _) => &HELVETICA,
            (Family::Sans, true, _) => &HELVETICA_BOLD,
            (Family::Serif, false, false) => &TIMES_ROMAN,
            (Family::Serif, true, false) => &TIMES_BOLD,
            (Family::Serif, false, true) => &TIMES_ITALIC,
            (Family::Serif, true, true) => &TIMES_BOLDITALIC,
        };
        let width = table[usize::from(byte)];
        f64::from(if width == 0 { 556 } else { width })
    }
}

#[derive(Clone, PartialEq)]
struct Style {
    face: Face,
    size: f64,
    color: Rgb,
    underline: bool,
    link: Option<String>,
}

impl Style {
    fn width(&self, bytes: &[u8]) -> f64 {
        bytes.iter().map(|b| self.face.units(*b)).sum::<f64>() * self.size / 1000.0
    }
}

struct Theme {
    body: Family,
    heading: Family,
    text: Rgb,
    heading_color: Rgb,
    accent: Rgb,
    muted: Rgb,
    header_background: Rgb,
    header_text: Rgb,
    border: Rgb,
    zebra: Rgb,
    code_background: Rgb,
}

fn theme_from(value: &Value) -> Result<Theme> {
    let name = value["preset"].as_str().unwrap_or("modern");
    let (body, heading, colors) = match name {
        "modern" => (
            Family::Sans,
            Family::Sans,
            [
                "1E293B", "0F172A", "2563EB", "64748B", "0F172A", "FFFFFF", "CBD5E1", "F1F5F9",
                "F1F5F9",
            ],
        ),
        "classic" => (
            Family::Serif,
            Family::Serif,
            [
                "111111", "000000", "7F1D1D", "555555", "E5E5E5", "000000", "999999", "F5F5F5",
                "F3F3F3",
            ],
        ),
        "corporate" => (
            Family::Sans,
            Family::Serif,
            [
                "374151", "1E3A5F", "B8923A", "6B7280", "1E3A5F", "FFFFFF", "D1D5DB", "F3F4F6",
                "F3F4F6",
            ],
        ),
        "minimal" => (
            Family::Sans,
            Family::Sans,
            [
                "222222", "000000", "444444", "777777", "FFFFFF", "000000", "DDDDDD", "FAFAFA",
                "F5F5F5",
            ],
        ),
        other => return Err(invalid(&format!("unknown theme preset `{other}`"))),
    };
    let keys = [
        "text",
        "heading",
        "accent",
        "muted",
        "table_header_background",
        "table_header_text",
        "table_border",
        "zebra",
        "code_background",
    ];
    let mut resolved = Vec::new();
    for (key, default) in keys.iter().zip(colors) {
        resolved.push(rgb(value["colors"][*key].as_str().unwrap_or(default))?);
    }
    let family = |key: &str, default: Family| {
        value["fonts"][key]
            .as_str()
            .map_or(Ok(default), Family::parse)
    };
    Ok(Theme {
        body: family("body", body)?,
        heading: family("heading", heading)?,
        text: resolved[0],
        heading_color: resolved[1],
        accent: resolved[2],
        muted: resolved[3],
        header_background: resolved[4],
        header_text: resolved[5],
        border: resolved[6],
        zebra: resolved[7],
        code_background: resolved[8],
    })
}

// ---- text -------------------------------------------------------------------------

/// WinAnsi (Windows-1252) byte for a character, if it has one.
fn win_ansi(c: char) -> Option<u8> {
    let code = c as u32;
    if (0x20..=0x7e).contains(&code) || (0xa0..=0xff).contains(&code) {
        return Some(code as u8);
    }
    Some(match c {
        '€' => 0x80,
        '‚' => 0x82,
        'ƒ' => 0x83,
        '„' => 0x84,
        '…' => 0x85,
        '†' => 0x86,
        '‡' => 0x87,
        'ˆ' => 0x88,
        '‰' => 0x89,
        'Š' => 0x8a,
        '‹' => 0x8b,
        'Œ' => 0x8c,
        'Ž' => 0x8e,
        '‘' => 0x91,
        '’' => 0x92,
        '“' => 0x93,
        '”' => 0x94,
        '•' => 0x95,
        '–' => 0x96,
        '—' => 0x97,
        '˜' => 0x98,
        '™' => 0x99,
        'š' => 0x9a,
        '›' => 0x9b,
        'œ' => 0x9c,
        'ž' => 0x9e,
        'Ÿ' => 0x9f,
        _ => return None,
    })
}

/// Text to WinAnsi bytes. Newlines stay as 0x0A (a forced break); tabs become spaces.
fn encode(text: &str, replaced: &mut usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\t' => out.extend_from_slice(b"    "),
            '\n' => out.push(b'\n'),
            '\r' => {}
            c if c.is_control() => {}
            c => out.push(win_ansi(c).unwrap_or_else(|| {
                *replaced += 1;
                b'?'
            })),
        }
    }
    out
}

fn escape(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for byte in bytes {
        if matches!(byte, b'(' | b')' | b'\\') {
            out.push(b'\\');
        }
        out.push(*byte);
    }
    out
}

fn utf16_hex(text: &str) -> String {
    let mut out = String::from("<FEFF");
    for unit in text.encode_utf16() {
        out.push_str(&format!("{unit:04X}"));
    }
    out.push('>');
    out
}

struct Piece {
    bytes: Vec<u8>,
    style: Style,
}

fn url_ok(url: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://"))
        && url.len() <= 2048
        && url.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// The inline content of a block: `runs` (each optionally bold, italic, underlined,
/// colored, sized, in another font family, or a link) or plain `text`.
fn pieces(value: &Value, base: &Style, theme: &Theme, replaced: &mut usize) -> Result<Vec<Piece>> {
    let mut out = Vec::new();
    if let Some(runs) = value["runs"].as_array() {
        for run in runs {
            let mut style = base.clone();
            if let Some(bold) = run["bold"].as_bool() {
                style.face.bold = bold;
            }
            if let Some(italic) = run["italic"].as_bool() {
                style.face.italic = italic;
            }
            if let Some(underline) = run["underline"].as_bool() {
                style.underline = underline;
            }
            if let Some(color) = run["color"].as_str() {
                style.color = rgb(color)?;
            }
            if let Some(size) = run["size"].as_f64() {
                style.size = size;
            }
            if let Some(font) = run["font"].as_str() {
                style.face.family = Family::parse(font)?;
            }
            if let Some(link) = run["link"].as_str() {
                if !url_ok(link) {
                    return Err(invalid("link must be an http(s) URL of printable ASCII"));
                }
                style.link = Some(link.to_owned());
                style.underline = true;
                if run["color"].is_null() {
                    style.color = theme.accent;
                }
            }
            out.push(Piece {
                bytes: encode(run["text"].as_str().unwrap_or_default(), replaced),
                style,
            });
        }
    } else {
        out.push(Piece {
            bytes: encode(value["text"].as_str().unwrap_or_default(), replaced),
            style: base.clone(),
        });
    }
    Ok(out)
}

#[derive(Clone)]
enum Item {
    Word(Vec<u8>, Style, f64),
    Space(Style, f64),
    Break,
}

struct Line {
    items: Vec<Item>,
    width: f64,
    /// Ended by a forced break or the end of the text: not stretched when justified.
    last: bool,
}

impl Line {
    fn size(&self) -> f64 {
        self.items
            .iter()
            .filter_map(|item| match item {
                Item::Word(_, style, _) | Item::Space(style, _) => Some(style.size),
                Item::Break => None,
            })
            .fold(0.0, f64::max)
    }
}

/// Greedy word wrap of styled pieces into lines no wider than `limit` points. A word
/// wider than the line is broken where it overflows.
fn wrap(pieces: &[Piece], limit: f64) -> Vec<Line> {
    let mut items: Vec<Item> = Vec::new();
    for piece in pieces {
        let mut word: Vec<u8> = Vec::new();
        let flush = |word: &mut Vec<u8>, items: &mut Vec<Item>| {
            if !word.is_empty() {
                let bytes = std::mem::take(word);
                let width = piece.style.width(&bytes);
                items.push(Item::Word(bytes, piece.style.clone(), width));
            }
        };
        for byte in &piece.bytes {
            match byte {
                b' ' => {
                    flush(&mut word, &mut items);
                    if !matches!(items.last(), Some(Item::Space(..))) {
                        items.push(Item::Space(piece.style.clone(), piece.style.width(b" ")));
                    }
                }
                b'\n' => {
                    flush(&mut word, &mut items);
                    items.push(Item::Break);
                }
                other => word.push(*other),
            }
        }
        flush(&mut word, &mut items);
    }
    let mut lines: Vec<Line> = Vec::new();
    let mut current = Line {
        items: vec![],
        width: 0.0,
        last: false,
    };
    let finish = |line: &mut Line, lines: &mut Vec<Line>, last: bool| {
        while matches!(line.items.last(), Some(Item::Space(..))) {
            if let Some(Item::Space(_, width)) = line.items.pop() {
                line.width -= width;
            }
        }
        line.last = last;
        lines.push(std::mem::replace(
            line,
            Line {
                items: vec![],
                width: 0.0,
                last: false,
            },
        ));
    };
    for item in items {
        match item {
            Item::Break => finish(&mut current, &mut lines, true),
            Item::Space(style, width) => {
                if !current.items.is_empty() {
                    current.width += width;
                    current.items.push(Item::Space(style, width));
                }
            }
            Item::Word(bytes, style, width) => {
                if current.width + width > limit
                    && current
                        .items
                        .iter()
                        .any(|item| matches!(item, Item::Word(..)))
                {
                    finish(&mut current, &mut lines, false);
                }
                let (mut bytes, mut width) = (bytes, width);
                while width > limit && bytes.len() > 1 {
                    let mut cut = 1;
                    while cut < bytes.len() - 1
                        && style.width(&bytes[..=cut]) <= limit - current.width
                    {
                        cut += 1;
                    }
                    let head = bytes[..cut].to_vec();
                    current.width += style.width(&head);
                    current.items.push(Item::Word(head, style.clone(), 0.0));
                    finish(&mut current, &mut lines, false);
                    bytes = bytes[cut..].to_vec();
                    width = style.width(&bytes);
                }
                current.width += width;
                current.items.push(Item::Word(bytes, style, width));
            }
        }
    }
    finish(&mut current, &mut lines, true);
    // A trailing empty line only exists if the text ended in a break.
    if lines.len() > 1
        && lines.last().is_some_and(|line| line.items.is_empty())
        && lines[lines.len() - 2].last
    {
        lines.pop();
    }
    lines
}

// ---- pages -----------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Align {
    Left,
    Center,
    Right,
    Justify,
}

impl Align {
    fn parse(value: &Value) -> Result<Self> {
        match value.as_str() {
            None | Some("left") => Ok(Self::Left),
            Some("center") => Ok(Self::Center),
            Some("right") => Ok(Self::Right),
            Some("justify") => Ok(Self::Justify),
            Some(other) => Err(invalid(&format!("unknown alignment `{other}`"))),
        }
    }
}

struct Link {
    rect: [f64; 4],
    uri: String,
}

#[derive(Default)]
struct PageBuf {
    ops: Vec<u8>,
    links: Vec<Link>,
}

struct Doc<'a> {
    theme: &'a Theme,
    pages: Vec<PageBuf>,
    used: [bool; 12],
    replaced: usize,
    width: f64,
    height: f64,
    left: f64,
    top: f64,
    bottom: f64,
    line_height: f64,
    base: Style,
    /// Top of the next line on the current page.
    y: f64,
    blocks: usize,
}

fn fill(ops: &mut Vec<u8>, color: Rgb) {
    ops.extend_from_slice(format!("{:.3} {:.3} {:.3} rg\n", color.0, color.1, color.2).as_bytes());
}
fn stroke(ops: &mut Vec<u8>, color: Rgb) {
    ops.extend_from_slice(format!("{:.3} {:.3} {:.3} RG\n", color.0, color.1, color.2).as_bytes());
}

impl Doc<'_> {
    fn page(&mut self) -> &mut PageBuf {
        self.pages.last_mut().expect("a page always exists")
    }

    fn new_page(&mut self) -> Result<()> {
        if self.pages.len() >= MAX_PAGES {
            return Err(invalid(&format!(
                "the document would exceed {MAX_PAGES} pages"
            )));
        }
        self.pages.push(PageBuf::default());
        self.y = self.top;
        Ok(())
    }

    fn at_top(&self) -> bool {
        (self.y - self.top).abs() < 0.01
    }

    /// Start a new page unless `needed` points still fit on this one.
    fn ensure(&mut self, needed: f64) -> Result<()> {
        if self.y - needed < self.bottom - 0.01 && !self.at_top() {
            self.new_page()?;
        }
        Ok(())
    }

    fn rect(&mut self, x: f64, top: f64, width: f64, height: f64, color: Rgb) {
        let ops = &mut self.page().ops;
        fill(ops, color);
        ops.extend_from_slice(
            format!("{x:.2} {:.2} {width:.2} {height:.2} re f\n", top - height).as_bytes(),
        );
    }

    fn line_segment(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, color: Rgb, thickness: f64) {
        let ops = &mut self.page().ops;
        stroke(ops, color);
        ops.extend_from_slice(
            format!("{thickness:.2} w {x1:.2} {y1:.2} m {x2:.2} {y2:.2} l S\n").as_bytes(),
        );
    }

    /// Draw one wrapped line whose top is `top`, inside the column `[x, x + width]`.
    fn draw_line(&mut self, line: &Line, x: f64, top: f64, width: f64, align: Align) {
        let size = line.size();
        if size == 0.0 {
            return;
        }
        let baseline = top - (size * self.line_height - size) / 2.0 - size * 0.8;
        let spaces = line
            .items
            .iter()
            .filter(|item| matches!(item, Item::Space(..)))
            .count();
        let (mut cursor, extra) = match align {
            Align::Left => (x, 0.0),
            Align::Center => (x + (width - line.width) / 2.0, 0.0),
            Align::Right => (x + width - line.width, 0.0),
            Align::Justify if !line.last && spaces > 0 && line.width < width => {
                (x, (width - line.width) / spaces as f64)
            }
            Align::Justify => (x, 0.0),
        };
        // Consecutive items in the same style are one text run.
        let mut runs: Vec<(Style, Vec<u8>, f64, usize)> = Vec::new();
        for item in &line.items {
            let (bytes, style, advance, is_space): (&[u8], &Style, f64, bool) = match item {
                Item::Word(bytes, style, width) => (bytes, style, *width, false),
                Item::Space(style, width) => (b" ", style, *width, true),
                Item::Break => continue,
            };
            match runs.last_mut() {
                Some(last) if last.0 == *style => {
                    last.1.extend_from_slice(bytes);
                    last.2 += advance;
                    last.3 += usize::from(is_space);
                }
                _ => runs.push((
                    style.clone(),
                    bytes.to_vec(),
                    advance,
                    usize::from(is_space),
                )),
            }
        }
        for (style, bytes, natural, run_spaces) in runs {
            let slot = style.face.slot();
            self.used[slot] = true;
            let total = natural + extra * run_spaces as f64;
            let mut op = Vec::new();
            fill(&mut op, style.color);
            op.extend_from_slice(
                format!(
                    "BT /F{} {:.2} Tf {cursor:.2} {baseline:.2} Td ",
                    slot + 1,
                    style.size
                )
                .as_bytes(),
            );
            if extra > 0.0 {
                op.extend_from_slice(format!("{extra:.3} Tw ").as_bytes());
            }
            op.push(b'(');
            op.extend(escape(&bytes));
            op.extend_from_slice(b") Tj ");
            if extra > 0.0 {
                op.extend_from_slice(b"0 Tw ");
            }
            op.extend_from_slice(b"ET\n");
            self.page().ops.extend(op);
            if style.underline {
                self.line_segment(
                    cursor,
                    baseline - style.size * 0.12,
                    cursor + total,
                    baseline - style.size * 0.12,
                    style.color,
                    (style.size / 16.0).max(0.4),
                );
            }
            if let Some(uri) = &style.link {
                self.page().links.push(Link {
                    rect: [
                        cursor,
                        baseline - style.size * 0.25,
                        cursor + total,
                        baseline + style.size * 0.8,
                    ],
                    uri: uri.clone(),
                });
            }
            cursor += total;
        }
    }

    /// Lay out lines in a column, breaking pages as needed and avoiding a single
    /// stranded line at the top or bottom of a page.
    fn flow(
        &mut self,
        lines: &[Line],
        x: f64,
        width: f64,
        align: Align,
        keep_next: f64,
    ) -> Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let heights: Vec<f64> = lines
            .iter()
            .map(|line| (line.size().max(self.base.size * 0.5)) * self.line_height)
            .collect();
        if lines.len() >= 2 {
            // Orphan control: keep at least two lines together, and the next block's start with a heading.
            let first_two = heights[0] + heights[1];
            self.ensure(first_two + keep_next)?;
        } else {
            self.ensure(heights[0] + keep_next)?;
        }
        let fits = |doc: &Doc, from: usize| -> usize {
            let mut used = 0.0;
            let mut count = 0;
            for height in &heights[from..] {
                if doc.y - used - height < doc.bottom - 0.01 && count > 0 {
                    break;
                }
                used += height;
                count += 1;
            }
            count
        };
        let mut index = 0;
        while index < lines.len() {
            let mut count = fits(self, index);
            let remaining = lines.len() - index;
            // Widow control: do not leave exactly one line for the next page.
            if remaining - count == 1 && count > 2 {
                count -= 1;
            }
            for offset in 0..count {
                let height = heights[index + offset];
                let top = self.y;
                self.draw_line(&lines[index + offset], x, top, width, align);
                self.y -= height;
            }
            index += count;
            if index < lines.len() {
                self.new_page()?;
            }
        }
        Ok(())
    }

    fn gap(&mut self, points: f64) {
        if !self.at_top() {
            self.y = (self.y - points).max(self.bottom);
        }
    }
}

// ---- blocks ----------------------------------------------------------------------

fn base_style(doc: &Doc, value: &Value, family: Family, color: Rgb) -> Result<Style> {
    let mut style = Style {
        face: Face {
            family,
            bold: false,
            italic: false,
        },
        size: doc.base.size,
        color,
        underline: false,
        link: None,
    };
    if let Some(size) = value["size"].as_f64() {
        if !(4.0..=96.0).contains(&size) {
            return Err(invalid("block size must be 4..=96"));
        }
        style.size = size;
    }
    style.face.bold = value["bold"].as_bool().unwrap_or(false);
    style.face.italic = value["italic"].as_bool().unwrap_or(false);
    if let Some(color) = value["color"].as_str() {
        style.color = rgb(color)?;
    }
    Ok(style)
}

fn paragraph_block(doc: &mut Doc, block: &Value) -> Result<()> {
    let style = base_style(doc, block, doc.theme.body, doc.theme.text)?;
    let mut replaced = 0;
    let content = pieces(block, &style, doc.theme, &mut replaced)?;
    doc.replaced += replaced;
    let indent = block["indent"].as_f64().unwrap_or(0.0).clamp(0.0, 200.0);
    let width = doc.width - indent;
    let lines = wrap(&content, width);
    let align = Align::parse(&block["align"])?;
    doc.flow(&lines, doc.left + indent, width, align, 0.0)?;
    let after = block["space_after"]
        .as_f64()
        .unwrap_or(style.size * 0.6)
        .clamp(0.0, 200.0);
    doc.gap(after);
    Ok(())
}

fn heading_block(doc: &mut Doc, block: &Value, title: bool) -> Result<()> {
    let level = if title {
        0
    } else {
        block["level"].as_u64().unwrap_or(1).clamp(1, 3) as usize
    };
    let factor = [2.4, 2.0, 1.5, 1.25][level];
    let mut style = base_style(doc, &json!({}), doc.theme.heading, doc.theme.heading_color)?;
    style.size = doc.base.size * factor;
    style.face.bold = true;
    if let Some(color) = block["color"].as_str() {
        style.color = rgb(color)?;
    }
    let mut replaced = 0;
    let content = pieces(block, &style, doc.theme, &mut replaced)?;
    doc.replaced += replaced;
    let lines = wrap(&content, doc.width);
    if !title {
        doc.gap(style.size * 0.9);
    }
    doc.flow(
        &lines,
        doc.left,
        doc.width,
        Align::parse(&block["align"])?,
        doc.base.size * doc.line_height * 2.0,
    )?;
    if title {
        doc.ensure(style.size * 0.6)?;
        let top = doc.y - style.size * 0.15;
        doc.rect(doc.left, top, 64.0, 2.5, doc.theme.accent);
        doc.y -= style.size * 0.6;
    } else {
        doc.gap(style.size * 0.35);
    }
    Ok(())
}

fn list_block(doc: &mut Doc, block: &Value) -> Result<()> {
    let numbered = block["style"].as_str() == Some("numbered");
    let items = block["items"]
        .as_array()
        .ok_or_else(|| invalid("a list needs items"))?;
    let mut count = 0usize;
    let mut stack: Vec<(&Value, usize)> = items.iter().rev().map(|item| (item, 0usize)).collect();
    let mut numbers = [0usize; 2];
    while let Some((item, level)) = stack.pop() {
        count += 1;
        if count > MAX_LIST_ITEMS {
            return Err(invalid("a list has too many items"));
        }
        let owned;
        let view = if let Some(text) = item.as_str() {
            owned = json!({"text":text});
            &owned
        } else {
            item
        };
        let style = base_style(doc, view, doc.theme.body, doc.theme.text)?;
        let mut replaced = 0;
        let content = pieces(view, &style, doc.theme, &mut replaced)?;
        doc.replaced += replaced;
        numbers[level] += 1;
        if level == 0 {
            numbers[1] = 0;
        }
        let marker_width = 18.0;
        let indent = level as f64 * 20.0;
        let lines = wrap(&content, doc.width - indent - marker_width);
        let marker: String = if numbered && level == 0 {
            format!("{}.", numbers[0])
        } else if level == 0 {
            "\u{2022}".into()
        } else {
            "\u{2013}".into()
        };
        let height = lines.first().map_or(style.size * doc.line_height, |line| {
            line.size().max(style.size) * doc.line_height
        });
        doc.ensure(height * lines.len().min(2) as f64)?;
        let top = doc.y;
        let marker_style = Style {
            color: if numbered && level == 0 {
                style.color
            } else {
                doc.theme.accent
            },
            face: Face {
                bold: numbered && level == 0,
                ..style.face
            },
            ..style.clone()
        };
        let marker_bytes: Vec<u8> = marker
            .chars()
            .map(|c| win_ansi(c).unwrap_or(b'?'))
            .collect();
        let marker_line = Line {
            width: marker_style.width(&marker_bytes),
            items: vec![Item::Word(
                marker_bytes.clone(),
                marker_style.clone(),
                marker_style.width(&marker_bytes),
            )],
            last: true,
        };
        doc.draw_line(
            &marker_line,
            doc.left + indent,
            top,
            marker_width,
            Align::Left,
        );
        doc.flow(
            &lines,
            doc.left + indent + marker_width,
            doc.width - indent - marker_width,
            Align::Left,
            0.0,
        )?;
        doc.gap(style.size * 0.25);
        if level == 0
            && let Some(children) = item["items"].as_array()
        {
            for child in children.iter().rev() {
                stack.push((child, 1));
            }
        }
    }
    doc.gap(doc.base.size * 0.4);
    Ok(())
}

fn code_block(doc: &mut Doc, block: &Value) -> Result<()> {
    let size = (doc.base.size * 0.9).max(6.0);
    let style = Style {
        face: Face {
            family: Family::Mono,
            bold: false,
            italic: false,
        },
        size,
        color: doc.theme.text,
        underline: false,
        link: None,
    };
    let mut replaced = 0;
    let text = encode(block["text"].as_str().unwrap_or_default(), &mut replaced);
    doc.replaced += replaced;
    let pad = 6.0;
    let per_line = (((doc.width - 2.0 * pad) / (0.6 * size)).floor() as usize).max(1);
    let mut lines: Vec<Line> = Vec::new();
    for raw in text.split(|b| *b == b'\n') {
        let chunks: Vec<&[u8]> = if raw.is_empty() {
            vec![&[][..]]
        } else {
            raw.chunks(per_line).collect()
        };
        for chunk in chunks {
            let width = style.width(chunk);
            lines.push(Line {
                items: vec![Item::Word(chunk.to_vec(), style.clone(), width)],
                width,
                last: true,
            });
        }
    }
    let line_h = size * doc.line_height;
    let mut index = 0;
    doc.gap(size * 0.3);
    while index < lines.len() {
        doc.ensure(line_h * 3.0_f64.min((lines.len() - index) as f64) + pad)?;
        let room = ((doc.y - doc.bottom - pad) / line_h).floor().max(1.0) as usize;
        let take = room.min(lines.len() - index);
        let height = take as f64 * line_h + 2.0 * pad;
        let top = doc.y;
        doc.rect(doc.left, top, doc.width, height, doc.theme.code_background);
        doc.rect(doc.left, top, 2.5, height, doc.theme.accent);
        doc.y -= pad;
        for line in &lines[index..index + take] {
            let top = doc.y;
            if line
                .items
                .iter()
                .any(|item| matches!(item, Item::Word(bytes, ..) if !bytes.is_empty()))
            {
                doc.draw_line(
                    line,
                    doc.left + pad + 3.0,
                    top,
                    doc.width - 2.0 * pad,
                    Align::Left,
                );
            }
            doc.y -= line_h;
        }
        doc.y -= pad;
        index += take;
        if index < lines.len() {
            doc.new_page()?;
        }
    }
    doc.gap(size * 0.6);
    Ok(())
}

fn quote_block(doc: &mut Doc, block: &Value) -> Result<()> {
    let mut style = base_style(doc, block, doc.theme.body, doc.theme.muted)?;
    style.face.italic = true;
    let mut replaced = 0;
    let mut content = pieces(block, &style, doc.theme, &mut replaced)?;
    if let Some(author) = block["author"].as_str() {
        let attribution = Style {
            face: Face {
                italic: false,
                ..style.face
            },
            color: doc.theme.muted,
            ..style.clone()
        };
        content.push(Piece {
            bytes: encode(&format!("\n\u{2014} {author}"), &mut replaced),
            style: attribution,
        });
    }
    doc.replaced += replaced;
    let indent = 16.0;
    let lines = wrap(&content, doc.width - indent);
    let total: f64 = lines.iter().map(|line| line.size() * doc.line_height).sum();
    doc.ensure(total.min(doc.top - doc.bottom))?;
    let (top, page_start) = (doc.y, doc.pages.len());
    doc.flow(
        &lines,
        doc.left + indent,
        doc.width - indent,
        Align::Left,
        0.0,
    )?;
    if doc.pages.len() == page_start {
        doc.rect(doc.left + 2.0, top, 2.5, top - doc.y, doc.theme.accent);
    }
    doc.gap(style.size * 0.7);
    Ok(())
}

fn rule_block(doc: &mut Doc) -> Result<()> {
    doc.ensure(doc.base.size)?;
    doc.gap(doc.base.size * 0.3);
    let y = doc.y;
    doc.line_segment(doc.left, y, doc.left + doc.width, y, doc.theme.border, 0.75);
    doc.gap(doc.base.size * 0.7);
    Ok(())
}

struct Cell {
    content: Vec<Piece>,
    lines: Vec<Line>,
    align: Align,
    background: Option<Rgb>,
}

fn table_block(doc: &mut Doc, block: &Value) -> Result<()> {
    let rows = block["rows"]
        .as_array()
        .ok_or_else(|| invalid("a table needs rows"))?;
    if rows.is_empty() || rows.len() > MAX_ROWS {
        return Err(invalid(&format!("a table needs 1..={MAX_ROWS} rows")));
    }
    let header = block["header"].as_bool().unwrap_or(true);
    let zebra = block["zebra"].as_bool().unwrap_or(false);
    let columns = rows
        .iter()
        .map(|row| row.as_array().map_or(0, Vec::len))
        .max()
        .unwrap_or(0);
    if columns == 0 || columns > MAX_COLUMNS {
        return Err(invalid(&format!("a table needs 1..={MAX_COLUMNS} columns")));
    }
    let column_align: Vec<Align> = (0..columns)
        .map(|c| Align::parse(&block["align"][c]))
        .collect::<Result<_>>()?;
    // Natural and minimum widths per column, then scaled to the available width.
    let size = doc.base.size * 0.95;
    let mut cells: Vec<Vec<Cell>> = Vec::new();
    let mut natural = vec![0.0f64; columns];
    let mut minimum = vec![0.0f64; columns];
    let mut replaced = 0;
    for (row_index, row) in rows.iter().enumerate() {
        let row = row
            .as_array()
            .ok_or_else(|| invalid("each table row is an array of cells"))?;
        let head = header && row_index == 0;
        let mut built = Vec::new();
        for column in 0..columns {
            let source = row.get(column).cloned().unwrap_or(Value::Null);
            let value = if source.is_string() {
                json!({"text":source})
            } else if source.is_null() {
                json!({"text":""})
            } else {
                source
            };
            let mut style = base_style(
                doc,
                &value,
                doc.theme.body,
                if head {
                    doc.theme.header_text
                } else {
                    doc.theme.text
                },
            )?;
            style.size = value["size"].as_f64().unwrap_or(size);
            if head {
                style.face.bold = true;
                style.face.family = doc.theme.heading;
            }
            let content = pieces(&value, &style, doc.theme, &mut replaced)?;
            let words: Vec<f64> = content
                .iter()
                .flat_map(|piece| {
                    piece
                        .bytes
                        .split(|b| *b == b' ' || *b == b'\n')
                        .map(|word| piece.style.width(word))
                        .collect::<Vec<_>>()
                })
                .collect();
            let whole: f64 = content
                .iter()
                .map(|piece| {
                    piece
                        .bytes
                        .split(|b| *b == b'\n')
                        .map(|part| piece.style.width(part))
                        .fold(0.0, f64::max)
                })
                .sum();
            natural[column] = natural[column].max(whole);
            minimum[column] = minimum[column].max(words.into_iter().fold(0.0, f64::max));
            let align = if value["align"].is_null() {
                column_align[column]
            } else {
                Align::parse(&value["align"])?
            };
            let background = match value["background"].as_str() {
                Some(color) => Some(rgb(color)?),
                None if head => Some(doc.theme.header_background),
                None if zebra && row_index % 2 == (if header { 0 } else { 1 }) => {
                    Some(doc.theme.zebra)
                }
                None => None,
            };
            built.push(Cell {
                content,
                lines: vec![],
                align,
                background,
            });
        }
        cells.push(built);
    }
    doc.replaced += replaced;
    let available = doc.width;
    let pad = 2.0 * CELL_PAD;
    let weights: Vec<f64> = if let Some(explicit) = block["columns"].as_array() {
        if explicit.len() != columns {
            return Err(invalid("`columns` needs one weight per column"));
        }
        explicit
            .iter()
            .map(|weight| {
                weight
                    .as_f64()
                    .filter(|w| *w > 0.0)
                    .ok_or_else(|| invalid("column weights must be positive numbers"))
            })
            .collect::<Result<_>>()?
    } else {
        natural
            .iter()
            .zip(&minimum)
            .map(|(n, m)| n.max(*m).max(12.0))
            .collect()
    };
    let total: f64 = weights.iter().sum();
    let mut widths: Vec<f64> = weights
        .iter()
        .map(|weight| weight / total * available)
        .collect();
    for (width, min) in widths.iter_mut().zip(&minimum) {
        *width = width.max(min + pad);
    }
    let overflow = widths.iter().sum::<f64>() - available;
    if overflow > 0.01 {
        let flexible: f64 = widths
            .iter()
            .zip(&minimum)
            .map(|(w, m)| (w - m - pad).max(0.0))
            .sum();
        if flexible > 0.0 {
            for (width, min) in widths.iter_mut().zip(&minimum) {
                let slack = (*width - min - pad).max(0.0);
                *width -= overflow * slack / flexible;
            }
        }
    }
    // Wrap every cell into its column.
    let mut rows_laid: Vec<(Vec<Cell>, f64)> = Vec::new();
    for row in cells {
        let mut laid = Vec::new();
        let mut height = 0.0f64;
        for (column, cell) in row.into_iter().enumerate() {
            let inner = (widths[column] - pad).max(4.0);
            let lines = wrap(&cell.content, inner);
            let cell_height: f64 = lines
                .iter()
                .map(|line| line.size().max(size) * doc.line_height)
                .sum();
            height = height.max(cell_height + pad);
            laid.push(Cell {
                content: vec![],
                lines,
                align: cell.align,
                background: cell.background,
            });
        }
        rows_laid.push((laid, height));
    }
    let page_room = doc.top - doc.bottom;
    let head_height = if header { rows_laid[0].1 } else { 0.0 };
    if rows_laid
        .iter()
        .skip(usize::from(header))
        .any(|(_, height)| height + head_height > page_room)
        || rows_laid[0].1 > page_room
    {
        return Err(invalid("a table row is taller than a page"));
    }
    doc.gap(size * 0.4);
    let border = doc.theme.border;
    for index in 0..rows_laid.len() {
        let height = rows_laid[index].1;
        // The header row never ends a page on its own: it travels with the first body row.
        let needed = if header && index == 0 {
            height + rows_laid.get(1).map_or(0.0, |(_, next)| *next)
        } else {
            height
        };
        let before = doc.pages.len();
        if doc.y - needed < doc.bottom - 0.01 && !doc.at_top() {
            doc.new_page()?;
        }
        if header && index > 0 && doc.pages.len() != before {
            // A continued table repeats its header at the top of the new page.
            draw_row(doc, &rows_laid[0].0, head_height, &widths, &border, size);
            doc.y -= head_height;
        }
        draw_row(doc, &rows_laid[index].0, height, &widths, &border, size);
        doc.y -= height;
    }
    doc.gap(size * 0.8);
    Ok(())
}

fn draw_row(doc: &mut Doc, row: &[Cell], height: f64, widths: &[f64], border: &Rgb, size: f64) {
    let top = doc.y;
    let mut x = doc.left;
    for (cell, width) in row.iter().zip(widths) {
        if let Some(color) = cell.background {
            doc.rect(x, top, *width, height, color);
        }
        x += width;
    }
    let mut x = doc.left;
    for (cell, width) in row.iter().zip(widths) {
        let mut y = top - CELL_PAD;
        for line in &cell.lines {
            let line_height = line.size().max(size) * doc.line_height;
            doc.draw_line(line, x + CELL_PAD, y, width - 2.0 * CELL_PAD, cell.align);
            y -= line_height;
        }
        x += width;
    }
    // Grid.
    let mut x = doc.left;
    doc.line_segment(
        doc.left,
        top,
        doc.left + widths.iter().sum::<f64>(),
        top,
        *border,
        0.5,
    );
    doc.line_segment(
        doc.left,
        top - height,
        doc.left + widths.iter().sum::<f64>(),
        top - height,
        *border,
        0.5,
    );
    doc.line_segment(x, top, x, top - height, *border, 0.5);
    for width in widths {
        x += width;
        doc.line_segment(x, top, x, top - height, *border, 0.5);
    }
}

fn block(doc: &mut Doc, value: &Value) -> Result<()> {
    doc.blocks += 1;
    if doc.blocks > MAX_BLOCKS {
        return Err(invalid(&format!("at most {MAX_BLOCKS} blocks")));
    }
    match value["type"].as_str().unwrap_or("paragraph") {
        "heading" => heading_block(doc, value, false),
        "paragraph" => paragraph_block(doc, value),
        "list" => list_block(doc, value),
        "table" => table_block(doc, value),
        "quote" => quote_block(doc, value),
        "code" => code_block(doc, value),
        "rule" => rule_block(doc),
        "spacer" => {
            let height = value["height"].as_f64().unwrap_or(12.0).clamp(0.0, 400.0);
            doc.gap(height);
            Ok(())
        }
        "page_break" => doc.new_page(),
        other => Err(invalid(&format!("unknown block type `{other}`"))),
    }
}

// ---- header, footer and the file -------------------------------------------------

fn decoration(
    doc: &mut Doc,
    spec: &Value,
    header: bool,
    title: &str,
    margins: (f64, f64),
) -> Result<()> {
    if !spec.is_object() {
        return Ok(());
    }
    let pages = doc.pages.len();
    let size = spec["size"]
        .as_f64()
        .unwrap_or(doc.base.size * 0.8)
        .clamp(5.0, 24.0);
    let color = spec["color"].as_str().map_or(Ok(doc.theme.muted), rgb)?;
    let style = Style {
        face: Face {
            family: doc.theme.body,
            bold: false,
            italic: false,
        },
        size,
        color,
        underline: false,
        link: None,
    };
    let band = if header { margins.0 } else { margins.1 };
    if band < size * 2.2 {
        return Err(invalid("the margin is too small for the header or footer"));
    }
    let y_top = if header {
        doc.height - band / 2.0 + size
    } else {
        band / 2.0 + size
    };
    for number in 1..=pages {
        for (slot, align) in [
            ("left", Align::Left),
            ("center", Align::Center),
            ("right", Align::Right),
        ] {
            let Some(template) = spec[slot].as_str() else {
                continue;
            };
            let text = template
                .replace("{page}", &number.to_string())
                .replace("{pages}", &pages.to_string())
                .replace("{title}", title);
            let mut replaced = 0;
            let bytes = encode(&text, &mut replaced);
            doc.replaced += replaced;
            let line = Line {
                width: style.width(&bytes),
                items: vec![Item::Word(
                    bytes.clone(),
                    style.clone(),
                    style.width(&bytes),
                )],
                last: true,
            };
            doc.pages.swap(number - 1, pages - 1);
            let saved_y = doc.y;
            doc.draw_line(&line, doc.left, y_top, doc.width, align);
            doc.y = saved_y;
            doc.pages.swap(number - 1, pages - 1);
        }
        if spec["rule"].as_bool().unwrap_or(false) {
            let y = if header {
                doc.height - band + 4.0
            } else {
                band - 4.0
            };
            doc.pages.swap(number - 1, pages - 1);
            doc.line_segment(doc.left, y, doc.left + doc.width, y, doc.theme.border, 0.5);
            doc.pages.swap(number - 1, pages - 1);
        }
    }
    Ok(())
}

pub(super) fn write(input: &Value) -> Result<(Vec<u8>, Value)> {
    let blocks = input["blocks"].as_array();
    let content = input["content"].as_str();
    if blocks.is_some() == content.is_some() {
        return Err(invalid(
            "provide exactly one of `content` (plain text) and `blocks` (styled blocks)",
        ));
    }
    if content.is_some_and(|text| text.chars().count() > MAX_CHARS) {
        return Err(invalid("content is too long"));
    }
    let (mut page_w, mut page_h) = match input["page_size"].as_str().unwrap_or("A4") {
        "A4" => (595.28, 841.89),
        "Letter" => (612.0, 792.0),
        other => return Err(invalid(&format!("unknown page size `{other}`"))),
    };
    if input["landscape"].as_bool() == Some(true) {
        std::mem::swap(&mut page_w, &mut page_h);
    }
    let all = input["margin"].as_f64().unwrap_or(50.0);
    let side = |key: &str| input["margins"][key].as_f64().unwrap_or(all);
    let (top_m, right_m, bottom_m, left_m) =
        (side("top"), side("right"), side("bottom"), side("left"));
    let size = input["font_size"].as_f64().unwrap_or(12.0);
    let line_height = input["line_height"].as_f64().unwrap_or(1.4);
    if ![top_m, right_m, bottom_m, left_m]
        .iter()
        .all(|m| (0.0..=200.0).contains(m))
        || !(6.0..=72.0).contains(&size)
        || !(1.0..=3.0).contains(&line_height)
    {
        return Err(invalid(
            "margins must be 0..=200, font_size 6..=72 and line_height 1..=3",
        ));
    }
    let (width, height) = (page_w - left_m - right_m, page_h - top_m - bottom_m);
    if width < size * 4.0 || height < size * 4.0 {
        return Err(invalid("the margins leave no room for text on the page"));
    }
    let theme = theme_from(&input["theme"])?;
    let title = input["title"].as_str().filter(|title| !title.is_empty());
    let base = Style {
        face: Face {
            family: theme.body,
            bold: false,
            italic: false,
        },
        size,
        color: theme.text,
        underline: false,
        link: None,
    };
    let mut doc = Doc {
        theme: &theme,
        pages: vec![PageBuf::default()],
        used: [false; 12],
        replaced: 0,
        width,
        height: page_h,
        left: left_m,
        top: page_h - top_m,
        bottom: bottom_m,
        line_height,
        base,
        y: page_h - top_m,
        blocks: 0,
    };
    doc.width = width;
    if let Some(title) = title {
        heading_block(&mut doc, &json!({"text":title}), true)?;
    }
    match (blocks, content) {
        (Some(blocks), _) => {
            for item in blocks {
                block(&mut doc, item)?;
            }
        }
        (None, Some(text)) => {
            // Plain text keeps every newline as a line break and its blank lines.
            let style = doc.base.clone();
            let mut replaced = 0;
            let pieces = vec![Piece {
                bytes: encode(text, &mut replaced),
                style,
            }];
            doc.replaced += replaced;
            let lines = wrap(&pieces, width);
            doc.flow(&lines, doc.left, width, Align::Left, 0.0)?;
        }
        _ => unreachable!(),
    }
    let title_text = title.unwrap_or_default();
    decoration(
        &mut doc,
        &input["header"],
        true,
        title_text,
        (top_m, bottom_m),
    )?;
    decoration(
        &mut doc,
        &input["footer"],
        false,
        title_text,
        (top_m, bottom_m),
    )?;

    // Serialize: catalog, pages, the fonts used, then each page's content, links and page.
    let mut objects: Vec<Vec<u8>> = vec![b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(), Vec::new()];
    let mut font_ids = [0usize; 12];
    for slot in 0..12 {
        if doc.used[slot] || slot == 0 {
            let face = Face {
                family: [Family::Sans, Family::Serif, Family::Mono][slot / 4],
                bold: slot % 2 == 1,
                italic: slot / 2 % 2 == 1,
            };
            objects.push(
                format!(
                    "<< /Type /Font /Subtype /Type1 /BaseFont /{} /Encoding /WinAnsiEncoding >>",
                    face.base_font()
                )
                .into_bytes(),
            );
            font_ids[slot] = objects.len();
        }
    }
    let fonts: String = (0..12)
        .filter(|slot| font_ids[*slot] != 0)
        .map(|slot| format!("/F{} {} 0 R ", slot + 1, font_ids[slot]))
        .collect();
    let mut page_ids = Vec::new();
    for page in &doc.pages {
        let packed = miniz_oxide::deflate::compress_to_vec_zlib(&page.ops, 6);
        let mut body = format!(
            "<< /Length {} /Filter /FlateDecode >>\nstream\n",
            packed.len()
        )
        .into_bytes();
        body.extend(packed);
        body.extend_from_slice(b"\nendstream");
        objects.push(body);
        let content_id = objects.len();
        let mut annotation_ids = Vec::new();
        for link in &page.links {
            let uri: String = link
                .uri
                .chars()
                .flat_map(|c| {
                    if matches!(c, '(' | ')' | '\\') {
                        vec!['\\', c]
                    } else {
                        vec![c]
                    }
                })
                .collect();
            objects.push(format!("<< /Type /Annot /Subtype /Link /Rect [{:.2} {:.2} {:.2} {:.2}] /Border [0 0 0] /A << /S /URI /URI ({uri}) >> >>", link.rect[0], link.rect[1], link.rect[2], link.rect[3]).into_bytes());
            annotation_ids.push(objects.len());
        }
        let annots = if annotation_ids.is_empty() {
            String::new()
        } else {
            format!(
                " /Annots [{}]",
                annotation_ids
                    .iter()
                    .map(|id| format!("{id} 0 R"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        };
        objects.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {page_w:.2} {page_h:.2}] /Resources << /Font << {fonts}>> >> /Contents {content_id} 0 R{annots} >>").into_bytes());
        page_ids.push(objects.len());
    }
    let kids: String = page_ids
        .iter()
        .map(|id| format!("{id} 0 R"))
        .collect::<Vec<_>>()
        .join(" ");
    objects[1] = format!(
        "<< /Type /Pages /Kids [{kids}] /Count {} >>",
        page_ids.len()
    )
    .into_bytes();
    let mut info = String::from("<< /Creator (Hive SDK) /Producer (Hive SDK)");
    if let Some(title) = title {
        info.push_str(&format!(" /Title {}", utf16_hex(title)));
    }
    for (key, field) in [("Author", "author"), ("Subject", "subject")] {
        if let Some(value) = input[field]
            .as_str()
            .filter(|value| !value.is_empty() && value.chars().count() <= 500)
        {
            info.push_str(&format!(" /{key} {}", utf16_hex(value)));
        }
    }
    info.push_str(" >>");
    objects.push(info.into_bytes());
    let info_id = objects.len();

    let mut out = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::new();
    for (index, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(
        format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes(),
    );
    for offset in offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R /Info {info_id} 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    Ok((
        out,
        json!({"pages":doc.pages.len(),"replaced_characters":doc.replaced}),
    ))
}
