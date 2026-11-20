//! Basic smoke for the PostgreSQL facade: local DDL/DML/SELECT/RETURNING,
//! transaction commit and rollback, and one protocol-v3 roundtrip through
//! `listen`. Detailed conformance lives downstream, not here.

use std::{fs, net::SocketAddr, path::PathBuf};

use postgres::{ClientConfig, Error, PostgreSQL, PostgreSQLClient, PostgreSQLOptions, PostgreSQLStorage, QueryResult, SqlValue};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("postgres-facade-{name}-{}-{}", std::process::id(), uuid()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn options(&self, database: &str) -> PostgreSQLOptions {
        PostgreSQLOptions { storage: PostgreSQLStorage { directory: self.0.clone() }, database: database.to_owned() }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn uuid() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}

fn rows_of(result: &QueryResult) -> Vec<Vec<String>> {
    result.rows.iter().map(|row| row.iter().map(cell).collect()).collect()
}

fn cell(value: &SqlValue) -> String {
    match value {
        SqlValue::Integer(value) => value.to_string(),
        SqlValue::String(value) => value.clone(),
        other => format!("{other:?}"),
    }
}

#[test]
fn zzz_probe_shapes() {
    let dir = TempDir::new("zzz-probe");
    let engine = PostgreSQL::open(dir.options("smoke")).unwrap();
    engine
        .execute(
            r#"CREATE TABLE probe_users (
                id UUID PRIMARY KEY,
                email TEXT NOT NULL UNIQUE,
                status SMALLINT NOT NULL DEFAULT 0,
                balance NUMERIC(18, 6) NOT NULL DEFAULT 0,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )"#,
        )
        .unwrap();
    for index in 0..3 {
        engine
            .execute(&format!(
                "INSERT INTO probe_users (id, email, status, created_at) VALUES ('00000000-0000-0000-0000-00000000000{index}', 'probe-{index}@example.com', {index}, '2026-11-20 00:00:00+00')"
            ))
            .unwrap();
    }

    for (name, sql) in [
        ("window", "SELECT email FROM probe_users"),
        (
            "on_conflict",
            "INSERT INTO probe_users (id, email, balance) VALUES ('00000000-0000-0000-0000-000000000001', 'probe-0@example.com', 0) ON CONFLICT (id) DO UPDATE SET balance = probe_users.balance + 1 RETURNING balance",
        ),
        (
            "on_conflict_unique",
            "INSERT INTO probe_users (id, email, balance) VALUES ('00000000-0000-0000-0000-000000000009', 'probe-0@example.com', 0) ON CONFLICT (email) DO UPDATE SET balance = probe_users.balance + 1 RETURNING id, balance",
        ),
        ("lateral", "SELECT u.email, t.total FROM probe_users u, LATERAL (SELECT count(*) AS total FROM probe_users x) t"),
        ("derived_comma", "SELECT u.email, t.total FROM probe_users u, (SELECT count(*) AS total FROM probe_users x) t"),
        ("derived_join", "SELECT u.email, t.total FROM probe_users u CROSS JOIN (SELECT count(*) AS total FROM probe_users x) t"),
        ("array_agg", "SELECT array_agg(id) AS ids FROM probe_users"),
        ("cte", "WITH picked AS (SELECT id FROM probe_users) SELECT count(*) FROM picked"),
    ] {
        match engine.query(sql) {
            Ok(result) => println!("PROBE {name}: ok rows={:?}", rows_of(&result)),
            Err(error) => println!("PROBE {name}: FAILED {error}"),
        }
    }
    engine.close().unwrap();
}

