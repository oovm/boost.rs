//! 集群 KV 执行器之上的 SQL 执行面。
pub mod sql_execute;

pub(crate) use sql_execute::backfill_generated_column as backfill_generated_rows;
pub use sql_execute::execute_plan;
