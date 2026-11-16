//! Text extraction from PDF.
//!
//! The file is scanned for every `n g obj … endobj` (the last definition of an
//! object wins, which is what incremental updates mean), object streams are
//! expanded, and the page tree is walked in order. Text comes from the content
//! streams, decoded with each font's `ToUnicode` map or simple encoding. This is
//! an extractor, not a renderer: encrypted files, fonts with no Unicode mapping,
//! scanned images and exotic filters are reported as warnings, never guessed.
use super::{Deadline, invalid};
use crate::Result;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    rc::Rc,
    sync::OnceLock,
};

const MAX_OBJECTS: usize = 2_000_000;
const MAX_STREAM: usize = 32 * 1024 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_PAGES: usize = 100_000;
pub(super) const MAX_PAGES_PER_REQUEST: usize = 200;
const MAX_TEXT_CHARS: usize = 4_000_000;
const MAX_CMAP: usize = 200_000;

type Dict = BTreeMap<String, Obj>;

#[derive(Clone, Debug)]
enum Obj {
    Null,
    Bool,
    Num(f64),
    Str(Vec<u8>),
    Name(String),
    Array(Vec<Obj>),
    Dict(Dict),
    Ref(u32),
    Stream(Rc<Stream>),
}

#[derive(Debug)]
struct Stream {
    dict: Dict,
    start: usize,
    len: usize,
}

fn is_space(byte: u8) -> bool {
    matches!(byte, 0 | 9 | 10 | 12 | 13 | 32)
}
fn is_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

struct Lexer<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn skip_space(&mut self) {
        while let Some(byte) = self.data.get(self.pos) {
            if is_space(*byte) {
                self.pos += 1;
            } else if *byte == b'%' {
                while self
                    .data
                    .get(self.pos)
                    .is_some_and(|b| !matches!(b, 10 | 13))
                {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    fn token(&mut self) -> &'a [u8] {
        let start = self.pos;
        while self
            .data
            .get(self.pos)
            .is_some_and(|b| !is_space(*b) && !is_delimiter(*b))
        {
            self.pos += 1;
        }
        &self.data[start..self.pos]
    }

    fn number(token: &[u8]) -> Option<f64> {
        let text = std::str::from_utf8(token).ok()?;
        if text.is_empty()
            || !text
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.'))
        {
            return None;
        }
        text.parse::<f64>().ok()
    }

    fn parse(&mut self, depth: usize) -> Result<Obj> {
        if depth > MAX_DEPTH {
            return Err(invalid("PDF objects are nested too deeply"));
        }
        self.skip_space();
        let byte = *self
            .data
            .get(self.pos)
            .ok_or_else(|| invalid("unexpected end of PDF data"))?;
        match byte {
            b'<' if self.data.get(self.pos + 1) == Some(&b'<') => {
                self.pos += 2;
                let mut dict = Dict::new();
                loop {
                    self.skip_space();
                    if self.data[self.pos..].starts_with(b">>") {
                        self.pos += 2;
                        return Ok(Obj::Dict(dict));
                    }
                    let Obj::Name(key) = self.parse(depth + 1)? else {
                        return Err(invalid("dictionary key is not a name"));
                    };
                    let value = self.parse(depth + 1)?;
                    dict.insert(key, value);
                }
            }
            b'<' => {
                self.pos += 1;
                let mut digits = Vec::new();
                while let Some(byte) = self.data.get(self.pos).copied() {
                    self.pos += 1;
                    match byte {
                        b'>' => break,
                        byte if byte.is_ascii_hexdigit() => digits.push(byte),
                        _ => {}
                    }
                }
                if digits.len() % 2 == 1 {
                    digits.push(b'0');
                }
                let bytes = digits
                    .chunks(2)
                    .map(|pair| {
                        u8::from_str_radix(std::str::from_utf8(pair).unwrap_or("0"), 16)
                            .unwrap_or(0)
                    })
                    .collect();
                Ok(Obj::Str(bytes))
            }
            b'[' => {
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    self.skip_space();
                    match self.data.get(self.pos) {
                        Some(b']') => {
                            self.pos += 1;
                            return Ok(Obj::Array(items));
                        }
                        None => return Err(invalid("unterminated array")),
                        _ => items.push(self.parse(depth + 1)?),
                    }
                }
            }
            b'(' => {
                self.pos += 1;
                let (mut out, mut nesting) = (Vec::new(), 1);
                while let Some(byte) = self.data.get(self.pos).copied() {
                    self.pos += 1;
                    match byte {
                        b'(' => {
                            nesting += 1;
                            out.push(byte);
                        }
                        b')' => {
                            nesting -= 1;
                            if nesting == 0 {
                                return Ok(Obj::Str(out));
                            }
                            out.push(byte);
                        }
                        b'\\' => {
                            let escaped = self.data.get(self.pos).copied().unwrap_or(b'\\');
                            self.pos += 1;
                            match escaped {
                                b'n' => out.push(b'\n'),
                                b'r' => out.push(b'\r'),
                                b't' => out.push(b'\t'),
                                b'b' => out.push(8),
                                b'f' => out.push(12),
                                b'0'..=b'7' => {
                                    let mut value = u32::from(escaped - b'0');
                                    for _ in 0..2 {
                                        match self.data.get(self.pos) {
                                            Some(next @ b'0'..=b'7') => {
                                                value = value * 8 + u32::from(next - b'0');
                                                self.pos += 1;
                                            }
                                            _ => break,
                                        }
                                    }
                                    out.push(value as u8);
                                }
                                b'\r' => {
                                    if self.data.get(self.pos) == Some(&b'\n') {
                                        self.pos += 1;
                                    }
                                }
                                b'\n' => {}
                                other => out.push(other),
                            }
                        }
                        other => out.push(other),
                    }
                }
                Err(invalid("unterminated string"))
            }
            b'/' => {
                self.pos += 1;
                let raw = self.token();
                let mut name = Vec::new();
                let mut at = 0;
                while at < raw.len() {
                    if raw[at] == b'#'
                        && at + 2 < raw.len()
                        && let Ok(value) = u8::from_str_radix(
                            std::str::from_utf8(&raw[at + 1..at + 3]).unwrap_or("zz"),
                            16,
                        )
                    {
                        name.push(value);
                        at += 3;
                    } else {
                        name.push(raw[at]);
                        at += 1;
                    }
                }
                Ok(Obj::Name(String::from_utf8_lossy(&name).into_owned()))
            }
            b')' | b'>' | b']' | b'{' | b'}' => Err(invalid("unexpected PDF delimiter")),
            _ => {
                let token = self.token();
                if token.is_empty() {
                    self.pos += 1;
                    return Err(invalid("unexpected PDF byte"));
                }
                match token {
                    b"true" | b"false" => return Ok(Obj::Bool),
                    b"null" => return Ok(Obj::Null),
                    _ => {}
                }
                let Some(number) = Self::number(token) else {
                    return Err(invalid("unexpected PDF keyword"));
                };
                // `n g R` is an indirect reference.
                if number >= 0.0 && number.fract() == 0.0 && token.iter().all(u8::is_ascii_digit) {
                    let saved = self.pos;
                    self.skip_space();
                    let generation = self.token();
                    if !generation.is_empty() && generation.iter().all(u8::is_ascii_digit) {
                        self.skip_space();
                        if self.data.get(self.pos) == Some(&b'R')
                            && self
                                .data
                                .get(self.pos + 1)
                                .is_none_or(|b| is_space(*b) || is_delimiter(*b))
                        {
                            self.pos += 1;
                            return Ok(Obj::Ref(number as u32));
                        }
                    }
                    self.pos = saved;
                }
                Ok(Obj::Num(number))
            }
        }
    }
}

fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|at| at + from)
}

