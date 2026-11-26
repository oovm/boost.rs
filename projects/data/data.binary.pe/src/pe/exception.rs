//! `PE` 异常目录解码。
//!
//! 数据目录 3 保存运行时函数表：一个紧凑的 `RUNTIME_FUNCTION` 数组。条目布局
//! 随机器类型变化：x64 是三个 `u32`（起始地址、结束地址、展开信息 `RVA`），
//! ARM/ARM64 把起始地址的低位打包成标志并随后跟一个展开字。这里只解码展开头，
//! 展开操作码留给反汇编器。

use serde::{Deserialize, Serialize};

use super::{
    analysis::PeAnalysisError,
    read::{PeImage, rva_to_offset},
};

/// 异常目录在数据目录中的索引。
const DIRECTORY_EXCEPTION: usize = 3;
/// 最多收集的运行时函数。
const MAX_RUNTIME_FUNCTIONS: usize = 512;

const MACHINE_I386: u16 = 0x014c;
const MACHINE_AMD64: u16 = 0x8664;
const MACHINE_ARMNT: u16 = 0x01c4;
const MACHINE_ARM64: u16 = 0xaa64;
const MACHINE_IA64: u16 = 0x0200;
const MACHINE_RISCV32: u16 = 0x5032;
const MACHINE_RISCV64: u16 = 0x5064;

/// 解码后的 `UNWIND_INFO` 头，x64 运行时函数共用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeUnwindInfo {
    /// 展开信息版本（实践中为 1 或 2）。
    pub version: u8,
    /// 首字节中的原始标志位。
    pub flags: u8,
    /// 标志名：`EHANDLER`、`UHANDLER`、`CHAININFO`。
    pub flag_names: Vec<String>,
    /// 序言大小（字节）。
    pub prolog_size: u8,
    /// 展开码槽数量。
    pub code_count: u8,
    /// 帧寄存器索引，仅帧指针函数非零。
    pub frame_register: u8,
    /// 缩放后的帧寄存器偏移。
    pub frame_offset: u8,
    /// 是否设置了 `CHAININFO` 标志。
    pub chained: bool,
}

/// 一条 `RUNTIME_FUNCTION` 条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeRuntimeFunction {
    /// 起始 `RVA`，ARM/ARM64 会屏蔽低位标志。
    pub begin_address: u32,
    /// 结束 `RVA`。ARM/ARM64 记录省略该字段，值为零。
    pub end_address: u32,
    /// 展开信息 `RVA`（x64）或展开数据字（ARM/ARM64）。
    pub unwind_info: u32,
    /// 展开信息的文件偏移，`RVA` 能映射进镜像时存在。
    pub unwind_offset: Option<u32>,
    /// 解码后的展开头，仅 x64。
    pub unwind: Option<PeUnwindInfo>,
}

/// 有界的 `PE` 异常目录列表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeExceptionReport {
    /// 镜像是否有非空异常目录。
    pub present: bool,
    /// `COFF` 机器类型。
    pub machine: u16,
    /// 稳定机器名，未知值为 `#<n>`。
    pub machine_name: String,
    /// 条目布局：`x64`、`arm64`、`arm` 或 `unknown`。
    pub entry_format: String,
    /// 条目字节数，未知布局时不存在。
    pub entry_size: Option<u32>,
    /// 通过存储上限的条目，按目录顺序排列。
    pub functions: Vec<PeRuntimeFunction>,
    /// 目录大小声明的条目数。
    pub total: usize,
    /// 列表或目录尾部是否被截断。
    pub truncated: bool,
}

