//! ECMA-335 CLI metadata reader.
//!
//! 只读取反编译取证需要的元数据：PE 中的 CLI 头、`#~` 表流、`#Strings` 名字堆、
//! TypeDef / MethodDef / NestedClass 三张表，以及方法体头部里的 IL 长度。
//! 不解析 IL 指令，也不解析自定义特性或签名。
//!
//! 实现完全位于 crate 内，不依赖第三方 .NET 元数据库。

#![warn(missing_docs)]

use std::fmt;
use std::fs;
use std::path::Path;

/// CLI 元数据读取失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CilError {
    /// 文件无法读取。
    Io(String),
    /// 字节不是可识别的 PE/CLI 映像。
    Parse(String),
}

impl fmt::Display for CilError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) | Self::Parse(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for CilError {}

/// 一条 TypeDef 记录（RID 从 1 开始）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CilTypeDef {
    /// 表内行号，从 1 开始。
    pub rid: u32,
    /// 类型命名空间，`<Module>` 为空。
    pub namespace: String,
    /// 类型名。
    pub name: String,
    /// ECMA-335 TypeAttributes。
    pub flags: u32,
    /// MethodDef 列表的起始 RID。
    pub method_list: u32,
}

/// 一条 MethodDef 记录（RID 从 1 开始）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CilMethodDef {
    /// 表内行号，从 1 开始。
    pub rid: u32,
    /// 方法体 RVA，`0` 表示没有方法体。
    pub rva: u32,
    /// 方法名。
    pub name: String,
}

/// 一条 NestedClass 记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CilNestedClass {
    /// 嵌套类型 RID。
    pub nested_class: u32,
    /// 外层类型 RID。
    pub enclosing_class: u32,
}

/// 解析后的 CLI 映像。
#[derive(Debug, Clone, Default)]
pub struct CilAssembly {
    bytes: Vec<u8>,
    sections: Vec<Section>,
    size_of_headers: u32,
    types: Vec<CilTypeDef>,
    methods: Vec<CilMethodDef>,
    nested_classes: Vec<CilNestedClass>,
}

impl CilAssembly {
    /// 从路径读取并解析。
    pub fn from_path(path: &Path) -> Result<Self, CilError> {
        let bytes = fs::read(path).map_err(|error| CilError::Io(format!("{}: {error}", path.display())))?;
        Self::from_bytes(bytes)
    }

    /// 从内存字节解析。
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, CilError> {
        let pe = parse_pe(&bytes)?;
        let cli_offset = rva_to_offset(&pe.sections, pe.size_of_headers, pe.cli_rva)
            .ok_or_else(|| parse_error("CLI header RVA is not mapped by any section"))?;
        if pe.cli_size < 16 {
            return Err(parse_error("CLI header is too small to hold metadata fields"));
        }
        let metadata_rva = read_u32(&bytes, cli_offset + 8)?;
        let metadata_size = read_u32(&bytes, cli_offset + 12)?;
        if metadata_rva == 0 || metadata_size < 16 {
            return Err(parse_error("CLI header does not point at valid metadata"));
        }
        let metadata_offset = rva_to_offset(&pe.sections, pe.size_of_headers, metadata_rva)
            .ok_or_else(|| parse_error("metadata RVA is not mapped by any section"))?;
        let metadata_end = metadata_offset
            .checked_add(metadata_size as usize)
            .ok_or_else(|| parse_error("metadata extent overflows the address space"))?;
        let metadata = bytes
            .get(metadata_offset..metadata_end)
            .ok_or_else(|| parse_error("metadata extent exceeds the image"))?;

        let parsed = parse_metadata(metadata)?;

        Ok(Self {
            bytes,
            sections: pe.sections,
            size_of_headers: pe.size_of_headers,
            types: parsed.types,
            methods: parsed.methods,
            nested_classes: parsed.nested_classes,
        })
    }

    /// 全部 TypeDef 行，按 RID 顺序。
    pub fn types(&self) -> &[CilTypeDef] {
        &self.types
    }

    /// 全部 MethodDef 行，按 RID 顺序。
    pub fn methods(&self) -> &[CilMethodDef] {
        &self.methods
    }

    /// 全部 NestedClass 行。
    pub fn nested_classes(&self) -> &[CilNestedClass] {
        &self.nested_classes
    }

    /// 读取方法体头部的代码长度；RVA 无效或没有方法体时返回 `None`。
    pub fn method_body_size(&self, rva: u32) -> Option<u32> {
        if rva == 0 {
            return None;
        }
        let offset = rva_to_offset(&self.sections, self.size_of_headers, rva)?;
        let header = *self.bytes.get(offset)?;
        match header & 0x03 {
            0x02 => Some(u32::from(header >> 2)),
            0x03 => {
                let code_size = self.bytes.get(offset + 4..offset + 8)?;
                Some(u32::from_le_bytes([
                    code_size[0],
                    code_size[1],
                    code_size[2],
                    code_size[3],
                ]))
            }
            _ => None,
        }
    }
}

/// 一个 PE 节区（只保留 RVA 映射需要的字段）。
#[derive(Debug, Clone, Copy)]
struct Section {
    virtual_address: u32,
    virtual_size: u32,
    raw_size: u32,
    raw_pointer: u32,
}

impl Section {
    /// 节区覆盖的最大范围，按 `max(VirtualSize, SizeOfRawData)` 计算。
    fn extent(&self) -> u32 {
        self.virtual_size.max(self.raw_size)
    }
}

