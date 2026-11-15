//! 单列 `UNIQUE` 二级索引的键编码。
//!
//! 每个索引占用一个独立 namespace（`{表 namespace}/idx/{field_id}`），条目形如
//! `0x01 ++ payload ++ 0x00`，值是该行的 rowid。`0x01` 前缀把条目与
//! [`READY_KEY`] 一类元数据分开，`0x00` 终结符让同一 namespace 里不存在「一个键是
//! 另一个键的前缀」的情况，因此等值与范围都能用 `after` / `before` 游标精确表达。
//!
//! payload 必须是保序编码：整数用符号位翻转的大端 8 字节，文本按 UTF-8 字节
//! （与 [`crate::sql_execute::cmp_string`] 的 code point 序一致）。
//!
//! 这里只有编码，不碰存储。planner 用同一份编码生成探测键，执行层用同一份编码
//! 维护条目，两侧不会各写一套。

use crate::catalog_bind::SqlFieldType;

/// 索引条目的前缀；范围扫描以它为 prefix。
pub const ENTRY_PREFIX: u8 = 0x01;

/// 索引条目的终结符。
pub const ENTRY_TERMINATOR: u8 = 0x00;

/// 索引完备标记。存在即表示「该索引覆盖表的全部行」，planner 才敢用它。
///
/// 以 `0x00` 开头，永远不会落进 `0x01` 开头的条目范围。
pub const READY_KEY: &[u8] = b"\x00ready";

/// 单列唯一索引所在的 namespace。
pub fn index_namespace(table_namespace: &str, field_id: u64) -> String {
    format!("{table_namespace}/idx/{field_id}")
}

/// 该列类型是否支持二级索引。
///
/// 只支持整数与文本：整数是保序大端编码，文本是 UTF-8 字节序。其余类型
/// （浮点、布尔、日期、未知）保持全表扫描，避免浮点格式化与布尔字面量的口径差异。
pub fn index_supported(field_type: &SqlFieldType) -> bool {
    matches!(field_type.inner(), SqlFieldType::Int32 | SqlFieldType::Int64 | SqlFieldType::String)
}

/// 该列是否支持带上下界的索引范围扫描（只有定长保序编码可以）。
pub fn range_supported(field_type: &SqlFieldType) -> bool {
    matches!(field_type.inner(), SqlFieldType::Int32 | SqlFieldType::Int64)
}

/// 一个已存值在索引里的 payload。
///
/// - `Ok(None)` —— SQL `NULL`：`UNIQUE` 允许多行 NULL，不写条目。
/// - `Ok(Some(payload))` —— 可编码。
/// - `Err(())` —— 该值无法按列类型编码。调用方必须让索引退回不可用状态，
///   不能悄悄少写一条，否则读过索引的结果集会漏行。
pub fn encode_value(bytes: &[u8], field_type: &SqlFieldType) -> Result<Option<Vec<u8>>, ()> {
    match field_type.inner() {
        SqlFieldType::Int32 | SqlFieldType::Int64 => {
            let text = std::str::from_utf8(bytes).map_err(|_| ())?;
            let value: i64 = text.trim().parse().map_err(|_| ())?;
            Ok(Some(encode_integer(value)))
        }
        SqlFieldType::String => Ok(Some(bytes.to_vec())),
        _ => Err(()),
    }
}

/// 整数保序编码：符号位翻转后的大端字节，数值序 == 字节序。
pub fn encode_integer(value: i64) -> Vec<u8> {
    ((value as u64) ^ (1_u64 << 63)).to_be_bytes().to_vec()
}

/// 一个 payload 对应的完整条目键（含前缀与终结符）。
pub fn entry_key(payload: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(payload.len() + 2);
    key.push(ENTRY_PREFIX);
    key.extend_from_slice(payload);
    key.push(ENTRY_TERMINATOR);
    key
}

/// 恰好包含该 payload 一条条目的下界（exclusive）：它是所有 `>= value` 条目的前缀。
pub fn inclusive_lower_cursor(payload: &[u8]) -> Vec<u8> {
    let mut cursor = Vec::with_capacity(payload.len() + 1);
    cursor.push(ENTRY_PREFIX);
    cursor.extend_from_slice(payload);
    cursor
}

/// 恰好排除该 payload 条目的下界（exclusive）。
pub fn exclusive_lower_cursor(payload: &[u8]) -> Vec<u8> {
    entry_key(payload)
}

/// 恰好包含该 payload 条目的上界（exclusive）。
///
/// 只对定长 payload（整数）成立：`0x01` 大于终结符 `0x00`，而下一个 payload 的首字节
/// 与当前 payload 已不同，因此中间不存在别的条目。
pub fn inclusive_upper_cursor(payload: &[u8]) -> Vec<u8> {
    let mut cursor = inclusive_lower_cursor(payload);
    cursor.push(ENTRY_PREFIX);
    cursor
}

/// 恰好排除该 payload 条目的上界（exclusive）。
pub fn exclusive_upper_cursor(payload: &[u8]) -> Vec<u8> {
    entry_key(payload)
}
