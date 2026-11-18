//! 调用方自有的游标表。
//!
//! 游标状态机归 [`database_execution::mongo::cursor`]，本模块只做门面再导出。

pub use database_execution::mongo::cursor::*;
