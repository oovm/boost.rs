//! 复制数据面:leader 写复制、成员端应用与状态转移的存储合同。
//!
//! 本模块只定义协议侧的抽象。实际分片存储在 `database.storage`,由宿主实现
//! [`ReplicaSink`];peer 通道因此不依赖存储格式,也不把 JSON 帧当成落盘格式。

use std::fmt;

use database_types::cluster::ShardId;

use super::frame::{ReplicaRecord, ShardReplica};

/// 状态转移游标:最后一条已接收记录的完整键序位置。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReplicaCursor {
    /// 命名空间。
    pub namespace: String,
    /// 不透明键字节。
    pub key: Vec<u8>,
}

impl ReplicaCursor {
    /// 从一条记录构造游标。
    pub fn of(record: &ReplicaRecord) -> Self {
        Self { namespace: record.namespace.clone(), key: record.key.clone() }
    }
}

/// 复制数据面在存储侧的失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplicaError {
    /// 本节点没有该分片的复制存储。
    Unavailable(String),
    /// 存储拒绝了该记录(损坏、序号冲突等)。
    Rejected(String),
}

impl fmt::Display for ReplicaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(formatter, "replica store is unavailable: {detail}"),
            Self::Rejected(detail) => write!(formatter, "replica store rejected the record: {detail}"),
        }
    }
}

impl std::error::Error for ReplicaError {}

impl ReplicaError {
    /// 稳定诊断码。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "database.cluster.replica.unavailable",
            Self::Rejected(_) => "database.cluster.replica.rejected",
        }
    }
}

/// 一个节点在某分片上的复制落点。
///
/// 实现必须逐字采用 [`ShardReplica::record`] 的 `revision`,并保证同一修订号的
/// 重放是幂等的;自行分配修订号会让副本之间的每键序号分叉。
pub trait ReplicaSink: Send + Sync + fmt::Debug {
    /// 应用一条复制写,返回该键当前持有的修订号。
    fn apply(&self, write: &ShardReplica) -> Result<u64, ReplicaError>;

    /// 返回严格位于 `after` 之后的记录,按完整键序升序,最多 `limit` 条。
    ///
    /// `limit` 为 `0` 表示不设上限。接收方用返回的最后一条记录构造下一页游标;
    /// 返回条数少于 `limit` 即表示该分片已传完。
    fn scan(&self, shard: &ShardId, after: Option<&ReplicaCursor>, limit: usize) -> Result<Vec<ReplicaRecord>, ReplicaError>;
}
