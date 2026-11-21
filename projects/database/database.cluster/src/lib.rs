//! `database-cluster`：分片、路由、所有权、fencing、复制与分布式编排。
#![warn(missing_docs)]

/// 目录真值与 schema 发布。
pub mod catalog;
/// 租约 / fencing 控制面。
pub mod control;
/// 节点间 peer 通道:join 握手、成员视图、epoch 通告与可选 mTLS。
pub mod peer;
/// 分片拓扑、路由、复制与分布式执行合同。
pub mod topology;
