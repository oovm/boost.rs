//! `PE` 延迟加载导入表解码。
//!
//! 数据目录 13 保存 `IMAGE_DELAYLOAD_DESCRIPTOR` 数组，每个延迟加载模块一条，
//! 以全零描述符结尾。每条记录指向 DLL 名、模块句柄槽以及并行的 Import Name
//! 与 Import Address 表；INT 条目要么是 hint/name 对的 `RVA`，要么是置了
//! 第 31 位的序号。这里只解析 v2（基于 `RVA`）布局；v1 记录存的是虚拟地址，
//! 因此其名称与表按原始字段报告并自然映射失败。本模块不加载模块，也不调用
//! 延迟加载辅助例程。

use serde::{Deserialize, Serialize};

use super::{
    analysis::PeAnalysisError,
    read::{PeImage, rva_to_offset},
};

/// 延迟导入目录在数据目录中的索引。
const DIRECTORY_DELAY_IMPORTS: usize = 13;
/// 描述符字节数。
const DESCRIPTOR_SIZE: usize = 32;
/// 最多收集的延迟模块。
const MAX_DELAY_MODULES: usize = 128;
/// 每个模块最多遍历的 thunk。
const MAX_DELAY_THUNKS: usize = 256;
/// 全镜像最多收集的 thunk。
const MAX_DELAY_TOTAL_THUNKS: usize = 1_024;
/// 序号导入标志。
const ORDINAL_FLAG: u32 = 0x8000_0000;

/// 一条延迟 thunk：INT 条目、并行 IAT 槽与导入。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeDelayThunk {
    /// 该 thunk 来自的 INT 条目 `RVA`。
    pub int_rva: u32,
    /// 并行 IAT 槽 `RVA`，描述符没有 IAT 时不存在。
    pub iat_rva: Option<u32>,
    /// 条目引用 hint/name 对时的导入名。
    pub name: Option<String>,
    /// 条目置了第 31 位时的导入序号。
    pub ordinal: Option<u32>,
    /// 名称导入的 hint 索引。
    pub hint: Option<u16>,
}

/// 一个延迟加载模块及其解码后的 thunk。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeDelayModule {
    /// 从其 `RVA` 读到的模块名，无法解析时为空串。
    pub name: String,
    /// 原始 `Attributes` 字段（v1 的 DLL 名 `RVA`）。
    pub attributes: u32,
    /// 链接器记录的 `dwTimeStamp`。
    pub timestamp: u32,
    /// 模块句柄槽 `RVA`。
    pub module_handle_rva: u32,
    /// 延迟 IAT 的 `RVA`。
    pub iat_rva: u32,
    /// 延迟 INT 的 `RVA`。
    pub int_rva: u32,
    /// 绑定 IAT 的 `RVA`。
    pub bound_iat_rva: u32,
    /// 卸载 IAT 的 `RVA`。
    pub unload_iat_rva: u32,
    /// 通过上限的 thunk，按 INT 顺序排列。
    pub thunks: Vec<PeDelayThunk>,
}

/// 有界的 `PE` 延迟导入列表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeDelayImportsReport {
    /// 镜像是否有非空延迟导入目录。
    pub present: bool,
    /// 描述符数组的 `RVA`。
    pub directory_rva: u32,
    /// 目录声明大小。
    pub directory_size: u32,
    /// 通过上限的模块，按目录顺序排列。
    pub modules: Vec<PeDelayModule>,
    /// 终止符或上限之前看到的描述符数。
    pub total_modules: usize,
    /// 所有模块列出的 thunk 数。
    pub total_thunks: usize,
    /// 是否因上限或不可读数据停止。
    pub truncated: bool,
}

