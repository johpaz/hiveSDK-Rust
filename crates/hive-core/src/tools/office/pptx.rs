//! PowerPoint (.pptx): slide text out, title/content/bullet slides in.
//!
//! Generated decks are 16:9 with plain text boxes on a blank layout: no images,
//! charts, transitions or presenter notes.
use super::{
    Deadline, invalid,
    xml::{self, Event, attr, escape},
    zip::{Archive, Writer},
};
use crate::Result;
use serde_json::{Value, json};

const MAX_SLIDES: usize = 500;
const MAX_POINTS: usize = 100;
const MAX_TEXT_CHARS: usize = 100_000;

/// `ppt/slides/slide12.xml` → 12; anything else (including `slide1.xml.rels`) → None.
fn slide_number(name: &str) -> Option<u32> {
    name.strip_prefix("ppt/slides/slide")?
        .strip_suffix(".xml")?
        .parse()
        .ok()
}

pub(super) fn read(bytes: &[u8], only: Option<u64>, deadline: &Deadline) -> Result<Value> {
    let archive = Archive::open(bytes)?;
    let mut parts: Vec<(u32, &str)> = archive
        .names()
        .filter_map(|name| slide_number(name).map(|number| (number, name)))
        .collect();
    if parts.is_empty() {
        return Err(invalid("not a PowerPoint presentation (no slides found)"));
    }
    parts.sort();
    if parts.len() > MAX_SLIDES * 4 {
        return Err(invalid("presentation has too many slides"));
    }
    let total = parts.len();
    let mut slides = Vec::new();
    for (position, (_, name)) in parts.into_iter().enumerate() {
        let number = position as u64 + 1;
        if only.is_some_and(|wanted| wanted != number) {
            continue;
        }
        deadline.check("reading the presentation")?;
        let (mut paragraphs, mut current, mut in_text) =
            (Vec::<String>::new(), String::new(), false);
        let (mut title, mut in_shape, mut shape_is_title, mut shape_text) =
            (None::<String>, false, false, Vec::<String>::new());
        for event in xml::parse(&archive.read_text(name)?)? {
            match event {
                Event::Start { name, .. } if name == "p:sp" => {
                    (in_shape, shape_is_title, shape_text) = (true, false, vec![])
                }
                Event::Empty { name, attrs } | Event::Start { name, attrs }
                    if name == "p:ph" && in_shape =>
                {
                    shape_is_title = matches!(attr(&attrs, "type"), Some("title" | "ctrTitle"));
                }
                Event::Start { name, .. } if name == "a:p" => current.clear(),
                Event::Start { name, .. } if name == "a:t" => in_text = true,
                Event::End { name } if name == "a:t" => in_text = false,
                Event::Text(text) if in_text => current.push_str(&text),
                Event::Empty { name, .. } | Event::Start { name, .. } if name == "a:br" => {
                    current.push('\n')
                }
                Event::End { name } if name == "a:p" => {
                    let text = current.trim().to_owned();
                    if !text.is_empty() {
                        if in_shape {
                            shape_text.push(text.clone());
                        }
                        paragraphs.push(text);
                    }
                    if paragraphs.iter().map(String::len).sum::<usize>() > MAX_TEXT_CHARS * 20 {
                        return Err(invalid("slide text exceeds the size limit"));
                    }
                }
                Event::End { name } if name == "p:sp" => {
                    if shape_is_title && title.is_none() && !shape_text.is_empty() {
                        title = Some(shape_text.join(" "));
                    }
                    in_shape = false;
                }
                _ => {}
            }
        }
        let title = title.or_else(|| paragraphs.first().cloned());
        let text = paragraphs
            .join(" ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        slides.push(json!({"number":number,"title":title,"text":text,"paragraphs":paragraphs}));
    }
    if let Some(wanted) = only
        && slides.is_empty()
    {
        return Err(invalid(&format!(
            "slide {wanted} not found; the presentation has {total}"
        )));
    }
    Ok(json!({"total_slides":total,"slides":slides}))
}

const HEADER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";
const NS: &str = "xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" xmlns:p=\"http://schemas.openxmlformats.org/presentationml/2006/main\"";
const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const WIDTH: i64 = 9_144_000;
const HEIGHT: i64 = 5_143_500;

/// Colors are six hex digits; fonts are typeface names.
#[derive(Clone)]
struct Palette {
    background: String,
    title: String,
    text: String,
    accent: String,
    accent2: String,
    cover_background: String,
    cover_text: String,
    title_font: String,
    body_font: String,
}

/// Named looks. Every one keeps body text at least 4.5:1 against its background.
fn preset(name: &str) -> Option<Palette> {
    let (
        background,
        title,
        text,
        accent,
        accent2,
        cover_background,
        cover_text,
        title_font,
        body_font,
    ) = match name {
        "modern" => (
            "F8FAFC", "0F172A", "334155", "2563EB", "F59E0B", "0F172A", "FFFFFF", "Calibri",
            "Calibri",
        ),
        "light" => (
            "FFFFFF", "1F2937", "4B5563", "0284C7", "94A3B8", "0284C7", "FFFFFF", "Calibri",
            "Calibri",
        ),
        "dark" => (
            "111827", "F9FAFB", "D1D5DB", "22D3EE", "A78BFA", "030712", "F9FAFB", "Calibri",
            "Calibri",
        ),
        "corporate" => (
            "FFFFFF", "1E3A5F", "374151", "1E3A5F", "B8923A", "1E3A5F", "FFFFFF", "Georgia",
            "Calibri",
        ),
        "warm" => (
            "FFFBF5", "7C2D12", "44403C", "EA580C", "FACC15", "7C2D12", "FFF7ED", "Georgia",
            "Calibri",
        ),
        _ => return None,
    };
    let owned = |value: &str| value.to_owned();
    Some(Palette {
        background: owned(background),
        title: owned(title),
        text: owned(text),
        accent: owned(accent),
        accent2: owned(accent2),
        cover_background: owned(cover_background),
        cover_text: owned(cover_text),
        title_font: owned(title_font),
        body_font: owned(body_font),
    })
}

fn hex(value: &str) -> Result<String> {
    if value.len() == 6 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(value.to_ascii_uppercase())
    } else {
        Err(invalid(
            "theme colors must be six hex digits such as 2563EB",
        ))
    }
}

