//! Frozen error-parameter contract tests.
//!
//! Each stable code owns one payload shape. These tests hold that shape still:
//! typed construction fills it, serialisation uses the frozen keys, strict
//! parsing refuses unstructured or over-filled payloads, and a code without the
//! data its shape needs refuses to ship a half-formed object.

use document_presentation::authored::{
    DeckError, codes,
    error::params::{ErrorParams, ObjectKind, ParamError},
};

const CODES: [&str; 11] = [
    codes::DECK_NOT_FOUND,
    codes::DECK_REVISION_CONFLICT,
    codes::OBJECT_NOT_FOUND,
    codes::OBJECT_TYPE_MISMATCH,
    codes::OBJECT_UNSUPPORTED,
    codes::ASSET_MISSING,
    codes::ASSET_IN_USE,
    codes::ARGUMENT_INVALID,
    codes::FORMAT_INVALID_PACKAGE,
    codes::FORMAT_UNSUPPORTED,
    codes::PLAYBACK_INVALID_STATE,
];

/// One fully-populated error per frozen code.
fn sample_errors() -> Vec<DeckError> {
    vec![
        DeckError::deck_not_found(),
        DeckError::revision_conflict(7, Some(6)).with_recoverable(true),
        DeckError::unknown(ObjectKind::Slide, "slide-3"),
        DeckError::unknown(ObjectKind::Layout, "layout-a"),
        DeckError::mismatch(ObjectKind::Element, "shape-1"),
        DeckError::unsupported("chart data"),
        DeckError::asset_missing("asset-9"),
        DeckError::asset_in_use("asset-9"),
        DeckError::new(codes::ARGUMENT_INVALID, "`row` 4 is outside the table"),
        DeckError::new(codes::FORMAT_INVALID_PACKAGE, "the entry is not valid deflate data"),
        DeckError::format_unsupported("odp"),
        DeckError::new(codes::PLAYBACK_INVALID_STATE, "the show is already running"),
    ]
}

#[test]
fn every_frozen_code_fills_its_shape_and_round_trips() {
    let mut seen = Vec::new();
    for error in sample_errors() {
        let frozen = error.frozen_params().expect("a typed error fills its frozen shape");
        assert_eq!(frozen.code(), error.code(), "the payload names the error's own code");
        let json = frozen.to_json();
        assert!(json.is_object(), "{} carries an object payload", error.code());
        let parsed = ErrorParams::parse(error.code(), &json).expect("the serialised payload parses back");
        assert_eq!(parsed, frozen, "serialising and parsing {} is lossless", error.code());
        seen.push(error.code());
    }
    seen.sort_unstable();
    seen.dedup();
    let mut expected = CODES.to_vec();
    expected.sort_unstable();
    assert_eq!(seen, expected, "every frozen code is covered");
}

#[test]
fn typed_constructors_name_the_object_and_the_format() {
    let json = DeckError::unknown(ObjectKind::Layout, "layout-a").frozen_params().expect("layout").to_json();
    assert_eq!(json, serde_json::json!({ "kind": "layout", "id": "layout-a" }));

    let json = DeckError::asset_in_use("asset-9").frozen_params().expect("asset").to_json();
    assert_eq!(json, serde_json::json!({ "assetId": "asset-9" }));

    let json = DeckError::format_unsupported("odp").frozen_params().expect("format").to_json();
    assert_eq!(json, serde_json::json!({ "format": "odp" }));

    let json = DeckError::revision_conflict(7, Some(6)).frozen_params().expect("conflict").to_json();
    assert_eq!(json, serde_json::json!({ "actual": 7, "expected": 6 }));

    let json = DeckError::revision_conflict(7, None).frozen_params().expect("conflict").to_json();
    assert_eq!(json, serde_json::json!({ "actual": 7 }), "an undeclared expected revision stays absent");
}

