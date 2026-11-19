//! MySQL statement pipeline: session-command dispatch, the bind → plan →
//! execute path and the prepared-statement registry.
//!
//! The gateway protocol layer owns packet framing, sequence numbers and row
//! encoding; this module owns *which* response a statement produces and every
//! session transition behind it, so the local engine and the wire session run
//! the same mechanism. The split mirrors [`pgsql`](crate::pgsql): the dialect
//! front end stays in `database.query`, the runtime behavior lives here.

use std::{borrow::Cow, collections::HashMap, io, sync::Arc};

use database_protocol_mysql::{
    MYSQL_TYPE_BLOB, MYSQL_TYPE_DATE, MYSQL_TYPE_DATETIME, MYSQL_TYPE_DOUBLE, MYSQL_TYPE_LONG, MYSQL_TYPE_LONGLONG, MYSQL_TYPE_NEWDECIMAL,
    MYSQL_TYPE_TINY, MYSQL_TYPE_VAR_STRING, SERVER_STATUS_IN_TRANS, append_packet, column_definition_payload, encode_length_encoded,
};
use database_query::{
    MYSQL_BLOB_CAPACITY, SqlBoundExpression, SqlBoundLiteral, SqlCatalog, SqlCatalogBoundExpression, SqlCatalogBoundStatement, SqlColumn,
    SqlDialect, SqlExecutionPlan, SqlFieldType, SqlFrontendError, SqlParameterStyle, SqlResult, SqlSessionCommand, SqlTransactionAction,
    SqlValue, analyze_meta_query, bind_sql_catalog_with_variables, describe_plan_for_dialect, is_accepted_collation,
    neutralized_sql_parameters, parse_ddl, parse_session_command, plan_statement_for_dialect, substitute_sql_parameters,
};

use crate::{cluster::TransactionHandle, sql_exec::execute_plan, sql_schema::apply_auto_increment};

use super::session::{
    MYSQL_VARIABLES, MysqlError, MysqlSqlRuntime, TransactionSession, is_known_mysql_variable, like_matches, mysql_system_variable,
    statement_error,
};

/// Protocol-side output of the MySQL statement pipeline.
///
/// The wire gateway implements this over a connection: execution decides which
/// response a statement produces (OK, ERR or a result set) and performs the
/// session transitions behind it, while the sink owns packet framing and the
/// response sequence. Every method answers the current command with exactly one
/// response at the gateway's fixed response sequence.
pub trait MysqlStatementSink {
    /// Writes an OK packet carrying the affected-row count, the generated
    /// `last_insert_id` when the statement allocated one, and the session
    /// status flags.
    fn write_ok(&mut self, affected_rows: u64, last_insert_id: Option<u64>, status_flags: u16) -> io::Result<()>;

    /// Writes an ERR packet.
    fn write_error(&mut self, code: u16, state: &str, message: &str) -> io::Result<()>;

    /// Writes a full result set: column count, definitions, rows and the
    /// trailing EOF. `prefix` reuses pre-encoded column-definition frames for a
    /// cached plan, paired with the number of columns they encode.
    fn write_result_set(
        &mut self,
        result: &SqlResult,
        column_types: &[u8],
        column_nullables: &[bool],
        binary_rows: bool,
        prefix: Option<(&[u8], usize)>,
        status_flags: u16,
    ) -> io::Result<()>;
}

/// Turns a structured frontend failure from one pipeline stage into the ERR
/// response written through the sink.
fn write_statement_error(sink: &mut impl MysqlStatementSink, stage: &str, error: SqlFrontendError) -> io::Result<()> {
    let error = statement_error(stage, error);
    sink.write_error(error.code, error.state, &error.message)
}

/// Writes a [`MysqlError`] produced by the session machinery through the sink.
fn write_mysql_error(sink: &mut impl MysqlStatementSink, error: &MysqlError) -> io::Result<()> {
    sink.write_error(error.code, error.state, &error.message)
}

/// Returns the first SQL keyword, skipping leading whitespace and comments.
///
/// Session-command detection parses a full syntax tree, which is wasted on the
/// DML/SELECT statements that dominate a gateway's traffic. A verb check up
/// front keeps that parser off the hot path without changing which statements
/// the session layer accepts.
fn leading_statement_verb(sql: &str) -> &str {
    let mut rest = sql.trim_start();
    loop {
        if let Some(comment) = rest.strip_prefix("--") {
            rest = comment.split_once('\n').map_or("", |(_, tail)| tail).trim_start();
        }
        else if let Some(comment) = rest.strip_prefix("/*") {
            rest = comment.split_once("*/").map_or("", |(_, tail)| tail).trim_start();
        }
        else {
            break;
        }
    }
    rest.split(|ch: char| ch.is_ascii_whitespace() || matches!(ch, '(' | ';')).next().unwrap_or("")
}

/// Whether `verb` can start a statement the session layer replaces with a
/// protocol command. Everything else is safe to send straight to the binder.
fn is_session_verb(verb: &str) -> bool {
    ["SET", "BEGIN", "START", "COMMIT", "ROLLBACK", "SAVEPOINT", "RELEASE", "SHOW", "PRAGMA", "RESET", "DISCARD"]
        .iter()
        .any(|candidate| verb.eq_ignore_ascii_case(candidate))
}