/// PE 头部里 CLI 元数据读取需要的字段。
struct PeImage {
    sections: Vec<Section>,
    size_of_headers: u32,
    cli_rva: u32,
    cli_size: u32,
}

/// 解析 DOS 头、PE 头、可选头与节表，取出 CLI 数据目录项。
fn parse_pe(bytes: &[u8]) -> Result<PeImage, CilError> {
    if read_u16(bytes, 0)? != 0x5A4D {
        return Err(parse_error("image does not start with an MZ signature"));
    }
    let pe_offset = read_u32(bytes, 0x3C)? as usize;
    if read_u32(bytes, pe_offset)? != 0x0000_4550 {
        return Err(parse_error("image does not carry a PE signature"));
    }

    let coff = pe_offset
        .checked_add(4)
        .ok_or_else(|| parse_error("COFF header offset overflows"))?;
    let number_of_sections = read_u16(bytes, coff + 2)? as usize;
    if number_of_sections == 0 || number_of_sections > 96 {
        return Err(parse_error("PE section count is out of range"));
    }
    let size_of_optional_header = read_u16(bytes, coff + 16)? as usize;
    let optional = coff
        .checked_add(20)
        .ok_or_else(|| parse_error("optional header offset overflows"))?;

    let magic = read_u16(bytes, optional)?;
    let (directories_offset, directory_count_offset) = match magic {
        0x10B => (96usize, 92usize),
        0x20B => (112usize, 108usize),
        _ => return Err(parse_error("unsupported PE optional header magic")),
    };
    let size_of_headers = read_u32(bytes, optional + 60)?;
    let directory_count = read_u32(bytes, optional + directory_count_offset)?;
    if directory_count < 15 {
        return Err(parse_error("PE has no CLI data directory"));
    }

    let cli_directory = optional
        .checked_add(directories_offset)
        .and_then(|base| base.checked_add(14 * 8))
        .ok_or_else(|| parse_error("CLI data directory offset overflows"))?;
    let cli_rva = read_u32(bytes, cli_directory)?;
    let cli_size = read_u32(bytes, cli_directory + 4)?;

    let section_table = optional
        .checked_add(size_of_optional_header)
        .ok_or_else(|| parse_error("section table offset overflows"))?;
    let mut sections = Vec::with_capacity(number_of_sections);
    for index in 0..number_of_sections {
        let base = section_table
            .checked_add(index * 40)
            .ok_or_else(|| parse_error("section header offset overflows"))?;
        sections.push(Section {
            virtual_size: read_u32(bytes, base + 8)?,
            virtual_address: read_u32(bytes, base + 12)?,
            raw_size: read_u32(bytes, base + 16)?,
            raw_pointer: read_u32(bytes, base + 20)?,
        });
    }

    Ok(PeImage {
        sections,
        size_of_headers,
        cli_rva,
        cli_size,
    })
}

/// 把 RVA 映射到文件偏移；头部区间按 1:1 处理，其余落在节区里。
fn rva_to_offset(sections: &[Section], size_of_headers: u32, rva: u32) -> Option<usize> {
    if rva < size_of_headers {
        return Some(rva as usize);
    }
    for section in sections {
        let start = section.virtual_address;
        let end = start.checked_add(section.extent())?;
        if rva >= start && rva < end {
            let delta = rva - start;
            return Some(section.raw_pointer.checked_add(delta)? as usize);
        }
    }
    None
}

/// 构造一条解析错误。
fn parse_error(message: &str) -> CilError {
    CilError::Parse(message.to_owned())
}
/// 解析结果集合。
struct ParsedMetadata {
    types: Vec<CilTypeDef>,
    methods: Vec<CilMethodDef>,
    nested_classes: Vec<CilNestedClass>,
}

/// 元数据流在根内的位置与长度。
#[derive(Debug, Clone, Copy)]
struct StreamRange {
    offset: usize,
    size: usize,
}

/// 堆索引宽度。
#[derive(Debug, Clone, Copy)]
struct HeapSizes {
    string: usize,
    guid: usize,
    blob: usize,
}

/// `#~` 表流的布局：行数、行宽和每张表的行数据起点（相对表流起点）。
struct TableLayout {
    rows: [u32; 64],
    row_sizes: [usize; 64],
    offsets: [usize; 64],
    heap: HeapSizes,
}

