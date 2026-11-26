//! `Mach-O` 头、加载命令、段与节的读取。
//!
//! 读取 `Mach-O` 32/64 位与大小端文件：固定头、`LC_SEGMENT` / `LC_SEGMENT_64`
//! 描述的段与节、`LC_MAIN` 提供的入口文件偏移，以及 `LC_SYMTAB` 符号表。
//! 重定位与 `dyld` 绑定信息不在此模块范围内。

use std::fmt;

use super::encryption::MachEncryptionInfo;
use super::identity::{MachBuildInfo, MachBuildSource, MachPlatform, uuid_string};
use super::linkedit::MachLinkeditData;

const FAT_MAGIC: u32 = 0xCAFE_BABE;
const FAT_MAGIC_64: u32 = 0xCAFE_BABF;

const LC_SEGMENT: u32 = 0x1;
const LC_SEGMENT_64: u32 = 0x19;
const LC_SYMTAB: u32 = 0x2;
const LC_MAIN: u32 = 0x8000_0028;
const LC_LOAD_DYLIB: u32 = 0xC;
const LC_LOAD_WEAK_DYLIB: u32 = 0x8000_0018;
const LC_REEXPORT_DYLIB: u32 = 0x8000_001F;
const LC_LOAD_UPWARD_DYLIB: u32 = 0x8000_0023;
const LC_ID_DYLIB: u32 = 0xD;
const LC_LAZY_LOAD_DYLIB: u32 = 0x20;
const LC_DYLD_INFO: u32 = 0x22;
const LC_DYLD_INFO_ONLY: u32 = 0x8000_0022;
const LC_UUID: u32 = 0x1B;
const LC_BUILD_VERSION: u32 = 0x32;
const LC_VERSION_MIN_MACOSX: u32 = 0x24;
const LC_VERSION_MIN_IPHONEOS: u32 = 0x25;
const LC_VERSION_MIN_TVOS: u32 = 0x2F;
const LC_VERSION_MIN_WATCHOS: u32 = 0x30;
const LC_ENCRYPTION_INFO: u32 = 0x21;
const LC_ENCRYPTION_INFO_64: u32 = 0x2C;
const LC_FUNCTION_STARTS: u32 = 0x26;
const LC_DYLD_EXPORTS_TRIE: u32 = 0x8000_0033;
const LC_DYLD_CHAINED_FIXUPS: u32 = 0x8000_0034;
const LC_CODE_SIGNATURE: u32 = 0x1D;

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
    /// `dyld` 绑定流中出现未知或不允许的操作码。
    BadBindOpcode {
        /// 操作码在文件中的偏移。
        offset: u64,
        /// 操作码字节。
        opcode: u8,
    },
    /// `dyld` 绑定流在设置符号前触发了绑定。
    BindWithoutSymbol {
        /// 操作码在文件中的偏移。
        offset: u64,
    },
    /// 绑定流引用了不存在的段。
    BindSegmentOutOfRange {
        /// 段序号。
        index: u8,
    },
    /// 输入不是通用（fat）容器。
    NotFatBinary,
    /// fat 架构表声明的切片数量超出上限。
    FatSliceLimit {
        /// 声明的切片数量。
        declared: u32,
        /// 允许的上限。
        limit: usize,
    },
    /// fat 架构表本身被截断。
    FatTableTruncated {
        /// 声明的切片数量。
        declared: u32,
        /// 文件中可用的字节数。
        available: usize,
    },
    /// fat 切片落在文件之外。
    FatSliceOutOfRange {
        /// 切片序号。
        index: usize,
        /// 声明的文件偏移。
        offset: u64,
        /// 声明的大小。
        size: u64,
    },
    /// 镜像没有 `LC_FUNCTION_STARTS` 命令。
    FunctionStartsMissing,
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
            Self::BadBindOpcode { offset, opcode } => {
                write!(f, "0x{offset:X} 处的 `dyld` 绑定操作码 0x{opcode:02X} 无效")
            }
            Self::BindWithoutSymbol { offset } => {
                write!(f, "0x{offset:X} 处的绑定在设置符号之前触发")
            }
            Self::BindSegmentOutOfRange { index } => {
                write!(f, "绑定流引用了不存在的段 {index}")
            }
            Self::NotFatBinary => write!(f, "输入不是通用（fat）Mach-O 容器"),
            Self::FatSliceLimit { declared, limit } => {
                write!(f, "fat 头声明 {declared} 个切片，超出 {limit} 条上限")
            }
            Self::FatTableTruncated { declared, available } => {
                write!(f, "fat 架构表声明 {declared} 个切片，但只剩 {available} 字节")
            }
            Self::FatSliceOutOfRange { index, offset, size } => {
                write!(f, "fat 切片 {index}（0x{offset:X}+{size}）落在文件之外")
            }
            Self::FunctionStartsMissing => write!(f, "镜像没有 `LC_FUNCTION_STARTS` 命令"),
        }
    }
}

