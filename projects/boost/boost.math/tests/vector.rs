//! `boost_math::vector` 的误差、特殊值与跨表示一致性。
//!
//! 参考值是宿主 `std` 的朴素 `sqrt(Σ x²)` 与逐项点积，只在中间平方不溢出、
//! 不下溢的良态范围内对照；极端范围用已知精确答案（如 `3-4-5` 与 `1e308`
//! 缩放）断言，宿主本身不是所有精度问题的绝对 oracle。

use boost_math::vector::{dot, norm};

/// 朴素参考，仅用于良态输入。
fn naive_norm(values: &[f64]) -> f64 {
    values.iter().map(|v| v * v).sum::<f64>().sqrt()
}

#[test]
fn dot_returns_none_on_length_mismatch_and_never_truncates() {
    assert_eq!(dot(&[1.0, 2.0], &[1.0, 2.0, 3.0]), None);
    assert_eq!(dot(&[1.0, 2.0, 3.0], &[1.0, 2.0]), None);
}

#[test]
fn dot_matches_known_products() {
    assert_eq!(dot(&[], &[]), Some(0.0));
    assert_eq!(dot(&[1.0, 2.0, 3.0], &[4.0, -5.0, 6.0]), Some(12.0));
    // 正交向量点积为零。
    assert_eq!(dot(&[1.0, 0.0], &[0.0, 1.0]), Some(0.0));
}

#[test]
fn dot_propagates_special_values_by_ieee_rules() {
    // `0 * inf` 是 `NaN`，逐项累加按 IEEE 规则传播，不做特殊折叠。
    assert!(dot(&[0.0, 1.0], &[f64::INFINITY, 1.0]).unwrap().is_nan());
    assert!(dot(&[1.0, f64::NAN], &[1.0, 1.0]).unwrap().is_nan());
    assert_eq!(dot(&[1.0, 1.0], &[f64::INFINITY, 0.0]), Some(f64::INFINITY));
}

#[test]
fn norm_matches_exact_pythagorean_triples() {
    assert_eq!(norm(&[3.0, 4.0]), 5.0);
    assert_eq!(norm(&[0.0, 5.0]), 5.0);
    assert_eq!(norm(&[-3.0, -4.0]), 5.0);
}

#[test]
fn norm_of_empty_and_all_zero_is_zero() {
    assert_eq!(norm(&[]), 0.0);
    assert_eq!(norm(&[0.0, -0.0, 0.0]), 0.0);
}

#[test]
fn norm_special_values_follow_frozen_semantics() {
    assert!(norm(&[1.0, f64::NAN]).is_nan());
    // 全 `NaN` 不能误报零范数。
    assert!(norm(&[f64::NAN, f64::NAN]).is_nan());
    assert_eq!(norm(&[1.0, f64::NEG_INFINITY]), f64::INFINITY);
    assert_eq!(norm(&[f64::INFINITY, f64::INFINITY]), f64::INFINITY);
}

#[test]
fn norm_does_not_overflow_on_huge_components() {
    // 朴素参考在这里溢出成 `inf`，稳定实现给出有限结果。
    let huge = [1.0e308, 1.0e308];
    assert!(naive_norm(&huge).is_infinite());
    let stable = norm(&huge);
    assert!(stable.is_finite());
    let expected = std::f64::consts::SQRT_2 * 1.0e308;
    let ulps = (stable - expected).abs() / expected;
    assert!(ulps < 1e-12, "scaled norm drifted: {stable} vs {expected}");
}

#[test]
fn norm_does_not_underflow_on_tiny_components() {
    // 次正规量级：朴素平方全部下溢成零，稳定实现保留长度。
    let tiny = [5e-324, 5e-324];
    assert_eq!(naive_norm(&tiny), 0.0);
    let stable = norm(&tiny);
    assert!(stable > 0.0, "stable norm must not collapse subnormals to zero");
    let expected = std::f64::consts::SQRT_2 * 5e-324;
    assert!((stable - expected).abs() <= expected);
}

#[test]
fn norm_agrees_with_naive_reference_on_well_scaled_inputs() {
    let cases: &[&[f64]] =
        &[&[1.0], &[1.0, 2.0, 3.0], &[-2.5, 0.5, 7.25, -9.0], &[1e-100, 1e-101, 1e-102], &[1e100, 2e100, 3e100], &[3.0, 4.0, 12.0]];
    for values in cases {
        let stable = norm(values);
        let reference = naive_norm(values);
        let tolerance = reference.abs() * 1e-12;
        assert!((stable - reference).abs() <= tolerance.max(f64::MIN_POSITIVE), "case {values:?}: {stable} vs {reference}");
    }
}

#[test]
fn fixed_arrays_and_dynamic_vectors_share_one_algorithm() {
    // 同一数据分别以固定数组与堆上 `Vec` 进入，结果必须逐位相同。
    let inline = [3.0f64, -4.0, 1e-200, 2.0e150];
    let heap: Vec<f64> = inline.to_vec();
    assert_eq!(norm(&inline).to_bits(), norm(&heap).to_bits());

    let other_inline = [2.0f64, 5.0, -1e-200, 0.5e150];
    let other_heap: Vec<f64> = other_inline.to_vec();
    let inline_dot = dot(&inline, &other_inline).expect("长度恒等");
    let heap_dot = dot(&heap, &other_heap).expect("长度恒等");
    assert_eq!(inline_dot.to_bits(), heap_dot.to_bits());

    // 混用两种表示也不改变结果。
    assert_eq!(dot(&inline, &other_heap).expect("长度恒等").to_bits(), inline_dot.to_bits());
}