fn is_select(sql: &str) -> bool {
    sql.len() >= 6 && sql.as_bytes()[..6].eq_ignore_ascii_case(b"select")
}

/// Cheap pre-filter for [`analyze_meta_query`].
///
/// The metadata analyzer normalises and lowercases the whole statement before
/// looking for its trigger words, which is pure overhead for the DML/SELECT
/// traffic that dominates a gateway. This scan rejects those statements without
/// allocating; it is deliberately conservative — a `SHOW` verb always reaches
/// the analyzer so whitespace variants still parse.
fn may_be_metadata_query(sql: &str) -> bool {
    const NEEDLES: &[&str] =
        &["information_schema", "pg_catalog", "show tables", "show databases", "database(", "current_database(", "current_schema(", "version("];
    NEEDLES.iter().any(|needle| contains_ignore_ascii_case(sql, needle)) || leading_statement_verb(sql).eq_ignore_ascii_case("show")
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|window| window.iter().zip(needle).all(|(left, right)| left.eq_ignore_ascii_case(right)))
}

/// MySQL drivers spell boolean session values as `ON`/`OFF`, which the parser reads as
/// bare identifiers rather than literals. Rewrite only that trailing literal as
/// `1`/`0` — the numeric form binds to an integer the session layer can read —
/// and leave the rest of the statement to the parser.
fn normalize_session_boolean(sql: &str) -> Cow<'_, str> {
    let trimmed = sql.trim_end();
    let (statement, terminator) = match trimmed.strip_suffix(';') {
        Some(statement) => (statement.trim_end(), ";"),
        None => (trimmed, ""),
    };
    if !statement.get(..4).is_some_and(|prefix| prefix.eq_ignore_ascii_case("SET ")) {
        return Cow::Borrowed(sql);
    }
    let Some(index) = statement.rfind('=')
    else {
        return Cow::Borrowed(sql);
    };
    let value = statement[index + 1..].trim();
    let replacement = if value.eq_ignore_ascii_case("ON") {
        "1"
    }
    else if value.eq_ignore_ascii_case("OFF") {
        "0"
    }
    else {
        return Cow::Borrowed(sql);
    };
    Cow::Owned(format!("{} {replacement}{terminator}", &statement[..=index]))
}

/// MySQL column type for a catalog-declared gateway field type.
fn mysql_field_type(field_type: &SqlFieldType) -> u8 {
    match field_type.inner() {
        SqlFieldType::Int32 => MYSQL_TYPE_LONG,
        SqlFieldType::Int64 => MYSQL_TYPE_LONGLONG,
        SqlFieldType::Float64 => MYSQL_TYPE_DOUBLE,
        SqlFieldType::Bool => MYSQL_TYPE_TINY,
        SqlFieldType::Bytes => MYSQL_TYPE_BLOB,
        // Exact aggregate results (`SUM`/`AVG` over integer operands) are reported
        // as `DECIMAL` in MySQL, which drivers read back as strings.
        SqlFieldType::Decimal => MYSQL_TYPE_NEWDECIMAL,
        // 列类型上报为 DATE/DATETIME 后，mysql2 会按类型把文本值转成 JS Date，
        // 与真库 8.4 的 `types` 场景一致。
        SqlFieldType::Date => MYSQL_TYPE_DATE,
        SqlFieldType::Datetime => MYSQL_TYPE_DATETIME,
        // uuid 的 wire 载荷是连字符文本，按变长字符串上报。
        SqlFieldType::Uuid => MYSQL_TYPE_VAR_STRING,
        SqlFieldType::String | SqlFieldType::Unknown | SqlFieldType::Optional(_) => MYSQL_TYPE_VAR_STRING,
    }
}

/// A planned SELECT plus the result shape derived from that plan.
#[derive(Debug, Clone)]
pub(crate) struct CachedSelectPlan {
    plan: Arc<SqlExecutionPlan>,
    column_types: Vec<u8>,
    column_nullables: Vec<bool>,
    /// Column-count and column-definition packets encoded for a response that
    /// starts at sequence 1. Rows and the trailing EOF are still appended per
    /// statement because their values and status flags change.
    prefix: Vec<u8>,
}

/// Upper bound on cached SELECT plans per session. The workloads that repeat
/// SQL keep a couple of hundred entries; crossing the bound drops the cache
/// instead of growing without limit.
const SELECT_CACHE_LIMIT: usize = 512;

/// The value `apply_auto_increment` allocated for the last row of an INSERT.
///
/// Only called when a sequence-owning column was actually appended; an INSERT
/// that supplies every column itself reports `0`, like the engine.
fn allocated_last_insert_id(statement: &SqlCatalogBoundStatement) -> Option<u64> {
    let SqlCatalogBoundStatement::Insert(insert) = statement
    else {
        return None;
    };
    match insert.rows.last()?.last()? {
        SqlCatalogBoundExpression::Literal(SqlBoundLiteral::Integer(value)) if *value >= 0 => Some(*value as u64),
        _ => None,
    }
}

