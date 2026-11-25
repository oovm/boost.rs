//! `PE` 导入表、导出表与 `COFF` 符号表读取。
//!
//! 只解析读取侧需要的字段：导入表给出 `DLL`、函数名或序号、`IAT` 槽 `RVA`；
//! 导出表给出名称、序号、`RVA` 与转发目标；`COFF` 符号表给出名称、值与节号。
//! 写出一侧仍由 `native` 与 `clr` 模块负责。

use std::fmt;

use super::read::{PeImage, rva_to_offset};

/// 导入目录在数据目录中的索引。
const IMPORT_DIRECTORY: usize = 1;
/// 导出目录在数据目录中的索引。
const EXPORT_DIRECTORY: usize = 0;
/// 调试目录在数据目录中的索引。
const DEBUG_DIRECTORY: usize = 6;
/// 单条调试目录条目的大小。
const DEBUG_ENTRY_SIZE: usize = 28;
/// 最多读取的调试目录条目数。
const MAX_DEBUG_ENTRIES: usize = 256;
/// `IMAGE_DEBUG_TYPE_CODEVIEW`。
const DEBUG_TYPE_CODEVIEW: u32 = 2;
/// 单个名称或字符串的最大长度。
const MAX_STRING: usize = 4096;
/// 单个导入表最多遍历的 thunk 数。
const MAX_THUNKS: usize = 65536;
/// 最多遍历的导入描述符数。
const MAX_DESCRIPTORS: usize = 4096;
/// 最多读取的导出函数数。
const MAX_EXPORTS: usize = 65536;

/// `PE` 分析错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeAnalysisError {
    /// `RVA` 无法映射到文件偏移。
    Rva(u32),
    /// 表结构越界或长度不足。
    Truncated {
        /// 表名。
        table: &'static str,
    },
    /// 结构声明的条目数超过上限。
    Limit {
        /// 表名。
        table: &'static str,
        /// 上限。
        limit: usize,
    },
}

impl fmt::Display for PeAnalysisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rva(rva) => write!(f, "`RVA` 0x{rva:X} 无法映射到文件偏移"),
            Self::Truncated { table } => write!(f, "{table} 越过文件末尾"),
            Self::Limit { table, limit } => write!(f, "{table} 超过 {limit} 条上限"),
        }
    }
}

impl std::error::Error for PeAnalysisError {}

/// 一条导入函数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeImport {
    /// 提供该函数的 `DLL` 名称。
    pub library: String,
    /// 函数名；按序号导入时为 `None`。
    pub name: Option<String>,
    /// 导入序号。
    pub ordinal: Option<u32>,
    /// `名称`/`序号` 表项中的 hint（若为按名称导入）。
    pub hint: Option<u16>,
    /// `IAT` 槽的 `RVA`；表缺失时为 `None`。
    pub iat_rva: Option<u32>,
}

/// 一条导出函数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeExport {
    /// 导出名；仅按序号导出时为 `None`。
    pub name: Option<String>,
    /// 导出序号。
    pub ordinal: u32,
    /// 导出函数 `RVA`。
    pub rva: u32,
    /// 转发目标（形如 `KERNEL32.Sleep`）。
    pub forwarder: Option<String>,
}

/// 一条 `COFF` 符号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeSymbol {
    /// 符号名。
    pub name: String,
    /// 符号值（通常是 `RVA`）。
    pub value: u32,
    /// 节号；`0` 表示未定义，`-1` 表示绝对值。
    pub section_number: i16,
    /// 类型字段。
    pub symbol_type: u16,
    /// 存储类别。
    pub storage_class: u8,
}

impl PeSymbol {
    /// 是否为函数符号。
    pub fn is_function(&self) -> bool {
        self.symbol_type & 0x20 != 0
    }
}

