//! A named `class` used by value is embedded in its parent row.
//!
//! The catalog lowers a class field to a schema-identified record, and the write
//! path validates the record recursively against the class's declared members.
//! These tests cover the legal shapes (a class value, `[Class]`, a nullable
//! class), the refused shapes (missing required member, unknown member, wrong
//! member type, non-record value) and a reopen that reads the same bytes.

use std::collections::BTreeMap;

use database_execution::engine::{catalog::ExecutionTypeKind, Connection};
use database_execution::program::Type;
use database_types::Value;

const SCHEMA: &str = "\
class Profile {
    name: utf8,
    age: i64,
    bio: utf8?,
}

table Account {
    @@id: i64,
    profile: Profile,
    history: [Profile],
    contact: Profile?,
}
";

fn path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("database-execution-class-value-{}-{label}.yydb", std::process::id()))
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

/// Builds a class record literal. `bio` is the optional member, so `None`
/// leaves it out instead of writing an explicit null.
fn profile(name: &str, age: i64, bio: Option<&str>) -> Value {
    let mut fields = BTreeMap::new();
    fields.insert("name".to_owned(), Value::Text(name.to_owned()));
    fields.insert("age".to_owned(), Value::I64(age));
    if let Some(bio) = bio {
        fields.insert("bio".to_owned(), Value::Text(bio.to_owned()));
    }
    Value::Row(fields)
}

/// Expected stored form of a class record: every declared member is present,
/// so a `T?` member the caller omitted reads back as an explicit null.
fn stored_profile(name: &str, age: i64, bio: Option<&str>) -> Value {
    let mut fields = BTreeMap::new();
    fields.insert("name".to_owned(), Value::Text(name.to_owned()));
    fields.insert("age".to_owned(), Value::I64(age));
    fields.insert("bio".to_owned(), bio.map_or(Value::Null, |bio| Value::Text(bio.to_owned())));
    Value::Row(fields)
}

fn insert(conn: &Connection, profile_value: Value, history: Vec<Value>, contact: Option<Value>) -> database_types::Result<()> {
    let mut named = BTreeMap::new();
    named.insert("profile".to_owned(), profile_value);
    named.insert("history".to_owned(), Value::List(history));
    let mut source = "Account.insert({ id: 1, profile: $profile, history: $history".to_owned();
    if let Some(contact) = contact {
        named.insert("contact".to_owned(), contact);
        source.push_str(", contact: $contact");
    }
    source.push_str(" })");
    conn.execute_vos_write_with_params(&source, &named)
}

fn account_rows(conn: &Connection) -> Vec<database_query::local::QueryRow> {
    conn.query_vos("Account.all().collect()").expect("read back")
}

#[test]
fn a_class_value_and_a_class_list_are_embedded_and_survive_a_reopen() {
    let path = path("round-trip");
    cleanup(&path);
    {
        let conn = Connection::open(&path).unwrap();
        conn.ensure_schema(SCHEMA).unwrap();

        let catalog = conn.execution_catalog().unwrap().expect("catalog");
        let profile_type = catalog.types.iter().find(|entry| entry.name == "Profile").expect("Profile");
        assert_eq!(profile_type.kind, ExecutionTypeKind::Class, "`class Profile` must stay a class in the catalog");
        let account = catalog.types.iter().find(|entry| entry.name == "Account").expect("Account");
        let field = |name: &str| account.fields.iter().find(|field| field.name == name).expect(name).handle.ty();
        assert_eq!(
            field("profile"),
            Type::Record { schema_id: profile_type.schema_id, field_count: 3 },
            "a by-value class must lower to a record identified by the class type id"
        );
        assert_eq!(field("history"), Type::List, "`[Profile]` lowers to a list whose elements are checked at write time");
        assert_eq!(
            field("contact"),
            Type::Record { schema_id: profile_type.schema_id, field_count: 3 },
            "a nullable class keeps the same record type"
        );

        insert(&conn, profile("Ada", 36, Some("first programmer")), vec![profile("Grace", 85, None)], None).unwrap();

        let rows = account_rows(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].get("id"), Some(&Value::I64(1)));
        assert_eq!(rows[0].get("profile"), Some(&stored_profile("Ada", 36, Some("first programmer"))));
        assert_eq!(
            rows[0].get("history"),
            Some(&Value::List(vec![stored_profile("Grace", 85, None)])),
            "a missing `T?` member normalizes to an explicit null"
        );
        assert_eq!(rows[0].get("contact"), None, "an omitted nullable class is not stored at all");
    }
    {
        let conn = Connection::open(&path).unwrap();
        let rows = account_rows(&conn);
        assert_eq!(rows.len(), 1, "the embedded class value must survive a reopen");
        assert_eq!(rows[0].get("profile"), Some(&stored_profile("Ada", 36, Some("first programmer"))));
        assert_eq!(rows[0].get("history"), Some(&Value::List(vec![stored_profile("Grace", 85, None)])));
        assert_eq!(rows[0].get("contact"), None);
    }
    cleanup(&path);
}

