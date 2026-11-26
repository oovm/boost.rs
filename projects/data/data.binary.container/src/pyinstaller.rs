//! Static decoding of PyInstaller `CArchive` bundles and `PYZ` archives.
//!
//! A PyInstaller executable carries its bundle appended to the binary: the
//! last bytes hold a cookie — the eight-byte magic `MEI\014\013\012\013\016`
//! followed by four big-endian words (package length, TOC offset, TOC length,
//! version word) — and the cookie is located by scanning the final kilobyte.
//! The package starts `package length` bytes before the cookie end, the table
//! of contents lives at its declared offset inside the package, and every TOC
//! entry is a big-endian header (`entry size`, `offset`, `length`, `compressed
//! length`, a one-bit compression flag and a type code) followed by a
//! NUL-terminated name. Entry offsets are package-relative.
//!
//! A type-code `z` entry is a `PYZ` archive: the magic `PYZ\0`, a four-byte
//! CPython magic, a big-endian TOC offset and a `marshal` dictionary mapping
//! dotted module names to `(is_package, offset, length)` triples whose offsets
//! are archive-relative. The dictionary is read with the crate's own
//! `marshal` decoder, and module payloads are zlib streams of bare `marshal`
//! code objects — exactly what [`crate::pyc`] consumes.
//!
//! Compression is zlib (`data-compression`, `Envelope::Zlib`) bounded by the
//! declared length. Nothing is executed, no external process is spawned, and
//! extraction writes only traversal-safe paths under the caller-chosen
//! directory.

use std::path::{Component, Path};

use data_compression::{CodecLimits, Zlib};
use serde::{Deserialize, Serialize};

use crate::pyc::magic_version;
use crate::read::window;
use crate::{ContainerError, Result};

/// The `CArchive` cookie magic.
const COOKIE_MAGIC: [u8; 8] = [b'M', b'E', b'I', 0x0C, 0x0B, 0x0A, 0x0B, 0x0E];

/// Byte length of one cookie (magic plus four big-endian words).
const COOKIE_SIZE: usize = 24;

/// How far back from the end of the file the cookie magic is searched.
const COOKIE_SCAN_WINDOW: usize = 1024;

/// The `PYZ` archive magic.
const PYZ_MAGIC: [u8; 4] = [b'P', b'Y', b'Z', 0x00];

/// Byte length of one fixed TOC entry header: four big-endian words, a flags
/// byte and a type-code byte. The declared entry size counts this header plus
/// the NUL-terminated name, so the name field is `entry size - 18` bytes.
const TOC_HEADER_SIZE: usize = 18;

/// `flags` bit marking a zlib-compressed entry.
const FLAG_COMPRESSED: u8 = 0x01;

/// Cap on TOC entries and PYZ modules decoded before truncating.
const DEFAULT_ENTRY_LIMIT: usize = 4096;

/// Cap on the decompressed bytes returned per entry or module.
const DEFAULT_PAYLOAD_LIMIT: usize = 256 * 1024 * 1024;

/// Cap on one fetched stored payload or whole PYZ image.
const MAX_PAYLOAD_BYTES: u64 = 1024 * 1024 * 1024;

/// One decoded `CArchive` or `PYZ` container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PyInstallerReport {
    /// Which container was decoded: `carchive` or `pyz`.
    pub archive_kind: String,
    /// Absolute offset of the package (or PYZ) start.
    pub start_offset: u64,
    /// Absolute offset of the cookie magic, `CArchive` only.
    pub cookie_offset: Option<u64>,
    /// Declared package length, `CArchive` only.
    pub package_length: Option<u64>,
    /// Package-relative TOC offset, `CArchive` only.
    pub toc_offset: Option<u64>,
    /// Byte length of the TOC, `CArchive` only.
    pub toc_length: Option<u64>,
    /// Raw cookie version word, `CArchive` only.
    pub version_word: Option<u32>,
    /// The version word rendered as `major.minor`.
    pub version: Option<String>,
    /// CPython magic recorded by a `PYZ` header.
    pub python_magic: Option<u32>,
    /// Interpreter named by the `PYZ` magic, e.g. `CPython 3.12`.
    pub python: Option<String>,
    /// TOC entries (`CArchive`) or, for a standalone `PYZ`, the module count.
    pub entry_count: usize,
    /// TOC entries in order, cut at the limit; `CArchive` only.
    pub entries: Vec<PyInstallerEntry>,
    /// Modules for a standalone `PYZ`, cut at the limit.
    pub modules: Vec<PyInstallerModule>,
    /// Whether any list was cut at the limit.
    pub truncated: bool,
}

