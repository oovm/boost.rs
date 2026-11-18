//! PostgreSQL statement execution: statement dispatch, bind → plan → execute
//! and the SQLSTATE mapping.
//!
//! [`dispatch_statement`] is the one semantic source both entry points use: the
//! local engine (`postgres` facade) and the wire gateway. Parsing and planning
//! belong to `database.query`; this module executes the resulting plan and owns
//! the transaction/DDL ordering that both paths must observe.

use std::sync::Arc;

use database_query::{
    SqlColumn, SqlDialect, SqlErrorKind, SqlExecutionPlan, SqlFrontendError, SqlSessionCommand, SqlTransactionAction, SqlValue,
    analyze_meta_query, apply_auto_increment, bind_sql_catalog, describe_plan, parse_ddl, parse_pg_utility, parse_session_command,
    plan_statement_for_dialect, rewrite_double_colon_casts,
};

use crate::cluster::TransactionHandle;

use super::session::{ABORTED_BLOCK, PgsqlSqlRuntime, TransactionControl, TransactionSession};

/// 执行结果的统一形状，供 SimpleQuery、扩展查询与本地直接入口共用。
pub struct ExecutionOutcome {
    /// Whether the statement is a read.
    pub is_read: bool,
    /// Result column descriptions, empty when the statement returns no rows.
    pub description: Vec<SqlColumn>,
    /// Result rows in column order.
    pub rows: Vec<Vec<SqlValue>>,
    /// PostgreSQL command tag, e.g. `INSERT 0 3`.
    pub command_tag: String,
}

impl ExecutionOutcome {
    /// Whether the statement reports a result set. Reads always do, and writes do
    /// only when the statement carried a `RETURNING` projection.
    pub fn returns_rows(&self) -> bool {
        self.is_read || !self.description.is_empty()
    }
}

/// Outcome of one dispatched statement.
pub enum StatementOutcome {
    /// Completed with a PostgreSQL command tag and no result rows.
    Command(&'static str),
    /// A session command the caller interprets: `SET` / `SHOW`, `RESET`,
    /// `DISCARD` and savepoints belong to the protocol surface that owns the
    /// session caches, so dispatch hands the parsed command back unchanged.
    Session(SqlSessionCommand),
    /// Bind → plan → execute result.
    Result(ExecutionOutcome),
}

/// One statement failure with its PostgreSQL SQLSTATE class.
pub struct StatementError {
    /// Five-character PostgreSQL SQLSTATE.
    pub sqlstate: String,
    /// Human-readable failure detail.
    pub message: String,
}

impl StatementError {
    pub(crate) fn new(sqlstate: impl Into<String>, message: impl Into<String>) -> Self {
        Self { sqlstate: sqlstate.into(), message: message.into() }
    }
}

/// Runs one statement through the shared order: DDL, transaction control, then
/// the caller-owned session commands, then bind → plan → execute.
///
/// Every failure that aborts the open block marks it aborted before returning,
/// so the caller only maps the SQLSTATE onto its transport.
pub fn dispatch_statement(
    sql: &str,
    runtime: Option<&Arc<PgsqlSqlRuntime>>,
    session: &mut TransactionSession,
) -> Result<StatementOutcome, StatementError> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Ok(StatementOutcome::Command(""));
    }
    // `expr::type` is lowered onto the shared `CAST(...)` node before any other
    // stage sees the text, so DDL detection, the session-command pre-filter and
    // the binder all agree with the PostgreSQL spelling.
    let normalized = rewrite_double_colon_casts(trimmed, SqlDialect::Postgres).map_err(|error| {
        session.fail();
        StatementError::new(pg_sqlstate(error.kind), format!("SQL cast error: {}", error.message))
    })?;
    let trimmed = normalized.as_ref();
    // `PREPARE` / `EXECUTE` / `DEALLOCATE` wrap another statement, so they run
    // before DDL detection would misclassify the wrapped text.
    if let Some(utility) = parse_pg_utility(trimmed).map_err(|error| {
        session.fail();
        StatementError::new(pg_sqlstate(error.kind), format!("SQL utility error: {}", error.message))
    })? {
        return super::prepared::execute_utility(&utility, runtime, session);
    }
    // DDL resolves against the local schema registry before the Oak session
    // parser, so a `CREATE` / `ALTER` / `DROP` outside the supported subset is
    // reported as such instead of being misclassified as an unknown table.
    match parse_ddl(trimmed, SqlDialect::Postgres) {
        Err(error) => {
            session.fail();
            return Err(StatementError::new(pg_sqlstate(error.kind), format!("SQL ddl error: {}", error.message)));
        }
        Ok(Some(statement)) => {
            if session.is_aborted() {
                return Err(StatementError::new("25P02", ABORTED_BLOCK));
            }
            let Some(runtime) = runtime
            else {
                return Err(StatementError::new("0A000", "YYDS PostgreSQL SQL execution is not implemented"));
            };
            return match runtime.schema.apply_ddl(&statement) {
                Ok(outcome) => Ok(StatementOutcome::Command(outcome.command_tag())),
                Err(error) => {
                    session.fail();
                    Err(StatementError::new(pg_sqlstate(error.kind), format!("SQL ddl error: {}", error.message)))
                }
            };
        }
        Ok(None) => {}
    }
    let command = parse_session_command(trimmed).map_err(|_| {
        session.fail();
        StatementError::new("42601", "Oak could not parse the SQL statement")
    })?;
    match command {
        Some(SqlSessionCommand::Transaction(action)) => {
            let control = match action {
                SqlTransactionAction::Begin => session.begin(runtime),
                SqlTransactionAction::Commit => session.commit(runtime).map_err(session_io_error)?,
                SqlTransactionAction::Rollback => session.rollback(runtime).map_err(session_io_error)?,
                _ => {
                    session.fail();
                    return Err(StatementError::new("0A000", "savepoints are not supported on the direct statement path"));
                }
            };
            match control {
                TransactionControl::Done(tag) => Ok(StatementOutcome::Command(tag)),
                TransactionControl::Rejected { code, message } => Err(StatementError::new(code, message)),
            }
        }
        Some(command) => Ok(StatementOutcome::Session(command)),
        None => {
            let Some(runtime) = runtime
            else {
                session.fail();
                return Err(StatementError::new("0A000", "PostgreSQL query execution is not implemented (no SQL runtime)"));
            };
            run_statement(trimmed, runtime, session).map(StatementOutcome::Result).map_err(|(sqlstate, message)| {
                StatementError::new(sqlstate, message)
            })
        }
    }
}

