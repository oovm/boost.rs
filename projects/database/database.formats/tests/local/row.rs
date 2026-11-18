//! 表行记录编解码与记录名布局。

use std::collections::BTreeMap;

use database_formats::yy::row::{ROW_CODEC_VERSION, decode_row, encode_row, row_key, row_references, table_prefix};
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