/// One `CArchive` TOC entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PyInstallerEntry {
    /// Zero-based position in the TOC.
    pub index: usize,
    /// Entry name as stored, NULs trimmed.
    pub name: String,
    /// Single-character type code.
    pub typecode: String,
    /// Kind named from the type code: `script`, `module`, `binary`, `data`,
    /// `pyz` or `other`.
    pub kind: String,
    /// Package-relative offset of the payload.
    pub offset: u64,
    /// Declared uncompressed length.
    pub length: u64,
    /// Stored (compressed) length.
    pub compress_length: u64,
    /// Whether the payload carries a zlib stream.
    pub compressed: bool,
    /// Module count for a `PYZ` entry, `None` when the archive did not parse.
    pub module_count: Option<usize>,
    /// Modules decoded from a `PYZ` entry, cut at the limit.
    pub modules: Vec<PyInstallerModule>,
    /// Destination file written during extraction, when requested.
    pub extracted: Option<String>,
    /// Whether this entry's module list was cut.
    pub truncated: bool,
}

/// One `PYZ` module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PyInstallerModule {
    /// Dotted module name.
    pub name: String,
    /// Whether the entry is a package.
    pub is_package: bool,
    /// Archive-relative offset of the payload.
    pub offset: u64,
    /// Stored length of the payload.
    pub length: u64,
    /// Destination file written during extraction, when requested.
    pub extracted: Option<String>,
}

/// Inputs of one PyInstaller decode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PyInstallerOptions {
    /// Absolute offset of the container start; `None` uses the image start.
    #[serde(default)]
    pub offset: Option<u64>,
    /// Cap on listed entries and modules; clamped to 65536.
    #[serde(default = "default_entry_limit")]
    pub entries_limit: usize,
    /// Optional directory to extract payloads into.
    #[serde(default)]
    pub extract_dir: Option<String>,
    /// Cap on one extracted file in bytes; clamped to 1 GiB.
    #[serde(default = "default_payload_limit")]
    pub extract_limit: usize,
}

impl Default for PyInstallerOptions {
    fn default() -> Self {
        Self { offset: None, entries_limit: default_entry_limit(), extract_dir: None, extract_limit: default_payload_limit() }
    }
}

fn default_entry_limit() -> usize {
    DEFAULT_ENTRY_LIMIT
}

fn default_payload_limit() -> usize {
    DEFAULT_PAYLOAD_LIMIT
}

/// Whether the bytes start a `PYZ` archive.
pub fn looks_like_pyz(bytes: &[u8]) -> bool {
    bytes.starts_with(&PYZ_MAGIC)
}

/// Locates the cookie magic in the final kilobyte of the image, returning its
/// absolute offset.
pub fn locate_cookie(image: &[u8]) -> Option<u64> {
    let size = image.len() as u64;
    if size < (COOKIE_SIZE + COOKIE_MAGIC.len()) as u64 {
        return None;
    }
    let window_size = size.min(COOKIE_SCAN_WINDOW as u64);
    let start = size - window_size;
    let bytes = window(image, start, window_size).ok()?;
    let position = bytes.windows(COOKIE_MAGIC.len()).rposition(|candidate| candidate == COOKIE_MAGIC)?;
    let absolute = start + position as u64;
    // The four words after the magic must fit in the file.
    if absolute + COOKIE_SIZE as u64 > size {
        return None;
    }
    Some(absolute)
}

