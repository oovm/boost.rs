//! MySQL 运行时会话与事务机制。
//!
//! 这里是 MySQL 事务资源（执行器句柄、schema checkpoint/restore、autocommit
//! 切换、savepoint、aborted 标记）的唯一 owner；协议网关只按调用顺序驱动这些
//! 方法并把结果写成 wire 响应。与 [`pgsql`](crate::pgsql) 相同的分工，但保留
//! MySQL 自身的方言差异：没有 ReadyForQuery 字节，事务状态经 OK/EOF 的状态位
//! 回报，`SET autocommit = 0` 隐式打开事务。

use std::{collections::HashMap, io, sync::Arc};

use database_protocol_mysql::{SERVER_STATUS_AUTOCOMMIT, SERVER_STATUS_IN_TRANS};
use database_query::{SavepointIntent, SqlCatalog, SqlErrorKind, SqlFrontendError};
use database_types::cluster::Error as ClusterError;

use super::statement::CachedSelectPlan;
use crate::{
    cluster::{KeyValueExecutor, TransactionHandle},
    sql_schema::{SqlSchemaCheckpoint, SqlSchemaRegistry},
};

/// Runtime dependencies injected into a MySQL session so SQL SELECT and DDL can execute.
#[derive(Debug)]
pub struct MysqlSqlRuntime {
    /// Local schema registry holding catalog snapshots and applying DDL.
    pub schema: Arc<SqlSchemaRegistry>,
    /// KV execution plane.
    pub executor: Arc<dyn KeyValueExecutor>,
}

/// A structured failure the gateway must report as a MySQL ERR packet.
#[derive(Debug)]
pub struct MysqlError {
    /// MySQL error number.
    pub code: u16,
    /// SQLSTATE carried in the ERR packet.
    pub state: &'static str,
    /// Engine-shaped message text.
    pub message: String,
}

impl MysqlError {
    pub(crate) fn new(code: u16, state: &'static str, message: impl Into<String>) -> Self {
        Self { code, state, message: message.into() }
    }
}

/// Maps a structured gateway failure to its MySQL error number and SQLSTATE.
pub fn mysql_error_code(kind: SqlErrorKind) -> (u16, &'static str) {
    use SqlErrorKind::*;
    match kind {
        Syntax => (1064, "42000"),
        UnknownTable | UnknownTableQualifier => (1146, "42S02"),
        AmbiguousTable | AmbiguousColumn => (1052, "23000"),
        UnknownColumn => (1054, "42S22"),
        TypeMismatch => (1366, "HY000"),
        ValueTooLong => (1406, "22001"),
        NotNullViolation => (1048, "23000"),
        // 真库 8.4 对「INSERT 缺 NOT NULL 且无 DEFAULT 的列」报 `ER_NO_DEFAULT_FOR_FIELD`。
        MissingDefault => (1364, "HY000"),
        Unsupported => (1235, "42000"),
        UnknownFunction => (1305, "42000"),
        UnknownVariable => (1193, "HY000"),
        // 只有无法分类的 catalog/storage 失败才落到通用码。
        Internal => (1105, "HY000"),
        CardinalityViolation => (1242, "21000"),
        InvalidGrouping => (1055, "42000"),
        OperandColumns => (1241, "21000"),
        Parameter => (1210, "HY000"),
        DuplicateKey => (1062, "23000"),
        Conflict => (1213, "40001"),
        DivisionByZero => (1365, "22012"),
        // MySQL 用 `1690` 报 BIGINT 越界，SQLSTATE 与 PostgreSQL 一致是 `22003`。
        NumericOverflow => (1690, "22003"),
        // PostgreSQL-only 分类：MySQL 对负长度返回空串，因此不会走到这里。
        InvalidSubstringLength => (1105, "HY000"),
        // PostgreSQL-only 分类：MySQL 的 CAST 对非法输入返回 0/NULL。
        InvalidTextRepresentation => (1105, "HY000"),
        GeneratedColumnWrite => (3105, "HY000"),
        // PostgreSQL-only sequence classifications: MySQL has no sequences.
        SequenceExhausted | SequenceNotCalled => (1105, "HY000"),
        TableAlreadyExists => (1050, "42S01"),
        ColumnAlreadyExists => (1060, "42S21"),
        IndexAlreadyExists => (1061, "42000"),
        UnknownIndex => (1091, "42000"),
        // MySQL 8.0.16+ 报 `ER_CHECK_CONSTRAINT_VIOLATED`，外键孤儿引用报 `1452`。
        CheckViolation => (3819, "HY000"),
        ForeignKeyViolation => (1452, "23000"),
    }
}

