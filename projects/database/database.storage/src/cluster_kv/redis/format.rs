//! `.yykv` 分片快照与 `.yyds` catalog 的字节编解码。
//!
//! 字节布局归 `database-formats`，本模块只负责把 Redis 键空间映射到上游记录模型，
//! 并保留两条本节点策略：只接受内联载荷（本节点不绑定对象存储），以及显式拒绝
//! 旧版 v2 快照。错误文本是分片对外合同的一部分，因此在这里稳定成形。

use std::collections::BTreeMap;

pub use database_formats::yy::yykv::{Key, Record};
use database_formats::yy::{
    yyds,
    yykv::{self, InlineValue, StoredValue},
};
use database_types::cluster::{Error as SourceError, ShardId};

/// 以 [`Key`] 为索引的记录表。
///
/// 这就是上游记录模型本身，所以一次快照编码直接借用活映射，不必每次写回都重建副本。
pub type Records = BTreeMap<Key, Record>;

/// 一条记录的内联载荷字节。
///
/// 本节点只存内联载荷：对象引用既不会作为行字节出现，也会被 [`encode_shard`] 与
/// [`decode_shard`] 拒绝。
pub fn record_bytes(record: &Record) -> &[u8] {
    match &record.value {
        StoredValue::Inline(value) => &value.0,
        StoredValue::Object(_) => &[],
    }
}

/// 按分片分配的 revision 构造一条内联记录。
pub fn inline_record(key: Key, value: Vec<u8>, revision: u64, expires_at: Option<i64>) -> Record {
    Record { key, value: StoredValue::Inline(InlineValue(value)), revision, expires_at }
}

/// 分片字节层失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardFormatError(pub String);

impl std::fmt::Display for ShardFormatError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ShardFormatError {}

/// 分片字节层结果。
pub type ShardResult<T> = std::result::Result<T, ShardFormatError>;

/// 把记录表序列化为 `.yykv` 字节。
pub fn encode_shard(records: &Records, next_revision: u64) -> ShardResult<Vec<u8>> {
    // 策略：每条记录都是内联载荷，负截止时间是调用方的错误。上游用 `as u64` 包住
    // 负过期值，所以拒绝留在这里而不是委托出去。
    for record in records.values() {
        if record.expires_at.is_some_and(|deadline| deadline < 0) {
            return Err(ShardFormatError("expiry deadline must not be negative".to_owned()));
        }
        if matches!(record.value, StoredValue::Object(_)) {
            return Err(ShardFormatError("shared object payloads are not supported here".to_owned()));
        }
    }
    yykv::encode_snapshot(records, next_revision).map_err(|source| encode_error(&source))
}

/// 解析 [`encode_shard`] 写出的 `.yykv` 字节。
///
/// 严格解码（含 revision 合同）来自 `database-formats`。两条策略留在这里：本节点不
/// 绑定对象存储，因此只接受内联载荷；旧版 v2 快照保留分片自己的拒绝措辞。
pub fn decode_shard(bytes: &[u8]) -> ShardResult<(Records, u64)> {
    let snapshot = yykv::decode_snapshot_strict(bytes).map_err(|source| shard_error(bytes, &source))?;
    // 共享载荷引用不得冒充行字节日志。
    if snapshot.records.values().any(|record| matches!(record.value, StoredValue::Object(_))) {
        return Err(ShardFormatError("shared object payloads are not supported here".to_owned()));
    }
    Ok((snapshot.records, snapshot.next_revision))
}

/// 序列化登记单个分片的 `.yyds` catalog 字节。
pub fn encode_catalog(shard_id: &str) -> Vec<u8> {
    yyds::encode_catalog(None, None, &[ShardId(shard_id.to_owned())], None, &[])
        .expect("one shard registration is always within the catalog limits")
}

/// 结构化的 `.yyds` 校验失败：`field` 指明失败的检查项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogError {
    /// 拒绝该 catalog 的检查项，例如 `magic` 或 `shard-id`。
    pub field: &'static str,
    /// 期望值与实际字节。
    pub detail: String,
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "catalog.{}: {}", self.field, self.detail)
    }
}

impl std::error::Error for CatalogError {}

