//! `Mach-O` 头、加载命令、段与节的读取。
//!
//! 读取 `Mach-O` 32/64 位与大小端文件：固定头、`LC_SEGMENT` / `LC_SEGMENT_64`
//! 描述的段与节，以及 `LC_MAIN` 提供的入口文件偏移。符号表、重定位与
//! `dyld` 信息不在此模块范围内。

use std::fmt;

const MH_MAGIC: u32 = 0xFEED_FACE;
const MH_MAGIC_64: u32 = 0xFEED_FACF;
const FAT_MAGIC: u32 = 0xCAFE_BABE;
const FAT_MAGIC_64: u32 = 0xCAFE_BABF;

const LC_SEGMENT: u32 = 0x1;
const LC_SEGMENT_64: u32 = 0x19;
const LC_MAIN: u32 = 0x8000_0028;

/// 32 位头大小。
const HEADER32_SIZE: usize = 28;
/// 64 位头大小。
const HEADER64_SIZE: usize = 32;
/// 32 位段命令固定部分大小。
const SEGMENT32_SIZE: u64 = 56;
/// 64 位段命令固定部分大小。
const SEGMENT64_SIZE: u64 = 72;
/// 32 位节大小。
const SECTION32_SIZE: u64 = 68;
/// 64 位节大小。
const SECTION64_SIZE: u64 = 80;
/// 加载命令的最小长度。
const LOAD_COMMAND_MIN_SIZE: u32 = 8;

/// `Mach-O` 字节序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachOEndian {
    /// 小端。
    Little,
    /// 大端。
    Big,
}

/// `Mach-O` 读取错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MachOParseError {
    /// 文件短于固定头。
    TooShort,
    /// 四个魔数字节都不是 `Mach-O`。
    BadMagic,
    /// 通用（fat）二进制，需要外层切片后再解析。
    FatBinary,
    /// 读取越过文件末尾。
    OutOfRange {
        /// 读取起点。
        offset: u64,
        /// 读取长度。
        length: u64,
    },
    /// 加载命令长度字段小于 8。
    BadCommandSize {
        /// 命令序号。
        index: u32,
        /// 声明的 `cmdsize`。
        size: u32,
    },
    /// 加载命令越过 `sizeofcmds` 或文件末尾。
    CommandOutOfRange {
        /// 命令序号。
        index: u32,
        /// 命令起点。
        offset: u64,
        /// 声明的 `cmdsize`。
        size: u32,
    },
    /// 段命令长度小于该位宽的最小段头。
    SegmentHeaderTooSmall {
        /// 声明的 `cmdsize`。
        size: u32,
    },
    /// 段命令声明的节数量超出 `cmdsize`。
    SegmentSectionsOutOfRange {
        /// 段名。
        segment: String,
        /// 声明的节数量。
        count: u32,
        /// `cmdsize`。
        size: u32,
    },
}

impl fmt::Display for MachOParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort => write!(f, "Mach-O 镜像过短"),
            Self::BadMagic => write!(f, "缺少 `Mach-O` 魔数"),
            Self::FatBinary => write!(f, "通用（fat）Mach-O 需要先选择架构切片"),
            Self::OutOfRange { offset, length } => {
                write!(f, "读取 0x{offset:X} 处的 {length} 字节越过文件末尾")
            }
            Self::BadCommandSize { index, size } => {
                write!(f, "第 {index} 条加载命令长度为 {size} 字节，小于 {LOAD_COMMAND_MIN_SIZE} 字节下限")
            }
            Self::CommandOutOfRange { index, offset, size } => {
                write!(f, "第 {index} 条加载命令 0x{offset:X} 处的 {size} 字节超出命令区")
            }
            Self::SegmentHeaderTooSmall { size } => {
                write!(f, "段命令长度为 {size} 字节，小于该位宽的段头")
            }
            Self::SegmentSectionsOutOfRange { segment, count, size } => {
                write!(f, "段 {segment} 声明 {count} 个节，超出 {size} 字节的命令长度")
            }
        }
    }
}

impl std::error::Error for MachOParseError {}

/// 解析后的 `Mach-O` 固定头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachHeader {
    /// 是否为 64 位。
    pub is_64: bool,
    /// 字节序。
    pub endian: MachOEndian,
    /// `cputype`。
    pub cpu_type: i32,
    /// `cpusubtype`。
    pub cpu_subtype: i32,
    /// `filetype`。
    pub file_type: u32,
    /// `ncmds`。
    pub command_count: u32,
    /// `sizeofcmds`。
    pub size_of_commands: u32,
    /// `flags`。
    pub flags: u32,
}

/// 解析后的段（`LC_SEGMENT` / `LC_SEGMENT_64`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachSegment {
    /// 段名，已去掉尾部填充。
    pub name: String,
    /// `vmaddr`。
    pub vm_address: u64,
    /// `vmsize`。
    pub vm_size: u64,
    /// `fileoff`。
    pub file_offset: u64,
    /// `filesize`。
    pub file_size: u64,
    /// `maxprot`。
    pub max_protection: u32,
    /// `initprot`。
    pub initial_protection: u32,
}

