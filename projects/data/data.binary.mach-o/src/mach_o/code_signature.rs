//! `Mach-O` 内嵌代码签名（`LC_CODE_SIGNATURE`）。
//!
//! `LC_CODE_SIGNATURE` 指向文件中的 `EmbeddedSignature` `SuperBlob`。本模块报告
//! blob 表与第一条 `CodeDirectory` 的字段（标识、团队、哈希类型、代码上限）。
//! 只解析，不校验：不重算哈希，也不构建证书链。

use super::read::MachOImage;

const EMBEDDED_SIGNATURE_MAGIC: u32 = 0xfade_0cc0;
const CODE_DIRECTORY_MAGIC: u32 = 0xfade_0c02;
const MAX_BLOBS: usize = 32;

/// 一条解码后的 `CodeDirectory` blob。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachCodeDirectory {
    /// blob 格式版本，例如 `0x20200`。
    pub version: u32,
    /// 原始 `flags`。
    pub flags: u32,
    /// `flags` 解码出的稳定标志名。
    pub flag_names: Vec<&'static str>,
    /// 哈希算法名。
    pub hash_type: String,
    /// 单个哈希的字节数。
    pub hash_size: u8,
    /// 页大小，由 log2 字段推导。
    pub page_size: u32,
    /// 签名覆盖的字节数。
    pub code_limit: u64,
    /// 签名标识，通常是 bundle id。
    pub identifier: String,
    /// 团队标识。
    pub team_id: Option<String>,
    /// 代码槽数量。
    pub code_slots: u32,
    /// 特殊槽数量。
    pub special_slots: u32,
}

/// 内嵌签名摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachCodeSignature {
    /// 是否存在 `LC_CODE_SIGNATURE`。
    pub present: bool,
    /// 签名 blob 的文件偏移。
    pub data_offset: u32,
    /// 声明的签名大小。
    pub data_size: u32,
    /// `SuperBlob` 魔数的稳定名称。
    pub magic: Option<String>,
    /// `SuperBlob` 索引中的条目数量。
    pub blob_count: Option<u32>,
    /// 索引顺序的 blob 类型名。
    pub blob_types: Vec<String>,
    /// 第一条 `CodeDirectory`。
    pub code_directory: Option<MachCodeDirectory>,
    /// 是否因上限或畸形结构截断。
    pub truncated: bool,
}

/// 解码 `Mach-O` 的内嵌代码签名。
pub fn read_code_signature(bytes: &[u8], image: &MachOImage) -> MachCodeSignature {
    let mut report = MachCodeSignature {
        present: false,
        data_offset: 0,
        data_size: 0,
        magic: None,
        blob_count: None,
        blob_types: Vec::new(),
        code_directory: None,
        truncated: false,
    };
    let Some(command) = image.code_signature
    else {
        return report;
    };
    report.present = true;
    report.data_offset = command.data_offset;
    report.data_size = command.data_size;

    let base = command.data_offset as usize;
    if base >= bytes.len() {
        report.truncated = true;
        return report;
    }
    let end = base.saturating_add(command.data_size as usize).min(bytes.len());
    if end < base.saturating_add(command.data_size as usize) {
        report.truncated = true;
    }
    let blob = &bytes[base..end];
    let Some(magic) = be_u32(blob, 0)
    else {
        report.truncated = true;
        return report;
    };
    if magic != EMBEDDED_SIGNATURE_MAGIC {
        report.magic = Some(format!("0x{magic:08x}"));
        return report;
    }
    report.magic = Some("embedded-signature".to_string());
    let count = be_u32(blob, 8).unwrap_or(0);
    report.blob_count = Some(count);

    for index in 0..(count as usize).min(MAX_BLOBS) {
        let entry = 12 + index * 8;
        let (Some(kind), Some(relative)) = (be_u32(blob, entry), be_u32(blob, entry + 4))
        else {
            report.truncated = true;
            break;
        };
        let relative = relative as usize;
        if relative >= blob.len() {
            report.truncated = true;
            continue;
        }
        report.blob_types.push(blob_type_name(kind));
        if kind == 0 && report.code_directory.is_none() {
            report.code_directory = decode_code_directory(&blob[relative..]);
        }
    }
    if count as usize > MAX_BLOBS {
        report.truncated = true;
    }
    report
}

fn decode_code_directory(blob: &[u8]) -> Option<MachCodeDirectory> {
    if be_u32(blob, 0)? != CODE_DIRECTORY_MAGIC {
        return None;
    }
    let version = be_u32(blob, 8)?;
    let flags = be_u32(blob, 12)?;
    let identifier_offset = be_u32(blob, 20)? as usize;
    let special_slots = be_u32(blob, 24)?;
    let code_slots = be_u32(blob, 28)?;
    let code_limit = be_u32(blob, 32)?;
    let hash_size = *blob.get(36)?;
    let hash_type = *blob.get(37)?;
    let page_shift = *blob.get(39)?;
    let team_offset = if version >= 0x20200 { be_u32(blob, 48) } else { None };
    let code_limit64 = if version >= 0x20300 { be_u64(blob, 56) } else { None };
    Some(MachCodeDirectory {
        version,
        flags,
        flag_names: flag_names(flags),
        hash_type: hash_type_name(hash_type),
        hash_size,
        page_size: if page_shift == 0 { 0 } else { 1u32 << page_shift.min(31) },
        code_limit: code_limit64.filter(|value| *value != 0).unwrap_or(u64::from(code_limit)),
        identifier: c_string(blob, identifier_offset).unwrap_or_default(),
        team_id: team_offset.filter(|value| *value != 0).and_then(|offset| c_string(blob, offset as usize)),
        code_slots,
        special_slots,
    })
}

fn blob_type_name(kind: u32) -> String {
    match kind {
        0 => "CodeDirectory".to_string(),
        1 => "Info.plist".to_string(),
        2 => "Requirements".to_string(),
        3 => "ResourceDir".to_string(),
        4 => "Application".to_string(),
        5 => "Entitlements".to_string(),
        6 => "DMG".to_string(),
        7 => "DER Entitlements".to_string(),
        8 => "LaunchConstraint".to_string(),
        other => format!("0x{other:08x}"),
    }
}

fn hash_type_name(kind: u8) -> String {
    match kind {
        0 => "none".to_string(),
        1 => "SHA1".to_string(),
        2 => "SHA256".to_string(),
        3 => "SHA256-truncated".to_string(),
        4 => "SHA384".to_string(),
        5 => "SHA512".to_string(),
        other => format!("0x{other:02x}"),
    }
}

fn flag_names(flags: u32) -> Vec<&'static str> {
    const KNOWN: [(u32, &str); 4] =
        [(0x0000_0001, "ADHOC"), (0x0000_0002, "HARD"), (0x0000_0100, "HOST"), (0x0000_0200, "LINKER_SIGNED")];
    KNOWN.into_iter().filter(|(bit, _)| flags & bit != 0).map(|(_, name)| name).collect()
}

fn c_string(bytes: &[u8], offset: usize) -> Option<String> {
    let tail = bytes.get(offset..)?;
    let end = tail.iter().position(|byte| *byte == 0).unwrap_or(tail.len());
    Some(String::from_utf8_lossy(&tail[..end]).into_owned())
}

fn be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?))
}

fn be_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.get(offset..offset + 8)?.try_into().ok()?))
}