/// Decodes the `CArchive` whose cookie ends the image.
pub fn analyze_carchive(image: &[u8], options: &PyInstallerOptions) -> Result<PyInstallerReport> {
    let size = image.len() as u64;
    let cookie = locate_cookie(image)
        .ok_or_else(|| ContainerError::parse("no PyInstaller CArchive cookie was found in the final 1024 bytes"))?;
    let tail = window(image, cookie, size - cookie)?;
    let package_length = be32(tail, 8)?;
    let toc_offset = be32(tail, 12)?;
    let toc_length = be32(tail, 16)?;
    let version_word = be32(tail, 20)?;

    let cookie_end = cookie + COOKIE_SIZE as u64;
    let package_len = u64::from(package_length);
    if package_len == 0 || package_len > cookie_end {
        return Err(ContainerError::parse(format!(
            "the cookie declares a package length of {package_length}, outside the file"
        ))
        .at(cookie, cookie_end));
    }
    let package_start = cookie_end - package_len;
    let toc_start = package_start
        .checked_add(u64::from(toc_offset))
        .ok_or_else(|| ContainerError::parse("the TOC offset overflows the file"))?;
    let toc_end =
        toc_start.checked_add(u64::from(toc_length)).ok_or_else(|| ContainerError::parse("the TOC length overflows the file"))?;
    if toc_end > cookie_end {
        return Err(ContainerError::parse(format!("the TOC at {toc_start}..{toc_end} runs past the cookie end {cookie_end}"))
            .at(toc_start, toc_end));
    }
    let toc = window(image, toc_start, u64::from(toc_length))?;

    let limit = options.entries_limit.clamp(1, 65536);
    let mut entries: Vec<PyInstallerEntry> = Vec::new();
    let mut truncated = false;
    let mut cursor = 0usize;
    let mut entry_count = 0usize;
    while cursor < toc.len() {
        if toc.len() - cursor < TOC_HEADER_SIZE {
            return Err(ContainerError::parse(format!(
                "the TOC ends with a {:#x}-byte partial entry header",
                toc.len() - cursor
            ))
            .at(toc_start + cursor as u64, toc_end));
        }
        let entry_size = be32(toc, cursor)? as usize;
        let offset = be32(toc, cursor + 4)?;
        let length = be32(toc, cursor + 8)?;
        let compress_length = be32(toc, cursor + 12)?;
        let flags = toc[cursor + 16];
        let typecode = toc[cursor + 17];
        if entry_size < TOC_HEADER_SIZE {
            return Err(ContainerError::parse(format!(
                "the TOC entry at {cursor:#x} declares a size of {entry_size}, under the {TOC_HEADER_SIZE}-byte header"
            ))
            .at(toc_start + cursor as u64, toc_start + cursor as u64 + TOC_HEADER_SIZE as u64));
        }
        let name_end = cursor + entry_size;
        if name_end > toc.len() {
            return Err(ContainerError::parse(format!(
                "the TOC entry at {cursor:#x} declares a size of {entry_size}, past the TOC end"
            ))
            .at(toc_start + cursor as u64, toc_start + name_end as u64));
        }
        let raw_name = &toc[cursor + TOC_HEADER_SIZE..name_end];
        let name = String::from_utf8_lossy(raw_name).trim_end_matches('\0').to_string();
        cursor = name_end;
        entry_count += 1;
        if entries.len() >= limit {
            truncated = true;
            continue;
        }
        entries.push(PyInstallerEntry {
            index: entry_count - 1,
            name,
            typecode: char::from(typecode).to_string(),
            kind: kind_of(typecode).to_string(),
            offset: u64::from(offset),
            length: u64::from(length),
            compress_length: u64::from(compress_length),
            compressed: flags & FLAG_COMPRESSED != 0,
            module_count: None,
            modules: Vec::new(),
            extracted: None,
            truncated: false,
        });
    }

    // Decode PYZ payloads for the module listing and extract on request.
    let cap = options.extract_limit.clamp(1, 1024 * 1024 * 1024);
    let directory = options.extract_dir.as_deref().map(Path::new);
    for entry in &mut entries {
        if entry.typecode != "z" && directory.is_none() {
            continue;
        }
        let payload = payload(image, package_start, entry, size)?;
        if entry.typecode == "z" {
            let modules = decode_pyz(&payload, limit).unwrap_or_default();
            entry.module_count = Some(modules.0);
            entry.truncated = modules.1;
            entry.modules = modules.2;
        }
        if let Some(dir) = directory {
            if entry.typecode != "z" && entry.length as usize <= cap {
                let written = write_entry(dir, &entry.name, &payload)?;
                entry.extracted = Some(written);
            }
            if entry.typecode == "z" {
                // The PYZ blob itself is not written; its modules are the payload.
                extract_pyz_modules(&payload, dir, &entry.name, &mut entry.modules, cap)?;
            }
        }
    }

    Ok(PyInstallerReport {
        archive_kind: "carchive".to_string(),
        start_offset: package_start,
        cookie_offset: Some(cookie),
        package_length: Some(u64::from(package_length)),
        toc_offset: Some(u64::from(toc_offset)),
        toc_length: Some(u64::from(toc_length)),
        version_word: Some(version_word),
        version: Some(version_label(version_word)),
        python_magic: None,
        python: None,
        entry_count,
        entries,
        modules: Vec::new(),
        truncated,
    })
}

