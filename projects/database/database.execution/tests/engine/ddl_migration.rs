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