fn font(value: &str) -> Result<String> {
    if (1..=64).contains(&value.len())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '.'))
    {
        Ok(value.to_owned())
    } else {
        Err(invalid(
            "theme fonts must be typeface names of letters, digits, spaces, `-` or `.`",
        ))
    }
}

/// A preset (`modern` unless named) with any color or font the caller overrides.
fn palette(theme: &Value) -> Result<Palette> {
    let name = theme["preset"].as_str().unwrap_or("modern");
    let mut palette =
        preset(name).ok_or_else(|| invalid(&format!("unknown theme preset `{name}`")))?;
    for (key, slot) in [
        ("background", &mut palette.background),
        ("title", &mut palette.title),
        ("text", &mut palette.text),
        ("accent", &mut palette.accent),
        ("accent2", &mut palette.accent2),
        ("cover_background", &mut palette.cover_background),
        ("cover_text", &mut palette.cover_text),
    ] {
        if let Some(value) = theme["colors"][key].as_str() {
            *slot = hex(value)?;
        }
    }
    for (key, slot) in [
        ("title", &mut palette.title_font),
        ("body", &mut palette.body_font),
    ] {
        if let Some(value) = theme["fonts"][key].as_str() {
            *slot = font(value)?;
        }
    }
    Ok(palette)
}

struct Slide {
    title: Option<String>,
    subtitle: Option<String>,
    content: Option<String>,
    points: Vec<String>,
    cover: bool,
}

fn solid(color: &str) -> String {
    format!("<a:solidFill><a:srgbClr val=\"{color}\"/></a:solidFill>")
}

