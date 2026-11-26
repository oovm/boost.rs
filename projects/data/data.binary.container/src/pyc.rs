//! Static decoding of CPython `marshal` streams and 3.11 through 3.13 bytecode.
//!
//! The input is either a raw `marshal` dump (the object graph, optionally
//! prefixed by the version byte `2` through `6` that `PyMarshal_Write`
//! writes) or a `.pyc` file whose first sixteen bytes are the container
//! header (magic,
//! flags, timestamp or hash, source size). The code-object field order
//! decoded here is the 3.11 one: `argcount`, `posonlyargcount`,
//! `kwonlyargcount`, `stacksize`, `flags`, `code`, `consts`, `names`,
//! `localsplusnames`, `localspluskinds`, `filename`, `name`, `qualname`,
//! `firstlineno`, `linetable`, `exceptiontable`. The `localsplus` pair
//! (CPython 3.11 through 3.13) replaced the older `varnames`, `freevars`
//! and `cellvars` tuples: the kind byte marks each name `0x20` local,
//! `0x40` cell or `0x80` free, and the report splits it back into the
//! three familiar lists.
//!
//! Every `code` object in the graph is flattened into the report in parse
//! order. Its bytecode is walked as word-code pairs: the opcode byte names
//! the instruction through the table of the selected interpreter version,
//! the argument byte resolves against the names, constants and locals
//! tables, and inline cache slots (opcode byte `0`, the only place that
//! value appears in non-adaptive code) are counted against the preceding
//! instruction instead of being emitted. Nothing is executed.
//!
//! [`analyze`] takes the whole container image; every offset in the report is
//! absolute inside that image.

use serde::{Deserialize, Serialize};

use crate::read::window;
use crate::{ContainerError, Result};

mod marshal;
mod render;

use marshal::{Parser, Value};

/// `co_localspluskinds` bit: the name is a fast local.
const LOCAL_KIND_LOCAL: u8 = 0x20;

/// `co_localspluskinds` bit: the name is a cell.
const LOCAL_KIND_CELL: u8 = 0x40;

/// `co_localspluskinds` bit: the name is a free variable.
const LOCAL_KIND_FREE: u8 = 0x80;

/// Size of the `.pyc` container header that precedes the stream.
const PYC_HEADER_SIZE: usize = 16;

/// Default cap on decoded code objects.
const DEFAULT_CODE_LIMIT: usize = 1024;

/// Default cap on emitted instructions across the whole graph.
const DEFAULT_INSTRUCTION_LIMIT: usize = 65_536;

/// Cap on rendered names, locals and consts per code object.
const RENDER_LIST_LIMIT: usize = 4096;

/// Hard cap on one decoded string, bytes object or the whole stream.
const MAX_BLOB_SIZE: usize = 64 * 1024 * 1024;

/// Interpreter whose opcode table decodes a raw `marshal` stream when the
/// input carries no magic and the caller pinned no version.
const DEFAULT_PYTHON: u16 = 312;

/// Inputs of one [`analyze`] call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PycOptions {
    /// Absolute offset of the stream start; defaults to the image start.
    #[serde(default)]
    pub offset: Option<u64>,
    /// Stream framing; `auto` picks `marshal` on the version byte, else `pyc`.
    #[serde(default)]
    pub format: PycFormat,
    /// Keep only code objects whose `qualname` or `name` contains this text.
    #[serde(default)]
    pub code: Option<String>,
    /// Opcode table for a raw `marshal` stream: `311`, `312` or `313`. A
    /// `.pyc` container always selects the table named by its magic.
    #[serde(default)]
    pub python: Option<u16>,
    /// Cap on decoded code objects; clamped to 16384.
    #[serde(default = "default_code_limit")]
    pub code_limit: usize,
    /// Cap on emitted instructions; clamped to 1048576.
    #[serde(default = "default_instruction_limit")]
    pub instruction_limit: usize,
}

impl Default for PycOptions {
    fn default() -> Self {
        Self {
            offset: None,
            format: PycFormat::default(),
            code: None,
            python: None,
            code_limit: default_code_limit(),
            instruction_limit: default_instruction_limit(),
        }
    }
}

/// Framing of the input bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PycFormat {
    /// `marshal` when the first byte is a code type byte or the version
    /// four byte, else `pyc`.
    #[default]
    Auto,
    /// Raw `marshal.dumps` output starting at the offset.
    Marshal,
    /// `.pyc` container: skip the sixteen-byte header at the offset.
    Pyc,
}

