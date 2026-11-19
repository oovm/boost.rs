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
