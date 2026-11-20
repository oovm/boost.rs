//! PostgreSQL statement execution: statement dispatch, bind → plan → execute
//! and the SQLSTATE mapping.
//!
//! [`dispatch_statement`] is the one semantic source both entry points use: the
//! local engine (`postgres` facade) and the wire gateway. Parsing and planning
//! belong to `database.query`; this module executes the resulting plan and owns
//! the transaction/DDL ordering that both paths must observe.

use std::sync::Arc;

use database_query::{
    OnCommit, PgObjectStatement, SqlCatalogBoundProjection, SqlCatalogBoundStatement, SqlColumn, SqlDdlStatement, SqlDialect, SqlErrorKind,
    SqlExecutionPlan, SqlFrontendError, SqlSessionCommand, SqlTransactionAction, SqlValue, analyze_meta_query, bind_sql_catalog, describe_plan,
    is_ddl_statement, parse_ddl, parse_pg_object_statement, parse_pg_utility, parse_session_command, plan_statement_for_dialect,
    rewrite_double_colon_casts, rewrite_escape_strings, rewrite_pg_temp_references,
};

use crate::{
    cluster::TransactionHandle,
    sql_schema::{SqlDdlOutcome, apply_auto_increment},
};

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
    // DDL resolves against the local schema registry before the `data.text` session
    // parser, so a `CREATE` / `ALTER` / `DROP` outside the supported subset is
    // reported as such instead of being misclassified as an unknown table.
    if let Some(object) = parse_pg_object_statement(trimmed).map_err(|error| {
        session.fail();
        StatementError::new(pg_sqlstate(error.kind), format!("SQL object error: {}", error.message))
    })? {
        return apply_object_statement(object, runtime, session);
    }
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
        StatementError::new("42601", "SQL parser could not parse the statement")
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
        Some(command) => {
            // `statement_timeout` is per-statement execution state, so the base
            // dispatch applies it for the wire session and the local engine
            // alike. Every other GUC stays a protocol-session surface.
            match &command {
                SqlSessionCommand::SetVariable { name, value, .. } if name.trim().eq_ignore_ascii_case("statement_timeout") => {
                    if session.is_aborted() {
                        return Err(StatementError::new("25P02", ABORTED_BLOCK));
                    }
                    let rendered = render_setting(value);
                    if !session.set_statement_timeout(&rendered) {
                        session.fail();
                        return Err(StatementError::new("22023", format!("invalid value for parameter \"statement_timeout\": \"{rendered}\"")));
                    }
                    return Ok(StatementOutcome::Command("SET"));
                }
                SqlSessionCommand::ResetVariables { name }
                    if name.as_deref().is_some_and(|name| name.trim().eq_ignore_ascii_case("statement_timeout")) =>
                {
                    if session.is_aborted() {
                        return Err(StatementError::new("25P02", ABORTED_BLOCK));
                    }
                    session.set_statement_timeout("0");
                    return Ok(StatementOutcome::Command("RESET"));
                }
                _ => {}
            }
            Ok(StatementOutcome::Session(command))
        }
        None => {
            let Some(runtime) = runtime
            else {
                session.fail();
                return Err(StatementError::new("0A000", "PostgreSQL query execution is not implemented (no SQL runtime)"));
            };
            run_statement(trimmed, runtime, session)
                .map(StatementOutcome::Result)
                .map_err(|(sqlstate, message)| StatementError::new(sqlstate, message))
        }
    }
}

/// Table privilege one plan requires, when the shape names exactly one table.
fn required_privilege(plan: &SqlExecutionPlan) -> Option<(database_storage::pgsql::PgPrivilege, &str)> {
    use database_storage::pgsql::PgPrivilege;
    match plan {
        SqlExecutionPlan::Read(plan) => Some((PgPrivilege::Select, plan.namespace.as_str())),
        SqlExecutionPlan::Scan(plan) => Some((PgPrivilege::Select, plan.namespace.as_str())),
        SqlExecutionPlan::Insert(plan) => Some((PgPrivilege::Insert, plan.namespace.as_str())),
        SqlExecutionPlan::MultiInsert(plans) => plans.first().map(|plan| (PgPrivilege::Insert, plan.namespace.as_str())),
        SqlExecutionPlan::Update(plan) => Some((PgPrivilege::Update, plan.namespace.as_str())),
        SqlExecutionPlan::UpdateScan(plan) => Some((PgPrivilege::Update, plan.namespace.as_str())),
        SqlExecutionPlan::Delete(plan) => Some((PgPrivilege::Delete, plan.namespace.as_str())),
        SqlExecutionPlan::DeleteScan(plan) => Some((PgPrivilege::Delete, plan.namespace.as_str())),
        // `DELETE ... USING` 跨多张表，与连接一样无法归到单表授权，交由受限角色拒绝。
        SqlExecutionPlan::Join(_)
        | SqlExecutionPlan::Scalar(_)
        | SqlExecutionPlan::SetOperation(_)
        | SqlExecutionPlan::InsertSelect(_)
        | SqlExecutionPlan::DeleteUsing(_) => None,
    }
}

