//! `dyld` 绑定操作码解释。
//!
//! 读取 `LC_DYLD_INFO` 的 `bind`、`weak_bind` 与 `lazy_bind` 三条数据流，
//! 按 `dyld` 绑定操作码状态机还原每个导入符号写入的槽位地址。重定位
//! 与导出 trie 不在本模块范围内。

use super::read::{MachOImage, MachOParseError};

const BIND_OPCODE_MASK: u8 = 0xF0;
const BIND_IMMEDIATE_MASK: u8 = 0x0F;
const BIND_SYMBOL_FLAGS_WEAK_IMPORT: u8 = 0x01;

const BIND_OPCODE_DONE: u8 = 0x00;
const BIND_OPCODE_SET_DYLIB_ORDINAL_IMM: u8 = 0x10;
const BIND_OPCODE_SET_DYLIB_ORDINAL_ULEB: u8 = 0x20;
const BIND_OPCODE_SET_DYLIB_SPECIAL_IMM: u8 = 0x30;
const BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM: u8 = 0x40;
const BIND_OPCODE_SET_TYPE_IMM: u8 = 0x50;
const BIND_OPCODE_SET_ADDEND_SLEB: u8 = 0x60;
const BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB: u8 = 0x70;
const BIND_OPCODE_ADD_ADDR_ULEB: u8 = 0x80;
const BIND_OPCODE_DO_BIND: u8 = 0x90;
const BIND_OPCODE_DO_BIND_ADD_ADDR_ULEB: u8 = 0xA0;
const BIND_OPCODE_DO_BIND_ADD_ADDR_IMM_SCALED: u8 = 0xB0;
const BIND_OPCODE_DO_BIND_ULEB_TIMES_SKIPPING_ULEB: u8 = 0xC0;

/// 指针宽度，绑定槽位按该宽度推进。
const POINTER_SIZE: u64 = 8;

const BIND_TYPE_POINTER: u8 = 1;
const BIND_TYPE_TEXT_ABSOLUTE32: u8 = 2;
const BIND_TYPE_TEXT_PCREL32: u8 = 3;

/// `BIND_OPCODE_SET_TYPE_IMM` 设置的绑定类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachBindType {
    /// `BIND_TYPE_POINTER`。
    Pointer,
    /// `BIND_TYPE_TEXT_ABSOLUTE32`。
    TextAbsolute32,
    /// `BIND_TYPE_TEXT_PCREL32`。
    TextPcrel32,
    /// 其他或尚未设置的值。
    Other(u8),
}

impl MachBindType {
    /// 从原始 `SET_TYPE_IMM` 值映射。
    pub fn from_value(value: u8) -> Self {
        match value {
            BIND_TYPE_POINTER => Self::Pointer,
            BIND_TYPE_TEXT_ABSOLUTE32 => Self::TextAbsolute32,
            BIND_TYPE_TEXT_PCREL32 => Self::TextPcrel32,
            other => Self::Other(other),
        }
    }

    /// 原始值。
    pub fn value(self) -> u8 {
        match self {
            Self::Pointer => BIND_TYPE_POINTER,
            Self::TextAbsolute32 => BIND_TYPE_TEXT_ABSOLUTE32,
            Self::TextPcrel32 => BIND_TYPE_TEXT_PCREL32,
            Self::Other(other) => other,
        }
    }

    /// 稳定名称，例如 `pointer`；未知值渲染为 `0x<hex>`。
    pub fn name(self) -> String {
        match self {
            Self::Pointer => "pointer".to_string(),
            Self::TextAbsolute32 => "text-abs32".to_string(),
            Self::TextPcrel32 => "text-pcrel32".to_string(),
            Self::Other(other) => format!("0x{other:02x}"),
        }
    }
}

/// 一条 `dyld` 绑定记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachBind {
    /// 导入符号名。
    pub symbol: String,
    /// 动态库序号；`0` 表示主镜像，负数表示 `dyld` 特殊查找。
    pub library_ordinal: i32,
    /// 序号对应的动态库名；特殊序号为 `None`。
    pub library: Option<String>,
    /// 目标段序号。
    pub segment_index: u8,
    /// 目标段名。
    pub segment: String,
    /// 槽位的虚拟地址。
    pub address: u64,
    /// 符号加数。
    pub addend: i64,
    /// 绑定类型。
    pub bind_type: MachBindType,
    /// 是否为弱导入。
    pub weak: bool,
    /// 是否来自 `lazy_bind` 流。
    pub lazy: bool,
    /// 数据流名称：`bind`、`weak_bind` 或 `lazy_bind`。
    pub stream: &'static str,
}

