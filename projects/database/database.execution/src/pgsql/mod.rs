//! PostgreSQL execution: session state, statement dispatch and the
//! bind → plan → execute path shared by the local engine and the wire gateway.
//!
//! The dialect front end (parsing, binding, planning) stays in `database.query`.
//! This module owns what runs a plan: the transaction block, the schema
//! checkpoint, the aborted-block rule and the SQLSTATE mapping.

pub mod cursors;
pub mod explain;
pub mod prepared;
pub mod session;
pub mod statement;

pub use cursors::execute_cursor_utility;
pub use explain::execute_explain;
pub use prepared::execute_utility;
pub use session::{
    ABORTED_BLOCK, DEFAULT_SEARCH_PATH, PgsqlSqlRuntime, PreparedSqlStatement, SessionCursor, TransactionControl, TransactionSession,
};
pub use statement::{
    ExecutionOutcome, StatementError, StatementOutcome, dispatch_statement, pg_sqlstate, run_statement, statement_error,
};
