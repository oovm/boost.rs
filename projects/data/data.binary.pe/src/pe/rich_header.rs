//! `PE` Rich 头解码。
//!
//! Rich 头是 MS 链接器写在 DOS 桩与 PE 头之间的 XOR 混淆构建戳。它以 ASCII
//! `Rich` 加四字节 XOR 密钥收尾，以 `DanS` 标记和三个等宽填充字开头；之后每个
//! 八字节条目是一个 `(compid, build)` 对与一个计数，二者都与密钥异或。解码器
//! 在 DOS 桩区域定位 `Rich`，用密钥校验 `DanS` 标记，并解码最多 64 条。已知
//! compid 会给出尽力而为的工具名；每条目始终保留原始 compid、build 与 count。
//! 没有通过校验的 Rich 头时报告 `present: false`。这里不证明工具链未被伪造。

use serde::{Deserialize, Serialize};

use super::{analysis::PeAnalysisError, read::PeImage};

/// 最多解码的条目。
const MAX_RICH_ENTRIES: usize = 64;
/// `DanS` 标记。
const DANS_MARKER: u32 = 0x536e_6144;
/// `Rich` 签名。
const RICH_SIGNATURE: &[u8] = b"Rich";

/// 一条解码后的 Rich 头条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeRichEntry {
    /// 链接器记录的组件 id。
    pub comp_id: u16,
    /// 该组件的构建号。
    pub build: u16,
    /// 该组件产出目标文件的数量。
    pub count: u32,
    /// 该 compid 的尽力工具名，未知时不存在。
    pub name: Option<String>,
}

/// 解码后的 `PE` Rich 头。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeRichHeaderReport {
    /// 是否找到通过校验的 Rich 头。
    pub present: bool,
    /// `Rich` 签名的文件偏移。
    pub rich_offset: u32,
    /// `DanS` 标记的文件偏移。
    pub dans_offset: u32,
    /// 混淆该段的 XOR 密钥。
    pub key: u32,
    /// 通过上限的条目，按段序排列。
    pub entries: Vec<PeRichEntry>,
    /// 解码段中的条目总数。
    pub total: usize,
    /// 段是否被上限截断或未按条目对齐。
    pub truncated: bool,
}

/// 解码一个 PE 镜像的 Rich 头。
pub fn read_rich_header(image: &PeImage, bytes: &[u8]) -> Result<PeRichHeaderReport, PeAnalysisError> {
    let start = 0x40usize;
    let end = usize::try_from(image.dos.pe_offset).map_err(|_| PeAnalysisError::Truncated { table: "PE 头" })?;
    let empty =
        || PeRichHeaderReport { present: false, rich_offset: 0, dans_offset: 0, key: 0, entries: Vec::new(), total: 0, truncated: false };
    if end <= start + 8 {
        return Ok(empty());
    }

    let mut rich_offset = None;
    for candidate in start..=(end - 8) {
        if bytes.get(candidate..candidate + 4).is_some_and(|window| window == RICH_SIGNATURE) {
            rich_offset = Some(candidate);
            break;
        }
    }
    let Some(rich) = rich_offset
    else {
        return Ok(empty());
    };
    let Some(key) = read_u32(bytes, rich + 4)
    else {
        return Ok(empty());
    };

    let mut dans = None;
    let mut probe = rich.saturating_sub(4);
    while probe >= start {
        if read_u32(bytes, probe).is_some_and(|value| value ^ key == DANS_MARKER) {
            dans = Some(probe);
            break;
        }
        if probe < start + 4 {
            break;
        }
        probe -= 4;
    }
    let Some(dans) = dans
    else {
        return Ok(empty());
    };

    let run_start = dans + 16;
    let mut truncated = false;
    let run_bytes = rich.saturating_sub(run_start);
    if !run_bytes.is_multiple_of(8) {
        truncated = true;
    }
    let total = run_bytes / 8;
    if total > MAX_RICH_ENTRIES {
        truncated = true;
    }
    let mut entries = Vec::new();
    for index in 0..total.min(MAX_RICH_ENTRIES) {
        let at = run_start + index * 8;
        let (Some(id), Some(count)) = (read_u32(bytes, at), read_u32(bytes, at + 4))
        else {
            truncated = true;
            break;
        };
        let id = id ^ key;
        let count = count ^ key;
        entries.push(PeRichEntry {
            comp_id: (id & 0xffff) as u16,
            build: (id >> 16) as u16,
            count,
            name: comp_id_name((id & 0xffff) as u16).map(str::to_owned),
        });
    }
    Ok(PeRichHeaderReport { present: true, rich_offset: rich as u32, dans_offset: dans as u32, key, entries, total, truncated })
}

fn comp_id_name(comp_id: u16) -> Option<&'static str> {
    match comp_id {
        1 => Some("Microsoft Visual C++ 5.0"),
        2 => Some("Microsoft Visual C++ 6.0"),
        155 => Some("Microsoft Visual C++ 14.x (VS2017-2022)"),
        216 | 218 | 220 | 222 | 224 | 226 | 228 | 233 | 239 | 243 => Some("Microsoft C/C++ linker or CRT (VS2017-2022 era)"),
        _ => None,
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?))
}