#[test]
fn locators_fill_the_payload_when_no_object_was_recorded() {
    let slide = document_presentation::authored::SlideId::new("slide-3").expect("identity");
    let element = document_presentation::authored::ElementId::new("text-2").expect("identity");

    let json = DeckError::new(codes::OBJECT_NOT_FOUND, "custom prose").with_slide(slide).frozen_params().expect("slide").to_json();
    assert_eq!(json, serde_json::json!({ "kind": "slide", "id": "slide-3" }), "a slide locator names the slide kind");

    let json = DeckError::new(codes::OBJECT_TYPE_MISMATCH, "custom prose")
        .with_element(element)
        .frozen_params()
        .expect("element")
        .to_json();
    assert_eq!(json, serde_json::json!({ "kind": "element", "id": "text-2" }), "an element locator wins over the slide");
}

#[test]
fn a_code_without_its_data_refuses_to_ship_a_payload() {
    let orphan = DeckError::new(codes::ASSET_IN_USE, "asset is used");
    assert_eq!(orphan.params(), None, "no asset identity means no payload");
    assert_eq!(orphan.frozen_params(), Err(ParamError::Unstructured(codes::ASSET_IN_USE)));

    let foreign = DeckError::new("io/read", "could not read the file");
    assert_eq!(foreign.params(), None, "codes outside the authored surface have no frozen shape here");
    assert_eq!(foreign.frozen_params(), Err(ParamError::Unstructured("io/read")));
}

#[test]
fn strict_parsing_rejects_unstructured_payloads() {
    for payload in [serde_json::json!(null), serde_json::json!("deck is missing"), serde_json::json!([1, 2])] {
        assert_eq!(
            ErrorParams::parse(codes::DECK_NOT_FOUND, &payload),
            Err(ParamError::NotAnObject),
            "a payload must be an object, not prose or a bare value"
        );
    }

    assert_eq!(
        ErrorParams::parse(codes::OBJECT_NOT_FOUND, &serde_json::json!({ "kind": "slide" })),
        Err(ParamError::Missing("id")),
        "a location without its identity is refused"
    );
    assert_eq!(
        ErrorParams::parse(codes::OBJECT_NOT_FOUND, &serde_json::json!({ "kind": "slide", "id": "s1", "reason": "x" })),
        Err(ParamError::UnknownKey("reason".to_owned())),
        "keys outside the frozen shape are refused"
    );
    assert_eq!(
        ErrorParams::parse(codes::OBJECT_NOT_FOUND, &serde_json::json!({ "kind": "widget", "id": "s1" })),
        Err(ParamError::UnknownKind("widget".to_owned())),
        "an object kind outside the frozen set is refused"
    );
    assert_eq!(
        ErrorParams::parse(codes::DECK_REVISION_CONFLICT, &serde_json::json!({ "actual": "7" })),
        Err(ParamError::Missing("actual")),
        "a revision must be a number"
    );
    assert_eq!(
        ErrorParams::parse(codes::DECK_NOT_FOUND, &serde_json::json!({ "reason": "gone" })),
        Err(ParamError::UnknownKey("reason".to_owned())),
        "a parameterless code accepts no parameters"
    );
    assert_eq!(
        ErrorParams::parse("deck/unknown", &serde_json::json!({})),
        Err(ParamError::UnknownCode("deck/unknown".to_owned())),
        "an unknown code has no frozen shape"
    );
    assert_eq!(
        ErrorParams::parse(codes::FORMAT_UNSUPPORTED, &serde_json::json!({ "format": 3 })),
        Err(ParamError::Missing("format")),
        "a format must be a string"
    );
}

#[test]
fn the_code_and_shape_ignore_the_display_text() {
    let first = DeckError::new(codes::ARGUMENT_INVALID, "`row` must be below 3");
    let second = DeckError::new(codes::ARGUMENT_INVALID, "line must be below 3 rows");

    assert_eq!(first.code(), second.code(), "rewording never moves the code");
    let first_json = first.frozen_params().expect("payload").to_json();
    let second_json = second.frozen_params().expect("payload").to_json();
    let keys = |value: &serde_json::Value| value.as_object().expect("object").keys().cloned().collect::<Vec<_>>();
    assert_eq!(keys(&first_json), keys(&second_json), "rewording never moves the payload keys");
    assert_eq!(keys(&first_json), vec!["reason".to_owned()], "the rejection reason is the only parameter");
}
