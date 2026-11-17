//! Key names and X keysyms, shared by both backends (the portals take keysyms too).
use crate::{Error, Result};

pub(super) const SHIFT_L: u32 = 0xffe1;

/// Keysym of one typed character, and whether it needs Shift held. Backends that have a keymap
/// (X11) work out Shift themselves; this is for the portal, whose compositor maps keysyms.
pub(super) fn for_char(ch: char) -> (u32, bool) {
    match ch {
        '\n' => (0xff0d, false),
        '\t' => (0xff09, false),
        ch => {
            let cp = ch as u32;
            let keysym = if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) {
                cp
            } else {
                0x0100_0000 | cp
            };
            // The compositor/keymap decides shifting for a keysym; asking for Shift here only
            // matters for the capital letters, which are their own keysyms already.
            (keysym, false)
        }
    }
}

fn named(name: &str) -> Option<u32> {
    Some(match name {
        "return" | "enter" => 0xff0d,
        "tab" => 0xff09,
        "escape" | "esc" => 0xff1b,
        "backspace" => 0xff08,
        "delete" | "del" => 0xffff,
        "insert" => 0xff63,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" | "page_up" | "prior" => 0xff55,
        "pagedown" | "page_down" | "next" => 0xff56,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "space" => 0x20,
        "plus" => 0x2b,
        "minus" => 0x2d,
        "shift" => SHIFT_L,
        "ctrl" | "control" => 0xffe3,
        "alt" | "option" => 0xffe9,
        "altgr" => 0xfe03,
        "super" | "meta" | "win" | "cmd" | "command" => 0xffeb,
        "capslock" => 0xffe5,
        "printscreen" | "print" => 0xff61,
        "menu" => 0xff67,
        _ => {
            let n: u32 = name.strip_prefix('f')?.parse().ok()?;
            if !(1..=24).contains(&n) {
                return None;
            }
            0xffbe + n - 1
        }
    })
}

fn is_modifier(keysym: u32) -> bool {
    matches!(keysym, 0xffe1 | 0xffe3 | 0xffe9 | 0xffeb | 0xfe03 | 0xffe5)
}

/// `"ctrl+shift+t"` → keysyms, the key last. Names are case-insensitive; a single character is that
/// character (an uppercase letter stays uppercase).
pub(super) fn parse_combo(text: &str) -> Result<Vec<u32>> {
    let bad = |why: String| Error::Input(why);
    let parts: Vec<&str> = text.split('+').collect();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(bad(
            "keys has an empty part (use `plus` for the + key)".into()
        ));
    }
    if parts.len() > 5 {
        return Err(bad("at most 4 modifiers and a key".into()));
    }
    let mut out = vec![];
    for part in &parts {
        let lower = part.to_lowercase();
        let keysym = if let Some(keysym) = named(&lower) {
            keysym
        } else {
            let mut chars = part.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => for_char(ch).0,
                _ => return Err(bad(format!("unknown key `{part}`"))),
            }
        };
        out.push(keysym);
    }
    let (last, modifiers) = out.split_last().expect("non-empty");
    if modifiers.iter().any(|m| !is_modifier(*m)) {
        return Err(bad(
            "only shift, ctrl, alt, altgr, super and capslock can come before the last key".into(),
        ));
    }
    let _ = last;
    Ok(out)
}

/// The character a keysym types, if it is a printable one (Latin-1 keysyms are their code points,
/// Unicode keysyms carry the code point after `0x01000000`).
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
pub(super) fn to_char(keysym: u32) -> Option<char> {
    match keysym {
        0x20..=0x7e | 0xa0..=0xff => char::from_u32(keysym),
        0x0100_0000..=0x0110_ffff => char::from_u32(keysym & 0x00ff_ffff),
        _ => None,
    }
}

/// Modifier keysyms.
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
pub(super) const CONTROL_L: u32 = 0xffe3;
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
pub(super) const ALT_L: u32 = 0xffe9;
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
pub(super) const SUPER_L: u32 = 0xffeb;