/// Maps a session I/O failure onto a SQLSTATE instead of dropping the
/// connection, so both entry points report it the same way.
fn session_io_error(error: std::io::Error) -> StatementError {
    StatementError::new("XX000", error.to_string())
}

/// bind → plan → execute，错误已映射为 SQLSTATE，并已标记事务失败。
pub fn run_statement(
    sql: &str,
    runtime: &Arc<PgsqlSqlRuntime>,
    session: &mut TransactionSession,
) -> Result<ExecutionOutcome, (String, String)> {
    // The extended protocol reaches this function without going through
    // `dispatch_statement`, so the cast lowering happens here as well. It is
    // idempotent: an already-lowered statement carries no `::`.
    let normalized = rewrite_double_colon_casts(sql, SqlDialect::Postgres).map_err(|error| {
        session.fail();
        statement_error("cast", error)
    })?;
    let sql = normalized.as_ref();
    // Metadata views are synthesized from the local schema registry. They never
    // bind into the catalog or touch the executor, so they branch off before the
    // regular session-command/bind path.
    let meta = match analyze_meta_query(sql, SqlDialect::Postgres) {
        Ok(meta) => meta,
        Err(error) => {
            session.fail();
            return Err(statement_error("metadata", error));
        }
    };
    if let Some(meta) = meta {
        let description = meta.describe();
        let result = meta.execute(&runtime.schema, "public");
        let command_tag = format!("SELECT {}", result.rows.len());
        return Ok(ExecutionOutcome { is_read: true, description, rows: result.rows, command_tag });
    }
    // 扩展查询不经过 `dispatch_statement`，语法错误必须在这里映射为 `42601`
    // 而不是被后面的 catalog 绑定错误统一归为 `42P01`。
    if parse_session_command(sql).is_err() {
        session.fail();
        return Err(("42601".to_string(), "Oak could not parse the SQL statement".to_string()));
    }
    let catalog = runtime.schema.catalog();
    let bound = bind_sql_catalog(sql, &catalog, catalog.schema_version(), SqlDialect::Postgres).map_err(|error| {
        session.fail();
        statement_error("bind", error)
    })?;
    // Sequence-owning columns default at write time. Allocation advances and
    // persists the counter before execution, so a failed or rolled-back
    // statement never reuses the value, matching PostgreSQL `nextval`.
    let bound = apply_auto_increment(&runtime.schema, bound).map_err(|error| {
        session.fail();
        statement_error("bind", error)
    })?;
    let plan = plan_statement_for_dialect(bound, &catalog, SqlDialect::Postgres).map_err(|error| {
        session.fail();
        statement_error("plan", error)
    })?;
    let is_read = matches!(
        &plan,
        SqlExecutionPlan::Read(_)
            | SqlExecutionPlan::Scan(_)
            | SqlExecutionPlan::Join(_)
            | SqlExecutionPlan::Scalar(_)
            | SqlExecutionPlan::SetOperation(_)
    );
    // Derive the result shape from the catalog plan so an empty result set
    // reports the declared type instead of falling back to `text`.
    let description = describe_plan(&plan);
    let handle_ref: Option<&dyn TransactionHandle> = session.transaction_handle();
    let result = crate::sql_exec::execute_plan(runtime.executor.as_ref(), handle_ref, &plan, SqlDialect::Postgres).map_err(|error| {
        session.fail();
        statement_error("execution", error)
    })?;
    let command_tag = if is_read {
        format!("SELECT {}", result.rows.len())
    }
    else {
        let affected = result.affected_rows;
        match &plan {
            SqlExecutionPlan::Insert(_) | SqlExecutionPlan::MultiInsert(_) | SqlExecutionPlan::InsertSelect(_) => {
                format!("INSERT 0 {affected}")
            }
            SqlExecutionPlan::Update(_) | SqlExecutionPlan::UpdateScan(_) => format!("UPDATE {affected}"),
            SqlExecutionPlan::Delete(_) | SqlExecutionPlan::DeleteScan(_) => format!("DELETE {affected}"),
            SqlExecutionPlan::Read(_)
            | SqlExecutionPlan::Scan(_)
            | SqlExecutionPlan::Join(_)
            | SqlExecutionPlan::Scalar(_)
            | SqlExecutionPlan::SetOperation(_) => {
                format!("SELECT {}", result.rows.len())
            }
        }
    };
    debug_assert_eq!(description.len(), result.columns.len(), "described columns must match executed columns");
    Ok(ExecutionOutcome { is_read, description, rows: result.rows, command_tag })
}

