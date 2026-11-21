use std::fmt;

use super::layout::ELF_EHDR_SIZE;

/// `SHN_XINDEX`：节名表索引需要从 0 号节的 `sh_link` 读取。
const SHN_XINDEX: u16 = 0xffff;
/// `ELF32` 节头大小。
const ELF32_SHDR_SIZE: u16 = 40;
/// `ELF64` 节头大小。
const ELF64_SHDR_SIZE: u16 = 64;

/// `ELF64` 可执行文件解析错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Elf64ParseError {
    /// 镜像过短。
    TooShort,
    /// `ELF` 魔数错误。
    BadMagic,
    /// 不是 64 位 `ELF`。
    NotElf64,
    /// 文件类型字段。
    FileType(u16),
    /// 机器类型字段。
    Machine(u16),
}

impl fmt::Display for Elf64ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort => write!(f, "ELF 镜像过短"),
            Self::BadMagic => write!(f, "ELF 镜像缺少 `\\x7fELF` 魔数"),
            Self::NotElf64 => write!(f, "ELF 镜像不是 64 位"),
            Self::FileType(kind) => write!(f, "ELF 镜像类型 0x{kind:04X}"),
            Self::Machine(machine) => write!(f, "ELF 镜像机器类型 0x{machine:04X}"),
        }
    }
}

impl std::error::Error for Elf64ParseError {}

/// 解析后的 `ELF64` 头摘要。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elf64Header {
    /// `e_type`。
    pub file_type: u16,
    /// `e_machine`。
    pub machine: u16,
}

/// 解析 `ELF64` 可执行文件头。
pub fn parse_elf64(bytes: &[u8]) -> Result<Elf64Header, Elf64ParseError> {
    if bytes.len() < ELF_EHDR_SIZE as usize {
        return Err(Elf64ParseError::TooShort);
    }
    if &bytes[0..4] != b"\x7fELF" {
        return Err(Elf64ParseError::BadMagic);
    }
    if bytes[4] != 2 {
        return Err(Elf64ParseError::NotElf64);
    }
    let file_type = u16::from_le_bytes(bytes[16..18].try_into().expect("slice"));
    let machine = u16::from_le_bytes(bytes[18..20].try_into().expect("slice"));
    Ok(Elf64Header { file_type, machine })
}

// ============================================================================
// 完整 ELF 头与节表读取
// ============================================================================

/// `ELF` 位宽。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfClass {
    /// 32 位。
    Elf32,
    /// 64 位。
    Elf64,
}

impl ElfClass {
    /// 是否为 64 位。
    pub fn is_64(self) -> bool {
        matches!(self, Self::Elf64)
    }

    /// 该位宽下的节头大小。
    fn section_header_size(self) -> u16 {
        match self {
            Self::Elf32 => ELF32_SHDR_SIZE,
            Self::Elf64 => ELF64_SHDR_SIZE,
        }
    }
}

impl fmt::Display for ElfClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Elf32 => "ELF32",
            Self::Elf64 => "ELF64",
        })
    }
}

/// `ELF` 字节序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfEndian {
    /// 小端。
    Little,
    /// 大端。
    Big,
}

/// `ELF` 头与节表解析错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElfParseError {
    /// 文件短于固定头。
    TooShort,
    /// 缺少 `\x7fELF` 魔数。
    BadMagic,
    /// `EI_CLASS` 不是 1（`ELF32`）或 2（`ELF64`）。
    UnsupportedClass(u8),
    /// `EI_DATA` 不是 1（小端）或 2（大端）。
    UnsupportedEncoding(u8),
    /// 读取越过文件末尾。
    OutOfRange {
        /// 读取起点。
        offset: u64,
        /// 读取长度。
        length: u64,
    },
    /// 节头表项小于该位宽的最小尺寸。
    SectionEntryTooSmall {
        /// `e_shentsize`。
        size: u16,
    },
    /// 节头表本身越界。
    SectionTableOutOfRange {
        /// `e_shoff`。
        offset: u64,
        /// 表总长度。
        length: u64,
    },
    /// 节名表索引超出节数量。
    SectionNameTableOutOfRange {
        /// `e_shstrndx`。
        index: usize,
        /// 节数量。
        count: usize,
    },
    /// 节名表越界。
    SectionNameTableOutOfRangeBytes {
        /// 节名表文件偏移。
        offset: u64,
        /// 节名表长度。
        length: u64,
    },
}