/// 解析 `BSJB` 元数据根，定位 `#~`（或 `#-`）与 `#Strings` 流并读取三张表。
fn parse_metadata(metadata: &[u8]) -> Result<ParsedMetadata, CilError> {
    if read_u32(metadata, 0)? != 0x424A_5342 {
        return Err(parse_error("metadata root does not carry the BSJB signature"));
    }
    let version_length = read_u32(metadata, 12)? as usize;
    let version_end = 16usize
        .checked_add(version_length)
        .ok_or_else(|| parse_error("metadata version length overflows"))?;
    let flags = read_u16(metadata, version_end)?;
    let stream_count = read_u16(metadata, version_end + 2)? as usize;
    if flags > 1 {
        return Err(parse_error("metadata root declares unsupported flags"));
    }
    if stream_count == 0 {
        return Err(parse_error("metadata root declares no streams"));
    }

    let mut cursor = version_end
        .checked_add(4)
        .ok_or_else(|| parse_error("metadata stream header offset overflows"))?;
    let mut tables_stream = None;
    let mut strings_stream = None;
    let mut uncompressed_tables = None;

    for _ in 0..stream_count {
        let offset = read_u32(metadata, cursor)? as usize;
        let size = read_u32(metadata, cursor + 4)? as usize;
        cursor = cursor
            .checked_add(8)
            .ok_or_else(|| parse_error("metadata stream header offset overflows"))?;
        let name_start = cursor;
        loop {
            let byte = read_u8(metadata, cursor)?;
            cursor = cursor
                .checked_add(1)
                .ok_or_else(|| parse_error("metadata stream name offset overflows"))?;
            if byte == 0 {
                break;
            }
            if cursor - name_start > 32 {
                return Err(parse_error("metadata stream name is unterminated"));
            }
        }
        let name_length = cursor - name_start - 1;
        let name = &metadata[name_start..name_start + name_length];
        let padded = (name_length + 1 + 3) & !3;
        cursor = name_start
            .checked_add(padded)
            .ok_or_else(|| parse_error("metadata stream name padding overflows"))?;

        let end = offset
            .checked_add(size)
            .ok_or_else(|| parse_error("metadata stream extent overflows"))?;
        if end > metadata.len() {
            return Err(parse_error("metadata stream extent exceeds the metadata root"));
        }
        let range = StreamRange { offset, size };
        match name {
            b"#~" => tables_stream = Some(range),
            b"#-" => uncompressed_tables = Some(range),
            b"#Strings" => strings_stream = Some(range),
            _ => {}
        }
    }

    let tables_stream = tables_stream
        .or(uncompressed_tables)
        .ok_or_else(|| parse_error("metadata has no #~ or #- table stream"))?;
    let strings_stream =
        strings_stream.ok_or_else(|| parse_error("metadata has no #Strings heap"))?;

    let tables = metadata
        .get(tables_stream.offset..tables_stream.offset + tables_stream.size)
        .ok_or_else(|| parse_error("table stream exceeds the metadata root"))?;
    let strings = metadata
        .get(strings_stream.offset..strings_stream.offset + strings_stream.size)
        .ok_or_else(|| parse_error("#Strings stream exceeds the metadata root"))?;

    let layout = parse_table_layout(tables)?;
    let types = parse_types(tables, strings, &layout)?;
    let methods = parse_methods(tables, strings, &layout)?;
    let nested_classes = parse_nested_classes(tables, &layout)?;

    Ok(ParsedMetadata {
        types,
        methods,
        nested_classes,
    })
}

/// 读取 `#~` 头部、`valid` 位图与行数，并计算 0x00–0x29 每张表的行宽和行起点。
fn parse_table_layout(tables: &[u8]) -> Result<TableLayout, CilError> {
    if tables.len() < 24 {
        return Err(parse_error("table stream is shorter than its header"));
    }
    let heap_sizes = tables[6];
    let heap = HeapSizes {
        string: if heap_sizes & 0x01 != 0 { 4 } else { 2 },
        guid: if heap_sizes & 0x02 != 0 { 4 } else { 2 },
        blob: if heap_sizes & 0x04 != 0 { 4 } else { 2 },
    };
    let valid = read_u64(tables, 8)?;

    let mut rows = [0u32; 64];
    let mut cursor = 24usize;
    for table in 0..64usize {
        if valid & (1u64 << table) != 0 {
            rows[table] = read_u32(tables, cursor)?;
            cursor = cursor
                .checked_add(4)
                .ok_or_else(|| parse_error("metadata row count offset overflows"))?;
        }
    }
    if cursor > tables.len() {
        return Err(parse_error("metadata row counts exceed the table stream"));
    }

    let index = IndexSizes { rows, heap };
    let mut row_sizes = [0usize; 64];
    let mut offsets = [0usize; 64];
    for table in 0x00..=0x29u8 {
        let row_size = index.row_size(table);
        row_sizes[table as usize] = row_size;
        offsets[table as usize] = cursor;
        let extent = (rows[table as usize] as usize)
            .checked_mul(row_size)
            .ok_or_else(|| parse_error("metadata table extent overflows"))?;
        cursor = cursor
            .checked_add(extent)
            .ok_or_else(|| parse_error("metadata table extent overflows"))?;
        if cursor > tables.len() {
            return Err(parse_error("metadata table rows exceed the table stream"));
        }
    }

    Ok(TableLayout {
        rows,
        row_sizes,
        offsets,
        heap,
    })
}

/// `#Strings` 堆里按下标读取以 0 结尾的名字。
fn read_heap_string(heap: &[u8], index: u32) -> Result<String, CilError> {
    if index == 0 {
        return Ok(String::new());
    }
    let start = index as usize;
    if start >= heap.len() {
        return Err(parse_error("#Strings index is out of bounds"));
    }
    let mut end = start;
    while end < heap.len() && heap[end] != 0 {
        end += 1;
    }
    if end == heap.len() {
        return Err(parse_error("#Strings entry is not null terminated"));
    }
    std::str::from_utf8(&heap[start..end])
        .map(str::to_owned)
        .map_err(|_| parse_error("#Strings entry is not valid UTF-8"))
}

/// 读取一个 2 字节或 4 字节的小端索引。
fn read_index(bytes: &[u8], offset: usize, width: usize) -> Result<u32, CilError> {
    if width == 2 {
        Ok(u32::from(read_u16(bytes, offset)?))
    } else {
        read_u32(bytes, offset)
    }
}
/// 根据堆大小标志与各表行数计算索引宽度。
struct IndexSizes {
    rows: [u32; 64],
    heap: HeapSizes,
}

impl IndexSizes {
    /// String 堆索引宽度。
    fn string(&self) -> usize {
        self.heap.string
    }