/// 解码一个 PE 镜像的延迟导入目录。
pub fn read_delay_imports(image: &PeImage, bytes: &[u8]) -> Result<PeDelayImportsReport, PeAnalysisError> {
    let directory = image.optional.data_directories[DIRECTORY_DELAY_IMPORTS];
    if directory.rva == 0 || directory.size == 0 {
        return Ok(PeDelayImportsReport {
            present: false,
            directory_rva: 0,
            directory_size: 0,
            modules: Vec::new(),
            total_modules: 0,
            total_thunks: 0,
            truncated: false,
        });
    }

    let resolve = |rva: u32| rva_to_offset(&image.sections, rva).ok().map(|value| value as usize);
    let base = resolve(directory.rva).ok_or(PeAnalysisError::Rva(directory.rva))?;
    let mut report = PeDelayImportsReport {
        present: true,
        directory_rva: directory.rva,
        directory_size: directory.size,
        modules: Vec::new(),
        total_modules: 0,
        total_thunks: 0,
        truncated: false,
    };

    let mut offset = base;
    loop {
        let Some(fields) = read_descriptor(bytes, offset)
        else {
            report.truncated = true;
            break;
        };
        if fields.iter().all(|value| *value == 0) {
            break;
        }
        report.total_modules += 1;
        if report.modules.len() >= MAX_DELAY_MODULES {
            report.truncated = true;
            break;
        }

        let name = resolve(fields[1]).and_then(|value| read_cstring(bytes, value)).unwrap_or_default();
        let mut thunks = Vec::new();
        if fields[4] != 0 {
            match resolve(fields[4]) {
                None => report.truncated = true,
                Some(table) => {
                    let mut budget = MAX_DELAY_TOTAL_THUNKS.saturating_sub(report.total_thunks);
                    for index in 0..MAX_DELAY_THUNKS {
                        let Some(entry) = read_u32(bytes, table + index * 4)
                        else {
                            report.truncated = true;
                            break;
                        };
                        if entry == 0 {
                            break;
                        }
                        if budget == 0 {
                            report.truncated = true;
                            break;
                        }
                        budget -= 1;
                        let step = u32::try_from(index * 4).unwrap_or(u32::MAX);
                        let mut thunk = PeDelayThunk {
                            int_rva: fields[4].saturating_add(step),
                            iat_rva: if fields[3] == 0 { None } else { Some(fields[3].saturating_add(step)) },
                            name: None,
                            ordinal: None,
                            hint: None,
                        };
                        if entry & ORDINAL_FLAG != 0 {
                            thunk.ordinal = Some(entry & 0xffff);
                            thunks.push(thunk);
                            continue;
                        }
                        let Some(name_offset) = resolve(entry)
                        else {
                            report.truncated = true;
                            break;
                        };
                        let (Some(hint), Some(import_name)) = (read_u16(bytes, name_offset), read_cstring(bytes, name_offset + 2))
                        else {
                            report.truncated = true;
                            break;
                        };
                        thunk.name = Some(import_name);
                        thunk.hint = Some(hint);
                        thunks.push(thunk);
                    }
                    if thunks.len() == MAX_DELAY_THUNKS {
                        report.truncated = true;
                    }
                }
            }
        }
        report.total_thunks += thunks.len();
        report.modules.push(build_module(fields, name, thunks));
        offset += DESCRIPTOR_SIZE;
    }
    Ok(report)
}

fn build_module(fields: [u32; 8], name: String, thunks: Vec<PeDelayThunk>) -> PeDelayModule {
    PeDelayModule {
        name,
        attributes: fields[0],
        timestamp: fields[7],
        module_handle_rva: fields[2],
        iat_rva: fields[3],
        int_rva: fields[4],
        bound_iat_rva: fields[5],
        unload_iat_rva: fields[6],
        thunks,
    }
}

fn read_descriptor(bytes: &[u8], offset: usize) -> Option<[u32; 8]> {
    let mut fields = [0u32; 8];
    for (index, field) in fields.iter_mut().enumerate() {
        *field = read_u32(bytes, offset.checked_add(index * 4)?)?;
    }
    Some(fields)
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
