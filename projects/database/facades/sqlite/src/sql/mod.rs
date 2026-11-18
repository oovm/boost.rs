//! SQL execution core shared by the Node-API, PyO3 and WebAssembly hosts.
//!
//! The statement subset, its AST and structured rejections live in
//! `database_query::sqlite` (the SQLite dialect query frontend); this module
//! re-exports them and owns only the execution engine, the host-function
//! bridge and the JSON envelope:
//!
//! - [`Engine::execute_one`] executes exactly one statement.
//! - [`Engine::execute_batch`] executes a list of statements in order.
//! - [`Engine::execute`] executes a semicolon-separated script.
//!
//! Every statement runs over the pure-Rust format-3 writer, so the resulting
//! image is a real `.sqlite3` file, not a simulation layer.

pub(crate) mod datetime;
pub mod engine;
pub mod host;
pub(crate) mod json;

pub use database_query::sqlite::{
    SqlError,
    frontend::{
        AggregateFunction, AlterAction, Assignment, BinaryOp, CastType, CheckConstraint, ColumnRef, CommonTable, CompareOp, CompoundOp,
        CompoundTerm, ConflictAction, ConflictClause, DropColumnConflict, Expr, ForeignKey, ForeignKeyAction, Frame, FrameBound, FrameEdge,
        FrameExclude, FrameUnits, IndexColumn, InsertValue, JoinKind, Limit, Literal, NamedWindow, OrderKey, ScalarFunction, SelectCore,
        SelectItems, Statement, TableConstraint, TableRef, TransactionAction, TriggerEvent, TriggerTiming, UnaryOp, WindowFunction, WindowSpec,
        core_references, parameter_slots, parse, parse_with_host_functions, rewrite_create_index_column, rewrite_create_index_table,
        rewrite_create_table_column, rewrite_create_table_drop_column, rewrite_create_table_name, rewrite_view_column, rewrite_view_table,
        statement_references, strip_schema_qualifiers,
    },
};
pub use engine::{Cell, Engine, ExecResult, results_json};
pub use host::HostFunctionTable;

pub(crate) use database_query::sqlite::json_string;

/// Maps a storage rejection onto the facade's stable error code.
///
/// Corruption keeps its own code instead of collapsing into `storage`, and
/// constraint families stay distinguishable so hosts can branch on them.
pub(crate) fn storage_failure(error: database_types::Error) -> SqlError {
    let code = match &error {
        database_types::Error::Corrupt(_) | database_types::Error::CorruptReason { .. } => "corrupt",
        database_types::Error::UniqueViolation { .. } => "unique-constraint",
        database_types::Error::Constraint { kind, .. } => match *kind {
            "unique" => "unique-constraint",
            "not-null" => "not-null-constraint",
            "check" => "check-constraint",
            "foreign-key" => "foreign-key-constraint",
            _ => "constraint-failed",
        },
        database_types::Error::Unsupported(_) => "unsupported",
        _ => "storage",
    };
    SqlError::new(code, error.to_string())
}