impl MachOParseError {
    /// 稳定的机器可读错误码；不随发布变化，也不含消息文本。
    pub fn code(&self) -> &'static str {
        match self {
            Self::TooShort => "macho/too-short",
            Self::BadMagic => "macho/bad-magic",
            Self::FatBinary => "macho/fat-binary",
            Self::OutOfRange { .. } => "macho/out-of-range",
            Self::BadCommandSize { .. } => "macho/bad-command-size",
            Self::CommandOutOfRange { .. } => "macho/command-out-of-range",
            Self::SegmentHeaderTooSmall { .. } => "macho/segment-header-too-small",
            Self::SegmentSectionsOutOfRange { .. } => "macho/segment-sections-out-of-range",
            Self::BadBindOpcode { .. } => "macho/bad-bind-opcode",
            Self::BindWithoutSymbol { .. } => "macho/bind-without-symbol",
            Self::BindSegmentOutOfRange { .. } => "macho/bind-segment-out-of-range",
            Self::NotFatBinary => "macho/not-fat-binary",
            Self::FatSliceLimit { .. } => "macho/fat-slice-limit",
            Self::FatTableTruncated { .. } => "macho/fat-table-truncated",
            Self::FatSliceOutOfRange { .. } => "macho/fat-slice-out-of-range",
            Self::FunctionStartsMissing => "macho/function-starts-missing",
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

/// `LC_SYMTAB` 指向的符号表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachSymtab {
    /// 符号表文件偏移。
    pub symbol_offset: u32,
    /// 符号数量。
    pub symbol_count: u32,
    /// 字符串表文件偏移。
    pub string_offset: u32,
    /// 字符串表长度。
    pub string_size: u32,
}

/// 一条动态库加载命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachDylibKind {
    /// `LC_LOAD_DYLIB`。
    Load,
    /// `LC_LOAD_WEAK_DYLIB`。
    Weak,
    /// `LC_REEXPORT_DYLIB`。
    Reexport,
    /// `LC_LOAD_UPWARD_DYLIB`。
    Upward,
    /// `LC_LAZY_LOAD_DYLIB`。
    Lazy,
    /// `LC_ID_DYLIB`，不作为绑定序号。
    Id,
}

impl MachDylibKind {
    /// 报告中使用的稳定小写名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::Weak => "weak",
            Self::Reexport => "reexport",
            Self::Upward => "upward",
            Self::Lazy => "lazy",
            Self::Id => "id",
        }
    }

    /// 是否占用 `dyld` 绑定序号。
    pub fn has_ordinal(self) -> bool {
        !matches!(self, Self::Id)
    }
}

/// 一条动态库加载命令（`LC_LOAD_DYLIB` 家族）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachDylib {
    /// 安装名或路径。
    pub name: String,
    /// 命令种类。
    pub kind: MachDylibKind,
    /// 链接器记录的时间戳。
    pub timestamp: u32,
    /// `current_version`，编码为 `x.y.z`。
    pub current_version: u32,
    /// `compatibility_version`。
    pub compatibility_version: u32,
    /// 绑定序号（1 起，按加载顺序）；`LC_ID_DYLIB` 为 `None`。
    pub ordinal: Option<u32>,
}

impl MachDylib {
    /// 是否为 `LC_LAZY_LOAD_DYLIB`。
    pub fn is_lazy(&self) -> bool {
        self.kind == MachDylibKind::Lazy
    }
}

