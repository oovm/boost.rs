//! 二级索引的键编码（单列，唯一与非唯一共用一套布局）。
//!
//! 每个索引占用一个独立 namespace（`{表 namespace}/idx2/{field_id}`）。条目键为
//!
//! ```text
//! 0x01 ++ escape(payload) ++ 0x00 0x00 ++ rowid
//! ```
//!
//! - `0x01` 前缀把条目与 [`READY_KEY`] 一类元数据分开；
//! - `payload` 内的 `0x00` 一律转义成 `0x00 0xFF`，因此 `0x00 0x00` 只可能是
//!   payload 结束符，读取时能从键里精确切出 `rowid`；
//! - `rowid` 放在键尾而不是值里：非唯一索引的同一列值会有多行，键必须唯一，
//!   否则后写的一行会覆盖前一行，索引变成漏行过滤器。
//!
//! 转义只把 `0x00` 拉长成 `0x00 0xFF`，其余字节原样保留，且 `0x00 0x00 < 0x00 0xFF`
//! 小于任何「0x00 后接更大字节」的形态，因此 payload 的字节序与原始值一致：
//! 整数按符号位翻转的大端 8 字节保序，文本按 UTF-8 字节序（与 SQL 的 code point 序一致）。
//!
//! 这里只有编码，不碰存储。planner 用同一份编码生成探测边界，执行层用同一份编码
//! 维护条目，两侧不会各写一套。

use crate::catalog_bind::SqlFieldType;

/// 索引条目的前缀；范围扫描以它为 prefix。
pub const ENTRY_PREFIX: u8 = 0x01;

/// payload 结束符（转义后 `0x00` 只会以 `0x00 0xFF` 出现）。
const VALUE_TERMINATOR: [u8; 2] = [0x00, 0x00];

/// 转义标记：payload 里的 `0x00` 写成 `0x00 0xFF`。
const ESCAPE_MARK: u8 = 0xFF;

/// 索引完备标记。存在即表示「该索引覆盖表的全部行」，planner 才敢用它。
///
/// 以 `0x00` 开头，永远不会落进 `0x01` 开头的条目范围。
pub const READY_KEY: &[u8] = b"\x00ready";

/// 单列索引所在的 namespace。
///
/// 布局与旧版（值里存 rowid）不兼容，因此 namespace 带 `idx2`：旧条目留在旧
/// namespace 里自然失效，读路径只按完备标记决定是否使用索引。
pub fn index_namespace(table_namespace: &str, field_id: u64) -> String {
    format!("{table_namespace}/idx2/{field_id}")
}

/// 该列类型是否支持二级索引。
///
/// 支持整数、文本与 uuid：整数是保序大端编码，文本是 UTF-8 字节序，uuid 是
/// 16 字节大端原文（字节序与规范文本序一致）。其余类型（浮点、布尔、日期、
/// 未知）保持全表扫描，避免浮点格式化与布尔字面量的口径差异。
pub fn index_supported(field_type: &SqlFieldType) -> bool {
    matches!(
        field_type.inner(),
        SqlFieldType::Int16 | SqlFieldType::Int32 | SqlFieldType::Int64 | SqlFieldType::String | SqlFieldType::Uuid
    )
}

/// 该列是否支持带上下界的索引范围扫描（只有定长保序编码可以）。
pub fn range_supported(field_type: &SqlFieldType) -> bool {
    matches!(field_type.inner(), SqlFieldType::Int16 | SqlFieldType::Int32 | SqlFieldType::Int64)
}