/// 解析并校验 `.yyds` catalog，返回登记的分片 id。
///
/// 解析、格式版本、单一登记规则与 `ShardId` 形状检查都来自 `database-formats`；
/// 只有拒绝措辞归本模块，以维持稳定的 `catalog.<field>: <detail>` 文本。
pub fn decode_catalog_shard_id(bytes: &[u8]) -> std::result::Result<String, CatalogError> {
    match yyds::decode_catalog_shard_id(bytes) {
        Ok(shard) => Ok(shard.0),
        Err(source) => Err(catalog_error(bytes, &source)),
    }
}

/// 把上游拒绝翻译成分片自己的稳定消息。
///
/// 上游错误只带类别不带越界值，所以带值的消息靠重读同一字段重建。这次遍历从不决定
/// catalog 是否合法，只解释上游已经做出的拒绝。
fn catalog_error(bytes: &[u8], source: &SourceError) -> CatalogError {
    let reject = |field: &'static str, detail: String| CatalogError { field, detail };
    // 历史顺序先解释头部，再解释单一登记规则。
    match source {
        SourceError::Corrupt("unknown catalog header") => {
            return reject("magic", "expected \"YYDS\\x01\"".to_owned());
        }
        SourceError::Unsupported("catalog format version requires regeneration") => {
            return reject("version", format!("expected 0, found {}", peek_u32(bytes, yyds::CATALOG_MAGIC.len()).unwrap_or(0)));
        }
        _ => {}
    }
    // 畸形计数通常表现为更晚的上游错误，因为上游只在解析完整 body 之后才计数。计数仍是
    // 两个保留段为空时最该先解释的字段。
    if let Some(count) = shard_count(bytes) {
        if count != 1 {
            return reject("count", format!("expected 1, found {count}"));
        }
    }
    match source {
        SourceError::Corrupt("catalog shard id utf8") => reject("shard-id", "not utf8".to_owned()),
        SourceError::Corrupt("trailing catalog bytes") => {
            let end = catalog_end(bytes).unwrap_or(bytes.len());
            reject("trailing", format!("expected no bytes after the shard registration, found {}", bytes.len().saturating_sub(end)))
        }
        SourceError::Corrupt("unexpected catalog eof") => reject("length", catalog_end(bytes).err().unwrap_or("truncated catalog").to_owned()),
        SourceError::Corrupt("invalid catalog schema presence flag") => reject("schema", "invalid presence flag".to_owned()),
        SourceError::Corrupt("invalid catalog routing presence flag") => reject("routing", "invalid presence flag".to_owned()),
        SourceError::Corrupt("invalid catalog routing epoch") => reject("routing", "epoch must be nonzero".to_owned()),
        SourceError::Unsupported("catalog registers no shard") => {
            reject("count", format!("expected 1, found {}", shard_count(bytes).unwrap_or(0)))
        }
        SourceError::Unsupported("catalog registers more than one shard") => {
            reject("count", format!("expected 1, found {}", shard_count(bytes).unwrap_or(1)))
        }
        SourceError::InvalidShardId { value } if value.is_empty() => reject("shard-id", "empty".to_owned()),
        SourceError::InvalidShardId { value } if value.contains(['/', '\\']) => reject("shard-id", "contains a path separator".to_owned()),
        SourceError::InvalidShardId { .. } => reject("shard-id", "not a single path segment".to_owned()),
        other => reject("catalog", format!("{other}")),
    }
}

/// 读取两个保留段为空的 catalog 的分片计数。
fn shard_count(bytes: &[u8]) -> Option<u32> {
    let magic = yyds::CATALOG_MAGIC.len();
    if peek_u32(bytes, magic + 4)? != 0 || peek_u32(bytes, magic + 8)? != 0 {
        return None;
    }
    peek_u32(bytes, magic + 12)
}

