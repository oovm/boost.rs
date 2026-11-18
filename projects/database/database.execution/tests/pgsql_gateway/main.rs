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
fn databases_roles_and_grants_are_cataloged_and_enforced() {
    let mut gateway = Gateway::new("objects");
    gateway.command("CREATE TABLE orders (id BIGINT PRIMARY KEY, total BIGINT)");
    assert_eq!(gateway.command("CREATE DATABASE app"), "CREATE DATABASE");
    assert_eq!(gateway.command("CREATE ROLE reader WITH LOGIN"), "CREATE ROLE");
    assert_eq!(gateway.command("GRANT SELECT, INSERT ON orders TO reader"), "GRANT");
    gateway.command("INSERT INTO orders (id, total) VALUES (1, 10)");

    gateway.session.set_user("reader".to_owned());
    assert_eq!(gateway.scalar("SELECT total FROM orders WHERE id = 1"), SqlValue::Integer(10));
    gateway.command("INSERT INTO orders (id, total) VALUES (2, 20)");
    let (code, message) = match gateway.sql("DELETE FROM orders WHERE id = 1") {
        Ok(_) => panic!("a role without DELETE must be refused"),
        Err(error) => error,
    };
    assert_eq!(code, "42501", "unexpected message: {message}");
    let (code, _) = match gateway.sql("SELECT o1.id FROM orders o1 JOIN orders o2 ON o1.id = o2.id") {
        Ok(_) => panic!("a shape the check cannot classify must be refused for a restricted role"),
        Err(error) => error,
    };
    assert_eq!(code, "0A000");

    gateway.session.set_user("postgres".to_owned());
    gateway.command("REVOKE SELECT ON orders FROM reader");
    gateway.session.set_user("reader".to_owned());
    let (code, _) = match gateway.sql("SELECT id FROM orders") {
        Ok(_) => panic!("a revoked SELECT must be refused"),
        Err(error) => error,
    };
    assert_eq!(code, "42501");
}

