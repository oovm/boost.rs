//! Execution entry points for prepared and one-shot local statements.
//!
//! Parsing, lowering and parameter binding stay in `database.query`; these
//! functions take the resulting models and run them against stored rows.

use std::collections::BTreeMap;

use database_query::local::{PreparedStatement, QueryKind, QueryRow, bind_ordered, prepare_sql, store::RowSource};
use database_types::{Error, Result, TableUniqueConstraint, Value};
use schema_types::ResolvedContract;

use super::{
    exec,
    write::{self, WriteOutcome},
};

/// Execute a VOS read pipeline against stored table rows.
pub fn execute(source: &str, contract: Option<&ResolvedContract>, records: &BTreeMap<String, Vec<u8>>) -> Result<Vec<QueryRow>> {
    exec::execute(source, contract, records)
}

/// Execute a prepared-and-bound statement against stored rows.
pub fn execute_prepared(
    stmt: &PreparedStatement,
    values: &[Value],
    contract: Option<&ResolvedContract>,
    records: &BTreeMap<String, Vec<u8>>,
) -> Result<Vec<QueryRow>> {
    exec::execute_ops(&stmt.ops, contract, records, values)
}

/// Execute a prepared-and-bound statement, also reporting per-operator timings.
///
/// Diagnostics only: the engine never reads the profile.
pub fn execute_prepared_profiled(
    stmt: &PreparedStatement,
    values: &[Value],
    contract: Option<&ResolvedContract>,
    records: &BTreeMap<String, Vec<u8>>,
) -> Result<(Vec<QueryRow>, exec::ExecProfile)> {
    exec::execute_ops_profiled(&stmt.ops, contract, records, values)
}

/// Execute a prepared read against a borrowed storage window.
///
/// Metadata rows stay in `records`; table scans decode rows from `source`, which
/// the caller must keep alive for the duration of the call. Every row leaves the
/// pipeline owned, so no borrow crosses this boundary.
pub fn execute_prepared_borrowed(
    stmt: &PreparedStatement,
    values: &[Value],
    contract: Option<&ResolvedContract>,
    records: &BTreeMap<String, Vec<u8>>,
    source: &dyn RowSource,
    profiled: bool,
) -> Result<(Vec<QueryRow>, exec::ExecProfile)> {
    if profiled {
        exec::execute_ops_borrowed_profiled(&stmt.ops, contract, records, source, values)
    }
    else {
        exec::execute_ops_borrowed(&stmt.ops, contract, records, source, values).map(|rows| (rows, exec::ExecProfile::default()))
    }
}

/// Execute a prepared-and-bound write statement against stored rows.
///
/// Returns [`Error::Unsupported`] for read statements so callers cannot
/// accidentally treat a `SELECT` plan as a mutation.
pub fn execute_write_prepared(
    stmt: &PreparedStatement,
    values: &[Value],
    contract: &ResolvedContract,
    declared: &[TableUniqueConstraint],
    records: &mut BTreeMap<String, Vec<u8>>,
) -> Result<WriteOutcome> {
    if stmt.write_ops.is_empty() {
        return Err(Error::Unsupported("statement is a read pipeline, not a write program"));
    }
    write::execute_ops(&stmt.write_ops, contract, declared, records, values)
}

/// Result of one SQL statement.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SqlOutcome {
    /// Rows produced by a read statement (empty for writes).
    pub rows: Vec<QueryRow>,
    /// Rows affected by a write statement (zero for reads).
    pub affected: usize,
}

/// Execute one SQL statement against stored table rows.
///
/// `values` binds the statement's parameters in declaration order, so
/// `?` placeholders map to the first, second, … supplied value. Reads return
/// rows, writes return the affected-row count.
pub fn execute_sql(
    source: &str,
    values: &[Value],
    contract: &ResolvedContract,
    declared: &[TableUniqueConstraint],
    records: &mut BTreeMap<String, Vec<u8>>,
) -> Result<SqlOutcome> {
    let stmt = prepare_sql(source)?;
    let bound = bind_ordered(&stmt, values)?;
    match stmt.query_kind {
        QueryKind::Read => {
            let rows = execute_prepared(&stmt, &bound, Some(contract), records)?;
            Ok(SqlOutcome { rows, affected: 0 })
        }
        QueryKind::Write => {
            let outcome = execute_write_prepared(&stmt, &bound, contract, declared, records)?;
            Ok(SqlOutcome { rows: outcome.rows, affected: outcome.affected })
        }
        QueryKind::Session => Err(Error::Schema {
            message: "session / transaction boundary statements must be executed through the connection surface".into(),
        }),
    }
}