struct Document<'a> {
    data: &'a [u8],
    objects: HashMap<u32, Obj>,
    trailer: Dict,
    warnings: Vec<String>,
}

impl<'a> Document<'a> {
    fn load(data: &'a [u8], deadline: &Deadline) -> Result<Self> {
        if find(data, b"%PDF-", 0).is_none_or(|at| at > 1024) {
            return Err(invalid("not a PDF file"));
        }
        let mut document = Self {
            data,
            objects: HashMap::new(),
            trailer: Dict::new(),
            warnings: Vec::new(),
        };
        let mut at = 0;
        let mut scanned = 0usize;
        while let Some(found) = find(data, b"obj", at) {
            at = found + 3;
            scanned += 1;
            if scanned.is_multiple_of(4096) {
                deadline.check("scanning the PDF")?;
            }
            // `<number> <generation> obj`: whitespace before, not part of `endobj`/`object`.
            if found == 0
                || !is_space(data[found - 1])
                || data.get(found + 3).is_some_and(|b| b.is_ascii_alphabetic())
            {
                continue;
            }
            let mut back = found - 1;
            while back > 0 && is_space(data[back]) {
                back -= 1;
            }
            let generation_end = back + 1;
            while back > 0 && data[back].is_ascii_digit() {
                back -= 1;
            }
            if generation_end == back + 1 || !is_space(data[back]) {
                continue;
            }
            while back > 0 && is_space(data[back]) {
                back -= 1;
            }
            let number_end = back + 1;
            while back > 0 && data[back].is_ascii_digit() {
                back -= 1;
            }
            let number_start = if data[back].is_ascii_digit() {
                back
            } else {
                back + 1
            };
            let Some(number) = std::str::from_utf8(&data[number_start..number_end])
                .ok()
                .and_then(|text| text.parse::<u32>().ok())
            else {
                continue;
            };
            let mut lexer = Lexer {
                data,
                pos: found + 3,
            };
            let Ok(value) = lexer.parse(0) else { continue };
            let value = match value {
                Obj::Dict(dict) => {
                    lexer.skip_space();
                    if data[lexer.pos.min(data.len())..].starts_with(b"stream") {
                        let mut start = lexer.pos + 6;
                        if data.get(start) == Some(&b'\r') {
                            start += 1;
                        }
                        if data.get(start) == Some(&b'\n') {
                            start += 1;
                        }
                        let declared = match dict.get("Length") {
                            Some(Obj::Num(length)) if *length >= 0.0 => Some(*length as usize),
                            _ => None,
                        };
                        let end = declared
                            .filter(|length| {
                                let mut check = Lexer {
                                    data,
                                    pos: start + length,
                                };
                                check.skip_space();
                                data.get(start..start + length).is_some()
                                    && data[check.pos.min(data.len())..].starts_with(b"endstream")
                            })
                            .map(|length| start + length)
                            .or_else(|| {
                                find(data, b"endstream", start).map(|stop| {
                                    let mut end = stop;
                                    if end > start && data[end - 1] == b'\n' {
                                        end -= 1;
                                    }
                                    if end > start && data[end - 1] == b'\r' {
                                        end -= 1;
                                    }
                                    end
                                })
                            });
                        match end {
                            Some(end) => {
                                at = end;
                                Obj::Stream(Rc::new(Stream {
                                    dict,
                                    start,
                                    len: end - start,
                                }))
                            }
                            None => Obj::Dict(dict),
                        }
                    } else {
                        Obj::Dict(dict)
                    }
                }
                other => other,
            };
            if document.objects.len() >= MAX_OBJECTS {
                return Err(invalid("the PDF has too many objects"));
            }
            document.objects.insert(number, value);
        }
        // Trailers: classic `trailer << … >>` dictionaries and cross-reference streams.
        let mut from = 0;
        while let Some(found) = find(data, b"trailer", from) {
            from = found + 7;
            if let Ok(Obj::Dict(dict)) = (Lexer { data, pos: from }).parse(0) {
                document.trailer.extend(dict);
            }
        }
        let xrefs: Vec<Dict> = document
            .objects
            .values()
            .filter_map(|object| match object {
                Obj::Stream(stream) if matches!(stream.dict.get("Type"), Some(Obj::Name(name)) if name == "XRef") => Some(stream.dict.clone()),
                _ => None,
            })
            .collect();
        for dict in xrefs {
            for key in ["Root", "Encrypt", "Info"] {
                if let Some(value) = dict.get(key) {
                    document.trailer.insert(key.to_owned(), value.clone());
                }
            }
        }
        if document.trailer.contains_key("Encrypt") {
            return Err(invalid("encrypted PDFs are not supported"));
        }
        document.expand_object_streams(deadline)?;
        Ok(document)
    }

