//! `yyds-client` 是 YY 远程客户端的产品门面。
//!
//! 传输、握手与重连语义归 `database.client`;本门面只把该能力固定到
//! YYDS 产品的作者面路径,不复制协议或连接状态机。除客户端本身,作者还需要
//! 行值/标量类型与宿主侧性能记录器,所以一并从这里导出——业务应用不应为了
//! 类型或埋点再引入节点/引擎门面。

#![warn(missing_docs)]

pub use database::client::*;
/// 宿主侧运行时性能记录器（与节点内置的卡点分析同一出口）。
pub use database::harness;
pub use database::types::{Decimal, Error, ScalarError, Timestamp, Value};
