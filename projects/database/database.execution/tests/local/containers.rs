//! `T?`, `&T` and `[T]` columns through the VOS write and read paths.

use std::collections::BTreeMap;

use database_execution::local::execute;
use database_execution::local::execute_write_with_params;
use database_query::local::{bind_vos_write, resolved_contract_from_document};
use database_types::Value;
use schema_types::ResolvedContract;

const CONTAINER_SCHEMA: &str = r#"
table Owner {
    @@id: utf8,
    label: utf8,
}

table Item {
    @@id: i64,
    note: utf8?,
    owner: &Owner,
    backup_owner: &Owner?,
    tags: [utf8],
    scores: [i64],
    choices: [bool]?,
}
"#;

const INSERT: &str = r#"Item::insert({
    id: $id,
    note: $note,
    owner: $owner,
    backup_owner: $backup_owner,
    tags: $tags,
    scores: $scores,
    choices: $choices,
})"#;

fn contract() -> ResolvedContract {
    resolved_contract_from_document(CONTAINER_SCHEMA).expect("container schema resolves")
}

fn write(records: &mut BTreeMap<String, Vec<u8>>, named: &[(&str, Value)]) {
    let contract = contract();
    let named: BTreeMap<String, Value> = named.iter().map(|(name, value)| ((*name).to_string(), value.clone())).collect();
    let bound = bind_vos_write(INSERT, &named, Some(CONTAINER_SCHEMA)).expect("write binds");
    execute_write_with_params(&bound.source, &contract, &[], Some(CONTAINER_SCHEMA), records, &bound.values).expect("write executes");
}

fn full_row() -> Vec<(&'static str, Value)> {
    vec![
        ("id", Value::I64(1)),
        ("note", Value::Null),
        ("owner", Value::Text("o1".into())),
        ("backup_owner", Value::Null),
        ("tags", Value::List(vec![Value::Text("a".into()), Value::Text("b".into())])),
        ("scores", Value::List(vec![Value::I64(4), Value::I64(9)])),
        ("choices", Value::List(vec![Value::Bool(true), Value::Bool(false)])),
    ]
}

#[test]
fn optional_reference_and_list_columns_round_trip() {
    let mut records = BTreeMap::new();
    write(&mut records, &full_row());

    let contract = contract();
    let rows = execute("Item.all().collect()", Some(&contract), &records).expect("read rows");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.get("note"), Some(&Value::Null));
    assert_eq!(row.get("owner"), Some(&Value::Text("o1".into())));
    assert_eq!(row.get("backup_owner"), Some(&Value::Null));
    assert_eq!(row.get("tags"), Some(&Value::List(vec![Value::Text("a".into()), Value::Text("b".into())])));
    assert_eq!(row.get("scores"), Some(&Value::List(vec![Value::I64(4), Value::I64(9)])));
    assert_eq!(row.get("choices"), Some(&Value::List(vec![Value::Bool(true), Value::Bool(false)])));
}

#[test]
fn an_optional_column_accepts_a_value() {
    let mut records = BTreeMap::new();
    let mut named = full_row();
    named[1] = ("note", Value::Text("hello".into()));
    named[3] = ("backup_owner", Value::Text("o2".into()));
    write(&mut records, &named);

    let contract = contract();
    let rows = execute("Item.all().collect()", Some(&contract), &records).expect("read rows");
    assert_eq!(rows[0].get("note"), Some(&Value::Text("hello".into())));
    assert_eq!(rows[0].get("backup_owner"), Some(&Value::Text("o2".into())));
}

#[test]
fn an_empty_list_round_trips_without_a_row() {
    let mut records = BTreeMap::new();
    let mut named = full_row();
    named[4] = ("tags", Value::List(Vec::new()));
    write(&mut records, &named);

    let contract = contract();
    let rows = execute("Item.all().collect()", Some(&contract), &records).expect("read rows");
    assert_eq!(rows[0].get("tags"), Some(&Value::List(Vec::new())));
}

#[test]
fn a_list_element_of_the_wrong_kind_is_rejected_before_writing() {
    let mut records = BTreeMap::new();
    let mut named = full_row();
    named[5] = ("scores", Value::List(vec![Value::I64(1), Value::Text("two".into())]));
    let contract = contract();
    let named: BTreeMap<String, Value> = named.iter().map(|(name, value)| ((*name).to_string(), value.clone())).collect();
    let bound = bind_vos_write(INSERT, &named, Some(CONTAINER_SCHEMA)).expect("write binds");
    let error = execute_write_with_params(&bound.source, &contract, &[], Some(CONTAINER_SCHEMA), &mut records, &bound.values)
        .expect_err("a text element in `[i64]` must be rejected");
    assert_eq!(error.code(), "database.schema.invalid", "unexpected error: {error}");
    assert!(records.is_empty(), "a rejected write must not reach the record map");
}

#[test]
fn a_scalar_where_a_list_is_declared_is_rejected() {
    let mut records = BTreeMap::new();
    let mut named = full_row();
    named[4] = ("tags", Value::Text("not-a-list".into()));
    let contract = contract();
    let named: BTreeMap<String, Value> = named.iter().map(|(name, value)| ((*name).to_string(), value.clone())).collect();
    let bound = bind_vos_write(INSERT, &named, Some(CONTAINER_SCHEMA)).expect("write binds");
    let error = execute_write_with_params(&bound.source, &contract, &[], Some(CONTAINER_SCHEMA), &mut records, &bound.values)
        .expect_err("`[utf8]` must not accept a bare string");
    assert_eq!(error.code(), "database.schema.invalid", "unexpected error: {error}");
}

#[test]
fn a_reference_stores_the_target_primary_key_and_rejects_another_kind() {
    let mut records = BTreeMap::new();
    let mut named = full_row();
    named[2] = ("owner", Value::I64(7));
    let contract = contract();
    let named: BTreeMap<String, Value> = named.iter().map(|(name, value)| ((*name).to_string(), value.clone())).collect();
    let bound = bind_vos_write(INSERT, &named, Some(CONTAINER_SCHEMA)).expect("write binds");
    let error = execute_write_with_params(&bound.source, &contract, &[], Some(CONTAINER_SCHEMA), &mut records, &bound.values)
        .expect_err("`&Owner` stores a utf8 key, not an integer");
    assert_eq!(error.code(), "database.schema.invalid", "unexpected error: {error}");
}