impl fmt::Display for ElfParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort => write!(f, "ELF 镜像过短"),
            Self::BadMagic => write!(f, "ELF 镜像缺少 `\\x7fELF` 魔数"),
            Self::UnsupportedClass(class) => write!(f, "ELF 位宽字段 `EI_CLASS` 为 {class}"),
            Self::UnsupportedEncoding(encoding) => write!(f, "ELF 字节序字段 `EI_DATA` 为 {encoding}"),
            Self::OutOfRange { offset, length } => {
                write!(f, "读取 0x{offset:X} 处的 {length} 字节越过文件末尾")
            }
            Self::SectionEntryTooSmall { size } => write!(f, "节头表项为 {size} 字节，小于该位宽的下限"),
            Self::SectionTableOutOfRange { offset, length } => {
                write!(f, "节头表 0x{offset:X} 处的 {length} 字节越过文件末尾")
            }
            Self::SectionNameTableOutOfRange { index, count } => {
                write!(f, "节名表索引 {index} 超出 {count} 个节")
            }
            Self::SectionNameTableOutOfRangeBytes { offset, length } => {
                write!(f, "节名表 0x{offset:X} 处的 {length} 字节越过文件末尾")
            }
        }
    }
}

impl std::error::Error for ElfParseError {}

/// 解析后的 `ELF` 文件头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElfHeader {
    /// 位宽。
    pub class: ElfClass,
    /// 字节序。
    pub endian: ElfEndian,
    /// `EI_OSABI`。
    pub os_abi: u8,
    /// `e_type`。
    pub file_type: u16,
    /// `e_machine`。
    pub machine: u16,
    /// `e_version`。
    pub version: u32,
    /// `e_entry`。
    pub entry: u64,
    /// `e_flags`。
    pub flags: u32,
    /// `e_ehsize`。
    pub header_size: u16,
    /// `e_shoff`。
    pub section_offset: u64,
    /// `e_shentsize`。
    pub section_entry_size: u16,
    /// 节数量（已展开 `e_shnum == 0` 的扩展编号）。
    pub section_count: usize,
    /// 节名表索引（已展开 `SHN_XINDEX`）。
    pub section_name_index: usize,
}

/// 解析后的 `ELF` 节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfSection {
    /// 节名；无法解析时为空串。
    pub name: String,
    /// `sh_type`。
    pub section_type: u32,
    /// `sh_flags`。
    pub flags: u64,
    /// `sh_addr`。
    pub address: u64,
    /// `sh_offset`。
    pub offset: u64,
    /// `sh_size`。
    pub size: u64,
    /// `sh_link`。
    pub link: u32,
    /// `sh_info`。
    pub info: u32,
    /// `sh_addralign`。
    pub alignment: u64,
    /// `sh_entsize`。
    pub entry_size: u64,
}

/// 解析后的 `ELF` 镜像。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfImage {
    /// 文件头。
    pub header: ElfHeader,
    /// 节表，按文件中的顺序排列；无节表时为空。
    pub sections: Vec<ElfSection>,
}

