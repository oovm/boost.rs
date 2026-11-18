//! 命令层唯一的存储入口。
//!
//! 合同本体归 [`database_storage::mongo`]（后端入口、事务/会话语义与 TTL 账目），
//! 记录编码归 [`database_formats::mongo`]；本模块只是门面对这两个 owner 的组装
//! 再导出，不再定义第二份存储合同。

pub use database_storage::mongo::*;
