//! Redis 键空间的命令执行面：把 [`KeyValueCommand`] 落到 [`RedisShard`]，并承载
//! hash / list / set / zset / HyperLogLog 的命令语义。
//!
//! 值种类命名空间解析、扁平值种类写入与删除由 [`RedisExecutor`] 表达；集合命令家族与
//! 阻塞等待表在 [`collections`] / [`blocking`] 内，字符串与键空间命令族在 [`string`]，
//! 事务会话在 [`session`]，认证状态机在 [`auth`]，持久化与生命周期计划在
//! [`persistence`]，`WAIT` / `LATENCY` 计划在 [`introspection`]，回复分帧 primitives 在
//! [`reply`]。

use std::sync::Mutex;

use database_query::cluster_kv::{KeyValueAction, KeyValueCommand, KeyValueResult};
use database_storage::cluster_kv::redis::{IncrementError, KINDS, NAMESPACE_PREFIX, RedisShard, RenameOutcome, format::record_bytes};
use database_types::cluster::{Error, FenceToken, Namespace};

use super::KeyValueExecutor;

pub use database_storage::cluster_kv::redis::check_kind;

pub mod auth;
pub mod blocking;
pub mod collections;
pub mod introspection;
pub mod persistence;
pub mod reply;
pub mod session;
pub mod string;

/// 一个分片上的键值执行器。
///
/// `shard` 由宿主持有（绑定层一个 `Store` 对应一个分片），执行器只借用它，因此同一
/// 分片上的读、写与事务都经过同一把锁。
#[derive(Debug)]
pub struct RedisExecutor<'a> {
    shard: &'a Mutex<RedisShard>,
}

impl<'a> RedisExecutor<'a> {
    /// Borrows one shard for the lifetime of a request batch.
    pub fn new(shard: &'a Mutex<RedisShard>) -> Self {
        Self { shard }
    }
}

/// 把 `redis:db<db>[:kind]` 拆回数据库序号与值种类。
pub fn locate(namespace: &Namespace) -> Option<(u8, &'static str)> {
    let rest = namespace.0.strip_prefix(NAMESPACE_PREFIX)?;
    let (index, kind) = match rest.split_once(':') {
        Some((index, kind)) => (index, kind),
        None => (rest, ""),
    };
    let db = index.parse::<u8>().ok()?;
    KINDS.into_iter().find(|candidate| *candidate == kind).map(|kind| (db, kind))
}

/// 统计扁平集合 blob 里长度前缀块的数量。
fn count_chunks(flat: &[u8]) -> Option<usize> {
    let mut offset = 0;
    let mut chunks = 0;
    while offset < flat.len() {
        let raw = flat.get(offset..offset + 4)?;
        let length = u32::from_le_bytes(raw.try_into().ok()?) as usize;
        offset = offset.checked_add(4)?.checked_add(length)?;
        if offset > flat.len() {
            return None;
        }
        chunks += 1;
    }
    Some(chunks)
}

/// 在扁平 blob 前补实体计数，交给上游处理器。
///
/// 分片把集合值存成扁平的长度前缀块，而上游处理器要求带计数的形状。转换只动字节结构：
/// 字段顺序、字段字节与值字节都不变。畸形 blob 原样交出，让上游解码器报告损坏。
fn with_count(flat: &[u8], divisor: usize) -> Vec<u8> {
    let Some(chunks) = count_chunks(flat)
    else {
        return flat.to_vec();
    };
    let mut out = Vec::with_capacity(flat.len() + 4);
    out.extend_from_slice(&((chunks / divisor) as u32).to_le_bytes());
    out.extend_from_slice(flat);
    out
}

/// 去掉上游写入的计数，还原分片保存的扁平布局。
fn without_count(counted: &[u8]) -> Vec<u8> {
    if counted.len() >= 4 { counted[4..].to_vec() } else { counted.to_vec() }
}

/// 分片值字节转成上游处理器要求的形状。
fn upstream_value(kind: &str, value: &[u8]) -> Vec<u8> {
    match kind {
        // hash 的计数是字段数，扁平块是「字段 + 值」成对出现。
        "hash" => with_count(value, 2),
        "set" | "list" => with_count(value, 1),
        _ => value.to_vec(),
    }
}