/// 解析 `ELF32` / `ELF64`（大端与小端）的头和节表。
///
/// 支持 `e_shnum == 0` 与 `SHN_XINDEX` 两种扩展编号：节数量与节名表索引分别从
/// 0 号节的 `sh_size` 与 `sh_link` 读取。节名在解析阶段就按 `shstrtab` 展开，
/// 越界的节名退化为空串而不是让整次解析失败。
pub fn parse_elf(bytes: &[u8]) -> Result<ElfImage, ElfParseError> {
    if bytes.len() < 16 {
        return Err(ElfParseError::TooShort);
    }
    if &bytes[0..4] != b"\x7fELF" {
        return Err(ElfParseError::BadMagic);
    }
    let class = match bytes[4] {
        1 => ElfClass::Elf32,
        2 => ElfClass::Elf64,
        other => return Err(ElfParseError::UnsupportedClass(other)),
    };
    let endian = match bytes[5] {
        1 => ElfEndian::Little,
        2 => ElfEndian::Big,
        other => return Err(ElfParseError::UnsupportedEncoding(other)),
    };
    let cursor = Cursor { bytes, endian };

    let minimum_header = if class.is_64() { 64usize } else { 52usize };
    if bytes.len() < minimum_header {
        return Err(ElfParseError::TooShort);
    }
    let (section_offset, section_entry_size, raw_count, raw_name_index, entry) = if class.is_64() {
        (
            cursor.u64(40)?,
            cursor.u16(58)?,
            cursor.u16(60)?,
            cursor.u16(62)?,
            cursor.u64(24)?,
        )
    }
    else {
        (
            u64::from(cursor.u32(32)?),
            cursor.u16(46)?,
            cursor.u16(48)?,
            cursor.u16(50)?,
            u64::from(cursor.u32(24)?),
        )
    };

    let mut header = ElfHeader {
        class,
        endian,
        os_abi: bytes[7],
        file_type: cursor.u16(16)?,
        machine: cursor.u16(18)?,
        version: cursor.u32(20)?,
        entry,
        flags: if class.is_64() { cursor.u32(48)? } else { cursor.u32(36)? },
        header_size: if class.is_64() { cursor.u16(52)? } else { cursor.u16(40)? },
        section_offset,
        section_entry_size,
        section_count: raw_count as usize,
        section_name_index: raw_name_index as usize,
    };

    if section_offset == 0 {
        header.section_count = 0;
        header.section_name_index = 0;
        return Ok(ElfImage { header, sections: Vec::new() });
    }
    if section_entry_size < class.section_header_size() {
        return Err(ElfParseError::SectionEntryTooSmall { size: section_entry_size });
    }

    // `e_shnum == 0` 时真实数量在 0 号节的 `sh_size`；`SHN_XINDEX` 时真实索引在 `sh_link`。
    let mut count = raw_count as usize;
    if count == 0 || raw_name_index == SHN_XINDEX {
        let first = read_section(&cursor, class, section_offset, section_entry_size, 0)?;
        if count == 0 {
            count = usize::try_from(first.size).map_err(|_| ElfParseError::SectionTableOutOfRange {
                offset: section_offset,
                length: first.size,
            })?;
        }
        if raw_name_index == SHN_XINDEX {
            header.section_name_index = first.link as usize;
        }
    }
    let table_length = u64::from(section_entry_size) * count as u64;
    if section_offset.saturating_add(table_length) > bytes.len() as u64 {
        return Err(ElfParseError::SectionTableOutOfRange { offset: section_offset, length: table_length });
    }

    let mut sections = Vec::with_capacity(count);
    let mut name_offsets = Vec::with_capacity(count);
    for index in 0..count {
        let base = section_offset + index as u64 * u64::from(section_entry_size);
        name_offsets.push(cursor.u32(base)?);
        sections.push(read_section(&cursor, class, section_offset, section_entry_size, index as u64)?);
    }

    let names = if header.section_name_index == 0 {
        &[][..]
    }
    else {
        let name_section = sections
            .get(header.section_name_index)
            .ok_or(ElfParseError::SectionNameTableOutOfRange { index: header.section_name_index, count })?;
        cursor
            .slice(name_section.offset, name_section.size)
            .map_err(|_| ElfParseError::SectionNameTableOutOfRangeBytes {
                offset: name_section.offset,
                length: name_section.size,
            })?
    };
    for (section, name_offset) in sections.iter_mut().zip(name_offsets) {
        section.name = read_name(names, name_offset as usize);
    }

    header.section_count = sections.len();
    Ok(ElfImage { header, sections })
}

