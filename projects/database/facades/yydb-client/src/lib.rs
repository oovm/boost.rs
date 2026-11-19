//! `yydb-client` 是 YYDB 远程客户端的产品门面。
//!
//! 传输、握手与重连语义归 `database.client`;本门面只把该能力固定到
//! YYDB 产品的作者面路径,不复制协议或连接状态机。

#![warn(missing_docs)]

pub use database::client::*;