/// One decoded constant, rendered for the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PycConst {
    /// Discriminator name: `int`, `str`, `bytes`, `tuple`, `code`, ...
    pub kind: String,
    /// Compact Python-style rendering, truncated when long.
    pub repr: String,
    /// Index into [`PycReport::codes`] when this constant is a code object.
    pub code_index: Option<usize>,
}

/// One decoded instruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PycInstruction {
    /// Byte offset of the instruction inside `co_code`.
    pub offset: u32,
    /// Raw opcode byte.
    pub opcode: u8,
    /// Opcode name, or `<opcode N>` when unknown.
    pub name: String,
    /// Combined argument, `None` for opcodes that carry no argument byte.
    pub arg: Option<u32>,
    /// Resolved argument text: name, constant repr, local, jump target, ...
    pub resolved: Option<String>,
    /// Inline cache slots that follow this instruction.
    pub caches: u32,
}

/// One decoded code object with its disassembly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PycCodeObject {
    /// Position in the flattened report list.
    pub index: usize,
    /// Dotted qualified name.
    pub qualname: String,
    /// Simple name.
    pub name: String,
    /// Source file recorded in the code object.
    pub filename: String,
    /// `co_argcount`.
    pub argcount: i32,
    /// `co_posonlyargcount`.
    pub posonlyargcount: i32,
    /// `co_kwonlyargcount`.
    pub kwonlyargcount: i32,
    /// `co_stacksize`.
    pub stacksize: i32,
    /// `co_flags` as a raw word.
    pub flags: u32,
    /// `co_flags` rendered as `OPTIMIZED, NEWLOCALS, ...`.
    pub flags_text: String,
    /// `co_firstlineno`.
    pub firstlineno: i32,
    /// Byte length of `co_code`.
    pub code_len: u32,
    /// Byte length of `co_linetable`.
    pub linetable_len: u32,
    /// Byte length of `co_exceptiontable`.
    pub exceptiontable_len: u32,
    /// `co_names`, cut at the render limit.
    pub names: Vec<String>,
    /// `co_varnames`, cut at the render limit.
    pub varnames: Vec<String>,
    /// `co_freevars`, cut at the render limit.
    pub freevars: Vec<String>,
    /// `co_cellvars`, cut at the render limit.
    pub cellvars: Vec<String>,
    /// `co_consts` summaries, cut at the render limit.
    pub consts: Vec<PycConst>,
    /// Disassembly, cut when the instruction budget ran out.
    pub instructions: Vec<PycInstruction>,
    /// Whether any list of this code object was cut.
    pub truncated: bool,
}

/// Full decode of one `marshal` stream or `.pyc` container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PycReport {
    /// Absolute offset of the stream start.
    pub start_offset: u64,
    /// Framing actually used: `marshal` or `pyc`.
    pub source_format: String,
    /// `marshal` version byte when the stream carried one.
    pub marshal_version: Option<u8>,
    /// `.pyc` magic word when the framing was `pyc`.
    pub pyc_magic: Option<u32>,
    /// `.pyc` flags word when the framing was `pyc`.
    pub pyc_flags: Option<u32>,
    /// Interpreter named by the magic, e.g. `CPython 3.11`.
    pub python: Option<String>,
    /// Opcode table the bytecode was decoded with, e.g. `3.12`; the table a
    /// raw `marshal` stream assumed when no magic named the interpreter.
    pub opcode_table: String,
    /// Code objects decoded before the limit and the filter.
    pub code_count: usize,
    /// Instructions emitted across every decoded code object.
    pub instruction_count: usize,
    /// Code objects in parse order, after the `--code` filter.
    pub codes: Vec<PycCodeObject>,
    /// Whether the graph or an instruction budget was cut.
    pub truncated: bool,
}

fn default_code_limit() -> usize {
    DEFAULT_CODE_LIMIT
}

fn default_instruction_limit() -> usize {
    DEFAULT_INSTRUCTION_LIMIT
}

/// Whether a window of bytes looks like bytecode rather than machine code.
///
/// The unified disassembly route uses this to pick the bytecode path when
/// `--format auto` meets a `marshal` stream (a version prefix byte, the
/// referenced code type `0xE3` or the bare code type `c`) or a `.pyc`
/// container (a two-byte magic in the assigned range followed by `CR LF`).
/// Classified containers stay on the machine path; an explicit `--format`
/// always overrides this guess.
pub fn sniff(bytes: &[u8]) -> bool {
    if matches!(bytes.first(), Some(2..=6) | Some(&0xE3) | Some(&b'c')) {
        return true;
    }
    if bytes.len() >= 4 && bytes[2] == 0x0D && bytes[3] == 0x0A {
        let magic = u16::from_le_bytes([bytes[0], bytes[1]]);
        return (2000..=4000).contains(&magic);
    }
    false
}

