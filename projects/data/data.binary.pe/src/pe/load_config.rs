//! `PE` 加载配置目录解码。
//!
//! 数据目录 10 携带安全 cookie、SEH 处理器表以及加载器与 `/GS` / `/guard` 用的
//! 控制流保护元数据。布局为小端且 PE32 与 PE32+ 不同。本模块读取文档化的前缀，
//! 解码 `GuardFlags` 并报告取值；它从不把这些取值当作保证。

use serde::{Deserialize, Serialize};

use super::{
    analysis::PeAnalysisError,
    read::{PeImage, rva_to_offset},
};

/// 加载配置表在数据目录中的索引。
const DIRECTORY_LOAD_CONFIG: usize = 10;

const IMAGE_GUARD_CF_INSTRUMENTED: u32 = 0x0000_0100;
const IMAGE_GUARD_CFW_INSTRUMENTED: u32 = 0x0000_0200;
const IMAGE_GUARD_CF_FUNCTION_TABLE_PRESENT: u32 = 0x0000_0400;
const IMAGE_GUARD_SECURITY_COOKIE_UNUSED: u32 = 0x0000_0800;
const IMAGE_GUARD_PROTECT_DELAYLOAD_IAT: u32 = 0x0000_1000;
const IMAGE_GUARD_DELAYLOAD_IAT_IN_ITS_OWN_SECTION: u32 = 0x0000_2000;
const IMAGE_GUARD_CF_EXPORT_SUPPRESSION_INFO_PRESENT: u32 = 0x0000_4000;
const IMAGE_GUARD_CF_ENABLE_EXPORT_SUPPRESSION: u32 = 0x0000_8000;
const IMAGE_GUARD_CF_LONGJUMP_TABLE_PRESENT: u32 = 0x0001_0000;
const IMAGE_GUARD_RF_INSTRUMENTED: u32 = 0x0002_0000;
const IMAGE_GUARD_RF_ENABLE: u32 = 0x0004_0000;
const IMAGE_GUARD_RF_STRICT: u32 = 0x0008_0000;
const IMAGE_GUARD_RETPOLINE_PRESENT: u32 = 0x0010_0000;
const IMAGE_GUARD_EH_CONTINUATION_TABLE_PRESENT: u32 = 0x0040_0000;
const IMAGE_GUARD_XFG_ENABLED: u32 = 0x0080_0000;
const IMAGE_GUARD_CASTGUARD_PRESENT: u32 = 0x0100_0000;
const IMAGE_GUARD_MEMCPY_PRESENT: u32 = 0x0200_0000;

/// 加载配置的 `CodeIntegrity` 块。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeCodeIntegrityInfo {
    /// 原始 `Flags` 字段。
    pub flags: u16,
    /// 目录文件索引。
    pub catalog: u16,
    /// 目录文件偏移。
    pub catalog_offset: u32,
}

/// `PE` 加载配置摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeLoadConfigReport {
    /// 镜像是否有非空加载配置目录。
    pub present: bool,
    /// 表是否使用 64 位布局。
    pub is64: bool,
    /// 表声明大小。
    pub size: u32,
    /// 链接器记录的构建时间戳。
    pub timestamp: u32,
    /// 表主版本。
    pub major_version: u16,
    /// 表次版本。
    pub minor_version: u16,
    /// `GlobalFlagsClear`。
    pub global_flags_clear: u32,
    /// `GlobalFlagsSet`。
    pub global_flags_set: u32,
    /// 默认临界区自旋超时。
    pub critical_section_default_timeout: u32,
    /// `DeCommitFreeBlockThreshold`。
    pub de_commit_free_block_threshold: u64,
    /// `DeCommitTotalFreeThreshold`。
    pub de_commit_total_free_threshold: u64,
    /// 锁前缀表地址。
    pub lock_prefix_table: u64,
    /// `MaximumAllocationSize`。
    pub maximum_allocation_size: u64,
    /// `VirtualMemoryThreshold`。
    pub virtual_memory_threshold: u64,
    /// `ProcessAffinityMask`。
    pub process_affinity_mask: u64,
    /// `ProcessHeapFlags`。
    pub process_heap_flags: u32,
    /// `CSDVersion`。
    pub csd_version: u16,
    /// `DependentLoadFlags`。
    pub dependent_load_flags: u16,
    /// 编辑列表地址。
    pub edit_list: u64,
    /// `/GS` 安全 cookie 地址。
    pub security_cookie: u64,
    /// SEH 处理器表地址。
    pub se_handler_table: u64,
    /// SEH 处理器表条目数。
    pub se_handler_count: u64,
    /// `GuardCFCheckFunctionPointer`。
    pub guard_cf_check_function_pointer: u64,
    /// `GuardCFDispatchFunctionPointer`。
    pub guard_cf_dispatch_function_pointer: u64,
    /// CFG 函数表地址。
    pub guard_cf_function_table: u64,
    /// CFG 函数表条目数。
    pub guard_cf_function_count: u64,
    /// 原始 `GuardFlags`。
    pub guard_flags: u32,
    /// 解码成稳定名称的 `GuardFlags`。
    pub guard_flags_text: Vec<String>,
    /// `CodeIntegrity` 块。
    pub code_integrity: PeCodeIntegrityInfo,
    /// 声明大小是否截断了文档化前缀。
    pub truncated: bool,
}