fn rectangle(id: u32, name: &str, frame: (i64, i64, i64, i64), color: &str) -> String {
    format!(
        "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"{name}\"/><p:cNvSpPr/><p:nvPr userDrawn=\"1\"/></p:nvSpPr><p:spPr><a:xfrm><a:off x=\"{}\" y=\"{}\"/><a:ext cx=\"{}\" cy=\"{}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom>{}<a:ln><a:noFill/></a:ln></p:spPr></p:sp>",
        frame.0,
        frame.1,
        frame.2,
        frame.3,
        solid(color)
    )
}

fn frame_xml(frame: Option<(i64, i64, i64, i64)>) -> String {
    match frame {
        Some((x, y, w, h)) => format!(
            "<a:xfrm><a:off x=\"{x}\" y=\"{y}\"/><a:ext cx=\"{w}\" cy=\"{h}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom>"
        ),
        None => String::new(),
    }
}

/// A placeholder in a master or layout; an absent frame inherits the parent's.
fn placeholder(
    id: u32,
    name: &str,
    kind: &str,
    frame: Option<(i64, i64, i64, i64)>,
    anchor: &str,
    level_style: &str,
    prompt: &str,
) -> String {
    format!(
        "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"{name}\"/><p:cNvSpPr><a:spLocks noGrp=\"1\"/></p:cNvSpPr><p:nvPr><p:ph{kind}/></p:nvPr></p:nvSpPr><p:spPr>{}</p:spPr><p:txBody><a:bodyPr anchor=\"{anchor}\"><a:normAutofit/></a:bodyPr><a:lstStyle>{level_style}</a:lstStyle><a:p><a:r><a:rPr lang=\"en-US\"/><a:t>{prompt}</a:t></a:r></a:p></p:txBody></p:sp>",
        frame_xml(frame)
    )
}

const GROUP: &str =
    "<p:nvGrpSpPr><p:cNvPr id=\"1\" name=\"\"/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>";