#[test]
fn gap71_probe() {
    let dir = TempDir::new("gap71");
    let engine = PostgreSQL::open(dir.options("smoke")).unwrap();
    engine.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, ts TIMESTAMPTZ, meta JSONB, name TEXT)").unwrap();
    engine
        .execute(
            r#"INSERT INTO t (id, ts, meta, name) VALUES (1, '2026-11-20 13:45:30+00', '{"a": {"b": 7}, "list": [10, 20]}', 'alpha')"#,
        )
        .unwrap();
    for (name, sql) in [
        ("extract_year", "SELECT EXTRACT(YEAR FROM ts) FROM t"),
        ("extract_epoch", "SELECT EXTRACT(EPOCH FROM ts) FROM t"),
        ("extract_dow", "SELECT EXTRACT(DOW FROM ts) FROM t"),
        ("interval_add", "SELECT ts + INTERVAL '1 day' FROM t"),
        ("interval_sub", "SELECT ts - INTERVAL '2 hours' FROM t"),
        ("interval_cmp", "SELECT id FROM t WHERE ts > now() - INTERVAL '30 days'"),
        ("json_arrow", r#"SELECT meta -> 'a' FROM t"#),
        ("json_arrow_text", r#"SELECT meta ->> 'a' FROM t"#),
        ("json_index", r#"SELECT meta -> 'list' -> 1 FROM t"#),
        ("json_nested", r#"SELECT meta -> 'a' ->> 'b' FROM t"#),
        ("recursive", "WITH RECURSIVE nums(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM nums WHERE n < 5) SELECT n FROM nums ORDER BY n"),
        ("delete_alias", "DELETE FROM t AS x WHERE x.name = 'alpha'"),
    ] {
        match engine.query(sql) {
            Ok(result) => println!("PROBE {name}: ok rows={:?}", rows_of(&result)),
            Err(error) => println!("PROBE {name}: FAILED {error}"),
        }
    }
    engine.close().unwrap();
}

#[test]
fn single_row_derived_tables_and_unique_conflict_targets() {
    let dir = TempDir::new("derived-unique");
    let engine = PostgreSQL::open(dir.options("smoke")).unwrap();
    engine.execute("CREATE TABLE u (id UUID PRIMARY KEY, email TEXT NOT NULL UNIQUE, balance NUMERIC(18, 6) NOT NULL DEFAULT 0)").unwrap();
    for index in 0..3 {
        engine
            .execute(&format!("INSERT INTO u (id, email) VALUES ('00000000-0000-0000-0000-00000000000{index}', 'u-{index}@example.com')"))
            .unwrap();
    }

    // A single-row aggregate derived table is a scalar subquery, whether it is
    // reached through `, LATERAL`, `, (...)` or `CROSS JOIN (...)`.
    for sql in [
        "SELECT b.email, t.total FROM u b, LATERAL (SELECT count(*) AS total FROM u x) t ORDER BY b.email",
        "SELECT b.email, t.total FROM u b, (SELECT count(*) AS total FROM u x) t ORDER BY b.email",
        "SELECT b.email, t.total FROM u b CROSS JOIN (SELECT count(*) AS total FROM u x) t ORDER BY b.email",
    ] {
        let rows = engine.query(sql).unwrap_or_else(|error| panic!("`{sql}` failed: {error}"));
        assert_eq!(
            rows_of(&rows),
            vec![
                vec!["u-0@example.com".to_owned(), "3".to_owned()],
                vec!["u-1@example.com".to_owned(), "3".to_owned()],
                vec!["u-2@example.com".to_owned(), "3".to_owned()],
            ],
            "`{sql}`"
        );
    }

    // The conflict target may be a declared unique column, not only the primary
    // key; the stored row keeps its own primary key.
    let rows = engine
        .query(
            "INSERT INTO u (id, email, balance) VALUES ('00000000-0000-0000-0000-000000000009', 'u-0@example.com', 0) \
             ON CONFLICT (email) DO UPDATE SET balance = u.balance + 1 RETURNING id, balance",
        )
        .unwrap();
    assert_eq!(rows_of(&rows), vec![vec!["00000000-0000-0000-0000-000000000000".to_owned(), "1".to_owned()]]);

    // A target no unique constraint covers is still refused.
    let rejected = engine.query(
        "INSERT INTO u (id, email) VALUES ('00000000-0000-0000-0000-00000000000a', 'u-9@example.com') ON CONFLICT (email, id) DO NOTHING",
    );
    assert!(rejected.is_err(), "a target spanning no declared constraint must be rejected");

    engine.close().unwrap();
}

#[test]
fn aggregate_filter_restricts_input_rows() {
    let dir = TempDir::new("agg-filter");
    let engine = PostgreSQL::open(dir.options("smoke")).unwrap();
    engine.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, status BIGINT, amount BIGINT)").unwrap();
    engine.execute("INSERT INTO t (id, status, amount) VALUES (1, 1, 10), (2, 0, 20), (3, 1, 30), (4, NULL, NULL)").unwrap();
    let row = engine.query("SELECT COUNT(*) FILTER (WHERE status = 1), SUM(amount) FILTER (WHERE status = 1) FROM t").unwrap();
    assert_eq!(rows_of(&row), vec![vec!["2".to_owned(), "40".to_owned()]]);
    let grouped = engine.query("SELECT status, COUNT(*) FILTER (WHERE amount >= 20) FROM t GROUP BY status ORDER BY status").unwrap();
    assert_eq!(rows_of(&grouped), vec![vec!["0", "1"], vec!["1", "1"], vec!["Null", "0"]]);
    engine.close().unwrap();
}

#[test]
fn for_update_is_accepted_without_row_locks() {
    let dir = TempDir::new("for-update");
    let engine = PostgreSQL::open(dir.options("smoke")).unwrap();
    engine.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)").unwrap();
    engine.execute("INSERT INTO t (id, name) VALUES (1, 'a')").unwrap();
    for clause in ["FOR UPDATE", "FOR UPDATE OF t", "FOR UPDATE NOWAIT", "FOR SHARE SKIP LOCKED", "FOR NO KEY UPDATE"] {
        let rows = engine.query(&format!("SELECT id FROM t WHERE id = 1 {clause}")).unwrap();
        assert_eq!(rows_of(&rows), vec![vec!["1".to_owned()]], "clause: {clause}");
    }
    engine.close().unwrap();
}

#[test]
fn ilike_matches_case_insensitively() {
    let dir = TempDir::new("ilike");
    let engine = PostgreSQL::open(dir.options("smoke")).unwrap();
    engine.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)").unwrap();
    engine.execute("INSERT INTO t (id, name) VALUES (1, 'Alpha'), (2, 'beta'), (3, NULL)").unwrap();
    let rows = engine.query("SELECT id FROM t WHERE name ILIKE 'AL%' ORDER BY id").unwrap();
    assert_eq!(rows_of(&rows), vec![vec!["1".to_owned()]]);
    let rows = engine.query("SELECT id FROM t WHERE name NOT ILIKE 'a%' ORDER BY id").unwrap();
    assert_eq!(rows_of(&rows), vec![vec!["2".to_owned()]]);
    let rows = engine.query("SELECT id FROM t WHERE name ILIKE '_ETA'").unwrap();
    assert_eq!(rows_of(&rows), vec![vec!["2".to_owned()]]);
    engine.close().unwrap();
}