    /// GUID 堆索引宽度。
    fn guid(&self) -> usize {
        self.heap.guid
    }

    /// Blob 堆索引宽度。
    fn blob(&self) -> usize {
        self.heap.blob
    }

    /// 简单表索引宽度：行数小于 65536 时为 2 字节。
    fn simple(&self, table: u8) -> usize {
        if self.rows[table as usize] < 0x1_0000 {
            2
        } else {
            4
        }
    }

    /// 编码索引宽度：由 tag 位宽与目标表最大行数决定。
    fn coded(&self, targets: &[u8], tag_bits: u32) -> usize {
        let mut max_rows = 0u32;
        for &table in targets {
            max_rows = max_rows.max(self.rows[table as usize]);
        }
        if u64::from(max_rows) < (1u64 << (16 - tag_bits)) {
            2
        } else {
            4
        }
    }

    fn type_def_or_ref(&self) -> usize {
        self.coded(&[0x02, 0x01, 0x1B], 2)
    }

    fn has_constant(&self) -> usize {
        self.coded(&[0x04, 0x08, 0x17], 2)
    }

    fn has_custom_attribute(&self) -> usize {
        self.coded(
            &[
                0x06, 0x04, 0x01, 0x02, 0x08, 0x09, 0x0A, 0x00, 0x0E, 0x17, 0x14, 0x11, 0x1A,
                0x1B, 0x20, 0x23, 0x26, 0x27, 0x28, 0x2A, 0x2C, 0x2B,
            ],
            5,
        )
    }

    fn has_field_marshal(&self) -> usize {
        self.coded(&[0x04, 0x08], 1)
    }

    fn has_decl_security(&self) -> usize {
        self.coded(&[0x02, 0x06, 0x20], 2)
    }

    fn member_ref_parent(&self) -> usize {
        self.coded(&[0x02, 0x01, 0x1A, 0x06, 0x1B], 3)
    }

    fn has_semantics(&self) -> usize {
        self.coded(&[0x14, 0x17], 1)
    }

    fn method_def_or_ref(&self) -> usize {
        self.coded(&[0x06, 0x0A], 1)
    }

    fn member_forwarded(&self) -> usize {
        self.coded(&[0x04, 0x06], 1)
    }

    fn implementation(&self) -> usize {
        self.coded(&[0x26, 0x23, 0x27], 2)
    }

    fn custom_attribute_type(&self) -> usize {
        self.coded(&[0x06, 0x0A], 3)
    }

    fn resolution_scope(&self) -> usize {
        self.coded(&[0x00, 0x1A, 0x23, 0x01], 2)
    }

    /// `TypeOrMethodDef` 编码索引宽度（GenericParam 相关列使用）。
    #[allow(dead_code)]
    fn type_or_method_def(&self) -> usize {
        self.coded(&[0x02, 0x06], 1)
    }

    /// 0x00–0x29 各表的完整行宽。
    fn row_size(&self, table: u8) -> usize {
        match table {
            0x00 => 2 + self.string() + self.guid() * 3,
            0x01 => self.resolution_scope() + self.string() * 2,
            0x02 => {
                4 + self.string() * 2
                    + self.type_def_or_ref()
                    + self.simple(0x04)
                    + self.simple(0x06)
            }
            0x03 => self.simple(0x04),
            0x04 => 2 + self.string() + self.blob(),
            0x05 => self.simple(0x06),
            0x06 => 4 + 2 + 2 + self.string() + self.blob() + self.simple(0x08),
            0x07 => self.simple(0x08),
            0x08 => 2 + 2 + self.string(),
            0x09 => self.simple(0x02) + self.type_def_or_ref(),
            0x0A => self.member_ref_parent() + self.string() + self.blob(),
            0x0B => 2 + self.has_constant() + self.blob(),
            0x0C => self.has_custom_attribute() + self.custom_attribute_type() + self.blob(),
            0x0D => self.has_field_marshal() + self.blob(),
            0x0E => 2 + self.has_decl_security() + self.blob(),
            0x0F => 2 + 4 + self.simple(0x02),
            0x10 => 4 + self.simple(0x04),
            0x11 => self.blob(),
            0x12 => self.simple(0x02) + self.simple(0x14),
            0x13 => self.simple(0x14),
            0x14 => 2 + self.string() + self.type_def_or_ref(),
            0x15 => self.simple(0x02) + self.simple(0x17),
            0x16 => self.simple(0x17),
            0x17 => 2 + self.string() + self.blob(),
            0x18 => 2 + self.simple(0x06) + self.has_semantics(),
            0x19 => self.simple(0x02) + self.method_def_or_ref() * 2,
            0x1A => self.string(),
            0x1B => self.blob(),
            0x1C => 2 + self.member_forwarded() + self.string() + self.simple(0x1A),
            0x1D => 4 + self.simple(0x04),
            0x1E => 4 + 4,
            0x1F => 4,
            0x20 => 4 + 8 + 4 + self.blob() + self.string() * 2,
            0x21 => 4,
            0x22 => 4 * 3,
            0x23 => 8 + 4 + self.blob() + self.string() * 2 + self.blob(),
            0x24 => 4 + self.simple(0x23),
            0x25 => 4 * 3 + self.simple(0x23),
            0x26 => 4 + self.string() + self.blob(),
            0x27 => 4 + 4 + self.string() * 2 + self.implementation(),
            0x28 => 4 + 4 + self.string() + self.implementation(),
            0x29 => self.simple(0x02) * 2,
            _ => 0,
        }
    }
}

