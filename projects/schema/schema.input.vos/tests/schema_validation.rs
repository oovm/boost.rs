//! Facade schema validation goes through the VOS syntax crate and the VOS schema contract.

use schema_input_vos::validate_schema;

#[test]
fn schema_validation_uses_vos_syntax_and_requires_table_primary() {
    assert!(validate_schema("table User { @@id: uuid }").is_ok());
    assert!(validate_schema("table User { id: uuid }").is_err());
    assert!(validate_schema("table User {").is_err());
}

#[test]
fn schema_validation_accepts_using_declarations() {
    // `using` names another source unit. A single-source projection cannot resolve
    // it, but it must not fail the schema contract either.
    let schema = "\
namespace demo::logs

using demo::core::User

table Entry {
    @@id: uuid,
    user_id: &demo::core::User,
}
";
    let projection = validate_schema(schema).expect("using must be accepted");
    assert_eq!(projection.types.len(), 1);
    assert_eq!(projection.types[0].canonical_path, ["demo", "logs", "Entry"]);
}

#[test]
fn attribute_arguments_survive_projection() {
    let projection = validate_schema("table Job { @@id: uuid, [unique(ux_job_scope)] scope: utf8 }").unwrap();
    let attribute = &projection.types[0].fields[1].attributes[0];
    assert_eq!(attribute.name, "unique");
    assert_eq!(attribute.syntax, "[unique(ux_job_scope)]");
}

#[test]
fn enums_and_flags_carry_stable_numeric_members() {
    use schema_input_vos::types::TypeContractKind;

    let schema = "\
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
    let projection = validate_schema(schema).expect("numeric declarations are part of the schema contract");
    let state = projection.types.iter().find(|item| item.canonical_path == ["State"]).expect("State");
    assert_eq!(state.kind, TypeContractKind::Enums);
    assert!(state.fields.is_empty(), "a numeric type has no fields");
    assert_eq!(
        state.members.iter().map(|member| (member.canonical_name.as_str(), member.value)).collect::<Vec<_>>(),
        [("Draft", 1), ("Published", 2)],
        "decimal and hex member values resolve to their declared number"
    );

    let access = projection.types.iter().find(|item| item.canonical_path == ["Access"]).expect("Access");
    assert_eq!(access.kind, TypeContractKind::Flags);
    assert_eq!(
        access.members.iter().map(|member| (member.canonical_name.as_str(), member.value)).collect::<Vec<_>>(),
        [("Read", 1), ("Write", 16)]
    );

    let doc = projection.types.iter().find(|item| item.canonical_path == ["Doc"]).expect("Doc");
    assert_eq!(doc.kind, TypeContractKind::Table);
    assert_eq!(
        doc.fields[1].canonical_type,
        schema_input_vos::types::CanonicalType::Named(vec!["State".to_owned()])
    );
}

#[test]
fn numeric_declarations_require_unique_members_and_a_value() {
    assert!(validate_schema("enums State { Draft }").is_err(), "a member without a value must be refused");
    assert!(validate_schema("enums State { Draft = 1, Published = 1 }").is_err(), "a repeated number must be refused");
    assert!(validate_schema("flags Access { Read = 0b01 }").is_err(), "a non-decimal/hex literal must be refused");
    assert!(validate_schema("flags Access { Read = 0x11 }").is_err(), "a multi-bit flag mask must be refused");
}

#[test]
fn unite_and_structure_carry_discriminants_and_a_closed_layout() {
    use schema_input_vos::types::TypeContractKind;

    let schema = "\
unite Contact {
    Email { address: utf8, label: utf8? },
    Sms { number: utf8 },
    None,
}

structure Address {
    zip: utf8,
    country: utf8,
}

table Person {
    @@id: i64,
    contact: Contact,
    address: Address,
}
";
    let projection = validate_schema(schema).expect("composite declarations are part of the schema contract");
    let contact = projection.types.iter().find(|item| item.canonical_path == ["Contact"]).expect("Contact");
    assert_eq!(contact.kind, TypeContractKind::Unite);
    assert!(contact.closed);
    assert!(contact.fields.is_empty());
    assert_eq!(
        contact
            .variants
            .iter()
            .map(|variant| (variant.canonical_name.as_str(), variant.discriminant, variant.fields.len()))
            .collect::<Vec<_>>(),
        [("Email", 0, 2), ("Sms", 1, 1), ("None", 2, 0)],
        "a variant's discriminant is its source-order index and a bare variant has no payload"
    );

    let address = projection.types.iter().find(|item| item.canonical_path == ["Address"]).expect("Address");
    assert_eq!(address.kind, TypeContractKind::Structure);
    assert!(address.closed);
    assert_eq!(address.fields.iter().map(|field| field.canonical_name.as_str()).collect::<Vec<_>>(), ["zip", "country"]);

    let person = projection.types.iter().find(|item| item.canonical_path == ["Person"]).expect("Person");
    assert_eq!(
        person.fields[1].canonical_type,
        schema_input_vos::types::CanonicalType::Named(vec!["Contact".to_owned()])
    );
    assert_eq!(
        person.fields[2].canonical_type,
        schema_input_vos::types::CanonicalType::Named(vec!["Address".to_owned()])
    );
}

#[test]
fn unite_rejects_duplicate_variants_and_payload_fields() {
    assert!(validate_schema("unite U { A, A }\ntable T { @@id: i64, u: U }").is_err(), "duplicate variant names must be refused");
    assert!(
        validate_schema("unite U { A { value: utf8, value: utf8 } }\ntable T { @@id: i64, u: U }").is_err(),
        "duplicate variant payload fields must be refused"
    );
}
