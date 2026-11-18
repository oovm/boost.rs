//! Basic smoke only: the model links, serializes and carries the `/v0`
//! contract version. Detailed correctness and consistency tests run
//! downstream in the `vos-language` repository's `vos-conformance` crate.

use schema_types::{CONTRACT_FORMAT_VERSION, CanonicalScalar, VectorValue};

#[test]
fn canonical_scalars_round_trip_through_names() {
    assert_eq!(CanonicalScalar::ALL.len(), 19);
    for scalar in CanonicalScalar::ALL {
        assert_eq!(CanonicalScalar::from_name(scalar.as_str()), Some(scalar));
        assert_eq!(scalar.to_string(), scalar.as_str());
    }
}

#[test]
fn contract_format_version_is_v0() {
    assert_eq!(CONTRACT_FORMAT_VERSION, "vos-contract-v0");
}

#[test]
fn vector_value_round_trips_through_json() {
    let vector = VectorValue::new(2, vec![0.5, -0.25]).unwrap();
    let json = vector.to_json().unwrap();
    assert_eq!(VectorValue::from_json(&json).unwrap(), vector);
}
