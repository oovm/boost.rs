//! Minimal smoke for the SQLite storage writer/reader pair: create, insert,
//! read back. The detailed b-tree / overflow / freelist / rollback matrix
//! lives downstream in the SQLite product repo (`sqlite-conformance`).

use database_formats::sqlite::WriteValue;
use database_storage::sqlite::{ColumnSpec, SchemaObjectKind, SqliteWriter, TableLimits};

fn column(name: &str, declared: &str) -> ColumnSpec {
    ColumnSpec {
        name: name.to_string(),
        declared_type: declared.to_string(),
        not_null: false,
        default_value: None,
        default_text: None,
        primary_key: false,
        unique: false,
        autoincrement: false,
        collate: None,
    }
}

#[test]
fn writer_round_trip_smoke() {
    let mut writer = SqliteWriter::create(4096).expect("create database");
    writer.create_table("items", &[column("id", "INTEGER"), column("name", "TEXT")]).expect("create table");
    let rowid = writer.insert("items", &[Some(WriteValue::Integer(1)), Some(WriteValue::Text("alpha".into()))]).expect("insert");
    assert_eq!(rowid, 1);
    let rows = writer.table_rows("items", TableLimits::default()).expect("read rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].rowid, 1);
    let schema = writer.schema_objects(TableLimits::default()).expect("read schema");
    assert!(schema.iter().any(|object| object.kind == SchemaObjectKind::Table && object.name == "items"));
}

#[test]
fn query_limits_do_not_cap_rows_but_keep_scan_guards() {
    // The SQL engine executes over an image it already holds in memory, so the
    // image bounds a scan; a fixed row cap would reject legitimate large
    // results. Page and payload guards stay in force.
    let query = TableLimits::query();
    let default = TableLimits::default();
    assert_eq!(query.max_rows, usize::MAX);
    assert_eq!(query.max_pages, default.max_pages);
    assert_eq!(query.max_payload_bytes, default.max_payload_bytes);
    assert_eq!(query.max_total_payload_bytes, default.max_total_payload_bytes);
}
