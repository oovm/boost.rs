//! `PE` 绑定导入表解码。
//!
//! 数据目录 11 保存 `IMAGE_BOUND_IMPORT_DESCRIPTOR` 数组：一个 `u32` 时间戳、
//! 一个从表起点计数的 `u16` 模块名偏移和一个 `u16` 转发器数量，随后内联着同样
//! 字段的 `IMAGE_BOUND_FORWARDER_REF` 记录。全零描述符终止数组。这里只记录
//! 绑定器看到的时间戳，不重新绑定也不加载模块。缺失终止符或不可读的名称偏移
//! 会置 `truncated` 并停止遍历。

use serde::{Deserialize, Serialize};

use super::{
    analysis::PeAnalysisError,
    read::{PeImage, rva_to_offset},
};

/// 绑定导入目录在数据目录中的索引。
const DIRECTORY_BOUND_IMPORTS: usize = 11;
/// 描述符字节数。
const DESCRIPTOR_SIZE: usize = 8;
/// 最多收集的描述符。
const MAX_BOUND_DESCRIPTORS: usize = 64;
/// 每个描述符最多收集的转发器。
const MAX_BOUND_FORWARDERS: usize = 32;

/// 绑定导入描述符内的一条转发器引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeBoundForwarder {
    /// 绑定器为转发模块记录的时间戳。
    pub timestamp: u32,
    /// 从绑定导入表起点计数的名称偏移。
    pub offset_module_name: u16,
    /// 从该偏移读到的模块名，无法解析时为空串。
    pub name: String,
}

/// 一条绑定导入描述符及其内联转发器引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeBoundModule {
    /// 绑定器为该模块记录的时间戳。
    pub timestamp: u32,
    /// 从绑定导入表起点计数的名称偏移。
    pub offset_module_name: u16,
    /// 从该偏移读到的模块名，无法解析时为空串。
    pub name: String,
    /// 通过上限的转发器引用。
    pub forwarders: Vec<PeBoundForwarder>,
}

/// 有界的 `PE` 绑定导入列表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeBoundImportsReport {
    /// 镜像是否有非空绑定导入目录。
    pub present: bool,
    /// 描述符数组的 `RVA`。
    pub directory_rva: u32,
    /// 目录声明大小。
    pub directory_size: u32,
    /// 通过上限的描述符，按表序排列。
    pub modules: Vec<PeBoundModule>,
    /// 终止符或上限之前看到的描述符数。
    pub total_modules: usize,
    /// 所有模块列出的转发器数。
    pub total_forwarders: usize,
    /// 是否因上限或不可读数据停止。
    pub truncated: bool,
}

/// 解码一个 PE 镜像的绑定导入目录。
pub fn read_bound_imports(image: &PeImage, bytes: &[u8]) -> Result<PeBoundImportsReport, PeAnalysisError> {
    let directory = image.optional.data_directories[DIRECTORY_BOUND_IMPORTS];
    if directory.rva == 0 || directory.size == 0 {
        return Ok(PeBoundImportsReport {
            present: false,
            directory_rva: 0,
            directory_size: 0,
            modules: Vec::new(),
            total_modules: 0,
            total_forwarders: 0,
            truncated: false,
        });
    }

    let resolve = |rva: u32| rva_to_offset(&image.sections, rva).ok().map(|value| value as usize);
    let base = resolve(directory.rva).ok_or(PeAnalysisError::Rva(directory.rva))?;
    let mut report = PeBoundImportsReport {
        present: true,
        directory_rva: directory.rva,
        directory_size: directory.size,
        modules: Vec::new(),
        total_modules: 0,
        total_forwarders: 0,
        truncated: false,
    };

    let mut offset = base;
    loop {
        let (Some(timestamp), Some(name_offset), Some(forwarders)) =
            (read_u32(bytes, offset), read_u16(bytes, offset + 4), read_u16(bytes, offset + 6))
        else {
            report.truncated = true;
            break;
        };
        if timestamp == 0 && name_offset == 0 && forwarders == 0 {
            break;
        }
        report.total_modules += 1;
        if report.modules.len() >= MAX_BOUND_DESCRIPTORS {
            report.truncated = true;
            break;
        }

        let mut module = PeBoundModule { timestamp, offset_module_name: name_offset, name: String::new(), forwarders: Vec::new() };
        match read_name(bytes, &resolve, directory.rva, name_offset) {
            Some(name) => module.name = name,
            None => report.truncated = true,
        }

        let count = usize::from(forwarders);
        for index in 0..count {
            if module.forwarders.len() >= MAX_BOUND_FORWARDERS {
                report.truncated = true;
                break;
            }
            let at = offset + DESCRIPTOR_SIZE + index * DESCRIPTOR_SIZE;
            let (Some(ref_timestamp), Some(ref_name_offset)) = (read_u32(bytes, at), read_u16(bytes, at + 4))
            else {
                report.truncated = true;
                break;
            };
            let mut forwarder = PeBoundForwarder { timestamp: ref_timestamp, offset_module_name: ref_name_offset, name: String::new() };
            match read_name(bytes, &resolve, directory.rva, ref_name_offset) {
                Some(name) => forwarder.name = name,
                None => report.truncated = true,
            }
            module.forwarders.push(forwarder);
        }
        report.total_forwarders += module.forwarders.len();
        report.modules.push(module);
        offset += DESCRIPTOR_SIZE + count * DESCRIPTOR_SIZE;
    }
    Ok(report)
}

fn read_name(bytes: &[u8], resolve: &impl Fn(u32) -> Option<usize>, directory_rva: u32, offset: u16) -> Option<String> {
    resolve(directory_rva.saturating_add(u32::from(offset))).and_then(|value| read_cstring(bytes, value))
}

fn read_cstring(bytes: &[u8], offset: usize) -> Option<String> {
    let tail = bytes.get(offset..)?;
    let end = tail.iter().position(|byte| *byte == 0)?;
    Some(String::from_utf8_lossy(&tail[..end]).into_owned())
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?))
}
