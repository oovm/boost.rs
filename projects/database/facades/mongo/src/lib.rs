//! MongoDB 产品门面：文档存储、游标、oplog 与命令语义。
//!
//! 本 crate 只做产品入口与组装，不导出 bin、不实现 server 主循环、不做 CLI；
//! 命令、游标、存储合同与记录编码各自归 `database.*` 的 owner crate。
//!
//! - [`bson`]：BSON 值 / 文档编解码（来自 `database-protocol-mongo`）
//! - [`json`]：Extended JSON 桥（来自 `database-protocol-mongo`）
//! - [`wire`]：帧头与 `OP_MSG` / `OP_QUERY` 转换（来自 `database-protocol-mongo`）
//! - [`id`]：ObjectId 生成（来自 `database-types`）
//! - `commands`：逐条命令原语（来自 `database-execution`，`storage` feature）
//! - `cursor`：调用方自有的游标表（来自 `database-execution`，`storage` feature）
//! - `oplog`：`local.oplog.rs` 条目形态（来自 `database-formats`，`storage` feature）
//! - `store`：`.yyds` catalog + `.yykv` shard 后端（`storage` feature）
//! - `storage`：命令层唯一的存储入口 trait（`storage` feature）
//! - `auth`：SCRAM 认证会话与用户、角色管理命令（`storage` feature）
//! - `authorization`：内置角色表、权限展开与命令授权判定（`storage` feature）
//! - `replica`：单成员副本集状态、集群时间与 `replSet*` 命令（`storage` feature）
//! - `retry`：重试写的登记表、键推导与应答重放（`storage` feature）
//! - `write_concern`：`writeConcern` / `$readPreference` 校验与写应答装饰（`storage` feature）
//! - `import`：`mongodump` 形态 `.bson` 流导入（`storage` feature）

#![deny(missing_docs)]

pub use database_protocol_mongo::{bson, json, wire};

pub mod id;

#[cfg(feature = "storage")]
pub mod auth;
#[cfg(feature = "storage")]
pub mod authorization;
#[cfg(feature = "storage")]
pub mod commands;
#[cfg(feature = "storage")]
pub mod cursor;
#[cfg(feature = "storage")]
pub mod import;
#[cfg(feature = "storage")]
pub mod oplog;
#[cfg(feature = "storage")]
pub mod replica;
/// 重试写的登记表、键推导与应答重放（`storage` feature）
#[cfg(feature = "storage")]
pub mod retry;
#[cfg(feature = "storage")]
pub mod storage;
#[cfg(feature = "storage")]
pub mod store;
#[cfg(feature = "storage")]
pub mod write_concern;

#[cfg(feature = "storage")]
pub use storage::MongoStorage;
#[cfg(feature = "storage")]
pub use store::Store;
