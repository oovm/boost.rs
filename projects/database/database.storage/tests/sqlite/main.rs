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