/// 计算某张表第 `row` 行（0 基）在表流里的偏移。
fn row_offset(layout: &TableLayout, table: u8, row: usize) -> Result<usize, CilError> {
    let start = layout.offsets[table as usize];
    let size = layout.row_sizes[table as usize];
    row.checked_mul(size)
        .and_then(|delta| start.checked_add(delta))
        .ok_or_else(|| parse_error("metadata row offset overflows"))
}

/// 读取 TypeDef 表。
fn parse_types(
    tables: &[u8],
    strings: &[u8],
    layout: &TableLayout,
) -> Result<Vec<CilTypeDef>, CilError> {
    let index = IndexSizes {
        rows: layout.rows,
        heap: layout.heap,
    };
    let count = layout.rows[0x02] as usize;
    let mut types = Vec::with_capacity(count);
    for row in 0..count {
        let mut cursor = row_offset(layout, 0x02, row)?;
        let flags = read_u32(tables, cursor)?;
        cursor += 4;
        let name = read_heap_string(strings, read_index(tables, cursor, index.string())?)?;
        cursor += index.string();
        let namespace = read_heap_string(strings, read_index(tables, cursor, index.string())?)?;
        cursor += index.string();
        cursor += index.type_def_or_ref();
        cursor += index.simple(0x04);
        let method_list = read_index(tables, cursor, index.simple(0x06))?;
        types.push(CilTypeDef {
            rid: row as u32 + 1,
            namespace,
            name,
            flags,
            method_list,
        });
    }
    Ok(types)
}

/// 读取 MethodDef 表。
fn parse_methods(
    tables: &[u8],
    strings: &[u8],
    layout: &TableLayout,
) -> Result<Vec<CilMethodDef>, CilError> {
    let index = IndexSizes {
        rows: layout.rows,
        heap: layout.heap,
    };
    let count = layout.rows[0x06] as usize;
    let mut methods = Vec::with_capacity(count);
    for row in 0..count {
        let mut cursor = row_offset(layout, 0x06, row)?;
        let rva = read_u32(tables, cursor)?;
        cursor += 4;
        cursor += 2;
        cursor += 2;
        let name = read_heap_string(strings, read_index(tables, cursor, index.string())?)?;
        methods.push(CilMethodDef {
            rid: row as u32 + 1,
            rva,
            name,
        });
    }
    Ok(methods)
}

/// 读取 NestedClass 表。
fn parse_nested_classes(
    tables: &[u8],
    layout: &TableLayout,
) -> Result<Vec<CilNestedClass>, CilError> {
    let index = IndexSizes {
        rows: layout.rows,
        heap: layout.heap,
    };
    let count = layout.rows[0x29] as usize;
    let mut nested_classes = Vec::with_capacity(count);
    for row in 0..count {
        let mut cursor = row_offset(layout, 0x29, row)?;
        let nested_class = read_index(tables, cursor, index.simple(0x02))?;
        cursor += index.simple(0x02);
        let enclosing_class = read_index(tables, cursor, index.simple(0x02))?;
        nested_classes.push(CilNestedClass {
            nested_class,
            enclosing_class,
        });
    }
    Ok(nested_classes)
}

fn read_u8(bytes: &[u8], offset: usize) -> Result<u8, CilError> {
    bytes
        .get(offset)
        .copied()
        .ok_or_else(|| parse_error("unexpected end of image"))
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, CilError> {
    let end = offset
        .checked_add(2)
        .ok_or_else(|| parse_error("offset overflows"))?;
    let slice = bytes
        .get(offset..end)
        .ok_or_else(|| parse_error("unexpected end of image"))?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, CilError> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| parse_error("offset overflows"))?;
    let slice = bytes
        .get(offset..end)
        .ok_or_else(|| parse_error("unexpected end of image"))?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, CilError> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| parse_error("offset overflows"))?;
    let slice = bytes
        .get(offset..end)
        .ok_or_else(|| parse_error("unexpected end of image"))?;
    Ok(u64::from_le_bytes([
        slice[0], slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7],
    ]))
}
#[cfg(test)]
mod tests {
    use super::*;

    const SECTION_RVA: u32 = 0x2000;
    const SECTION_RAW: usize = 0x200;
    const IMAGE_LEN: usize = 0x1200;
    const METADATA_FILE_OFFSET: usize = 0x300;
    const TINY_BODY_RVA: u32 = 0x2A00;
    const FAT_BODY_RVA: u32 = 0x2A10;