/// Number of bound columns an INSERT supplies. `apply_auto_increment` appends
/// one column per sequence-owning field that the statement omitted.
fn bound_insert_column_count(statement: &SqlCatalogBoundStatement) -> usize {
    match statement {
        SqlCatalogBoundStatement::Insert(insert) => insert.fields.len(),
        _ => 0,
    }
}

/// Binds, applies sequence defaults and plans one statement.
fn build_plan(
    sql: &str,
    runtime: &Arc<MysqlSqlRuntime>,
    session: &mut TransactionSession,
    catalog: &SqlCatalog,
) -> Result<(SqlExecutionPlan, Option<u64>), (&'static str, SqlFrontendError)> {
    let bound = bind_sql_catalog_with_variables(
        sql,
        catalog,
        catalog.schema_version(),
        &|name: &str| mysql_system_variable(name, session.autocommit),
        SqlDialect::Mysql,
    )
    .map_err(|error| ("bind", error))?;
    // `AUTO_INCREMENT` / serial columns default at write time, after binding and
    // before planning. The counter is consumed before execution and survives a
    // rollback, matching both MySQL and PostgreSQL sequence semantics.
    let columns_before = bound_insert_column_count(&bound);
    let bound = apply_auto_increment(&runtime.schema, bound).map_err(|error| ("bind", error))?;
    // The engine reports the generated value in the OK packet; drivers surface
    // it as `insertId` / `lastrowid`.
    let last_insert_id = if bound_insert_column_count(&bound) > columns_before { allocated_last_insert_id(&bound) } else { None };
    let plan = plan_statement_for_dialect(bound, catalog, SqlDialect::Mysql).map_err(|error| ("plan", error))?;
    Ok((plan, last_insert_id))
}

/// Derives the wire column types a plan declares.
fn plan_column_types(plan: &SqlExecutionPlan) -> (Vec<u8>, Vec<bool>) {
    let columns = describe_plan_for_dialect(plan, SqlDialect::Mysql);
    let types = columns.iter().map(|column| mysql_field_type(&column.field_type)).collect();
    let nullables = columns.iter().map(|column| column.field_type.is_nullable()).collect();
    (types, nullables)
}

/// Pre-encodes the column-count and column-definition packets for a response
/// that starts at sequence 1, so a cached plan reuses them instead of rebuilding
/// one `Vec` per column on every execution. The trailing EOF stays dynamic
/// because it carries the session status flags.
fn encode_result_prefix(columns: &[SqlColumn], column_types: &[u8], column_nullables: &[bool]) -> Vec<u8> {
    let mut prefix = Vec::new();
    let mut count = Vec::new();
    encode_length_encoded(&mut count, columns.len() as u64);
    if append_packet(&mut prefix, 1, &count).is_err() {
        return Vec::new();
    }
    for (index, column) in columns.iter().enumerate() {
        let declared = column_types.get(index).copied().unwrap_or_else(|| mysql_field_type(&column.field_type));
        let nullable = column_nullables.get(index).copied().unwrap_or(true);
        let definition = column_definition_payload(&column.name, declared, nullable, MYSQL_BLOB_CAPACITY as u32);
        if append_packet(&mut prefix, 2u8.wrapping_add(index as u8), &definition).is_err() {
            return Vec::new();
        }
    }
    prefix
}

/// Runs one `COM_QUERY` statement text through the full MySQL session
/// machinery: the DDL pre-check, the metadata pre-filter, session commands and
/// the plain bind → plan → execute path.
///
/// The gateway has already decoded the SQL text and fixed the response
/// sequence; every branch below produces exactly one sink response.
pub fn dispatch_query(
    sink: &mut impl MysqlStatementSink,
    sql: &str,
    runtime: Option<&Arc<MysqlSqlRuntime>>,
    session: &mut TransactionSession,
) -> io::Result<()> {
    // DDL resolves against the local schema registry instead of the
    // session-command parser, so a `CREATE` / `ALTER` / `DROP` that the
    // registry cannot apply never falls through to the binder.
    // The first keyword decides whether a full DDL parse can pay off;
    // this is the same leading-word test `parse_ddl` performs.
    let verb = leading_statement_verb(sql);
    let is_ddl = verb.eq_ignore_ascii_case("create") || verb.eq_ignore_ascii_case("alter") || verb.eq_ignore_ascii_case("drop");
    let parsed_ddl = if is_ddl { parse_ddl(sql, SqlDialect::Mysql) } else { Ok(None) };
    match parsed_ddl {
        Err(error) => return write_statement_error(sink, "ddl", error),
        Ok(Some(statement)) => {
            let Some(rt) = runtime
            else {
                return sink.write_error(1235, "42000", "no SQL runtime");
            };
            if session.failed {
                return sink.write_error(40001, "HY000", "current transaction is aborted");
            }
            return match rt.schema.apply_ddl(&statement) {
                Ok(_) => sink.write_ok(0, None, session.status_flags()),
                Err(error) => write_statement_error(sink, "ddl", error),
            };
        }
        Ok(None) => {}
    }
    // Metadata views never touch the executor or the session transaction: they
    // are synthesized from the local schema registry. `analyze_meta_query`
    // normalises and lowercases the whole statement before looking for its
    // trigger words; reject the DML/SELECT traffic with the allocation-free
    // pre-filter first.
    let analyzed_meta = if may_be_metadata_query(sql) { analyze_meta_query(sql, SqlDialect::Mysql) } else { Ok(None) };
    match analyzed_meta {
        Err(error) => {
            session.fail();
            write_statement_error(sink, "metadata", error)
        }
        Ok(Some(meta)) => {
            let Some(rt) = runtime
            else {
                return sink.write_error(1235, "42000", "no SQL runtime");
            };
            let described = meta.describe();
            let result = meta.execute(&rt.schema.snapshot(), &session.schema);
            let column_types: Vec<u8> = described.iter().map(|column| mysql_field_type(&column.field_type)).collect();
            let column_nullables: Vec<bool> = described.iter().map(|column| column.field_type.is_nullable()).collect();
            let status_flags = session.status_flags();
            sink.write_result_set(&result, &column_types, &column_nullables, false, None, status_flags)
        }
        Ok(None) => {
            // Only session verbs can project to a protocol command. DML and
            // SELECT skip the extra full parse. Empty input keeps the old path
            // so its error classification does not change.
            let parsed_session =
                if verb.is_empty() || is_session_verb(verb) { parse_session_command(&normalize_session_boolean(sql)) } else { Ok(None) };
            session_command(sink, parsed_session, sql, runtime, session)
        }
    }
}