/// 读取导入表。
///
/// 按描述符顺序展开每个 `DLL` 的 `ILT`（缺失时退回 `IAT`），序号导入与
/// 名称导入都会保留；`IAT` 槽 `RVA` 用于把导入与运行期地址对应起来。
pub fn read_imports(image: &PeImage, bytes: &[u8]) -> Result<Vec<PeImport>, PeAnalysisError> {
    let directory = image.optional.data_directories[IMPORT_DIRECTORY];
    if directory.rva == 0 || directory.size == 0 {
        return Ok(Vec::new());
    }
    let view = View { image, bytes };
    let is_64 = image.optional.magic == 0x020b;
    let thunk_size = if is_64 { 8u64 } else { 4u64 };
    let mut cursor = view.rva_offset(directory.rva)?;
    let mut imports = Vec::new();

    for _ in 0..MAX_DESCRIPTORS {
        let original_first_thunk = view.u32(cursor, "导入描述符")?;
        let name_rva = view.u32(cursor + 12, "导入描述符")?;
        let first_thunk = view.u32(cursor + 16, "导入描述符")?;
        if original_first_thunk == 0 && name_rva == 0 && first_thunk == 0 {
            return Ok(imports);
        }
        let library = view.cstring(name_rva)?;
        let lookup_rva = if original_first_thunk != 0 { original_first_thunk } else { first_thunk };

        if lookup_rva != 0 {
            let lookup_offset = view.rva_offset(lookup_rva)?;
            for index in 0..MAX_THUNKS {
                let entry_offset = lookup_offset + index as u64 * thunk_size;
                let raw =
                    if is_64 { view.u64(entry_offset, "导入地址表")? } else { u64::from(view.u32(entry_offset, "导入地址表")?) };
                if raw == 0 {
                    break;
                }
                let by_ordinal = if is_64 { raw >> 63 != 0 } else { raw >> 31 != 0 };
                let (name, hint, ordinal) = if by_ordinal {
                    (None, None, Some((raw & 0xffff) as u32))
                }
                else {
                    let name_rva = (raw & 0x7fff_ffff) as u32;
                    let name_offset = view.rva_offset(name_rva)?;
                    let hint = view.u16(name_offset, "导入名称")?;
                    (Some(view.cstring(name_rva + 2)?), Some(hint), None)
                };
                let iat_rva = (first_thunk != 0).then(|| first_thunk.wrapping_add((index as u32).wrapping_mul(thunk_size as u32)));
                imports.push(PeImport { library: library.clone(), name, ordinal, hint, iat_rva });
            }
        }
        cursor += 20;
    }
    Err(PeAnalysisError::Limit { table: "导入描述符", limit: MAX_DESCRIPTORS })
}

/// 读取导出表。
///
/// 结果按函数表中的索引顺序排列，序号为 `Base + 索引`；`RVA` 落入导出目录
/// 范围的条目视为转发器，`forwarder` 给出目标字符串。
pub fn read_exports(image: &PeImage, bytes: &[u8]) -> Result<Vec<PeExport>, PeAnalysisError> {
    let directory = image.optional.data_directories[EXPORT_DIRECTORY];
    if directory.rva == 0 || directory.size == 0 {
        return Ok(Vec::new());
    }
    let view = View { image, bytes };
    let base = view.rva_offset(directory.rva)?;
    let ordinal_base = view.u32(base + 16, "导出目录")?;
    let function_count = view.u32(base + 20, "导出目录")? as usize;
    let name_count = view.u32(base + 24, "导出目录")? as usize;
    let functions_rva = view.u32(base + 28, "导出目录")?;
    let names_rva = view.u32(base + 32, "导出目录")?;
    let ordinals_rva = view.u32(base + 36, "导出目录")?;
    if function_count > MAX_EXPORTS {
        return Err(PeAnalysisError::Limit { table: "导出函数", limit: MAX_EXPORTS });
    }
    if name_count > MAX_EXPORTS {
        return Err(PeAnalysisError::Limit { table: "导出名称", limit: MAX_EXPORTS });
    }

    let mut names = std::collections::BTreeMap::new();
    if name_count != 0 && names_rva != 0 && ordinals_rva != 0 {
        let names_offset = view.rva_offset(names_rva)?;
        let ordinals_offset = view.rva_offset(ordinals_rva)?;
        for index in 0..name_count {
            let name_rva = view.u32(names_offset + index as u64 * 4, "导出名称")?;
            let function_index = view.u16(ordinals_offset + index as u64 * 2, "导出序号")? as u32;
            names.insert(function_index, view.cstring(name_rva)?);
        }
    }

    let mut exports = Vec::new();
    if function_count != 0 && functions_rva != 0 {
        let functions_offset = view.rva_offset(functions_rva)?;
        let forwarder_end = directory.rva.saturating_add(directory.size);
        for index in 0..function_count {
            let rva = view.u32(functions_offset + index as u64 * 4, "导出函数")?;
            if rva == 0 {
                continue;
            }
            let forwarder = (rva >= directory.rva && rva < forwarder_end).then(|| view.cstring(rva)).transpose()?;
            exports.push(PeExport {
                name: names.get(&(index as u32)).cloned(),
                ordinal: ordinal_base.saturating_add(index as u32),
                rva,
                forwarder,
            });
        }
    }
    Ok(exports)
}

