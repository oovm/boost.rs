//! `LC_DYLD_CHAINED_FIXUPS`：头、导入条目与 starts 表摘要。
//!
//! 链式指针本身不解码。

use super::read::{MachOEndian, MachOImage};

/// `dyld_chained_fixups_header`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachChainedFixupsHeader {
    /// 结构版本；当前为 `0`。
    pub fixups_version: u32,
    /// `dyld_chained_starts_in_image` 相对链数据的偏移。
    pub starts_offset: u32,
    /// 导入表相对链数据的偏移。
    pub imports_offset: u32,
    /// 符号字符串池相对链数据的偏移。
    pub symbols_offset: u32,
    /// 声明的导入数量。
    pub imports_count: u32,
    /// 导入条目布局：`1` 普通、`2` 带 addend、`3` 带 64 位 addend。
    pub imports_format: u32,
    /// 符号池编码：`0` 未压缩、`1` zlib。
    pub symbols_format: u32,
}

/// 一条导入条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachChainedImport {
    /// 库序号；负值表示特殊序号（如 `-2` 平查找）。
    pub lib_ordinal: i32,
    /// 是否为弱导入。
    pub weak_import: bool,
    /// 符号池内偏移。
    pub name_offset: u32,
    /// 解析后的符号名；池被压缩或偏移越界时为空串。
    pub name: String,
    /// `ADDEND` / `ADDEND64` 格式的加数。
    pub addend: Option<i64>,
}

/// `LC_DYLD_CHAINED_FIXUPS` 摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachChainedFixups {
    /// 是否存在该命令。
    pub present: bool,
    /// 链数据的文件偏移。
    pub data_offset: u32,
    /// 链数据大小。
    pub data_size: u32,
    /// 解码后的头，数据足够长时存在。
    pub header: Option<MachChainedFixupsHeader>,
    /// 保留下来的导入条目。
    pub imports: Vec<MachChainedImport>,
    /// 是否因上限移除条目。
    pub truncated: bool,
    /// `dyld_chained_starts_in_image` 的段数量。
    pub starts_in_image: Option<u32>,
    /// 拥有非空链起始表的段数量。
    pub starts_in_segment_count: usize,
}

const MAX_CHAINED_IMPORTS: usize = 512;
const DYLD_CHAINED_IMPORT: u32 = 1;
const DYLD_CHAINED_IMPORT_ADDEND: u32 = 2;
const DYLD_CHAINED_IMPORT_ADDEND64: u32 = 3;

/// 解码 `LC_DYLD_CHAINED_FIXUPS` 的头、导入与 starts 表。
pub fn read_chained_fixups(bytes: &[u8], image: &MachOImage) -> MachChainedFixups {
    let mut report = MachChainedFixups {
        present: false,
        data_offset: 0,
        data_size: 0,
        header: None,
        imports: Vec::new(),
        truncated: false,
        starts_in_image: None,
        starts_in_segment_count: 0,
    };
    let Some(command) = image.chained_fixups
    else {
        return report;
    };
    report.present = true;
    report.data_offset = command.data_offset;
    report.data_size = command.data_size;

    let base = command.data_offset as u64;
    let endian = image.header.endian;
    let read = |offset: u64| read_u32(bytes, endian, offset);
    let (
        Some(fixups_version),
        Some(starts_offset),
        Some(imports_offset),
        Some(symbols_offset),
        Some(imports_count),
        Some(imports_format),
        Some(symbols_format),
    ) = (
        read(base),
        read(base + 4),
        read(base + 8),
        read(base + 12),
        read(base + 16),
        read(base + 20),
        read(base + 24),
    )
    else {
        report.truncated = true;
        return report;
    };
    report.header = Some(MachChainedFixupsHeader {
        fixups_version,
        starts_offset,
        imports_offset,
        symbols_offset,
        imports_count,
        imports_format,
        symbols_format,
    });

    let data = bytes.get(base as usize..base as usize + command.data_size as usize).unwrap_or(&[]);
    let symbols = if symbols_format == 0 {
        data.get(symbols_offset as usize..).unwrap_or(&[])
    }
    else {
        &[]
    };
    let imports_base = base.saturating_add(u64::from(imports_offset));
    let declared = imports_count as usize;
    let limit = declared.min(MAX_CHAINED_IMPORTS);
    let known_format =
        matches!(imports_format, DYLD_CHAINED_IMPORT | DYLD_CHAINED_IMPORT_ADDEND | DYLD_CHAINED_IMPORT_ADDEND64);
    if known_format {
        for index in 0..limit {
            match chained_import(bytes, endian, imports_base, index, imports_format, symbols) {
                Some(import) => report.imports.push(import),
                None => {
                    report.truncated = true;
                    break;
                }
            }
        }
    }
    report.truncated |= declared > report.imports.len();

    if let Some(seg_count) = read(base + u64::from(starts_offset)) {
        report.starts_in_image = Some(seg_count);
        let max = (seg_count as usize).min(MAX_CHAINED_IMPORTS);
        let table = base + u64::from(starts_offset) + 4;
        for index in 0..max {
            match read(table + (index as u64) * 4) {
                Some(value) if value != 0 => report.starts_in_segment_count += 1,
                Some(_) => {}
                None => {
                    report.truncated = true;
                    break;
                }
            }
        }
    }
    report
}

