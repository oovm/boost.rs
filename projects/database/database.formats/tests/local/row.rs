//! 表行记录编解码与记录名布局。

use std::collections::BTreeMap;

use database_formats::yy::row::{
    NamedFieldLayout, NamedValueLayout, NamedVariantLayout, ROW_CODEC_VERSION, RowSchemaDirectory, ValueLayout, decode_row,
    decode_row_with_directory, decode_tagged, decode_tagged_with_directory, encode_row, encode_row_with_directory, encode_tagged,
    row_key, row_references, table_prefix,
};
use database_types::{ObjectKind, ObjectRef, Value};

fn sample_row() -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("id".to_string(), Value::I64(7)),
        ("title".to_string(), Value::Text("hello".into())),
        ("flag".to_string(), Value::Bool(true)),
        ("missing".to_string(), Value::Null),
    ])
}

#[test]
fn a_row_round_trips_through_the_binary_record() {
    let row = sample_row();
    let bytes = encode_row(&row).expect("encode");
    assert_eq!(bytes[0], ROW_CODEC_VERSION, "the record must start with its version");
    assert_eq!(decode_row(&bytes).expect("decode"), row);
}

#[test]
fn record_names_live_under_the_row_prefix() {
    assert_eq!(row_key("Doc", "7"), "__yydb/v0/row/Doc/7");
    assert_eq!(table_prefix("Doc"), "__yydb/v0/row/Doc/");
}

#[test]
fn an_object_reference_is_reported_without_resolving_it() {
    let object = ObjectRef { algo: database_types::HashAlgo::Blake3, hash: [1; 32], size: 3, kind: ObjectKind::Blob };
    let row = BTreeMap::from([("payload".to_string(), Value::Object(object.clone()))]);
    let bytes = encode_row(&row).expect("encode");
    let references = row_references(&bytes).expect("walk references");
    assert_eq!(references.objects, vec![object]);
    assert!(references.files.is_empty());
}

#[test]
fn a_retired_record_version_fails_closed() {
    let mut bytes = encode_row(&sample_row()).expect("encode");
    bytes[0] = 0;
    assert!(decode_row(&bytes).is_err(), "a record from the retired codec must be rejected");
}

#[test]
fn unsigned_integers_round_trip_through_both_codecs() {
    let mut row = sample_row();
    row.insert("huge".to_string(), Value::U64(u64::MAX));
    let bytes = encode_row(&row).expect("encode");
    assert_eq!(decode_row(&bytes).expect("decode"), row);
    // 超过 `i64::MAX` 的值只能以 u64 标签往返，不能按有符号数截断。
    let cell = encode_tagged(&Value::U64(u64::MAX)).expect("encode tagged");
    assert_eq!(decode_tagged(&cell).expect("decode tagged"), Value::U64(u64::MAX));
}

/// Two payload shapes plus a bare variant, as the `unite` acceptance requires.
const CONTACT_SCHEMA: u64 = 42;
const CONTACT_REVISION: u32 = 7;

fn contact_named(revision: u32, email_fields: &[&str], email_variant: bool) -> BTreeMap<u64, NamedValueLayout> {
    let mut variants = Vec::new();
    if email_variant {
        variants.push(NamedVariantLayout {
            discriminant: 0,
            fields: email_fields.iter().map(|name| NamedFieldLayout { name: (*name).to_owned(), ty: ValueLayout::Dynamic }).collect(),
        });
    }
    variants.push(NamedVariantLayout {
        discriminant: 1,
        fields: vec![NamedFieldLayout { name: "number".to_owned(), ty: ValueLayout::Dynamic }],
    });
    variants.push(NamedVariantLayout { discriminant: 2, fields: Vec::new() });
    BTreeMap::from([(CONTACT_SCHEMA, NamedValueLayout::Unite { schema_id: CONTACT_SCHEMA, revision, variants })])
}

fn contact_directory(revision: u32, email_fields: &[&str], email_variant: bool) -> RowSchemaDirectory {
    let columns = BTreeMap::from([("contact".to_owned(), ValueLayout::Named(CONTACT_SCHEMA))]);
    RowSchemaDirectory::new(columns, contact_named(revision, email_fields, email_variant))
}

fn contact_email(address: &str, label: Option<&str>) -> Value {
    // A missing optional member is stored as an explicit null, the same shape
    // write-boundary coercion normalizes to.
    let mut payload = BTreeMap::new();
    payload.insert("address".to_owned(), Value::Text(address.to_owned()));
    payload.insert("label".to_owned(), label.map_or(Value::Null, |label| Value::Text(label.to_owned())));
    Value::List(vec![Value::I64(0), Value::Row(payload)])
}

