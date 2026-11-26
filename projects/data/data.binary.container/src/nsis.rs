//! Decoding of `NSIS` installer containers found in a PE overlay.
//!
//! An `NSIS` installer is a stub executable plus one archive payload. The
//! payload starts with a 28-byte first header (`flags`, the
//! `0xDEADBEEF`/`NullsoftInst` signature, `header_size` and `arc_size`), then a
//! compressed header block that holds the script commands, the string table and
//! the file manifest. This module decodes that structure statically: it reports
//! the container header, the eight internal blocks, the script command stream
//! with resolved strings and the list of files the installer extracts. Payload
//! files can be written to a caller-chosen directory; nothing is executed.
//!
//! [`analyze`] takes the whole container image; offsets in the report are
//! absolute inside that image.

use std::path::{Component, Path, PathBuf};

use data_compression::{CodecLimits, Deflate, Lzma1};
use serde::{Deserialize, Serialize};

use crate::read::window;
use crate::{ContainerError, Result};

/// The `NSIS` first-header signature: `0xDEADBEEF` then `NullsoftInst`.
pub const SIGNATURE: &[u8] = b"\xef\xbe\xad\xdeNullsoftInst";

/// Size of the first header struct (flags, signature, header size, arc size).
const FIRST_HEADER_SIZE: u64 = 28;

/// Default cap on rendered script commands.
const DEFAULT_COMMANDS_LIMIT: usize = 4096;

/// Default cap on rendered strings.
const DEFAULT_STRINGS_LIMIT: usize = 8192;

/// Default cap on one extracted payload file.
const DEFAULT_EXTRACT_LIMIT: usize = 256 * 1024 * 1024;

/// Hard cap on the decompressed header block.
const MAX_HEADER_BLOCK: usize = 64 * 1024 * 1024;

/// Window of solid-stream bytes read to recover the header block.
const SOLID_PROBE: u64 = 16 * 1024 * 1024;

/// Size of one script command (`NSIS` 3): opcode plus six parameters.
const COMMAND_SIZE: usize = 28;

/// Inputs of the `NSIS` decode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NsisOptions {
    /// Absolute offset of the first header; `None` scans the image.
    #[serde(default)]
    pub offset: Option<u64>,
    /// Cap on rendered script commands; clamped to 65536.
    #[serde(default = "default_commands_limit")]
    pub commands_limit: usize,
    /// Cap on rendered strings; clamped to 65536.
    #[serde(default = "default_strings_limit")]
    pub strings_limit: usize,
    /// Optional directory to extract payload files into.
    #[serde(default)]
    pub extract_dir: Option<String>,
    /// Cap on one extracted file in bytes; clamped to 1 GiB.
    #[serde(default = "default_extract_limit")]
    pub extract_limit: usize,
}

impl Default for NsisOptions {
    fn default() -> Self {
        Self {
            offset: None,
            commands_limit: default_commands_limit(),
            strings_limit: default_strings_limit(),
            extract_dir: None,
            extract_limit: default_extract_limit(),
        }
    }
}

/// The 28-byte `NSIS` first header plus the container framing facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NsisHeader {
    /// Absolute offset of the `flags` field (four bytes before the signature).
    pub start_offset: u64,
    /// `flags` field; `NSIS` keeps only the low nibble meaningful.
    pub flags: u32,
    /// Decompressed size of the header block.
    pub header_size: u32,
    /// Size of the data block (uncompressed archive payload).
    pub arc_size: u32,
    /// Absolute offset of the compressed-size field that follows the header.
    pub data_stream_offset: u64,
    /// Raw value at `data_stream_offset`; bit 31 marks a non-solid container.
    pub compressed_header_size: u32,
    /// Whether the archive uses one solid compressed stream.
    pub solid: bool,
    /// Framing of the header block: `none`, `deflate`, `lzma` or `bzip2`.
    pub method: String,
    /// Absolute offset where the file data chunks start (non-solid only).
    pub data_base: Option<u64>,
}

/// One of the eight blocks inside the decompressed header block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NsisBlock {
    /// Stable block name: `pages`, `sections`, `entries`, `strings`,
    /// `langtables`, `ctlcolors`, `font` or `data`.
    pub name: String,
    /// Offset of the block inside the decompressed header block.
    pub offset: u32,
    /// Block size in its own unit (commands, characters, entries).
    pub num: u32,
}

