//! `writeConcern` / `$readPreference` 校验与写应答装饰。
//!
//! 实现归 [`database_execution::mongo::write_concern`]，本模块只做门面再导出。

pub use database_execution::mongo::write_concern::*;
