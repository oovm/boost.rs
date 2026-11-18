//! `database-storage`：读写、锁、快照、checkpoint、恢复与对象存储。
#![deny(missing_docs)]

/// 分布式分片 KV 与复制日志。
pub mod cluster_kv;
/// 嵌入式单文件库高层 API。
pub mod local;
/// 页缓存 / BTree / WAL / SHM 原语。
pub mod local_io;
/// PostgreSQL 数据目录：catalog、`.yykv` shard 与 `postmaster.pid` lease。
pub mod pgsql;
/// SQLite format-3 存储面。
pub mod sqlite;