/// 一个已存值在索引里的 payload。
///
/// 输入是按列类型解释后的 [`SqlValue`]，不是行载荷字节：legacy 行与标签行
/// 的存储字节不同，索引必须按逻辑值编码才能与探测边界对齐。
///
/// - `Ok(None)` —— SQL `NULL`：任何索引都不为 NULL 写条目，等值探测也匹配不到 NULL。
/// - `Ok(Some(payload))` —— 可编码。
/// - `Err(())` —— 该值无法按列类型编码。调用方必须让索引退回不可用状态，
///   不能悄悄少写一条，否则读过索引的结果集会漏行。
pub fn encode_value(value: &crate::SqlValue, field_type: &SqlFieldType) -> Result<Option<Vec<u8>>, ()> {
    match (value, field_type.inner()) {
        (crate::SqlValue::Null, _) => Ok(None),
        (_, SqlFieldType::Int16 | SqlFieldType::Int32 | SqlFieldType::Int64) => match value {
            crate::SqlValue::Integer(integer) => Ok(Some(encode_integer(*integer))),
            _ => Err(()),
        },
        (_, SqlFieldType::String) => match value {
            crate::SqlValue::String(text) => Ok(Some(text.as_bytes().to_vec())),
            _ => Err(()),
        },
        // uuid 在行里是 16 字节大端原文；文本形态先解析成规范字节，与行存储对齐。
        (_, SqlFieldType::Uuid) => match value {
            crate::SqlValue::Bytes(bytes) if bytes.len() == 16 => Ok(Some(bytes.clone())),
            crate::SqlValue::String(text) => crate::catalog_bind::parse_uuid(text).map(|bytes| Some(bytes.to_vec())).ok_or(()),
            _ => Err(()),
        },
        _ => Err(()),
    }
}

/// 整数保序编码：符号位翻转后的大端字节，数值序 == 字节序。
pub fn encode_integer(value: i64) -> Vec<u8> {
    ((value as u64) ^ (1_u64 << 63)).to_be_bytes().to_vec()
}

/// payload 的转义形式。
fn escape(payload: &[u8]) -> Vec<u8> {
    let mut escaped = Vec::with_capacity(payload.len() + 4);
    for byte in payload {
        escaped.push(*byte);
        if *byte == 0x00 {
            escaped.push(ESCAPE_MARK);
        }
    }
    escaped
}

/// 同一列值的条目公共前缀：`0x01 ++ escape(payload) ++ 0x00 0x00`。
pub fn value_prefix(payload: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(payload.len() + 5);
    key.push(ENTRY_PREFIX);
    key.extend_from_slice(&escape(payload));
    key.extend_from_slice(&VALUE_TERMINATOR);
    key
}

/// 完整条目键。
pub fn entry_key(payload: &[u8], rowid: &[u8]) -> Vec<u8> {
    let mut key = value_prefix(payload);
    key.extend_from_slice(rowid);
    key
}

/// 恰好越过该列值全部条目的边界（`0x01 ++ escape(payload) ++ 0x00 0x01`）。
///
/// - 作为 `after`：只保留严格大于该值的条目；
/// - 作为 `before`：只保留小于等于该值的条目。
pub fn value_upper(payload: &[u8]) -> Vec<u8> {
    let mut key = value_prefix(payload);
    let last = key.len() - 1;
    key[last] = 0x01;
    key
}

/// 从条目键里切出 `rowid`。
///
/// 键必须由 [`entry_key`] 产生；转义不当或缺少结束符时返回 `None`。
pub fn rowid_of(key: &[u8]) -> Option<&[u8]> {
    if key.first() != Some(&ENTRY_PREFIX) {
        return None;
    }
    let mut cursor = 1;
    while cursor < key.len() {
        if key[cursor] == 0x00 {
            match key.get(cursor + 1) {
                Some(0xFF) => cursor += 2,
                Some(0x00) => return key.get(cursor + 2..),
                _ => return None,
            }
        }
        else {
            cursor += 1;
        }
    }
    None
}

/// 该列值全部条目的扫描区间（`after` / `before` 皆为 exclusive）。
pub fn equality_cursors(payload: &[u8]) -> (Vec<u8>, Vec<u8>) {
    (value_prefix(payload), value_upper(payload))
}
