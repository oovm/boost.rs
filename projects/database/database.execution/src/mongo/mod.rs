//! MongoDB 命令语义、游标表与存储合同入口。
//!
//! 命令实现只写一份：产品门面与 `yyds` 网关都消费本模块，不再各自持有一套
//! Mongo 命令或游标状态机。

/// 逐条命令原语（过滤、规划、读写、聚合、索引与事务命令）。
pub mod commands;
/// 调用方自有的游标表。
pub mod cursor;
/// ObjectId 生成，合同与实现归 `database.types`。
pub mod id {
    pub use database_types::mongo::generate_object_id;
}
/// Mongo 文档存储合同，归 `database.storage`。
pub mod storage {
    pub use database_storage::mongo::*;
}
/// `local.oplog.rs` 条目形态，归 `database.formats`。
pub use database_formats::mongo::oplog;