/// Dispatches one parsed session command, mirroring the gateway's historical
/// branch-for-branch responses.
fn session_command(
    sink: &mut impl MysqlStatementSink,
    parsed: Result<Option<SqlSessionCommand>, SqlFrontendError>,
    sql: &str,
    runtime: Option<&Arc<MysqlSqlRuntime>>,
    session: &mut TransactionSession,
) -> io::Result<()> {
    match parsed {
        Ok(Some(SqlSessionCommand::SetNames { character_set, .. })) if !character_set.eq_ignore_ascii_case("utf8mb4") => {
            sink.write_error(1115, "42000", "unknown character set")
        }
        // 文本按 code point 精确比较，collation 只是会话元数据；接受与 DDL
        // 同一份白名单，驱动默认的 `utf8mb4_0900_ai_ci` 也能建立连接。
        Ok(Some(SqlSessionCommand::SetNames { collation, .. })) if collation.as_deref().is_none_or(is_accepted_collation) => {
            sink.write_ok(0, None, session.status_flags())
        }
        Ok(Some(SqlSessionCommand::SetNames { collation, .. })) => {
            // Engine text for `ER_UNKNOWN_COLLATION`.
            let name = collation.as_deref().unwrap_or("utf8mb4");
            sink.write_error(1273, "HY000", &format!("Unknown collation: '{name}'"))
        }
        Ok(Some(SqlSessionCommand::Transaction(action))) => transaction_command(sink, action, runtime, session),
        Ok(Some(SqlSessionCommand::Savepoint(intent))) => match session.savepoint(intent) {
            Ok(()) => sink.write_ok(0, None, session.status_flags()),
            Err(error) => write_mysql_error(sink, &error),
        },
        Ok(Some(SqlSessionCommand::ShowVariables { pattern, .. })) => {
            let mut rows = Vec::new();
            for (name, value) in MYSQL_VARIABLES {
                if pattern.as_deref().is_none_or(|pattern| like_matches(pattern, name)) {
                    let effective_value = if *name == "autocommit" { if session.autocommit { "ON" } else { "OFF" } } else { value };
                    rows.push(vec![SqlValue::String((*name).to_string()), SqlValue::String(effective_value.to_string())]);
                }
            }
            let result = SqlResult { columns: vec!["Variable_name".into(), "Value".into()], rows, affected_rows: 0 };
            let status_flags = session.status_flags();
            sink.write_result_set(&result, &[], &[false, false], false, None, status_flags)
        }
        Ok(Some(SqlSessionCommand::SetVariable { name, value, .. })) => set_variable(sink, &name, value, runtime, session),
        Ok(Some(SqlSessionCommand::ShowParameter { name, .. })) => {
            session.fail();
            sink.write_error(1064, "42000", &format!("MySQL requires SHOW VARIABLES instead of SHOW {name}"))
        }
        Ok(Some(SqlSessionCommand::Pragma { .. })) => {
            session.fail();
            sink.write_error(1064, "42000", "PRAGMA is not MySQL syntax")
        }
        Ok(Some(SqlSessionCommand::ResetVariables { .. })) | Ok(Some(SqlSessionCommand::Discard(_))) => {
            // `RESET ...` / `DISCARD ...` are PostgreSQL session verbs. MySQL has no
            // equivalent statement in the gateway subset, so answer with its normal
            // syntax error instead of pretending to reset state.
            sink.write_error(1064, "42000", "RESET / DISCARD session statements are PostgreSQL syntax")
        }
        Err(_) => {
            // 语法错误与“未接入执行器”必须是不同错误，否则没有 runtime 时
            // 客户端会把语法错误误判为不支持的能力。
            session.fail();
            sink.write_error(1064, "42000", "SQL parser could not parse the statement")
        }
        Ok(None) => {
            if session.failed {
                return sink.write_error(40001, "HY000", "current transaction is aborted");
            }
            match runtime {
                Some(rt) => execute_sql(sink, sql, rt, session, &[], false, &[], true),
                None => sink.write_error(1235, "42000", "YYDS MySQL SQL execution is not implemented (no SQL runtime)"),
            }
        }
    }
}