#[test]
fn local_engine_statements_and_transactions() {
    let dir = TempDir::new("local");
    let engine = PostgreSQL::open(dir.options("smoke")).unwrap();
    engine.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)").unwrap();
    let insert = engine.execute("INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b')").unwrap();
    assert_eq!(insert.command_tag, "INSERT 0 2");
    let selected = engine.query("SELECT id, name FROM t WHERE id = 2").unwrap();
    assert_eq!(rows_of(&selected), vec![vec!["2".to_owned(), "b".to_owned()]]);
    let returning = engine.query("INSERT INTO t (id, name) VALUES (3, 'c') RETURNING id").unwrap();
    assert_eq!(rows_of(&returning), vec![vec!["3".to_owned()]]);

    engine
        .transaction(|tx| {
            tx.execute("INSERT INTO t (id, name) VALUES (4, 'd')")?;
            Ok(())
        })
        .unwrap();
    let committed = engine.query("SELECT name FROM t WHERE id = 4").unwrap();
    assert_eq!(rows_of(&committed), vec![vec!["d".to_owned()]]);

    let failed = engine.transaction(|tx| {
        tx.execute("INSERT INTO t (id, name) VALUES (5, 'e')")?;
        Err::<(), Error>(Error::State("rollback requested by test".to_owned()))
    });
    assert!(failed.is_err());
    let rolled_back = engine.query("SELECT id FROM t WHERE id = 5").unwrap();
    assert_eq!(rolled_back.row_count(), 0);
    engine.close().unwrap();
}

#[test]
fn concat_operator_propagates_null() {
    let dir = TempDir::new("concat");
    let engine = PostgreSQL::open(dir.options("smoke")).unwrap();
    engine.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)").unwrap();
    engine.execute("INSERT INTO t (id, name) VALUES (1, 'a'), (2, NULL)").unwrap();
    let joined = engine.query("SELECT name || '-x' || id FROM t WHERE id = 1").unwrap();
    assert_eq!(rows_of(&joined), vec![vec!["a-x1".to_owned()]]);
    let nulls = engine.query("SELECT name || '-x' FROM t WHERE id = 2").unwrap();
    assert!(matches!(nulls.rows[0][0], SqlValue::Null));
    engine.close().unwrap();
}

