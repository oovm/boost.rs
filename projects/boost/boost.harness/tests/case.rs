//! Verification that `#[boost_harness::test::case]` expands one stable harness entry per declared case.

#![cfg(feature = "macros")]

#[boost_harness::test::case(
    "one plus one" => (1, 1),
    "two plus two" => (2, 2),
)]
fn addition(left: i32, right: i32) {
    boost_harness::test::assert_eq!(left + right, left * 2);
}

#[boost_harness::test::case("single positive" => 5, "single negative" => -5)]
fn single_parameter(value: i32) {
    boost_harness::test::assert!(value != 0);
}

#[boost_harness::test::case("text" => "alpha", "other text" => "beta")]
fn result_body(value: &str) -> Result<(), String> {
    if value.is_empty() { Err("empty value".to_owned()) } else { Ok(()) }
}

#[boost_harness::test::case("tuple parameter" => (7, 11))]
fn tuple_parameter(pair: (i32, i32)) {
    boost_harness::test::assert_eq!(pair.0 + pair.1, 18);
}
