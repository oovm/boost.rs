//! `database-execution`：连接、游标、算子、写入与事务协调。
//!
//! UDF 合同与注册表已提级到 [`database_udf`]；本包只保留执行程序桥接。

#![deny(missing_docs)]

/// 分布式执行面。
pub mod cluster;
/// 嵌入式引擎（连接 / 会话 / 存储协调）。
pub mod engine;
/// 本地记录图上的算子与写约束执行（模型与计划在 `database.query`）。
pub mod local;
/// MongoDB 命令语义、游标表与存储合同入口。
pub mod mongo;
/// MySQL 会话、事务机制、语句分派与 bind → plan → execute。
pub mod mysql;
/// 执行程序表示与求值。
pub mod program;
/// PostgreSQL 会话、语句分派与 bind → plan → execute。
pub mod pgsql;
/// 执行侧正式 schema/sequence 入口：DDL 变更、序列分配、checkpoint 与原子发布。
pub mod sql_schema;
pub mod sql_exec;
/// SQLite 方言的求值机制（表达式 / 单元格 / 标量函数 / 宿主函数桥）。
pub mod sqlite;
/// 执行程序 ↔ UDF 边界桥接。
pub mod udf_bridge;

/// UDF 能力再导出（合同 + 注册表 + 宿主/原生适配 + 执行桥）。
pub mod udf {
    pub use crate::udf_bridge::*;
    pub use database_udf::*;
}