/// `LC_DYLD_INFO` / `LC_DYLD_INFO_ONLY` 给出的各条数据流位置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachDyldInfo {
    /// `rebase_off`。
    pub rebase_offset: u32,
    /// `rebase_size`。
    pub rebase_size: u32,
    /// `bind_off`。
    pub bind_offset: u32,
    /// `bind_size`。
    pub bind_size: u32,
    /// `weak_bind_off`。
    pub weak_bind_offset: u32,
    /// `weak_bind_size`。
    pub weak_bind_size: u32,
    /// `lazy_bind_off`。
    pub lazy_bind_offset: u32,
    /// `lazy_bind_size`。
    pub lazy_bind_size: u32,
    /// `export_off`。
    pub export_offset: u32,
    /// `export_size`。
    pub export_size: u32,
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
    /// `LC_SYMTAB` 给出的符号表。
    pub symtab: Option<MachSymtab>,
    /// 动态库加载命令，按加载顺序排列。
    pub dylibs: Vec<MachDylib>,
    /// `LC_DYLD_INFO` 给出的绑定数据流。
    pub dyld_info: Option<MachDyldInfo>,
    /// `LC_UUID` 给出的 16 字节载荷。
    pub uuid: Option<[u8; 16]>,
    /// `LC_BUILD_VERSION` 或 `LC_VERSION_MIN_*` 给出的平台与版本。
    pub build: Option<MachBuildInfo>,
    /// `LC_ENCRYPTION_INFO` / `LC_ENCRYPTION_INFO_64` 给出的加密信息。
    pub encryption: Option<MachEncryptionInfo>,
    /// `LC_FUNCTION_STARTS` 给出的载荷位置。
    pub function_starts: Option<MachLinkeditData>,
    /// `LC_DYLD_EXPORTS_TRIE` 给出的载荷位置。
    pub exports_trie: Option<MachLinkeditData>,
    /// `LC_DYLD_CHAINED_FIXUPS` 给出的载荷位置。
    pub chained_fixups: Option<MachLinkeditData>,
    /// `LC_CODE_SIGNATURE` 给出的载荷位置。
    pub code_signature: Option<MachLinkeditData>,
}

