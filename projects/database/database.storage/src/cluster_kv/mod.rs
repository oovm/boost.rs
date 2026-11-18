//! 分布式分片 KV 存储与复制日志（磁盘后缀见 `SHARD_SUFFIX`）。

mod replication_log;
mod store;

/// 节点目录中的分片文件。
pub mod node;

pub mod redis;

pub use database_formats::yy::yykv::{InlineValue, Key, ObjectRef, Record, StoredValue};
pub use node::{open_shards, shard_path};
pub use replication_log::DurableReplicationLog;
pub use store::{FileShard, KvStore, MemoryShard, ShardChange, compare_and_put, now_unix_ms};