#[test]
fn a_unite_value_round_trips_with_discriminant_and_positional_payload() {
    let directory = contact_directory(CONTACT_REVISION, &["address", "label"], true);
    let mut row = BTreeMap::new();
    row.insert("contact".to_owned(), contact_email("ada@example.com", Some("work")));
    let bytes = encode_row_with_directory(&row, &directory).expect("encode unite row");
    // The record must not repeat the variant name or the payload field names.
    assert!(!bytes.windows(b"Email".len()).any(|window| window == b"Email"), "variant name leaked into the record");
    assert!(!bytes.windows(b"address".len()).any(|window| window == b"address"), "payload member name leaked into the record");
    // Reopen: a separately built directory of the same shape must decode the bytes.
    let reopened = contact_directory(CONTACT_REVISION, &["address", "label"], true);
    assert_eq!(decode_row_with_directory(&bytes, &reopened).expect("decode unite row"), row);
}

#[test]
fn unite_variants_with_different_payload_shapes_and_a_bare_variant_round_trip() {
    let mut row = BTreeMap::new();
    row.insert("email".to_owned(), contact_email("ada@example.com", None));
    row.insert("sms".to_owned(), Value::List(vec![Value::I64(1), Value::Row(BTreeMap::from([("number".to_owned(), Value::Text("+1".to_owned()))]))]));
    row.insert("none".to_owned(), Value::List(vec![Value::I64(2), Value::Row(BTreeMap::new())]));
    let columns = ["email", "sms", "none"]
        .into_iter()
        .map(|name| (name.to_owned(), ValueLayout::Named(CONTACT_SCHEMA)))
        .collect();
    let directory = RowSchemaDirectory::new(columns, contact_named(CONTACT_REVISION, &["address", "label"], true));
    let bytes = encode_row_with_directory(&row, &directory).expect("encode three variants");
    assert_eq!(decode_row_with_directory(&bytes, &directory).expect("decode three variants"), row);
}

#[test]
fn an_undeclared_discriminant_or_a_payload_shape_mismatch_is_rejected() {
    let directory = contact_directory(CONTACT_REVISION, &["address", "label"], true);
    let mut row = BTreeMap::new();
    row.insert("contact".to_owned(), contact_email("ada@example.com", Some("work")));
    let bytes = encode_row_with_directory(&row, &directory).expect("encode");

    // Same schema id and revision but the discriminant is no longer declared.
    let without_email = contact_directory(CONTACT_REVISION, &["address", "label"], false);
    assert!(decode_row_with_directory(&bytes, &without_email).is_err(), "an undeclared discriminant must be rejected");

    // Same discriminant but a payload member count that disagrees with the bytes.
    let narrowed = contact_directory(CONTACT_REVISION, &["address"], true);
    assert!(decode_row_with_directory(&bytes, &narrowed).is_err(), "a payload/variant mismatch must be rejected");

    // A different layout revision is rejected instead of reinterpreted.
    let stale = contact_directory(8, &["address", "label"], true);
    assert!(decode_row_with_directory(&bytes, &stale).is_err(), "an unknown layout revision must be rejected");
}

#[test]
fn renaming_a_variant_keeps_the_discriminant_and_decodes_old_records() {
    let original = contact_directory(CONTACT_REVISION, &["address", "label"], true);
    let mut row = BTreeMap::new();
    row.insert("contact".to_owned(), contact_email("ada@example.com", Some("work")));
    let bytes = encode_row_with_directory(&row, &original).expect("encode with the old spelling");

    // A rename keeps the discriminant and payload shape, so the layout revision
    // and the stored bytes stay valid; decoding still binds the original id.
    let renamed = contact_directory(CONTACT_REVISION, &["address", "label"], true);
    let decoded = decode_row_with_directory(&bytes, &renamed).expect("rename must keep old records readable");
    assert_eq!(decoded, row);
    match decoded.get("contact") {
        Some(Value::List(items)) => assert_eq!(items[0], Value::I64(0), "the discriminant is the variant identity"),
        other => panic!("expected a canonical union value, got {other:?}"),
    }
}

#[test]
fn an_unknown_named_tag_is_rejected_by_both_decoders() {
    let directory = contact_directory(CONTACT_REVISION, &["address", "label"], true);
    assert!(decode_tagged_with_directory(&[0xFF], &directory).is_err());
    assert!(decode_tagged(&[0xFF]).is_err(), "the ambient decoder must reject unknown tags as well");
}

