//! MySQL 的运行时会话、事务机制与 SQL 执行管线。
//!
//! 与 [`pgsql`](crate::pgsql) 同样的分工：执行资源在这里拥有
//! 事务句柄、schema checkpoint/restore、autocommit 切换、savepoint、
//! bind → plan → execute 管线、会话命令分发与 MySQL 错误码映射；
//! `database.server` 的 yyds MySQL 网关只保留协议会话——握手、组帧、
//! 预编译语句登记与结果行编码——按调用顺序驱动本模块。
//! 实例路由与 catalog 仍由 cluster/storage 决定，不在此处。

pub mod session;
pub mod statement;

pub use session::{MysqlError, MysqlSqlRuntime, TransactionSession, mysql_error_code, statement_error};
pub use statement::{
    MysqlColumnDescription, MysqlPrepareOutcome, MysqlPreparedRegistry, MysqlPreparedStatement, MysqlStatementSink, dispatch_query,
    execute_prepared, prepare_statement,
};