/// Decodes one `marshal` stream whose root is a `PYZ` module dictionary:
/// dotted names mapped to `(is_package, offset, length)` triples. Used by
/// the PyInstaller reader; anything else is a parse error.
pub(crate) fn marshal_module_toc(bytes: &[u8]) -> Result<Vec<(String, bool, u64, u64)>> {
    let table = crate::pyc_opcodes::table_for(DEFAULT_PYTHON).expect("the default opcode table exists");
    let mut parser = Parser::new(bytes, 1, 0, DEFAULT_PYTHON, table);
    let root = parser.stream()?;
    let Value::Dict(pairs) = root
    else {
        return Err(ContainerError::parse("the PYZ table of contents is not a marshal dictionary"));
    };
    let mut out = Vec::with_capacity(pairs.len());
    for (key, value) in pairs {
        let (Value::Str(name), Value::Tuple(fields)) = (key, value)
        else {
            return Err(ContainerError::parse("the PYZ table of contents holds a name that does not map to a triple"));
        };
        if fields.len() != 3 {
            return Err(ContainerError::parse(format!("the PYZ entry {name} does not have three fields")));
        }
        let number = |value: &Value| match value {
            Value::Int(word) => u64::try_from(*word).ok(),
            Value::Bool(flag) => Some(u64::from(*flag)),
            _ => None,
        };
        let Some(offset) = number(&fields[1])
        else {
            return Err(ContainerError::parse(format!("the PYZ entry {name} has a non-numeric offset")));
        };
        let Some(length) = number(&fields[2])
        else {
            return Err(ContainerError::parse(format!("the PYZ entry {name} has a non-numeric length")));
        };
        out.push((name.clone(), number(&fields[0]).unwrap_or(0) != 0, offset, length));
    }
    Ok(out)
}

