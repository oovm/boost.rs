//! `PE` 缓解姿态：checksec 风格的摘要。
//!
//! 所有信息来自可选头、节表与加载配置目录。本报告只陈述事实，不声称某项缓解
//! 在运行期真的保护了进程。

use serde::{Deserialize, Serialize};

use super::{
    analysis::PeAnalysisError,
    read::{PeImage, rva_to_offset},
};

const DIRECTORY_SECURITY: usize = 4;
const DIRECTORY_BASE_RELOCATION: usize = 5;
const DIRECTORY_LOAD_CONFIG: usize = 10;

const DYNAMIC_BASE: u16 = 0x0040;
const HIGH_ENTROPY_VA: u16 = 0x0020;
const NX_COMPAT: u16 = 0x0100;
const GUARD_CF: u16 = 0x4000;
const FORCE_INTEGRITY: u16 = 0x0080;
const NO_SEH: u16 = 0x0400;
const TERMINAL_SERVER_AWARE: u16 = 0x8000;

const SECTION_MEM_EXECUTE: u32 = 0x2000_0000;
const SECTION_MEM_WRITE: u32 = 0x8000_0000;

const LOAD_CONFIG_COOKIE32: usize = 0x3c;
const LOAD_CONFIG_COOKIE64: usize = 0x58;
const LOAD_CONFIG_GUARD_FLAGS32: usize = 0x58;
const LOAD_CONFIG_GUARD_FLAGS64: usize = 0x90;
const GUARD_CF_INSTRUMENTED: u32 = 0x100;

/// 一个同时具备写与执行权限的节。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeWritableExecutableSection {
    /// 节名。
    pub name: String,
    /// 虚拟地址。
    pub address: u32,
    /// 虚拟大小。
    pub size: u32,
}

/// `PE` 缓解姿态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeMitigationsReport {
    /// 原始 `DllCharacteristics`。
    pub dll_characteristics: u16,
    /// 是否设置了 `DYNAMIC_BASE`（ASLR）。
    pub aslr: bool,
    /// 是否设置了 `HIGH_ENTROPY_VA`。
    pub high_entropy_va: bool,
    /// 是否设置了 `NX_COMPAT`（DEP）。
    pub dep: bool,
    /// 头里是否设置了 `GUARD_CF`，或加载配置对 CFG 做了插桩。
    pub cfg: bool,
    /// 是否设置了 `FORCE_INTEGRITY`。
    pub force_integrity: bool,
    /// 是否设置了 `NO_SEH`。
    pub no_seh: bool,
    /// 是否设置了 `TERMINAL_SERVER_AWARE`。
    pub terminal_server_aware: bool,
    /// 基址重定位目录是否非空。
    pub relocations_present: bool,
    /// 证书表是否非空；签名未经验证。
    pub signed: bool,
    /// 同时具备写与执行权限的节。
    pub writable_executable_sections: Vec<PeWritableExecutableSection>,
    /// 加载配置安全 cookie 指针是否非零；没有加载配置时为 `None`。
    pub security_cookie: Option<bool>,
    /// 加载配置中的原始 `GuardFlags`，存在时给出。
    pub guard_flags: Option<u32>,
}

/// 汇总一个 PE 镜像的缓解姿态。
pub fn read_mitigations(image: &PeImage, bytes: &[u8]) -> Result<PeMitigationsReport, PeAnalysisError> {
    let characteristics = image.optional.dll_characteristics;
    let directories = &image.optional.data_directories;

    let writable_executable_sections = image
        .sections
        .iter()
        .filter(|section| section.characteristics & SECTION_MEM_EXECUTE != 0 && section.characteristics & SECTION_MEM_WRITE != 0)
        .map(|section| PeWritableExecutableSection { name: section.name.clone(), address: section.virtual_address, size: section.virtual_size })
        .collect::<Vec<_>>();

    let relocations_present = directories[DIRECTORY_BASE_RELOCATION].size > 0;
    let signed = directories[DIRECTORY_SECURITY].size > 0;
    let (security_cookie, guard_flags) = load_config(bytes, image);

    Ok(PeMitigationsReport {
        dll_characteristics: characteristics,
        aslr: characteristics & DYNAMIC_BASE != 0,
        high_entropy_va: characteristics & HIGH_ENTROPY_VA != 0,
        dep: characteristics & NX_COMPAT != 0,
        cfg: characteristics & GUARD_CF != 0 || guard_flags.is_some_and(|flags| flags & GUARD_CF_INSTRUMENTED != 0),
        force_integrity: characteristics & FORCE_INTEGRITY != 0,
        no_seh: characteristics & NO_SEH != 0,
        terminal_server_aware: characteristics & TERMINAL_SERVER_AWARE != 0,
        relocations_present,
        signed,
        writable_executable_sections,
        security_cookie,
        guard_flags,
    })
}

/// 读取加载配置的安全 cookie 与 `GuardFlags` 字段。
fn load_config(bytes: &[u8], image: &PeImage) -> (Option<bool>, Option<u32>) {
    let directory = image.optional.data_directories[DIRECTORY_LOAD_CONFIG];
    if directory.rva == 0 || directory.size == 0 {
        return (None, None);
    }
    let Ok(base) = rva_to_offset(&image.sections, directory.rva)
    else {
        return (None, None);
    };
    let base = base as usize;
    let is_64 = image.optional.magic == 0x20b;
    let cookie = if is_64 { read_u64(bytes, base + LOAD_CONFIG_COOKIE64) } else { read_u32(bytes, base + LOAD_CONFIG_COOKIE32).map(u64::from) };
    let guard_flags = if is_64 { read_u32(bytes, base + LOAD_CONFIG_GUARD_FLAGS64) } else { read_u32(bytes, base + LOAD_CONFIG_GUARD_FLAGS32) };
    (cookie.map(|value| value != 0), guard_flags)
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(offset..offset.checked_add(8)?)?.try_into().ok()?))
}