/// One decoded script command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NsisCommand {
    /// Zero-based index inside the entries block.
    pub index: usize,
    /// Offset inside the decompressed header block.
    pub offset: u32,
    /// Raw opcode value.
    pub opcode: u32,
    /// Command name from the `NSIS` 3 table, `CommandN` when unknown.
    pub name: String,
    /// Raw parameter words.
    pub params: Vec<u32>,
    /// Parameters rendered as strings, registers or integers.
    pub rendered: Vec<String>,
}

/// One file the installer extracts (a `File` command).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NsisFile {
    /// Zero-based extraction order.
    pub index: usize,
    /// Index of the owning command in the entries block.
    pub command_index: usize,
    /// Output path as rendered from the string table.
    pub output: String,
    /// Absolute offset of the data chunk header, when the layout allows it.
    pub data_offset: Option<u64>,
    /// Uncompressed size from the chunk header, when readable.
    pub uncompressed_size: Option<u64>,
    /// Compressed size from the chunk header, when readable.
    pub compressed_size: Option<u64>,
    /// Raw `FILETIME` recorded by the command.
    pub modified: Option<u64>,
    /// Destination file written during extraction, when requested.
    pub extracted: Option<String>,
}

/// Full decode of one `NSIS` container.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NsisReport {
    /// Decoded first header and framing.
    pub header: NsisHeader,
    /// The eight header-block blocks.
    pub blocks: Vec<NsisBlock>,
    /// Whether the string table is UTF-16LE.
    pub unicode: bool,
    /// Total characters in the string table.
    pub string_chars: usize,
    /// Strings in table order, capped by `strings_limit`.
    pub strings: Vec<String>,
    /// Total commands in the entries block.
    pub command_count: usize,
    /// Decoded commands in stream order, capped by `commands_limit`.
    pub commands: Vec<NsisCommand>,
    /// Extracted-file manifest in extraction order.
    pub files: Vec<NsisFile>,
    /// Whether any list was capped.
    pub truncated: bool,
}

fn default_commands_limit() -> usize {
    DEFAULT_COMMANDS_LIMIT
}

fn default_strings_limit() -> usize {
    DEFAULT_STRINGS_LIMIT
}

fn default_extract_limit() -> usize {
    DEFAULT_EXTRACT_LIMIT
}

/// Decodes the `NSIS` container of one image.
pub fn analyze(image: &[u8], options: &NsisOptions) -> Result<NsisReport> {
    let start = locate(image, options.offset)?;
    let head = window(image, start, FIRST_HEADER_SIZE)?;

    let flags = word(head, 0);
    let header_size = word(head, 20);
    let arc_size = word(head, 24);
    let data_stream_offset = start + FIRST_HEADER_SIZE;

    if header_size == 0 || header_size as usize > MAX_HEADER_BLOCK {
        return Err(ContainerError::budget(format!("the NSIS header size {header_size} is out of range"))
            .at(data_stream_offset, data_stream_offset + 4));
    }

    let probe = window(image, data_stream_offset, 8)?;
    let framing = word(probe, 0);
    let (method, solid, compressed_header_size) = classify_framing(framing, header_size, probe);

    let block = decompress_header(image, data_stream_offset, header_size, method, solid)?;

    let header = NsisHeader {
        start_offset: start,
        flags,
        header_size,
        arc_size,
        data_stream_offset,
        compressed_header_size,
        solid,
        method: method.to_string(),
        data_base: if solid {
            None
        }
        else {
            data_stream_offset.checked_add(compressed_header_size as u64).and_then(|value| value.checked_add(4))
        },
    };

    let blocks = read_blocks(&block)?;
    let block_of = |name: &str| blocks.iter().find(|entry| entry.name == name);

    let strings_block = block_of("strings").ok_or_else(|| ContainerError::parse("the NSIS header block has no strings block"))?;
    let lang_block = block_of("langtables").ok_or_else(|| ContainerError::parse("the NSIS header block has no langtables block"))?;
    let unicode = u16::from_le_bytes([
        *block.get(strings_block.offset as usize).unwrap_or(&1),
        *block.get(strings_block.offset as usize + 1).unwrap_or(&0),
    ]) == 0;
    let string_chars = ((lang_block.offset - strings_block.offset) as usize) / if unicode { 2 } else { 1 };

    let mut truncated = false;

    let strings_limit = options.strings_limit.clamp(1, 65536);
    let strings = read_strings(&block, strings_block.offset as usize, string_chars, unicode, strings_limit, &mut truncated);

    let entries = block_of("entries").ok_or_else(|| ContainerError::parse("the NSIS header block has no entries block"))?;
    let command_count = (entries.num as usize).min((block.len() - entries.offset as usize) / COMMAND_SIZE);

    let commands_limit = options.commands_limit.clamp(1, 65536);
    let mut commands = Vec::new();
    for index in 0..command_count {
        if commands.len() == commands_limit {
            truncated = true;
            break;
        }
        let offset = entries.offset as usize + index * COMMAND_SIZE;
        let opcode = word(&block, offset);
        let params: Vec<u32> = (1..7).map(|slot| word(&block, offset + slot * 4)).collect();
        let rendered = params
            .iter()
            .map(|&value| render_param(&block, strings_block.offset as usize, string_chars, unicode, value))
            .collect();
        commands.push(NsisCommand {
            index,
            offset: offset as u32,
            opcode,
            name: command_name(opcode).to_string(),
            params,
            rendered,
        });
    }

    let mut files = read_files(&commands, &header, image, &mut truncated);

    if let Some(dir) = &options.extract_dir {
        extract_files(image, &mut files, Path::new(dir), options.extract_limit.clamp(1, 1024 * 1024 * 1024), &header)?;
    }

    Ok(NsisReport { header, blocks, unicode, string_chars, strings, command_count, commands, files, truncated })
}

