//! Frozen error-parameter contract tests.
//!
//! Each stable code owns one payload shape. These tests hold that shape still:
//! typed construction fills it, serialisation uses the frozen keys, strict
//! parsing refuses unstructured or over-filled payloads, and a code without the
//! data its shape needs refuses to ship a half-formed object.

use document_spreadsheet_formula::error::{
    codes,
    params::{ErrorParams, ObjectKind, ParamError},
};

const CODES: [&str; 11] = [
    codes::ARGUMENT_INVALID,
    codes::WORKBOOK_NOT_FOUND,
    codes::REVISION_CONFLICT,
    codes::OBJECT_NOT_FOUND,
    codes::OBJECT_UNSUPPORTED,
    codes::FORMAT_INVALID,
    codes::FORMAT_UNSUPPORTED,
    codes::IO_READ,
    codes::IO_WRITE,
    codes::IO_EXISTS,
    codes::IO_FAILURE,
];

/// One fully-populated payload per frozen code.
fn sample_params() -> Vec<ErrorParams> {
    vec![
        ErrorParams::ArgumentInvalid { reason: "`row` 4 is outside the table".to_owned() },
        ErrorParams::WorkbookNotFound,
        ErrorParams::RevisionConflict { actual: 7, expected: Some(6) },
        ErrorParams::NotFound { kind: ObjectKind::Sheet, id: Some("Sheet1".to_owned()) },
        ErrorParams::Unsupported { operation: "edit operation".to_owned(), name: "pivot.add".to_owned() },
        ErrorParams::FormatInvalid { format: "csv".to_owned(), reason: "the file is not valid UTF-8".to_owned() },
        ErrorParams::FormatUnsupported { format: "ods".to_owned() },
        ErrorParams::IoRead { path: "book.xlsx".to_owned(), kind: "NotFound".to_owned() },
        ErrorParams::IoWrite { path: "book.xlsx".to_owned() },
        ErrorParams::IoExists { path: "book.xlsx".to_owned() },
        ErrorParams::IoFailure { reason: "the written package lost a sheet".to_owned() },
    ]
}

#[test]
fn every_frozen_code_fills_its_shape_and_round_trips() {
    let mut seen = Vec::new();
    for params in sample_params() {
        let code = params.code();
        let json = params.to_json();
        assert!(json.is_object(), "{code} carries an object payload");
        let parsed = ErrorParams::parse(code, &json).expect("the serialised payload parses back");
        assert_eq!(parsed, params, "serialising and parsing {code} is lossless");
        seen.push(code);
    }
    seen.sort_unstable();
    seen.dedup();
    let mut expected = CODES.to_vec();
    expected.sort_unstable();
    assert_eq!(seen, expected, "every frozen code is covered");
}

#[test]
fn typed_construction_uses_the_frozen_keys() {
    let json = ErrorParams::ArgumentInvalid { reason: "`row` is outside the table".to_owned() }.to_json();
    assert_eq!(json, serde_json::json!({ "reason": "`row` is outside the table" }));

    let json = ErrorParams::WorkbookNotFound.to_json();
    assert_eq!(json, serde_json::json!({}));

    let json = ErrorParams::RevisionConflict { actual: 7, expected: Some(6) }.to_json();
    assert_eq!(json, serde_json::json!({ "actual": 7, "expected": 6 }));

    let json = ErrorParams::RevisionConflict { actual: 7, expected: None }.to_json();
    assert_eq!(json, serde_json::json!({ "actual": 7 }), "an undeclared expected revision stays absent");

    let json = ErrorParams::NotFound { kind: ObjectKind::DefinedName, id: Some("Sales".to_owned()) }.to_json();
    assert_eq!(json, serde_json::json!({ "kind": "defined name", "id": "Sales" }));

    let json = ErrorParams::Unsupported { operation: "template".to_owned(), name: "budget".to_owned() }.to_json();
    assert_eq!(json, serde_json::json!({ "operation": "template", "name": "budget" }));

    let json = ErrorParams::FormatInvalid { format: "csv".to_owned(), reason: "bad bytes".to_owned() }.to_json();
    assert_eq!(json, serde_json::json!({ "format": "csv", "reason": "bad bytes" }));

    let json = ErrorParams::IoRead { path: "book.xlsx".to_owned(), kind: "PermissionDenied".to_owned() }.to_json();
    assert_eq!(json, serde_json::json!({ "path": "book.xlsx", "kind": "PermissionDenied" }));

    let json = ErrorParams::IoFailure { reason: "the package lost a sheet".to_owned() }.to_json();
    assert_eq!(json, serde_json::json!({ "reason": "the package lost a sheet" }));
}