fn chained_import(
    bytes: &[u8],
    endian: MachOEndian,
    base: u64,
    index: usize,
    format: u32,
    symbols: &[u8],
) -> Option<MachChainedImport> {
    if format == DYLD_CHAINED_IMPORT_ADDEND64 {
        let entry = base.checked_add((index as u64).checked_mul(16)?)?;
        let raw = read_u64(bytes, endian, entry)?;
        let addend = read_u64(bytes, endian, entry + 8)?;
        let lib_ordinal = i32::from((raw & 0xffff) as u16 as i16);
        let weak_import = (raw >> 16) & 1 != 0;
        let name_offset = (raw >> 32) as u32;
        return Some(MachChainedImport {
            lib_ordinal,
            weak_import,
            name_offset,
            name: chained_name(symbols, name_offset),
            addend: Some(addend as i64),
        });
    }

    let entry_size = if format == DYLD_CHAINED_IMPORT_ADDEND { 8 } else { 4 };
    let entry = base.checked_add((index as u64).checked_mul(entry_size)?)?;
    let raw = read_u32(bytes, endian, entry)?;
    let lib_ordinal = i32::from((raw & 0xff) as u8 as i8);
    let weak_import = (raw >> 8) & 1 != 0;
    let name_offset = raw >> 9;
    let addend = match format {
        DYLD_CHAINED_IMPORT_ADDEND => read_u32(bytes, endian, entry + 4).map(|value| i64::from(value as i32)),
        _ => None,
    };
    if format == DYLD_CHAINED_IMPORT_ADDEND && addend.is_none() {
        return None;
    }
    Some(MachChainedImport { lib_ordinal, weak_import, name_offset, name: chained_name(symbols, name_offset), addend })
}

fn chained_name(symbols: &[u8], offset: u32) -> String {
    let Some(rest) = symbols.get(offset as usize..)
    else {
        return String::new();
    };
    let end = rest.iter().position(|byte| *byte == 0).unwrap_or(rest.len());
    String::from_utf8_lossy(&rest[..end]).into_owned()
}

pub(super) fn read_u32(bytes: &[u8], endian: MachOEndian, offset: u64) -> Option<u32> {
    let start = usize::try_from(offset).ok()?;
    let raw: [u8; 4] = bytes.get(start..start + 4)?.try_into().ok()?;
    Some(match endian {
        MachOEndian::Little => u32::from_le_bytes(raw),
        MachOEndian::Big => u32::from_be_bytes(raw),
    })
}

pub(super) fn read_u64(bytes: &[u8], endian: MachOEndian, offset: u64) -> Option<u64> {
    let start = usize::try_from(offset).ok()?;
    let raw: [u8; 8] = bytes.get(start..start + 8)?.try_into().ok()?;
    Some(match endian {
        MachOEndian::Little => u64::from_le_bytes(raw),
        MachOEndian::Big => u64::from_be_bytes(raw),
    })
}