/// Decodes the `marshal` stream or `.pyc` container of one image.
pub fn analyze(image: &[u8], options: &PycOptions) -> Result<PycReport> {
    let start = options.offset.unwrap_or(0);
    let size = image.len() as u64;
    let remaining = size.checked_sub(start).ok_or_else(|| {
        ContainerError::parse(format!("offset {start} is past the end of the file")).at(start, size)
    })?;
    if remaining > MAX_BLOB_SIZE as u64 {
        return Err(ContainerError::budget(format!(
            "the marshal stream at {start} is {remaining} bytes, over the {MAX_BLOB_SIZE} byte cap"
        ))
        .at(start, size));
    }
    let data = window(image, start, remaining)?;

    let mut stream = 0usize;
    let mut pyc_magic = None;
    let mut pyc_flags = None;
    let format = match options.format {
        PycFormat::Auto if matches!(data.first(), Some(2..=6) | Some(&0xE3) | Some(&b'c')) => PycFormat::Marshal,
        PycFormat::Auto => PycFormat::Pyc,
        explicit => explicit,
    };
    if format == PycFormat::Pyc {
        if data.len() < PYC_HEADER_SIZE {
            return Err(ContainerError::parse("the pyc container header is truncated").at(start, size));
        }
        pyc_magic = Some(u32::from_le_bytes(data[0..4].try_into().expect("four-byte window")));
        pyc_flags = Some(u32::from_le_bytes(data[4..8].try_into().expect("four-byte window")));
        stream = PYC_HEADER_SIZE;
    }

    // The magic names the interpreter of a container; a raw stream carries no
    // version, so the pinned `python` option or the default picks the table.
    let version = match pyc_magic {
        Some(magic) => magic_version(magic).ok_or_else(|| {
            ContainerError::parse(format!(
                "unknown pyc magic {magic:#x}; the decoder reads CPython {}",
                crate::pyc_opcodes::SUPPORTED_VERSIONS
                    .iter()
                    .map(|version| version_label(*version))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
            .at(start, start + 4)
        })?,
        None => options.python.unwrap_or(DEFAULT_PYTHON),
    };
    let table = crate::pyc_opcodes::table_for(version).ok_or_else(|| {
        ContainerError::parse(format!(
            "no opcode table for CPython {}; the decoder reads {}",
            version_label(version),
            crate::pyc_opcodes::SUPPORTED_VERSIONS
                .iter()
                .map(|version| version_label(*version))
                .collect::<Vec<_>>()
                .join(", ")
        ))
        .at(start + stream as u64, size)
    })?;

    let code_limit = options.code_limit.clamp(1, 16_384);
    let instruction_limit = options.instruction_limit.clamp(1, 1_048_576);
    let mut parser = Parser::new(&data[stream..], code_limit, instruction_limit, version, table);
    if !matches!(parser.stream()?, Value::Code(_)) {
        return Err(ContainerError::parse("the marshal stream does not start with a code object")
            .at(start + stream as u64, size));
    }

    let instruction_count = parser.codes.iter().map(|code| code.instructions.len()).sum();
    let truncated = parser.truncated || parser.codes.iter().any(|code| code.truncated);
    let python = pyc_magic.and_then(python_name);
    let mut codes = parser.codes;
    if let Some(needle) = &options.code {
        codes.retain(|code| code.qualname.contains(needle) || code.name.contains(needle));
        if codes.is_empty() {
            return Err(ContainerError::parse(format!("no code object matches {needle}")));
        }
    }

    Ok(PycReport {
        start_offset: start,
        source_format: match format {
            PycFormat::Marshal => "marshal".to_string(),
            _ => "pyc".to_string(),
        },
        marshal_version: parser.version,
        pyc_magic,
        pyc_flags,
        python,
        opcode_table: version_label(version),
        code_count: parser.decoded_codes,
        instruction_count,
        codes,
        truncated,
    })
}

/// Names the interpreter version from a `.pyc` magic number.
pub(crate) fn magic_version(magic: u32) -> Option<u16> {
    Some(match magic & 0xffff {
        3413 => 308,
        3425 => 309,
        3439 => 310,
        3495 => 311,
        3531 => 312,
        3571 => 313,
        _ => return None,
    })
}

/// Formats a `311`-style version as `3.11`.
fn version_label(version: u16) -> String {
    format!("{}.{}", version / 100, version % 100)
}

/// Names the interpreter from a `.pyc` magic number.
fn python_name(magic: u32) -> Option<String> {
    magic_version(magic).map(|version| format!("CPython {}", version_label(version)))
}

/// Keeps the first `RENDER_LIST_LIMIT` items and flags the cut.
fn render_list(items: Vec<String>, truncated: &mut bool) -> Vec<String> {
    if items.len() > RENDER_LIST_LIMIT {
        *truncated = true;
        items.into_iter().take(RENDER_LIST_LIMIT).collect()
    }
    else {
        items
    }
}

/// Splits the 3.11+ `localsplus` pair into the three classic name lists.
/// A name can carry both the local and the cell bit, so it appears in
/// both lists, exactly like the `co_*` accessors expose it.
fn split_localsplus(names: &[String], kinds: &[u8]) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut varnames = Vec::new();
    let mut freevars = Vec::new();
    let mut cellvars = Vec::new();
    for (index, text) in names.iter().enumerate() {
        let kind = kinds.get(index).copied().unwrap_or(LOCAL_KIND_LOCAL);
        if kind & LOCAL_KIND_LOCAL != 0 {
            varnames.push(text.clone());
        }
        if kind & LOCAL_KIND_CELL != 0 {
            cellvars.push(text.clone());
        }
        if kind & LOCAL_KIND_FREE != 0 {
            freevars.push(text.clone());
        }
    }
    (varnames, freevars, cellvars)
}

/// Renders `co_flags` as comma-separated `CO_*` names.
fn flags_text(flags: u32) -> String {
    const NAMES: [(u32, &str); 10] = [
        (1, "OPTIMIZED"),
        (2, "NEWLOCALS"),
        (4, "VARARGS"),
        (8, "VARKEYWORDS"),
        (16, "NESTED"),
        (32, "GENERATOR"),
        (64, "NOFREE"),
        (128, "COROUTINE"),
        (256, "ITERABLE_COROUTINE"),
        (512, "ASYNC_GENERATOR"),
    ];
    let mut parts: Vec<String> = Vec::new();
    let mut rest = flags;
    for (bit, name) in NAMES {
        if rest & bit != 0 {
            parts.push((*name).to_string());
            rest &= !bit;
        }
    }
    if rest != 0 {
        parts.push(format!("<{rest:#x}>"));
    }
    parts.join(", ")
}