/// Transaction-control verbs, one response each.
fn transaction_command(
    sink: &mut impl MysqlStatementSink,
    action: SqlTransactionAction,
    runtime: Option<&Arc<MysqlSqlRuntime>>,
    session: &mut TransactionSession,
) -> io::Result<()> {
    match action {
        SqlTransactionAction::Begin if session.failed => sink.write_error(40001, "HY000", "current transaction is aborted"),
        SqlTransactionAction::Begin => {
            let Some(rt) = runtime
            else {
                return sink.write_error(1235, "42000", "no SQL runtime");
            };
            // The executor owns the transaction capability contract: BEGIN goes
            // through `begin_transaction` so an executor without a durable atomic
            // commit path rejects the verb at its boundary. A BEGIN failure does
            // not abort the session — nothing was opened.
            match session.begin(rt) {
                Ok(()) => sink.write_ok(0, None, SERVER_STATUS_IN_TRANS),
                Err(message) => sink.write_error(1235, "42000", &message),
            }
        }
        SqlTransactionAction::Commit => match session.commit(runtime)? {
            Ok(()) => sink.write_ok(0, None, session.status_flags()),
            Err(error) => write_mysql_error(sink, &error),
        },
        SqlTransactionAction::Rollback => {
            session.rollback(runtime)?;
            sink.write_ok(0, None, session.status_flags())
        }
        SqlTransactionAction::Savepoint(_) | SqlTransactionAction::RollbackToSavepoint(_) | SqlTransactionAction::ReleaseSavepoint(_) => {
            sink.write_error(1235, "42000", "SAVEPOINT / ROLLBACK TO SAVEPOINT / RELEASE SAVEPOINT are not yet supported by the gateway")
        }
    }
}

/// `SET <variable>`: MySQL only accepts known session variables; `autocommit`
/// is the one with transaction side effects.
fn set_variable(
    sink: &mut impl MysqlStatementSink,
    name: &str,
    value: SqlBoundExpression,
    runtime: Option<&Arc<MysqlSqlRuntime>>,
    session: &mut TransactionSession,
) -> io::Result<()> {
    // MySQL 只接受已知 session 变量；未知变量应返回 1193。
    if !is_known_mysql_variable(name) {
        session.fail();
        return sink.write_error(1193, "HY000", &format!("Unknown system variable '{name}'"));
    }
    // Values arrive as `@@session.autocommit` from drivers that qualify the
    // scope, so strip the same prefix the variable resolver strips before
    // matching the session flag.
    let bare = name.strip_prefix("@@").unwrap_or(name);
    let needle = bare.rsplit('.').next().unwrap_or(bare).trim().to_ascii_lowercase();
    if needle == "autocommit" {
        let autocommit = match value {
            SqlBoundExpression::Literal(SqlBoundLiteral::String(s)) => TransactionSession::parse_bool_value(&s),
            SqlBoundExpression::Literal(SqlBoundLiteral::Integer(n)) => n != 0,
            _ => true, // 默认 ON
        };
        // MySQL commits whatever transaction is open when autocommit is switched
        // back on, including the handle this session opened implicitly.
        match session.set_autocommit(runtime, autocommit)? {
            Ok(()) => {}
            Err(error) => return write_mysql_error(sink, &error),
        }
    }
    sink.write_ok(0, None, session.status_flags())
}