/// Finds the first header: `offset` when given, otherwise a signature scan.
pub fn locate(image: &[u8], offset: Option<u64>) -> Result<u64> {
    let size = image.len() as u64;
    if let Some(value) = offset {
        let head = window(image, value, FIRST_HEADER_SIZE)?;
        if head[4..20] == *SIGNATURE {
            return Ok(value);
        }
        return Err(ContainerError::parse("the offset does not point at an NSIS first header")
            .at(value + 4, value + 20));
    }

    let window_size = 8 * 1024 * 1024u64;
    let overlap = SIGNATURE.len() as u64 - 1;
    let mut base = 0u64;
    while base < size {
        let length = (window_size + overlap).min(size - base);
        let bytes = window(image, base, length)?;
        if let Some(found) = bytes.windows(SIGNATURE.len()).position(|entry| entry == SIGNATURE) {
            let signature = base + found as u64;
            if signature < 4 {
                return Err(ContainerError::parse("the NSIS signature sits at the start of the file")
                    .at(signature, signature + SIGNATURE.len() as u64));
            }
            return Ok(signature - 4);
        }
        if length == size - base {
            break;
        }
        base += length - overlap;
    }
    Err(ContainerError::parse("no NSIS first header signature was found in the file"))
}

/// Names the framing of the header block from its size field.
fn classify_framing(framing: u32, header_size: u32, probe: &[u8]) -> (&'static str, bool, u32) {
    if framing == header_size {
        return ("none", false, framing);
    }
    let solid = framing & 0x8000_0000 == 0;
    let size = framing & 0x7fff_ffff;
    let stream = &probe[4..];
    let method: &'static str = if is_lzma(stream) || (stream.first().is_some_and(|byte| *byte <= 1) && is_lzma(&stream[1..])) {
        "lzma"
    }
    else if stream.starts_with(b"BZh") {
        "bzip2"
    }
    else {
        "deflate"
    };
    (method, solid, size)
}

/// Whether the bytes start with an `NSIS` `LZMA` properties word.
fn is_lzma(bytes: &[u8]) -> bool {
    bytes.len() >= 7 && bytes[0] == 0x5d && bytes[1] == 0 && bytes[2] == 0 && bytes[5] == 0 && bytes[6] & 0x80 == 0
}