    fn expand_object_streams(&mut self, deadline: &Deadline) -> Result<()> {
        let streams: Vec<Rc<Stream>> = self
            .objects
            .values()
            .filter_map(|object| match object {
                Obj::Stream(stream) if matches!(stream.dict.get("Type"), Some(Obj::Name(name)) if name == "ObjStm") => Some(stream.clone()),
                _ => None,
            })
            .collect();
        for stream in streams {
            deadline.check("reading object streams")?;
            let Ok(bytes) = self.decode(&stream) else {
                continue;
            };
            let count = self.number(stream.dict.get("N")).unwrap_or(0.0) as usize;
            let first = self.number(stream.dict.get("First")).unwrap_or(0.0) as usize;
            let mut header = Lexer {
                data: &bytes,
                pos: 0,
            };
            for _ in 0..count.min(100_000) {
                header.skip_space();
                let (Ok(Obj::Num(number)), Ok(Obj::Num(offset))) =
                    (header.parse(0), header.parse(0))
                else {
                    break;
                };
                let mut inner = Lexer {
                    data: &bytes,
                    pos: first.saturating_add(offset as usize),
                };
                if let Ok(value) = inner.parse(0) {
                    self.objects.entry(number as u32).or_insert(value);
                }
            }
        }
        Ok(())
    }

    fn resolve<'b>(&'b self, object: &'b Obj) -> &'b Obj {
        let mut current = object;
        for _ in 0..16 {
            match current {
                Obj::Ref(number) => match self.objects.get(number) {
                    Some(next) => current = next,
                    None => return &Obj::Null,
                },
                other => return other,
            }
        }
        &Obj::Null
    }

    fn get<'b>(&'b self, dict: &'b Dict, key: &str) -> &'b Obj {
        dict.get(key)
            .map_or(&Obj::Null, |object| self.resolve(object))
    }

    fn number(&self, object: Option<&Obj>) -> Option<f64> {
        match self.resolve(object?) {
            Obj::Num(value) => Some(*value),
            _ => None,
        }
    }

    fn dict_of<'b>(&'b self, object: &'b Obj) -> Option<&'b Dict> {
        match self.resolve(object) {
            Obj::Dict(dict) => Some(dict),
            Obj::Stream(stream) => Some(&stream.dict),
            _ => None,
        }
    }

    fn name<'b>(&'b self, dict: &'b Dict, key: &str) -> Option<&'b str> {
        match self.get(dict, key) {
            Obj::Name(name) => Some(name),
            _ => None,
        }
    }