/// 一条调试目录条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeDebugEntry {
    /// `Characteristics`。
    pub characteristics: u32,
    /// `TimeDateStamp`。
    pub timestamp: u32,
    /// `MajorVersion`。
    pub major_version: u16,
    /// `MinorVersion`。
    pub minor_version: u16,
    /// `IMAGE_DEBUG_TYPE_*`。
    pub debug_type: u32,
    /// `SizeOfData`。
    pub size_of_data: u32,
    /// 调试数据的 `RVA`。
    pub address_of_raw_data: u32,
    /// 调试数据的文件偏移。
    pub pointer_to_raw_data: u32,
    /// `CodeView` 载荷；非 `CodeView` 条目为 `None`。
    pub code_view: Option<PeCodeView>,
}

impl PeDebugEntry {
    /// 调试类型名，未知类型返回 `unknown`。
    pub fn type_name(&self) -> &'static str {
        match self.debug_type {
            0 => "unknown",
            1 => "coff",
            2 => "codeview",
            3 => "fpo",
            4 => "misc",
            5 => "exception",
            6 => "fixup",
            9 => "borland",
            10 => "reserved10",
            11 => "clsid",
            12 => "vc_feature",
            13 => "pogo",
            14 => "iltcg",
            16 => "repro",
            17 => "embedded_portable_pdb",
            19 => "embedded_pdb",
            20 => "pdb_checksum",
            _ => "unknown",
        }
    }
}

/// `CodeView` 调试载荷（`RSDS` / `NB10`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeCodeView {
    /// 四字节魔数，`RSDS` 或 `NB10`。
    pub signature: String,
    /// `RSDS` 携带的 16 字节 `GUID`。
    pub guid: Option<[u8; 16]>,
    /// `RSDS` 的 `age` 或 `NB10` 的 `age`。
    pub age: u32,
    /// `PDB` 文件路径。
    pub path: String,
}

