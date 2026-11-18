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
    value::Cell,
};
pub use engine::{Engine, ExecResult, results_json};
pub use host::HostFunctionTable;

pub(crate) use database_query::sqlite::{bind, catalog, json_string, storage_failure, value};