/// Recovers the decompressed header block.
fn decompress_header(
    image: &[u8],
    data_stream_offset: u64,
    header_size: u32,
    method: &str,
    solid: bool,
) -> Result<Vec<u8>> {
    if method != "deflate" && method != "none" && method != "lzma" {
        return Err(ContainerError::unsupported(format!(
            "the NSIS header block is {method}-compressed; only deflate, lzma and none are decoded"
        )));
    }

    if method == "none" {
        return Ok(window(image, data_stream_offset + 4, u64::from(header_size))?.to_vec());
    }

    let size = image.len() as u64;

    if method == "lzma" {
        let stream = if solid {
            let available = size.saturating_sub(data_stream_offset + 4);
            window(image, data_stream_offset + 4, SOLID_PROBE.min(available))?
        }
        else {
            let size_field = word(window(image, data_stream_offset, 4)?, 0);
            window(image, data_stream_offset + 4, (size_field & 0x7fff_ffff) as u64)?
        };
        if stream.len() < 13 {
            return Err(ContainerError::parse("the NSIS LZMA stream is shorter than its properties header")
                .at(data_stream_offset + 4, data_stream_offset + 4 + stream.len() as u64));
        }
        let properties: [u8; 5] = stream[..5].try_into().expect("five-byte window");
        let facts = Lzma1::decode_raw(&properties, &stream[13..], Some(u64::from(header_size)), CodecLimits::new())
            .map_err(|error| ContainerError::parse(format!("the NSIS LZMA header block did not decode: {error}")))?;
        if facts.value.len() != header_size as usize {
            return Err(ContainerError::parse(format!(
                "the NSIS LZMA header block decoded to {} bytes, not the announced {header_size}",
                facts.value.len()
            )));
        }
        return Ok(facts.value);
    }

    if solid {
        let available = size.saturating_sub(data_stream_offset + 4);
        let stream = window(image, data_stream_offset + 4, SOLID_PROBE.min(available))?;
        let decoded = Deflate::decode_bounded(stream, CodecLimits::new().with_max_output_bytes(u64::from(header_size)));
        if decoded.is_complete() {
            return Ok(decoded.output);
        }
        if decoded.output.len() >= header_size as usize {
            return Ok(decoded.output[..header_size as usize].to_vec());
        }
        return Err(ContainerError::parse(format!("the solid NSIS stream did not decode cleanly ({})", decoded.status.code())));
    }

    let size_field = word(window(image, data_stream_offset, 4)?, 0);
    let compressed = (size_field & 0x7fff_ffff) as u64;
    let stream = window(image, data_stream_offset + 4, compressed)?;
    let decoded = Deflate::decode_bounded(stream, CodecLimits::new().with_max_output_bytes(header_size as u64 + 1));
    if decoded.is_complete() && decoded.output.len() < header_size as usize {
        return Err(ContainerError::parse(format!(
            "the NSIS header block decoded to {} bytes, not the announced {header_size}",
            decoded.output.len()
        )));
    }
    if decoded.output.len() >= header_size as usize {
        return Ok(decoded.output[..header_size as usize].to_vec());
    }
    Err(ContainerError::parse(format!("the NSIS header block did not decode cleanly ({})", decoded.status.code())))
}

/// Reads the eight block headers of the decompressed header block.
fn read_blocks(block: &[u8]) -> Result<Vec<NsisBlock>> {
    const NAMES: [&str; 8] = ["pages", "sections", "entries", "strings", "langtables", "ctlcolors", "font", "data"];
    if block.len() < 4 + NAMES.len() * 8 {
        return Err(ContainerError::parse("the NSIS header block is too small to hold its block table"));
    }
    Ok(NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| NsisBlock {
            name: (*name).to_string(),
            offset: word(block, 4 + index * 8),
            num: word(block, 4 + index * 8 + 4),
        })
        .collect())
}

/// Walks the string table and renders every string.
fn read_strings(
    block: &[u8],
    strings_offset: usize,
    chars: usize,
    unicode: bool,
    limit: usize,
    truncated: &mut bool,
) -> Vec<String> {
    let mut strings = Vec::new();
    let mut position = 0usize;
    while position < chars {
        let (text, next) = read_string_at(block, strings_offset, position, chars, unicode);
        position = next + 1;
        if text.is_empty() {
            continue;
        }
        if strings.len() == limit {
            *truncated = true;
            break;
        }
        strings.push(text);
    }
    strings
}

/// Reads the string starting at unit `position`; returns it and the terminator index.
fn read_string_at(block: &[u8], strings_offset: usize, position: usize, chars: usize, unicode: bool) -> (String, usize) {
    let mut units = Vec::new();
    let mut index = position;
    while index < chars {
        let byte_index = strings_offset + index * if unicode { 2 } else { 1 };
        if unicode {
            let Some(pair) = block.get(byte_index..byte_index + 2)
            else {
                break;
            };
            let value = u16::from_le_bytes([pair[0], pair[1]]);
            if value == 0 {
                break;
            }
            units.push(value);
        }
        else {
            let Some(&value) = block.get(byte_index)
            else {
                break;
            };
            if value == 0 {
                break;
            }
            units.push(value as u16);
        }
        index += 1;
    }
    (decode_nsis_string(&units), index)
}

