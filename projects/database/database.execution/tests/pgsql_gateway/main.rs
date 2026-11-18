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
        let runtime = std::sync::Arc::new(PgsqlSqlRuntime::new(
            std::sync::Arc::new(SqlSchemaRegistry::new_in_memory()),
            std::sync::Arc::new(executor),
        ));
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
fn generated_identity_and_added_constraints_are_enforced() {
    let mut gateway = Gateway::new("ddl");
    gateway.command("CREATE TABLE identity_rows (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, name TEXT UNIQUE)");
    assert_eq!(gateway.command("INSERT INTO identity_rows (name) VALUES ('a')"), "INSERT 0 1");
    assert_eq!(gateway.command("INSERT INTO identity_rows (name) VALUES ('b')"), "INSERT 0 1");
    assert_eq!(gateway.scalar("SELECT COUNT(*)::int8 FROM identity_rows"), SqlValue::Integer(2));
    assert_eq!(gateway.scalar("SELECT id FROM identity_rows WHERE name = 'b'"), SqlValue::Integer(2));

    // A duplicate on the inline UNIQUE column reports the PostgreSQL class.
    let (code, message) = match gateway.sql("INSERT INTO identity_rows (name) VALUES ('a')") {
        Ok(_) => panic!("a duplicate unique value must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "23505", "unexpected message: {message}");

    gateway.command("CREATE TABLE added (id BIGINT PRIMARY KEY, email TEXT, score BIGINT)");
    gateway.command("ALTER TABLE added ADD CONSTRAINT added_email_key UNIQUE (email)");
    gateway.command("INSERT INTO added (id, email, score) VALUES (1, 'a@example.com', 10)");
    let (code, _) = match gateway.sql("INSERT INTO added (id, email, score) VALUES (2, 'a@example.com', 20)") {
        Ok(_) => panic!("the added UNIQUE constraint must reject a duplicate"),
        Err(error) => error,
    };
    assert_eq!(code, "23505");

    gateway.command("ALTER TABLE added ADD CONSTRAINT added_score_check CHECK (score > 0)");
    let (code, _) = match gateway.sql("INSERT INTO added (id, email, score) VALUES (3, 'b@example.com', -1)") {
        Ok(_) => panic!("the added CHECK constraint must reject a negative score"),
        Err(error) => error,
    };
    assert_eq!(code, "23514");

    gateway.command("ALTER TABLE added ADD CONSTRAINT added_parent_fk FOREIGN KEY (id) REFERENCES added (id)");
    let (code, _) = match gateway.sql("INSERT INTO added (id, email, score) VALUES (9, 'z@example.com', 1)") {
        Ok(_) => panic!("the added foreign key must reject a missing parent"),
        Err(error) => error,
    };
    assert_eq!(code, "23503");
}

#[test]
fn a_stored_generated_column_is_rejected_with_a_capability_error() {
    let mut gateway = Gateway::new("ddl-generated");
    let (code, message) = match gateway.sql("CREATE TABLE stored (id BIGINT PRIMARY KEY, total BIGINT GENERATED ALWAYS AS (id * 2) STORED)") {
        Ok(_) => panic!("stored generated columns are not implemented yet"),
        Err(error) => error,
    };
    assert_eq!(code, "0A000", "unexpected message: {message}");
}

#[test]
fn copy_state_inserts_text_and_csv_rows() {
    use database_execution::pgsql::{CopyInPlan, CopyInState};
    use database_query::CopyFormat;

    let mut gateway = Gateway::new("copy");
    gateway.command("CREATE TABLE copied (id BIGINT PRIMARY KEY, name TEXT)");
    let plan = CopyInPlan {
        table: "copied".to_owned(),
        columns: vec!["id".to_owned(), "name".to_owned()],
        format: CopyFormat::Text,
        delimiter: '\t',
        header: false,
    };
    let mut state = CopyInState::new(plan);
    state.push(b"1\tone\n2\t\\N\n3\ttab\\there\n");
    let inserted = state.finish(&gateway.runtime, &mut gateway.session).expect("text copy");
    assert_eq!(inserted, 3);
    assert_eq!(gateway.scalar("SELECT COUNT(*)::int8 FROM copied"), SqlValue::Integer(3));
    assert_eq!(gateway.scalar("SELECT name FROM copied WHERE id = 3"), SqlValue::String("tab\there".to_owned()));
    assert_eq!(gateway.scalar("SELECT name FROM copied WHERE id = 2"), SqlValue::Null);

    let csv = CopyInPlan {
        table: "copied".to_owned(),
        columns: Vec::new(),
        format: CopyFormat::Csv,
        delimiter: ',',
        header: true,
    };
    let mut state = CopyInState::new(csv);
    state.push(b"id,name\n4,\"a,b\"\n");
    assert_eq!(state.finish(&gateway.runtime, &mut gateway.session).expect("csv copy"), 1);
    assert_eq!(gateway.scalar("SELECT name FROM copied WHERE id = 4"), SqlValue::String("a,b".to_owned()));
}

#[test]
fn copy_errors_report_their_sqlstates() {
    use database_execution::pgsql::{CopyInPlan, CopyInState};
    use database_query::CopyFormat;

    let mut gateway = Gateway::new("copy-errors");
    gateway.command("CREATE TABLE copied (id BIGINT PRIMARY KEY, name TEXT)");
    let plan = CopyInPlan {
        table: "copied".to_owned(),
        columns: vec!["id".to_owned(), "name".to_owned()],
        format: CopyFormat::Text,
        delimiter: '\t',
        header: false,
    };
    let mut state = CopyInState::new(plan.clone());
    state.push(b"1\n");
    let error = state.finish(&gateway.runtime, &mut gateway.session).expect_err("short row must fail");
    assert_eq!(error.sqlstate, "22P04");

    let mut missing = CopyInState::new(CopyInPlan { table: "absent".to_owned(), ..plan });
    missing.push(b"1\tx\n");
    let error = missing.finish(&gateway.runtime, &mut gateway.session).expect_err("unknown table must fail");
    assert_eq!(error.sqlstate, "42P01");
}

#[test]
fn listen_and_notify_queue_messages_for_the_same_session() {
    let mut gateway = Gateway::new("notify");
    assert_eq!(gateway.command("LISTEN events"), "LISTEN");
    assert_eq!(gateway.command("NOTIFY events, 'hello'"), "NOTIFY");
    let pending = gateway.session.take_notifications();
    assert_eq!(
        pending,
        vec![database_execution::pgsql::Notification { process_id: std::process::id(), channel: "events".to_owned(), payload: "hello".to_owned() }]
    );

    assert_eq!(gateway.command("UNLISTEN events"), "UNLISTEN");
    gateway.command("NOTIFY events, 'lost'");
    assert!(gateway.session.take_notifications().is_empty());

    gateway.command("LISTEN audit");
    gateway.command("UNLISTEN *");
    gateway.command("NOTIFY audit, 'lost'");
    assert!(gateway.session.take_notifications().is_empty());
}

#[test]
fn notify_errors_report_their_sqlstates() {
    let mut gateway = Gateway::new("notify-errors");
    let long_channel = "c".repeat(64);
    let (code, _) = match gateway.sql(&format!("LISTEN {long_channel}")) {
        Ok(_) => panic!("oversized channel names must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "42601");

    let long_payload = "p".repeat(8001);
    let (code, _) = match gateway.sql(&format!("NOTIFY events, '{long_payload}'")) {
        Ok(_) => panic!("oversized payloads must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "22001");
}

#[test]
fn explain_reports_the_plan_and_analyze_reports_actual_rows() {
    let mut gateway = Gateway::new("explain");
    gateway.command("CREATE TABLE explained (id BIGINT PRIMARY KEY, name TEXT)");
    gateway.command("INSERT INTO explained (id, name) VALUES (1, 'a'), (2, 'b')");

    let plan = gateway.expect("EXPLAIN SELECT name FROM explained");
    assert_eq!(plan.description[0].name, "QUERY PLAN");
    let lines: Vec<String> = plan.rows.iter().map(|row| match &row[0] {
        SqlValue::String(text) => text.clone(),
        other => panic!("plan line must be text, got {other:?}"),
    }).collect();
    assert!(lines.iter().any(|line| line.contains("explained")), "plan must name the table: {lines:?}");
    assert_eq!(plan.command_tag, "EXPLAIN");

    let analyzed = gateway.expect("EXPLAIN ANALYZE SELECT name FROM explained");
    let root = match &analyzed.rows[0][0] {
        SqlValue::String(text) => text.clone(),
        other => panic!("plan line must be text, got {other:?}"),
    };
    assert!(root.contains("actual rows=2"), "unexpected root line: {root}");

    let (code, _) = match gateway.sql("EXPLAIN CREATE TABLE nope (id INT)") {
        Ok(_) => panic!("EXPLAIN of DDL must be rejected"),
        Err(error) => error,
    };
    assert_eq!(code, "0A000");
}

#[test]
fn cursors_fetch_forward_absolute_and_relative_rows() {
    let mut gateway = Gateway::new("cursors");
    gateway.command("CREATE TABLE cursor_rows (id BIGINT PRIMARY KEY, name TEXT)");
    gateway.command("INSERT INTO cursor_rows (id, name) VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd')");
    assert_eq!(gateway.command("BEGIN"), "BEGIN");
    assert_eq!(gateway.command("DECLARE c NO SCROLL CURSOR FOR SELECT id FROM cursor_rows ORDER BY id"), "DECLARE CURSOR");
    assert_eq!(gateway.expect("FETCH NEXT FROM c").rows, vec![vec![SqlValue::Integer(1)]]);
    assert_eq!(gateway.expect("FETCH 2 FROM c").rows, vec![vec![SqlValue::Integer(2)], vec![SqlValue::Integer(3)]]);
    assert_eq!(gateway.expect("FETCH ABSOLUTE 1 FROM c").rows, vec![vec![SqlValue::Integer(1)]]);
    assert_eq!(gateway.expect("FETCH RELATIVE 2 FROM c").rows, vec![vec![SqlValue::Integer(4)]]);
    assert_eq!(gateway.expect("FETCH ALL FROM c").rows.len(), 0);
    assert_eq!(gateway.expect("FETCH ABSOLUTE -1 FROM c").rows, vec![vec![SqlValue::Integer(4)]]);
    assert_eq!(gateway.command("CLOSE c"), "CLOSE CURSOR");
    let (code, _) = match gateway.sql("FETCH ALL FROM c") {
        Ok(_) => panic!("a closed cursor must not be fetchable"),
        Err(error) => error,
    };
    assert_eq!(code, "34000");
    assert_eq!(gateway.command("COMMIT"), "COMMIT");
}

#[test]
fn cursor_errors_report_their_sqlstates() {
    let mut gateway = Gateway::new("cursor-errors");
    let (code, message) = match gateway.sql("DECLARE c CURSOR FOR SELECT 1") {
        Ok(_) => panic!("DECLARE outside a transaction block must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "25P01", "unexpected message: {message}");

    gateway.command("BEGIN");
    gateway.command("DECLARE c CURSOR FOR SELECT 1");
    let (code, _) = match gateway.sql("DECLARE c CURSOR FOR SELECT 2") {
        Ok(_) => panic!("a duplicate cursor name must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "42P03");

    let (code, _) = match gateway.sql("FETCH ALL FROM missing") {
        Ok(_) => panic!("FETCH of an unknown cursor must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "34000");
    gateway.command("COMMIT");
}

#[test]
fn a_holdable_cursor_survives_commit_and_a_plain_one_does_not() {
    let mut gateway = Gateway::new("cursor-hold");
    gateway.command("CREATE TABLE hold_rows (id BIGINT PRIMARY KEY)");
    gateway.command("INSERT INTO hold_rows (id) VALUES (1), (2)");
    gateway.command("BEGIN");
    gateway.command("DECLARE held CURSOR WITH HOLD FOR SELECT id FROM hold_rows ORDER BY id");
    gateway.command("DECLARE plain CURSOR FOR SELECT id FROM hold_rows ORDER BY id");
    assert_eq!(gateway.command("COMMIT"), "COMMIT");
    assert_eq!(gateway.expect("FETCH ALL FROM held").rows.len(), 2);
    let (code, _) = match gateway.sql("FETCH ALL FROM plain") {
        Ok(_) => panic!("a cursor without HOLD closes at COMMIT"),
        Err(error) => error,
    };
    assert_eq!(code, "34000");
}

#[test]
fn prepared_statements_execute_with_arguments_and_returning() {
    let mut gateway = Gateway::new("prepare");
    gateway.command("CREATE TABLE prepared (id BIGINT PRIMARY KEY, name TEXT)");
    assert_eq!(gateway.command("PREPARE add (bigint, text) AS INSERT INTO prepared (id, name) VALUES ($1, $2)"), "PREPARE");
    assert_eq!(gateway.command("EXECUTE add (1, 'one')"), "INSERT 0 1");
    assert_eq!(gateway.command("EXECUTE add (2, 'two')"), "INSERT 0 1");
    assert_eq!(gateway.scalar("SELECT COUNT(*)::int8 FROM prepared"), SqlValue::Integer(2));
    assert_eq!(gateway.command("PREPARE pick (bigint) AS SELECT name FROM prepared WHERE id = $1"), "PREPARE");
    let picked = gateway.expect("EXECUTE pick (2)");
    assert_eq!(picked.rows, vec![vec![SqlValue::String("two".to_owned())]]);
    assert_eq!(gateway.command("DEALLOCATE pick"), "DEALLOCATE");
    assert_eq!(gateway.command("DEALLOCATE ALL"), "DEALLOCATE ALL");
}

#[test]
fn prepared_statement_failures_report_their_sqlstates() {
    let mut gateway = Gateway::new("prepare-errors");
    gateway.command("PREPARE p AS SELECT 1");
    let (code, message) = match gateway.sql("PREPARE p AS SELECT 2") {
        Ok(_) => panic!("duplicate PREPARE must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "42P05", "unexpected message: {message}");

    let (code, _) = match gateway.sql("EXECUTE missing") {
        Ok(_) => panic!("EXECUTE of an unknown statement must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "26000");

    gateway.command("PREPARE two (int, int) AS SELECT $1");
    let (code, _) = match gateway.sql("EXECUTE two (1)") {
        Ok(_) => panic!("a wrong argument count must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "42601");

    let (code, _) = match gateway.sql("DEALLOCATE missing") {
        Ok(_) => panic!("DEALLOCATE of an unknown statement must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "26000");
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
