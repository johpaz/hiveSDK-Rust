use super::{Inner, invalid};
use crate::{Error, Result};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    path::{Component, Path, PathBuf},
};

const WALK_LIMIT: usize = 50_000;

fn io(error: std::io::Error) -> Error {
    Error::State(format!("filesystem error: {}", error.kind()))
}

/// Resolve a root-relative path without escaping the canonical root: parent and
/// root components are rejected, and the nearest existing ancestor is
/// canonicalized so symlinks cannot lead outside. Check-then-use races with
/// another process modifying the tree are not prevented.
pub(super) fn resolve(root: &Path, relative: &str, allow_missing: bool) -> Result<PathBuf> {
    let mut clean = PathBuf::new();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            _ => return Err(invalid("path must be relative and stay inside the root")),
        }
    }
    let joined = root.join(&clean);
    let inside = |path: &Path| {
        path.starts_with(root)
            .then_some(())
            .ok_or_else(|| invalid("path escapes the sandbox root"))
    };
    if fs::symlink_metadata(&joined).is_ok() {
        let canonical = joined.canonicalize().map_err(io)?;
        inside(&canonical)?;
        return Ok(canonical);
    }
    if !allow_missing {
        return Err(invalid("path does not exist"));
    }
    let mut existing = joined.as_path();
    let mut tail = Vec::new();
    while fs::symlink_metadata(existing).is_err() {
        tail.push(
            existing
                .file_name()
                .ok_or_else(|| invalid("invalid path"))?,
        );
        existing = existing.parent().ok_or_else(|| invalid("invalid path"))?;
    }
    let mut canonical = existing.canonicalize().map_err(io)?;
    inside(&canonical)?;
    for part in tail.into_iter().rev() {
        canonical.push(part);
    }
    Ok(canonical)
}

