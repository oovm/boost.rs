//! [`kernel_number`] 合同测试。

use science_numeric::{NumericValue, add, div, pow};
use science_types::DiagnosticCode;

#[test]
fn exact_add_rational() {
    let a = NumericValue::rational_i64(1, 3).unwrap();
    let b = NumericValue::rational_i64(1, 6).unwrap();
    let got = add(a, b).unwrap();
    assert_eq!(got, NumericValue::rational_i64(1, 2).unwrap());
}

#[test]
fn div_by_zero_is_science_code() {
    let err = div(NumericValue::small_int(1), NumericValue::small_int(0)).unwrap_err();
    assert_eq!(err.code, DiagnosticCode::DivideByZero);
    assert_eq!(err.code.as_str(), "SCIENCE_DIVIDE_BY_ZERO");
}

#[test]
fn zero_to_the_zero_is_domain_error() {
    let err = pow(&NumericValue::small_int(0), &NumericValue::small_int(0)).unwrap_err();
    assert_eq!(err.code, DiagnosticCode::DomainError);
    assert_eq!(err.code.as_str(), "SCIENCE_DOMAIN_ERROR");
    let one = pow(&NumericValue::small_int(2), &NumericValue::small_int(0)).unwrap();
    assert_eq!(one, NumericValue::small_int(1));
}

#[test]
fn rational_power_perfect_cube_root() {
    let got = pow(&NumericValue::small_int(-8), &NumericValue::rational_i64(1, 3).unwrap()).unwrap();
    assert_eq!(got, NumericValue::small_int(-2));
    let got = pow(&NumericValue::small_int(16), &NumericValue::rational_i64(1, 4).unwrap()).unwrap();
    assert_eq!(got, NumericValue::small_int(2));
}