#[test]
fn an_explicit_null_optional_member_is_accepted_next_to_an_omitted_one() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();

    let mut fields = BTreeMap::new();
    fields.insert("name".to_owned(), Value::Text("Ada".to_owned()));
    fields.insert("age".to_owned(), Value::I64(36));
    fields.insert("bio".to_owned(), Value::Null);
    insert(&conn, Value::Row(fields), Vec::new(), Some(Value::Null)).unwrap();

    let rows = account_rows(&conn);
    assert_eq!(rows[0].get("profile"), Some(&stored_profile("Ada", 36, None)));
    assert_eq!(rows[0].get("contact"), Some(&Value::Null));
}

#[test]
fn a_missing_required_member_is_refused() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();

    let mut fields = BTreeMap::new();
    fields.insert("name".to_owned(), Value::Text("Ada".to_owned()));
    let error = insert(&conn, Value::Row(fields), Vec::new(), None).expect_err("a class record without `age` must be refused");
    assert_eq!(error.code(), "database.schema.invalid");
    assert!(format!("{error}").contains("missing required member `age`"), "{error}");
    assert!(account_rows(&conn).is_empty(), "a refused class value must not store a row");
}

#[test]
fn an_unknown_member_is_refused() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();

    let mut fields = match profile("Ada", 36, None) {
        Value::Row(fields) => fields,
        other => panic!("expected a record, got {other:?}"),
    };
    fields.insert("nickname".to_owned(), Value::Text("ada".to_owned()));
    let error = insert(&conn, Value::Row(fields), Vec::new(), None).expect_err("a member the class does not declare must be refused");
    assert_eq!(error.code(), "database.schema.invalid");
    assert!(format!("{error}").contains("does not declare record member `nickname`"), "{error}");
    assert!(account_rows(&conn).is_empty(), "a refused class value must not store a row");
}

#[test]
fn a_member_of_the_wrong_type_is_refused_with_its_path() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();

    let mut fields = BTreeMap::new();
    fields.insert("name".to_owned(), Value::Text("Ada".to_owned()));
    fields.insert("age".to_owned(), Value::Text("thirty-six".to_owned()));
    let error = insert(&conn, Value::Row(fields), Vec::new(), None).expect_err("a text `age` must be refused");
    assert_eq!(error.code(), "database.schema.invalid");
    let message = format!("{error}");
    assert!(message.contains("class `Profile` member `age`"), "{message}");
    assert!(message.contains("requires a `i64` value"), "{message}");
    assert!(account_rows(&conn).is_empty(), "a refused class value must not store a row");
}

#[test]
fn a_non_record_value_for_a_class_is_refused() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();

    let error = insert(&conn, Value::Text("Ada".to_owned()), Vec::new(), None).expect_err("a text value cannot stand in for a class");
    assert_eq!(error.code(), "database.schema.invalid");
    assert!(format!("{error}").contains("class `Profile` requires a record value, got `text`"), "{error}");
    assert!(account_rows(&conn).is_empty(), "a refused class value must not store a row");
}

#[test]
fn an_invalid_list_element_is_refused() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();

    let mut fields = BTreeMap::new();
    fields.insert("name".to_owned(), Value::Text("Grace".to_owned()));
    let error = insert(&conn, profile("Ada", 36, None), vec![Value::Row(fields)], None)
        .expect_err("an element missing `age` must be refused");
    assert_eq!(error.code(), "database.schema.invalid");
    assert!(format!("{error}").contains("missing required member `age`"), "{error}");
    assert!(account_rows(&conn).is_empty(), "a refused list element must not store a row");
}