/// Decodes a standalone `PYZ` archive starting at `offset`.
pub fn analyze_pyz(image: &[u8], options: &PyInstallerOptions) -> Result<PyInstallerReport> {
    let size = image.len() as u64;
    let base = options.offset.unwrap_or(0);
    if base >= size {
        return Err(ContainerError::parse(format!("offset {base} is past the end of the file")).at(base, size));
    }
    let span = size - base;
    if span > MAX_PAYLOAD_BYTES {
        return Err(ContainerError::budget(format!(
            "the PYZ archive at {base} is {span} bytes, over the {MAX_PAYLOAD_BYTES} byte cap"
        ))
        .at(base, size));
    }
    let archive = window(image, base, span)?;
    if !looks_like_pyz(archive) {
        return Err(ContainerError::parse("the bytes at the offset are not a PYZ archive").at(base, base + 4));
    }
    if archive.len() < 12 {
        return Err(ContainerError::parse("the PYZ archive is shorter than its header").at(base, size));
    }
    let python_magic = u32::from_le_bytes(archive[4..8].try_into().expect("four-byte window"));
    let toc_offset = be32(archive, 8)?;
    let toc_start = usize::try_from(toc_offset)
        .map_err(|_| ContainerError::parse("the PYZ TOC offset overflows the archive").at(base, size))?;
    if toc_start >= archive.len() {
        return Err(ContainerError::parse(format!("the PYZ TOC offset {toc_offset} is past the end of the archive"))
            .at(base + toc_start as u64, size));
    }
    let (count, truncated, mut modules) = pyz_modules(&archive[toc_start..], limit_of(options))?;

    if let Some(dir) = options.extract_dir.as_deref().map(Path::new) {
        let cap = options.extract_limit.clamp(1, 1024 * 1024 * 1024);
        let pyz_name = format!("archive-{base:x}.pyz");
        extract_pyz_modules(archive, dir, &pyz_name, &mut modules, cap)?;
    }

    Ok(PyInstallerReport {
        archive_kind: "pyz".to_string(),
        start_offset: base,
        cookie_offset: None,
        package_length: None,
        toc_offset: Some(u64::from(toc_offset)),
        toc_length: None,
        version_word: None,
        version: None,
        python_magic: Some(python_magic),
        python: magic_version(python_magic).map(|version| format!("CPython {}.{}", version / 100, version % 100)),
        entry_count: count,
        entries: Vec::new(),
        modules,
        truncated,
    })
}

fn limit_of(options: &PyInstallerOptions) -> usize {
    options.entries_limit.clamp(1, 65536)
}

/// Reads one entry payload: the stored window, inflated when flagged.
fn payload(image: &[u8], package_start: u64, entry: &PyInstallerEntry, size: u64) -> Result<Vec<u8>> {
    let start = package_start
        .checked_add(entry.offset)
        .ok_or_else(|| ContainerError::parse("the entry offset overflows the file"))?;
    let end = start
        .checked_add(entry.compress_length)
        .ok_or_else(|| ContainerError::parse("the entry length overflows the file"))?;
    if end > size {
        return Err(ContainerError::parse(format!("the entry at {start:#x}..{end:#x} runs past the end of the file"))
            .at(start, end));
    }
    if entry.compress_length > MAX_PAYLOAD_BYTES {
        return Err(ContainerError::budget(format!(
            "the stored payload of entry {} is {} bytes, over the {MAX_PAYLOAD_BYTES} byte cap",
            entry.index, entry.compress_length
        ))
        .at(start, end));
    }
    let stored = window(image, start, entry.compress_length)?;
    if !entry.compressed {
        return Ok(stored.to_vec());
    }
    let limit = entry.length.min(DEFAULT_PAYLOAD_LIMIT as u64) as usize;
    let decoded = Zlib::decode_bounded(stored, CodecLimits::new().with_max_output_bytes(limit as u64));
    if decoded.is_complete() || !decoded.output.is_empty() {
        Ok(decoded.output)
    }
    else {
        Err(ContainerError::parse(format!("the zlib payload of entry {} did not inflate", entry.index)).at(start, end))
    }
}