#[test]
fn scalar_functions_nullif_greatest_least_date_trunc() {
    let dir = TempDir::new("scalars");
    let engine = PostgreSQL::open(dir.options("smoke")).unwrap();
    engine.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, created_at TEXT)").unwrap();
    engine.execute("INSERT INTO t (id, created_at) VALUES (1, '2026-11-18 13:45:30')").unwrap();
    let row = engine.query("SELECT NULLIF(1, 1), NULLIF(2, 1), GREATEST(3, 7, 5), LEAST(3, 7, 5), GREATEST(3, NULL) FROM t").unwrap();
    // PostgreSQL 的 GREATEST/LEAST 忽略 NULL 实参。
    assert_eq!(rows_of(&row), vec![vec!["Null", "2", "7", "3", "3"]]);
    let times =
        engine.query("SELECT DATE_TRUNC('day', created_at), DATE_TRUNC('month', created_at), DATE_TRUNC('hour', created_at) FROM t").unwrap();
    assert_eq!(rows_of(&times), vec![vec!["2026-11-18 00:00:00", "2026-11-01 00:00:00", "2026-11-18 13:00:00"]]);
    engine.close().unwrap();
}

#[test]
fn boolean_aggregates_ignore_nulls() {
    let dir = TempDir::new("bool-agg");
    let engine = PostgreSQL::open(dir.options("smoke")).unwrap();
    engine.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, flag BOOLEAN)").unwrap();
    engine.execute("INSERT INTO t (id, flag) VALUES (1, TRUE), (2, NULL), (3, TRUE)").unwrap();
    let row = engine.query("SELECT BOOL_AND(flag), BOOL_OR(flag) FROM t").unwrap();
    assert_eq!(rows_of(&row), vec![vec!["Boolean(true)", "Boolean(true)"]]);
    engine.execute("INSERT INTO t (id, flag) VALUES (4, FALSE)").unwrap();
    let row = engine.query("SELECT BOOL_AND(flag), BOOL_OR(flag) FROM t").unwrap();
    assert_eq!(rows_of(&row), vec![vec!["Boolean(false)", "Boolean(true)"]]);
    let empty = engine.query("SELECT BOOL_AND(flag) FROM t WHERE id = 99").unwrap();
    assert!(matches!(empty.rows[0][0], SqlValue::Null));
    engine.close().unwrap();
}

#[test]
fn ddl_survives_a_restart_and_rollback_restores_the_persisted_state() {
    let dir = TempDir::new("persist");
    {
        let engine = PostgreSQL::open(dir.options("smoke")).unwrap();
        engine.execute("CREATE TABLE kept (id BIGINT PRIMARY KEY, name TEXT)").unwrap();
        engine.execute("INSERT INTO kept (id, name) VALUES (1, 'durable')").unwrap();
        // A rolled-back CREATE must not leave a table behind in the document.
        engine
            .transaction(|tx| {
                tx.execute("CREATE TABLE discarded (id BIGINT PRIMARY KEY)")?;
                Err::<(), Error>(Error::State("rollback requested by test".to_owned()))
            })
            .unwrap_err();
        engine.close().unwrap();
    }
    let reopened = PostgreSQL::open(dir.options("smoke")).unwrap();
    let rows = reopened.query("SELECT name FROM kept WHERE id = 1").unwrap();
    assert_eq!(rows_of(&rows), vec![vec!["durable".to_owned()]]);
    let missing = reopened.query("SELECT id FROM discarded").unwrap_err();
    assert!(matches!(missing, Error::Sql { ref sqlstate, .. } if sqlstate == "42P01"), "unexpected error: {missing}");
    reopened.close().unwrap();
}