/// Runs one SQL statement through the full bind → plan → execute pipeline and
/// emits exactly one response through the sink.
///
/// Depending on the plan variant: SELECT → result set; INSERT/UPDATE/DELETE →
/// OK with the real affected-row count returned by the executor (multi-row
/// INSERT counts N, UPDATE/DELETE count matched rows).
/// When `column_types` is empty the declared column types are inferred from the
/// plan, otherwise the caller's already-declared types are used.
/// `binary_rows` encodes result rows with the MySQL binary protocol (the
/// prepared-statement path).
fn execute_sql(
    sink: &mut impl MysqlStatementSink,
    sql: &str,
    runtime: &Arc<MysqlSqlRuntime>,
    session: &mut TransactionSession,
    column_types: &[u8],
    binary_rows: bool,
    column_nullables: &[bool],
    meta_checked: bool,
) -> io::Result<()> {
    if !meta_checked && may_be_metadata_query(sql) {
        let analyzed_meta = analyze_meta_query(sql, SqlDialect::Mysql);
        match analyzed_meta {
            Err(error) => {
                session.fail();
                return write_statement_error(sink, "metadata", error);
            }
            Ok(Some(meta)) => {
                let described = meta.describe();
                let result = meta.execute(&runtime.schema.snapshot(), &session.schema);
                let declared_types: Vec<u8> = described.iter().map(|column| mysql_field_type(&column.field_type)).collect();
                let declared_nullables: Vec<bool> = described.iter().map(|column| column.field_type.is_nullable()).collect();
                let column_types = if column_types.is_empty() { &declared_types[..] } else { column_types };
                let column_nullables = if column_nullables.is_empty() { &declared_nullables[..] } else { column_nullables };
                let status_flags = session.status_flags();
                return sink.write_result_set(&result, column_types, column_nullables, binary_rows, None, status_flags);
            }
            Ok(None) => {}
        }
    }
    let revision = runtime.schema.revision();
    if session.catalog_cache.as_ref().is_none_or(|(cached, _)| *cached != revision) {
        session.select_cache.clear();
        session.catalog_cache = Some((revision, Arc::new(runtime.schema.catalog())));
    }
    // Only SELECT plans are catalog-stable: writes allocate AUTO_INCREMENT
    // values at bind time and must never be replayed from a cache. `@@`
    // references and unrendered placeholders stay uncached because their
    // resolved values are baked into the plan.
    let cacheable = is_select(sql) && !sql.contains("@@") && !sql.contains('?');
    if cacheable {
        if let Some(cached) = session.select_cache.get(sql).cloned() {
            let (declared_types, declared_nullables) = if column_types.is_empty() {
                (&cached.column_types[..], &cached.column_nullables[..])
            }
            else {
                (column_types, column_nullables)
            };
            return finish_plan(
                sink,
                &cached.plan,
                None,
                declared_types,
                declared_nullables,
                binary_rows,
                Some((&cached.prefix, cached.column_types.len())),
                runtime,
                session,
            );
        }
    }
    let catalog = Arc::clone(&session.catalog_cache.as_ref().expect("catalog cache was just filled").1);
    let (plan, last_insert_id) = match build_plan(sql, runtime, session, &catalog) {
        Ok(planned) => planned,
        Err((stage, error)) => {
            session.fail();
            return write_statement_error(sink, stage, error);
        }
    };
    let plan = Arc::new(plan);
    // Text protocol (`COM_QUERY`) has no prepare-time description, so derive the
    // declared column types from the plan instead of guessing from row values.
    // Prepared executes already carry that description, and describing again
    // would put an extra plan walk on every first-time parameter value.
    let described = if column_types.is_empty() { Some(describe_plan_for_dialect(&plan, SqlDialect::Mysql)) } else { None };
    let (declared_types, declared_nullables) = match &described {
        Some(columns) => (
            columns.iter().map(|column| mysql_field_type(&column.field_type)).collect(),
            columns.iter().map(|column| column.field_type.is_nullable()).collect(),
        ),
        None => (column_types.to_vec(), column_nullables.to_vec()),
    };
    if cacheable {
        let prefix =
            described.as_deref().map(|columns| encode_result_prefix(columns, &declared_types, &declared_nullables)).unwrap_or_default();
        if session.select_cache.len() >= SELECT_CACHE_LIMIT {
            session.select_cache.clear();
        }
        session.select_cache.insert(
            sql.to_string(),
            Arc::new(CachedSelectPlan {
                plan: Arc::clone(&plan),
                column_types: declared_types.clone(),
                column_nullables: declared_nullables.clone(),
                prefix,
            }),
        );
    }
    finish_plan(sink, &plan, last_insert_id, &declared_types, &declared_nullables, binary_rows, None, runtime, session)
}

/// Executes one resolved plan and emits its OK or result-set response.
#[allow(clippy::too_many_arguments)]
fn finish_plan(
    sink: &mut impl MysqlStatementSink,
    plan: &SqlExecutionPlan,
    last_insert_id: Option<u64>,
    declared_types: &[u8],
    declared_nullables: &[bool],
    binary_rows: bool,
    prefix: Option<(&[u8], usize)>,
    runtime: &Arc<MysqlSqlRuntime>,
    session: &mut TransactionSession,
) -> io::Result<()> {
    // DML 写操作 —— 先 execute 再回 OK packet
    let is_write = !matches!(
        plan,
        SqlExecutionPlan::Read(_)
            | SqlExecutionPlan::Scan(_)
            | SqlExecutionPlan::Join(_)
            | SqlExecutionPlan::Scalar(_)
            | SqlExecutionPlan::SetOperation(_)
    );
    // MySQL drivers enable transactions with `SET autocommit = 0` rather than
    // sending `BEGIN`, so the first write has to open the handle lazily before
    // it can join the client's transaction block.
    if is_write && session.handle.is_none() && !session.autocommit {
        if let Err(err) = session.begin_implicit_transaction(runtime) {
            session.fail();
            return sink.write_error(1235, "42000", &format!("BEGIN failed: {err}"));
        }
    }
    let handle_ref: Option<&dyn TransactionHandle> = session.handle.as_deref();
    let result = match execute_plan(runtime.executor.as_ref(), handle_ref, plan, SqlDialect::Mysql) {
        Ok(result) => result,
        Err(error) => {
            session.fail();
            return write_statement_error(sink, "execution", error);
        }
    };
    let status_flags = session.status_flags();
    if is_write {
        return sink.write_ok(result.affected_rows, last_insert_id, status_flags);
    }
    let (owned_types, owned_nullables);
    let (column_types, column_nullables) = if declared_types.is_empty() {
        let (types, nullables) = plan_column_types(plan);
        owned_types = types;
        owned_nullables = nullables;
        (&owned_types[..], &owned_nullables[..])
    }
    else {
        (declared_types, declared_nullables)
    };
    sink.write_result_set(&result, column_types, column_nullables, binary_rows, prefix, status_flags)
}

