//! `PE` TLS 目录解码。
//!
//! TLS 数据目录（索引 9）记录原始模板范围、索引槽与一个回调虚拟地址数组，
//! 数组以空指针结尾。地址是虚拟地址，因此先经镜像基址换算成 `RVA`，再经节表
//! 映射到文件偏移。本模块从不调用回调。

use serde::{Deserialize, Serialize};

use super::{
    analysis::PeAnalysisError,
    read::{PeImage, rva_to_offset},
};

/// TLS 表在数据目录中的索引。
const DIRECTORY_TLS: usize = 9;
/// 最多收集的回调。
const MAX_CALLBACKS: usize = 64;

/// 一个 TLS 回调地址及其文件映射。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeTlsCallback {
    /// 数组中存的原样回调虚拟地址。
    pub address: u64,
    /// 回调落在镜像内时的镜像相对地址。
    pub rva: Option<u64>,
    /// `RVA` 映射到节时回调代码的文件偏移。
    pub offset: Option<u64>,
}

/// `PE` TLS 表摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeTlsReport {
    /// 镜像是否有非空 TLS 目录。
    pub present: bool,
    /// 镜像是否使用 64 位 TLS 布局。
    pub is64: bool,
    /// TLS 目录的 `RVA`。
    pub directory_rva: u32,
    /// TLS 目录声明大小。
    pub directory_size: u32,
    /// `StartAddressOfRawData` 的虚拟地址。
    pub raw_data_start: u64,
    /// `EndAddressOfRawData` 的虚拟地址。
    pub raw_data_end: u64,
    /// `AddressOfIndex` 的虚拟地址。
    pub index_address: u64,
    /// `AddressOfCallBacks` 的虚拟地址。
    pub callbacks_address: u64,
    /// 回调数组的镜像相对地址，落在镜像内时存在。
    pub callbacks_rva: Option<u64>,
    /// `SizeOfZeroFill`（字节）。
    pub size_of_zero_fill: u32,
    /// 原始 `Characteristics` 字段。
    pub characteristics: u32,
    /// 通过上限的非空回调，按数组顺序排列。
    pub callbacks: Vec<PeTlsCallback>,
    /// 上限或畸形数组是否停止了列表。
    pub truncated: bool,
}

impl PeTlsReport {
    fn empty(is64: bool) -> Self {
        Self {
            present: false,
            is64,
            directory_rva: 0,
            directory_size: 0,
            raw_data_start: 0,
            raw_data_end: 0,
            index_address: 0,
            callbacks_address: 0,
            callbacks_rva: None,
            size_of_zero_fill: 0,
            characteristics: 0,
            callbacks: Vec::new(),
            truncated: false,
        }
    }
}

/// 解码一个 PE 镜像的 TLS 目录。
pub fn read_tls_directory(image: &PeImage, bytes: &[u8]) -> Result<PeTlsReport, PeAnalysisError> {
    let is64 = image.optional.magic == 0x20b;
    let directory = image.optional.data_directories[DIRECTORY_TLS];
    if directory.rva == 0 || directory.size == 0 {
        return Ok(PeTlsReport::empty(is64));
    }

    let base =
        usize::try_from(rva_to_offset(&image.sections, directory.rva).map_err(|_| PeAnalysisError::Rva(directory.rva))?).unwrap_or(usize::MAX);
    let mut report = PeTlsReport {
        present: true,
        is64,
        directory_rva: directory.rva,
        directory_size: directory.size,
        raw_data_start: 0,
        raw_data_end: 0,
        index_address: 0,
        callbacks_address: 0,
        callbacks_rva: None,
        size_of_zero_fill: 0,
        characteristics: 0,
        callbacks: Vec::new(),
        truncated: false,
    };

    let fields = if is64 {
        let (Some(start), Some(end), Some(index), Some(callbacks)) = (
            read_u64(bytes, base),
            read_u64(bytes, base.saturating_add(8)),
            read_u64(bytes, base.saturating_add(16)),
            read_u64(bytes, base.saturating_add(24)),
        )
        else {
            report.truncated = true;
            return Ok(report);
        };
        (start, end, index, callbacks)
    }
    else {
        let (Some(start), Some(end), Some(index), Some(callbacks)) = (
            read_u32(bytes, base),
            read_u32(bytes, base.saturating_add(4)),
            read_u32(bytes, base.saturating_add(8)),
            read_u32(bytes, base.saturating_add(12)),
        )
        else {
            report.truncated = true;
            return Ok(report);
        };
        (u64::from(start), u64::from(end), u64::from(index), u64::from(callbacks))
    };
    report.raw_data_start = fields.0;
    report.raw_data_end = fields.1;
    report.index_address = fields.2;
    report.callbacks_address = fields.3;
    let zero_fill_offset = if is64 { base.saturating_add(32) } else { base.saturating_add(16) };
    report.size_of_zero_fill = read_u32(bytes, zero_fill_offset).unwrap_or(0);
    report.characteristics = read_u32(bytes, zero_fill_offset.saturating_add(4)).unwrap_or(0);

    let image_base = image.optional.image_base;
    let Some(callbacks_rva) = report.callbacks_address.checked_sub(image_base)
    else {
        report.truncated = true;
        return Ok(report);
    };
    report.callbacks_rva = Some(callbacks_rva);
    let Ok(array_offset) = rva_to_offset(&image.sections, callbacks_rva as u32)
    else {
        report.truncated = true;
        return Ok(report);
    };
    let array_offset = array_offset as usize;
    let pointer_size = if is64 { 8 } else { 4 };

    for index in 0..MAX_CALLBACKS {
        let at = array_offset.saturating_add(index.saturating_mul(pointer_size));
        let address = if is64 { read_u64(bytes, at) } else { read_u32(bytes, at).map(u64::from) };
        let Some(address) = address
        else {
            report.truncated = true;
            break;
        };
        if address == 0 {
            break;
        }
        let rva = address.checked_sub(image_base);
        let offset =
            rva.and_then(|value| u32::try_from(value).ok()).and_then(|value| rva_to_offset(&image.sections, value).ok()).map(u64::from);
        report.callbacks.push(PeTlsCallback { address, rva, offset });
    }
    if report.callbacks.len() == MAX_CALLBACKS {
        report.truncated = true;
    }
    Ok(report)
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?))
}