#[test]
fn copy_from_stdin_accepts_text_and_csv_streams() {
    let dir = TempDir::new("copy");
    let engine = PostgreSQL::open(dir.options("smoke")).unwrap();
    engine.execute("CREATE TABLE copied (id BIGINT PRIMARY KEY, name TEXT)").unwrap();
    let listener = engine.listen("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let mut client = PostgreSQLClient::open(&ClientConfig::new("127.0.0.1", listener.endpoint().port(), "postgres", "smoke")).unwrap();

    let text = client.copy_in("COPY copied (id, name) FROM STDIN", &[b"1\tone\n2\t\\N\n"]).unwrap();
    assert_eq!(text.tag, "COPY 2");
    let csv = client.copy_in("COPY copied FROM STDIN WITH (FORMAT csv, HEADER)", &[b"id,name\n3,\"a,b\"\n"]).unwrap();
    assert_eq!(csv.tag, "COPY 1");

    let direct = engine.query("SELECT id, name FROM copied ORDER BY id").unwrap();
    assert_eq!(direct.row_count(), 3);
    let rows = client.execute("SELECT id, name FROM copied ORDER BY id").unwrap();
    let result = rows.first().expect("one result set");
    assert_eq!(result.rows.len(), 3, "rows: {:?}", result.rows);
    assert_eq!(result.rows[2].get(1).cloned().flatten().as_deref(), Some("a,b"));
    client.close();

    let mut missing = PostgreSQLClient::open(&ClientConfig::new("127.0.0.1", listener.endpoint().port(), "postgres", "smoke")).unwrap();
    let failure = missing.copy_in("COPY absent FROM STDIN", &[b"1\n"]).unwrap();
    let error = failure.error.expect("unknown table must fail the copy");
    assert!(format!("{error}").contains("42P01"), "unexpected error: {error}");
    // The connection stays usable after a rejected COPY.
    assert_eq!(missing.execute("SELECT 1").unwrap().len(), 1);
    missing.close();
    listener.close().expect("listener stops");
    engine.close().unwrap();
}

#[test]
fn listen_and_notify_reach_another_connection_before_its_next_ready() {
    let dir = TempDir::new("notify");
    let engine = PostgreSQL::open(dir.options("smoke")).unwrap();
    let listener = engine.listen("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let port = listener.endpoint().port();
    let mut subscriber = PostgreSQLClient::open(&ClientConfig::new("127.0.0.1", port, "postgres", "smoke")).unwrap();
    subscriber.execute("LISTEN events").unwrap();
    let mut publisher = PostgreSQLClient::open(&ClientConfig::new("127.0.0.1", port, "postgres", "smoke")).unwrap();
    publisher.execute("NOTIFY events, 'payload'").unwrap();
    subscriber.execute("SELECT 1").unwrap();
    let notifications = subscriber.take_notifications();
    assert_eq!(notifications.len(), 1, "expected one notification, got {notifications:?}");
    assert_eq!(notifications[0].channel, "events");
    assert_eq!(notifications[0].payload, "payload");
    publisher.close();
    subscriber.close();
    listener.close().expect("listener stops");
    engine.close().unwrap();
}

#[test]
fn listeners_require_the_configured_password_authentication() {
    use std::error::Error as _;

    let dir = TempDir::new("auth");
    let engine = PostgreSQL::open(dir.options("smoke")).unwrap();
    engine.execute("CREATE TABLE secret (id BIGINT PRIMARY KEY, name TEXT)").unwrap();
    engine.execute("INSERT INTO secret (id, name) VALUES (1, 'guarded')").unwrap();
    let endpoint = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
    for authentication in [
        postgres::Authentication::Password { user: "postgres".to_owned(), password: "s3cret".to_owned() },
        postgres::Authentication::Md5 { user: "postgres".to_owned(), password: "s3cret".to_owned() },
        postgres::Authentication::ScramSha256 { user: "postgres".to_owned(), password: "s3cret".to_owned() },
    ] {
        let label = format!("{authentication:?}");
        let listener = engine.listen_with(endpoint, authentication).unwrap();
        let mut accepted =
            PostgreSQLClient::open(&ClientConfig::new("127.0.0.1", listener.endpoint().port(), "postgres", "smoke").with_password("s3cret"))
                .unwrap_or_else(|error| panic!("{label} rejected the configured password: {error}"));
        let results = accepted.execute("SELECT name FROM secret WHERE id = 1").unwrap();
        assert_eq!(results[0].cell(0, 0), Some(Some("guarded")));
        accepted.close();

        let rejected =
            PostgreSQLClient::open(&ClientConfig::new("127.0.0.1", listener.endpoint().port(), "postgres", "smoke").with_password("wrong"));
        let error = match rejected {
            Ok(mut client) => {
                let error = client.execute("SELECT 1").unwrap_err();
                client.close();
                error
            }
            Err(error) => error,
        };
        let text = format!("{error}");
        let source = error.source().map(|source| format!("{source}"));
        assert!(
            text.contains("28P01") || source.as_deref().is_some_and(|source| source.contains("28P01")),
            "{label} must reject a wrong password with 28P01, got {text} / {source:?}"
        );
        listener.close().expect("listener stops");
    }
    engine.close().unwrap();
}

#[test]
fn listeners_verify_the_password_a_role_stores() {
    use std::error::Error as _;

    let dir = TempDir::new("role-auth");
    let engine = PostgreSQL::open(dir.options("smoke")).unwrap();
    engine.execute("CREATE TABLE secret (id BIGINT PRIMARY KEY, name TEXT)").unwrap();
    engine.execute("INSERT INTO secret (id, name) VALUES (1, 'guarded')").unwrap();
    engine.execute("CREATE ROLE app WITH LOGIN PASSWORD 'r0le'").unwrap();
    engine.execute("GRANT SELECT ON secret TO app").unwrap();

    let endpoint = "127.0.0.1:0".parse::<SocketAddr>().unwrap();
    for method in [postgres::PasswordMethod::Cleartext, postgres::PasswordMethod::Md5, postgres::PasswordMethod::ScramSha256] {
        let listener = engine.listen_with(endpoint, postgres::Authentication::role_password(method)).unwrap();
        let mut accepted =
            PostgreSQLClient::open(&ClientConfig::new("127.0.0.1", listener.endpoint().port(), "app", "smoke").with_password("r0le"))
                .unwrap_or_else(|error| panic!("{method:?} rejected the catalog password: {error}"));
        let results = accepted.execute("SELECT name FROM secret WHERE id = 1").unwrap();
        assert_eq!(results[0].cell(0, 0), Some(Some("guarded")));
        accepted.close();

        let rejected =
            PostgreSQLClient::open(&ClientConfig::new("127.0.0.1", listener.endpoint().port(), "app", "smoke").with_password("wrong"));
        let error = match rejected {
            Ok(mut client) => {
                let error = client.execute("SELECT 1").unwrap_err();
                client.close();
                error
            }
            Err(error) => error,
        };
        let text = format!("{error}");
        let source = error.source().map(|source| format!("{source}"));
        assert!(
            text.contains("28P01") || source.as_deref().is_some_and(|source| source.contains("28P01")),
            "{method:?} must reject a wrong password with 28P01, got {text} / {source:?}"
        );

        // A role the catalog knows without a stored password cannot log in.
        let unknown =
            PostgreSQLClient::open(&ClientConfig::new("127.0.0.1", listener.endpoint().port(), "postgres", "smoke").with_password(""));
        let error = match unknown {
            Ok(mut client) => {
                let error = client.execute("SELECT 1").unwrap_err();
                client.close();
                error
            }
            Err(error) => error,
        };
        let text = format!("{error}");
        let source = error.source().map(|source| format!("{source}"));
        assert!(
            text.contains("28P01") || source.as_deref().is_some_and(|source| source.contains("28P01")),
            "{method:?} must reject a role without a stored password with 28P01, got {text} / {source:?}"
        );
        listener.close().expect("listener stops");
    }
    engine.close().unwrap();
}

#[test]
fn listen_answers_protocol_v3_clients() {
    let dir = TempDir::new("listen");
    let engine = PostgreSQL::open(dir.options("smoke")).unwrap();
    engine.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)").unwrap();
    engine.execute("INSERT INTO t (id, name) VALUES (1, 'wire')").unwrap();
    let listener = engine.listen("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();

    let config = ClientConfig::new("127.0.0.1", listener.endpoint().port(), "postgres", "smoke");
    let mut client = PostgreSQLClient::open(&config).unwrap();
    let results = client.execute("SELECT name FROM t WHERE id = 1").unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].cell(0, 0), Some(Some("wire")));
    client.close();
    engine.close().unwrap();
}

#[test]
fn listeners_share_one_engine_and_close_independently() {
    let dir = TempDir::new("listeners");
    let engine = PostgreSQL::open(dir.options("smoke")).unwrap();
    engine.execute("CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)").unwrap();
    engine.execute("INSERT INTO t (id, name) VALUES (1, 'shared')").unwrap();
    let first = engine.listen("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    let second = engine.listen("127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
    assert_ne!(first.endpoint().port(), second.endpoint().port());

    let mut closed = PostgreSQLClient::open(&ClientConfig::new("127.0.0.1", first.endpoint().port(), "postgres", "smoke")).unwrap();
    first.close();
    assert!(closed.execute("SELECT 1").is_err());
    closed.close();

    let mut open = PostgreSQLClient::open(&ClientConfig::new("127.0.0.1", second.endpoint().port(), "postgres", "smoke")).unwrap();
    let results = open.execute("SELECT name FROM t WHERE id = 1").unwrap();
    assert_eq!(results[0].cell(0, 0), Some(Some("shared")));
    open.close();
    engine.close().unwrap();
}