/// Result-column shape a prepared statement advertises: the wire type code and
/// nullability already mapped from the catalog type.
#[derive(Debug, Clone)]
pub struct MysqlColumnDescription {
    /// Column name as the plan declares it.
    pub name: String,
    /// `MYSQL_TYPE_*` code reported in the ColumnDefinition packet.
    pub type_code: u8,
    /// Whether the column definition advertises the nullable flag.
    pub nullable: bool,
}

/// Outcome of [`prepare_statement`].
pub enum MysqlPrepareOutcome {
    /// The failure response was already written through the sink.
    Failed,
    /// The statement prepared; the gateway advertises the shape and registers
    /// it under a fresh id.
    Statement {
        /// Number of `?` placeholders the statement references.
        parameter_count: usize,
        /// Result-column shape, empty for writes and metadata-less statements.
        columns: Vec<MysqlColumnDescription>,
    },
}

/// Validates one prepared statement against the catalog and derives its result
/// shape. The gateway owns packet framing (prepare-OK, parameter and column
/// definitions) and the statement-id registry.
pub fn prepare_statement(
    sink: &mut impl MysqlStatementSink,
    sql: &str,
    runtime: Option<&Arc<MysqlSqlRuntime>>,
    session: &mut TransactionSession,
) -> io::Result<MysqlPrepareOutcome> {
    let Some(runtime) = runtime
    else {
        session.fail();
        sink.write_error(1235, "42000", "YYDS MySQL SQL execution is not implemented (no SQL runtime)")?;
        return Ok(MysqlPrepareOutcome::Failed);
    };
    let (neutral, parameter_count) = match neutralized_sql_parameters(sql, SqlParameterStyle::QuestionMark) {
        Ok(result) => result,
        Err(error) => {
            session.fail();
            write_statement_error(sink, "parameter", error)?;
            return Ok(MysqlPrepareOutcome::Failed);
        }
    };
    let description = match analyze_meta_query(&neutral, SqlDialect::Mysql) {
        Err(error) => {
            session.fail();
            write_statement_error(sink, "metadata", error)?;
            return Ok(MysqlPrepareOutcome::Failed);
        }
        Ok(Some(meta)) => meta.describe(),
        Ok(None) => {
            let catalog = runtime.schema.catalog();
            let bound = match bind_sql_catalog_with_variables(
                &neutral,
                &catalog,
                catalog.schema_version(),
                &|name: &str| mysql_system_variable(name, session.autocommit),
                SqlDialect::Mysql,
            ) {
                Ok(bound) => bound,
                Err(error) => {
                    session.fail();
                    write_statement_error(sink, "bind", error)?;
                    return Ok(MysqlPrepareOutcome::Failed);
                }
            };
            // 写入语句的值类型检查推迟到 execute，因为只有执行时才知道参数的真实类型，
            // 因此这里只为 SELECT 推导结果列。
            match bound {
                SqlCatalogBoundStatement::Select(_) | SqlCatalogBoundStatement::SetOperation(_) => {
                    match plan_statement_for_dialect(bound, &catalog, SqlDialect::Mysql) {
                        Ok(plan) => describe_plan_for_dialect(&plan, SqlDialect::Mysql),
                        Err(error) => {
                            session.fail();
                            write_statement_error(sink, "plan", error)?;
                            return Ok(MysqlPrepareOutcome::Failed);
                        }
                    }
                }
                _ => Vec::new(),
            }
        }
    };
    Ok(MysqlPrepareOutcome::Statement {
        parameter_count,
        columns: description
            .iter()
            .map(|column| MysqlColumnDescription {
                name: column.name.clone(),
                type_code: mysql_field_type(&column.field_type),
                nullable: column.field_type.is_nullable(),
            })
            .collect(),
    })
}

/// One rendered SQL text plus the exact parameter values that produced it.
/// Values are compared element-wise instead of by hash so a collision can
/// never reuse the wrong statement text.
#[derive(Debug, Clone)]
struct RenderedSql {
    values: Vec<SqlValue>,
    text: Arc<str>,
}

/// Upper bound on rendered texts per prepared statement. A parameter cycle
/// longer than this re-renders instead of growing without limit.
const RENDER_CACHE_LIMIT: usize = 256;

