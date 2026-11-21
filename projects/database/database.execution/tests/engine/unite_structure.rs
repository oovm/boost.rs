//! `structure` is a closed field record and `unite` is a tagged union.
//!
//! A `structure` value is embedded like a class record and its closed member set
//! is enforced at the write boundary. A `unite` value carries a stable variant
//! discriminant plus that variant's payload; the row codec takes both from the
//! catalog directory, so a renamed variant keeps its identity and a declaration
//! that is reordered rejects old bytes instead of reinterpreting them.

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
    Email { address: utf8, label: utf8? },
    Sms { number: utf8 },
    None,
}

table Person {
    @@id: i64,
    address: Address,
    contact: Contact?,
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

/// The author-facing spelling of a union value: one variant name plus payload.
fn variant(name: &str, payload: &[(&str, Value)]) -> Value {
    let mut fields = BTreeMap::new();
    for (key, value) in payload {
        fields.insert((*key).to_owned(), value.clone());
    }
    let mut named = BTreeMap::new();
    named.insert(name.to_owned(), Value::Row(fields));
    Value::Row(named)
}

/// The canonical spelling a decode returns: `[discriminant, payload]`.
fn stored_variant(discriminant: i64, payload: &[(&str, Value)]) -> Value {
    let mut fields = BTreeMap::new();
    for (key, value) in payload {
        fields.insert((*key).to_owned(), value.clone());
    }
    Value::List(vec![Value::I64(discriminant), Value::Row(fields)])
}

fn insert(conn: &Connection, id: i64, address: Value, contact: Option<Value>) -> database_types::Result<()> {
    let mut named = BTreeMap::new();
    named.insert("id".to_owned(), Value::I64(id));
    named.insert("address".to_owned(), address);
    let mut source = "Person.insert({ id: $id, address: $address".to_owned();
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
            [("Email", 0, 2), ("Sms", 1, 1), ("None", 2, 0)],
            "variant discriminants and payload sizes stay in the catalog"
        );
        assert_eq!(contact.variants[0].fields[0].ty, Type::Text);

        let person = catalog.types.iter().find(|entry| entry.name == "Person").expect("Person");
        let field = |name: &str| person.fields.iter().find(|field| field.name == name).expect(name).handle.ty();
        assert_eq!(field("address"), Type::Record { schema_id: address_type.schema_id, field_count: 2 });
        assert_eq!(
            field("contact"),
            Type::Record { schema_id: contact.schema_id, field_count: 3 },
            "a unite field lowers to a record shape whose member count is the variant count"
        );

        insert(&conn, 1, address("100000", Some("CN")), None).unwrap();
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
    let error = insert(&conn, 1, Value::Row(fields), None).expect_err("a closed structure rejects undeclared members");
    assert_eq!(error.code(), "database.schema.invalid");
    assert!(format!("{error}").contains("does not declare record member `district`"), "{error}");

    let error = insert(&conn, 1, Value::Row(BTreeMap::new()), None).expect_err("a closed structure requires every member");
    assert_eq!(error.code(), "database.schema.invalid");
    assert!(format!("{error}").contains("missing required member `zip`"), "{error}");
    assert!(rows(&conn).is_empty(), "a refused structure value must not store a row");
}

#[test]
fn unite_variants_with_different_payloads_carry_their_discriminant_across_a_reopen() {
    let path = path("unite-round-trip");
    cleanup(&path);
    {
        let conn = Connection::open(&path).unwrap();
        conn.ensure_schema(SCHEMA).unwrap();

        // Three variants: two payload shapes and one bare variant.
        insert(&conn, 1, address("100000", None), Some(variant("Email", &[("address", Value::Text("ada@example.com".to_owned()))])))
            .unwrap();
        insert(&conn, 2, address("200000", None), Some(variant("Sms", &[("number", Value::Text("+1-555-0100".to_owned()))]))).unwrap();
        insert(&conn, 3, address("300000", None), Some(variant("None", &[]))).unwrap();

        let stored = rows(&conn);
        assert_eq!(stored.len(), 3);
        assert_eq!(
            stored[0].get("contact"),
            Some(&stored_variant(0, &[("address", Value::Text("ada@example.com".to_owned())), ("label", Value::Null)])),
            "the write normalizes a missing optional payload member to an explicit null"
        );
        assert_eq!(
            stored[1].get("contact"),
            Some(&stored_variant(1, &[("number", Value::Text("+1-555-0100".to_owned()))])),
            "a variant with another payload shape keeps its own discriminant"
        );
        assert_eq!(stored[2].get("contact"), Some(&stored_variant(2, &[])), "a bare variant carries no payload members");
    }
    {
        let conn = Connection::open(&path).unwrap();
        let stored = rows(&conn);
        assert_eq!(stored.len(), 3, "all variants must survive a reopen");
        assert_eq!(
            stored[0].get("contact"),
            Some(&stored_variant(0, &[("address", Value::Text("ada@example.com".to_owned())), ("label", Value::Null)]))
        );
        assert_eq!(stored[1].get("contact"), Some(&stored_variant(1, &[("number", Value::Text("+1-555-0100".to_owned()))])));
        assert_eq!(stored[2].get("contact"), Some(&stored_variant(2, &[])));
    }
    cleanup(&path);
}

#[test]
fn a_unite_write_rejects_unknown_variants_and_payload_mismatches() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();

    let error = insert(&conn, 1, address("100000", None), Some(variant("Fax", &[]))).expect_err("unknown variant");
    assert_eq!(error.code(), "database.schema.invalid");
    assert!(format!("{error}").contains("`Fax` is not a declared variant of `unite Contact`"), "{error}");

    let error = insert(&conn, 1, address("100000", None), Some(variant("Email", &[("label", Value::Text("work".to_owned()))])))
        .expect_err("missing required payload member");
    assert!(format!("{error}").contains("is missing required payload member `address`"), "{error}");

    let error = insert(
        &conn,
        1,
        address("100000", None),
        Some(variant("Email", &[("address", Value::Text("ada@example.com".to_owned())), ("district", Value::Text("x".to_owned()))])),
    )
    .expect_err("undeclared payload member");
    assert!(format!("{error}").contains("does not declare payload member `district`"), "{error}");

    let error = insert(&conn, 1, address("100000", None), Some(variant("Sms", &[("address", Value::Text("x".to_owned()))])))
        .expect_err("payload of another variant");
    assert!(format!("{error}").contains("does not declare payload member `address`"), "{error}");

    // The canonical spelling is validated too: an undeclared discriminant is refused.
    let canonical = Value::List(vec![Value::I64(9), Value::Row(BTreeMap::new())]);
    let error = insert(&conn, 1, address("100000", None), Some(canonical)).expect_err("undeclared discriminant");
    assert!(format!("{error}").contains("value `9` is not a declared variant of `unite Contact`"), "{error}");

    assert!(rows(&conn).is_empty(), "a refused union value must not store a row");
}
