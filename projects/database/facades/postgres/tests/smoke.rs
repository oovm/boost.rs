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
        Err::<(), Error>(Error::Engine("rollback requested by test".to_owned()))
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