/// 解析后的节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachSection {
    /// 所属段名。
    pub segment: String,
    /// 节名，已去掉尾部填充。
    pub name: String,
    /// `addr`。
    pub address: u64,
    /// `size`。
    pub size: u64,
    /// `offset`。
    pub file_offset: u64,
    /// `align`，以 2 的幂次表示。
    pub alignment: u32,
    /// `flags`。
    pub flags: u32,
}

/// 解析后的 `Mach-O` 镜像。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachOImage {
    /// 固定头。
    pub header: MachHeader,
    /// 段表，按加载命令顺序排列。
    pub segments: Vec<MachSegment>,
    /// 节表，按段与节在文件中的顺序排列。
    pub sections: Vec<MachSection>,
    /// `LC_MAIN` 给出的入口文件偏移。
    pub entry_offset: Option<u64>,
}

/// 识别 `Mach-O` 魔数，返回 `(is_64, endian)`。
///
/// 通用（fat）二进制不返回结果，调用方需要先选择架构切片。
pub fn macho_magic(bytes: &[u8]) -> Option<(bool, MachOEndian)> {
    match bytes.get(0..4)? {
        [0xcf, 0xfa, 0xed, 0xfe] => Some((true, MachOEndian::Little)),
        [0xce, 0xfa, 0xed, 0xfe] => Some((false, MachOEndian::Little)),
        [0xfe, 0xed, 0xfa, 0xcf] => Some((true, MachOEndian::Big)),
        [0xfe, 0xed, 0xfa, 0xce] => Some((false, MachOEndian::Big)),
        _ => None,
    }
}

/// 只解析固定头，不读取加载命令。
///
/// 该入口用于格式探测与头部摘要：`ncmds` 与 `sizeofcmds` 只做字段读取，
/// 不要求命令区完整。
pub fn parse_macho_header(bytes: &[u8]) -> Result<MachHeader, MachOParseError> {
    let (is_64, endian) = macho_magic(bytes).ok_or_else(|| {
        if is_fat(bytes) { MachOParseError::FatBinary } else { MachOParseError::BadMagic }
    })?;
    let header_size = if is_64 { HEADER64_SIZE } else { HEADER32_SIZE };
    if bytes.len() < header_size {
        return Err(MachOParseError::TooShort);
    }
    let reader = Reader { bytes, endian };
    Ok(MachHeader {
        is_64,
        endian,
        cpu_type: reader.u32(4)? as i32,
        cpu_subtype: reader.u32(8)? as i32,
        file_type: reader.u32(12)?,
        command_count: reader.u32(16)?,
        size_of_commands: reader.u32(20)?,
        flags: reader.u32(24)?,
    })
}

/// 解析固定头、加载命令、段表与节表。
///
/// 未知的加载命令按 `cmdsize` 跳过；命令区必须落在文件内，且每条命令的
/// `cmdsize` 不小于 8 字节且不越界。
pub fn parse_macho(bytes: &[u8]) -> Result<MachOImage, MachOParseError> {
    let header = parse_macho_header(bytes)?;
    let header_size = if header.is_64 { HEADER64_SIZE } else { HEADER32_SIZE } as u64;
    let commands_end = header_size + u64::from(header.size_of_commands);
    if commands_end > bytes.len() as u64 {
        return Err(MachOParseError::OutOfRange { offset: header_size, length: u64::from(header.size_of_commands) });
    }

    let reader = Reader { bytes, endian: header.endian };
    let mut image = MachOImage { header, segments: Vec::new(), sections: Vec::new(), entry_offset: None };
    let mut offset = header_size;
    for index in 0..image.header.command_count {
        if offset + u64::from(LOAD_COMMAND_MIN_SIZE) > commands_end {
            return Err(MachOParseError::CommandOutOfRange { index, offset, size: LOAD_COMMAND_MIN_SIZE });
        }
        let command = reader.u32(offset)?;
        let size = reader.u32(offset + 4)?;
        if size < LOAD_COMMAND_MIN_SIZE {
            return Err(MachOParseError::BadCommandSize { index, size });
        }
        if offset + u64::from(size) > commands_end {
            return Err(MachOParseError::CommandOutOfRange { index, offset, size });
        }

        match command {
            LC_SEGMENT => {
                let segment = reader.segment32(offset, size)?;
                reader.section32(&mut image.sections, offset, size, &segment.name)?;
                image.segments.push(segment);
            }
            LC_SEGMENT_64 => {
                let segment = reader.segment64(offset, size)?;
                reader.section64(&mut image.sections, offset, size, &segment.name)?;
                image.segments.push(segment);
            }
            LC_MAIN => {
                if size >= 24 {
                    image.entry_offset = Some(reader.u64(offset + 8)?);
                }
            }
            _ => {}
        }
        offset += u64::from(size);
    }
    Ok(image)
}

/// 是否为通用（fat）二进制。
fn is_fat(bytes: &[u8]) -> bool {
    match bytes.get(0..4) {
        Some(raw) => {
            let be = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
            let le = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
            be == FAT_MAGIC || be == FAT_MAGIC_64 || le == FAT_MAGIC || le == FAT_MAGIC_64
        }
        None => false,
    }
}