fn master(p: &Palette) -> String {
    let title_frame = (685_800, 300_000, 7_772_400, 800_000);
    let body_frame = (685_800, 1_350_000, 7_772_400, 3_300_000);
    let level = |n: u32, size: u32, margin: u32, bullet: &str| {
        format!(
            "<a:lvl{n}pPr marL=\"{margin}\" indent=\"-285750\" algn=\"l\" defTabSz=\"914400\" rtl=\"0\" eaLnBrk=\"1\" latinLnBrk=\"0\" hangingPunct=\"1\"><a:lnSpc><a:spcPct val=\"100000\"/></a:lnSpc><a:spcBef><a:spcPts val=\"900\"/></a:spcBef><a:buClr><a:schemeClr val=\"accent1\"/></a:buClr><a:buFont typeface=\"Arial\"/><a:buChar char=\"{bullet}\"/><a:defRPr sz=\"{size}\" kern=\"1200\"><a:solidFill><a:schemeClr val=\"tx1\"/></a:solidFill><a:latin typeface=\"+mn-lt\"/><a:ea typeface=\"+mn-ea\"/><a:cs typeface=\"+mn-cs\"/></a:defRPr></a:lvl{n}pPr>"
        )
    };
    let body_style = format!(
        "{}{}{}",
        level(1, 2000, 285_750, "&#8226;"),
        level(2, 1800, 685_800, "&#8211;"),
        level(3, 1600, 1_085_850, "&#8226;")
    );
    let other = "<a:lvl1pPr marL=\"0\" algn=\"l\" defTabSz=\"914400\" rtl=\"0\" eaLnBrk=\"1\" latinLnBrk=\"0\" hangingPunct=\"1\"><a:defRPr sz=\"1800\" kern=\"1200\"><a:solidFill><a:schemeClr val=\"tx1\"/></a:solidFill><a:latin typeface=\"+mn-lt\"/><a:ea typeface=\"+mn-ea\"/><a:cs typeface=\"+mn-cs\"/></a:defRPr></a:lvl1pPr>";
    let title_style = format!(
        "<a:lvl1pPr algn=\"l\" defTabSz=\"914400\" rtl=\"0\" eaLnBrk=\"1\" latinLnBrk=\"0\" hangingPunct=\"1\"><a:lnSpc><a:spcPct val=\"90000\"/></a:lnSpc><a:spcBef><a:spcPct val=\"0\"/></a:spcBef><a:buNone/><a:defRPr sz=\"3000\" b=\"1\" kern=\"1200\">{}<a:latin typeface=\"+mj-lt\"/><a:ea typeface=\"+mj-ea\"/><a:cs typeface=\"+mj-cs\"/></a:defRPr></a:lvl1pPr>",
        solid(&p.title)
    );
    format!(
        "{HEADER}<p:sldMaster {NS}><p:cSld><p:bg><p:bgPr><a:solidFill><a:schemeClr val=\"bg1\"/></a:solidFill><a:effectLst/></p:bgPr></p:bg><p:spTree>{GROUP}{}{}{}{}</p:spTree></p:cSld><p:clrMap bg1=\"lt1\" tx1=\"dk1\" bg2=\"lt2\" tx2=\"dk2\" accent1=\"accent1\" accent2=\"accent2\" accent3=\"accent3\" accent4=\"accent4\" accent5=\"accent5\" accent6=\"accent6\" hlink=\"hlink\" folHlink=\"folHlink\"/><p:sldLayoutIdLst><p:sldLayoutId id=\"2147483649\" r:id=\"rId1\"/><p:sldLayoutId id=\"2147483650\" r:id=\"rId2\"/></p:sldLayoutIdLst><p:txStyles><p:titleStyle>{title_style}</p:titleStyle><p:bodyStyle>{body_style}</p:bodyStyle><p:otherStyle>{other}</p:otherStyle></p:txStyles></p:sldMaster>",
        rectangle(
            2,
            "Accent Bar",
            (0, 0, 137_160, HEIGHT),
            "ACCENT1_PLACEHOLDER"
        )
        .replace(
            &solid("ACCENT1_PLACEHOLDER"),
            "<a:solidFill><a:schemeClr val=\"accent1\"/></a:solidFill>"
        ),
        rectangle(
            3,
            "Title Rule",
            (685_800, 1_130_000, 914_400, 45_720),
            "ACCENT2_PLACEHOLDER"
        )
        .replace(
            &solid("ACCENT2_PLACEHOLDER"),
            "<a:solidFill><a:schemeClr val=\"accent2\"/></a:solidFill>"
        ),
        placeholder(
            4,
            "Title Placeholder 3",
            " type=\"title\"",
            Some(title_frame),
            "ctr",
            "",
            "Click to edit Master title style"
        ),
        placeholder(
            5,
            "Text Placeholder 4",
            " type=\"body\" idx=\"1\"",
            Some(body_frame),
            "t",
            "",
            "Click to edit Master text styles"
        ),
    )
}

fn cover_layout(p: &Palette) -> String {
    let title_style = format!(
        "<a:lvl1pPr algn=\"l\"><a:defRPr sz=\"4400\" b=\"1\">{}</a:defRPr></a:lvl1pPr>",
        solid(&p.cover_text)
    );
    let subtitle_style = format!(
        "<a:lvl1pPr marL=\"0\" indent=\"0\" algn=\"l\"><a:buNone/><a:defRPr sz=\"2000\">{}</a:defRPr></a:lvl1pPr>",
        solid(&p.cover_text)
    );
    format!(
        "{HEADER}<p:sldLayout {NS} type=\"title\" preserve=\"1\" showMasterSp=\"0\"><p:cSld name=\"Title Slide\"><p:bg><p:bgPr>{}<a:effectLst/></p:bgPr></p:bg><p:spTree>{GROUP}{}{}{}{}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>",
        solid(&p.cover_background),
        rectangle(
            2,
            "Cover Band",
            (0, HEIGHT - 228_600, WIDTH, 228_600),
            &p.accent2
        ),
        rectangle(
            3,
            "Cover Rule",
            (685_800, 3_020_000, 1_371_600, 54_864),
            &p.accent2
        ),
        placeholder(
            4,
            "Title 3",
            " type=\"ctrTitle\"",
            Some((685_800, 1_250_000, 7_772_400, 1_650_000)),
            "b",
            &title_style,
            "Click to edit Master title style"
        ),
        placeholder(
            5,
            "Subtitle 4",
            " type=\"subTitle\" idx=\"1\"",
            Some((685_800, 3_200_000, 7_772_400, 900_000)),
            "t",
            &subtitle_style,
            "Click to edit Master subtitle style"
        ),
    )
}

