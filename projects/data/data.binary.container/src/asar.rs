//! Decoding of Electron `asar` archives.
//!
//! An `asar` archive is a pickle-serialized JSON header followed by one
//! concatenated data blob. The first sixteen bytes are four little-endian
//! words: the constant `4`, the serialized header block size, the string-block
//! size and the JSON length. The JSON tree lists every file as
//! `{"files": {name: {"size", "offset", "unpacked"?}}}` where `offset` is
//! relative to the data area, which starts at `8 + block_size`. Files marked
//! `unpacked` live next to the archive on disk and carry no blob bytes.
//!
//! This module decodes the tree statically and can write the packed entries
//! back out under a caller-chosen directory; nothing is executed.
//!
//! [`analyze`] takes the whole container image; offsets in the report are
//! absolute inside that image.

use std::{
    collections::BTreeMap,
    path::{Component, Path},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::read::window;
use crate::{ContainerError, Result};

/// Value of the first header word; it is the pickle payload-size field.
const MAGIC: u32 = 4;

/// Size of the four header words.
const HEADER_SIZE: u64 = 16;

/// Default cap on listed file entries.
const DEFAULT_ENTRIES_LIMIT: usize = 4096;

/// Default cap on one extracted file.
const DEFAULT_EXTRACT_LIMIT: usize = 256 * 1024 * 1024;

/// Hard cap on the JSON header block.
const MAX_JSON_SIZE: usize = 64 * 1024 * 1024;

/// Inputs of the `asar` decode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsarOptions {
    /// Absolute offset of the archive start; defaults to the image start.
    #[serde(default)]
    pub offset: Option<u64>,
    /// Cap on listed file entries; clamped to 65536.
    #[serde(default = "default_entries_limit")]
    pub entries_limit: usize,
    /// Optional directory to extract the packed entries into.
    #[serde(default)]
    pub extract_dir: Option<String>,
    /// Cap on one extracted file in bytes; clamped to 1 GiB.
    #[serde(default = "default_extract_limit")]
    pub extract_limit: usize,
}

impl Default for AsarOptions {
    fn default() -> Self {
        Self { offset: None, entries_limit: default_entries_limit(), extract_dir: None, extract_limit: default_extract_limit() }
    }
}

/// The sixteen-byte `asar` header plus the derived data-area facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsarHeader {
    /// Absolute offset of the first header word.
    pub start_offset: u64,
    /// Serialized header block size; the data area starts at `8 + block_size`.
    pub block_size: u32,
    /// String-block size recorded by the pickle wrapper.
    pub string_block_size: u32,
    /// Byte length of the JSON header.
    pub string_size: u32,
    /// Absolute offset where packed file data begins.
    pub data_start: u64,
}

/// One file listed in the header tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsarEntry {
    /// Slash-separated path inside the archive, in tree order.
    pub path: String,
    /// Uncompressed size in bytes.
    pub size: u64,
    /// Absolute file offset of the packed bytes; `None` for unpacked entries.
    pub offset: Option<u64>,
    /// Whether the file is stored loose next to the archive instead of packed.
    pub unpacked: bool,
    /// Destination file written during extraction, when requested.
    pub extracted: Option<String>,
}

/// Full decode of one `asar` archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsarReport {
    /// Decoded header and data-area facts.
    pub header: AsarHeader,
    /// Total number of files in the tree, before the listing limit.
    pub entry_count: usize,
    /// Number of directories in the tree.
    pub directory_count: usize,
    /// Total packed bytes referenced by the tree.
    pub packed_bytes: u64,
    /// Files in path order, cut at the listing limit.
    pub entries: Vec<AsarEntry>,
    /// Whether the listing was cut at the limit.
    pub truncated: bool,
}

fn default_entries_limit() -> usize {
    DEFAULT_ENTRIES_LIMIT
}

fn default_extract_limit() -> usize {
    DEFAULT_EXTRACT_LIMIT
}

