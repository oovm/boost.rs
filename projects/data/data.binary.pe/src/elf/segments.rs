//! `ELF` 程序头表（段）解码。
//!
//! 只依赖 `e_ident` 与固定头，不读取可能损坏的节表。畸形表项会截断列表并置
//! `truncated`，而不是让整份报告失败。

use super::read::{Cursor, ElfParseError, parse_elf_header};

/// 一条已知 `p_type` 名称之外的类型值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfSegmentType {
    /// `PT_NULL`。
    Null,
    /// `PT_LOAD`。
    Load,
    /// `PT_DYNAMIC`。
    Dynamic,
    /// `PT_INTERP`。
    Interp,
    /// `PT_NOTE`。
    Note,
    /// `PT_SHLIB`。
    Shlib,
    /// `PT_PHDR`。
    Phdr,
    /// `PT_TLS`。
    Tls,
    /// `PT_GNU_EH_FRAME`。
    GnuEhFrame,
    /// `PT_GNU_STACK`。
    GnuStack,
    /// `PT_GNU_RELRO`。
    GnuRelro,
    /// `PT_GNU_PROPERTY`。
    GnuProperty,
    /// 未识别的 `p_type`。
    Other(u32),
}

impl ElfSegmentType {
    /// 从原始 `p_type` 映射。
    pub fn from_value(value: u32) -> Self {
        match value {
            0 => Self::Null,
            1 => Self::Load,
            2 => Self::Dynamic,
            3 => Self::Interp,
            4 => Self::Note,
            5 => Self::Shlib,
            6 => Self::Phdr,
            7 => Self::Tls,
            0x6474_e550 => Self::GnuEhFrame,
            0x6474_e551 => Self::GnuStack,
            0x6474_e552 => Self::GnuRelro,
            0x6474_e553 => Self::GnuProperty,
            other => Self::Other(other),
        }
    }

    /// 原始 `p_type`。
    pub fn value(self) -> u32 {
        match self {
            Self::Null => 0,
            Self::Load => 1,
            Self::Dynamic => 2,
            Self::Interp => 3,
            Self::Note => 4,
            Self::Shlib => 5,
            Self::Phdr => 6,
            Self::Tls => 7,
            Self::GnuEhFrame => 0x6474_e550,
            Self::GnuStack => 0x6474_e551,
            Self::GnuRelro => 0x6474_e552,
            Self::GnuProperty => 0x6474_e553,
            Self::Other(other) => other,
        }
    }

    /// 稳定名称，例如 `PT_LOAD`；未知值渲染为 `0x<hex>`。
    pub fn name(self) -> String {
        match self {
            Self::Null => "PT_NULL".to_string(),
            Self::Load => "PT_LOAD".to_string(),
            Self::Dynamic => "PT_DYNAMIC".to_string(),
            Self::Interp => "PT_INTERP".to_string(),
            Self::Note => "PT_NOTE".to_string(),
            Self::Shlib => "PT_SHLIB".to_string(),
            Self::Phdr => "PT_PHDR".to_string(),
            Self::Tls => "PT_TLS".to_string(),
            Self::GnuEhFrame => "PT_GNU_EH_FRAME".to_string(),
            Self::GnuStack => "PT_GNU_STACK".to_string(),
            Self::GnuRelro => "PT_GNU_RELRO".to_string(),
            Self::GnuProperty => "PT_GNU_PROPERTY".to_string(),
            Self::Other(other) => format!("0x{other:x}"),
        }
    }
}

/// 一条程序头表项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfSegment {
    /// 表内序号。
    pub index: usize,
    /// `p_type`。
    pub segment_type: ElfSegmentType,
    /// `p_offset`。
    pub offset: u64,
    /// `p_vaddr`。
    pub virtual_address: u64,
    /// `p_paddr`。
    pub physical_address: u64,
    /// `p_filesz`。
    pub file_size: u64,
    /// `p_memsz`。
    pub memory_size: u64,
    /// `p_flags`。
    pub flags: u32,
    /// `p_align`。
    pub alignment: u64,
}

