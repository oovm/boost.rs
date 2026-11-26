//! `dyld` 数据流使用的 ULEB128 / SLEB128 读取器。

/// 读取 ULEB128，返回值与下一位置。
pub(crate) fn read_uleb(data: &[u8], offset: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0u32;
    let mut cursor = offset;
    loop {
        let byte = *data.get(cursor)?;
        cursor += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some((value, cursor));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}