    /// Decoded bytes of a stream; filters beyond Flate and the ASCII ones fail.
    fn decode(&self, stream: &Stream) -> Result<Vec<u8>> {
        let mut bytes = self.data[stream.start..stream.start + stream.len].to_vec();
        let filters: Vec<String> = match self.get(&stream.dict, "Filter") {
            Obj::Name(name) => vec![name.clone()],
            Obj::Array(items) => items
                .iter()
                .filter_map(|item| match self.resolve(item) {
                    Obj::Name(name) => Some(name.clone()),
                    _ => None,
                })
                .collect(),
            _ => vec![],
        };
        let parameters = match self.get(&stream.dict, "DecodeParms") {
            Obj::Dict(dict) => Some(dict),
            Obj::Array(items) => items.first().and_then(|item| self.dict_of(item)),
            _ => None,
        };
        if parameters.is_some_and(|dict| {
            self.number(dict.get("Predictor"))
                .is_some_and(|value| value > 1.0)
        }) {
            return Err(invalid("stream predictors are not supported"));
        }
        for filter in filters {
            bytes = match filter.as_str() {
                "FlateDecode" | "Fl" => {
                    miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&bytes, MAX_STREAM)
                        .or_else(|_| {
                            miniz_oxide::inflate::decompress_to_vec_with_limit(
                                bytes.get(2..).unwrap_or_default(),
                                MAX_STREAM,
                            )
                        })
                        .map_err(|_| invalid("corrupt or oversized Flate stream"))?
                }
                "ASCIIHexDecode" | "AHx" => {
                    let digits: Vec<u8> = bytes
                        .iter()
                        .copied()
                        .take_while(|b| *b != b'>')
                        .filter(u8::is_ascii_hexdigit)
                        .collect();
                    digits
                        .chunks(2)
                        .map(|pair| {
                            u8::from_str_radix(
                                &format!(
                                    "{}{}",
                                    pair[0] as char,
                                    *pair.get(1).unwrap_or(&b'0') as char
                                ),
                                16,
                            )
                            .unwrap_or(0)
                        })
                        .collect()
                }
                "ASCII85Decode" | "A85" => ascii85(&bytes)?,
                other => return Err(invalid(&format!("unsupported stream filter `{other}`"))),
            };
        }
        if bytes.len() > MAX_STREAM {
            return Err(invalid("stream exceeds the size limit"));
        }
        Ok(bytes)
    }

    fn pages(&self) -> Result<Vec<(Dict, Dict)>> {
        let root = match self.trailer.get("Root").map(|root| self.resolve(root)) {
            Some(Obj::Dict(root)) => root,
            _ => self
                .objects
                .values()
                .find_map(|object| match object {
                    Obj::Dict(dict) if matches!(dict.get("Type"), Some(Obj::Name(name)) if name == "Catalog") => Some(dict),
                    _ => None,
                })
                .ok_or_else(|| invalid("the PDF has no document catalog"))?,
        };
        let mut pages = Vec::new();
        let mut seen = HashSet::new();
        let start = root.get("Pages").cloned().unwrap_or(Obj::Null);
        self.walk(&start, &Dict::new(), &mut pages, &mut seen, 0)?;
        Ok(pages)
    }

    fn walk(
        &self,
        node: &Obj,
        inherited: &Dict,
        pages: &mut Vec<(Dict, Dict)>,
        seen: &mut HashSet<u32>,
        depth: usize,
    ) -> Result<()> {
        if depth > 32 || pages.len() > MAX_PAGES {
            return Err(invalid("the page tree is too deep or too large"));
        }
        if let Obj::Ref(number) = node
            && !seen.insert(*number)
        {
            return Ok(());
        }
        let Some(dict) = self.dict_of(node) else {
            return Ok(());
        };
        let mut resources = inherited.clone();
        if let Some(own) = dict
            .get("Resources")
            .and_then(|resources| self.dict_of(resources))
        {
            resources = own.clone();
        }
        match self.get(dict, "Kids") {
            Obj::Array(kids) => {
                for kid in kids {
                    self.walk(kid, &resources, pages, seen, depth + 1)?;
                }
            }
            _ => pages.push((dict.clone(), resources)),
        }
        Ok(())
    }

    fn text_of_info(&self, key: &str) -> Option<String> {
        let info = self.dict_of(self.trailer.get("Info")?)?;
        match self.get(info, key) {
            Obj::Str(bytes) if bytes.starts_with(&[0xfe, 0xff]) => {
                let units: Vec<u16> = bytes[2..]
                    .chunks(2)
                    .filter(|pair| pair.len() == 2)
                    .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
                    .collect();
                Some(String::from_utf16_lossy(&units).chars().take(500).collect())
            }
            Obj::Str(bytes) => Some(bytes.iter().map(|b| *b as char).take(500).collect()),
            _ => None,
        }
    }
}

fn ascii85(bytes: &[u8]) -> Result<Vec<u8>> {
    let (mut out, mut group, mut count) = (Vec::new(), 0u32, 0);
    let body = bytes.strip_prefix(b"<~").unwrap_or(bytes);
    for byte in body {
        match byte {
            b'~' => break,
            byte if is_space(*byte) => {}
            b'z' if count == 0 => out.extend_from_slice(&[0; 4]),
            b'!'..=b'u' => {
                group = group
                    .checked_mul(85)
                    .and_then(|g| g.checked_add(u32::from(byte - b'!')))
                    .ok_or_else(|| invalid("corrupt ASCII85 stream"))?;
                count += 1;
                if count == 5 {
                    out.extend_from_slice(&group.to_be_bytes());
                    (group, count) = (0, 0);
                }
            }
            _ => return Err(invalid("corrupt ASCII85 stream")),
        }
    }
    if count > 1 {
        for _ in count..5 {
            group = group.wrapping_mul(85).wrapping_add(84);
        }
        out.extend_from_slice(&group.to_be_bytes()[..count - 1]);
    }
    Ok(out)
}

// ---- fonts -----------------------------------------------------------------------

#[derive(Default)]
struct Font {
    composite: bool,
    unicode: HashMap<u32, String>,
    table: Vec<Option<char>>,
    code_bytes: usize,
    /// Advance widths by character code, in thousandths of an em.
    widths: HashMap<u32, f64>,
    default_width: f64,
}

const WIN_ANSI_HIGH: &str = "€\u{0}‚ƒ„…†‡ˆ‰Š‹Œ\u{0}Ž\u{0}\u{0}‘’“”•–—˜™š›œ\u{0}žŸ";
const MAC_ROMAN_HIGH: &str = "ÄÅÇÉÑÖÜáàâäãåçéèêëíìîïñóòôöõúùûü†°¢£§•¶ß®©™´¨≠ÆØ∞±≤≥¥µ∂∑∏π∫ªºΩæø¿¡¬√ƒ≈∆«»… ÀÃÕŒœ–—“”‘’÷◊ÿŸ⁄€‹›ﬁﬂ‡·‚„‰ÂÊÁËÈÍÎÏÌÓÔ\u{f8ff}ÒÚÛÙıˆ˜¯˘˙˚¸˝˛ˇ";

fn win_ansi_table() -> Vec<Option<char>> {
    let high: Vec<char> = WIN_ANSI_HIGH.chars().collect();
    (0..256u32)
        .map(|code| match code {
            0x20..=0x7e | 0xa0..=0xff => char::from_u32(code),
            0x80..=0x9f => high
                .get((code - 0x80) as usize)
                .copied()
                .filter(|c| *c != '\u{0}'),
            _ => None,
        })
        .collect()
}

