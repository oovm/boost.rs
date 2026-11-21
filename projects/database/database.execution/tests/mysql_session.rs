//! MySQL 会话变量回读的真实执行回归。
//!
//! 用真实执行器（`LocalExecutor` + `FileShard`）驱动 `dispatch_query`，钉死
//! `SET <variable>` → `SELECT @@<variable>` / `SHOW VARIABLES` 的闭环：客户端设置
//! 的会话值必须被存储并在同一连接内回读，而不是永远返回启动默认值。

use std::{collections::HashMap, fs, path::PathBuf, sync::Arc};

use database_cluster::topology::ShardMap;
use database_execution::{
    cluster::{KeyValueExecutor, LocalExecutor},
    mysql::{MysqlSqlRuntime, MysqlStatementSink, TransactionSession, dispatch_query},
    sql_schema::SqlSchemaRegistry,
};
use database_query::{SqlResult, SqlValue};
use database_storage::cluster_kv::FileShard;
use database_types::cluster::{ShardEpoch, ShardId};

#[derive(Debug, Clone, PartialEq)]
enum Reply {
    Ok { affected_rows: u64 },
    Error { code: u16, message: String },
    Rows(SqlResult),
}

/// Captures the single response each statement writes.
#[derive(Default)]
struct Recorder {
    replies: Vec<Reply>,
}

impl MysqlStatementSink for Recorder {
    fn write_ok(&mut self, affected_rows: u64, _last_insert_id: Option<u64>, _status_flags: u16) -> std::io::Result<()> {
        self.replies.push(Reply::Ok { affected_rows });
        Ok(())
    }

    fn write_error(&mut self, code: u16, _state: &str, message: &str) -> std::io::Result<()> {
        self.replies.push(Reply::Error { code, message: message.to_string() });
        Ok(())
    }

    fn write_result_set(
        &mut self,
        result: &SqlResult,
        _column_types: &[u8],
        _column_nullables: &[bool],
        _binary_rows: bool,
        _prefix: Option<(&[u8], usize)>,
        _status_flags: u16,
    ) -> std::io::Result<()> {
        self.replies.push(Reply::Rows(result.clone()));
        Ok(())
    }
}

struct Gateway {
    runtime: Arc<MysqlSqlRuntime>,
    session: TransactionSession,
    directory: PathBuf,
}

impl Gateway {
    fn new(name: &str) -> Self {
        let directory = std::env::temp_dir().join(format!("database-execution-mysql-session-{name}-{}", std::process::id()));
        let _ = fs::remove_file(directory.join("local-0.yykv"));
        fs::create_dir_all(&directory).unwrap();
        let shard = ShardId("local-0".into());
        let executor: Arc<dyn KeyValueExecutor> = Arc::new(
            LocalExecutor::new(
                ShardMap::new(ShardEpoch(1), vec![shard.clone()]).unwrap(),
                HashMap::from([(shard, FileShard::open(directory.join("local-0.yykv")).unwrap())]),
                Vec::new(),
            )
            .unwrap(),
        );
        let runtime = Arc::new(MysqlSqlRuntime { schema: Arc::new(SqlSchemaRegistry::new_in_memory()), executor });
        Self { runtime, session: TransactionSession::new(), directory }
    }

    /// Runs one statement and returns its response.
    fn run(&mut self, sql: &str) -> Reply {
        let mut sink = Recorder::default();
        dispatch_query(&mut sink, sql, Some(&self.runtime), &mut self.session)
            .unwrap_or_else(|error| panic!("`{sql}` failed to write a response: {error}"));
        assert_eq!(sink.replies.len(), 1, "`{sql}` must produce exactly one response");
        sink.replies.remove(0)
    }

    fn expect_rows(&mut self, sql: &str) -> SqlResult {
        match self.run(sql) {
            Reply::Rows(result) => result,
            other => panic!("`{sql}` expected a result set, got {other:?}"),
        }
    }