/// 读取调试目录。
///
/// 目录项按 28 字节顺序解析；`CodeView` 条目会继续读取 `RSDS`（`GUID`、
/// `age`、`PDB` 路径）或 `NB10`（`age`、`PDB` 路径）。数据定位优先用
/// `PointerToRawData`，为 0 时退回 `AddressOfRawData` 的 `RVA` 映射。
pub fn read_debug_directory(image: &PeImage, bytes: &[u8]) -> Result<Vec<PeDebugEntry>, PeAnalysisError> {
    let directory = image.optional.data_directories[DEBUG_DIRECTORY];
    if directory.rva == 0 || directory.size == 0 {
        return Ok(Vec::new());
    }
    let view = View { image, bytes };
    let base = view.rva_offset(directory.rva)?;
    let count = (directory.size as usize / DEBUG_ENTRY_SIZE).min(MAX_DEBUG_ENTRIES);
    view.slice(base, (count * DEBUG_ENTRY_SIZE) as u64, "调试目录")?;

    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
        let cursor = base + (index * DEBUG_ENTRY_SIZE) as u64;
        let debug_type = view.u32(cursor + 12, "调试目录")?;
        let size_of_data = view.u32(cursor + 16, "调试目录")?;
        let address_of_raw_data = view.u32(cursor + 20, "调试目录")?;
        let pointer_to_raw_data = view.u32(cursor + 24, "调试目录")?;
        let code_view = if debug_type == DEBUG_TYPE_CODEVIEW && size_of_data >= 8 {
            view.code_view(pointer_to_raw_data, address_of_raw_data, size_of_data)?
        }
        else {
            None
        };
        entries.push(PeDebugEntry {
            characteristics: view.u32(cursor, "调试目录")?,
            timestamp: view.u32(cursor + 4, "调试目录")?,
            major_version: view.u16(cursor + 8, "调试目录")?,
            minor_version: view.u16(cursor + 10, "调试目录")?,
            debug_type,
            size_of_data,
            address_of_raw_data,
            pointer_to_raw_data,
            code_view,
        });
    }
    Ok(entries)
}

/// 读取 `COFF` 符号表。
///
/// 名称超过 8 字节时从符号表后的字符串表读取；辅助符号记录会被跳过。
/// 没有符号表时返回空列表。
pub fn read_coff_symbols(image: &PeImage, bytes: &[u8]) -> Result<Vec<PeSymbol>, PeAnalysisError> {
    let pointer = image.coff.pointer_to_symbol_table;
    let count = image.coff.number_of_symbols;
    if pointer == 0 || count == 0 {
        return Ok(Vec::new());
    }
    let view = View { image, bytes };
    let table_offset = u64::from(pointer);
    let table_size = u64::from(count).saturating_mul(18);
    view.slice(table_offset, table_size, "COFF 符号表")?;
    let strings_offset = table_offset + table_size;
    let strings_size = view.u32(strings_offset, "COFF 字符串表")?;

    let mut symbols = Vec::new();
    let mut index = 0u64;
    while index < u64::from(count) {
        let entry = table_offset + index * 18;
        let record = view.slice(entry, 18, "COFF 符号表")?;
        let name = if record[0..4] == [0, 0, 0, 0] {
            let name_offset = u32::from_le_bytes(record[4..8].try_into().expect("slice"));
            if name_offset >= strings_size { String::new() } else { read_cstring(bytes, (strings_offset + u64::from(name_offset)) as usize) }
        }
        else {
            let length = record[..8].iter().position(|byte| *byte == 0).unwrap_or(8);
            String::from_utf8_lossy(&record[..length]).into_owned()
        };
        let value = u32::from_le_bytes(record[8..12].try_into().expect("slice"));
        let section_number = i16::from_le_bytes(record[12..14].try_into().expect("slice"));
        let symbol_type = u16::from_le_bytes(record[14..16].try_into().expect("slice"));
        let storage_class = record[16];
        let auxiliary = record[17] as u64;
        if !name.is_empty() {
            symbols.push(PeSymbol { name, value, section_number, symbol_type, storage_class });
        }
        index += 1 + auxiliary;
    }
    Ok(symbols)
}

/// 按镜像映射读取字段的视图。
struct View<'a> {
    image: &'a PeImage,
    bytes: &'a [u8],
}

impl<'a> View<'a> {
    /// 把 `RVA` 换算成文件偏移：头部区间按 1:1 映射，其余走节表。
    fn rva_offset(&self, rva: u32) -> Result<u64, PeAnalysisError> {
        let headers = u64::from(self.image.optional.size_of_headers);
        if headers != 0 && u64::from(rva) < headers {
            return Ok(u64::from(rva));
        }
        rva_to_offset(&self.image.sections, rva).map(u64::from).map_err(|_| PeAnalysisError::Rva(rva))
    }