/// 解码一个 PE 镜像的异常目录。
pub fn read_exception_directory(image: &PeImage, bytes: &[u8]) -> Result<PeExceptionReport, PeAnalysisError> {
    let machine = image.coff.machine;
    let (entry_format, entry_size) = entry_layout(machine);
    let directory = image.optional.data_directories[DIRECTORY_EXCEPTION];
    if directory.rva == 0 || directory.size == 0 {
        return Ok(PeExceptionReport {
            present: false,
            machine,
            machine_name: machine_name(machine),
            entry_format: entry_format.to_string(),
            entry_size,
            functions: Vec::new(),
            total: 0,
            truncated: false,
        });
    }

    let Some(entry_size) = entry_size
    else {
        return Ok(PeExceptionReport {
            present: true,
            machine,
            machine_name: machine_name(machine),
            entry_format: entry_format.to_string(),
            entry_size: None,
            functions: Vec::new(),
            total: 0,
            truncated: false,
        });
    };
    let base =
        usize::try_from(rva_to_offset(&image.sections, directory.rva).map_err(|_| PeAnalysisError::Rva(directory.rva))?).unwrap_or(usize::MAX);
    let size = directory.size as usize;
    let end = base.saturating_add(size);
    let declared = size / entry_size as usize;

    let mut report = PeExceptionReport {
        present: true,
        machine,
        machine_name: machine_name(machine),
        entry_format: entry_format.to_string(),
        entry_size: Some(entry_size),
        functions: Vec::new(),
        total: declared,
        truncated: !size.is_multiple_of(entry_size as usize),
    };

    let mut offset = base;
    while report.functions.len() < MAX_RUNTIME_FUNCTIONS && offset.saturating_add(entry_size as usize) <= end {
        let Some(begin_raw) = read_u32(bytes, offset)
        else {
            report.truncated = true;
            break;
        };
        let Some(second) = read_u32(bytes, offset + 4)
        else {
            report.truncated = true;
            break;
        };
        if entry_format == "x64" {
            let Some(unwind_info) = read_u32(bytes, offset + 8)
            else {
                report.truncated = true;
                break;
            };
            let unwind_offset = rva_to_offset(&image.sections, unwind_info).ok();
            let unwind = unwind_offset.and_then(|value| decode_unwind(bytes, value as usize));
            report.functions.push(PeRuntimeFunction { begin_address: begin_raw, end_address: second, unwind_info, unwind_offset, unwind });
        }
        else {
            // ARM 与 ARM64 把每条记录的低两位打包进起始地址并随后存一个展开字。
            report.functions.push(PeRuntimeFunction {
                begin_address: begin_raw & !0x3,
                end_address: 0,
                unwind_info: second,
                unwind_offset: None,
                unwind: None,
            });
        }
        offset += entry_size as usize;
    }

    if report.functions.len() < declared && report.functions.len() >= MAX_RUNTIME_FUNCTIONS {
        report.truncated = true;
    }
    Ok(report)
}

fn entry_layout(machine: u16) -> (&'static str, Option<u32>) {
    match machine {
        MACHINE_AMD64 => ("x64", Some(12)),
        MACHINE_ARM64 => ("arm64", Some(8)),
        MACHINE_ARMNT => ("arm", Some(8)),
        _ => ("unknown", None),
    }
}

fn machine_name(machine: u16) -> String {
    match machine {
        MACHINE_I386 => "i386".to_string(),
        MACHINE_AMD64 => "amd64".to_string(),
        MACHINE_ARMNT => "armnt".to_string(),
        MACHINE_ARM64 => "arm64".to_string(),
        MACHINE_IA64 => "ia64".to_string(),
        MACHINE_RISCV32 => "riscv32".to_string(),
        MACHINE_RISCV64 => "riscv64".to_string(),
        other => format!("#{other}"),
    }
}

fn decode_unwind(bytes: &[u8], offset: usize) -> Option<PeUnwindInfo> {
    let header = bytes.get(offset..offset + 4)?;
    let version = header[0] & 0x07;
    if version == 0 {
        return None;
    }
    let flags = header[0] >> 3;
    Some(PeUnwindInfo {
        version,
        flags,
        flag_names: unwind_flag_names(flags),
        prolog_size: header[1],
        code_count: header[2],
        frame_register: header[3] & 0x0f,
        frame_offset: header[3] >> 4,
        chained: flags & 0x04 != 0,
    })
}

fn unwind_flag_names(flags: u8) -> Vec<String> {
    const NAMES: [(u8, &str); 3] = [(0x01, "EHANDLER"), (0x02, "UHANDLER"), (0x04, "CHAININFO")];
    NAMES.iter().filter(|(bit, _)| flags & bit != 0).map(|(_, name)| (*name).to_string()).collect()
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(offset..offset + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(raw))
}