impl MachOImage {
    /// `LC_UUID` 渲染为规范 UUID 文本。
    pub fn uuid_string(&self) -> Option<String> {
        self.uuid.as_ref().map(uuid_string)
    }
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
    let (is_64, endian) =
        macho_magic(bytes).ok_or_else(|| if is_fat(bytes) { MachOParseError::FatBinary } else { MachOParseError::BadMagic })?;
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
    let mut image = MachOImage {
        header,
        segments: Vec::new(),
        sections: Vec::new(),
        entry_offset: None,
        symtab: None,
        dylibs: Vec::new(),
        dyld_info: None,
        uuid: None,
        build: None,
        encryption: None,
        function_starts: None,
        exports_trie: None,
        chained_fixups: None,
        code_signature: None,
    };
    let mut ordinal = 0u32;
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
            LC_SYMTAB => {
                if size >= 24 {
                    image.symtab = Some(MachSymtab {
                        symbol_offset: reader.u32(offset + 8)?,
                        symbol_count: reader.u32(offset + 12)?,
                        string_offset: reader.u32(offset + 16)?,
                        string_size: reader.u32(offset + 20)?,
                    });
                }
            }
            LC_LOAD_DYLIB | LC_LOAD_WEAK_DYLIB | LC_REEXPORT_DYLIB | LC_LOAD_UPWARD_DYLIB | LC_LAZY_LOAD_DYLIB | LC_ID_DYLIB => {
                if let Some(dylib) = reader.dylib(offset, size, command)? {
                    if dylib.kind.has_ordinal() {
                        ordinal += 1;
                        image.dylibs.push(MachDylib { ordinal: Some(ordinal), ..dylib });
                    }
                    else {
                        image.dylibs.push(dylib);
                    }
                }
            }
            LC_UUID => {
                if size >= 24 {
                    image.uuid = reader.slice(offset + 8, 16)?.try_into().ok();
                }
            }
            LC_BUILD_VERSION => {
                if size >= 24 {
                    image.build = Some(MachBuildInfo {
                        platform: MachPlatform::from_value(reader.u32(offset + 8)?),
                        min_os: reader.u32(offset + 12)?,
                        sdk: reader.u32(offset + 16)?,
                        source: MachBuildSource::BuildVersion,
                    });
                }
            }
            LC_VERSION_MIN_MACOSX | LC_VERSION_MIN_IPHONEOS | LC_VERSION_MIN_TVOS | LC_VERSION_MIN_WATCHOS => {
                if size >= 16 {
                    image.build = Some(MachBuildInfo {
                        platform: version_min_platform(command),
                        min_os: reader.u32(offset + 8)?,
                        sdk: reader.u32(offset + 12)?,
                        source: MachBuildSource::VersionMin,
                    });
                }
            }
            LC_ENCRYPTION_INFO => {
                if size >= 20 {
                    image.encryption = Some(MachEncryptionInfo {
                        crypt_offset: reader.u32(offset + 8)?,
                        crypt_size: reader.u32(offset + 12)?,
                        crypt_id: reader.u32(offset + 16)?,
                        is_64: false,
                    });
                }
            }
            LC_ENCRYPTION_INFO_64 => {
                if size >= 24 {
                    image.encryption = Some(MachEncryptionInfo {
                        crypt_offset: reader.u32(offset + 8)?,
                        crypt_size: reader.u32(offset + 12)?,
                        crypt_id: reader.u32(offset + 16)?,
                        is_64: true,
                    });
                }
            }
            LC_FUNCTION_STARTS => {
                image.function_starts = reader.linkedit_data(offset, size)?;
            }
            LC_DYLD_EXPORTS_TRIE => {
                image.exports_trie = reader.linkedit_data(offset, size)?;
            }
            LC_DYLD_CHAINED_FIXUPS => {
                image.chained_fixups = reader.linkedit_data(offset, size)?;
            }
            LC_CODE_SIGNATURE => {
                image.code_signature = reader.linkedit_data(offset, size)?;
            }
            LC_DYLD_INFO | LC_DYLD_INFO_ONLY => {
                if size >= 48 {
                    image.dyld_info = Some(MachDyldInfo {
                        rebase_offset: reader.u32(offset + 8)?,
                        rebase_size: reader.u32(offset + 12)?,
                        bind_offset: reader.u32(offset + 16)?,
                        bind_size: reader.u32(offset + 20)?,
                        weak_bind_offset: reader.u32(offset + 24)?,
                        weak_bind_size: reader.u32(offset + 28)?,
                        lazy_bind_offset: reader.u32(offset + 32)?,
                        lazy_bind_size: reader.u32(offset + 36)?,
                        export_offset: reader.u32(offset + 40)?,
                        export_size: reader.u32(offset + 44)?,
                    });
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
    fat_magic(bytes).is_some()
}

/// 识别通用（fat）容器魔数，返回是否为 64 位 `fat_arch_64` 布局。
pub fn fat_magic(bytes: &[u8]) -> Option<bool> {
    let raw = bytes.get(0..4)?;
    let be = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
    let le = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
    match (be == FAT_MAGIC || be == FAT_MAGIC_64, le == FAT_MAGIC || le == FAT_MAGIC_64) {
        (true, _) => Some(be == FAT_MAGIC_64),
        (_, true) => Some(le == FAT_MAGIC_64),
        _ => None,
    }
}

/// `LC_VERSION_MIN_*` 对应的平台。
fn version_min_platform(command: u32) -> MachPlatform {
    match command {
        LC_VERSION_MIN_MACOSX => MachPlatform::MacOs,
        LC_VERSION_MIN_IPHONEOS => MachPlatform::IOs,
        LC_VERSION_MIN_TVOS => MachPlatform::TvOs,
        _ => MachPlatform::WatchOs,
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

    fn u16(&self, offset: u64) -> Result<u16, MachOParseError> {
        let raw = self.slice(offset, 2)?;
        Ok(match self.endian {
            MachOEndian::Little => u16::from_le_bytes([raw[0], raw[1]]),
            MachOEndian::Big => u16::from_be_bytes([raw[0], raw[1]]),
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

    /// 解析 `LC_LOAD_DYLIB` 家族命令。
    fn dylib(&self, offset: u64, size: u32, command: u32) -> Result<Option<MachDylib>, MachOParseError> {
        if size < 24 {
            return Ok(None);
        }
        let name_offset = self.u32(offset + 8)?;
        if name_offset == 0 || u64::from(name_offset) >= u64::from(size) {
            return Ok(None);
        }
        let start = offset + u64::from(name_offset);
        let raw = self.slice(start, u64::from(size) - u64::from(name_offset))?;
        let length = raw.iter().position(|byte| *byte == 0).unwrap_or(raw.len());
        let name = String::from_utf8_lossy(&raw[..length]).into_owned();
        let kind = match command {
            LC_LOAD_DYLIB => MachDylibKind::Load,
            LC_LOAD_WEAK_DYLIB => MachDylibKind::Weak,
            LC_REEXPORT_DYLIB => MachDylibKind::Reexport,
            LC_LOAD_UPWARD_DYLIB => MachDylibKind::Upward,
            LC_LAZY_LOAD_DYLIB => MachDylibKind::Lazy,
            _ => MachDylibKind::Id,
        };
        Ok(Some(MachDylib {
            name,
            kind,
            timestamp: self.u32(offset + 12)?,
            current_version: self.u32(offset + 16)?,
            compatibility_version: self.u32(offset + 20)?,
            ordinal: None,
        }))
    }

    /// 解析 `linkedit_data_command` 的 `dataoff` / `datasize`。
    fn linkedit_data(&self, offset: u64, size: u32) -> Result<Option<MachLinkeditData>, MachOParseError> {
        if size < 16 {
            return Ok(None);
        }
        Ok(Some(MachLinkeditData { data_offset: self.u32(offset + 8)?, data_size: self.u32(offset + 12)? }))
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

    fn section32(&self, sections: &mut Vec<MachSection>, offset: u64, size: u32, segment: &str) -> Result<(), MachOParseError> {
        let count = u64::from(self.u32(offset + 48)?);
        let first = offset + SEGMENT32_SIZE;
        if first + count * SECTION32_SIZE > offset + u64::from(size) {
            return Err(MachOParseError::SegmentSectionsOutOfRange { segment: segment.to_string(), count: count as u32, size });
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

    fn section64(&self, sections: &mut Vec<MachSection>, offset: u64, size: u32, segment: &str) -> Result<(), MachOParseError> {
        let count = u64::from(self.u32(offset + 64)?);
        let first = offset + SEGMENT64_SIZE;
        if first + count * SECTION64_SIZE > offset + u64::from(size) {
            return Err(MachOParseError::SegmentSectionsOutOfRange { segment: segment.to_string(), count: count as u32, size });
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

/// 由 Mach-O `cputype` 归纳出的指令集族。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MachArchitecture {
    /// 32 位 x86。
    X86,
    /// 64 位 x86。
    X64,
    /// 32 位 ARM。
    Arm,
    /// 64 位 ARM。
    Arm64,
    /// 未识别的 CPU 类型。
    Unknown,
}

impl MachArchitecture {
    /// 报告中使用的稳定小写名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::X86 => "x86",
            Self::X64 => "x64",
            Self::Arm => "arm",
            Self::Arm64 => "arm64",
            Self::Unknown => "unknown",
        }
    }
}

/// 把 `cputype` 映射到指令集族。
pub fn architecture(cpu_type: i32) -> MachArchitecture {
    match cpu_type {
        7 => MachArchitecture::X86,
        0x0100_0007 => MachArchitecture::X64,
        0x0000_000c => MachArchitecture::Arm,
        0x0100_000c => MachArchitecture::Arm64,
        _ => MachArchitecture::Unknown,
    }
}

/// 一条 `Mach-O` 符号（`nlist`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachSymbol {
    /// 符号名。
    pub name: String,
    /// `n_value`。
    pub value: u64,
    /// `n_type`。
    pub symbol_type: u8,
    /// `n_sect`。
    pub section: u8,
    /// `n_desc`。
    pub description: u16,
}

impl MachSymbol {
    /// 是否为外部符号。
    pub fn is_external(&self) -> bool {
        self.symbol_type & 0x01 != 0
    }

    /// 是否为未定义（导入）符号。
    pub fn is_undefined(&self) -> bool {
        self.symbol_type & 0x0e == 0x00
    }

    /// 是否为定义在某个节中的符号。
    pub fn is_section(&self) -> bool {
        self.symbol_type & 0x0e == 0x0e && self.section != 0
    }
}

/// 读取 `LC_SYMTAB` 描述的全部符号。
///
/// 没有符号表时返回空列表；符号表或字符串表越界时返回错误。
pub fn read_macho_symbols(bytes: &[u8], image: &MachOImage) -> Result<Vec<MachSymbol>, MachOParseError> {
    let Some(symtab) = image.symtab
    else {
        return Ok(Vec::new());
    };
    let entry_size = if image.header.is_64 { 16u64 } else { 12u64 };
    let reader = Reader { bytes, endian: image.header.endian };
    let table_length = u64::from(symtab.symbol_count) * entry_size;
    reader.slice(u64::from(symtab.symbol_offset), table_length)?;
    let strings = reader.slice(u64::from(symtab.string_offset), u64::from(symtab.string_size))?;

    let mut symbols = Vec::with_capacity(symtab.symbol_count as usize);
    for index in 0..u64::from(symtab.symbol_count) {
        let base = u64::from(symtab.symbol_offset) + index * entry_size;
        let name_index = reader.u32(base)? as usize;
        let symbol_type = reader.slice(base + 4, 1)?[0];
        let section = reader.slice(base + 5, 1)?[0];
        let description = reader.u16(base + 6)?;
        let value = if image.header.is_64 { reader.u64(base + 8)? } else { u64::from(reader.u32(base + 8)?) };
        let name = match strings.get(name_index..) {
            Some(tail) => {
                let length = tail.iter().position(|byte| *byte == 0).unwrap_or(tail.len());
                String::from_utf8_lossy(&tail[..length]).into_owned()
            }
            None => String::new(),
        };
        symbols.push(MachSymbol { name, value, symbol_type, section, description });
    }
    Ok(symbols)
}
