//! PostgreSQL gateway behaviour that `database.execution::pgsql` owns: the
//! dispatch order, session state and the SQLSTATE each failure reports.
//!
//! Parsing and planning stay in `database.query`, so these cases drive the same
//! `dispatch_statement` entry the wire session and the local engine use.

use std::{collections::HashMap, fs, path::PathBuf};

use database_cluster::topology::ShardMap;
use database_execution::{
    cluster::LocalExecutor,
    pgsql::{ExecutionOutcome, PgsqlSqlRuntime, StatementOutcome, TransactionSession, dispatch_statement},
};
use database_query::{SqlSchemaRegistry, SqlValue};
use database_storage::cluster_kv::FileShard;
use database_types::cluster::{ShardEpoch, ShardId};

struct Gateway {
    runtime: std::sync::Arc<PgsqlSqlRuntime>,
    session: TransactionSession,
    directory: PathBuf,
}

impl Gateway {
    fn new(name: &str) -> Self {
        let directory = std::env::temp_dir().join(format!("database-execution-pgsql-{name}-{}", std::process::id()));
        let _ = fs::remove_file(directory.join("local-0.yykv"));
        fs::create_dir_all(&directory).unwrap();
        let shard = ShardId("local-0".into());
        let executor = LocalExecutor::new(
            ShardMap::new(ShardEpoch(1), vec![shard.clone()]).unwrap(),
            HashMap::from([(shard, FileShard::open(directory.join("local-0.yykv")).unwrap())]),
            Vec::new(),
        )
        .unwrap();
        let runtime =
            std::sync::Arc::new(PgsqlSqlRuntime { schema: std::sync::Arc::new(SqlSchemaRegistry::new_in_memory()), executor: std::sync::Arc::new(executor) });
        Self { runtime, session: TransactionSession::new(), directory }
    }

    fn sql(&mut self, text: &str) -> Result<StatementOutcome, (String, String)> {
        dispatch_statement(text, Some(&self.runtime), &mut self.session).map_err(|error| (error.sqlstate, error.message))
    }

    fn expect(&mut self, text: &str) -> ExecutionOutcome {
        match self.sql(text) {
            Ok(StatementOutcome::Result(outcome)) => outcome,
            Ok(StatementOutcome::Command(tag)) => panic!("`{text}` returned command tag `{tag}`, expected rows"),
            Ok(StatementOutcome::Session(_)) => panic!("`{text}` returned a session command, expected rows"),
            Err((code, message)) => panic!("`{text}` failed with {code}: {message}"),
        }
    }

    fn command(&mut self, text: &str) -> String {
        match self.sql(text) {
            Ok(StatementOutcome::Command(tag)) => tag.to_string(),
            Ok(StatementOutcome::Result(outcome)) => outcome.command_tag,
            Ok(StatementOutcome::Session(_)) => panic!("`{text}` returned a session command, expected a command tag"),
            Err((code, message)) => panic!("`{text}` failed with {code}: {message}"),
        }
    }

    fn scalar(&mut self, text: &str) -> SqlValue {
        let outcome = self.expect(text);
        assert_eq!(outcome.rows.len(), 1, "`{text}` must return exactly one row");
        outcome.rows[0][0].clone()
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.directory.join("local-0.yykv"));
        let _ = fs::remove_dir(&self.directory);
    }
}

#[test]
fn double_colon_casts_lower_onto_the_shared_cast_node() {
    let mut gateway = Gateway::new("casts");
    assert_eq!(gateway.scalar("SELECT '1'::int8 + 1"), SqlValue::Integer(2));
    assert_eq!(gateway.scalar("SELECT 7::text"), SqlValue::String("7".to_owned()));
    assert_eq!(gateway.scalar("SELECT ('4')::bigint::text"), SqlValue::String("4".to_owned()));
    gateway.command("CREATE TABLE casted (id BIGINT PRIMARY KEY, body TEXT)");
    gateway.command("INSERT INTO casted (id, body) VALUES (1, '{\"a\":1}')");
    assert_eq!(gateway.scalar("SELECT body::text FROM casted WHERE id = 1"), SqlValue::String("{\"a\":1}".to_owned()));
    assert_eq!(gateway.scalar("SELECT body::text FROM casted WHERE id = 1"), SqlValue::String("{\"a\":1}".to_owned()));
    // A binary target keeps raw octets instead of storing the hex spelling as text.
    assert_eq!(gateway.scalar("SELECT '\\x4142'::bytea"), SqlValue::Bytes(vec![0x41, 0x42]));
}

#[test]
fn an_invalid_cast_value_reports_the_type_input_sqlstate() {
    let mut gateway = Gateway::new("casts-error");
    let (code, message) = match gateway.sql("SELECT 'abc'::int8") {
        Ok(_) => panic!("an invalid integer cast must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "22P02", "unexpected message: {message}");
}

#[test]
fn a_cast_without_a_target_type_reports_a_syntax_error() {
    let mut gateway = Gateway::new("casts-syntax");
    let (code, _) = match gateway.sql("SELECT 1::") {
        Ok(_) => panic!("a cast without a target type must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "42601");
}