/// 沿上游 catalog 布局走一遍，返回 catalog body 的结束位置。
fn catalog_end(bytes: &[u8]) -> std::result::Result<usize, &'static str> {
    let mut cursor = yyds::CATALOG_MAGIC.len();
    read_u32_diagnostic(bytes, &mut cursor, "truncated version")?;
    if read_u32_diagnostic(bytes, &mut cursor, "truncated schema flag")? == 1 {
        read_u32_diagnostic(bytes, &mut cursor, "truncated schema section")?;
        skip_block(bytes, &mut cursor, "truncated schema section", "truncated schema section")?;
    }
    if read_u32_diagnostic(bytes, &mut cursor, "truncated routing section")? == 1 {
        skip(bytes, &mut cursor, 8, "truncated routing section")?;
    }
    let count = read_u32_diagnostic(bytes, &mut cursor, "truncated shard count")?;
    for _ in 0..count {
        skip_block(bytes, &mut cursor, "truncated shard id length", "truncated shard id")?;
    }
    if read_u32_diagnostic(bytes, &mut cursor, "truncated reserved fields")? == 1 {
        skip_block(bytes, &mut cursor, "truncated resolved contract", "truncated resolved contract")?;
    }
    let owners = read_u32_diagnostic(bytes, &mut cursor, "truncated reserved fields")?;
    for _ in 0..owners {
        skip_block(bytes, &mut cursor, "truncated owners", "truncated owners")?;
        skip_block(bytes, &mut cursor, "truncated owners", "truncated owners")?;
        skip(bytes, &mut cursor, 8, "truncated owners")?;
    }
    Ok(cursor)
}

fn peek_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?))
}