pub(super) fn display(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn text(arguments: &Value, key: &str) -> String {
    arguments[key].as_str().unwrap_or_default().to_owned()
}

pub(super) fn run(inner: &Inner, name: &str, arguments: &Value) -> Result<Value> {
    match name {
        "fs_read" => read(inner, arguments),
        "fs_write" => write(inner, arguments),
        "fs_edit" => edit(inner, arguments),
        "fs_list" => list(inner, arguments),
        "fs_glob" => glob(inner, arguments),
        "fs_grep" => grep(inner, arguments),
        _ => Err(invalid("unknown filesystem tool")),
    }
}

fn read(inner: &Inner, arguments: &Value) -> Result<Value> {
    let path = resolve(&inner.root, &text(arguments, "path"), false)?;
    if !path.is_file() {
        return Err(invalid("path is not a file"));
    }
    let offset = arguments["offset"].as_u64().unwrap_or(0);
    let limit = arguments["limit"].as_u64().unwrap_or(2000);
    // The scan budget includes skipped lines. Take also bounds a single line
    // without a newline; read_until alone can allocate the entire file.
    let mut reader = BufReader::new(
        File::open(&path)
            .map_err(io)?
            .take(inner.max_read as u64 + 1),
    );
    let mut line = Vec::new();
    let (mut skipped, mut lines, mut bytes, mut truncated) = (0u64, 0u64, 0usize, false);
    let mut content = String::new();
    while skipped < offset {
        line.clear();
        if reader.read_until(b'\n', &mut line).map_err(io)? == 0 {
            break;
        }
        bytes += line.len();
        if bytes > inner.max_read {
            truncated = true;
            break;
        }
        skipped += 1;
    }
    while !truncated {
        line.clear();
        if reader.read_until(b'\n', &mut line).map_err(io)? == 0 {
            break;
        }
        if lines >= limit || bytes.saturating_add(line.len()) > inner.max_read {
            truncated = true;
            break;
        }
        content.push_str(
            std::str::from_utf8(&line).map_err(|_| invalid("file is not valid UTF-8 text"))?,
        );
        bytes += line.len();
        lines += 1;
    }
    Ok(json!({"path":display(&inner.root,&path),"content":content,
              "start_line":offset + 1,"lines":lines,"truncated":truncated}))
}

fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    if path.is_dir() {
        return Err(invalid("path is a directory"));
    }
    let parent = path.parent().ok_or_else(|| invalid("invalid path"))?;
    let temp = parent.join(format!(".hive-tmp-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(content)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(io)
}

/// Create or atomically replace a binary file inside the root; returns its
/// root-relative path. Callers bound `bytes` themselves.
pub(super) fn write_bytes(
    inner: &Inner,
    relative: &str,
    bytes: &[u8],
    create_dirs: bool,
) -> Result<String> {
    let path = resolve(&inner.root, relative, true)?;
    if path == inner.root {
        return Err(invalid("path must name a file"));
    }
    let parent = path.parent().ok_or_else(|| invalid("invalid path"))?;
    if !parent.exists() {
        if !create_dirs {
            return Err(invalid("parent directory does not exist; set create_dirs"));
        }
        fs::create_dir_all(parent).map_err(io)?;
    }
    atomic_write(&path, bytes)?;
    Ok(display(&inner.root, &path))
}

fn write(inner: &Inner, arguments: &Value) -> Result<Value> {
    let content = text(arguments, "content");
    if content.len() > inner.max_write {
        return Err(invalid("content exceeds max_write_bytes"));
    }
    let path = resolve(&inner.root, &text(arguments, "path"), true)?;
    if path == inner.root {
        return Err(invalid("path must name a file"));
    }
    let parent = path.parent().ok_or_else(|| invalid("invalid path"))?;
    if !parent.exists() {
        if arguments["create_dirs"].as_bool() != Some(true) {
            return Err(invalid("parent directory does not exist; set create_dirs"));
        }
        fs::create_dir_all(parent).map_err(io)?;
    }
    atomic_write(&path, content.as_bytes())?;
    Ok(json!({"path":display(&inner.root,&path),"bytes":content.len()}))
}

fn edit(inner: &Inner, arguments: &Value) -> Result<Value> {
    let path = resolve(&inner.root, &text(arguments, "path"), false)?;
    if !path.is_file() {
        return Err(invalid("path is not a file"));
    }
    if path.metadata().map_err(io)?.len() > inner.max_read as u64 {
        return Err(invalid("file exceeds max_read_bytes"));
    }
    let original = bounded_text(&path, inner.max_read)?;
    let (old, new) = (text(arguments, "old"), text(arguments, "new"));
    let count = original.matches(&old).count();
    let all = arguments["replace_all"].as_bool() == Some(true);
    if count == 0 {
        return Err(invalid("text to replace was not found"));
    }
    if count > 1 && !all {
        return Err(invalid(format!(
            "text matches {count} times; make it unique or set replace_all"
        )));
    }
    let result_size = original
        .len()
        .saturating_sub(old.len().saturating_mul(count))
        .saturating_add(new.len().saturating_mul(count));
    if result_size > inner.max_write {
        return Err(invalid("result exceeds max_write_bytes"));
    }
    let updated = original.replace(&old, &new);
    atomic_write(&path, updated.as_bytes())?;
    Ok(json!({"path":display(&inner.root,&path),"replacements":count}))
}

fn list(inner: &Inner, arguments: &Value) -> Result<Value> {
    let path = resolve(
        &inner.root,
        arguments["path"].as_str().unwrap_or("."),
        false,
    )?;
    if !path.is_dir() {
        return Err(invalid("path is not a directory"));
    }
    let limit = arguments["limit"].as_u64().unwrap_or(200) as usize;
    let mut entries = Vec::new();
    let mut scan_capped = false;
    for entry in fs::read_dir(&path).map_err(io)? {
        if entries.len() == WALK_LIMIT {
            scan_capped = true;
            break;
        }
        let entry = entry.map_err(io)?;
        let kind = entry.file_type().map_err(io)?;
        let kind_name = if kind.is_symlink() {
            "symlink"
        } else if kind.is_dir() {
            "dir"
        } else if kind.is_file() {
            "file"
        } else {
            "other"
        };
        let size = if kind.is_file() {
            entry.metadata().map(|meta| meta.len()).unwrap_or(0)
        } else {
            0
        };
        entries.push((
            entry.file_name().to_string_lossy().into_owned(),
            kind_name,
            size,
        ));
    }
    entries.sort();
    let truncated = scan_capped || entries.len() > limit;
    entries.truncate(limit);
    let entries: Vec<_> = entries
        .into_iter()
        .map(|(name, kind, size)| json!({"name":name,"type":kind,"size":size}))
        .collect();
    Ok(json!({"path":display(&inner.root,&path),"entries":entries,"truncated":truncated}))
}

/// Depth-first walk in sorted order that never follows symlinks and visits at
/// most `WALK_LIMIT` entries. `visit` returns false to stop early.
fn walk(start: &Path, mut visit: impl FnMut(&Path) -> bool) -> Result<bool> {
    let (mut stack, mut seen) = (vec![start.to_path_buf()], 0usize);
    while let Some(dir) = stack.pop() {
        let mut children: Vec<_> = fs::read_dir(&dir)
            .map_err(io)?
            .take(WALK_LIMIT.saturating_sub(seen) + 1)
            .filter_map(|entry| entry.ok())
            .collect();
        children.sort_by_key(|entry| entry.file_name());
        let mut subdirs = Vec::new();
        for entry in children {
            seen += 1;
            if seen > WALK_LIMIT {
                return Ok(true);
            }
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                subdirs.push(entry.path());
            } else if kind.is_file() && !visit(&entry.path()) {
                return Ok(false);
            }
        }
        stack.extend(subdirs.into_iter().rev());
    }
    Ok(false)
}

fn segment_match(pattern: &[char], text: &[char]) -> bool {
    let mut row = vec![false; text.len() + 1];
    row[0] = true;
    for symbol in pattern {
        let mut diagonal = row[0];
        row[0] &= *symbol == '*';
        for (index, character) in text.iter().enumerate() {
            let previous = row[index + 1];
            row[index + 1] = if *symbol == '*' {
                previous || row[index]
            } else {
                diagonal && (*symbol == '?' || symbol == character)
            };
            diagonal = previous;
        }
    }
    row[text.len()]
}

fn parts_match(pattern: &[&str], text: &[&str]) -> bool {
    let mut row = vec![false; text.len() + 1];
    row[0] = true;
    for part in pattern {
        let mut diagonal = row[0];
        row[0] &= *part == "**";
        for (index, segment) in text.iter().enumerate() {
            let previous = row[index + 1];
            row[index + 1] = if *part == "**" {
                previous || row[index]
            } else {
                diagonal
                    && segment_match(
                        &part.chars().collect::<Vec<_>>(),
                        &segment.chars().collect::<Vec<_>>(),
                    )
            };
            diagonal = previous;
        }
    }
    row[text.len()]
}

fn bounded_text(path: &Path, max: usize) -> Result<String> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(io)?
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() > max {
        return Err(invalid("file exceeds max_read_bytes"));
    }
    String::from_utf8(bytes).map_err(|_| invalid("file is not valid UTF-8 text"))
}