/// Decodes the `asar` archive of one image.
pub fn analyze(image: &[u8], options: &AsarOptions) -> Result<AsarReport> {
    let start = options.offset.unwrap_or(0);
    let head = window(image, start, HEADER_SIZE)?;
    let magic = word(head, 0);
    if magic != MAGIC {
        return Err(ContainerError::parse(format!("expected asar header magic {MAGIC}, found {magic}")).at(start, start + 4));
    }
    let block_size = word(head, 4);
    let string_block_size = word(head, 8);
    let string_size = word(head, 12);

    let json_size = usize::try_from(string_size)
        .map_err(|_| ContainerError::parse("asar JSON header is too large").at(start + 12, start + 16))?;
    if string_size == 0 || json_size > MAX_JSON_SIZE {
        return Err(ContainerError::budget(format!("the asar JSON header size {string_size} is out of range"))
            .at(start + 12, start + 16));
    }
    let data_start = start
        .checked_add(8)
        .and_then(|value| value.checked_add(u64::from(block_size)))
        .ok_or_else(|| ContainerError::parse("asar header block size overflows"))?;
    if data_start > image.len() as u64 {
        return Err(ContainerError::parse(format!("the asar data area starts at {data_start}, past the end of the file"))
            .at(start, data_start));
    }

    let json = window(image, start + HEADER_SIZE, u64::from(string_size))?;
    let tree: Value = serde_json::from_slice(json)
        .map_err(|error| ContainerError::parse(format!("asar JSON header: {error}")).at(start + 16, start + 16 + string_size as u64))?;

    let mut files = BTreeMap::new();
    let mut directories = 0usize;
    walk(&tree, "", &mut files, &mut directories)?;

    let entry_count = files.len();
    let packed_bytes = files.iter().filter(|(_, info)| !info.unpacked).map(|(_, info)| info.size).sum();

    let limit = options.entries_limit.clamp(1, 65536);
    let mut truncated = false;
    let mut entries = Vec::new();
    for (path, info) in &files {
        if entries.len() == limit {
            truncated = true;
            break;
        }
        let offset = if info.unpacked { None } else { data_start.checked_add(info.relative_offset.unwrap_or(0)) };
        entries.push(AsarEntry { path: path.clone(), size: info.size, offset, unpacked: info.unpacked, extracted: None });
    }

    if let Some(dir) = &options.extract_dir {
        let limit_bytes = options.extract_limit.clamp(1, 1024 * 1024 * 1024) as u64;
        for entry in &mut entries {
            let Some(absolute) = entry.offset
            else {
                continue;
            };
            if entry.size == 0 || entry.size > limit_bytes {
                continue;
            }
            if absolute.checked_add(entry.size).is_none_or(|end| end > image.len() as u64) {
                return Err(ContainerError::parse(format!("asar entry {} runs past the end of the file", entry.path))
                    .at(absolute, absolute + entry.size));
            }
            let bytes = window(image, absolute, entry.size)?;
            let written = write_entry(Path::new(dir), &entry.path, bytes)?;
            entry.extracted = Some(written);
        }
    }

    Ok(AsarReport {
        header: AsarHeader { start_offset: start, block_size, string_block_size, string_size, data_start },
        entry_count,
        directory_count: directories,
        packed_bytes,
        entries,
        truncated,
    })
}

/// Tree facts collected for one file node.
struct FileInfo {
    size: u64,
    relative_offset: Option<u64>,
    unpacked: bool,
}

/// Collects every file node into `files` and counts directories.
fn walk(node: &Value, prefix: &str, files: &mut BTreeMap<String, FileInfo>, directories: &mut usize) -> Result<()> {
    let children = match node.get("files") {
        Some(Value::Object(map)) => map,
        Some(_) => return Err(ContainerError::parse("an asar directory has a non-object files field")),
        None => return Ok(()),
    };
    for (name, child) in children {
        if !name.chars().all(|c| c != '/' && c != '\\' && c != '\0') {
            return Err(ContainerError::parse(format!("unsafe asar entry name: {name}")));
        }
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        if child.get("files").is_some() {
            *directories += 1;
            walk(child, &path, files, directories)?;
            continue;
        }
        let size = number(child.get("size")).unwrap_or(0);
        let unpacked = child.get("unpacked").and_then(Value::as_bool).unwrap_or(false);
        let relative_offset = number(child.get("offset"));
        files.insert(path, FileInfo { size, relative_offset, unpacked });
    }
    Ok(())
}

/// Reads a JSON number that may be stored as an integer or a decimal string.
fn number(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// Writes one packed entry under `directory`, mirroring its archive path.
fn write_entry(directory: &Path, path: &str, bytes: &[u8]) -> Result<String> {
    let relative = path.trim_start_matches('/');
    let mut target = directory.to_path_buf();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => target.push(part),
            _ => return Err(ContainerError::parse(format!("unsafe asar entry path: {path}"))),
        }
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, bytes)?;
    Ok(target.display().to_string())
}

/// Reads one little-endian word from a fetched header window.
fn word(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four-byte window"))
}
