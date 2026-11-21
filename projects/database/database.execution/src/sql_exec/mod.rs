//! 集群 KV 执行器之上的 SQL 执行面。
use std::cell::Cell;

pub mod sql_execute;

pub(crate) use sql_execute::backfill_generated_column as backfill_generated_rows;
pub use sql_execute::execute_plan;

thread_local! {
    /// 当前 dispatch 是否按 `CLIENT_FOUND_ROWS` 口径报告 DML 的命中行数。
    ///
    /// 这是握手协商出来的**每连接**语义，而执行面只拿到执行器、事务句柄与方言，
    /// 没有会话对象可挂。MySQL 网关在握手时记住客户端能力位，并把每条语句的
    /// dispatch 圈进 [`with_found_rows`] 的作用域；作用域外的执行（本地引擎、
    /// 其它方言）保持默认的改变行数口径。
    static FOUND_ROWS: Cell<bool> = const { Cell::new(false) };
}

/// 在 `dispatch` 执行期间采用 `CLIENT_FOUND_ROWS`（或默认的 changed rows）口径。
///
/// 作用域可嵌套：退出时恢复进入前的取值，内层不会污染同一线程上的外层语句。
pub fn with_found_rows<T>(enabled: bool, dispatch: impl FnOnce() -> T) -> T {
    let previous = FOUND_ROWS.with(|flag| flag.replace(enabled));
    let outcome = dispatch();
    FOUND_ROWS.with(|flag| flag.set(previous));
    outcome
}

/// 当前 dispatch 是否要求命中行数口径。
pub(crate) fn found_rows_enabled() -> bool {
    FOUND_ROWS.with(Cell::get)
}