/// Turns a frontend failure from one pipeline stage into the ERR packet payload.
pub fn statement_error(stage: &str, error: SqlFrontendError) -> MysqlError {
    let (code, state) = mysql_error_code(error.kind);
    MysqlError::new(code, state, format!("SQL {stage} error: {}", error.message))
}

/// Emulated `SHOW VARIABLES` table. Values describe what the gateway actually
/// implements: no strict mode, a single-node engine, and utf8mb4 text.
pub(crate) const MYSQL_VARIABLES: &[(&str, &str)] = &[
    ("autocommit", "ON"),
    ("character_set_client", "utf8mb4"),
    ("character_set_connection", "utf8mb4"),
    ("character_set_results", "utf8mb4"),
    ("character_set_server", "utf8mb4"),
    ("collation_connection", "utf8mb4_bin"),
    ("collation_server", "utf8mb4_bin"),
    ("hostname", "localhost"),
    ("init_connect", ""),
    ("lower_case_table_names", "0"),
    ("max_allowed_packet", "1048576"),
    ("net_write_timeout", "60"),
    ("socket", ""),
    ("sql_mode", ""),
    ("time_zone", "SYSTEM"),
    ("transaction_isolation", "READ-COMMITTED"),
    ("tx_isolation", "READ-COMMITTED"),
    ("version", "8.0.36-yyds"),
    ("version_comment", "YYDS MySQL gateway"),
    ("wait_timeout", "28800"),
];

/// Resolves a `SELECT @@version` style system-variable probe against the
/// session's effective variable values.
///
/// The scope prefix (`@@global.` / `@@session.` / `@@local.`) is accepted and
/// ignored: the gateway keeps one variable table per connection. A value the
/// client assigned with `SET` shadows the static default for the rest of the
/// connection, which is how drivers read back `SET time_zone = '+08:00'`
/// instead of the startup `SYSTEM`. Numeric-looking values are returned as
/// numbers so drivers reading `@@max_allowed_packet` or
/// `@@lower_case_table_names` get integer semantics.
///
/// `autocommit` is the one variable the session owns. Drivers read it back to
/// decide whether they still need to disable it, so answering with the static
/// table would hide a session that already switched transactions off.
pub(crate) fn mysql_system_variable(name: &str, session: &TransactionSession) -> Result<database_query::SqlBoundLiteral, SqlFrontendError> {
    let variable = mysql_variable_name(name);
    let value = session.variable_value(&variable)?;
    if !value.is_empty() && value.chars().all(|ch| ch.is_ascii_digit()) {
        if let Ok(number) = value.parse::<i64>() {
            return Ok(database_query::SqlBoundLiteral::Integer(number));
        }
    }
    Ok(database_query::SqlBoundLiteral::String(value))
}

/// Canonical name of a `SET` / `@@` variable reference: scope prefixes and the
/// `@@` sigil stripped, lowercased.
pub(crate) fn mysql_variable_name(name: &str) -> String {
    let bare = name.strip_prefix("@@").unwrap_or(name);
    bare.rsplit('.').next().unwrap_or(bare).trim().to_ascii_lowercase()
}

/// Default value of one variable from the static table.
fn mysql_variable_default(variable: &str) -> Option<&'static str> {
    MYSQL_VARIABLES.iter().find(|(candidate, _)| *candidate == variable).map(|(_, value)| *value)
}

/// Whether a `SET` target names a known MySQL session variable the gateway
/// emulates via `MYSQL_VARIABLES`. Rejects unknown names with `1193` at the
/// session layer instead of silently accepting them.
pub(crate) fn is_known_mysql_variable(name: &str) -> bool {
    let short = mysql_variable_name(name);
    MYSQL_VARIABLES.iter().any(|(key, _)| *key == short)
}

/// MySQL `LIKE` matching for `SHOW VARIABLES LIKE 'pattern'` — `%` matches any
/// run of bytes and `_` matches one byte, case-insensitively.
pub(crate) fn like_matches(pattern: &str, value: &str) -> bool {
    fn walk(pattern: &[u8], value: &[u8]) -> bool {
        match pattern.first() {
            None => value.is_empty(),
            Some(b'%') => walk(&pattern[1..], value) || (!value.is_empty() && walk(pattern, &value[1..])),
            Some(b'_') => !value.is_empty() && walk(&pattern[1..], &value[1..]),
            Some(byte) => value.first().is_some_and(|candidate| candidate.eq_ignore_ascii_case(byte)) && walk(&pattern[1..], &value[1..]),
        }
    }
    walk(pattern.to_ascii_lowercase().as_bytes(), value.to_ascii_lowercase().as_bytes())
}