/// Checks the session role's table grants against the plan.
///
/// The check participates in the same authorization path authentication uses:
/// the role comes from the startup `user`, and only a role the catalog knows and
/// that is not a superuser is restricted. A role the catalog does not know keeps
/// the pre-catalog behaviour, and a temporary table is always reachable by the
/// session that created it. A shape the check cannot classify is refused with
/// `0A000` for a restricted role instead of being silently allowed.
fn enforce_privileges(
    plan: &SqlExecutionPlan,
    catalog: &database_query::SqlCatalog,
    runtime: &Arc<PgsqlSqlRuntime>,
    role: &str,
    is_temp_type: impl Fn(u64) -> bool,
) -> Result<(), (String, String)> {
    let Some(entry) = runtime.objects.role(role)
    else {
        return Ok(());
    };
    if entry.superuser {
        return Ok(());
    }
    let Some((privilege, namespace)) = required_privilege(plan)
    else {
        return Err((
            "0A000".to_string(),
            format!("statement shape cannot be authorized for role \"{role}\": only single-table reads and writes are supported"),
        ));
    };
    let Some(type_id) = namespace.strip_prefix("table:").and_then(|id| id.parse::<u64>().ok())
    else {
        return Ok(());
    };
    if is_temp_type(type_id) {
        return Ok(());
    }
    let Some(table) = catalog.tables().iter().find(|table| table.type_id == type_id)
    else {
        return Ok(());
    };
    if runtime.objects.has_privilege(role, &table.name, privilege) {
        return Ok(());
    }
    Err(("42501".to_string(), format!("permission denied for table {}", table.name)))
}

/// Applies a `CREATE DATABASE` / `CREATE ROLE` / `GRANT` / `REVOKE` statement.
fn apply_object_statement(
    statement: PgObjectStatement,
    runtime: Option<&Arc<PgsqlSqlRuntime>>,
    session: &mut TransactionSession,
) -> Result<StatementOutcome, StatementError> {
    if session.is_aborted() {
        return Err(StatementError::new("25P02", ABORTED_BLOCK));
    }
    let Some(runtime) = runtime
    else {
        return Err(StatementError::new("0A000", "YYDS PostgreSQL SQL execution is not implemented (no SQL runtime)"));
    };
    let objects = Arc::clone(&runtime.objects);
    let catalog = session.catalog(&runtime.schema);
    let table_exists = |name: &str| catalog.tables().iter().any(|table| table.name == name);
    let failure = |session: &mut TransactionSession, error: database_storage::pgsql::ObjectError| {
        session.fail();
        StatementError::new(error.sqlstate(), error.message())
    };
    match statement {
        PgObjectStatement::CreateDatabase { database, if_not_exists } => {
            validate_database(&database, &objects).map_err(|error| failure(session, error))?;
            objects.create_database(database, if_not_exists).map_err(|error| failure(session, error))?;
            Ok(StatementOutcome::Command("CREATE DATABASE"))
        }
        PgObjectStatement::CreateRole { role, password, if_not_exists } => {
            let role = match password {
                Some(password) => {
                    let password =
                        database_storage::pgsql::PgPassword::derive(&password, &role.name).map_err(|error| failure(session, error))?;
                    database_storage::pgsql::PgRole { password: Some(password), ..role }
                }
                None => role,
            };
            objects.create_role(role, if_not_exists).map_err(|error| failure(session, error))?;
            Ok(StatementOutcome::Command("CREATE ROLE"))
        }
        PgObjectStatement::Grant { privileges, table, roles } => {
            for role in &roles {
                objects.grant(role, &table, &privileges, table_exists).map_err(|error| failure(session, error))?;
            }
            Ok(StatementOutcome::Command("GRANT"))
        }
        PgObjectStatement::Revoke { privileges, table, roles } => {
            for role in &roles {
                objects.revoke(role, &table, &privileges, table_exists).map_err(|error| failure(session, error))?;
            }
            Ok(StatementOutcome::Command("REVOKE"))
        }
        PgObjectStatement::GrantMembership { roles, members } => {
            for role in &roles {
                for member in &members {
                    objects.grant_membership(role, member).map_err(|error| failure(session, error))?;
                }
            }
            Ok(StatementOutcome::Command("GRANT ROLE"))
        }
        PgObjectStatement::RevokeMembership { roles, members } => {
            for role in &roles {
                for member in &members {
                    objects.revoke_membership(role, member).map_err(|error| failure(session, error))?;
                }
            }
            Ok(StatementOutcome::Command("REVOKE ROLE"))
        }
        PgObjectStatement::GrantAllTables { schema, privileges, roles } => {
            let tables = schema_tables(&schema, &catalog).map_err(|error| failure(session, error))?;
            for role in &roles {
                objects.grant_tables(role, &tables, &privileges).map_err(|error| failure(session, error))?;
            }
            Ok(StatementOutcome::Command("GRANT"))
        }
        PgObjectStatement::RevokeAllTables { schema, privileges, roles } => {
            let tables = schema_tables(&schema, &catalog).map_err(|error| failure(session, error))?;
            for role in &roles {
                objects.revoke_tables(role, &tables, &privileges).map_err(|error| failure(session, error))?;
            }
            Ok(StatementOutcome::Command("REVOKE"))
        }
        PgObjectStatement::CreateSequence { sequence, if_not_exists } => {
            objects.create_sequence(sequence, if_not_exists).map_err(|error| failure(session, error))?;
            Ok(StatementOutcome::Command("CREATE SEQUENCE"))
        }
        PgObjectStatement::AlterSequence { name, alteration } => {
            objects.alter_sequence(&name, &alteration).map_err(|error| failure(session, error))?;
            Ok(StatementOutcome::Command("ALTER SEQUENCE"))
        }
        PgObjectStatement::DropSequence { names, if_exists } => {
            for name in &names {
                objects.drop_sequence(name, if_exists).map_err(|error| failure(session, error))?;
            }
            Ok(StatementOutcome::Command("DROP SEQUENCE"))
        }
    }
}