impl ElfSegment {
    /// 是否可读（`PF_R`）。
    pub fn readable(&self) -> bool {
        self.flags & 4 != 0
    }

    /// 是否可写（`PF_W`）。
    pub fn writable(&self) -> bool {
        self.flags & 2 != 0
    }

    /// 是否可执行（`PF_X`）。
    pub fn executable(&self) -> bool {
        self.flags & 1 != 0
    }

    /// `p_flags` 渲染为 `R` / `W` / `X` 的组合。
    pub fn flag_text(&self) -> String {
        let mut text = String::with_capacity(3);
        if self.readable() {
            text.push('R');
        }
        if self.writable() {
            text.push('W');
        }
        if self.executable() {
            text.push('X');
        }
        text
    }
}

/// 程序头表摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ElfProgramHeaders {
    /// 是否为 64 位程序头布局。
    pub is_64: bool,
    /// 头部声明的表项数量。
    pub total: usize,
    /// 成功读取的表项，按表内顺序。
    pub segments: Vec<ElfSegment>,
    /// 是否有表项因越界或尾随字节被截断。
    pub truncated: bool,
}

/// 读取 `ELF32` / `ELF64` 的程序头表。
///
/// 没有程序头表（`e_phoff == 0` 或 `e_phnum == 0`）时返回 `present == false`
/// 的空摘要；表项越界时停止并置 `truncated`。
pub fn read_program_headers(bytes: &[u8]) -> Result<ElfProgramHeaders, ElfParseError> {
    let header = parse_elf_header(bytes)?;
    let is_64 = header.class.is_64();
    let mut report = ElfProgramHeaders { is_64, total: 0, segments: Vec::new(), truncated: false };
    if header.program_offset == 0 || header.program_count == 0 || header.program_entry_size == 0 {
        return Ok(report);
    }
    let minimum = if is_64 { 56 } else { 32 };
    if header.program_entry_size < minimum {
        return Err(ElfParseError::ProgramHeaderEntryTooSmall { size: header.program_entry_size });
    }
    report.total = header.program_count;
    let table_length = u64::from(header.program_entry_size) * header.program_count as u64;
    if header.program_offset.saturating_add(table_length) > bytes.len() as u64 {
        report.truncated = true;
    }

    let cursor = Cursor::new(bytes, header.endian);
    for index in 0..header.program_count {
        let base = header.program_offset + index as u64 * u64::from(header.program_entry_size);
        let Ok(segment) = read_segment(&cursor, is_64, index, base) else {
            report.truncated = true;
            break;
        };
        report.segments.push(segment);
    }
    Ok(report)
}

/// 读取一条程序头表项。
pub(crate) fn read_segment(
    cursor: &Cursor<'_>,
    is_64: bool,
    index: usize,
    base: u64,
) -> Result<ElfSegment, ElfParseError> {
    if is_64 {
        Ok(ElfSegment {
            index,
            segment_type: ElfSegmentType::from_value(cursor.u32(base)?),
            offset: cursor.u64(base + 8)?,
            virtual_address: cursor.u64(base + 16)?,
            physical_address: cursor.u64(base + 24)?,
            file_size: cursor.u64(base + 32)?,
            memory_size: cursor.u64(base + 40)?,
            flags: cursor.u32(base + 4)?,
            alignment: cursor.u64(base + 48)?,
        })
    }
    else {
        Ok(ElfSegment {
            index,
            segment_type: ElfSegmentType::from_value(cursor.u32(base)?),
            offset: u64::from(cursor.u32(base + 4)?),
            virtual_address: u64::from(cursor.u32(base + 8)?),
            physical_address: u64::from(cursor.u32(base + 12)?),
            file_size: u64::from(cursor.u32(base + 16)?),
            memory_size: u64::from(cursor.u32(base + 20)?),
            flags: cursor.u32(base + 24)?,
            alignment: u64::from(cursor.u32(base + 28)?),
        })
    }
}