/// 上游写入的值字节转成分片保存的扁平布局。
fn downstream_value(kind: &str, value: Vec<u8>) -> Vec<u8> {
    match kind {
        "hash" | "set" | "list" => without_count(&value),
        _ => value,
    }
}

impl KeyValueExecutor for RedisExecutor<'_> {
    fn execute(&self, fence: Option<&FenceToken>, command: KeyValueCommand) -> database_types::cluster::Result<KeyValueResult> {
        // 本执行器只服务一个本地分片，没有需要 fence 的租约。
        let _ = fence;
        let (db, kind) = locate(&command.namespace).ok_or(Error::Unsupported("unknown redis namespace"))?;
        let mut shard = self.shard.lock().map_err(|_| Error::Unsupported("store lock poisoned"))?;
        let key = command.key.as_slice();
        match command.action {
            KeyValueAction::Get => {
                Ok(KeyValueResult::Get(shard.get_kind(db, kind, key).map(|record| upstream_value(kind, record_bytes(record)))))
            }
            KeyValueAction::Exists => Ok(KeyValueResult::Exists(shard.contains_kind(db, kind, key))),
            KeyValueAction::GetDelete => {
                let value = shard.get_kind(db, kind, key).map(|record| upstream_value(kind, record_bytes(record)));
                if value.is_some() {
                    shard.delete_kind(db, kind, key).map_err(Error::Io)?;
                }
                Ok(KeyValueResult::GetDelete(value))
            }
            KeyValueAction::GetSet(value) => {
                let previous = shard.get_kind(db, kind, key).map(|record| upstream_value(kind, record_bytes(record)));
                let value = downstream_value(kind, value);
                shard.set_kind_revision(db, kind, key, &value, None).map_err(Error::Io)?;
                Ok(KeyValueResult::GetSet(previous))
            }
            KeyValueAction::Put(value) => {
                let value = downstream_value(kind, value);
                let revision = shard.set_kind_revision(db, kind, key, &value, None).map_err(Error::Io)?;
                Ok(KeyValueResult::Put { revision })
            }
            KeyValueAction::PutKeepingExpiry(value) => {
                let value = downstream_value(kind, value);
                let expires_at = shard.deadline_kind(db, kind, key);
                let revision = shard.set_kind_revision(db, kind, key, &value, expires_at).map_err(Error::Io)?;
                Ok(KeyValueResult::Put { revision })
            }
            KeyValueAction::PutIfAbsent(value) => {
                if shard.contains_kind(db, kind, key) {
                    return Ok(KeyValueResult::PutIfAbsent { revision: None });
                }
                let value = downstream_value(kind, value);
                let revision = shard.set_kind_revision(db, kind, key, &value, None).map_err(Error::Io)?;
                Ok(KeyValueResult::PutIfAbsent { revision: Some(revision) })
            }
            KeyValueAction::PutIfPresent(value) => {
                if !shard.contains_kind(db, kind, key) {
                    return Ok(KeyValueResult::PutIfPresent { revision: None });
                }
                let value = downstream_value(kind, value);
                let revision = shard.set_kind_revision(db, kind, key, &value, None).map_err(Error::Io)?;
                Ok(KeyValueResult::PutIfPresent { revision: Some(revision) })
            }
            KeyValueAction::PutIfPresentKeepingExpiry(value) => {
                if !shard.contains_kind(db, kind, key) {
                    return Ok(KeyValueResult::PutIfPresent { revision: None });
                }
                let value = downstream_value(kind, value);
                let expires_at = shard.deadline_kind(db, kind, key);
                let revision = shard.set_kind_revision(db, kind, key, &value, expires_at).map_err(Error::Io)?;
                Ok(KeyValueResult::PutIfPresent { revision: Some(revision) })
            }
            KeyValueAction::PutExpiring { value, expires_at } => {
                let value = downstream_value(kind, value);
                let revision = shard.set_kind_revision(db, kind, key, &value, Some(expires_at)).map_err(Error::Io)?;
                Ok(KeyValueResult::Put { revision })
            }
            KeyValueAction::PutIfAbsentExpiring { value, expires_at } => {
                if shard.contains_kind(db, kind, key) {
                    return Ok(KeyValueResult::PutIfAbsent { revision: None });
                }
                let value = downstream_value(kind, value);
                let revision = shard.set_kind_revision(db, kind, key, &value, Some(expires_at)).map_err(Error::Io)?;
                Ok(KeyValueResult::PutIfAbsent { revision: Some(revision) })
            }
            KeyValueAction::PutIfPresentExpiring { value, expires_at } => {
                if !shard.contains_kind(db, kind, key) {
                    return Ok(KeyValueResult::PutIfPresent { revision: None });
                }
                let value = downstream_value(kind, value);
                let revision = shard.set_kind_revision(db, kind, key, &value, Some(expires_at)).map_err(Error::Io)?;
                Ok(KeyValueResult::PutIfPresent { revision: Some(revision) })
            }
            KeyValueAction::Delete => Ok(KeyValueResult::Delete { removed: shard.delete_kind(db, kind, key).map_err(Error::Io)? }),
            KeyValueAction::SetExpiry { expires_at } => {
                Ok(KeyValueResult::ExpiryChanged { applied: shard.set_deadline_kind(db, kind, key, Some(expires_at)).map_err(Error::Io)? })
            }
            KeyValueAction::ClearExpiry => {
                let had_deadline = shard.contains_kind(db, kind, key) && shard.deadline_kind(db, kind, key).is_some();
                shard.set_deadline_kind(db, kind, key, None).map_err(Error::Io)?;
                Ok(KeyValueResult::ExpiryChanged { applied: had_deadline })
            }
            KeyValueAction::GetExpiry => {
                Ok(KeyValueResult::Expiry { present: shard.contains_kind(db, kind, key), expires_at: shard.deadline_kind(db, kind, key) })
            }
            KeyValueAction::GetRevision => Ok(KeyValueResult::Revision(shard.get_kind(db, kind, key).map(|record| record.revision))),
            KeyValueAction::IncrementBy(delta) => {
                if !kind.is_empty() {
                    return Err(Error::Unsupported("increments are only defined for the string kind"));
                }
                match shard.incr_by(db, key, delta).map_err(Error::Io)? {
                    Ok(value) => Ok(KeyValueResult::Increment { value }),
                    Err(IncrementError::NotAnInteger) => Err(Error::InvalidIntegerValue),
                    Err(IncrementError::Overflow) => Err(Error::IntegerOverflow),
                }
            }
            KeyValueAction::Rename { destination, if_absent } => {
                let outcome = shard.rename(db, key, &destination, if_absent).map_err(Error::Io)?;
                Ok(KeyValueResult::Rename {
                    source_present: !matches!(outcome, RenameOutcome::Missing),
                    applied: matches!(outcome, RenameOutcome::Renamed),
                })
            }
        }
    }

    fn execute_read_batch(
        &self,
        fence: Option<&FenceToken>,
        commands: &[KeyValueCommand],
    ) -> database_types::cluster::Result<Vec<KeyValueResult>> {
        // 宿主的同步客户端一次只发一条命令，因此把读背靠背复放不会与并发写交错。
        let _ = fence;
        commands.iter().map(|command| self.execute(None, command.clone())).collect()
    }

    fn execute_scan(
        &self,
        fence: Option<&FenceToken>,
        namespace: &Namespace,
        prefix: &[u8],
        after: Option<&[u8]>,
        before: Option<&[u8]>,
        limit: usize,
    ) -> database_types::cluster::Result<Vec<(Vec<u8>, Vec<u8>)>> {
        // 本执行器只服务一个本地分片，没有需要 fence 的租约。
        let _ = fence;
        let (db, kind) = locate(namespace).ok_or(Error::Unsupported("unknown redis namespace"))?;
        let shard = self.shard.lock().map_err(|_| Error::Unsupported("store lock poisoned"))?;
        let mut records = Vec::new();
        for key in shard.keys(db) {
            if !key.starts_with(prefix) {
                continue;
            }
            if after.is_some_and(|after| key.as_slice() <= after) {
                continue;
            }
            if before.is_some_and(|before| key.as_slice() >= before) {
                continue;
            }
            if limit != 0 && records.len() >= limit {
                break;
            }
            // `keys` 合并了一个数据库的每种值种类，因此只活在别的种类命名空间里的键不属于这次扫描。
            if let Some(record) = shard.get_kind(db, kind, &key) {
                records.push((key, upstream_value(kind, record_bytes(record))));
            }
        }
        Ok(records)
    }
}
