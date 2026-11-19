//! Minimal smoke for the `sqlite` facade: open, one effective statement, one
//! error path, close. The detailed connection / cursor / journal / lock matrix
//! lives downstream in the SQLite product repo (`sqlite-conformance`).

use sqlite::{Cell, Connection, ConnectionOptions};

#[test]
fn open_execute_read_close_smoke() {
    let mut connection = Connection::open(ConnectionOptions::default()).expect("in-memory connection");
    connection.execute("CREATE TABLE t (a INTEGER)", &[]).expect("create table");
    connection.execute("INSERT INTO t VALUES (?1)", &[Cell::Integer(3)]).expect("insert");
    let rows = connection.execute("SELECT a FROM t", &[]).expect("select");
    assert_eq!(rows.last().expect("result").rows, vec![vec![Cell::Integer(3)]]);
    connection.close().expect("close");
    assert_eq!(connection.execute("SELECT 1", &[]).expect_err("closed").code, "db-closed");
}

#[test]
fn missing_file_without_create_is_not_found() {
    let path = std::env::temp_dir().join(format!("sqlite-smoke-missing-{}.sqlite", std::process::id()));
    let error = Connection::open(ConnectionOptions { filename: Some(path), create: false, ..Default::default() }).expect_err("missing file");
    assert_eq!(error.code, "not-found");
}