impl MachBind {
    /// 报告使用的稳定流名：`bind`、`weak` 或 `lazy`。
    pub fn stream_name(&self) -> &'static str {
        match self.stream {
            "weak_bind" => "weak",
            "lazy_bind" => "lazy",
            other => other,
        }
    }

    /// 动态库名；正序号解析为安装名，特殊序号渲染为 `self` 等文本。
    pub fn dylib_name(&self) -> String {
        if let Some(library) = &self.library {
            return library.clone();
        }
        match self.library_ordinal {
            0 => "self",
            -1 => "main-executable",
            -2 => "flat-lookup",
            -3 => "weak-lookup",
            _ => "special",
        }
        .to_string()
    }
}

/// 解释全部绑定数据流。
///
/// 没有 `LC_DYLD_INFO` 时返回空列表；任一数据流越界或操作码非法时返回错误。
pub fn read_bindings(bytes: &[u8], image: &MachOImage) -> Result<Vec<MachBind>, MachOParseError> {
    let Some(info) = image.dyld_info
    else {
        return Ok(Vec::new());
    };
    let mut bindings = Vec::new();
    let streams = [
        ("bind", info.bind_offset, info.bind_size, false, false),
        ("weak_bind", info.weak_bind_offset, info.weak_bind_size, false, true),
        ("lazy_bind", info.lazy_bind_offset, info.lazy_bind_size, true, false),
    ];
    for (name, offset, size, lazy, weak) in streams {
        if size == 0 {
            continue;
        }
        let start = u64::from(offset);
        let end = start + u64::from(size);
        if end > bytes.len() as u64 {
            return Err(MachOParseError::OutOfRange { offset: start, length: u64::from(size) });
        }
        run_stream(bytes, image, &mut bindings, start, end, lazy, weak, name)?;
    }
    Ok(bindings)
}

/// 解析时累积的绑定状态。
#[derive(Default)]
struct State {
    symbol: String,
    flags: u8,
    ordinal: i32,
    addend: i64,
    bind_type: u8,
    segment: Option<u8>,
    address: u64,
}

/// 解释单条数据流。
fn run_stream(
    bytes: &[u8],
    image: &MachOImage,
    bindings: &mut Vec<MachBind>,
    start: u64,
    end: u64,
    lazy: bool,
    weak_stream: bool,
    stream: &'static str,
) -> Result<(), MachOParseError> {
    let mut cursor = start as usize;
    let end = end as usize;
    let mut state = State::default();
    while cursor < end {
        let opcode_offset = cursor as u64;
        let opcode = bytes[cursor];
        cursor += 1;
        let immediate = opcode & BIND_IMMEDIATE_MASK;
        match opcode & BIND_OPCODE_MASK {
            BIND_OPCODE_DONE => {
                if !lazy {
                    break;
                }
                state.symbol.clear();
                state.addend = 0;
            }
            BIND_OPCODE_SET_DYLIB_ORDINAL_IMM => state.ordinal = i32::from(immediate),
            BIND_OPCODE_SET_DYLIB_ORDINAL_ULEB => state.ordinal = read_uleb(bytes, &mut cursor, end)? as i32,
            BIND_OPCODE_SET_DYLIB_SPECIAL_IMM => {
                state.ordinal = if immediate == 0 { 0 } else { i32::from((immediate | 0xF0) as i8) };
            }
            BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM => {
                state.flags = immediate;
                state.symbol = read_symbol(bytes, &mut cursor, end);
            }
            BIND_OPCODE_SET_TYPE_IMM => state.bind_type = immediate,
            BIND_OPCODE_SET_ADDEND_SLEB => state.addend = read_sleb(bytes, &mut cursor, end)?,
            BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB => {
                state.segment = Some(immediate);
                let index = usize::from(immediate);
                let segment = image.segments.get(index).ok_or(MachOParseError::BindSegmentOutOfRange { index: immediate })?;
                state.address = segment.vm_address.wrapping_add(read_uleb(bytes, &mut cursor, end)?);
            }
            BIND_OPCODE_ADD_ADDR_ULEB => {
                state.address = state.address.wrapping_add(read_uleb(bytes, &mut cursor, end)?);
            }
            BIND_OPCODE_DO_BIND => {
                let stride = emit(image, &state, bindings, opcode_offset, lazy, weak_stream, stream)?;
                state.address = state.address.wrapping_add(stride);
            }
            BIND_OPCODE_DO_BIND_ADD_ADDR_ULEB => {
                let skip = read_uleb(bytes, &mut cursor, end)?;
                let stride = emit(image, &state, bindings, opcode_offset, lazy, weak_stream, stream)?;
                state.address = state.address.wrapping_add(stride).wrapping_add(skip);
            }
            BIND_OPCODE_DO_BIND_ADD_ADDR_IMM_SCALED => {
                let stride = emit(image, &state, bindings, opcode_offset, lazy, weak_stream, stream)?;
                state.address = state.address.wrapping_add(stride).wrapping_add(u64::from(immediate) * POINTER_SIZE);
            }
            BIND_OPCODE_DO_BIND_ULEB_TIMES_SKIPPING_ULEB => {
                let count = read_uleb(bytes, &mut cursor, end)?;
                let skip = read_uleb(bytes, &mut cursor, end)?;
                for _ in 0..count {
                    let stride = emit(image, &state, bindings, opcode_offset, lazy, weak_stream, stream)?;
                    state.address = state.address.wrapping_add(stride).wrapping_add(skip);
                }
            }
            _ => return Err(MachOParseError::BadBindOpcode { offset: opcode_offset, opcode }),
        }
    }
    Ok(())
}