    fn slice(&self, offset: u64, length: u64, table: &'static str) -> Result<&'a [u8], PeAnalysisError> {
        let end = offset.checked_add(length).ok_or(PeAnalysisError::Truncated { table })?;
        if end > self.bytes.len() as u64 {
            return Err(PeAnalysisError::Truncated { table });
        }
        Ok(&self.bytes[offset as usize..end as usize])
    }

    fn u16(&self, offset: u64, table: &'static str) -> Result<u16, PeAnalysisError> {
        let raw = self.slice(offset, 2, table)?;
        Ok(u16::from_le_bytes([raw[0], raw[1]]))
    }

    fn u32(&self, offset: u64, table: &'static str) -> Result<u32, PeAnalysisError> {
        let raw = self.slice(offset, 4, table)?;
        Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
    }

    fn u64(&self, offset: u64, table: &'static str) -> Result<u64, PeAnalysisError> {
        let raw = self.slice(offset, 8, table)?;
        Ok(u64::from_le_bytes(raw.try_into().expect("slice")))
    }

    /// 读取 `RVA` 处的 `0` 结尾字符串。
    fn cstring(&self, rva: u32) -> Result<String, PeAnalysisError> {
        let offset = self.rva_offset(rva)?;
        Ok(read_cstring(self.bytes, offset as usize))
    }

    /// 读取 `CodeView` 载荷。
    fn code_view(&self, pointer_to_raw_data: u32, address_of_raw_data: u32, size_of_data: u32) -> Result<Option<PeCodeView>, PeAnalysisError> {
        let offset = if pointer_to_raw_data != 0 {
            u64::from(pointer_to_raw_data)
        }
        else if address_of_raw_data != 0 {
            self.rva_offset(address_of_raw_data)?
        }
        else {
            return Ok(None);
        };
        let length = u64::from(size_of_data);
        let available = (self.bytes.len() as u64).saturating_sub(offset);
        let read_len = length.min(MAX_STRING as u64).min(available);
        if read_len < 8 {
            return Ok(None);
        }
        let data = self.slice(offset, read_len, "调试数据")?;
        let signature = String::from_utf8_lossy(&data[0..4]).into_owned();
        match &data[0..4] {
            b"RSDS" if read_len >= 24 => {
                let mut guid = [0u8; 16];
                guid.copy_from_slice(&data[4..20]);
                let age = u32::from_le_bytes([data[20], data[21], data[22], data[23]]);
                let path = read_cstring_limited(&data[24..], length.saturating_sub(24) as usize);
                Ok(Some(PeCodeView { signature, guid: Some(guid), age, path }))
            }
            b"NB10" if read_len >= 16 => {
                let age = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
                let path = read_cstring_limited(&data[16..], length.saturating_sub(16) as usize);
                Ok(Some(PeCodeView { signature, guid: None, age, path }))
            }
            _ => Ok(Some(PeCodeView { signature, guid: None, age: 0, path: String::new() })),
        }
    }
}

/// 读取以 `0` 结尾的字符串，越界时返回空串，长度限制为 [`MAX_STRING`]。
fn read_cstring(bytes: &[u8], offset: usize) -> String {
    let Some(tail) = bytes.get(offset..)
    else {
        return String::new();
    };
    read_cstring_limited(tail, MAX_STRING)
}

/// 在给定字节区间内读取以 `0` 结尾的字符串。
fn read_cstring_limited(bytes: &[u8], max: usize) -> String {
    let limit = max.min(MAX_STRING).min(bytes.len());
    let length = bytes[..limit].iter().position(|byte| *byte == 0).unwrap_or(limit);
    String::from_utf8_lossy(&bytes[..length]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{read_coff_symbols, read_debug_directory, read_exports, read_imports};
    use crate::pe::parse_pe;

    fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    /// 单节 `PE32+`：`.rdata` 覆盖文件 0x200..0x800，导入/导出/符号都在其中。
    fn sample_pe() -> Vec<u8> {
        let mut bytes = vec![0u8; 0x800];
        bytes[0..2].copy_from_slice(b"MZ");
        put_u32(&mut bytes, 0x3c, 0x80);
        bytes[0x80..0x84].copy_from_slice(b"PE\0\0");
        put_u16(&mut bytes, 0x84, 0x8664);
        put_u16(&mut bytes, 0x86, 1);
        put_u32(&mut bytes, 0x8c, 0x600);
        put_u32(&mut bytes, 0x90, 1);
        put_u16(&mut bytes, 0x94, 0xf0);
        put_u16(&mut bytes, 0x98, 0x20b);
        put_u64(&mut bytes, 0x98 + 24, 0x1_4000_0000);
        put_u32(&mut bytes, 0x98 + 56, 0x2000);
        put_u32(&mut bytes, 0x98 + 60, 0x200);
        // 数据目录：导出 0x1100/0x28，导入 0x1000/0x28。
        put_u32(&mut bytes, 0x98 + 112, 0x1100);
        put_u32(&mut bytes, 0x98 + 116, 0x28);
        put_u32(&mut bytes, 0x98 + 120, 0x1000);
        put_u32(&mut bytes, 0x98 + 124, 0x28);

        let section = 0x188;
        bytes[section..section + 6].copy_from_slice(b".rdata");
        put_u32(&mut bytes, section + 8, 0x600);
        put_u32(&mut bytes, section + 12, 0x1000);
        put_u32(&mut bytes, section + 16, 0x600);
        put_u32(&mut bytes, section + 20, 0x200);
        put_u32(&mut bytes, section + 36, 0x4000_0040);

        // 导入描述符：ILT 0x1040，名称 0x1030，IAT 0x1060。
        put_u32(&mut bytes, 0x200, 0x1040);
        put_u32(&mut bytes, 0x20c, 0x1030);
        put_u32(&mut bytes, 0x210, 0x1060);
        bytes[0x230..0x23d].copy_from_slice(b"KERNEL32.dll\0");
        put_u64(&mut bytes, 0x240, 0x1080);
        put_u64(&mut bytes, 0x248, 0x8000_0000_0000_0001);
        put_u64(&mut bytes, 0x260, 0x1080);
        put_u64(&mut bytes, 0x268, 0x8000_0000_0000_0001);
        bytes[0x282..0x28e].copy_from_slice(b"ExitProcess\0");

        // 导出目录。
        put_u32(&mut bytes, 0x300 + 12, 0x1130);
        put_u32(&mut bytes, 0x300 + 16, 1);
        put_u32(&mut bytes, 0x300 + 20, 2);
        put_u32(&mut bytes, 0x300 + 24, 1);
        put_u32(&mut bytes, 0x300 + 28, 0x1140);
        put_u32(&mut bytes, 0x300 + 32, 0x1148);
        put_u32(&mut bytes, 0x300 + 36, 0x114c);
        bytes[0x330..0x33b].copy_from_slice(b"sample.dll\0");
        put_u32(&mut bytes, 0x340, 0x1000);
        put_u32(&mut bytes, 0x348, 0x1150);
        put_u16(&mut bytes, 0x34c, 0);
        bytes[0x350..0x356].copy_from_slice(b"Start\0");

        // 一个 `COFF` 符号，名称内联。
        bytes[0x600..0x606].copy_from_slice(b"native");
        put_u32(&mut bytes, 0x608, 0x1000);
        put_u16(&mut bytes, 0x60c, 1);
        put_u16(&mut bytes, 0x60e, 0x20);
        bytes[0x610] = 2;
        put_u32(&mut bytes, 0x612, 4);

        // 调试目录：一条 `CodeView` 条目指向 0x400 处的 `RSDS` 记录。
        put_u32(&mut bytes, 0x98 + 112 + 6 * 8, 0x1180);
        put_u32(&mut bytes, 0x98 + 112 + 6 * 8 + 4, 28);
        put_u32(&mut bytes, 0x380 + 4, 0x1122_3344);
        put_u32(&mut bytes, 0x380 + 12, 2);
        let path = b"C:\\sample.pdb\0";
        put_u32(&mut bytes, 0x380 + 16, (4 + 16 + 4 + path.len()) as u32);
        put_u32(&mut bytes, 0x380 + 20, 0x1200);
        put_u32(&mut bytes, 0x380 + 24, 0x400);
        bytes[0x400..0x404].copy_from_slice(b"RSDS");
        for (index, byte) in bytes[0x404..0x414].iter_mut().enumerate() {
            *byte = index as u8;
        }
        put_u32(&mut bytes, 0x414, 7);
        bytes[0x418..0x418 + path.len()].copy_from_slice(path);
        bytes
    }

    #[test]
    fn imports_cover_names_and_ordinals() {
        let bytes = sample_pe();
        let image = parse_pe(&bytes).expect("parse");
        let imports = read_imports(&image, &bytes).expect("imports");
        assert_eq!(imports.len(), 2);
        assert_eq!(imports[0].library, "KERNEL32.dll");
        assert_eq!(imports[0].name.as_deref(), Some("ExitProcess"));
        assert_eq!(imports[0].hint, Some(0));
        assert_eq!(imports[0].iat_rva, Some(0x1060));
        assert_eq!(imports[1].name, None);
        assert_eq!(imports[1].ordinal, Some(1));
        assert_eq!(imports[1].iat_rva, Some(0x1068));
    }

    #[test]
    fn exports_keep_ordinals_and_skip_holes() {
        let bytes = sample_pe();
        let image = parse_pe(&bytes).expect("parse");
        let exports = read_exports(&image, &bytes).expect("exports");
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].name.as_deref(), Some("Start"));
        assert_eq!(exports[0].ordinal, 1);
        assert_eq!(exports[0].rva, 0x1000);
        assert!(exports[0].forwarder.is_none());
    }