/// Renders `NSIS` string units, expanding the in-string escape codes.
fn decode_nsis_string(units: &[u16]) -> String {
    let mut text = String::new();
    let mut index = 0usize;
    while index < units.len() {
        let unit = units[index];
        index += 1;
        match unit {
            0x01 => {
                let next = units.get(index).copied().unwrap_or(0);
                index += 1;
                text.push_str(&format!("%LANG#{next}"));
            }
            0x02 => {
                let next = units.get(index).copied().unwrap_or(0);
                index += 1;
                text.push_str(&format!("%SHELL#{next}"));
            }
            0x03 => {
                let next = units.get(index).copied().unwrap_or(0);
                index += 1;
                if next & 0x80 != 0 {
                    text.push_str(var_name(next & 0x7f));
                }
                else {
                    text.push_str(&format!("${next}"));
                }
            }
            0x04 => {
                let next = units.get(index).copied().unwrap_or(0);
                index += 1;
                text.push(char::from_u32(next as u32).unwrap_or('?'));
            }
            _ => {
                if unit < 0x20 {
                    text.push_str(&format!("\\x{unit:02x}"));
                }
                else {
                    text.push(char::from_u32(unit as u32).unwrap_or('?'));
                }
            }
        }
    }
    text
}

/// Names of the `NSIS` built-in variables used in strings and parameters.
fn var_name(index: u16) -> &'static str {
    match index {
        0..=9 => const_var(index as usize),
        10..=19 => const_rvar(index as usize),
        20 => "$CMDLINE",
        21 => "$INSTDIR",
        22 => "$OUTDIR",
        23 => "$EXEDIR",
        24 => "$LANGUAGE",
        25 => "$TEMP",
        26 => "$PLUGINSDIR",
        27 => "$EXEPATH",
        28 => "$EXEFILE",
        29 => "$HWNDPARENT",
        30 => "$_CLICK",
        31 => "$_OUTDIR",
        _ => "$UNKNOWN",
    }
}

fn const_var(index: usize) -> &'static str {
    ["$0", "$1", "$2", "$3", "$4", "$5", "$6", "$7", "$8", "$9"][index]
}

fn const_rvar(index: usize) -> &'static str {
    ["$R0", "$R1", "$R2", "$R3", "$R4", "$R5", "$R6", "$R7", "$R8", "$R9"][index - 10]
}

/// Renders one command parameter as a string, register or integer.
fn render_param(block: &[u8], strings_offset: usize, string_chars: usize, unicode: bool, value: u32) -> String {
    if value == 0 {
        return String::new();
    }
    if (value as i32) < 0 {
        return format!("%LANG#{}", (value as i32).unsigned_abs() - 1);
    }
    if (value as usize) < string_chars {
        let (text, _) = read_string_at(block, strings_offset, value as usize, string_chars, unicode);
        if !text.is_empty() {
            return text;
        }
    }
    format!("#{value}")
}

/// Names one `NSIS` 3 opcode.
fn command_name(opcode: u32) -> &'static str {
    const NAMES: [&str; 74] = [
        "Invalid",
        "Return",
        "Goto",
        "Abort",
        "Quit",
        "Call",
        "DetailPrint",
        "Sleep",
        "BringToFront",
        "SetDetailsView",
        "SetFileAttributes",
        "SetOutPath",
        "IfFileExists",
        "SetFlag",
        "IfFlag",
        "GetFlag",
        "Rename",
        "GetFullPathName",
        "SearchPath",
        "GetTempFileName",
        "File",
        "Delete",
        "MessageBox",
        "RMDir",
        "StrLen",
        "StrCpy",
        "StrCmp",
        "ReadEnvStr",
        "IntCmp",
        "IntOp",
        "IntFmt",
        "PushPop",
        "FindWindow",
        "SendMessage",
        "IsWindow",
        "GetDlgItem",
        "SetCtlColors",
        "SetBrandingImage",
        "CreateFont",
        "ShowWindow",
        "ExecShell",
        "Exec",
        "GetFileTime",
        "GetDLLVersion",
        "RegisterDLL",
        "CreateShortCut",
        "CopyFiles",
        "Reboot",
        "WriteINIStr",
        "ReadINIStr",
        "DeleteReg",
        "WriteReg",
        "ReadReg",
        "EnumReg",
        "FileClose",
        "FileOpen",
        "FileWrite",
        "FileRead",
        "FileSeek",
        "FindClose",
        "FindNext",
        "FindFirst",
        "WriteUninstaller",
        "Section",
        "InstTypeSet",
        "GetLabelAddr",
        "GetFunctionAddress",
        "LockWindow",
        "FileWriteWord",
        "FileReadWord",
        "Log",
        "FindProc",
        "GetFontVersion",
        "GetFontName",
    ];
    NAMES.get(opcode as usize).copied().unwrap_or("Unknown")
}

