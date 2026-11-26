//! `PE` 基址重定位解码。
//!
//! 走数据目录 5 后面的 `IMAGE_BASE_RELOCATION` 块并汇总重定位类型。每块覆盖
//! 一个 4 KiB 页，每个修复项是 16 位：高四位类型加低十二位页内偏移。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{
    analysis::PeAnalysisError,
    read::{PeImage, rva_to_offset},
};

/// 基址重定位目录在数据目录中的索引。
const DIRECTORY_BASE_RELOCATION: usize = 5;
/// 最多遍历的块数。
const MAX_BLOCKS: usize = 4096;
/// 最多保留的块数。
const MAX_STORED_BLOCKS: usize = 128;

/// 某一重定位类型的修复项数量。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeRelocationKindCount {
    /// 稳定类型名（`HIGHLOW`、`DIR64` 等），未知类型为 `#<n>`。
    pub kind: String,
    /// 该类型的修复项数量。
    pub count: u32,
}

/// 一个 `IMAGE_BASE_RELOCATION` 块。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeRelocationBlock {
    /// 该块修正的 4 KiB 页 `VirtualAddress`。
    pub page_rva: u32,
    /// 块声明大小，包含 8 字节块头。
    pub block_size: u32,
    /// 从块中解码出的修复项数量。
    pub entries: u32,
    /// 块内按类型统计的修复项，按类型号排序。
    pub kinds: Vec<PeRelocationKindCount>,
}

/// 有界的 `PE` 基址重定位列表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeRelocationsReport {
    /// 镜像是否有非空基址重定位目录。
    pub present: bool,
    /// 通过存储上限的块，按目录顺序排列。
    pub blocks: Vec<PeRelocationBlock>,
    /// 目录声明的块数。
    pub total_blocks: usize,
    /// 所有块中的修复项总数。
    pub total_entries: u64,
    /// 全镜像按类型统计的修复项，按类型号排序。
    pub kinds: Vec<PeRelocationKindCount>,
    /// 块或列表是否被上限或畸形尾部截断。
    pub truncated: bool,
}

/// 解码一个 PE 镜像的基址重定位。
pub fn read_relocations(image: &PeImage, bytes: &[u8]) -> Result<PeRelocationsReport, PeAnalysisError> {
    let directory = image.optional.data_directories[DIRECTORY_BASE_RELOCATION];
    if directory.rva == 0 || directory.size == 0 {
        return Ok(PeRelocationsReport {
            present: false,
            blocks: Vec::new(),
            total_blocks: 0,
            total_entries: 0,
            kinds: Vec::new(),
            truncated: false,
        });
    }
    let base =
        usize::try_from(rva_to_offset(&image.sections, directory.rva).map_err(|_| PeAnalysisError::Rva(directory.rva))?).unwrap_or(usize::MAX);

    let mut report =
        PeRelocationsReport { present: true, blocks: Vec::new(), total_blocks: 0, total_entries: 0, kinds: Vec::new(), truncated: false };
    let mut totals: BTreeMap<u16, u32> = BTreeMap::new();
    let end = base.saturating_add(directory.size as usize);
    let mut offset = base;

    while offset.saturating_add(8) <= end {
        if report.total_blocks >= MAX_BLOCKS {
            report.truncated = true;
            break;
        }
        let Ok(page_rva) = read_u32(bytes, offset)
        else {
            report.truncated = true;
            break;
        };
        let Ok(block_size) = read_u32(bytes, offset + 4)
        else {
            report.truncated = true;
            break;
        };
        if block_size < 8 {
            report.truncated = true;
            break;
        }
        let block_end = offset.saturating_add(block_size as usize);
        if block_end > end || block_end > bytes.len() {
            report.truncated = true;
            break;
        }

        report.total_blocks += 1;
        let entries = (block_size - 8) / 2;
        let mut kinds: BTreeMap<u16, u32> = BTreeMap::new();
        for index in 0..entries {
            let at = offset + 8 + index as usize * 2;
            let Ok(raw) = read_u16(bytes, at)
            else {
                report.truncated = true;
                break;
            };
            let kind = raw >> 12;
            *kinds.entry(kind).or_insert(0) += 1;
            *totals.entry(kind).or_insert(0) += 1;
        }
        report.total_entries += u64::from(entries);

        if report.blocks.len() < MAX_STORED_BLOCKS {
            report.blocks.push(PeRelocationBlock {
                page_rva,
                block_size,
                entries,
                kinds: kinds.into_iter().map(|(kind, count)| PeRelocationKindCount { kind: kind_name(kind), count }).collect(),
            });
        }
        else {
            report.truncated = true;
        }

        offset = block_end;
    }

    if offset < end && !report.truncated {
        // 不足一个块头的尾部说明目录大小并未描述完整块。
        report.truncated = true;
    }
    report.kinds = totals.into_iter().map(|(kind, count)| PeRelocationKindCount { kind: kind_name(kind), count }).collect();
    Ok(report)
}

fn kind_name(kind: u16) -> String {
    match kind {
        0 => "ABSOLUTE".to_string(),
        1 => "HIGH".to_string(),
        2 => "LOW".to_string(),
        3 => "HIGHLOW".to_string(),
        4 => "HIGHADJ".to_string(),
        5 => "MIPS_JMPADDR".to_string(),
        7 => "ARM_MOV32".to_string(),
        9 => "MIPS_JMPADDR16".to_string(),
        10 => "DIR64".to_string(),
        other => format!("#{other}"),
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, PeAnalysisError> {
    let raw: [u8; 2] = bytes
        .get(offset..offset.saturating_add(2))
        .and_then(|slice| slice.try_into().ok())
        .ok_or(PeAnalysisError::Truncated { table: "重定位表" })?;
    Ok(u16::from_le_bytes(raw))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, PeAnalysisError> {
    let raw: [u8; 4] = bytes
        .get(offset..offset.saturating_add(4))
        .and_then(|slice| slice.try_into().ok())
        .ok_or(PeAnalysisError::Truncated { table: "重定位块头" })?;
    Ok(u32::from_le_bytes(raw))
}
