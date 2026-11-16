//! Minimal, bounded ZIP container for OOXML.
//!
//! The reader accepts what Office writes (stored or deflated entries, no
//! encryption, no ZIP64) and refuses everything else. Because the content comes
//! from an untrusted file, every size is checked before it is used: entry
//! counts, per-entry and total uncompressed bytes, compression ratio and the
//! CRC-32 of each inflated entry. Nothing is extracted to disk, so names are only
//! ever compared, never turned into paths.
use crate::{Error, Result};
use std::collections::BTreeMap;

const MAX_ENTRIES: usize = 4096;
const MAX_NAME: usize = 512;
/// Largest uncompressed entry and total across entries.
pub(super) const MAX_ENTRY_BYTES: usize = 32 * 1024 * 1024;
pub(super) const MAX_TOTAL_BYTES: usize = 96 * 1024 * 1024;
/// Entries above 1 MiB may not inflate more than this many times their size.
const MAX_RATIO: usize = 200;

fn invalid(message: &str) -> Error {
    Error::Input(format!("invalid ZIP/OOXML container: {message}"))
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn u16_at(data: &[u8], at: usize) -> Result<usize> {
    Ok(u16::from_le_bytes(
        data.get(at..at + 2)
            .ok_or_else(|| invalid("truncated"))?
            .try_into()
            .unwrap(),
    ) as usize)
}
fn u32_at(data: &[u8], at: usize) -> Result<usize> {
    Ok(u32::from_le_bytes(
        data.get(at..at + 4)
            .ok_or_else(|| invalid("truncated"))?
            .try_into()
            .unwrap(),
    ) as usize)
}

struct Entry {
    method: usize,
    crc: u32,
    compressed: usize,
    size: usize,
    offset: usize,
}

/// Named entries of an archive, inflated lazily.
pub(super) struct Archive<'a> {
    data: &'a [u8],
    entries: BTreeMap<String, Entry>,
    inflated: std::cell::Cell<usize>,
}

impl<'a> Archive<'a> {
    pub(super) fn open(data: &'a [u8]) -> Result<Self> {
        // End-of-central-directory record: last 22 bytes, or up to 64 KiB of comment before it.
        let floor = data.len().saturating_sub(22 + 65_535);
        let end = (floor..=data.len().saturating_sub(22))
            .rev()
            .find(|at| data.get(*at..*at + 4) == Some(b"PK\x05\x06"))
            .ok_or_else(|| invalid("end of central directory not found"))?;
        let (disk, with_directory) = (u16_at(data, end + 4)?, u16_at(data, end + 6)?);
        let (count, total) = (u16_at(data, end + 8)?, u16_at(data, end + 10)?);
        if disk != 0 || with_directory != 0 || count != total {
            return Err(invalid("multi-disk archives are not supported"));
        }
        if count == 0xffff
            || u32_at(data, end + 12)? == 0xffff_ffff
            || u32_at(data, end + 16)? == 0xffff_ffff
        {
            return Err(invalid("ZIP64 archives are not supported"));
        }
        if count > MAX_ENTRIES {
            return Err(invalid("too many entries"));
        }
        let (size, offset) = (u32_at(data, end + 12)?, u32_at(data, end + 16)?);
        if offset.checked_add(size).is_none_or(|stop| stop > end) {
            return Err(invalid("central directory is out of bounds"));
        }
        let mut entries = BTreeMap::new();
        let mut at = offset;
        let mut declared_total = 0usize;
        for _ in 0..count {
            if data.get(at..at + 4) != Some(b"PK\x01\x02") {
                return Err(invalid("bad central directory entry"));
            }
            let flags = u16_at(data, at + 8)?;
            let method = u16_at(data, at + 10)?;
            let (crc, compressed, uncompressed) = (
                u32_at(data, at + 16)? as u32,
                u32_at(data, at + 20)?,
                u32_at(data, at + 24)?,
            );
            let (name_len, extra_len, comment_len) = (
                u16_at(data, at + 28)?,
                u16_at(data, at + 30)?,
                u16_at(data, at + 32)?,
            );
            let local = u32_at(data, at + 42)?;
            let name_bytes = data
                .get(at + 46..at + 46 + name_len)
                .ok_or_else(|| invalid("truncated name"))?;
            at += 46 + name_len + extra_len + comment_len;
            if flags & 1 != 0 {
                return Err(invalid("encrypted entries are not supported"));
            }
            if name_len == 0 || name_len > MAX_NAME {
                return Err(invalid("bad entry name length"));
            }
            let name = std::str::from_utf8(name_bytes)
                .map_err(|_| invalid("entry name is not UTF-8"))?
                .to_owned();
            if name.ends_with('/') {
                continue;
            }
            if !matches!(method, 0 | 8) {
                return Err(invalid("unsupported compression method"));
            }
            if uncompressed > MAX_ENTRY_BYTES {
                return Err(invalid("entry is too large"));
            }
            declared_total = declared_total.saturating_add(uncompressed);
            if declared_total > MAX_TOTAL_BYTES {
                return Err(invalid("archive expands beyond the size limit"));
            }
            if uncompressed > 1024 * 1024 && uncompressed / compressed.max(1) > MAX_RATIO {
                return Err(invalid("suspicious compression ratio"));
            }
            if entries
                .insert(
                    name,
                    Entry {
                        method,
                        crc,
                        compressed,
                        size: uncompressed,
                        offset: local,
                    },
                )
                .is_some()
            {
                return Err(invalid("duplicate entry name"));
            }
        }
        Ok(Self {
            data,
            entries,
            inflated: std::cell::Cell::new(0),
        })
    }