    fn expect_ok(&mut self, sql: &str) {
        match self.run(sql) {
            Reply::Ok { .. } => {}
            other => panic!("`{sql}` expected OK, got {other:?}"),
        }
    }

    /// Single-cell scalar text of a one-row query.
    fn scalar(&mut self, sql: &str) -> SqlValue {
        let result = self.expect_rows(sql);
        assert_eq!(result.rows.len(), 1, "`{sql}` must return exactly one row");
        result.rows[0][0].clone()
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.directory.join("local-0.yykv"));
        let _ = fs::remove_dir(&self.directory);
    }
}

fn text(value: SqlValue) -> String {
    match value {
        SqlValue::String(text) => text,
        other => panic!("expected a text variable value, got {other:?}"),
    }
}

#[test]
fn set_time_zone_reads_back_the_assigned_value() {
    let mut gateway = Gateway::new("time-zone");
    gateway.expect_ok("SET time_zone = '+08:00'");

    assert_eq!(text(gateway.scalar("SELECT @@session.time_zone")), "+08:00");
    assert_eq!(text(gateway.scalar("SELECT @@time_zone")), "+08:00");

    // `SHOW VARIABLES` reads the same effective value, not the static default.
    let shown = gateway.expect_rows("SHOW VARIABLES LIKE 'time_zone'");
    assert_eq!(shown.rows.len(), 1);
    assert_eq!(shown.rows[0][0], SqlValue::String("time_zone".into()));
    assert_eq!(shown.rows[0][1], SqlValue::String("+08:00".into()));

    // Another assignment replaces the stored value.
    gateway.expect_ok("SET @@session.time_zone = 'UTC'");
    assert_eq!(text(gateway.scalar("SELECT @@session.time_zone")), "UTC");
}

#[test]
fn every_known_variable_keeps_its_session_value() {
    let mut gateway = Gateway::new("variables");
    gateway.expect_ok("SET sql_mode = 'ANSI_QUOTES,STRICT_TRANS_TABLES'");
    gateway.expect_ok("SET wait_timeout = 120");
    gateway.expect_ok("SET max_allowed_packet = 4194304");

    assert_eq!(text(gateway.scalar("SELECT @@sql_mode")), "ANSI_QUOTES,STRICT_TRANS_TABLES");
    assert_eq!(gateway.scalar("SELECT @@wait_timeout"), SqlValue::Integer(120));
    assert_eq!(gateway.scalar("SELECT @@max_allowed_packet"), SqlValue::Integer(4_194_304));

    // Untouched variables still answer their default.
    assert_eq!(text(gateway.scalar("SELECT @@version_comment")), "YYDS MySQL gateway");
}

#[test]
fn reset_connection_drops_session_variable_overrides() {
    let mut gateway = Gateway::new("reset");
    gateway.expect_ok("SET time_zone = '+08:00'");
    assert_eq!(text(gateway.scalar("SELECT @@session.time_zone")), "+08:00");

    gateway.session.reset_connection(Some(&gateway.runtime)).expect("reset connection");
    assert_eq!(text(gateway.scalar("SELECT @@session.time_zone")), "SYSTEM");
}

#[test]
fn unknown_variable_still_reports_1193() {
    let mut gateway = Gateway::new("unknown");
    match gateway.run("SET definitely_not_a_variable = 1") {
        Reply::Error { code, .. } => assert_eq!(code, 1193),
        other => panic!("unknown variable must be rejected, got {other:?}"),
    }
}

#[test]
fn setup_keeps_autocommit_flag_in_charge() {
    let mut gateway = Gateway::new("autocommit");
    // `SET autocommit` keeps its transaction side effects; the readback follows
    // the session flag rather than the stored text table.
    gateway.expect_ok("SET autocommit = 0");
    assert_eq!(text(gateway.scalar("SELECT @@autocommit")), "OFF");
    gateway.expect_ok("SET autocommit = 1");
    assert_eq!(text(gateway.scalar("SELECT @@autocommit")), "ON");
}