/// `*` and `?` stay inside one path segment; `**` spans segments. A pattern
/// without `/` matches the file name at any depth.
fn glob_match(pattern: &str, relative: &str) -> bool {
    let text: Vec<&str> = relative.split('/').collect();
    if pattern.contains('/') {
        let parts: Vec<&str> = pattern.split('/').collect();
        parts_match(&parts, &text)
    } else {
        parts_match(&[pattern], &text[text.len() - 1..])
    }
}

fn glob(inner: &Inner, arguments: &Value) -> Result<Value> {
    let pattern = text(arguments, "pattern");
    let limit = arguments["limit"].as_u64().unwrap_or(200) as usize;
    let mut matches = Vec::new();
    let mut truncated = false;
    let capped = walk(&inner.root, |path| {
        let relative = display(&inner.root, path);
        if glob_match(&pattern, &relative) {
            if matches.len() == limit {
                truncated = true;
                return false;
            }
            matches.push(relative);
        }
        true
    })?;
    Ok(json!({"matches":matches,"truncated":truncated || capped}))
}

fn grep(inner: &Inner, arguments: &Value) -> Result<Value> {
    let pattern = text(arguments, "pattern");
    let insensitive = arguments["case_insensitive"].as_bool() == Some(true);
    let needle = if insensitive {
        pattern.to_lowercase()
    } else {
        pattern
    };
    let filter = arguments["glob"].as_str().map(str::to_owned);
    let limit = arguments["limit"].as_u64().unwrap_or(100) as usize;
    let start = resolve(
        &inner.root,
        arguments["path"].as_str().unwrap_or("."),
        false,
    )?;
    let mut hits = Vec::new();
    let mut truncated = false;
    let mut scan = |path: &Path| -> bool {
        let relative = display(&inner.root, path);
        if filter
            .as_ref()
            .is_some_and(|glob| !glob_match(glob, &relative))
        {
            return true;
        }
        if path.metadata().map(|meta| meta.len()).unwrap_or(u64::MAX) > inner.max_read as u64 {
            return true;
        }
        let Ok(content) = bounded_text(path, inner.max_read) else {
            return true;
        };
        for (index, line) in content.lines().enumerate() {
            let found = if insensitive {
                line.to_lowercase().contains(&needle)
            } else {
                line.contains(&needle)
            };
            if found {
                if hits.len() == limit {
                    truncated = true;
                    return false;
                }
                let shown: String = line.chars().take(300).collect();
                hits.push(json!({"path":relative,"line":index + 1,"text":shown}));
            }
        }
        true
    };
    let capped = if start.is_file() {
        scan(&start);
        false
    } else {
        walk(&start, scan)?
    };
    Ok(json!({"matches":hits,"truncated":truncated || capped}))
}