/// 按字节序读取的视图。
#[derive(Clone, Copy)]
struct Reader<'a> {
    bytes: &'a [u8],
    endian: MachOEndian,
}

impl<'a> Reader<'a> {
    fn slice(&self, offset: u64, length: u64) -> Result<&'a [u8], MachOParseError> {
        let end = offset.checked_add(length).ok_or(MachOParseError::OutOfRange { offset, length })?;
        if end > self.bytes.len() as u64 {
            return Err(MachOParseError::OutOfRange { offset, length });
        }
        Ok(&self.bytes[offset as usize..end as usize])
    }

    fn u32(&self, offset: u64) -> Result<u32, MachOParseError> {
        let raw = self.slice(offset, 4)?;
        let raw = [raw[0], raw[1], raw[2], raw[3]];
        Ok(match self.endian {
            MachOEndian::Little => u32::from_le_bytes(raw),
            MachOEndian::Big => u32::from_be_bytes(raw),
        })
    }

    fn u64(&self, offset: u64) -> Result<u64, MachOParseError> {
        let raw = self.slice(offset, 8)?;
        let raw = [raw[0], raw[1], raw[2], raw[3], raw[4], raw[5], raw[6], raw[7]];
        Ok(match self.endian {
            MachOEndian::Little => u64::from_le_bytes(raw),
            MachOEndian::Big => u64::from_be_bytes(raw),
        })
    }

    fn name(&self, offset: u64) -> Result<String, MachOParseError> {
        let raw = self.slice(offset, 16)?;
        let length = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
        Ok(String::from_utf8_lossy(&raw[..length]).trim_end().to_string())
    }

    fn segment32(&self, offset: u64, size: u32) -> Result<MachSegment, MachOParseError> {
        if u64::from(size) < SEGMENT32_SIZE {
            return Err(MachOParseError::SegmentHeaderTooSmall { size });
        }
        Ok(MachSegment {
            name: self.name(offset + 8)?,
            vm_address: u64::from(self.u32(offset + 24)?),
            vm_size: u64::from(self.u32(offset + 28)?),
            file_offset: u64::from(self.u32(offset + 32)?),
            file_size: u64::from(self.u32(offset + 36)?),
            max_protection: self.u32(offset + 40)?,
            initial_protection: self.u32(offset + 44)?,
        })
    }

    fn segment64(&self, offset: u64, size: u32) -> Result<MachSegment, MachOParseError> {
        if u64::from(size) < SEGMENT64_SIZE {
            return Err(MachOParseError::SegmentHeaderTooSmall { size });
        }
        Ok(MachSegment {
            name: self.name(offset + 8)?,
            vm_address: self.u64(offset + 24)?,
            vm_size: self.u64(offset + 32)?,
            file_offset: self.u64(offset + 40)?,
            file_size: self.u64(offset + 48)?,
            max_protection: self.u32(offset + 56)?,
            initial_protection: self.u32(offset + 60)?,
        })
    }

    fn section32(
        &self,
        sections: &mut Vec<MachSection>,
        offset: u64,
        size: u32,
        segment: &str,
    ) -> Result<(), MachOParseError> {
        let count = u64::from(self.u32(offset + 48)?);
        let first = offset + SEGMENT32_SIZE;
        if first + count * SECTION32_SIZE > offset + u64::from(size) {
            return Err(MachOParseError::SegmentSectionsOutOfRange {
                segment: segment.to_string(),
                count: count as u32,
                size,
            });
        }
        for index in 0..count {
            let base = first + index * SECTION32_SIZE;
            sections.push(MachSection {
                segment: self.name(base + 16)?,
                name: self.name(base)?,
                address: u64::from(self.u32(base + 32)?),
                size: u64::from(self.u32(base + 36)?),
                file_offset: u64::from(self.u32(base + 40)?),
                alignment: self.u32(base + 44)?,
                flags: self.u32(base + 56)?,
            });
        }
        Ok(())
    }

    fn section64(
        &self,
        sections: &mut Vec<MachSection>,
        offset: u64,
        size: u32,
        segment: &str,
    ) -> Result<(), MachOParseError> {
        let count = u64::from(self.u32(offset + 64)?);
        let first = offset + SEGMENT64_SIZE;
        if first + count * SECTION64_SIZE > offset + u64::from(size) {
            return Err(MachOParseError::SegmentSectionsOutOfRange {
                segment: segment.to_string(),
                count: count as u32,
                size,
            });
        }
        for index in 0..count {
            let base = first + index * SECTION64_SIZE;
            sections.push(MachSection {
                segment: self.name(base + 16)?,
                name: self.name(base)?,
                address: self.u64(base + 32)?,
                size: self.u64(base + 40)?,
                file_offset: u64::from(self.u32(base + 48)?),
                alignment: self.u32(base + 52)?,
                flags: self.u32(base + 64)?,
            });
        }
        Ok(())
    }
}