    #[test]
    fn reads_core_tables_without_extra_prefix_rows() {
        let mut heap = StringHeap::new();
        let module_name = heap.intern("<Module>");
        let demo = heap.intern("Demo");
        let outer = heap.intern("Outer");
        let inner = heap.intern("Inner");
        let method_one = heap.intern("MethodOne");
        let method_two = heap.intern("MethodTwo");
        let method_three = heap.intern("MethodThree");

        let tables = vec![
            (0x00u8, vec![module_row(module_name)]),
            (
                0x02u8,
                vec![
                    type_def_row(0x0000_0000, module_name, 0, 0, 1, 1),
                    type_def_row(0x0000_0001, outer, demo, 0, 1, 2),
                    type_def_row(0x0000_0104, inner, demo, 2, 1, 3),
                ],
            ),
            (
                0x06u8,
                vec![
                    method_def_row(TINY_BODY_RVA, method_one),
                    method_def_row(FAT_BODY_RVA, method_two),
                    method_def_row(0, method_three),
                ],
            ),
            (0x29u8, vec![nested_row(3, 2)]),
        ];
        let image = build_image(&tables, &heap.bytes);
        let assembly = CilAssembly::from_bytes(image).expect("image should parse");

        let types = assembly.types();
        assert_eq!(types.len(), 3);
        assert_eq!(types[0].rid, 1);
        assert_eq!(types[0].name, "<Module>");
        assert_eq!(types[0].namespace, "");
        assert_eq!(types[0].flags, 0);
        assert_eq!(types[0].method_list, 1);
        assert_eq!(types[1].rid, 2);
        assert_eq!(types[1].name, "Outer");
        assert_eq!(types[1].namespace, "Demo");
        assert_eq!(types[1].flags, 1);
        assert_eq!(types[1].method_list, 2);
        assert_eq!(types[2].rid, 3);
        assert_eq!(types[2].name, "Inner");
        assert_eq!(types[2].flags, 0x0104);
        assert_eq!(types[2].method_list, 3);

        let methods = assembly.methods();
        assert_eq!(methods.len(), 3);
        assert_eq!(methods[0].rid, 1);
        assert_eq!(methods[0].name, "MethodOne");
        assert_eq!(methods[0].rva, TINY_BODY_RVA);
        assert_eq!(methods[1].rid, 2);
        assert_eq!(methods[1].name, "MethodTwo");
        assert_eq!(methods[1].rva, FAT_BODY_RVA);
        assert_eq!(methods[2].rid, 3);
        assert_eq!(methods[2].name, "MethodThree");
        assert_eq!(methods[2].rva, 0);

        assert_eq!(
            assembly.nested_classes(),
            &[CilNestedClass {
                nested_class: 3,
                enclosing_class: 2,
            }]
        );

        assert_eq!(assembly.method_body_size(TINY_BODY_RVA), Some(5));
        assert_eq!(assembly.method_body_size(FAT_BODY_RVA), Some(64));
        assert_eq!(assembly.method_body_size(0), None);
        assert_eq!(assembly.method_body_size(0xFFFF_FF00), None);
        assert_eq!(assembly.method_body_size(TINY_BODY_RVA + 1), None);
    }

    #[test]
    fn row_widths_cover_field_and_param_tables() {
        let mut heap = StringHeap::new();
        let module_name = heap.intern("<Module>");
        let demo = heap.intern("Demo");
        let outer = heap.intern("RowWidths");
        let method_name = heap.intern("Run");
        let second_method = heap.intern("Stop");
        let field_name = heap.intern("counter");
        let other_field = heap.intern("label");
        let param_name = heap.intern("value");

        let tables = vec![
            (0x00u8, vec![module_row(module_name)]),
            (
                0x02u8,
                vec![
                    type_def_row(0, module_name, 0, 0, 1, 1),
                    type_def_row(0x0000_0101, outer, demo, 0, 1, 2),
                ],
            ),
            (
                0x04u8,
                vec![field_row(field_name), field_row(other_field)],
            ),
            (
                0x06u8,
                vec![
                    method_def_row(TINY_BODY_RVA, method_name),
                    method_def_row(0, second_method),
                ],
            ),
            (
                0x08u8,
                vec![param_row(param_name, 1), param_row(param_name, 2)],
            ),
            (0x29u8, vec![nested_row(2, 1)]),
        ];
        let image = build_image(&tables, &heap.bytes);
        let assembly = CilAssembly::from_bytes(image).expect("image should parse");

        assert_eq!(assembly.types().len(), 2);
        assert_eq!(assembly.types()[1].name, "RowWidths");
        assert_eq!(assembly.types()[1].namespace, "Demo");
        assert_eq!(assembly.types()[1].method_list, 2);
        assert_eq!(assembly.methods().len(), 2);
        assert_eq!(assembly.methods()[0].name, "Run");
        assert_eq!(assembly.methods()[1].name, "Stop");
        assert_eq!(
            assembly.nested_classes(),
            &[CilNestedClass {
                nested_class: 2,
                enclosing_class: 1,
            }]
        );
        assert_eq!(assembly.method_body_size(TINY_BODY_RVA), Some(5));
    }

    #[test]
    fn truncated_images_return_parse_errors() {
        let mut heap = StringHeap::new();
        let module_name = heap.intern("<Module>");
        let method_name = heap.intern("Run");
        let tables = vec![
            (0x00u8, vec![module_row(module_name)]),
            (
                0x02u8,
                vec![type_def_row(0, module_name, 0, 0, 1, 1)],
            ),
            (0x06u8, vec![method_def_row(TINY_BODY_RVA, method_name)]),
            (0x29u8, vec![nested_row(1, 1)]),
        ];
        let image = build_image(&tables, &heap.bytes);

        for length in [0usize, 1, 2, 0x3C, 0x40, 0x80, 0x100, 0x1A0, 0x200, 0x250] {
            let result = CilAssembly::from_bytes(image[..length].to_vec());
            assert!(
                matches!(result, Err(CilError::Parse(_))),
                "truncating to {length:#x} should be a parse error"
            );
        }
        for length in [
            METADATA_FILE_OFFSET,
            METADATA_FILE_OFFSET + 8,
            METADATA_FILE_OFFSET + 32,
            METADATA_FILE_OFFSET + 48,
        ] {
            let result = CilAssembly::from_bytes(image[..length].to_vec());
            assert!(
                matches!(result, Err(CilError::Parse(_))),
                "truncating metadata to {length:#x} should be a parse error"
            );
        }
    }

