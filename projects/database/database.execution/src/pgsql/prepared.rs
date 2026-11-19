//! SQL-level `PREPARE` / `EXECUTE` / `DEALLOCATE`.
//!
//! The session owns the registered statements; this module applies the
//! PostgreSQL command tags and error classes so the wire session and the local
//! engine observe one behaviour.

use std::sync::Arc;

use database_query::{PgUtilityStatement, SqlParameterStyle, SqlValue, substitute_sql_parameters};

use super::{
    session::{PgsqlSqlRuntime, PreparedSqlStatement, TransactionSession},
    statement::{StatementError, StatementOutcome, run_statement},
};

/// Applies one parsed utility statement.
pub fn execute_utility(
    utility: &PgUtilityStatement,
    runtime: Option<&Arc<PgsqlSqlRuntime>>,
    session: &mut TransactionSession,
) -> Result<StatementOutcome, StatementError> {
    if session.is_aborted() {
        return Err(StatementError::new("25P02", super::session::ABORTED_BLOCK));
    }
    match utility {
        PgUtilityStatement::Prepare { name, parameter_types, sql } => {
            let statement = PreparedSqlStatement { sql: sql.clone(), parameter_types: parameter_types.clone() };
            if !session.prepare_statement(name, statement) {
                return Err(StatementError::new("42P05", format!("prepared statement \"{name}\" already exists")));
            }
            Ok(StatementOutcome::Command("PREPARE"))
        }
        PgUtilityStatement::DeclareCursor { .. } | PgUtilityStatement::Fetch { .. } | PgUtilityStatement::CloseCursor { .. } => {
            super::cursors::execute_cursor_utility(utility, runtime, session)
        }
        PgUtilityStatement::Explain { analyze, sql } => super::explain::execute_explain(sql, *analyze, runtime, session),
        PgUtilityStatement::Listen { channel } => {
            let Some(runtime) = runtime
            else {
                return Err(StatementError::new("0A000", "LISTEN requires a runtime-backed session"));
            };
            session.listen(runtime, channel);
            Ok(StatementOutcome::Command("LISTEN"))
        }
        PgUtilityStatement::Unlisten { channel } => {
            session.unlisten(channel.as_deref());
            Ok(StatementOutcome::Command("UNLISTEN"))
        }
        PgUtilityStatement::Notify { channel, payload } => {
            let Some(runtime) = runtime
            else {
                return Err(StatementError::new("0A000", "NOTIFY requires a runtime-backed session"));
            };
            if payload.len() > 8000 {
                return Err(StatementError::new("22001", "NOTIFY payload must be shorter than 8000 bytes"));
            }
            session.notify(runtime, channel, payload, std::process::id());
            Ok(StatementOutcome::Command("NOTIFY"))
        }
        PgUtilityStatement::CopyFromStdin { .. } => Err(StatementError::new("0A000", "COPY FROM STDIN requires the wire CopyIn protocol")),
        PgUtilityStatement::Deallocate { name } => {
            let removed = session.deallocate_statement(name.as_deref());
            if let Some(name) = name {
                if !removed {
                    return Err(StatementError::new("26000", format!("prepared statement \"{name}\" does not exist")));
                }
                Ok(StatementOutcome::Command("DEALLOCATE"))
            }
            else {
                Ok(StatementOutcome::Command("DEALLOCATE ALL"))
            }
        }
        PgUtilityStatement::Execute { name, arguments } => {
            let Some(statement) = session.prepared_statement(name)
            else {
                return Err(StatementError::new("26000", format!("prepared statement \"{name}\" does not exist")));
            };
            let sql = statement.sql.clone();
            let declared = statement.parameter_types.len();
            if declared > 0 && declared != arguments.len() {
                return Err(StatementError::new(
                    "42601",
                    format!("wrong number of parameters for prepared statement \"{name}\": expected {declared}, got {}", arguments.len()),
                ));
            }
            let Some(runtime) = runtime
            else {
                return Err(StatementError::new("0A000", "PostgreSQL query execution is not implemented (no SQL runtime)"));
            };
            let mut values = Vec::with_capacity(arguments.len());
            for argument in arguments {
                values.push(parse_argument(argument)?);
            }
            let rendered = substitute_sql_parameters(&sql, &values, SqlParameterStyle::DollarNumber)
                .map_err(|error| StatementError::new("08P01", format!("SQL parameter error: {}", error.message)))?;
            run_statement(&rendered, runtime, session)
                .map(StatementOutcome::Result)
                .map_err(|(sqlstate, message)| StatementError::new(sqlstate, message))
        }
    }
}

/// Parses one `EXECUTE` argument. The gateway accepts literals so a prepared
/// statement cannot smuggle an arbitrary expression through the argument list.
fn parse_argument(source: &str) -> Result<SqlValue, StatementError> {
    let text = source.trim();
    if text.eq_ignore_ascii_case("NULL") {
        return Ok(SqlValue::Null);
    }
    if text.eq_ignore_ascii_case("TRUE") {
        return Ok(SqlValue::Boolean(true));
    }
    if text.eq_ignore_ascii_case("FALSE") {
        return Ok(SqlValue::Boolean(false));
    }
    if let Some(inner) = text.strip_prefix('\'') {
        let Some(inner) = inner.strip_suffix('\'')
        else {
            return Err(StatementError::new("42601", format!("unterminated string argument: {text}")));
        };
        return Ok(SqlValue::String(inner.replace("''", "'")));
    }
    if let Ok(value) = text.parse::<i64>() {
        return Ok(SqlValue::Integer(value));
    }
    if let Ok(value) = text.parse::<f64>() {
        return Ok(SqlValue::Number(value));
    }
    Err(StatementError::new("0A000", format!("EXECUTE arguments must be literals, got `{text}`")))
}
