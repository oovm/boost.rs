//! YYDB-DDL-02 回归：rename 后按新目录重建唯一规则，并保留既有行。

use database_execution::engine::{
    Connection,
    catalog::{FieldPath, RenameMap},
};

fn rename_field(type_name: &str, old: &str, new: &str) -> RenameMap {
    let mut renames = RenameMap::default();
    renames.fields.insert(FieldPath { type_name: type_name.to_owned(), field_name: old.to_owned() }, new.to_owned());
    renames
}

#[test]
fn field_rename_rebuilds_the_unique_rule_against_the_new_column() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema("table Sku { @@id: i64, [unique] sku: utf8, name: utf8 }").unwrap();
    conn.execute_vos_write(r#"Sku.insert({ id: 1, sku: "A", name: "one" })"#).unwrap();
    conn.execute_vos_write(r#"Sku.insert({ id: 2, sku: "B", name: "two" })"#).unwrap();

    conn.migrate_schema("table Sku { @@id: i64, [unique] code: utf8, name: utf8 }", &rename_field("Sku", "sku", "code")).unwrap();

    let error = conn.execute_vos_write(r#"Sku.insert({ id: 3, code: "A", name: "dup" })"#).unwrap_err();
    assert_eq!(error.code(), "database.unique.violation", "{error}");
    let rows = conn.query_vos("Sku.all().collect()").unwrap();
    assert_eq!(rows.len(), 2, "the duplicate must not be stored: {rows:?}");
}

#[test]
fn a_type_change_without_a_declared_migration_is_rejected() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema("table Item { @@id: i64, qty: i64 }").unwrap();
    conn.execute_vos_write(r#"Item.insert({ id: 1, qty: 7 })"#).unwrap();

    let error = conn.migrate_schema("table Item { @@id: i64, qty: utf8 }", &RenameMap::default()).unwrap_err();
    assert_eq!(error.code(), "database.schema.invalid", "{error}");
    assert!(error.to_string().contains("Item.qty"), "{error}");

    // The rejected migration leaves schema and rows untouched.
    assert_eq!(conn.schema_version().unwrap(), Some(1));
    let rows = conn.query_vos("Item.all().collect()").unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
}

#[test]
fn ensure_schema_rejects_a_type_change_instead_of_adopting_the_document() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema("table Item { @@id: i64, qty: i64 }").unwrap();

    let error = conn.ensure_schema("table Item { @@id: i64, qty: utf8 }").unwrap_err();
    assert_eq!(error.code(), "database.schema.invalid", "{error}");
    assert_eq!(conn.schema().unwrap().unwrap().document, "table Item { @@id: i64, qty: i64 }");
}

#[test]
fn a_rejected_migration_leaves_the_published_schema_and_rows_for_reopen() {
    let path = std::env::temp_dir().join(format!("ddl-migration-rejected-{}.yydx", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let conn = Connection::open(&path).unwrap();
        conn.ensure_schema("table Item { @@id: i64, qty: i64 }").unwrap();
        conn.execute_vos_write(r#"Item.insert({ id: 1, qty: 7 })"#).unwrap();
        assert!(conn.migrate_schema("table Item { @@id: i64, qty: utf8 }", &RenameMap::default()).is_err());
        conn.close();
    }
    let conn = Connection::open(&path).unwrap();
    assert_eq!(conn.schema_version().unwrap(), Some(1), "a failed migration must not advance the version");
    assert_eq!(conn.query_vos("Item.all().collect()").unwrap().len(), 1);
    conn.close();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_published_rename_invalidates_prepared_statements() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema("table Sku { @@id: i64, sku: utf8 }").unwrap();
    conn.execute_vos_write(r#"Sku.insert({ id: 1, sku: "A" })"#).unwrap();
    let stmt = conn.prepare_vos("Sku.all().collect()").unwrap();

    conn.migrate_schema("table Sku { @@id: i64, code: utf8 }", &rename_field("Sku", "sku", "code")).unwrap();

    let error = conn.query_prepared(&stmt, &Default::default()).unwrap_err();
    assert_eq!(error.code(), "database.schema.invalidated", "{error}");
    let fresh = conn.prepare_vos("Sku.all().collect()").unwrap();
    assert_eq!(conn.query_prepared(&fresh, &Default::default()).unwrap().len(), 1);
}

#[test]
fn a_rename_with_an_appended_optional_field_keeps_data_and_identity() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema("table Sku { @@id: i64, [unique] sku: utf8, name: utf8 }").unwrap();
    conn.execute_vos_write(r#"Sku.insert({ id: 1, sku: "A", name: "one" })"#).unwrap();

    conn.migrate_schema(
        "table Sku { @@id: i64, [unique] code: utf8, name: utf8, memo: utf8? }",
        &rename_field("Sku", "sku", "code"),
    )
    .unwrap();

    let all = conn.query_vos("Sku.all().collect()").unwrap();
    assert_eq!(all.len(), 1, "the renamed row keeps its data: {all:?}");
    let rows = conn.query_vos(r#"Sku.all().filter(s => s.code == "A").collect()"#).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let error = conn.execute_vos_write(r#"Sku.insert({ id: 2, code: "A", name: "dup" })"#).unwrap_err();
    assert_eq!(error.code(), "database.unique.violation", "{error}");
}

#[test]
fn appending_an_optional_field_keeps_old_rows_readable_as_null() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema("table Note { @@id: i64, name: utf8 }").unwrap();
    conn.execute_vos_write(r#"Note.insert({ id: 1, name: "first" })"#).unwrap();

    conn.migrate_schema("table Note { @@id: i64, name: utf8, memo: utf8? }", &RenameMap::default()).unwrap();

    let rows = conn.query_vos("Note.all().collect()").unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let missing = conn.query_vos("Note.all().filter(n => n.memo == null).collect()").unwrap();
    assert_eq!(missing.len(), 1, "an old row reads the appended optional column as null");
    conn.execute_vos_write(r#"Note.insert({ id: 2, name: "second", memo: "hi" })"#).unwrap();
    conn.execute_vos_write(r#"Note.insert({ id: 3, name: "third" })"#).unwrap();
    let present = conn.query_vos(r#"Note.all().filter(n => n.memo == "hi").collect()"#).unwrap();
    assert_eq!(present.len(), 1, "{present:?}");
}

#[test]
fn a_published_optional_field_survives_reopening_with_old_rows() {
    let path = std::env::temp_dir().join(format!("ddl-migration-optional-{}.yydx", std::process::id()));
    let _ = std::fs::remove_file(&path);
    {
        let conn = Connection::open(&path).unwrap();
        conn.ensure_schema("table Note { @@id: i64, name: utf8 }").unwrap();
        conn.execute_vos_write(r#"Note.insert({ id: 1, name: "first" })"#).unwrap();
        conn.migrate_schema("table Note { @@id: i64, name: utf8, memo: utf8? }", &RenameMap::default()).unwrap();
        conn.close();
    }
    let conn = Connection::open(&path).unwrap();
    let rows = conn.query_vos("Note.all().collect()").unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    conn.close();
    let _ = std::fs::remove_file(&path);
}