#[test]
fn a_location_without_an_identity_stays_valid() {
    let json = ErrorParams::NotFound { kind: ObjectKind::Sheet, id: None }.to_json();
    assert_eq!(json, serde_json::json!({ "kind": "sheet" }), "a workbook without a sheet names only the kind");
    assert_eq!(ErrorParams::parse(codes::OBJECT_NOT_FOUND, &json), Ok(ErrorParams::NotFound { kind: ObjectKind::Sheet, id: None }));
}

#[test]
fn strict_parsing_rejects_unstructured_payloads() {
    for payload in [serde_json::json!(null), serde_json::json!("the workbook is gone"), serde_json::json!([1, 2])] {
        assert_eq!(
            ErrorParams::parse(codes::WORKBOOK_NOT_FOUND, &payload),
            Err(ParamError::NotAnObject),
            "a payload must be an object, not prose or a bare value"
        );
    }

    assert_eq!(
        ErrorParams::parse(codes::OBJECT_NOT_FOUND, &serde_json::json!({ "id": "Sheet1" })),
        Err(ParamError::Missing("kind")),
        "a location without its kind is refused"
    );
    assert_eq!(
        ErrorParams::parse(codes::OBJECT_NOT_FOUND, &serde_json::json!({ "kind": "sheet", "id": "Sheet1", "reason": "x" })),
        Err(ParamError::UnknownKey("reason".to_owned())),
        "keys outside the frozen shape are refused"
    );
    assert_eq!(
        ErrorParams::parse(codes::OBJECT_NOT_FOUND, &serde_json::json!({ "kind": "widget", "id": "w1" })),
        Err(ParamError::UnknownKind("widget".to_owned())),
        "an object kind outside the frozen set is refused"
    );
    assert_eq!(
        ErrorParams::parse(codes::OBJECT_NOT_FOUND, &serde_json::json!({ "kind": "sheet", "id": 4 })),
        Err(ParamError::WrongType("id")),
        "an object identity must be a string"
    );
    assert_eq!(
        ErrorParams::parse(codes::REVISION_CONFLICT, &serde_json::json!({ "actual": "7" })),
        Err(ParamError::Missing("actual")),
        "a revision must be a number"
    );
    assert_eq!(
        ErrorParams::parse(codes::WORKBOOK_NOT_FOUND, &serde_json::json!({ "reason": "gone" })),
        Err(ParamError::UnknownKey("reason".to_owned())),
        "a parameterless code accepts no parameters"
    );
    assert_eq!(
        ErrorParams::parse("workbook/unknown", &serde_json::json!({})),
        Err(ParamError::UnknownCode("workbook/unknown".to_owned())),
        "an unknown code has no frozen shape"
    );
    assert_eq!(
        ErrorParams::parse(codes::FORMAT_UNSUPPORTED, &serde_json::json!({ "format": 3 })),
        Err(ParamError::Missing("format")),
        "a format must be a string"
    );
    assert_eq!(
        ErrorParams::parse(codes::IO_READ, &serde_json::json!({ "path": "book.xlsx" })),
        Err(ParamError::Missing("kind")),
        "an OS error kind is part of the frozen payload"
    );
}

#[test]
fn the_code_and_shape_ignore_the_display_text() {
    let first = ErrorParams::ArgumentInvalid { reason: "`row` must be below 3".to_owned() };
    let second = ErrorParams::ArgumentInvalid { reason: "the row must be below 3".to_owned() };

    assert_eq!(first.code(), second.code(), "rewording never moves the code");
    let keys = |value: &serde_json::Value| value.as_object().expect("object").keys().cloned().collect::<Vec<_>>();
    assert_eq!(keys(&first.to_json()), keys(&second.to_json()), "rewording never moves the payload keys");
    assert_eq!(keys(&first.to_json()), vec!["reason".to_owned()], "the rejection reason is the only parameter");
}