/// Checks the options one `CREATE DATABASE` records.
///
/// `ENCODING` is the gateway's one storage encoding, `TEMPLATE` must be a
/// database the cluster knows, `OWNER` must be an existing role and
/// `CONNECTION LIMIT` must be `-1` (unlimited) or a non-negative count.
fn validate_database(
    database: &database_storage::pgsql::PgDatabase,
    objects: &database_storage::pgsql::PgObjectCatalog,
) -> Result<(), database_storage::pgsql::ObjectError> {
    use database_storage::pgsql::ObjectError;
    if !matches!(database.encoding.to_ascii_uppercase().as_str(), "UTF8" | "UNICODE") {
        return Err(ObjectError::InvalidOption(format!("encoding \"{}\" is not supported, the gateway stores UTF8", database.encoding)));
    }
    if database.connection_limit < -1 {
        return Err(ObjectError::InvalidOption("`CONNECTION LIMIT` must be -1 or a non-negative number".to_string()));
    }
    if objects.role(&database.owner).is_none() {
        return Err(ObjectError::UnknownRole(database.owner.clone()));
    }
    if !database.template.eq_ignore_ascii_case("template0")
        && !database.template.eq_ignore_ascii_case("template1")
        && objects.database(&database.template).is_none()
    {
        return Err(ObjectError::UnknownTemplate(database.template.clone()));
    }
    Ok(())
}

/// Tables `GRANT ... ON ALL TABLES IN SCHEMA name` expands over.
///
/// The gateway publishes one schema, so any other name is `3F000` instead of a
/// silent no-op. The expansion only covers the relations that exist now, which
/// is PostgreSQL's bulk-grant rule.
fn schema_tables(schema: &str, catalog: &database_query::SqlCatalog) -> Result<Vec<String>, database_storage::pgsql::ObjectError> {
    if !schema.eq_ignore_ascii_case("public") {
        return Err(database_storage::pgsql::ObjectError::UnknownSchema(schema.to_string()));
    }
    Ok(catalog.tables().iter().map(|table| table.name.clone()).collect())
}

/// Sequence state one statement exposes to `nextval` / `currval` / `setval`.
///
/// The owner stays `database.storage::pgsql::PgObjectCatalog`; this adapter only
/// maps its failures onto the SQLSTATE categories the frontend already carries.
struct ObjectSequences(Arc<database_storage::pgsql::PgObjectCatalog>);

impl database_query::SqlSequenceCatalog for ObjectSequences {
    fn nextval(&self, name: &str) -> Result<i64, SqlFrontendError> {
        self.0.nextval(name).map_err(sequence_error)
    }

    fn currval(&self, name: &str) -> Result<i64, SqlFrontendError> {
        self.0.currval(name).map_err(sequence_error)
    }

    fn setval(&self, name: &str, value: i64, is_called: bool) -> Result<i64, SqlFrontendError> {
        self.0.setval(name, value, is_called).map_err(sequence_error)
    }
}