    #[test]
    fn non_cli_bytes_return_parse_errors() {
        assert!(matches!(
            CilAssembly::from_bytes(Vec::new()),
            Err(CilError::Parse(_))
        ));
        assert!(matches!(
            CilAssembly::from_bytes(vec![0u8; 1024]),
            Err(CilError::Parse(_))
        ));

        let mut heap = StringHeap::new();
        let module_name = heap.intern("<Module>");
        let tables = vec![
            (0x00u8, vec![module_row(module_name)]),
            (
                0x02u8,
                vec![type_def_row(0, module_name, 0, 0, 1, 1)],
            ),
        ];
        let mut image = build_image(&tables, &heap.bytes);
        image[METADATA_FILE_OFFSET] ^= 0xFF;
        assert!(matches!(
            CilAssembly::from_bytes(image),
            Err(CilError::Parse(_))
        ));
    }

    #[test]
    fn missing_metadata_streams_return_parse_errors() {
        let mut heap = StringHeap::new();
        let module_name = heap.intern("<Module>");
        let tables = vec![
            (0x00u8, vec![module_row(module_name)]),
            (
                0x02u8,
                vec![type_def_row(0, module_name, 0, 0, 1, 1)],
            ),
        ];
        let structured = assemble(tables, heap.bytes);
        let image = patch_metadata(&structured);
        assert!(CilAssembly::from_bytes(image).is_ok());

        let mut bad = structured.clone();
        bad.truncate(bad.len() - 1);
        let image = patch_metadata(&bad);
        assert!(matches!(
            CilAssembly::from_bytes(image),
            Err(CilError::Parse(_))
        ));
    }

    /// 名字堆：第一字节固定为 0，保证下标 0 是空串。
    struct StringHeap {
        bytes: Vec<u8>,
        entries: Vec<(String, u32)>,
    }

    impl StringHeap {
        fn new() -> Self {
            Self {
                bytes: vec![0],
                entries: vec![(String::new(), 0)],
            }
        }

        fn intern(&mut self, value: &str) -> u32 {
            if let Some((_, offset)) = self.entries.iter().find(|(entry, _)| entry == value) {
                return *offset;
            }
            let offset = self.bytes.len() as u32;
            self.bytes.extend_from_slice(value.as_bytes());
            self.bytes.push(0);
            self.entries.push((value.to_owned(), offset));
            offset
        }
    }

    fn module_row(name: u32) -> Vec<u8> {
        let mut row = Vec::new();
        row.extend_from_slice(&0u16.to_le_bytes());
        row.extend_from_slice(&(name as u16).to_le_bytes());
        for _ in 0..3 {
            row.extend_from_slice(&0u16.to_le_bytes());
        }
        row
    }

    fn type_def_row(
        flags: u32,
        name: u32,
        namespace: u32,
        extends: u16,
        field_list: u16,
        method_list: u16,
    ) -> Vec<u8> {
        let mut row = Vec::new();
        row.extend_from_slice(&flags.to_le_bytes());
        row.extend_from_slice(&(name as u16).to_le_bytes());
        row.extend_from_slice(&(namespace as u16).to_le_bytes());
        row.extend_from_slice(&extends.to_le_bytes());
        row.extend_from_slice(&field_list.to_le_bytes());
        row.extend_from_slice(&method_list.to_le_bytes());
        row
    }

    fn method_def_row(rva: u32, name: u32) -> Vec<u8> {
        let mut row = Vec::new();
        row.extend_from_slice(&rva.to_le_bytes());
        row.extend_from_slice(&0u16.to_le_bytes());
        row.extend_from_slice(&0x0016u16.to_le_bytes());
        row.extend_from_slice(&(name as u16).to_le_bytes());
        row.extend_from_slice(&0u16.to_le_bytes());
        row.extend_from_slice(&1u16.to_le_bytes());
        row
    }

    fn field_row(name: u32) -> Vec<u8> {
        let mut row = Vec::new();
        row.extend_from_slice(&0x0001u16.to_le_bytes());
        row.extend_from_slice(&(name as u16).to_le_bytes());
        row.extend_from_slice(&0u16.to_le_bytes());
        row
    }

    fn param_row(name: u32, sequence: u16) -> Vec<u8> {
        let mut row = Vec::new();
        row.extend_from_slice(&0u16.to_le_bytes());
        row.extend_from_slice(&sequence.to_le_bytes());
        row.extend_from_slice(&(name as u16).to_le_bytes());
        row
    }

    fn nested_row(nested: u16, enclosing: u16) -> Vec<u8> {
        let mut row = Vec::new();
        row.extend_from_slice(&nested.to_le_bytes());
        row.extend_from_slice(&enclosing.to_le_bytes());
        row
    }

    /// 生成最小 `#~` 表流；`tables` 必须按表号升序。
    fn build_table_stream(tables: &[(u8, Vec<Vec<u8>>)]) -> Vec<u8> {
        let mut valid = 0u64;
        for (table, rows) in tables {
            assert!(*table < 64);
            assert!(!rows.is_empty());
            valid |= 1u64 << table;
        }

        let mut stream = Vec::new();
        stream.extend_from_slice(&0u32.to_le_bytes());
        stream.push(2);
        stream.push(0);
        stream.push(0);
        stream.push(1);
        stream.extend_from_slice(&valid.to_le_bytes());
        stream.extend_from_slice(&0u64.to_le_bytes());
        for (table, rows) in tables {
            assert_eq!(valid & (1u64 << table), 1u64 << table);
            stream.extend_from_slice(&(rows.len() as u32).to_le_bytes());
        }
        for (_, rows) in tables {
            for row in rows {
                stream.extend_from_slice(row);
            }
        }
        stream
    }