/// Session-scoped transaction state. Unlike PostgreSQL there is no wire-protocol
/// ReadyForQuery byte for transaction mode, so the session only tracks the
/// storage handle and the failure flag; the gateway reports both through the
/// OK/EOF status flags.
#[derive(Debug)]
pub struct TransactionSession {
    pub(crate) executor: Option<Arc<dyn KeyValueExecutor>>,
    pub(crate) handle: Option<Box<dyn TransactionHandle + 'static>>,
    pub(crate) failed: bool,
    /// Whether `autocommit` session variable is ON (default). `SET autocommit = OFF`
    /// clears the `SERVER_STATUS_AUTOCOMMIT` flag on subsequent OK / EOF responses.
    pub(crate) autocommit: bool,
    /// Session-variable values the client assigned with `SET`. They shadow the
    /// static [`MYSQL_VARIABLES`] defaults for the rest of the connection and are
    /// dropped by `COM_RESET_CONNECTION`.
    pub(crate) variables: HashMap<String, String>,
    /// Schema selected by the handshake database field or `COM_INIT_DB`, used
    /// verbatim by `DATABASE()` and the synthesized `TABLE_SCHEMA` columns.
    pub(crate) schema: String,
    /// Registry snapshot captured at `BEGIN`. The catalog rides the same
    /// transaction as rows: `COMMIT` keeps it, every rollback path restores it.
    pub(crate) schema_checkpoint: Option<SqlSchemaCheckpoint>,
    /// Last catalog snapshot and the registry revision it came from. Reusing it
    /// skips a full schema clone per statement; any DDL bumps the revision.
    pub(crate) catalog_cache: Option<(u32, Arc<SqlCatalog>)>,
    /// Catalog-stable SELECT plans keyed by SQL text. Repeated point lookups
    /// (and prepared statements whose rendered values cycle) skip bind and
    /// planning entirely; any DDL revision drops the whole cache.
    pub(crate) select_cache: HashMap<String, Arc<CachedSelectPlan>>,
}

impl TransactionSession {
    /// A fresh session in autocommit mode with no open transaction.
    pub fn new() -> Self {
        Self {
            executor: None,
            handle: None,
            failed: false,
            autocommit: true,
            variables: HashMap::new(),
            schema: String::new(),
            schema_checkpoint: None,
            catalog_cache: None,
            select_cache: HashMap::new(),
        }
    }

    /// The default schema the session resolves unqualified names against.
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// Selects the default schema (`COM_INIT_DB` or the handshake database field).
    pub fn set_schema(&mut self, schema: impl Into<String>) {
        self.schema = schema.into();
    }

    /// Whether an explicit transaction handle is open.
    pub fn in_transaction(&self) -> bool {
        self.handle.is_some()
    }

    /// Whether the `autocommit` session variable is ON.
    pub fn is_autocommit(&self) -> bool {
        self.autocommit
    }

    /// Effective value of one session variable: the client's `SET` override,
    /// the `autocommit` flag, or the static default. Unknown names keep the
    /// `1193` classification instead of inventing a value.
    pub(crate) fn variable_value(&self, variable: &str) -> Result<String, SqlFrontendError> {
        if variable == "autocommit" {
            return Ok(if self.autocommit { "ON" } else { "OFF" }.to_string());
        }
        if let Some(value) = self.variables.get(variable) {
            return Ok(value.clone());
        }
        mysql_variable_default(variable)
            .map(str::to_string)
            .ok_or_else(|| SqlFrontendError::new(SqlErrorKind::UnknownVariable, format!("Unknown system variable '{variable}'")))
    }

    /// Records a `SET <variable> = <value>` for the rest of the connection.
    pub(crate) fn set_variable(&mut self, name: &str, value: String) {
        self.variables.insert(mysql_variable_name(name), value);
    }

    /// `SHOW VARIABLES` rows: static defaults in table order with every client
    /// override applied.
    pub(crate) fn variables_snapshot(&self) -> Vec<(&'static str, String)> {
        MYSQL_VARIABLES
            .iter()
            .map(|(name, default)| {
                let value = if *name == "autocommit" {
                    if self.autocommit { "ON".to_string() } else { "OFF".to_string() }
                }
                else if let Some(value) = self.variables.get(*name) {
                    value.clone()
                }
                else {
                    (*default).to_string()
                };
                (*name, value)
            })
            .collect()
    }

    /// Whether the transaction is aborted and only rollback can end it.
    pub fn is_failed(&self) -> bool {
        self.failed
    }