fn mac_roman_table() -> Vec<Option<char>> {
    let high: Vec<char> = MAC_ROMAN_HIGH.chars().collect();
    (0..256u32)
        .map(|code| match code {
            0x20..=0x7e => char::from_u32(code),
            0x80..=0xff => high.get((code - 0x80) as usize).copied(),
            _ => None,
        })
        .collect()
}

const GLYPHS: &str = "space: exclam:! quotedbl:\" numbersign:# dollar:$ percent:% ampersand:& quotesingle:' quoteright:’ quoteleft:‘ parenleft:( parenright:) asterisk:* plus:+ comma:, hyphen:- minus:− period:. slash:/ colon:: semicolon:; less:< equal:= greater:> question:? at:@ bracketleft:[ backslash:\\ bracketright:] asciicircum:^ underscore:_ grave:` braceleft:{ bar:| braceright:} asciitilde:~ zero:0 one:1 two:2 three:3 four:4 five:5 six:6 seven:7 eight:8 nine:9 Agrave:À Aacute:Á Acircumflex:Â Atilde:Ã Adieresis:Ä Aring:Å AE:Æ Ccedilla:Ç Egrave:È Eacute:É Ecircumflex:Ê Edieresis:Ë Igrave:Ì Iacute:Í Icircumflex:Î Idieresis:Ï Ntilde:Ñ Ograve:Ò Oacute:Ó Ocircumflex:Ô Otilde:Õ Odieresis:Ö Oslash:Ø Ugrave:Ù Uacute:Ú Ucircumflex:Û Udieresis:Ü Yacute:Ý germandbls:ß agrave:à aacute:á acircumflex:â atilde:ã adieresis:ä aring:å ae:æ ccedilla:ç egrave:è eacute:é ecircumflex:ê edieresis:ë igrave:ì iacute:í icircumflex:î idieresis:ï ntilde:ñ ograve:ò oacute:ó ocircumflex:ô otilde:õ odieresis:ö oslash:ø ugrave:ù uacute:ú ucircumflex:û udieresis:ü yacute:ý ydieresis:ÿ endash:– emdash:— quotedblleft:“ quotedblright:” quotesinglbase:‚ quotedblbase:„ bullet:• ellipsis:… fi:ﬁ fl:ﬂ Euro:€ trademark:™ copyright:© registered:® degree:° section:§ paragraph:¶ exclamdown:¡ questiondown:¿ guillemotleft:« guillemotright:» multiply:× divide:÷ plusminus:± mu:µ sterling:£ yen:¥ cent:¢ currency:¤ brokenbar:¦ dieresis:¨ macron:¯ acute:´ cedilla:¸ ordfeminine:ª ordmasculine:º onesuperior:¹ twosuperior:² threesuperior:³ onequarter:¼ onehalf:½ threequarters:¾ logicalnot:¬ Scaron:Š scaron:š Zcaron:Ž zcaron:ž OE:Œ oe:œ Ydieresis:Ÿ dagger:† daggerdbl:‡ perthousand:‰ florin:ƒ circumflex:ˆ tilde:˜";

fn glyph_char(name: &str) -> Option<char> {
    static TABLE: OnceLock<HashMap<&'static str, char>> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        GLYPHS
            .split(' ')
            .filter_map(|pair| {
                let (name, text) = pair.split_once(':')?;
                Some((
                    name,
                    if name == "space" {
                        ' '
                    } else {
                        text.chars().next()?
                    },
                ))
            })
            .collect()
    });
    if name.chars().count() == 1 {
        return name.chars().next();
    }
    if let Some(found) = table.get(name) {
        return Some(*found);
    }
    let hex = name
        .strip_prefix("uni")
        .or_else(|| name.strip_prefix('u'))?;
    (hex.len() >= 4 && hex.len() <= 6 && hex.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| u32::from_str_radix(hex, 16).ok().and_then(char::from_u32))
        .flatten()
}