/// Parses a PYZ payload into `(count, truncated, modules)`.
fn decode_pyz(bytes: &[u8], limit: usize) -> Result<(usize, bool, Vec<PyInstallerModule>)> {
    if !looks_like_pyz(bytes) {
        return Err(ContainerError::parse("the PYZ entry does not start with the PYZ magic"));
    }
    if bytes.len() < 12 {
        return Err(ContainerError::parse("the PYZ entry is shorter than its header"));
    }
    let toc_offset = be32(bytes, 8)? as usize;
    if toc_offset >= bytes.len() {
        return Err(ContainerError::parse("the PYZ TOC offset is past the end of the archive"));
    }
    pyz_modules(&bytes[toc_offset..], limit)
}

/// Reads the `marshal` module dictionary of a PYZ TOC.
fn pyz_modules(toc: &[u8], limit: usize) -> Result<(usize, bool, Vec<PyInstallerModule>)> {
    let entries = crate::pyc::marshal_module_toc(toc)?;
    let mut truncated = false;
    let mut modules = Vec::new();
    for (name, is_package, offset, length) in &entries {
        if modules.len() >= limit {
            truncated = true;
            continue;
        }
        modules.push(PyInstallerModule {
            name: name.clone(),
            is_package: *is_package,
            offset: *offset,
            length: *length,
            extracted: None,
        });
    }
    Ok((entries.len(), truncated, modules))
}

/// Inflates and writes every listed module from one in-memory PYZ image.
fn extract_pyz_modules(
    bytes: &[u8],
    directory: &Path,
    pyz_name: &str,
    modules: &mut [PyInstallerModule],
    cap: usize,
) -> Result<()> {
    for module in modules {
        if module.length as usize > cap {
            continue;
        }
        let end = module
            .offset
            .checked_add(module.length)
            .ok_or_else(|| ContainerError::parse("the module length overflows the archive"))?;
        if end > bytes.len() as u64 {
            return Err(ContainerError::parse(format!("the PYZ module {} runs past the end of the archive", module.name))
                .at(module.offset, end));
        }
        let stored = &bytes[module.offset as usize..end as usize];
        // The PYZ table records the stored length, so the inflate cap is the
        // payload limit, not the declared size.
        let decoded = Zlib::decode_bounded(stored, CodecLimits::new().with_max_output_bytes(DEFAULT_PAYLOAD_LIMIT as u64));
        let out = if decoded.is_complete() || !decoded.output.is_empty() {
            decoded.output
        }
        else {
            // `--no-compress` bundles store bare marshal streams.
            stored.to_vec()
        };
        let written = write_entry(directory, &format!("{pyz_name}/{}.pyc", module.name), &out)?;
        module.extracted = Some(written);
    }
    Ok(())
}

/// Names the kind behind a type code.
fn kind_of(typecode: u8) -> &'static str {
    match typecode {
        b's' => "script",
        b'M' => "module",
        b'B' => "binary",
        b'd' => "data",
        b'z' => "pyz",
        _ => "other",
    }
}

/// Renders the cookie version word as `major.minor`.
fn version_label(word: u32) -> String {
    format!("{}.{}", word / 100, word % 100)
}

/// Reads one big-endian word at `offset`.
fn be32(bytes: &[u8], offset: usize) -> Result<u32> {
    let window = bytes.get(offset..offset + 4).ok_or_else(|| {
        ContainerError::parse("the container ends inside a header word").at(offset as u64, offset as u64 + 4)
    })?;
    Ok(u32::from_be_bytes(window.try_into().expect("four-byte window")))
}

/// Writes one extracted entry under `directory`, mirroring its archive name.
fn write_entry(directory: &Path, name: &str, bytes: &[u8]) -> Result<String> {
    let mut target = directory.to_path_buf();
    for part in name.split(['/', '\\']).filter(|part| !part.is_empty() && *part != ".") {
        let mut components = Path::new(part).components();
        if components.next() != Some(Component::Normal(part.as_ref())) || components.next().is_some() {
            return Err(ContainerError::parse(format!("unsafe container entry name: {name}")));
        }
        target.push(part);
    }
    if target == directory {
        return Err(ContainerError::parse(format!("container entry name resolves to the extraction directory: {name}")));
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, bytes)?;
    Ok(target.display().to_string())
}