/// 按位宽与字节序读取的视图。
#[derive(Clone, Copy)]
struct Cursor<'a> {
    bytes: &'a [u8],
    endian: ElfEndian,
}

impl<'a> Cursor<'a> {
    fn slice(&self, offset: u64, length: u64) -> Result<&'a [u8], ElfParseError> {
        let end = offset.checked_add(length).ok_or(ElfParseError::OutOfRange { offset, length })?;
        if end > self.bytes.len() as u64 {
            return Err(ElfParseError::OutOfRange { offset, length });
        }
        Ok(&self.bytes[offset as usize..end as usize])
    }

    fn u16(&self, offset: u64) -> Result<u16, ElfParseError> {
        let raw = self.slice(offset, 2)?;
        Ok(match self.endian {
            ElfEndian::Little => u16::from_le_bytes([raw[0], raw[1]]),
            ElfEndian::Big => u16::from_be_bytes([raw[0], raw[1]]),
        })
    }

    fn u32(&self, offset: u64) -> Result<u32, ElfParseError> {
        let raw = self.slice(offset, 4)?;
        let raw = [raw[0], raw[1], raw[2], raw[3]];
        Ok(match self.endian {
            ElfEndian::Little => u32::from_le_bytes(raw),
            ElfEndian::Big => u32::from_be_bytes(raw),
        })
    }

    fn u64(&self, offset: u64) -> Result<u64, ElfParseError> {
        let raw = self.slice(offset, 8)?;
        let raw = [raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7]];
        Ok(match self.endian {
            ElfEndian::Little => u64::from_le_bytes(raw),
            ElfEndian::Big => u64::from_be_bytes(raw),
        })
    }
}

/// 读取一个节头；`name` 字段留空，由调用方按节名表补全。
fn read_section(
    cursor: &Cursor<'_>,
    class: ElfClass,
    table_offset: u64,
    entry_size: u16,
    index: u64,
) -> Result<ElfSection, ElfParseError> {
    let base = table_offset + index * u64::from(entry_size);
    cursor.slice(base, u64::from(entry_size))?;
    let mut section = ElfSection {
        name: String::new(),
        section_type: cursor.u32(base + 4)?,
        flags: 0,
        address: 0,
        offset: 0,
        size: 0,
        link: 0,
        info: 0,
        alignment: 0,
        entry_size: 0,
    };
    if class.is_64() {
        section.flags = cursor.u64(base + 8)?;
        section.address = cursor.u64(base + 16)?;
        section.offset = cursor.u64(base + 24)?;
        section.size = cursor.u64(base + 32)?;
        section.link = cursor.u32(base + 40)?;
        section.info = cursor.u32(base + 44)?;
        section.alignment = cursor.u64(base + 48)?;
        section.entry_size = cursor.u64(base + 56)?;
    }
    else {
        section.flags = u64::from(cursor.u32(base + 8)?);
        section.address = u64::from(cursor.u32(base + 12)?);
        section.offset = u64::from(cursor.u32(base + 16)?);
        section.size = u64::from(cursor.u32(base + 20)?);
        section.link = cursor.u32(base + 24)?;
        section.info = cursor.u32(base + 28)?;
        section.alignment = u64::from(cursor.u32(base + 32)?);
        section.entry_size = u64::from(cursor.u32(base + 36)?);
    }
    Ok(section)
}

/// 读取以 `0` 结尾的节名；偏移越界时返回空串。
fn read_name(names: &[u8], offset: usize) -> String {
    let Some(tail) = names.get(offset..)
    else {
        return String::new();
    };
    let length = tail.iter().position(|byte| *byte == 0).unwrap_or(tail.len());
    String::from_utf8_lossy(&tail[..length]).into_owned()
}