fn skip(bytes: &[u8], cursor: &mut usize, length: usize, detail: &'static str) -> std::result::Result<(), &'static str> {
    if *cursor + length > bytes.len() {
        return Err(detail);
    }
    *cursor += length;
    Ok(())
}

fn read_u32_diagnostic(bytes: &[u8], cursor: &mut usize, detail: &'static str) -> std::result::Result<u32, &'static str> {
    skip(bytes, cursor, 4, detail)?;
    Ok(u32::from_le_bytes(bytes[*cursor - 4..*cursor].try_into().unwrap_or([0; 4])))
}

fn skip_block(
    bytes: &[u8],
    cursor: &mut usize,
    length_detail: &'static str,
    body_detail: &'static str,
) -> std::result::Result<(), &'static str> {
    let length = read_u32_diagnostic(bytes, cursor, length_detail)? as usize;
    skip(bytes, cursor, length, body_detail)
}

/// 把上游 `.yykv` 写入拒绝翻译成分片自己的稳定消息。
fn encode_error(source: &SourceError) -> ShardFormatError {
    match source {
        SourceError::Unsupported("yykv next_revision must be at least 1") => ShardFormatError("next revision must be at least 1".to_owned()),
        SourceError::Unsupported("yykv revision must be below next_revision") => ShardFormatError("invalid shard revision".to_owned()),
        SourceError::Unsupported("too many shard records") => ShardFormatError("too many shard records".to_owned()),
        SourceError::Unsupported("shard value too large") => ShardFormatError("shard field too large".to_owned()),
        other => ShardFormatError(other.to_string()),
    }
}

/// 把上游 `.yykv` 读取拒绝翻译成分片自己的稳定消息。
///
/// 上游错误带类别但不带耗尽字节的字段，所以截断 body 靠重读解码器同一批偏移来解释。
/// 这次遍历从不决定快照是否合法，只解释上游已经做出的拒绝。
fn shard_error(bytes: &[u8], source: &SourceError) -> ShardFormatError {
    // 历史顺序先解释头部，再解释其他字段。
    if bytes.len() < yykv::KV_MAGIC.len() {
        return ShardFormatError("truncated shard header".to_owned());
    }
    let magic = &bytes[..yykv::KV_MAGIC.len()];
    if magic == yykv::KV_LEGACY_MAGIC {
        return ShardFormatError("legacy .yykv v2 snapshot carries no checksum: delete the shard and let the node recreate it".to_owned());
    }
    if magic != yykv::KV_MAGIC {
        return ShardFormatError("unknown .yykv header".to_owned());
    }
    if bytes.len() < yykv::KV_MAGIC.len() + 8 {
        return ShardFormatError("truncated `shard checksum`".to_owned());
    }
    match source {
        SourceError::CorruptReason { message: "yykv checksum mismatch", .. } => ShardFormatError(".yykv checksum mismatch".to_owned()),
        SourceError::CorruptReason { message: "truncated yykv checksum", .. } => ShardFormatError("truncated `shard checksum`".to_owned()),
        SourceError::Corrupt("invalid shard revision") | SourceError::Corrupt("legacy shard revision space exhausted") => {
            ShardFormatError("invalid shard revision".to_owned())
        }
        SourceError::CorruptReason { message: "trailing shard bytes", .. } => ShardFormatError("trailing shard bytes".to_owned()),
        SourceError::CorruptReason { message: "duplicate shard key", .. } => ShardFormatError("duplicate shard key".to_owned()),
        SourceError::CorruptReason { message: "shard namespace utf8", .. } => ShardFormatError("shard namespace is not utf8".to_owned()),
        // 任何非内联载荷都拒绝，包括哈希字节不是 utf8 的 tag-1 记录，因为本节点两种情况
        // 都没有对象存储。
        SourceError::CorruptReason { message: "shard object hash utf8", .. }
        | SourceError::CorruptReason { message: "invalid shard value kind", .. }
        | SourceError::Unsupported("yykv object-backed value requires an object store") => {
            ShardFormatError("shared object payloads are not supported here".to_owned())
        }
        SourceError::CorruptReason { message: "legacy yykv revision layout: next_revision is zero", .. } => {
            ShardFormatError("legacy .yykv revision layout: next revision is zero".to_owned())
        }
        SourceError::CorruptReason { message: "legacy yykv revision layout: record sits on next_revision", .. } => {
            ShardFormatError("legacy .yykv revision layout: a record sits on the next revision".to_owned())
        }
        SourceError::CorruptReason { message: "unexpected shard eof" | "shard length overflow", .. } => {
            ShardFormatError(truncated_detail(bytes))
        }
        other => ShardFormatError(other.to_string()),
    }
}

/// 指出第一个耗尽字节的 `.yykv` 字段。
///
/// 只在上游已判定快照截断之后运行，因此镜像解码器的读取顺序但不产出任何解码值。
fn truncated_detail(bytes: &[u8]) -> String {
    let body = bytes.get(yykv::KV_MAGIC.len() + 8..).unwrap_or_default();
    let mut cursor = 0_usize;
    if let Err(detail) = read_scalar(body, &mut cursor, 8, "next revision") {
        return detail;
    }
    let count = match read_count(body, &mut cursor) {
        Ok(count) => count as usize,
        Err(detail) => return detail,
    };
    for _ in 0..count {
        if let Err(detail) = read_block(body, &mut cursor, "namespace") {
            return detail;
        }
        if let Err(detail) = read_block(body, &mut cursor, "key") {
            return detail;
        }
        if let Err(detail) = read_scalar(body, &mut cursor, 8, "revision") {
            return detail;
        }
        if let Err(detail) = read_scalar(body, &mut cursor, 8, "deadline") {
            return detail;
        }
        if cursor >= body.len() {
            return "unexpected end of file".to_owned();
        }
        cursor += 1;
        if let Err(detail) = read_block(body, &mut cursor, "value") {
            return detail;
        }
    }
    // 上游报告了记录遍历已经看不到的截断。
    "unexpected end of file".to_owned()
}

fn read_scalar(body: &[u8], cursor: &mut usize, length: usize, field: &str) -> std::result::Result<(), String> {
    if *cursor + length > body.len() {
        return Err(format!("truncated `{field}`"));
    }
    *cursor += length;
    Ok(())
}

fn read_count(body: &[u8], cursor: &mut usize) -> std::result::Result<u32, String> {
    if *cursor + 4 > body.len() {
        return Err("truncated `record count`".to_owned());
    }
    let value = u32::from_le_bytes(body[*cursor..*cursor + 4].try_into().unwrap_or([0; 4]));
    *cursor += 4;
    Ok(value)
}

fn read_block(body: &[u8], cursor: &mut usize, field: &str) -> std::result::Result<(), String> {
    if *cursor + 4 > body.len() {
        return Err(format!("truncated `{field}` length"));
    }
    let length = u32::from_le_bytes(body[*cursor..*cursor + 4].try_into().unwrap_or([0; 4])) as usize;
    *cursor += 4;
    if *cursor + length > body.len() {
        return Err(format!("truncated `{field}` bytes"));
    }
    *cursor += length;
    Ok(())
}