    /// Marks the open transaction aborted; statements then fail until rollback.
    pub fn fail(&mut self) {
        if self.handle.is_some() {
            self.failed = true;
        }
    }

    /// Captures the registry state when a transaction block opens.
    pub(crate) fn capture_schema(&mut self, runtime: Option<&Arc<MysqlSqlRuntime>>) {
        self.schema_checkpoint = runtime.map(|rt| rt.schema.checkpoint());
    }

    /// Restores the snapshot captured at `BEGIN`. Only rollback paths call
    /// this — a successful `COMMIT` drops the checkpoint instead.
    pub fn restore_schema(&mut self, runtime: Option<&Arc<MysqlSqlRuntime>>) -> io::Result<()> {
        if let (Some(checkpoint), Some(rt)) = (self.schema_checkpoint.take(), runtime) {
            rt.schema.restore(checkpoint).map_err(|error| io::Error::other(error.message))?;
        }
        Ok(())
    }

    /// Opens the transaction that `SET autocommit = 0` implies.
    ///
    /// MySQL clients switch transactions on with a session variable instead of
    /// sending `BEGIN`, so the first write of such a connection has to open
    /// the handle lazily. Without it every statement would commit on its own
    /// and the client's `ROLLBACK` would have nothing to undo.
    pub(crate) fn begin_implicit_transaction(&mut self, runtime: &Arc<MysqlSqlRuntime>) -> Result<(), ClusterError> {
        if self.handle.is_some() {
            return Ok(());
        }
        let handle = Arc::clone(&runtime.executor).begin_transaction(None)?;
        self.executor = Some(runtime.executor.clone());
        self.handle = Some(handle);
        self.failed = false;
        self.capture_schema(Some(runtime));
        Ok(())
    }

    /// `BEGIN`: opens the storage transaction and captures the catalog checkpoint.
    ///
    /// The executor owns the transaction capability contract: BEGIN goes
    /// through `begin_transaction` so an executor without a durable atomic
    /// commit path rejects the verb at its boundary. The single-node
    /// gateway passes `None` explicitly — there is no coordinator fence yet.
    pub fn begin(&mut self, runtime: &Arc<MysqlSqlRuntime>) -> Result<(), String> {
        match Arc::clone(&runtime.executor).begin_transaction(None) {
            Ok(handle) => {
                self.executor = Some(runtime.executor.clone());
                self.handle = Some(handle);
                self.failed = false;
                self.capture_schema(Some(runtime));
                Ok(())
            }
            Err(err) => Err(format!("BEGIN failed: {err}")),
        }
    }

    /// `COMMIT`: publishes the open transaction. Committing an aborted
    /// transaction rolls it back instead, catalog changes included.
    pub fn commit(&mut self, runtime: Option<&Arc<MysqlSqlRuntime>>) -> io::Result<Result<(), MysqlError>> {
        if self.failed {
            if let Some(handle) = self.handle.take() {
                handle.rollback();
            }
            self.executor = None;
            self.failed = false;
            // `COMMIT` of an aborted block is a rollback: catalog changes go too.
            self.restore_schema(runtime)?;
            return Ok(Ok(()));
        }
        let mut commit_error = None;
        if let Some(handle) = self.handle.take() {
            if let Err(err) = handle.commit() {
                commit_error = Some(err);
            }
        }
        self.executor = None;
        self.failed = false;
        match commit_error {
            Some(err) => {
                // A failed commit did not publish the transaction, so
                // the DDL it collected is discarded too.
                self.restore_schema(runtime)?;
                Ok(Err(match err {
                    // OCC 拒绝必须回报给客户端，否则并发写会被静默丢弃。
                    //
                    // 已知能力缺口：没有行级锁等待。并发写同一行的第二个事务
                    // 不会阻塞，先提交者在这里拿到可重试的 1213；提交是原子
                    // 的，失败事务不会晚写。补齐需要 `cluster/local.rs` 的
                    // `LocalTransactionHandle` 在语句阶段持锁并按有界、
                    // 可取消的方式等待（owner: execution cluster 事务层），
                    // MySQL 网关不伪造行锁。
                    ClusterError::CasConflict { .. } => MysqlError::new(1213, "40001", format!("COMMIT failed: serialization conflict: {err}")),
                    err => MysqlError::new(1105, "HY000", format!("COMMIT failed: {err}")),
                }))
            }
            None => {
                // The commit point publishes the schema revision as well.
                self.schema_checkpoint = None;
                Ok(Ok(()))
            }
        }
    }