/// Maps one catalog failure onto the frontend category that names its SQLSTATE.
fn sequence_error(error: database_storage::pgsql::ObjectError) -> SqlFrontendError {
    use database_storage::pgsql::ObjectError;
    let kind = match &error {
        ObjectError::UnknownTable(_) => SqlErrorKind::UnknownTable,
        ObjectError::SequenceExhausted(_) => SqlErrorKind::SequenceExhausted,
        ObjectError::SequenceNotCalled(_) => SqlErrorKind::SequenceNotCalled,
        _ => SqlErrorKind::Unsupported,
    };
    SqlFrontendError::new(kind, error.message())
}

/// Renders a `SET` value the way the GUC parser expects it.
fn render_setting(value: &database_query::SqlBoundExpression) -> String {
    use database_query::{SqlBoundExpression, SqlBoundLiteral};
    match value {
        SqlBoundExpression::Literal(SqlBoundLiteral::String(text)) => text.clone(),
        SqlBoundExpression::Literal(SqlBoundLiteral::Integer(number)) => number.to_string(),
        SqlBoundExpression::Literal(SqlBoundLiteral::Number(text)) => text.clone(),
        SqlBoundExpression::Literal(SqlBoundLiteral::Boolean(flag)) => flag.to_string(),
        SqlBoundExpression::Identifier(name) => name.clone(),
        _ => String::new(),
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
    outcome: &SqlDdlOutcome,
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
    if !matches!(outcome, SqlDdlOutcome::AlterTableAddColumn { added: true }) {
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
    crate::sql_exec::backfill_generated_rows(runtime.executor.as_ref(), session.transaction_handle(), &namespace, &plan, &field_types, dialect)
        .map(|_| ())
        .map_err(|error| {
            let (code, message) = statement_error("backfill", error);
            StatementError::new(code, message)
        })
}

/// bind → plan → execute，错误已映射为 SQLSTATE，并已标记事务失败。
/// Runs one already-rendered statement and enforces the session's
/// `statement_timeout`.
///
/// The engine executes synchronously and cannot preempt a running operator, so
/// the deadline is checked when the statement returns: the result is discarded,
/// `57014` is reported and an open block is aborted, which rolls its writes
/// back. A timeout outside a block leaves the already-published statement in
/// place, exactly like a client-side cancel that arrived after the commit.
pub fn run_statement(
    sql: &str,
    runtime: &Arc<PgsqlSqlRuntime>,
    session: &mut TransactionSession,
) -> Result<ExecutionOutcome, (String, String)> {
    // An aborted block rejects every later statement without executing it, so a
    // stale `statement_timeout` cannot mask the `25P02` the client must see.
    if session.is_aborted() {
        return Err(("25P02".to_string(), ABORTED_BLOCK.to_string()));
    }
    let started = std::time::Instant::now();
    let outcome = run_statement_inner(sql, runtime, session)?;
    if let Some(timeout) = session.statement_timeout()
        && started.elapsed() > timeout
    {
        session.fail();
        return Err(("57014".to_string(), "canceling statement due to statement timeout".to_string()));
    }
    Ok(outcome)
}

fn run_statement_inner(
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
        let result = meta.execute(&runtime.schema.snapshot(), "public");
        let command_tag = format!("SELECT {}", result.rows.len());
        return Ok(ExecutionOutcome { is_read: true, description, rows: result.rows, command_tag });
    }
    // 扩展查询不经过 `dispatch_statement`，语法错误必须在这里映射为 `42601`
    // 而不是被后面的 catalog 绑定错误统一归为 `42P01`。
    if parse_session_command(sql).is_err() {
        session.fail();
        return Err(("42601".to_string(), "SQL parser could not parse the statement".to_string()));
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
    enforce_privileges(&plan, &catalog, runtime, session.user(), |type_id| session.owns_temp_type(type_id))?;
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
    // `nextval` / `currval` / `setval` read and advance the durable sequence
    // state in the object catalog; the statement installs it for its own run.
    let sequences: Arc<dyn database_query::SqlSequenceCatalog> = Arc::new(ObjectSequences(Arc::clone(&runtime.objects)));
    let result = database_query::with_sequence_catalog(sequences, || {
        crate::sql_exec::execute_plan(runtime.executor.as_ref(), handle_ref, &plan, SqlDialect::Postgres)
    })
    .map_err(|error| {
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
            SqlExecutionPlan::Delete(_) | SqlExecutionPlan::DeleteScan(_) | SqlExecutionPlan::DeleteUsing(_) => format!("DELETE {affected}"),
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
        SequenceExhausted => "2200H",
        SequenceNotCalled => "55000",
    }
}

/// Renders one front-end failure as `(SQLSTATE, message)` for the wire and the
/// local engine.
pub fn statement_error(stage: &str, error: SqlFrontendError) -> (String, String) {
    (pg_sqlstate(error.kind).to_string(), format!("SQL {stage} error: {}", error.message))
}