fn content_layout() -> String {
    format!(
        "{HEADER}<p:sldLayout {NS} type=\"obj\" preserve=\"1\"><p:cSld name=\"Title and Content\"><p:spTree>{GROUP}{}{}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>",
        placeholder(
            2,
            "Title 1",
            " type=\"title\"",
            None,
            "ctr",
            "",
            "Click to edit Master title style"
        ),
        placeholder(
            3,
            "Content Placeholder 2",
            " idx=\"1\"",
            None,
            "t",
            "",
            "Click to edit Master text styles"
        ),
    )
}

fn theme(p: &Palette) -> String {
    let fill = "<a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill>";
    let line = "<a:ln w=\"9525\" cap=\"flat\" cmpd=\"sng\" algn=\"ctr\"><a:solidFill><a:schemeClr val=\"phClr\"/></a:solidFill><a:prstDash val=\"solid\"/></a:ln>";
    let colors = format!(
        "<a:dk1><a:srgbClr val=\"{}\"/></a:dk1><a:lt1><a:srgbClr val=\"{}\"/></a:lt1><a:dk2><a:srgbClr val=\"{}\"/></a:dk2><a:lt2><a:srgbClr val=\"{}\"/></a:lt2><a:accent1><a:srgbClr val=\"{}\"/></a:accent1><a:accent2><a:srgbClr val=\"{}\"/></a:accent2><a:accent3><a:srgbClr val=\"A5A5A5\"/></a:accent3><a:accent4><a:srgbClr val=\"FFC000\"/></a:accent4><a:accent5><a:srgbClr val=\"5B9BD5\"/></a:accent5><a:accent6><a:srgbClr val=\"70AD47\"/></a:accent6><a:hlink><a:srgbClr val=\"{}\"/></a:hlink><a:folHlink><a:srgbClr val=\"954F72\"/></a:folHlink>",
        p.text, p.background, p.title, p.background, p.accent, p.accent2, p.accent
    );
    format!(
        "{HEADER}<a:theme xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" name=\"Hive\"><a:themeElements><a:clrScheme name=\"Hive\">{colors}</a:clrScheme><a:fontScheme name=\"Hive\"><a:majorFont><a:latin typeface=\"{}\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:majorFont><a:minorFont><a:latin typeface=\"{}\"/><a:ea typeface=\"\"/><a:cs typeface=\"\"/></a:minorFont></a:fontScheme><a:fmtScheme name=\"Office\"><a:fillStyleLst>{fill}{fill}{fill}</a:fillStyleLst><a:lnStyleLst>{line}{line}{line}</a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst>{fill}{fill}{fill}</a:bgFillStyleLst></a:fmtScheme></a:themeElements></a:theme>",
        escape(&p.title_font),
        escape(&p.body_font),
    )
}

/// Smaller type for long titles and dense bodies, so text fits without relying on
/// PowerPoint to shrink it after an edit.
fn title_size(text: &str, cover: bool) -> Option<u32> {
    let length = text.chars().count();
    match (cover, length) {
        (true, 71..) => Some(2800),
        (true, 41..) => Some(3600),
        (false, 91..) => Some(2000),
        (false, 61..) => Some(2400),
        _ => None,
    }
}

fn body_size(slide: &Slide) -> Option<u32> {
    let (items, chars) = if slide.points.is_empty() {
        let text = slide.content.as_deref().unwrap_or_default();
        (text.split('\n').count(), text.chars().count())
    } else {
        (
            slide.points.len(),
            slide.points.iter().map(|point| point.chars().count()).sum(),
        )
    };
    match (items, chars) {
        (13.., _) | (_, 1201..) => Some(1200),
        (10.., _) | (_, 801..) => Some(1400),
        (7.., _) | (_, 451..) => Some(1600),
        _ => None,
    }
}

