//! Mongo namespace 的 `.yyds` catalog + `.yykv` shard 后端。
//!
//! 实现归 [`database_storage::mongo::store`]，本模块只做门面再导出。

pub use database_storage::mongo::{store::*, *};
