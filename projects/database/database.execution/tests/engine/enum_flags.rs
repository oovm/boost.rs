//! `enums` and `flags` are named numeric types.
//!
//! The catalog lowers both to `i64` and keeps the declared member table, so a
//! write can only store a declared `enums` number or a mask of declared `flags`
//! bits. These tests cover the legal values, the refused ones, and a reopen that
//! keeps the same bytes and named member table.

use std::collections::BTreeMap;

use database_execution::engine::{catalog::ExecutionTypeKind, Connection};
use database_execution::program::Type;
use database_types::Value;

const SCHEMA: &str = "\
enums State {
    Draft = 1,
    Published = 0x2,
}

flags Access {
    Read = 0x01,
    Write = 0x10,
}

table Doc {
    @@id: i64,
    state: State,
    access: Access,
}
";

fn path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("database-execution-enum-flags-{}-{label}.yydb", std::process::id()))
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

fn insert(conn: &Connection, id: i64, state: i64, access: i64) -> database_types::Result<()> {
    let mut named = BTreeMap::new();
    named.insert("state".to_owned(), Value::I64(state));
    named.insert("access".to_owned(), Value::I64(access));
    let source = format!("Doc.insert({{ id: {id}, state: $state, access: $access }})");
    conn.execute_vos_write_with_params(&source, &named)
}

fn rows(conn: &Connection) -> Vec<database_query::local::QueryRow> {
    conn.query_vos("Doc.all().collect()").expect("read back")
}

#[test]
fn declared_members_are_stored_and_survive_a_reopen() {
    let path = path("round-trip");
    cleanup(&path);
    {
        let conn = Connection::open(&path).unwrap();
        conn.ensure_schema(SCHEMA).unwrap();

        let catalog = conn.execution_catalog().unwrap().expect("catalog");
        let state = catalog.types.iter().find(|entry| entry.name == "State").expect("State");
        assert_eq!(state.kind, ExecutionTypeKind::Enums);
        assert_eq!(state.members.iter().map(|member| (member.name.as_str(), member.value)).collect::<Vec<_>>(), [("Draft", 1), ("Published", 2)]);
        let access = catalog.types.iter().find(|entry| entry.name == "Access").expect("Access");
        assert_eq!(access.kind, ExecutionTypeKind::Flags);
        assert_eq!(access.members.iter().map(|member| (member.name.as_str(), member.value)).collect::<Vec<_>>(), [("Read", 1), ("Write", 16)]);

        let doc = catalog.types.iter().find(|entry| entry.name == "Doc").expect("Doc");
        let field = |name: &str| doc.fields.iter().find(|field| field.name == name).expect(name).handle.ty();
        assert_eq!(field("state"), Type::I64, "an `enums` column stores the declared number");
        assert_eq!(field("access"), Type::I64, "a `flags` column stores the bit mask");

        insert(&conn, 1, 2, 17).unwrap();
        insert(&conn, 2, 1, 0).unwrap();

        let stored = rows(&conn);
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].get("state"), Some(&Value::I64(2)));
        assert_eq!(stored[0].get("access"), Some(&Value::I64(17)), "every combination of declared bits is one legal mask");
        assert_eq!(stored[1].get("access"), Some(&Value::I64(0)), "the empty flag set is legal");

        let filtered = conn.query_vos("Doc.filter(x => x.state == 2).collect()").expect("filter by enum member");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].get("id"), Some(&Value::I64(1)));

        conn.begin().unwrap();
        insert(&conn, 3, 1, 1).unwrap();
        conn.rollback().unwrap();
        assert_eq!(rows(&conn).len(), 2, "a rolled back write leaves no row");
    }
    {
        let conn = Connection::open(&path).unwrap();
        let stored = rows(&conn);
        assert_eq!(stored.len(), 2, "numeric values must survive a reopen");
        assert_eq!(stored[0].get("state"), Some(&Value::I64(2)));
        assert_eq!(stored[0].get("access"), Some(&Value::I64(17)));
    }
    cleanup(&path);
}

#[test]
fn an_undeclared_enum_number_is_refused() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();

    let error = insert(&conn, 1, 3, 1).expect_err("`3` is not a declared `State` member");
    assert_eq!(error.code(), "database.schema.invalid");
    let message = format!("{error}");
    assert!(message.contains("value `3` is not a declared member of `enums State`"), "{message}");
    assert!(message.contains("Draft=1, Published=2"), "{message}");
    assert!(rows(&conn).is_empty(), "a refused enum value must not store a row");
}

#[test]
fn an_undeclared_flag_bit_is_refused() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();

    let error = insert(&conn, 1, 1, 2).expect_err("bit 1 is not declared by `Access`");
    assert_eq!(error.code(), "database.schema.invalid");
    let message = format!("{error}");
    assert!(message.contains("flags `Access` mask `0x2` sets undeclared bits `0x2`"), "{message}");
    assert!(rows(&conn).is_empty(), "a refused flag mask must not store a row");
}

#[test]
fn numeric_members_reject_duplicate_numbers_and_multi_bit_flags_at_install() {
    let conn = Connection::open_in_memory().unwrap();
    assert!(
        conn.ensure_schema("enums State { Draft = 1, Published = 1 }\ntable Doc { @@id: i64, state: State }").is_err(),
        "two members cannot share one stable number"
    );
    assert!(
        conn.ensure_schema("flags Access { Both = 0x11 }\ntable Doc { @@id: i64, access: Access }").is_err(),
        "a `flags` member owns exactly one bit"
    );
}