/// Maps a structured gateway failure to its PostgreSQL SQLSTATE. Protocol code
/// never inspects diagnostic text to recover the category.
pub fn pg_sqlstate(kind: SqlErrorKind) -> &'static str {
    use SqlErrorKind::*;
    match kind {
        Syntax => "42601",
        UnknownTable | UnknownTableQualifier => "42P01",
        AmbiguousTable | AmbiguousColumn => "42702",
        UnknownColumn => "42703",
        TypeMismatch => "42804",
        ValueTooLong => "22001",
        NotNullViolation => "23502",
        // PostgreSQL 把「缺 NOT NULL 且无 DEFAULT 的列」并入 not-null 违例。
        MissingDefault => "23502",
        Unsupported => "0A000",
        UnknownFunction => "42883",
        UnknownVariable => "42704",
        // Only failures the gateway cannot classify reach the generic SQLSTATE.
        Internal => "XX000",
        CardinalityViolation => "21000",
        InvalidGrouping => "42803",
        OperandColumns => "42601",
        Parameter => "08P01",
        DuplicateKey => "23505",
        Conflict => "40001",
        DivisionByZero => "22012",
        NumericOverflow => "22003",
        InvalidSubstringLength => "22011",
        TableAlreadyExists => "42P07",
        ColumnAlreadyExists => "42701",
        IndexAlreadyExists => "42P07",
        UnknownIndex => "42704",
        CheckViolation => "23514",
        ForeignKeyViolation => "23503",
        InvalidTextRepresentation => "22P02",
    }
}

/// Renders one front-end failure as `(SQLSTATE, message)` for the wire and the
/// local engine.
pub fn statement_error(stage: &str, error: SqlFrontendError) -> (String, String) {
    (pg_sqlstate(error.kind).to_string(), format!("SQL {stage} error: {}", error.message))
}