/// Collects the `File` commands into the extraction manifest.
fn read_files(commands: &[NsisCommand], header: &NsisHeader, image: &[u8], truncated: &mut bool) -> Vec<NsisFile> {
    let size = image.len() as u64;
    let mut files = Vec::new();
    for command in commands {
        if command.opcode != 20 {
            continue;
        }
        if files.len() == 65536 {
            *truncated = true;
            break;
        }
        let output = command.rendered.get(1).cloned().unwrap_or_default();
        let modified = Some(((command.params[4] as u64) << 32) | command.params[3] as u64).filter(|value| *value != 0);
        let mut entry = NsisFile {
            index: files.len(),
            command_index: command.index,
            output,
            data_offset: None,
            uncompressed_size: None,
            compressed_size: None,
            modified,
            extracted: None,
        };
        if let (Some(base), Some(position)) =
            (header.data_base, command.params.get(2).copied().filter(|value| *value != u32::MAX))
        {
            let offset = base + u64::from(position);
            if let Ok(chunk) = window(image, offset, 4) {
                let packed = word(chunk, 0);
                let size_field = u64::from(packed & 0x7fff_ffff);
                if offset + 4 + size_field <= size {
                    entry.data_offset = Some(offset);
                    if packed & 0x8000_0000 != 0 {
                        entry.compressed_size = Some(size_field);
                    }
                    else {
                        entry.uncompressed_size = Some(size_field);
                    }
                }
            }
        }
        files.push(entry);
    }
    files
}

/// Writes every manifest entry with a readable chunk into `directory`.
///
/// A non-solid chunk is a single little-endian word at the absolute `File`
/// position: the top bit marks DEFLATE data, the low 31 bits are the stored or
/// compressed length, and the payload follows four bytes later.
fn extract_files(
    image: &[u8],
    files: &mut [NsisFile],
    directory: &Path,
    limit: usize,
    header: &NsisHeader,
) -> Result<usize> {
    if header.solid {
        return Err(ContainerError::unsupported("extraction of solid NSIS archives is not implemented"));
    }
    if header.method != "deflate" {
        return Err(ContainerError::unsupported(format!(
            "extraction of NSIS archives using {} is not implemented",
            header.method
        )));
    }
    let mut written = 0usize;
    for file in files {
        let Some(offset) = file.data_offset
        else {
            continue;
        };
        let size = file.compressed_size.or(file.uncompressed_size).unwrap_or(0);
        if size == 0 || size > limit as u64 {
            continue;
        }
        let stream = window(image, offset + 4, size)?;
        let output = if file.compressed_size.is_some() {
            let decoded = Deflate::decode_bounded(stream, CodecLimits::new().with_max_output_bytes(limit as u64));
            if !decoded.is_complete() {
                return Err(ContainerError::parse(format!(
                    "the NSIS file {} did not decode cleanly ({})",
                    file.index,
                    decoded.status.code()
                ))
                .at(offset + 4, offset + 4 + size));
            }
            decoded.output
        }
        else {
            stream.to_vec()
        };
        let path = safe_path(directory, &file.output)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, &output)?;
        file.uncompressed_size = Some(output.len() as u64);
        file.extracted = Some(path.display().to_string());
        written += 1;
    }
    Ok(written)
}

/// Maps an installer output path onto a safe local path.
fn safe_path(directory: &Path, output: &str) -> Result<PathBuf> {
    let normalized = output.replace('\\', "/");
    let mut path = directory.to_path_buf();
    for component in Path::new(&normalized).components() {
        match component {
            Component::Normal(segment) => path.push(segment),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) | Component::CurDir => {}
        }
    }
    if path == directory {
        return Err(ContainerError::parse("the NSIS file path has no usable component"));
    }
    Ok(path)
}

/// Little-endian word at `index`.
fn word(bytes: &[u8], index: usize) -> u32 {
    bytes.get(index..index + 4).map(|entry| u32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]])).unwrap_or(0)
}