fn utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks(2)
        .map(|pair| {
            if pair.len() == 2 {
                u16::from_be_bytes([pair[0], pair[1]])
            } else {
                u16::from(pair[0])
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// `ToUnicode` CMap: `bfchar` and `bfrange` entries, plus the code length.
fn parse_cmap(data: &[u8]) -> (HashMap<u32, String>, Option<usize>) {
    #[derive(PartialEq)]
    enum Token {
        Hex(Vec<u8>),
        Open,
        Close,
        Word(String),
    }
    let mut tokens = Vec::new();
    let mut lexer = Lexer { data, pos: 0 };
    while lexer.pos < data.len() && tokens.len() < MAX_CMAP * 4 {
        lexer.skip_space();
        match data.get(lexer.pos) {
            None => break,
            Some(b'<') if data.get(lexer.pos + 1) != Some(&b'<') => {
                if let Ok(Obj::Str(bytes)) = lexer.parse(0) {
                    tokens.push(Token::Hex(bytes));
                }
            }
            Some(b'[') => {
                lexer.pos += 1;
                tokens.push(Token::Open);
            }
            Some(b']') => {
                lexer.pos += 1;
                tokens.push(Token::Close);
            }
            Some(b'<' | b'>' | b'(' | b')' | b'/' | b'{' | b'}') => {
                lexer.pos += 1;
                lexer.token();
            }
            Some(_) => {
                let word = lexer.token();
                if word.is_empty() {
                    lexer.pos += 1;
                } else {
                    tokens.push(Token::Word(String::from_utf8_lossy(word).into_owned()));
                }
            }
        }
    }
    let code = |bytes: &[u8]| {
        bytes
            .iter()
            .fold(0u32, |acc, byte| acc << 8 | u32::from(*byte))
    };
    let (mut map, mut width) = (HashMap::new(), 0usize);
    let mut at = 0;
    while at < tokens.len() {
        match &tokens[at] {
            Token::Word(word) if word == "begincodespacerange" => {
                if let Some(Token::Hex(low)) = tokens.get(at + 1) {
                    width = width.max(low.len());
                }
            }
            Token::Word(word) if word == "beginbfchar" => {
                at += 1;
                while let (Some(Token::Hex(source)), Some(Token::Hex(target))) =
                    (tokens.get(at), tokens.get(at + 1))
                {
                    map.insert(code(source), utf16(target));
                    at += 2;
                }
                continue;
            }
            Token::Word(word) if word == "beginbfrange" => {
                at += 1;
                while let (Some(Token::Hex(low)), Some(Token::Hex(high))) =
                    (tokens.get(at), tokens.get(at + 1))
                {
                    let (low, high) = (code(low), code(high));
                    if high < low || high - low > 65_535 {
                        break;
                    }
                    match tokens.get(at + 2) {
                        Some(Token::Hex(target)) => {
                            let base = utf16(target);
                            for offset in 0..=(high - low) {
                                let mut text: Vec<char> = base.chars().collect();
                                if let Some(last) = text.last_mut() {
                                    *last = char::from_u32(*last as u32 + offset).unwrap_or(*last);
                                }
                                map.insert(low + offset, text.into_iter().collect());
                            }
                            at += 3;
                        }
                        Some(Token::Open) => {
                            at += 3;
                            let mut offset = 0;
                            while let Some(Token::Hex(target)) = tokens.get(at) {
                                map.insert(low + offset, utf16(target));
                                offset += 1;
                                at += 1;
                            }
                            at += 1; // the closing bracket
                        }
                        _ => break,
                    }
                    if map.len() > MAX_CMAP {
                        return (map, (width > 0).then_some(width));
                    }
                }
                continue;
            }
            _ => {}
        }
        at += 1;
    }
    (map, (width > 0).then_some(width))
}

impl Document<'_> {
    /// `Widths`/`FirstChar` for simple fonts, `W`/`DW` of the descendant for composite ones.
    /// Fonts without metrics (the base-14 set) fall back to half an em per glyph.
    fn read_widths(&self, dict: &Dict, font: &mut Font) {
        if font.composite {
            let descendant = match self.get(dict, "DescendantFonts") {
                Obj::Array(items) => items.first().and_then(|item| self.dict_of(item)),
                _ => None,
            };
            let Some(descendant) = descendant else { return };
            font.default_width = self.number(descendant.get("DW")).unwrap_or(1000.0);
            if let Obj::Array(entries) = self.get(descendant, "W") {
                let mut at = 0;
                while at < entries.len() && font.widths.len() < MAX_CMAP {
                    let Some(first) = self.number(entries.get(at)) else {
                        break;
                    };
                    match entries.get(at + 1).map(|item| self.resolve(item)) {
                        Some(Obj::Array(list)) => {
                            for (offset, width) in list.iter().enumerate() {
                                if let Some(width) = self.number(Some(width)) {
                                    font.widths.insert(first as u32 + offset as u32, width);
                                }
                            }
                            at += 2;
                        }
                        Some(Obj::Num(last)) => {
                            let width = self
                                .number(entries.get(at + 2))
                                .unwrap_or(font.default_width);
                            let last = (*last as u32).min(first as u32 + 65_535);
                            for code in first as u32..=last {
                                font.widths.insert(code, width);
                            }
                            at += 3;
                        }
                        _ => break,
                    }
                }
            }
        } else if let Obj::Array(widths) = self.get(dict, "Widths") {
            let first = self.number(dict.get("FirstChar")).unwrap_or(0.0) as u32;
            for (offset, width) in widths.iter().enumerate().take(256) {
                if let Some(width) = self.number(Some(width)) {
                    font.widths.insert(first + offset as u32, width);
                }
            }
            if let Some(missing) = self
                .dict_of(dict.get("FontDescriptor").unwrap_or(&Obj::Null))
                .and_then(|descriptor| self.number(descriptor.get("MissingWidth")))
            {
                font.default_width = missing;
            }
        }
    }

    fn font(&self, object: &Obj) -> Font {
        let Some(dict) = self.dict_of(object) else {
            return Font::default();
        };
        let composite = self.name(dict, "Subtype") == Some("Type0");
        let mut font = Font {
            composite,
            code_bytes: if composite { 2 } else { 1 },
            default_width: 500.0,
            ..Font::default()
        };
        self.read_widths(dict, &mut font);
        if let Obj::Stream(stream) = self.get(dict, "ToUnicode")
            && let Ok(bytes) = self.decode(stream)
        {
            let (map, width) = parse_cmap(&bytes);
            font.unicode = map;
            if composite && let Some(width) = width {
                font.code_bytes = width.clamp(1, 4);
            }
        }
        if !composite {
            let mut table = win_ansi_table();
            match self.get(dict, "Encoding") {
                Obj::Name(name) if name == "MacRomanEncoding" => table = mac_roman_table(),
                Obj::Dict(encoding) => {
                    if self.name(encoding, "BaseEncoding") == Some("MacRomanEncoding") {
                        table = mac_roman_table();
                    }
                    if let Obj::Array(differences) = self.get(encoding, "Differences") {
                        let mut code = 0usize;
                        for item in differences {
                            match self.resolve(item) {
                                Obj::Num(start) => code = *start as usize,
                                Obj::Name(name) => {
                                    if code < 256 {
                                        table[code] = glyph_char(name);
                                    }
                                    code += 1;
                                }
                                _ => {}
                            }
                        }
                    }
                }
                _ => {}
            }
            font.table = table;
        }
        font
    }

    /// Decode a shown string and its advance (thousandths of an em); `None` when the
    /// font cannot be mapped to Unicode.
    fn show(&self, font: &Font, bytes: &[u8]) -> Option<(String, f64)> {
        let mut out = String::new();
        let mut advance = 0.0;
        if font.composite {
            if font.unicode.is_empty() {
                return None;
            }
            for chunk in bytes.chunks(font.code_bytes.max(1)) {
                let code = chunk
                    .iter()
                    .fold(0u32, |acc, byte| acc << 8 | u32::from(*byte));
                advance += font
                    .widths
                    .get(&code)
                    .copied()
                    .unwrap_or(font.default_width);
                if let Some(text) = font.unicode.get(&code) {
                    out.push_str(text);
                }
            }
        } else {
            for byte in bytes {
                advance += font
                    .widths
                    .get(&u32::from(*byte))
                    .copied()
                    .unwrap_or(font.default_width);
                if let Some(text) = font.unicode.get(&u32::from(*byte)) {
                    out.push_str(text);
                } else if let Some(Some(character)) = font.table.get(*byte as usize) {
                    out.push(*character);
                }
            }
        }
        Some((out, advance))
    }
}

// ---- content streams -------------------------------------------------------------

struct Page<'a> {
    document: &'a Document<'a>,
    fonts: HashMap<String, Rc<Font>>,
    text: String,
    unsupported: HashSet<String>,
}

impl Page<'_> {
    fn push(&mut self, text: &str) {
        self.text.push_str(text);
    }
    fn newline(&mut self) {
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push('\n');
        }
    }
    fn space(&mut self) {
        if !self.text.is_empty() && !self.text.ends_with([' ', '\n']) {
            self.text.push(' ');
        }
    }
}