fn run(text: &str, size: Option<u32>) -> String {
    let size = size
        .map(|size| format!(" sz=\"{size}\""))
        .unwrap_or_default();
    format!(
        "<a:r><a:rPr lang=\"en-US\"{size} dirty=\"0\"/><a:t>{}</a:t></a:r>",
        escape(text)
    )
}

fn text_shape(id: u32, name: &str, kind: &str, paragraphs: &str) -> String {
    format!(
        "<p:sp><p:nvSpPr><p:cNvPr id=\"{id}\" name=\"{name}\"/><p:cNvSpPr><a:spLocks noGrp=\"1\"/></p:cNvSpPr><p:nvPr><p:ph{kind}/></p:nvPr></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr><a:normAutofit/></a:bodyPr><a:lstStyle/>{paragraphs}</p:txBody></p:sp>"
    )
}

fn slide_xml(slide: &Slide) -> String {
    let mut shapes = String::new();
    if slide.cover {
        let title = slide.title.as_deref().unwrap_or_default();
        shapes.push_str(&text_shape(
            2,
            "Title 1",
            " type=\"ctrTitle\"",
            &format!("<a:p>{}</a:p>", run(title, title_size(title, true))),
        ));
        if let Some(subtitle) = &slide.subtitle {
            shapes.push_str(&text_shape(
                3,
                "Subtitle 2",
                " type=\"subTitle\" idx=\"1\"",
                &format!("<a:p>{}</a:p>", run(subtitle, None)),
            ));
        }
    } else {
        if let Some(title) = &slide.title {
            shapes.push_str(&text_shape(
                2,
                "Title 1",
                " type=\"title\"",
                &format!("<a:p>{}</a:p>", run(title, title_size(title, false))),
            ));
        }
        let size = body_size(slide);
        let body = if !slide.points.is_empty() {
            slide
                .points
                .iter()
                .map(|point| format!("<a:p>{}</a:p>", run(point, size)))
                .collect::<String>()
        } else if let Some(content) = &slide.content {
            content
                .split('\n')
                .map(|line| {
                    format!(
                        "<a:p><a:pPr marL=\"0\" indent=\"0\"><a:buNone/></a:pPr>{}</a:p>",
                        run(line, size)
                    )
                })
                .collect::<String>()
        } else {
            String::new()
        };
        if !body.is_empty() {
            shapes.push_str(&text_shape(3, "Content Placeholder 2", " idx=\"1\"", &body));
        }
    }
    format!(
        "{HEADER}<p:sld {NS}><p:cSld><p:spTree>{GROUP}{shapes}</p:spTree></p:cSld><p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"
    )
}