/// A prepared statement as advertised by `COM_STMT_PREPARE`: the SQL text, the
/// catalog-derived result shape and the parameter types the client last bound.
#[derive(Debug)]
pub struct MysqlPreparedStatement {
    /// Statement text as prepared (placeholders still present).
    pub sql: String,
    /// Number of `?` placeholders the statement references.
    pub parameter_count: usize,
    /// Wire type of every result column, derived once at prepare time so an
    /// execute never rebuilds these vectors.
    pub column_types: Vec<u8>,
    /// Nullability of every result column.
    pub column_nullables: Vec<bool>,
    /// `(type code, unsigned flag)` per parameter, remembered so an execute
    /// that reuses the previous binding can decode values.
    pub parameter_types: Vec<(u8, bool)>,
    /// Rendered SQL keyed by the decoded parameter values. Rendering depends
    /// only on the value list (the null bitmap and wire types are already
    /// folded into those values), so a repeated bind skips `walk_parameters`
    /// and its per-character buffer entirely.
    render_cache: Vec<RenderedSql>,
}

impl MysqlPreparedStatement {
    /// Registers a freshly prepared statement: no parameter types bound yet
    /// and an empty render cache.
    pub fn new(sql: String, parameter_count: usize, column_types: Vec<u8>, column_nullables: Vec<bool>) -> Self {
        Self { sql, parameter_count, column_types, column_nullables, parameter_types: Vec::new(), render_cache: Vec::new() }
    }

    /// Looks up a previously rendered text for exactly these values.
    pub fn cached_render(&self, values: &[SqlValue]) -> Option<Arc<str>> {
        self.render_cache.iter().find(|entry| entry.values == values).map(|entry| Arc::clone(&entry.text))
    }

    /// Remembers one rendered text, dropping the cache when it outgrows the
    /// bound instead of growing without limit.
    pub fn remember_render(&mut self, values: &[SqlValue], text: Arc<str>) {
        if self.render_cache.len() >= RENDER_CACHE_LIMIT {
            self.render_cache.clear();
        }
        self.render_cache.push(RenderedSql { values: values.to_vec(), text });
    }
}

/// Per-connection prepared statement registry. Statement ids are never reused
/// and the whole table is dropped when the session ends.
#[derive(Debug)]
pub struct MysqlPreparedRegistry {
    entries: HashMap<u32, MysqlPreparedStatement>,
    next_id: u32,
}

impl MysqlPreparedRegistry {
    /// An empty registry starting at statement id 1.
    pub fn new() -> Self {
        Self { entries: HashMap::new(), next_id: 1 }
    }

    /// Reserves the next statement id.
    pub fn allocate(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        id
    }

    /// Registers one prepared statement under an allocated id.
    pub fn insert(&mut self, id: u32, statement: MysqlPreparedStatement) {
        self.entries.insert(id, statement);
    }

    /// Looks one statement up.
    pub fn get(&self, id: &u32) -> Option<&MysqlPreparedStatement> {
        self.entries.get(id)
    }

    /// Mutable access for the execute path, which remembers renders and
    /// re-binds parameter types.
    pub fn get_mut(&mut self, id: &u32) -> Option<&mut MysqlPreparedStatement> {
        self.entries.get_mut(id)
    }

    /// Whether the id is registered (`COM_STMT_RESET`).
    pub fn contains(&self, id: &u32) -> bool {
        self.entries.contains_key(id)
    }

    /// Drops one statement (`COM_STMT_CLOSE`).
    pub fn remove(&mut self, id: &u32) -> Option<MysqlPreparedStatement> {
        self.entries.remove(id)
    }

    /// Drops every statement (`COM_RESET_CONNECTION`).
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

impl Default for MysqlPreparedRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Runs one already-decoded `COM_STMT_EXECUTE` binding through the shared SQL
/// pipeline: the aborted-block rule, the rendered-text cache and the execute
/// with the statement's declared column shape.
pub fn execute_prepared(
    sink: &mut impl MysqlStatementSink,
    statement_id: u32,
    values: &[SqlValue],
    runtime: Option<&Arc<MysqlSqlRuntime>>,
    registry: &mut MysqlPreparedRegistry,
    session: &mut TransactionSession,
) -> io::Result<()> {
    if session.failed {
        return sink.write_error(40001, "HY000", "current transaction is aborted");
    }
    let Some(runtime) = runtime
    else {
        session.fail();
        return sink.write_error(1235, "42000", "YYDS MySQL SQL execution is not implemented (no SQL runtime)");
    };
    let Some(statement) = registry.get_mut(&statement_id)
    else {
        return sink.write_error(1243, "HY000", &format!("Unknown prepared statement handler ({statement_id})"));
    };
    let rendered: Arc<str> = match statement.cached_render(values) {
        Some(text) => text,
        None => {
            let rendered = match substitute_sql_parameters(&statement.sql, values, SqlParameterStyle::QuestionMark) {
                Ok(rendered) => rendered,
                Err(error) => {
                    session.fail();
                    return write_statement_error(sink, "parameter", error);
                }
            };
            let text: Arc<str> = Arc::from(rendered);
            statement.remember_render(values, Arc::clone(&text));
            text
        }
    };
    execute_sql(sink, &rendered, runtime, session, &statement.column_types, true, &statement.column_nullables, false)
}
