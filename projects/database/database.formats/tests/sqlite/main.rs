//! Minimal smoke for the SQLite format-3 header and record codec. The detailed
//! field corpus and codec matrix live downstream in the SQLite product repo
//! (`sqlite-conformance`).

use database_formats::sqlite::{
    DEFAULT_PAGE_SIZE, ENGINE_LIBRARY_VERSION, MAGIC, RecordLimits, RecordValue, TextEncoding, WriteValue, blank_database,
    decode_library_version, decode_record, encode_record, validate_database,
};

#[test]
fn blank_database_and_record_round_trip_smoke() {
    let pages = blank_database().expect("blank database");
    assert_eq!(pages.len(), DEFAULT_PAGE_SIZE);
    assert_eq!(validate_database(&pages).expect("page size"), DEFAULT_PAGE_SIZE);
    assert_eq!(decode_library_version(&pages).expect("library version"), ENGINE_LIBRARY_VERSION);
    assert_eq!(&pages[..MAGIC.len()], MAGIC);

    let payload = encode_record(&[WriteValue::Integer(7), WriteValue::Text("ok".into())], TextEncoding::Utf8);
    let decoded = decode_record(&payload, TextEncoding::Utf8, RecordLimits::default()).expect("decode record");
    assert_eq!(decoded, vec![RecordValue::Integer(7), RecordValue::Text("ok".into())]);
}
