//! `yyds-client` 是 YY 远程客户端的产品门面。
//!
//! 传输、握手与重连语义归 `database.client`;本门面只把该能力固定到
//! YYDS 产品的作者面路径,不复制协议或连接状态机。除客户端本身,作者还需要
//! 行值/标量类型与宿主侧性能记录器,所以一并从这里导出——业务应用不应为了
//! 类型或埋点再引入节点/引擎门面。

#![warn(missing_docs)]

/// YY 线原生客户端：连接、认证会话、prepared、事务与关闭/重连。
pub use database::client::yy::Client;
/// 客户端生命周期状态。
pub use database::client::yy::ClientState;
/// 结果行（列名 → 类型化值）。
pub use database::client::yy::Row;
/// 表投影句柄：把表操作编译成 VOS 源，再走共享的 prepared 帧。
pub use database::client::yy::Table;
/// 宿主侧运行时性能记录器（与节点内置的卡点分析同一出口）。
pub use database::harness;
pub use database::{
    client::*,
    types::{Decimal, Error, ScalarError, Timestamp, Value},
};