/// 按当前状态产出一条绑定记录，返回该绑定占用的槽位步长。
#[allow(clippy::too_many_arguments)]
fn emit(
    image: &MachOImage,
    state: &State,
    bindings: &mut Vec<MachBind>,
    offset: u64,
    lazy: bool,
    weak_stream: bool,
    stream: &'static str,
) -> Result<u64, MachOParseError> {
    if state.symbol.is_empty() {
        return Err(MachOParseError::BindWithoutSymbol { offset });
    }
    let index = state.segment.unwrap_or(0);
    if let Some(index) = state.segment {
        if image.segments.get(usize::from(index)).is_none() {
            return Err(MachOParseError::BindSegmentOutOfRange { index });
        }
    }
    let library = (state.ordinal > 0)
        .then(|| image.dylibs.iter().find(|dylib| dylib.ordinal == Some(state.ordinal as u32)).map(|dylib| dylib.name.clone()))
        .flatten();
    let bind_type = MachBindType::from_value(state.bind_type);
    let stride = match bind_type {
        MachBindType::TextAbsolute32 | MachBindType::TextPcrel32 => 4,
        _ => POINTER_SIZE,
    };
    let segment = image
        .segments
        .iter()
        .find(|segment| {
            segment.vm_size > 0
                && state.address >= segment.vm_address
                && state.address < segment.vm_address.saturating_add(segment.vm_size)
        })
        .map_or_else(String::new, |segment| segment.name.clone());
    bindings.push(MachBind {
        symbol: state.symbol.clone(),
        library_ordinal: state.ordinal,
        library,
        segment_index: index,
        segment,
        address: state.address,
        addend: state.addend,
        bind_type,
        weak: weak_stream || state.flags & BIND_SYMBOL_FLAGS_WEAK_IMPORT != 0,
        lazy,
        stream,
    });
    Ok(stride)
}

/// 读取 `0` 结尾的符号名，未终止时取到流末尾。
fn read_symbol(bytes: &[u8], cursor: &mut usize, end: usize) -> String {
    let start = *cursor;
    while *cursor < end && bytes[*cursor] != 0 {
        *cursor += 1;
    }
    let symbol = String::from_utf8_lossy(&bytes[start..*cursor]).into_owned();
    if *cursor < end {
        *cursor += 1;
    }
    symbol
}

/// 读取 ULEB128。
fn read_uleb(bytes: &[u8], cursor: &mut usize, end: usize) -> Result<u64, MachOParseError> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        if *cursor >= end {
            return Err(MachOParseError::OutOfRange { offset: *cursor as u64, length: 1 });
        }
        let byte = bytes[*cursor];
        *cursor += 1;
        if shift < 64 {
            result |= u64::from(byte & 0x7f) << shift;
        }
        shift += 7;
        if byte & 0x80 == 0 {
            return Ok(result);
        }
    }
}

/// 读取 SLEB128。
fn read_sleb(bytes: &[u8], cursor: &mut usize, end: usize) -> Result<i64, MachOParseError> {
    let mut result = 0i64;
    let mut shift = 0u32;
    loop {
        if *cursor >= end {
            return Err(MachOParseError::OutOfRange { offset: *cursor as u64, length: 1 });
        }
        let byte = bytes[*cursor];
        *cursor += 1;
        if shift < 64 {
            result |= i64::from(byte & 0x7f) << shift;
        }
        shift += 7;
        if byte & 0x80 == 0 {
            if shift < 64 && byte & 0x40 != 0 {
                result |= -1i64 << shift;
            }
            return Ok(result);
        }
    }
}