    pub(super) fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    pub(super) fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Bytes of an entry, verified against its declared size and CRC-32.
    pub(super) fn read(&self, name: &str) -> Result<Vec<u8>> {
        let entry = self
            .entries
            .get(name)
            .ok_or_else(|| invalid(&format!("missing part {name}")))?;
        if self.data.get(entry.offset..entry.offset + 4) != Some(b"PK\x03\x04") {
            return Err(invalid("bad local header"));
        }
        let (name_len, extra_len) = (
            u16_at(self.data, entry.offset + 26)?,
            u16_at(self.data, entry.offset + 28)?,
        );
        let start = entry.offset + 30 + name_len + extra_len;
        let body = self
            .data
            .get(start..start + entry.compressed)
            .ok_or_else(|| invalid("entry data is out of bounds"))?;
        let bytes = if entry.method == 0 {
            body.to_vec()
        } else {
            miniz_oxide::inflate::decompress_to_vec_with_limit(
                body,
                entry.size.min(MAX_ENTRY_BYTES) + 1,
            )
            .map_err(|_| invalid("corrupt or oversized deflate stream"))?
        };
        if bytes.len() != entry.size || crc32(&bytes) != entry.crc {
            return Err(invalid(
                "entry content does not match its recorded size and CRC-32",
            ));
        }
        let total = self.inflated.get().saturating_add(bytes.len());
        if total > MAX_TOTAL_BYTES {
            return Err(invalid("archive expands beyond the size limit"));
        }
        self.inflated.set(total);
        Ok(bytes)
    }

    pub(super) fn read_text(&self, name: &str) -> Result<String> {
        String::from_utf8(self.read(name)?)
            .map_err(|_| invalid(&format!("part {name} is not UTF-8")))
    }
}

/// Deterministic writer: fixed 1980-01-01 timestamps, UTF-8 names, deflate.
#[derive(Default)]
pub(super) struct Writer {
    body: Vec<u8>,
    directory: Vec<u8>,
    count: usize,
}

impl Writer {
    pub(super) fn add(&mut self, name: &str, data: &[u8]) -> Result<()> {
        if name.is_empty()
            || name.len() > MAX_NAME
            || name.starts_with('/')
            || name.contains("..")
            || name.contains('\\')
        {
            return Err(Error::Input("invalid archive part name".into()));
        }
        if data.len() > MAX_ENTRY_BYTES || self.count >= MAX_ENTRIES {
            return Err(Error::Input(
                "generated document exceeds the archive limits".into(),
            ));
        }
        let packed = miniz_oxide::deflate::compress_to_vec(data, 6);
        let (method, stored): (u16, &[u8]) = if packed.len() < data.len() {
            (8, &packed)
        } else {
            (0, data)
        };
        let crc = crc32(data);
        let offset =
            u32::try_from(self.body.len()).map_err(|_| Error::Input("archive too large".into()))?;
        let header = |out: &mut Vec<u8>, central: bool| {
            out.extend_from_slice(if central {
                b"PK\x01\x02"
            } else {
                b"PK\x03\x04"
            });
            if central {
                out.extend_from_slice(&20u16.to_le_bytes());
            }
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0x0800u16.to_le_bytes());
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0x0021u16.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(stored.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
        };
        header(&mut self.body, false);
        self.body.extend_from_slice(name.as_bytes());
        self.body.extend_from_slice(stored);
        header(&mut self.directory, true);
        self.directory
            .extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        self.directory.extend_from_slice(&offset.to_le_bytes());
        self.directory.extend_from_slice(name.as_bytes());
        self.count += 1;
        Ok(())
    }

    pub(super) fn finish(mut self) -> Result<Vec<u8>> {
        let start =
            u32::try_from(self.body.len()).map_err(|_| Error::Input("archive too large".into()))?;
        let size = u32::try_from(self.directory.len())
            .map_err(|_| Error::Input("archive too large".into()))?;
        self.body.append(&mut self.directory);
        self.body.extend_from_slice(b"PK\x05\x06\0\0\0\0");
        self.body
            .extend_from_slice(&(self.count as u16).to_le_bytes());
        self.body
            .extend_from_slice(&(self.count as u16).to_le_bytes());
        self.body.extend_from_slice(&size.to_le_bytes());
        self.body.extend_from_slice(&start.to_le_bytes());
        self.body.extend_from_slice(&0u16.to_le_bytes());
        Ok(self.body)
    }
}