    /// `ROLLBACK`: undoes the open transaction and restores the catalog checkpoint.
    pub fn rollback(&mut self, runtime: Option<&Arc<MysqlSqlRuntime>>) -> io::Result<()> {
        if let Some(handle) = self.handle.take() {
            handle.rollback();
        }
        self.executor = None;
        self.failed = false;
        self.restore_schema(runtime)
    }

    /// Applies a savepoint verb inside the open transaction. Any failure
    /// aborts the transaction, matching the gateway's aborted-block rule.
    pub fn savepoint(&mut self, intent: SavepointIntent) -> Result<(), MysqlError> {
        let handle = match self.handle.as_ref() {
            Some(handle) => handle.as_ref(),
            None => {
                self.fail();
                return Err(MysqlError::new(1305, "42000", "SAVEPOINT is only valid inside a transaction"));
            }
        };
        let result = match intent {
            SavepointIntent::Create { name } => handle.savepoint_create(&name),
            SavepointIntent::RollbackTo { name } => handle.savepoint_rollback_to(&name),
            SavepointIntent::Release { name } => handle.savepoint_release(&name),
        };
        match result {
            Ok(()) => Ok(()),
            Err(ClusterError::Unsupported(msg)) if msg.contains("does not exist") || msg.contains("already exists") => {
                self.fail();
                Err(MysqlError::new(1305, "42000", msg))
            }
            Err(err) => {
                self.fail();
                Err(MysqlError::new(1105, "HY000", format!("savepoint failed: {err}")))
            }
        }
    }

    /// `SET autocommit`: switching back on commits whatever transaction is
    /// open, including the handle this session opened implicitly. A failed
    /// commit restores the catalog and leaves autocommit enabled, like the
    /// gateway has always reported.
    pub fn set_autocommit(&mut self, runtime: Option<&Arc<MysqlSqlRuntime>>, autocommit: bool) -> io::Result<Result<(), MysqlError>> {
        if autocommit && !self.autocommit {
            if let Some(handle) = self.handle.take() {
                let outcome = handle.commit();
                self.executor = None;
                if let Err(err) = outcome {
                    self.restore_schema(runtime)?;
                    self.autocommit = true;
                    return Ok(Err(MysqlError::new(1105, "HY000", format!("COMMIT failed: {err}"))));
                }
                self.schema_checkpoint = None;
            }
        }
        self.autocommit = autocommit;
        Ok(Ok(()))
    }

    /// Drops the session without publishing anything: an open transaction is
    /// rolled back and the catalog checkpoint restored.
    pub fn abandon(&mut self, runtime: Option<&Arc<MysqlSqlRuntime>>) {
        if let Some(handle) = self.handle.take() {
            handle.rollback();
        }
        self.executor = None;
        self.failed = false;
        let _ = self.restore_schema(runtime);
    }

    /// `COM_RESET_CONNECTION` session half: the session must look like a
    /// brand-new connection. Any open explicit transaction is rolled back —
    /// never committed — because the gateway keeps one commit point per
    /// command, autocommit returns to its default ON, the aborted flag clears
    /// and the plan caches drop. The gateway additionally drops every
    /// prepared statement handle (real MySQL also invalidates them on reset).
    pub fn reset_connection(&mut self, runtime: Option<&Arc<MysqlSqlRuntime>>) -> io::Result<()> {
        if let Some(handle) = self.handle.take() {
            handle.rollback();
        }
        self.executor = None;
        self.failed = false;
        self.autocommit = true;
        self.variables.clear();
        self.schema.clear();
        self.select_cache.clear();
        // Resetting a connection discards an open transaction, catalog changes included.
        self.restore_schema(runtime)
    }

    /// Parse a session-variable value (`ON` / `OFF` / `1` / `0` / `TRUE` / `FALSE`)
    /// into a boolean. Treats anything not explicitly falsy as true (MySQL default).
    pub(crate) fn parse_bool_value(s: &str) -> bool {
        matches!(s.to_ascii_lowercase().as_str(), "on" | "1" | "true" | "yes")
    }

    /// MySQL server-status flags for the current OK / EOF response.
    /// `SERVER_STATUS_IN_TRANS` when an explicit transaction is open;
    /// `SERVER_STATUS_AUTOCOMMIT` when autocommit is on and no explicit handle;
    /// 0 when autocommit is off and no explicit handle.
    pub fn status_flags(&self) -> u16 {
        if self.handle.is_some() {
            SERVER_STATUS_IN_TRANS
        }
        else if self.autocommit {
            SERVER_STATUS_AUTOCOMMIT
        }
        else {
            0
        }
    }
}

impl Default for TransactionSession {
    fn default() -> Self {
        Self::new()
    }
}
