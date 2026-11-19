//! SCRAM-SHA-1 / SCRAM-SHA-256 认证会话与用户、角色管理命令。
//!
//! 实现归 [`database_execution::mongo::auth`]，本模块只做门面再导出。

pub use database_execution::mongo::auth::*;
