//! Migrated integration tests for `redis`.
//!
//! 详细正确性矩阵已整体迁到下游 Redis 产品仓的 `redis-conformance` 承接区，
//! 这里只保留最小 smoke：入口创建、最小命令往返与结果。

mod gateway;
mod sessions;
