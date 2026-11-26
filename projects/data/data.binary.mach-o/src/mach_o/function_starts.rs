//! `LC_FUNCTION_STARTS` 的 ULEB128 增量表。

use super::leb::read_uleb;
use super::read::{MachOImage, MachOParseError};

/// `LC_FUNCTION_STARTS` 的有界函数起始地址列表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachFunctionStarts {
    /// 增量累加的基准：`__TEXT` 段的虚拟地址。
    pub image_base: u64,
    /// 按表顺序的绝对函数起始地址。
    pub addresses: Vec<u64>,
    /// 编码表声明的条目总数。
    pub total: usize,
    /// 是否因上限移除地址。
    pub truncated: bool,
}

/// 解码 `Mach-O` 的 `LC_FUNCTION_STARTS` 增量表。
///
/// 没有该命令时返回 [`MachOParseError::FunctionStartsMissing`]。
pub fn read_function_starts(
    bytes: &[u8],
    image: &MachOImage,
    limit: Option<usize>,
) -> Result<MachFunctionStarts, MachOParseError> {
    let table = image.function_starts.ok_or(MachOParseError::FunctionStartsMissing)?;
    let start = table.data_offset as usize;
    let end = start.saturating_add(table.data_size as usize);
    let data = bytes.get(start..end).unwrap_or(&[]);
    let image_base = image
        .segments
        .iter()
        .find(|segment| segment.name == "__TEXT")
        .map_or(0, |segment| segment.vm_address);

    let limit = limit.unwrap_or(usize::MAX);
    let mut addresses = Vec::new();
    let mut total = 0usize;
    let mut cursor = 0usize;
    let mut current = image_base;
    let mut truncated = end > bytes.len();
    while cursor < data.len() {
        let Some((delta, next)) = read_uleb(data, cursor)
        else {
            truncated = true;
            break;
        };
        cursor = next;
        if delta == 0 {
            break;
        }
        current = current.saturating_add(delta);
        total += 1;
        if addresses.len() < limit {
            addresses.push(current);
        }
    }
    truncated |= total > addresses.len();
    Ok(MachFunctionStarts { image_base, addresses, total, truncated })
}
