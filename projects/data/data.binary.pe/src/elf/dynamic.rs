//! `ELF` 动态链接事实：`PT_INTERP` 解释器、`PT_DYNAMIC` 的 `DT_NEEDED`、
//! `DT_SONAME`、`DT_RPATH` 与 `DT_RUNPATH`。
//!
//! 虚拟地址经 `PT_LOAD` 段映射到文件偏移，因此节表被剥离时仍可工作。

use super::read::{Cursor, ElfParseError, parse_elf_header};
use super::segments::{ElfSegmentType, read_program_headers};

const DT_NULL: i64 = 0;
const DT_NEEDED: i64 = 1;
const DT_STRTAB: i64 = 5;
const DT_STRSZ: i64 = 10;
const DT_SONAME: i64 = 14;
const DT_RPATH: i64 = 15;
const DT_RUNPATH: i64 = 29;

/// 动态条目上限，防止损坏的段值撑爆列表。
const MAX_DYNAMIC_ENTRIES: usize = 4096;
/// `DT_NEEDED` 上限。
const MAX_NEEDED: usize = 256;
/// 搜索路径上限。
const MAX_SEARCH_PATHS: usize = 64;

/// 一张 `ELF` 镜像的动态链接事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfDynamic {
    /// 是否存在 `PT_DYNAMIC` 段。
    pub present: bool,
    /// `PT_INTERP` 指定的程序解释器。
    pub interpreter: Option<String>,
    /// `DT_SONAME`。
    pub soname: Option<String>,
    /// `DT_NEEDED` 库名，保持动态条目顺序。
    pub needed: Vec<String>,
    /// `DT_RPATH` 按 `:` 切分后的条目。
    pub rpath: Vec<String>,
    /// `DT_RUNPATH` 按 `:` 切分后的条目。
    pub runpath: Vec<String>,
    /// `DT_NULL` 之前读取的条目数量。
    pub dynamic_entries: usize,
    /// 是否因上限或越界截断。
    pub truncated: bool,
}

/// 读取 `ELF32` / `ELF64` 的动态链接事实。
pub fn read_dynamic(bytes: &[u8]) -> Result<ElfDynamic, ElfParseError> {
    let header = parse_elf_header(bytes)?;
    let program = read_program_headers(bytes)?;
    let is_64 = header.class.is_64();
    let cursor = Cursor::new(bytes, header.endian);

    let mut loads = Vec::new();
    let mut interpreter = None;
    let mut dynamic = None;
    for segment in &program.segments {
        match segment.segment_type {
            ElfSegmentType::Load => loads.push(Load {
                virtual_address: segment.virtual_address,
                offset: segment.offset,
                file_size: segment.file_size,
            }),
            ElfSegmentType::Interp => interpreter = Some((segment.offset, segment.file_size)),
            ElfSegmentType::Dynamic => dynamic = Some((segment.offset, segment.file_size)),
            _ => {}
        }
    }

    let mut report = ElfDynamic {
        present: dynamic.is_some(),
        interpreter: None,
        soname: None,
        needed: Vec::new(),
        rpath: Vec::new(),
        runpath: Vec::new(),
        dynamic_entries: 0,
        truncated: program.truncated,
    };
    if let Some((offset, size)) = interpreter {
        report.interpreter = Some(cursor_string(&cursor, offset, size)?);
    }
    let Some((offset, size)) = dynamic
    else {
        return Ok(report);
    };

    let mut strtab_address = None;
    let mut strtab_size = None;
    let mut needed_offsets = Vec::new();
    let mut soname = None;
    let mut rpath = None;
    let mut runpath = None;

    let entry_size = if is_64 { 16 } else { 8 };
    let count = (size / entry_size) as usize;
    for index in 0..count.min(MAX_DYNAMIC_ENTRIES) {
        let base = offset + index as u64 * entry_size;
        let (tag, value) = dynamic_entry(&cursor, is_64, base)?;
        if tag == DT_NULL {
            break;
        }
        match tag {
            DT_NEEDED => needed_offsets.push(value),
            DT_STRTAB => strtab_address = Some(value),
            DT_STRSZ => strtab_size = Some(value),
            DT_SONAME => soname = Some(value),
            DT_RPATH => rpath = Some(value),
            DT_RUNPATH => runpath = Some(value),
            _ => {}
        }
        report.dynamic_entries += 1;
    }
    if count > MAX_DYNAMIC_ENTRIES {
        report.truncated = true;
    }

    let references = needed_offsets.len()
        + usize::from(soname.is_some())
        + usize::from(rpath.is_some())
        + usize::from(runpath.is_some());
    if references == 0 {
        return Ok(report);
    }
    let address = strtab_address.ok_or(ElfParseError::DynamicStringTableMissing)?;
    let table_offset = map_address(&loads, address).ok_or(ElfParseError::DynamicStringTableUnmapped { address })?;
    let limit = strtab_size.unwrap_or(u64::MAX);

    if needed_offsets.len() > MAX_NEEDED {
        report.truncated = true;
    }
    for index in needed_offsets.iter().take(MAX_NEEDED) {
        report.needed.push(dynamic_string(&cursor, table_offset, limit, *index)?);
    }
    report.soname = soname.map(|index| dynamic_string(&cursor, table_offset, limit, index)).transpose()?;
    report.rpath = split_paths(rpath.map(|index| dynamic_string(&cursor, table_offset, limit, index)).transpose()?);
    report.runpath = split_paths(runpath.map(|index| dynamic_string(&cursor, table_offset, limit, index)).transpose()?);
    if report.rpath.len() > MAX_SEARCH_PATHS || report.runpath.len() > MAX_SEARCH_PATHS {
        report.truncated = true;
        report.rpath.truncate(MAX_SEARCH_PATHS);
        report.runpath.truncate(MAX_SEARCH_PATHS);
    }
    Ok(report)
}

