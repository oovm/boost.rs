//! PostgreSQL statement execution: statement dispatch, bind → plan → execute
//! and the SQLSTATE mapping.
//!
//! [`dispatch_statement`] is the one semantic source both entry points use: the
//! local engine (`postgres` facade) and the wire gateway. Parsing and planning
//! belong to `database.query`; this module executes the resulting plan and owns
//! the transaction/DDL ordering that both paths must observe.

use std::sync::Arc;

use database_query::{
    OnCommit, SqlCatalogBoundProjection, SqlCatalogBoundStatement, SqlColumn, SqlDdlStatement, SqlDialect, SqlErrorKind, SqlExecutionPlan,
    SqlFrontendError, SqlSessionCommand, SqlTransactionAction, SqlValue, analyze_meta_query, apply_auto_increment, bind_sql_catalog,
    describe_plan, is_ddl_statement, parse_ddl, parse_pg_utility, parse_session_command, plan_statement_for_dialect,
    rewrite_double_colon_casts, rewrite_escape_strings, rewrite_pg_temp_references,
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
#[derive(Debug)]
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
    // `E'...'` literals decode to the standard spelling first, so every later
    // stage — the cast lowering included — sees one literal form.
    let escaped = rewrite_escape_strings(trimmed).map_err(|error| {
        session.fail();
        StatementError::new(pg_sqlstate(error.kind), format!("SQL literal error: {}", error.message))
    })?;
    let escaped = escaped.as_str();
    let normalized = rewrite_double_colon_casts(escaped, SqlDialect::Postgres).map_err(|error| {
        session.fail();
        StatementError::new(pg_sqlstate(error.kind), format!("SQL cast error: {}", error.message))
    })?;
    let escaped = normalized.as_ref();
    // `pg_temp.<table>` names the session's temporary schema; the shared binder
    // only resolves bare names, so the qualifier is lowered onto the bare name
    // for every table this session owns. DDL keeps its qualifier because the DDL
    // parser owns the `CREATE TEMP TABLE pg_temp.x` spelling.
    let temp_rewritten;
    let trimmed = if is_ddl_statement(escaped) {
        escaped
    }
    else {
        temp_rewritten = rewrite_pg_temp_references(escaped, |name| session.temp_table_exists(name));
        temp_rewritten.as_str()
    };
    // `PREPARE` / `EXECUTE` / `DEALLOCATE` wrap another statement, so they run
    // before DDL detection would misclassify the wrapped text.
    if let Some(utility) = parse_pg_utility(trimmed).map_err(|error| {
        session.fail();
        StatementError::new(pg_sqlstate(error.kind), format!("SQL utility error: {}", error.message))
    })? {
        return match &utility {
            database_query::PgUtilityStatement::DeclareCursor { .. }
            | database_query::PgUtilityStatement::Fetch { .. }
            | database_query::PgUtilityStatement::CloseCursor { .. } => super::cursors::execute_cursor_utility(&utility, runtime, session),
            _ => super::prepared::execute_utility(&utility, runtime, session),
        };
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
            // `ON COMMIT DELETE ROWS` would have to run a write statement from
            // inside the commit epilogue, so it stays an explicit rejection
            // instead of silently preserving the rows.
            if let SqlDdlStatement::CreateTable { on_commit: OnCommit::DeleteRows, .. } = &statement {
                session.fail();
                return Err(StatementError::new(
                    "0A000",
                    "`ON COMMIT DELETE ROWS` is not supported: the gateway cannot delete rows at the commit boundary, use `ON COMMIT DROP` or delete explicitly",
                ));
            }
            let temporary = match &statement {
                SqlDdlStatement::CreateTable { temporary, name, .. } => *temporary || session.temp_table_exists(name),
                SqlDdlStatement::AlterTableAddColumn { table, .. } => session.temp_table_exists(table),
                SqlDdlStatement::AlterTableAddConstraint { table, .. } => session.temp_table_exists(table),
                SqlDdlStatement::DropTable { names, .. } => names.iter().any(|name| session.temp_table_exists(name)),
                SqlDdlStatement::CreateIndex { .. } | SqlDdlStatement::DropIndex { .. } => false,
            };
            let applied = if temporary {
                session.apply_temp_ddl(&statement)
            }
            else {
                let Some(runtime) = runtime
                else {
                    return Err(StatementError::new("0A000", "YYDS PostgreSQL SQL execution is not implemented"));
                };
                runtime.schema.apply_ddl(&statement)
            };
            return match applied {
                Ok(outcome) => {
                    if let Some(runtime) = runtime {
                        backfill_added_generated_column(&statement, &outcome, session, runtime)?;
                    }
                    Ok(StatementOutcome::Command(outcome.command_tag()))
                }
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

/// `ALTER TABLE ... ADD COLUMN ... GENERATED ... STORED`: computes the new column
/// for every existing row right after the DDL lands.
///
/// PostgreSQL backfills generated columns inside the same statement, so the
/// registry must never claim a generated column whose stored rows still carry
/// no value. The backfill runs through the same change set as a statement
/// write, so it publishes once and restores with a rolled-back block.
fn backfill_added_generated_column(
    statement: &SqlDdlStatement,
    outcome: &database_query::SqlDdlOutcome,
    session: &TransactionSession,
    runtime: &Arc<PgsqlSqlRuntime>,
) -> Result<(), StatementError> {
    let SqlDdlStatement::AlterTableAddColumn { table, column, .. } = statement
    else {
        return Ok(());
    };
    let Some(expression) = &column.generated
    else {
        return Ok(());
    };
    if !matches!(outcome, database_query::SqlDdlOutcome::AlterTableAddColumn { added: true }) {
        return Ok(());
    }
    let dialect = SqlDialect::Postgres;
    let catalog = session.catalog(&runtime.schema);
    let Some(table_entry) = catalog.tables().iter().find(|entry| &entry.name == table)
    else {
        return Err(StatementError::new("42P01", format!("relation \"{table}\" does not exist")));
    };
    let Some(field) = table_entry.fields.iter().find(|field| field.name == column.name.as_str())
    else {
        return Err(StatementError::new("42703", format!("column \"{}\" does not exist", column.name)));
    };
    let source = format!("SELECT ({expression}) FROM \"{}\"", table_entry.name);
    let bound = bind_sql_catalog(&source, &catalog, catalog.schema_version(), dialect).map_err(|error| {
        let (code, message) = statement_error("bind", error);
        StatementError::new(code, message)
    })?;
    let SqlCatalogBoundStatement::Select(select) = bound
    else {
        return Err(StatementError::new("XX000", "generated column binding produced a non-SELECT statement"));
    };
    let Some(SqlCatalogBoundProjection::Expression { expression: bound_expression, .. }) = select.projections.into_iter().next()
    else {
        return Err(StatementError::new("XX000", "generated column binding produced no projection"));
    };
    let plan = database_query::GeneratedColumnPlan {
        field_id: field.field_id,
        name: field.name.clone(),
        field_type: field.field_type.clone(),
        capacity: field.capacity,
        expression: bound_expression,
    };
    let field_types = table_entry.fields.iter().map(|field| (field.field_id, field.field_type.clone())).collect();
    let namespace = format!("table:{}", table_entry.type_id);
    crate::sql_exec::backfill_generated_rows(
        runtime.executor.as_ref(),
        session.transaction_handle(),
        &namespace,
        &plan,
        &field_types,
        dialect,
    )
    .map(|_| ())
    .map_err(|error| {
        let (code, message) = statement_error("backfill", error);
        StatementError::new(code, message)
    })
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
    let escaped = rewrite_escape_strings(&normalized).map_err(|error| {
        session.fail();
        statement_error("literal", error)
    })?;
    let sql = escaped.as_str();
    // The extended protocol reaches this function without `dispatch_statement`,
    // so the temporary-schema rewrite happens here as well.
    let temp_rewritten = rewrite_pg_temp_references(sql, |name| session.temp_table_exists(name));
    let sql = temp_rewritten.as_str();
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
    let catalog = session.catalog(&runtime.schema);
    let bound = bind_sql_catalog(sql, &catalog, catalog.schema_version(), SqlDialect::Postgres).map_err(|error| {
        session.fail();
        statement_error("bind", error)
    })?;
    // Sequence-owning columns default at write time. Allocation advances and
    // persists the counter before execution, so a failed or rolled-back
    // statement never reuses the value, matching PostgreSQL `nextval`. A
    // temporary table draws its sequence from the session's registry.
    let sequences = match &bound {
        database_query::SqlCatalogBoundStatement::Insert(insert) => session.sequence_registry(&runtime.schema, insert.table.type_id),
        _ => &runtime.schema,
    };
    let bound = apply_auto_increment(sequences, bound).map_err(|error| {
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
        GeneratedColumnWrite => "428C9",
    }
}

/// Renders one front-end failure as `(SQLSTATE, message)` for the wire and the
/// local engine.
pub fn statement_error(stage: &str, error: SqlFrontendError) -> (String, String) {
    (pg_sqlstate(error.kind).to_string(), format!("SQL {stage} error: {}", error.message))
}