    #[test]
    fn coff_symbols_are_decoded() {
        let bytes = sample_pe();
        let image = parse_pe(&bytes).expect("parse");
        let symbols = read_coff_symbols(&image, &bytes).expect("symbols");
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "native");
        assert_eq!(symbols[0].value, 0x1000);
        assert_eq!(symbols[0].section_number, 1);
        assert!(symbols[0].is_function());
    }

    #[test]
    fn debug_directory_reads_codeview_rsds() {
        let bytes = sample_pe();
        let image = parse_pe(&bytes).expect("parse");
        let entries = read_debug_directory(&image, &bytes).expect("debug");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].debug_type, 2);
        assert_eq!(entries[0].type_name(), "codeview");
        assert_eq!(entries[0].timestamp, 0x1122_3344);
        let code_view = entries[0].code_view.as_ref().expect("codeview");
        assert_eq!(code_view.signature, "RSDS");
        assert_eq!(code_view.age, 7);
        assert_eq!(code_view.path, "C:\\sample.pdb");
        let guid = code_view.guid.expect("guid");
        assert_eq!(guid[0], 0);
        assert_eq!(guid[15], 15);
    }

    #[test]
    fn missing_debug_directory_yields_empty_list() {
        let mut bytes = sample_pe();
        put_u32(&mut bytes, 0x98 + 112 + 6 * 8, 0);
        let image = parse_pe(&bytes).expect("parse");
        assert!(read_debug_directory(&image, &bytes).unwrap().is_empty());
    }

    #[test]
    fn missing_tables_yield_empty_lists() {
        let mut bytes = sample_pe();
        put_u32(&mut bytes, 0x98 + 112, 0);
        put_u32(&mut bytes, 0x98 + 120, 0);
        put_u32(&mut bytes, 0x8c, 0);
        put_u32(&mut bytes, 0x90, 0);
        let image = parse_pe(&bytes).expect("parse");
        assert!(read_imports(&image, &bytes).unwrap().is_empty());
        assert!(read_exports(&image, &bytes).unwrap().is_empty());
        assert!(read_coff_symbols(&image, &bytes).unwrap().is_empty());
    }
}