/// 动态条目读取器所需的 `PT_LOAD` 映射。
struct Load {
    virtual_address: u64,
    offset: u64,
    file_size: u64,
}

fn map_address(loads: &[Load], address: u64) -> Option<u64> {
    loads.iter().find_map(|load| {
        if load.file_size == 0 {
            return None;
        }
        let end = load.virtual_address.saturating_add(load.file_size);
        if address >= load.virtual_address && address < end {
            Some(load.offset.saturating_add(address - load.virtual_address))
        }
        else {
            None
        }
    })
}

fn split_paths(value: Option<String>) -> Vec<String> {
    value.map(|raw| raw.split(':').filter(|part| !part.is_empty()).map(str::to_string).collect()).unwrap_or_default()
}

fn dynamic_entry(cursor: &Cursor<'_>, is_64: bool, base: u64) -> Result<(i64, u64), ElfParseError> {
    if is_64 {
        Ok((cursor.u64(base)? as i64, cursor.u64(base + 8)?))
    }
    else {
        Ok((i64::from(cursor.u32(base)? as i32), u64::from(cursor.u32(base + 4)?)))
    }
}

/// 读取 `offset` 处、以 `length` 为界的 NUL 结尾字符串。
fn cursor_string(cursor: &Cursor<'_>, offset: u64, length: u64) -> Result<String, ElfParseError> {
    let region = cursor.slice(offset, length)?;
    Ok(c_string(region))
}

/// 读取动态字符串表中 `index` 处、以 `size` 为界的 NUL 结尾字符串。
fn dynamic_string(cursor: &Cursor<'_>, table: u64, size: u64, index: u64) -> Result<String, ElfParseError> {
    if index >= size {
        return Err(ElfParseError::DynamicStringOutOfRange { index, size });
    }
    cursor_string(cursor, table.saturating_add(index), size.saturating_sub(index))
}

fn c_string(region: &[u8]) -> String {
    let end = region.iter().position(|byte| *byte == 0).unwrap_or(region.len());
    String::from_utf8_lossy(&region[..end]).into_owned()
}