#[test]
fn object_statement_errors_report_their_sqlstates() {
    let mut gateway = Gateway::new("objects-errors");
    gateway.command("CREATE TABLE orders (id BIGINT PRIMARY KEY)");
    let (code, message) = match gateway.sql("CREATE DATABASE postgres") {
        Ok(_) => panic!("the bootstrap database already exists"),
        Err(error) => error,
    };
    assert_eq!(code, "42P04", "unexpected message: {message}");
    let (code, _) = match gateway.sql("CREATE ROLE postgres") {
        Ok(_) => panic!("the bootstrap role already exists"),
        Err(error) => error,
    };
    assert_eq!(code, "42710");
    gateway.command("CREATE ROLE reader");
    let (code, _) = match gateway.sql("GRANT SELECT ON orders TO ghost") {
        Ok(_) => panic!("a grant to an unknown role must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "42704");
    let (code, _) = match gateway.sql("GRANT SELECT ON missing TO reader") {
        Ok(_) => panic!("a grant on an unknown table must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "42P01");

    // Unsupported clauses name their reason instead of being silently accepted.
    for statement in [
        "CREATE DATABASE other TEMPLATE template0",
        "CREATE SEQUENCE s",
        "GRANT ALL TABLES IN SCHEMA public TO reader",
    ] {
        let (code, message) = match gateway.sql(statement) {
            Ok(_) => panic!("`{statement}` must be rejected"),
            Err(error) => error,
        };
        assert_eq!(code, "0A000", "unexpected message for `{statement}`: {message}");
    }
}

#[test]
fn role_passwords_are_cataloged_as_verifiers() {
    let mut gateway = Gateway::new("role-password");
    assert_eq!(gateway.command("CREATE ROLE app WITH LOGIN PASSWORD 'r0le'"), "CREATE ROLE");
    let role = gateway.runtime.objects.role("app").expect("role is cataloged");
    let password = role.password.clone().expect("the role keeps a verifier");
    assert!(password.verifies_cleartext("r0le", "app"));
    assert!(!password.verifies_cleartext("wrong", "app"));
    assert!(!serde_json::to_string(&role).unwrap().contains("r0le"), "the document must not carry the plaintext");

    // A role created without `PASSWORD` never authenticates.
    gateway.command("CREATE ROLE quiet");
    assert!(gateway.runtime.objects.role("quiet").expect("role is cataloged").password.is_none());

    // `PASSWORD` outside a literal is a syntax error, not a silent ignore.
    let (code, message) = match gateway.sql("CREATE ROLE broken PASSWORD hidden") {
        Ok(_) => panic!("an unquoted password must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "42601", "unexpected message: {message}");
}

#[test]
fn statement_timeout_reports_57014_and_rolls_the_block_back() {
    let mut gateway = Gateway::new("timeout");
    gateway.command("CREATE TABLE bulk (id BIGINT PRIMARY KEY)");
    gateway.command("BEGIN");
    assert_eq!(gateway.command("SET statement_timeout = 1"), "SET");
    let values: Vec<String> = (0..20_000).map(|id| format!("({id})")).collect();
    let (code, message) = match gateway.sql(&format!("INSERT INTO bulk (id) VALUES {}", values.join(","))) {
        Ok(_) => panic!("a statement above the deadline must report `57014`"),
        Err(error) => error,
    };
    assert_eq!(code, "57014", "unexpected message: {message}");
    // The failed statement aborts the block, and the rollback discards its rows.
    let (code, _) = match gateway.sql("SELECT COUNT(*)::int8 FROM bulk") {
        Ok(_) => panic!("an aborted block rejects later statements"),
        Err(error) => error,
    };
    assert_eq!(code, "25P02");
    assert_eq!(gateway.command("ROLLBACK"), "ROLLBACK");
    assert_eq!(gateway.scalar("SELECT COUNT(*)::int8 FROM bulk"), SqlValue::Integer(0));

    // Outside a block the session survives the timeout.
    assert_eq!(gateway.command("SET statement_timeout = '1ms'"), "SET");
    assert_eq!(gateway.command("SET statement_timeout = 0"), "SET");
    assert_eq!(gateway.scalar("SELECT 1::int8"), SqlValue::Integer(1));
}

#[test]
fn an_invalid_statement_timeout_value_reports_22023() {
    let mut gateway = Gateway::new("timeout-errors");
    let (code, message) = match gateway.sql("SET statement_timeout = 'soon'") {
        Ok(_) => panic!("a non-numeric timeout must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "22023", "unexpected message: {message}");
}

#[test]
fn escape_string_literals_decode_backslash_sequences() {
    let mut gateway = Gateway::new("estring");
    assert_eq!(gateway.scalar("SELECT E'a\\nb'"), SqlValue::String("a\nb".to_owned()));
    assert_eq!(gateway.scalar("SELECT E'col\\there'"), SqlValue::String("col\there".to_owned()));
    assert_eq!(gateway.scalar("SELECT E'\\x41\\x42'"), SqlValue::String("AB".to_owned()));
    assert_eq!(gateway.scalar("SELECT E'it\\'s'"), SqlValue::String("it's".to_owned()));
    assert_eq!(gateway.scalar("SELECT E'back\\\\slash'"), SqlValue::String("back\\slash".to_owned()));
    // A standard literal keeps its backslash: `standard_conforming_strings` is on.
    assert_eq!(gateway.scalar("SELECT 'a\\nb'"), SqlValue::String("a\\nb".to_owned()));
}

#[test]
fn an_unterminated_escape_literal_reports_a_syntax_error() {
    let mut gateway = Gateway::new("estring-errors");
    let (code, message) = match gateway.sql("SELECT E'abc") {
        Ok(_) => panic!("an unterminated escape literal must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "42601", "unexpected message: {message}");
}

#[test]
fn stored_generated_columns_are_computed_on_every_write() {
    let mut gateway = Gateway::new("generated");
    gateway.command(
        "CREATE TABLE stored (id BIGINT PRIMARY KEY, price BIGINT, total BIGINT GENERATED ALWAYS AS (price * 2) STORED)",
    );
    gateway.command("INSERT INTO stored (id, price) VALUES (1, 21)");
    assert_eq!(gateway.scalar("SELECT total FROM stored WHERE id = 1"), SqlValue::Integer(42));
    gateway.command("UPDATE stored SET price = 50 WHERE id = 1");
    assert_eq!(gateway.scalar("SELECT total FROM stored WHERE id = 1"), SqlValue::Integer(100));
    // Multi-row INSERT and a scan UPDATE go through the same computation.
    gateway.command("INSERT INTO stored (id, price) VALUES (2, 1), (3, 2)");
    assert_eq!(gateway.scalar("SELECT SUM(total)::int8 FROM stored"), SqlValue::Integer(106));
    gateway.command("UPDATE stored SET price = price + 1");
    assert_eq!(gateway.scalar("SELECT SUM(total)::int8 FROM stored"), SqlValue::Integer(112));

    // `ALTER TABLE ... ADD COLUMN ... GENERATED` backfills existing rows.
    gateway.command("ALTER TABLE stored ADD COLUMN doubled BIGINT GENERATED ALWAYS AS (price * 4) STORED");
    assert_eq!(gateway.scalar("SELECT doubled FROM stored WHERE id = 1"), SqlValue::Integer(204));
    assert_eq!(gateway.scalar("SELECT SUM(doubled)::int8 FROM stored"), SqlValue::Integer(224));
}

#[test]
fn writing_a_generated_column_reports_the_generated_column_sqlstate() {
    let mut gateway = Gateway::new("generated-errors");
    gateway.command("CREATE TABLE stored (id BIGINT PRIMARY KEY, price BIGINT, total BIGINT GENERATED ALWAYS AS (price * 2) STORED)");
    let (code, message) = match gateway.sql("INSERT INTO stored (id, price, total) VALUES (1, 10, 0)") {
        Ok(_) => panic!("an INSERT naming a generated column must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "428C9", "unexpected message: {message}");

    gateway.command("INSERT INTO stored (id, price) VALUES (1, 10)");
    let (code, _) = match gateway.sql("UPDATE stored SET total = 1 WHERE id = 1") {
        Ok(_) => panic!("an UPDATE assigning a generated column must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "428C9");

    let (code, _) = match gateway.sql(
        "CREATE TABLE broken (id BIGINT PRIMARY KEY, total BIGINT GENERATED ALWAYS AS (id * 2))",
    ) {
        Ok(_) => panic!("a generated column without `STORED` must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "42601");
}

#[test]
fn temporary_tables_are_session_local_and_stay_out_of_the_persistent_schema() {
    let mut gateway = Gateway::new("temp");
    gateway.command("CREATE TABLE public_rows (id BIGINT PRIMARY KEY, v TEXT)");
    gateway.command("INSERT INTO public_rows (id, v) VALUES (1, 'public')");
    assert_eq!(gateway.command("CREATE TEMP TABLE staged (id BIGINT PRIMARY KEY, v TEXT)"), "CREATE TABLE");
    gateway.command("INSERT INTO staged (id, v) VALUES (1, 'staged')");
    assert_eq!(gateway.scalar("SELECT COUNT(*)::int8 FROM staged"), SqlValue::Integer(1));
    assert_eq!(gateway.scalar("SELECT v FROM pg_temp.staged WHERE id = 1"), SqlValue::String("staged".to_owned()));
    // The persisted registry never learns about the temporary table.
    assert!(gateway.runtime.schema.catalog().tables().iter().all(|table| table.name != "staged"));

    // A second connection over the same runtime cannot see it.
    let mut other = TransactionSession::new();
    let (code, _) = match dispatch_statement("SELECT id FROM staged", Some(&gateway.runtime), &mut other) {
        Ok(_) => panic!("a temporary table must not leak into another session"),
        Err(error) => (error.sqlstate, error.message),
    };
    assert_eq!(code, "42P01");

    // A temporary table shadows a persistent table of the same name: the
    // unqualified lookup resolves `pg_temp` before `public`.
    gateway.command("CREATE TEMP TABLE public_rows (id BIGINT PRIMARY KEY, v TEXT)");
    assert_eq!(gateway.scalar("SELECT COUNT(*)::int8 FROM public_rows"), SqlValue::Integer(0));
    gateway.command("DROP TABLE public_rows");
    assert_eq!(gateway.scalar("SELECT COUNT(*)::int8 FROM public_rows"), SqlValue::Integer(1));
}

#[test]
fn on_commit_drop_removes_the_temporary_table_and_delete_rows_is_rejected() {
    let mut gateway = Gateway::new("temp-on-commit");
    gateway.command("BEGIN");
    gateway.command("CREATE TEMP TABLE scratch (id BIGINT PRIMARY KEY) ON COMMIT DROP");
    gateway.command("INSERT INTO scratch (id) VALUES (1)");
    assert_eq!(gateway.command("COMMIT"), "COMMIT");
    let (code, _) = match gateway.sql("SELECT id FROM scratch") {
        Ok(_) => panic!("`ON COMMIT DROP` must remove the table at the commit point"),
        Err(error) => error,
    };
    assert_eq!(code, "42P01");

    let (code, message) = match gateway.sql("CREATE TEMP TABLE kept (id BIGINT PRIMARY KEY) ON COMMIT DELETE ROWS") {
        Ok(_) => panic!("`ON COMMIT DELETE ROWS` must be rejected explicitly"),
        Err(error) => error,
    };
    assert_eq!(code, "0A000", "unexpected message: {message}");

    let (code, _) = match gateway.sql("CREATE TABLE persistent (id BIGINT PRIMARY KEY) ON COMMIT DROP") {
        Ok(_) => panic!("`ON COMMIT` on a persistent table must fail"),
        Err(error) => error,
    };
    assert_eq!(code, "42601");
}

#[test]
fn a_temporary_table_created_in_a_rolled_back_block_disappears() {
    let mut gateway = Gateway::new("temp-rollback");
    gateway.command("BEGIN");
    gateway.command("CREATE TEMP TABLE aborted (id BIGINT PRIMARY KEY)");
    assert_eq!(gateway.command("ROLLBACK"), "ROLLBACK");
    let (code, _) = match gateway.sql("SELECT id FROM aborted") {
        Ok(_) => panic!("a temporary table created in a rolled-back block must vanish"),
        Err(error) => error,
    };
    assert_eq!(code, "42P01");
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
