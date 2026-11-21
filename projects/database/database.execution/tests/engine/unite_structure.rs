//! `structure` is a closed field record and `unite` is a tagged union.
//!
//! A `structure` value is embedded like a class record and its closed member set
//! is enforced at the write boundary. A `unite` value needs a tagged codec that
//! this execution slice does not own, so the catalog keeps its variants and
//! discriminants as metadata and a write is refused instead of losing the tag.

use std::collections::BTreeMap;

use database_execution::engine::{catalog::ExecutionTypeKind, Connection};
use database_execution::program::Type;
use database_types::Value;

const SCHEMA: &str = "\
structure Address {
    zip: utf8,
    country: utf8?,
}

unite Contact {
    Email { address: utf8 },
    None,
}

table Person {
    @@id: i64,
    address: Address,
    contact: Contact,
}
";

fn path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("database-execution-unite-structure-{}-{label}.yydb", std::process::id()))
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

fn address(zip: &str, country: Option<&str>) -> Value {
    let mut fields = BTreeMap::new();
    fields.insert("zip".to_owned(), Value::Text(zip.to_owned()));
    if let Some(country) = country {
        fields.insert("country".to_owned(), Value::Text(country.to_owned()));
    }
    Value::Row(fields)
}

fn stored_address(zip: &str, country: Option<&str>) -> Value {
    let mut fields = BTreeMap::new();
    fields.insert("zip".to_owned(), Value::Text(zip.to_owned()));
    fields.insert("country".to_owned(), country.map_or(Value::Null, |country| Value::Text(country.to_owned())));
    Value::Row(fields)
}

fn insert(conn: &Connection, address: Value, contact: Option<Value>) -> database_types::Result<()> {
    let mut named = BTreeMap::new();
    named.insert("address".to_owned(), address);
    let mut source = "Person.insert({ id: 1, address: $address".to_owned();
    if let Some(contact) = contact {
        named.insert("contact".to_owned(), contact);
        source.push_str(", contact: $contact");
    }
    source.push_str(" })");
    conn.execute_vos_write_with_params(&source, &named)
}

fn rows(conn: &Connection) -> Vec<database_query::local::QueryRow> {
    conn.query_vos("Person.all().collect()").expect("read back")
}

#[test]
fn a_structure_value_is_a_closed_record_and_survives_a_reopen() {
    let path = path("round-trip");
    cleanup(&path);
    {
        let conn = Connection::open(&path).unwrap();
        conn.ensure_schema(SCHEMA).unwrap();

        let catalog = conn.execution_catalog().unwrap().expect("catalog");
        let address_type = catalog.types.iter().find(|entry| entry.name == "Address").expect("Address");
        assert_eq!(address_type.kind, ExecutionTypeKind::Structure);
        assert!(address_type.closed, "`structure` declares a closed layout");
        let contact = catalog.types.iter().find(|entry| entry.name == "Contact").expect("Contact");
        assert_eq!(contact.kind, ExecutionTypeKind::Unite);
        assert!(contact.closed);
        assert_eq!(
            contact
                .variants
                .iter()
                .map(|variant| (variant.name.as_str(), variant.discriminant, variant.fields.len()))
                .collect::<Vec<_>>(),
            [("Email", 0, 1), ("None", 1, 0)],
            "variant discriminants and payload sizes stay in the catalog"
        );
        assert_eq!(contact.variants[0].fields[0].ty, Type::Text);

        let person = catalog.types.iter().find(|entry| entry.name == "Person").expect("Person");
        let field = |name: &str| person.fields.iter().find(|field| field.name == name).expect(name).handle.ty();
        assert_eq!(field("address"), Type::Record { schema_id: address_type.schema_id, field_count: 2 });
        assert_eq!(
            field("contact"),
            Type::Record { schema_id: contact.schema_id, field_count: 2 },
            "a unite field lowers to a record shape whose member count is the variant count"
        );

        insert(&conn, address("100000", Some("CN")), None).unwrap();
        let stored = rows(&conn);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].get("address"), Some(&stored_address("100000", Some("CN"))));
    }
    {
        let conn = Connection::open(&path).unwrap();
        let stored = rows(&conn);
        assert_eq!(stored.len(), 1, "the structure value must survive a reopen");
        assert_eq!(stored[0].get("address"), Some(&stored_address("100000", Some("CN"))));
    }
    cleanup(&path);
}

#[test]
fn a_structure_rejects_unknown_and_missing_members() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();

    let mut fields = match address("100000", None) {
        Value::Row(fields) => fields,
        other => panic!("expected a record, got {other:?}"),
    };
    fields.insert("district".to_owned(), Value::Text("haidian".to_owned()));
    let error = insert(&conn, Value::Row(fields), None).expect_err("a closed structure rejects undeclared members");
    assert_eq!(error.code(), "database.schema.invalid");
    assert!(format!("{error}").contains("does not declare record member `district`"), "{error}");

    let error = insert(&conn, Value::Row(BTreeMap::new()), None).expect_err("a closed structure requires every member");
    assert_eq!(error.code(), "database.schema.invalid");
    assert!(format!("{error}").contains("missing required member `zip`"), "{error}");
    assert!(rows(&conn).is_empty(), "a refused structure value must not store a row");
}

#[test]
fn a_unite_value_is_refused_instead_of_losing_its_discriminant() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();

    let mut payload = BTreeMap::new();
    payload.insert("address".to_owned(), Value::Text("ada@example.com".to_owned()));
    let error = insert(&conn, address("100000", None), Some(Value::Row(payload))).expect_err("a union value needs a tagged codec");
    assert_eq!(error.code(), "database.schema.invalid");
    let message = format!("{error}");
    assert!(message.contains("column type `unite Contact` requires the tagged union codec"), "{message}");
    assert!(rows(&conn).is_empty(), "a refused union value must not store a row");
}