/// Text positions are tracked in user-space units. Moving to a new line origin
/// starts a new line for a vertical jump, and a word for a horizontal gap wider than
/// a typical letter-spacing error; a move that lands exactly where the previous
/// text ended is a continuation, which PDF producers use to split a word into runs.
fn advance(page: &mut Page, end: (f64, f64), to: (f64, f64), size: f64, first: bool) {
    if first {
        return;
    }
    let height = size.abs().max(1.0);
    if (to.1 - end.1).abs() > height * 0.3 {
        page.newline();
    } else {
        let gap = to.0 - end.0;
        if gap > height * 0.15 || gap < -height * 0.5 {
            page.space();
        }
    }
}

fn read_content(page: &mut Page, content: &[u8], deadline: &Deadline) -> Result<()> {
    let mut lexer = Lexer {
        data: content,
        pos: 0,
    };
    let mut operands: Vec<Obj> = Vec::new();
    let (mut font, mut name, mut size) = (None::<Rc<Font>>, String::new(), 12.0f64);
    // The current line's origin, how far text has advanced along it, and where the
    // previous text ended; `scale` and `sx`/`sy` come from the text matrix.
    let (mut line_x, mut line_y, mut shown, mut leading) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let (mut end, mut scale, mut sx, mut sy) = ((0.0f64, 0.0f64), 1.0f64, 1.0f64, 1.0f64);
    let (mut first, mut steps) = (true, 0usize);
    while lexer.pos < content.len() {
        steps += 1;
        if steps.is_multiple_of(8192) {
            deadline.check("reading page content")?;
        }
        lexer.skip_space();
        let Some(byte) = content.get(lexer.pos).copied() else {
            break;
        };
        if matches!(
            byte,
            b'0'..=b'9' | b'+' | b'-' | b'.' | b'/' | b'(' | b'<' | b'['
        ) {
            match lexer.parse(0) {
                Ok(object) => {
                    if operands.len() < 512 {
                        operands.push(object);
                    }
                }
                Err(_) => lexer.pos += 1,
            }
            continue;
        }
        let operator = lexer.token();
        if operator.is_empty() {
            lexer.pos += 1;
            continue;
        }
        let number = |index: usize| match operands.get(index) {
            Some(Obj::Num(value)) => *value,
            _ => 0.0,
        };
        // A new text line origin: compare it with where the last text ended.
        let moved = |page: &mut Page,
                     to: (f64, f64),
                     shown: &mut f64,
                     end: &mut (f64, f64),
                     size: f64,
                     first: &mut bool| {
            advance(page, *end, to, size, *first);
            (*shown, *end) = (0.0, to);
        };
        match operator {
            b"BT" => {
                (line_x, line_y, shown) = (0.0, 0.0, 0.0);
                (scale, sx, sy) = (1.0, 1.0, 1.0);
            }
            b"Tf" => {
                if let (Some(Obj::Name(font_name)), Some(Obj::Num(value))) =
                    (operands.first(), operands.get(1))
                {
                    name = font_name.clone();
                    font = page.fonts.get(font_name).cloned();
                    size = *value;
                }
            }
            b"TL" => leading = number(0),
            b"Td" | b"TD" => {
                let (tx, ty) = (number(0), number(1));
                if operator == b"TD" {
                    leading = -ty;
                }
                (line_x, line_y) = (line_x + tx * sx, line_y + ty * sy);
                moved(
                    page,
                    (line_x, line_y),
                    &mut shown,
                    &mut end,
                    size * scale,
                    &mut first,
                );
            }
            b"Tm" => {
                sx = number(0).hypot(number(1));
                sy = number(2).hypot(number(3));
                scale = if sy > 0.0 { sy } else { 1.0 };
                sx = if sx > 0.0 { sx } else { 1.0 };
                (line_x, line_y) = (number(4), number(5));
                moved(
                    page,
                    (line_x, line_y),
                    &mut shown,
                    &mut end,
                    size * scale,
                    &mut first,
                );
            }
            b"T*" => {
                line_y -= (if leading != 0.0 { leading } else { size * 1.2 }) * sy;
                moved(
                    page,
                    (line_x, line_y),
                    &mut shown,
                    &mut end,
                    size * scale,
                    &mut first,
                );
            }
            b"Tj" | b"'" | b"\"" | b"TJ" => {
                if operator == b"'" || operator == b"\"" {
                    line_y -= (if leading != 0.0 { leading } else { size * 1.2 }) * sy;
                    moved(
                        page,
                        (line_x, line_y),
                        &mut shown,
                        &mut end,
                        size * scale,
                        &mut first,
                    );
                }
                first = false;
                let pieces: Vec<&Obj> = match operands.last() {
                    Some(Obj::Array(items)) => items.iter().collect(),
                    Some(other) => vec![other],
                    None => vec![],
                };
                for piece in pieces {
                    match piece {
                        Obj::Str(bytes) => match font
                            .as_ref()
                            .and_then(|current| page.document.show(current, bytes))
                        {
                            Some((text, advance_units)) => {
                                page.push(&text);
                                shown += advance_units / 1000.0 * size * sx;
                            }
                            None => {
                                page.unsupported.insert(name.clone());
                            }
                        },
                        Obj::Num(adjustment) => {
                            shown -= adjustment / 1000.0 * size * sx;
                            if *adjustment < -200.0 {
                                page.space();
                            }
                        }
                        _ => {}
                    }
                }
                end = (line_x + shown, line_y);
                if page.text.chars().count() > MAX_TEXT_CHARS {
                    return Err(invalid("page text exceeds the size limit"));
                }
            }
            b"BI" => {
                // Inline image data may contain anything; skip to its `EI`.
                match find(content, b"EI", lexer.pos) {
                    Some(stop) => lexer.pos = stop + 2,
                    None => break,
                }
            }
            _ => {}
        }
        operands.clear();
    }
    Ok(())
}

