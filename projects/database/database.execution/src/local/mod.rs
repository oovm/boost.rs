//! Operator execution for the local record-map query layer.
//!
//! `database.query` owns the models, binding and plan (the lowered `QueryOp`
//! tree); this module owns what runs them: scan, filter, projection, sorting,
//! joins, set operations, aggregates, windows and the write operators.

mod exec;
mod prepared;
mod resolve;
mod result;
mod write;

pub use exec::{ExecProfile, cmp_values, compare_values, execute_ops_borrowed, execute_ops_borrowed_profiled, execute_ops_profiled};
pub use prepared::{
    SqlOutcome, execute, execute_prepared, execute_prepared_borrowed, execute_prepared_profiled, execute_sql, execute_write_prepared,
};
pub use result::{ColumnMeta, EncodedResult, ResultCell, ResultFormat, encode_result};
pub use write::{WriteOutcome, execute_ops};
