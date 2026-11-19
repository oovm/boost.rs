//! SQL-level `DECLARE` / `FETCH` / `CLOSE` cursor statements.
//!
//! Cursors are session state owned by [`TransactionSession`]. `DECLARE` runs the
//! query once and keeps the rows, `FETCH` slices them, and the transaction
//! boundary decides what closes: `COMMIT` keeps `WITH HOLD` cursors, `ROLLBACK`
//! closes every cursor, matching PostgreSQL.

use std::sync::Arc;

use database_query::{FetchDirection, PgUtilityStatement};

use super::{
    session::{PgsqlSqlRuntime, SessionCursor, TransactionSession},
    statement::{ExecutionOutcome, StatementError, StatementOutcome, run_statement},
};

/// Applies one cursor utility statement.
pub fn execute_cursor_utility(
    utility: &PgUtilityStatement,
    runtime: Option<&Arc<PgsqlSqlRuntime>>,
    session: &mut TransactionSession,
) -> Result<StatementOutcome, StatementError> {
    if session.is_aborted() {
        return Err(StatementError::new("25P02", super::session::ABORTED_BLOCK));
    }
    match utility {
        PgUtilityStatement::DeclareCursor { name, with_hold, sql } => {
            if !session.in_transaction() {
                return Err(StatementError::new("25P01", "DECLARE CURSOR can only be used in transaction blocks"));
            }
            let Some(runtime) = runtime
            else {
                return Err(StatementError::new("0A000", "PostgreSQL query execution is not implemented (no SQL runtime)"));
            };
            let outcome = run_statement(sql, runtime, session).map_err(|(sqlstate, message)| StatementError::new(sqlstate, message))?;
            let cursor =
                SessionCursor { sql: sql.clone(), description: outcome.description, rows: outcome.rows, position: 0, with_hold: *with_hold };
            if !session.declare_cursor(name, cursor) {
                return Err(StatementError::new("42P03", format!("cursor \"{name}\" already exists")));
            }
            Ok(StatementOutcome::Command("DECLARE CURSOR"))
        }
        PgUtilityStatement::Fetch { name, direction } => {
            let Some(cursor) = session.cursor_mut(name)
            else {
                return Err(StatementError::new("34000", format!("cursor \"{name}\" does not exist")));
            };
            let description = cursor.description.clone();
            let rows = fetch_rows(cursor, *direction);
            let command_tag = format!("FETCH {}", rows.len());
            Ok(StatementOutcome::Result(ExecutionOutcome { is_read: true, description, rows, command_tag }))
        }
        PgUtilityStatement::CloseCursor { name } => {
            let all = name.eq_ignore_ascii_case("ALL");
            let closed = session.close_cursor(if all { None } else { Some(name.as_str()) });
            if !all && !closed {
                return Err(StatementError::new("34000", format!("cursor \"{name}\" does not exist")));
            }
            Ok(StatementOutcome::Command("CLOSE CURSOR"))
        }
        _ => unreachable!("only cursor statements reach the cursor executor"),
    }
}

/// Advances `cursor` by `direction` and returns the rows that fetch observes.
fn fetch_rows(cursor: &mut SessionCursor, direction: FetchDirection) -> Vec<Vec<database_query::SqlValue>> {
    let total = cursor.rows.len();
    let clamp = |position: i64| -> Option<usize> { if position < 0 || position as usize >= total { None } else { Some(position as usize) } };
    match direction {
        FetchDirection::Next => take(&cursor.rows, &mut cursor.position, 1),
        FetchDirection::All => {
            let start = cursor.position.min(total);
            cursor.position = total;
            cursor.rows[start..].to_vec()
        }
        FetchDirection::Forward(count) => {
            if count <= 0 {
                return Vec::new();
            }
            take(&cursor.rows, &mut cursor.position, count as usize)
        }
        FetchDirection::Absolute(row) => {
            let target = if row > 0 { row - 1 } else { total as i64 + row };
            match clamp(target) {
                Some(index) => {
                    cursor.position = index + 1;
                    vec![cursor.rows[index].clone()]
                }
                None => {
                    cursor.position = if row < 0 { total } else { 0 };
                    Vec::new()
                }
            }
        }
        FetchDirection::Relative(offset) => {
            let target = cursor.position as i64 + offset;
            match clamp(target) {
                Some(index) => {
                    cursor.position = index + 1;
                    vec![cursor.rows[index].clone()]
                }
                None => Vec::new(),
            }
        }
    }
}

fn take(rows: &[Vec<database_query::SqlValue>], position: &mut usize, count: usize) -> Vec<Vec<database_query::SqlValue>> {
    let start = (*position).min(rows.len());
    let end = start.saturating_add(count).min(rows.len());
    *position = end;
    rows[start..end].to_vec()
}