pub(super) fn read(
    bytes: &[u8],
    start: Option<u64>,
    end: Option<u64>,
    deadline: &Deadline,
) -> Result<Value> {
    let document = Document::load(bytes, deadline)?;
    let leaves = document.pages()?;
    let total = leaves.len();
    if total == 0 {
        return Err(invalid("the PDF has no pages"));
    }
    let first = start.unwrap_or(1).max(1) as usize;
    let last = end.map_or(total, |end| end as usize).min(total);
    if first > last {
        return Err(invalid(&format!(
            "page range is empty; the PDF has {total} pages"
        )));
    }
    if last - first + 1 > MAX_PAGES_PER_REQUEST {
        return Err(invalid(&format!(
            "at most {MAX_PAGES_PER_REQUEST} pages per request"
        )));
    }
    let mut warnings: Vec<String> = document.warnings.clone();
    let mut font_cache: HashMap<u32, Rc<Font>> = HashMap::new();
    let mut pages = Vec::new();
    let mut empty = Vec::new();
    for number in first..=last {
        deadline.check("reading the PDF")?;
        let (dict, resources) = &leaves[number - 1];
        let mut page = Page {
            document: &document,
            fonts: HashMap::new(),
            text: String::new(),
            unsupported: HashSet::new(),
        };
        if let Some(fonts) = resources
            .get("Font")
            .and_then(|fonts| document.dict_of(fonts))
        {
            for (font_name, entry) in fonts {
                let font = match entry {
                    Obj::Ref(id) => font_cache
                        .entry(*id)
                        .or_insert_with(|| Rc::new(document.font(entry)))
                        .clone(),
                    other => Rc::new(document.font(other)),
                };
                page.fonts.insert(font_name.clone(), font);
            }
        }
        let contents: Vec<&Obj> = match document.get(dict, "Contents") {
            Obj::Array(items) => items.iter().collect(),
            _ => dict.get("Contents").into_iter().collect(),
        };
        for item in contents {
            if let Obj::Stream(stream) = document.resolve(item) {
                match document.decode(stream) {
                    Ok(data) => read_content(&mut page, &data, deadline)?,
                    Err(error) => warnings.push(format!(
                        "page {number}: {}",
                        match error {
                            crate::Error::Input(m) =>
                                m.replace("invalid ZIP/OOXML container: ", ""),
                            other => other.to_string(),
                        }
                    )),
                }
                page.text.push('\n');
            }
        }
        for font in &page.unsupported {
            warnings.push(format!(
                "page {number}: font `{font}` has no Unicode mapping, so its text was skipped"
            ));
        }
        let text = page
            .text
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .replace('\u{ad}', "")
            // Symbol-font list bullets arrive as this private-use code point.
            .replace('\u{f0b7}', "•");
        if text.is_empty() {
            empty.push(number);
        }
        pages.push(json!({"number":number,"text":text}));
    }
    if !empty.is_empty() {
        warnings.push(format!(
            "no extractable text on pages {empty:?}; they may be scanned images"
        ));
    }
    warnings.dedup();
    let joined: Vec<String> = pages
        .iter()
        .map(|page| page["text"].as_str().unwrap_or_default().to_owned())
        .collect();
    let metadata: serde_json::Map<String, Value> = [
        "Title", "Author", "Subject", "Keywords", "Creator", "Producer",
    ]
    .iter()
    .filter_map(|key| {
        document
            .text_of_info(key)
            .map(|value| (key.to_ascii_lowercase(), json!(value)))
    })
    .collect();
    Ok(
        json!({"total_pages":total,"pages":pages,"text":joined.join("\n\n"),"metadata":metadata,"warnings":warnings}),
    )
}