impl PeLoadConfigReport {
    fn empty(is64: bool) -> Self {
        Self {
            present: false,
            is64,
            size: 0,
            timestamp: 0,
            major_version: 0,
            minor_version: 0,
            global_flags_clear: 0,
            global_flags_set: 0,
            critical_section_default_timeout: 0,
            de_commit_free_block_threshold: 0,
            de_commit_total_free_threshold: 0,
            lock_prefix_table: 0,
            maximum_allocation_size: 0,
            virtual_memory_threshold: 0,
            process_affinity_mask: 0,
            process_heap_flags: 0,
            csd_version: 0,
            dependent_load_flags: 0,
            edit_list: 0,
            security_cookie: 0,
            se_handler_table: 0,
            se_handler_count: 0,
            guard_cf_check_function_pointer: 0,
            guard_cf_dispatch_function_pointer: 0,
            guard_cf_function_table: 0,
            guard_cf_function_count: 0,
            guard_flags: 0,
            guard_flags_text: Vec::new(),
            code_integrity: PeCodeIntegrityInfo { flags: 0, catalog: 0, catalog_offset: 0 },
            truncated: false,
        }
    }
}

/// 解码一个 PE 镜像的加载配置目录。
pub fn read_load_config(image: &PeImage, bytes: &[u8]) -> Result<PeLoadConfigReport, PeAnalysisError> {
    let is64 = image.optional.magic == 0x20b;
    let directory = image.optional.data_directories[DIRECTORY_LOAD_CONFIG];
    if directory.rva == 0 || directory.size == 0 {
        return Ok(PeLoadConfigReport::empty(is64));
    }
    let base =
        usize::try_from(rva_to_offset(&image.sections, directory.rva).map_err(|_| PeAnalysisError::Rva(directory.rva))?).unwrap_or(usize::MAX);

    let mut report = PeLoadConfigReport::empty(is64);
    report.present = true;
    report.size = read_u32(bytes, base).unwrap_or(0);
    report.timestamp = read_u32(bytes, base + 4).unwrap_or(0);
    report.major_version = read_u16(bytes, base + 8).unwrap_or(0);
    report.minor_version = read_u16(bytes, base + 10).unwrap_or(0);
    report.global_flags_clear = read_u32(bytes, base + 12).unwrap_or(0);
    report.global_flags_set = read_u32(bytes, base + 16).unwrap_or(0);
    report.critical_section_default_timeout = read_u32(bytes, base + 20).unwrap_or(0);

    let expected = if is64 { 0xa0 } else { 0x74 };
    report.truncated = (report.size as usize) < expected;

    if is64 {
        report.de_commit_free_block_threshold = read_u64(bytes, base + 0x18).unwrap_or(0);
        report.de_commit_total_free_threshold = read_u64(bytes, base + 0x20).unwrap_or(0);
        report.lock_prefix_table = read_u64(bytes, base + 0x28).unwrap_or(0);
        report.maximum_allocation_size = read_u64(bytes, base + 0x30).unwrap_or(0);
        report.virtual_memory_threshold = read_u64(bytes, base + 0x38).unwrap_or(0);
        report.process_affinity_mask = read_u64(bytes, base + 0x40).unwrap_or(0);
        report.process_heap_flags = read_u32(bytes, base + 0x48).unwrap_or(0);
        report.csd_version = read_u16(bytes, base + 0x4c).unwrap_or(0);
        report.dependent_load_flags = read_u16(bytes, base + 0x4e).unwrap_or(0);
        report.edit_list = read_u64(bytes, base + 0x50).unwrap_or(0);
        report.security_cookie = read_u64(bytes, base + 0x58).unwrap_or(0);
        report.se_handler_table = read_u64(bytes, base + 0x60).unwrap_or(0);
        report.se_handler_count = read_u64(bytes, base + 0x68).unwrap_or(0);
        report.guard_cf_check_function_pointer = read_u64(bytes, base + 0x70).unwrap_or(0);
        report.guard_cf_dispatch_function_pointer = read_u64(bytes, base + 0x78).unwrap_or(0);
        report.guard_cf_function_table = read_u64(bytes, base + 0x80).unwrap_or(0);
        report.guard_cf_function_count = read_u64(bytes, base + 0x88).unwrap_or(0);
        report.guard_flags = read_u32(bytes, base + 0x90).unwrap_or(0);
        report.code_integrity = PeCodeIntegrityInfo {
            flags: read_u16(bytes, base + 0x94).unwrap_or(0),
            catalog: read_u16(bytes, base + 0x96).unwrap_or(0),
            catalog_offset: read_u32(bytes, base + 0x98).unwrap_or(0),
        };
    }
    else {
        report.de_commit_free_block_threshold = u64::from(read_u32(bytes, base + 0x18).unwrap_or(0));
        report.de_commit_total_free_threshold = u64::from(read_u32(bytes, base + 0x1c).unwrap_or(0));
        report.lock_prefix_table = u64::from(read_u32(bytes, base + 0x20).unwrap_or(0));
        report.maximum_allocation_size = u64::from(read_u32(bytes, base + 0x24).unwrap_or(0));
        report.virtual_memory_threshold = u64::from(read_u32(bytes, base + 0x28).unwrap_or(0));
        report.process_heap_flags = read_u32(bytes, base + 0x2c).unwrap_or(0);
        report.process_affinity_mask = u64::from(read_u32(bytes, base + 0x30).unwrap_or(0));
        report.csd_version = read_u16(bytes, base + 0x34).unwrap_or(0);
        report.dependent_load_flags = read_u16(bytes, base + 0x36).unwrap_or(0);
        report.edit_list = u64::from(read_u32(bytes, base + 0x38).unwrap_or(0));
        report.security_cookie = u64::from(read_u32(bytes, base + 0x3c).unwrap_or(0));
        report.se_handler_table = u64::from(read_u32(bytes, base + 0x40).unwrap_or(0));
        report.se_handler_count = u64::from(read_u32(bytes, base + 0x44).unwrap_or(0));
        report.guard_cf_check_function_pointer = u64::from(read_u32(bytes, base + 0x48).unwrap_or(0));
        report.guard_cf_dispatch_function_pointer = u64::from(read_u32(bytes, base + 0x4c).unwrap_or(0));
        report.guard_cf_function_table = u64::from(read_u32(bytes, base + 0x50).unwrap_or(0));
        report.guard_cf_function_count = u64::from(read_u32(bytes, base + 0x54).unwrap_or(0));
        report.guard_flags = read_u32(bytes, base + 0x58).unwrap_or(0);
        report.code_integrity = PeCodeIntegrityInfo {
            flags: read_u16(bytes, base + 0x5c).unwrap_or(0),
            catalog: read_u16(bytes, base + 0x5e).unwrap_or(0),
            catalog_offset: read_u32(bytes, base + 0x60).unwrap_or(0),
        };
    }
    report.guard_flags_text = guard_flag_names(report.guard_flags);
    Ok(report)
}