pub(super) fn write(input: &Value) -> Result<Vec<u8>> {
    let palette = palette(&input["theme"])?;
    let mut slides = Vec::new();
    if let Some(title) = input["presentation_title"]
        .as_str()
        .filter(|title| !title.is_empty())
    {
        let subtitle = input["presentation_subtitle"]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned);
        slides.push(Slide {
            title: Some(title.to_owned()),
            subtitle,
            content: None,
            points: vec![],
            cover: true,
        });
    }
    let items = input["slides"]
        .as_array()
        .ok_or_else(|| invalid("a presentation needs slides"))?;
    for item in items {
        let text = |key: &str| {
            item[key]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
        };
        let points: Vec<String> = item["points"]
            .as_array()
            .map(|points| {
                points
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        if points.len() > MAX_POINTS {
            return Err(invalid(&format!("at most {MAX_POINTS} points per slide")));
        }
        let (title, content) = (text("title"), text("content"));
        if [&title, &content].iter().any(|value| {
            value
                .as_ref()
                .is_some_and(|text| text.chars().count() > MAX_TEXT_CHARS)
        }) {
            return Err(invalid("slide text is too long"));
        }
        slides.push(Slide {
            title,
            subtitle: None,
            content,
            points,
            cover: false,
        });
    }
    if slides.is_empty() || slides.len() > MAX_SLIDES {
        return Err(invalid(&format!(
            "a presentation needs 1..={MAX_SLIDES} slides"
        )));
    }
    let count = slides.len();
    let mut zip = Writer::default();
    let slide_overrides: String = (1..=count).map(|n| format!("<Override PartName=\"/ppt/slides/slide{n}.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slide+xml\"/>")).collect();
    zip.add("[Content_Types].xml", format!("{HEADER}<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/ppt/presentation.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml\"/><Override PartName=\"/ppt/slideMasters/slideMaster1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml\"/><Override PartName=\"/ppt/slideLayouts/slideLayout1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml\"/><Override PartName=\"/ppt/slideLayouts/slideLayout2.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml\"/><Override PartName=\"/ppt/theme/theme1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.theme+xml\"/>{slide_overrides}</Types>").as_bytes())?;
    zip.add("_rels/.rels", format!("{HEADER}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{REL}/officeDocument\" Target=\"ppt/presentation.xml\"/></Relationships>").as_bytes())?;
    let ids: String = (1..=count)
        .map(|n| format!("<p:sldId id=\"{}\" r:id=\"rId{}\"/>", 255 + n, n + 1))
        .collect();
    zip.add("ppt/presentation.xml", format!("{HEADER}<p:presentation {NS}><p:sldMasterIdLst><p:sldMasterId id=\"2147483648\" r:id=\"rId1\"/></p:sldMasterIdLst><p:sldIdLst>{ids}</p:sldIdLst><p:sldSz cx=\"{WIDTH}\" cy=\"{HEIGHT}\"/><p:notesSz cx=\"6858000\" cy=\"9144000\"/></p:presentation>").as_bytes())?;
    let slide_rels: String = (1..=count)
        .map(|n| {
            format!(
                "<Relationship Id=\"rId{}\" Type=\"{REL}/slide\" Target=\"slides/slide{n}.xml\"/>",
                n + 1
            )
        })
        .collect();
    zip.add("ppt/_rels/presentation.xml.rels", format!("{HEADER}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{REL}/slideMaster\" Target=\"slideMasters/slideMaster1.xml\"/>{slide_rels}<Relationship Id=\"rId{}\" Type=\"{REL}/theme\" Target=\"theme/theme1.xml\"/></Relationships>", count + 2).as_bytes())?;
    zip.add(
        "ppt/slideMasters/slideMaster1.xml",
        master(&palette).as_bytes(),
    )?;
    zip.add("ppt/slideMasters/_rels/slideMaster1.xml.rels", format!("{HEADER}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{REL}/slideLayout\" Target=\"../slideLayouts/slideLayout1.xml\"/><Relationship Id=\"rId2\" Type=\"{REL}/slideLayout\" Target=\"../slideLayouts/slideLayout2.xml\"/><Relationship Id=\"rId3\" Type=\"{REL}/theme\" Target=\"../theme/theme1.xml\"/></Relationships>").as_bytes())?;
    zip.add(
        "ppt/slideLayouts/slideLayout1.xml",
        cover_layout(&palette).as_bytes(),
    )?;
    zip.add(
        "ppt/slideLayouts/slideLayout2.xml",
        content_layout().as_bytes(),
    )?;
    for layout in 1..=2 {
        zip.add(&format!("ppt/slideLayouts/_rels/slideLayout{layout}.xml.rels"), format!("{HEADER}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{REL}/slideMaster\" Target=\"../slideMasters/slideMaster1.xml\"/></Relationships>").as_bytes())?;
    }
    zip.add("ppt/theme/theme1.xml", theme(&palette).as_bytes())?;
    for (index, slide) in slides.iter().enumerate() {
        let number = index + 1;
        let layout = if slide.cover { 1 } else { 2 };
        zip.add(
            &format!("ppt/slides/slide{number}.xml"),
            slide_xml(slide).as_bytes(),
        )?;
        zip.add(&format!("ppt/slides/_rels/slide{number}.xml.rels"), format!("{HEADER}<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{REL}/slideLayout\" Target=\"../slideLayouts/slideLayout{layout}.xml\"/></Relationships>").as_bytes())?;
    }
    zip.finish()
}