#[test]
fn a_union_value_needs_its_directory_to_decode() {
    let directory = contact_directory(CONTACT_REVISION, &["address", "label"], true);
    let mut row = BTreeMap::new();
    row.insert("contact".to_owned(), contact_email("ada@example.com", None));
    let bytes = encode_row_with_directory(&row, &directory).expect("encode");
    assert!(decode_row(&bytes).is_err(), "named values fail closed without the schema directory");
}

/// One closed `structure` with a required and an optional member.
const ADDRESS_SCHEMA: u64 = 43;

fn address_directory(revision: u32, members: &[&str]) -> RowSchemaDirectory {
    let named = BTreeMap::from([(
        ADDRESS_SCHEMA,
        NamedValueLayout::Structure {
            schema_id: ADDRESS_SCHEMA,
            revision,
            fields: members.iter().map(|name| NamedFieldLayout { name: (*name).to_owned(), ty: ValueLayout::Dynamic }).collect(),
        },
    )]);
    let columns = BTreeMap::from([("address".to_owned(), ValueLayout::Named(ADDRESS_SCHEMA))]);
    RowSchemaDirectory::new(columns, named)
}

fn stored_address(zip: &str, country: Option<&str>) -> Value {
    let mut fields = BTreeMap::new();
    fields.insert("zip".to_owned(), Value::Text(zip.to_owned()));
    fields.insert("country".to_owned(), country.map_or(Value::Null, |country| Value::Text(country.to_owned())));
    Value::Row(fields)
}

#[test]
fn a_structure_value_uses_a_closed_positional_layout() {
    let directory = address_directory(9, &["zip", "country"]);
    let mut row = BTreeMap::new();
    row.insert("address".to_owned(), stored_address("100000", None));
    let closed = encode_row_with_directory(&row, &directory).expect("encode closed structure");
    // The dynamic encoding keeps every member name; the closed one must not.
    let dynamic = encode_row(&row).expect("encode dynamic record");
    assert!(!closed.windows(b"zip".len()).any(|window| window == b"zip"), "member name leaked into the closed record");
    assert!(!closed.windows(b"country".len()).any(|window| window == b"country"), "member name leaked into the closed record");
    assert_eq!(closed.len(), 42, "closed structure record is 42 persisted bytes");
    assert_eq!(dynamic.len(), 50, "the dynamic name-tagged record is 50 persisted bytes");
    assert!(closed.len() < dynamic.len(), "the closed layout must not be larger than the dynamic one");
    assert_eq!(decode_row_with_directory(&closed, &directory).expect("decode closed structure"), row);
}

#[test]
fn a_structure_rejects_unknown_members_unknown_revisions_and_truncated_records() {
    let directory = address_directory(9, &["zip", "country"]);
    let mut row = BTreeMap::new();
    row.insert("address".to_owned(), stored_address("100000", None));
    let bytes = encode_row_with_directory(&row, &directory).expect("encode");

    let mut with_unknown = match stored_address("100000", None) {
        Value::Row(fields) => fields,
        other => panic!("expected a record, got {other:?}"),
    };
    with_unknown.insert("district".to_owned(), Value::Text("haidian".to_owned()));
    let mut refused = BTreeMap::new();
    refused.insert("address".to_owned(), Value::Row(with_unknown));
    assert!(encode_row_with_directory(&refused, &directory).is_err(), "an undeclared member must be refused");

    let stale = address_directory(10, &["zip", "country"]);
    assert!(decode_row_with_directory(&bytes, &stale).is_err(), "an unknown layout revision must be rejected");
    // A declaration reorder changes the positional binding, so the execution
    // catalog derives a new revision for it and old bytes reject instead of
    // binding a value to another member.
    let reordered = address_directory(11, &["country", "zip"]);
    assert!(decode_row_with_directory(&bytes, &reordered).is_err(), "a reordered layout needs its own revision");

    assert!(decode_row_with_directory(&bytes[..bytes.len() - 1], &directory).is_err(), "a truncated record must be rejected");

    // Corrupt the field count inside the nested value: version + count + bitmap +
    // column name, then tag + schema id + revision, then the count itself.
    let mut corrupt = bytes.clone();
    corrupt[28] = 3;
    assert!(decode_row_with_directory(&corrupt, &directory).is_err(), "a corrupted field count must be rejected");
}


