//! 通用（fat/universal）`Mach-O` 容器的架构切片表。
//!
//! 一段镜像需要先用 [`read_fat_slices`] 选择切片，再把该区间交给
//! [`parse_macho`](super::read::parse_macho)。

use super::read::{MachArchitecture, MachOParseError, architecture, fat_magic, macho_magic};

/// 声明的切片数量上限，防止损坏的头撑爆分配。
pub const MAX_FAT_SLICES: usize = 256;

/// 一条架构切片。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachFatSlice {
    /// 架构表内序号，从 `0` 开始。
    pub index: usize,
    /// 原始 `cputype`。
    pub cpu_type: i32,
    /// 原始 `cpusubtype`。
    pub cpu_subtype: i32,
    /// 由 `cputype` 归纳出的指令集族。
    pub architecture: MachArchitecture,
    /// 切片在文件中的偏移。
    pub offset: u64,
    /// 切片大小。
    pub size: u64,
    /// 对齐，以 2 的幂次表示。
    pub alignment: u32,
    /// 切片以 `Mach-O` 魔数开头时的位宽。
    pub bits: Option<u32>,
}

/// fat 架构表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachFatSlices {
    /// 是否使用 64 位 `fat_arch_64` 布局。
    pub is_fat64: bool,
    /// 切片，按表内顺序。
    pub slices: Vec<MachFatSlice>,
}

/// 读取通用（fat/universal）`Mach-O` 的架构切片表。
///
/// 非 fat 输入返回 [`MachOParseError::NotFatBinary`]；切片越界返回
/// [`MachOParseError::FatSliceOutOfRange`]。
pub fn read_fat_slices(bytes: &[u8]) -> Result<MachFatSlices, MachOParseError> {
    let is_fat64 = fat_magic(bytes).ok_or(MachOParseError::NotFatBinary)?;
    let header = bytes.get(0..8).ok_or(MachOParseError::TooShort)?;
    let count = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    if count as usize > MAX_FAT_SLICES {
        return Err(MachOParseError::FatSliceLimit { declared: count, limit: MAX_FAT_SLICES });
    }
    let count = count as usize;
    let entry_size = if is_fat64 { 32 } else { 20 };
    let table_end = 8usize.saturating_add(count.saturating_mul(entry_size));
    let table = bytes.get(8..table_end).ok_or(MachOParseError::FatTableTruncated {
        declared: count as u32,
        available: bytes.len().saturating_sub(8),
    })?;

    let mut slices = Vec::with_capacity(count);
    for index in 0..count {
        let entry = &table[index * entry_size..(index + 1) * entry_size];
        let cpu_type = be_u32(entry, 0)? as i32;
        let cpu_subtype = be_u32(entry, 4)? as i32;
        let (offset, size, alignment) = if is_fat64 {
            (be_u64(entry, 8)?, be_u64(entry, 16)?, be_u32(entry, 24)?)
        }
        else {
            (u64::from(be_u32(entry, 8)?), u64::from(be_u32(entry, 12)?), be_u32(entry, 16)?)
        };
        let end = offset
            .checked_add(size)
            .filter(|end| *end <= bytes.len() as u64)
            .ok_or(MachOParseError::FatSliceOutOfRange { index, offset, size })?;
        let bits = macho_magic(&bytes[offset as usize..end as usize]).map(|(is_64, _)| if is_64 { 64 } else { 32 });
        slices.push(MachFatSlice {
            index,
            cpu_type,
            cpu_subtype,
            architecture: architecture(cpu_type),
            offset,
            size,
            alignment,
            bits,
        });
    }
    Ok(MachFatSlices { is_fat64, slices })
}

fn be_u32(bytes: &[u8], offset: usize) -> Result<u32, MachOParseError> {
    let raw: [u8; 4] = bytes
        .get(offset..offset + 4)
        .and_then(|slice| slice.try_into().ok())
        .ok_or(MachOParseError::OutOfRange { offset: offset as u64, length: 4 })?;
    Ok(u32::from_be_bytes(raw))
}

fn be_u64(bytes: &[u8], offset: usize) -> Result<u64, MachOParseError> {
    let raw: [u8; 8] = bytes
        .get(offset..offset + 8)
        .and_then(|slice| slice.try_into().ok())
        .ok_or(MachOParseError::OutOfRange { offset: offset as u64, length: 8 })?;
    Ok(u64::from_be_bytes(raw))
}