fn guard_flag_names(flags: u32) -> Vec<String> {
    let known = [
        (IMAGE_GUARD_CF_INSTRUMENTED, "CF_INSTRUMENTED"),
        (IMAGE_GUARD_CFW_INSTRUMENTED, "CFW_INSTRUMENTED"),
        (IMAGE_GUARD_CF_FUNCTION_TABLE_PRESENT, "CF_FUNCTION_TABLE_PRESENT"),
        (IMAGE_GUARD_SECURITY_COOKIE_UNUSED, "SECURITY_COOKIE_UNUSED"),
        (IMAGE_GUARD_PROTECT_DELAYLOAD_IAT, "PROTECT_DELAYLOAD_IAT"),
        (IMAGE_GUARD_DELAYLOAD_IAT_IN_ITS_OWN_SECTION, "DELAYLOAD_IAT_IN_ITS_OWN_SECTION"),
        (IMAGE_GUARD_CF_EXPORT_SUPPRESSION_INFO_PRESENT, "CF_EXPORT_SUPPRESSION_INFO_PRESENT"),
        (IMAGE_GUARD_CF_ENABLE_EXPORT_SUPPRESSION, "CF_ENABLE_EXPORT_SUPPRESSION"),
        (IMAGE_GUARD_CF_LONGJUMP_TABLE_PRESENT, "CF_LONGJUMP_TABLE_PRESENT"),
        (IMAGE_GUARD_RF_INSTRUMENTED, "RF_INSTRUMENTED"),
        (IMAGE_GUARD_RF_ENABLE, "RF_ENABLE"),
        (IMAGE_GUARD_RF_STRICT, "RF_STRICT"),
        (IMAGE_GUARD_RETPOLINE_PRESENT, "RETPOLINE_PRESENT"),
        (IMAGE_GUARD_EH_CONTINUATION_TABLE_PRESENT, "EH_CONTINUATION_TABLE_PRESENT"),
        (IMAGE_GUARD_XFG_ENABLED, "XFG_ENABLED"),
        (IMAGE_GUARD_CASTGUARD_PRESENT, "CASTGUARD_PRESENT"),
        (IMAGE_GUARD_MEMCPY_PRESENT, "MEMCPY_PRESENT"),
    ];
    known.into_iter().filter(|(bit, _)| flags & bit != 0).map(|(_, name)| name.to_string()).collect()
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?))
}
