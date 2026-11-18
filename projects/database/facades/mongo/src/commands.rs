//! 逐条命令原语。
//!
//! 命令语义归 [`database_execution::mongo::commands`]，本模块只做门面再导出。

pub use database_execution::mongo::commands::*;