    /// 组装元数据根（两个流），再写入 PE 映像。
    fn build_image(tables: &[(u8, Vec<Vec<u8>>)], strings: &[u8]) -> Vec<u8> {
        let metadata = assemble(tables.to_vec(), strings.to_vec());
        patch_metadata(&metadata)
    }

    /// 只组装 `BSJB` 元数据根与两个流，不构造 PE。
    fn assemble(tables: Vec<(u8, Vec<Vec<u8>>)>, strings: Vec<u8>) -> Vec<u8> {
        let table_stream = build_table_stream(&tables);
        let version = b"v4.0.30319\0";
        let tilde_name = b"#~\0\0";
        let strings_name = b"#Strings\0\0\0";
        let header_length = 16 + version.len() + 4;
        let stream_header_length = 8 + tilde_name.len() + 8 + strings_name.len();
        let tilde_offset = header_length + stream_header_length;
        let strings_offset = align4(tilde_offset + table_stream.len());

        let mut metadata = Vec::new();
        metadata.extend_from_slice(&0x424A_5342u32.to_le_bytes());
        metadata.extend_from_slice(&1u16.to_le_bytes());
        metadata.extend_from_slice(&1u16.to_le_bytes());
        metadata.extend_from_slice(&0u32.to_le_bytes());
        metadata.extend_from_slice(&(version.len() as u32).to_le_bytes());
        metadata.extend_from_slice(version);
        metadata.extend_from_slice(&0u16.to_le_bytes());
        metadata.extend_from_slice(&2u16.to_le_bytes());
        metadata.extend_from_slice(&(tilde_offset as u32).to_le_bytes());
        metadata.extend_from_slice(&(table_stream.len() as u32).to_le_bytes());
        metadata.extend_from_slice(tilde_name);
        metadata.extend_from_slice(&(strings_offset as u32).to_le_bytes());
        metadata.extend_from_slice(&(strings.len() as u32).to_le_bytes());
        metadata.extend_from_slice(strings_name);
        assert_eq!(metadata.len(), tilde_offset);
        metadata.extend_from_slice(&table_stream);
        metadata.resize(strings_offset, 0);
        metadata.extend_from_slice(&strings);
        metadata
    }

    /// 把元数据放入一个 PE32 映像，并在节区里放 tiny/fat 两个方法体。
    fn patch_metadata(metadata: &[u8]) -> Vec<u8> {
        assert!(metadata.len() < 0x900);
        let mut image = vec![0u8; IMAGE_LEN];
        image[0] = b'M';
        image[1] = b'Z';
        put_u32(&mut image, 0x3C, 0x80);

        let pe = 0x80usize;
        image[pe..pe + 4].copy_from_slice(b"PE\0\0");
        let coff = pe + 4;
        put_u16(&mut image, coff, 0x014C);
        put_u16(&mut image, coff + 2, 1);
        put_u16(&mut image, coff + 16, 224);
        put_u16(&mut image, coff + 18, 0x0102);

        let optional = coff + 20;
        put_u16(&mut image, optional, 0x010B);
        put_u32(&mut image, optional + 60, 0x200);
        put_u32(&mut image, optional + 92, 16);
        put_u32(&mut image, optional + 96 + 14 * 8, SECTION_RVA);
        put_u32(&mut image, optional + 96 + 14 * 8 + 4, 72);

        let section = optional + 224;
        image[section..section + 8].copy_from_slice(b".text\0\0\0");
        put_u32(&mut image, section + 8, 0x1000);
        put_u32(&mut image, section + 12, SECTION_RVA);
        put_u32(&mut image, section + 16, 0x1000);
        put_u32(&mut image, section + 20, SECTION_RAW as u32);

        put_u32(&mut image, 0x200, 72);
        put_u16(&mut image, 0x204, 2);
        put_u16(&mut image, 0x206, 5);
        put_u32(&mut image, 0x208, 0x2100);
        put_u32(&mut image, 0x20C, metadata.len() as u32);
        put_u32(&mut image, 0x210, 1);

        image[METADATA_FILE_OFFSET..METADATA_FILE_OFFSET + metadata.len()]
            .copy_from_slice(metadata);

        let tiny = [0x16u8, 0x00, 0x00, 0x00, 0x00, 0x00];
        let tiny_offset = file_offset(TINY_BODY_RVA);
        image[tiny_offset..tiny_offset + tiny.len()].copy_from_slice(&tiny);

        let mut fat = Vec::new();
        fat.extend_from_slice(&0x3003u16.to_le_bytes());
        fat.extend_from_slice(&1u16.to_le_bytes());
        fat.extend_from_slice(&64u32.to_le_bytes());
        fat.extend_from_slice(&0u32.to_le_bytes());
        fat.extend_from_slice(&[0u8; 64]);
        let fat_offset = file_offset(FAT_BODY_RVA);
        image[fat_offset..fat_offset + fat.len()].copy_from_slice(&fat);

        image
    }

    fn file_offset(rva: u32) -> usize {
        SECTION_RAW + (rva - SECTION_RVA) as usize
    }

    fn align4(value: usize) -> usize {
        (value + 3) & !3
    }

    fn put_u16(image: &mut [u8], offset: usize, value: u16) {
        image[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(image: &mut [u8], offset: usize, value: u32) {
        image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
}
