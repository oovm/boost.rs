//! 数值内核的独立复核。
//!
//! 合同规定误差界写在文档，但注释本身不算证明，因此这里用两类参考分别测量：
//! 宿主标准库（`sqrt` 按 IEEE 正确舍入，`ln` / `exp` 接近正确舍入，作为
//! 参考而非绝对 oracle）与独立已知精确值。误差按 `f64` 位距离计量，即结果
//! 与参考之间相差的 ULP 个数，界与 `numeric` 模块文档一致：`sqrt` 不超过
//! 1 ULP，`ln` 与 `exp` 不超过 4 ULP。特殊值逐函数按冻结语义断言，
//! 不把 `NaN` 统一改成零。

use boost_math::{numeric, numeric::OrderedF64};

#[test]
fn ordered_f64_freezes_key_semantics() {
    // NaN 与 NaN 相等，并在顺序中最大。
    assert_eq!(OrderedF64::new(f64::NAN), OrderedF64::new(f64::NAN));
    assert!(OrderedF64::new(f64::NAN) > OrderedF64::new(f64::INFINITY));
    assert_eq!(OrderedF64::new(f64::NAN).cmp(&OrderedF64::new(f64::NAN)), std::cmp::Ordering::Equal);

    // ±0 按数值比较相等，不按位区分，Ord 与 Eq 一致。
    assert_eq!(OrderedF64::new(0.0), OrderedF64::new(-0.0));
    assert_eq!(OrderedF64::new(0.0).cmp(&OrderedF64::new(-0.0)), std::cmp::Ordering::Equal);

    // 取值保留原始位，包括 NaN payload 与零的符号。
    let payload = f64::from_bits(0x7ff8_0000_0000_0001);
    assert_eq!(OrderedF64::new(payload).get().to_bits(), payload.to_bits());
    assert_eq!(OrderedF64::new(-0.0).get().to_bits(), (-0.0f64).to_bits());
}

/// 两个 `f64` 之间的位距离，即相差的 ULP 个数。
///
/// 只在两者同号且都非零时有意义；异号或含零时调用方按特殊值分支处理。
fn ulp_distance(value: f64, reference: f64) -> u64 {
    debug_assert_eq!(value.is_sign_negative(), reference.is_sign_negative());
    debug_assert!(value != 0.0 && reference != 0.0);
    (value.to_bits() as i64 - reference.to_bits() as i64).unsigned_abs()
}

/// 确定性的伪随机网格：线性同余，覆盖整个 `u64` 空间。
struct Lcg(u64);

impl Lcg {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0
    }

    /// 返回 `[1e-300, 1e300]` 对数尺度上的一个值。
    fn positive_wide(&mut self) -> f64 {
        let exponent = (self.next_u64() % 1201) as f64 - 300.0;
        let mantissa = (self.next_u64() >> 11) as f64 / 4503599627370496.0; // [0, 1)
        f64::powi(10.0, exponent as i32) * (1.0 + mantissa)
    }
}

#[test]
fn sqrt_stays_within_one_ulp_of_the_correctly_rounded_reference() {
    let mut rng = Lcg(0x9e3779b97f4a7c15);
    for _ in 0..20_000 {
        let value = rng.positive_wide();
        let got = numeric::sqrt(value);
        let reference = value.sqrt();
        assert!(ulp_distance(got, reference) <= 1, "sqrt({value}) = {got}, 正确舍入值 {reference}");
    }
}

#[test]
fn sqrt_handles_subnormals_and_powers_of_four_exactly() {
    // 4 的幂的平方根是 2 的幂，格式内精确。
    for k in -200..=200 {
        let value = f64::from_bits(((1023 + 2 * k) as u64) << 52);
        let expected = f64::from_bits(((1023 + k) as u64) << 52);
        assert_eq!(numeric::sqrt(value), expected, "sqrt(2^({}) * 2) 应为 2^{k}", 2 * k);
    }
    // 次正规数：放大再缩回的路径仍落在 1 ULP 内。
    let mut rng = Lcg(0x1234_5678_9abc_def0);
    for _ in 0..2_000 {
        let bits = rng.next_u64() & 0x000f_ffff_ffff_ffff;
        let value = f64::from_bits(bits);
        if value == 0.0 {
            continue;
        }
        let got = numeric::sqrt(value);
        let reference = value.sqrt();
        assert!(ulp_distance(got, reference) <= 1, "sqrt({value}) = {got}, 正确舍入值 {reference}");
    }
}

#[test]
fn ln_stays_within_four_ulps_of_the_reference() {
    let mut rng = Lcg(0xcafe_f00d_dead_beef);
    for _ in 0..20_000 {
        let value = rng.positive_wide();
        let got = numeric::ln(value);
        let reference = value.ln();
        assert!(ulp_distance(got, reference) <= 4, "ln({value}) = {got}, 参考 {reference}");
    }
    // 接近 1 的对数用绝对界衡量：相对误差在零附近不成立。
    let mut rng = Lcg(0x0bad_c0de_1234_5678);
    for _ in 0..2_000 {
        let offset = (rng.next_u64() >> 11) as f64 * 1e-8;
        let value = 1.0 + offset;
        let got = numeric::ln(value);
        let reference = value.ln();
        assert!((got - reference).abs() <= 4.0 * f64::EPSILON * reference.abs().max(1e-12), "ln(1+{offset}) = {got}, 参考 {reference}");
    }
}

#[test]
fn ln_matches_known_exact_values() {
    assert_eq!(numeric::ln(1.0), 0.0);
    // `ln(2^k) = k * ln 2`，参考用同一常量的精确倍数。
    for k in -500..=500 {
        let value = f64::from_bits(((1023 + k) as u64) << 52);
        let reference = (k as f64) * core::f64::consts::LN_2;
        let got = numeric::ln(value);
        if reference == 0.0 {
            assert_eq!(got, 0.0);
            continue;
        }
        assert!(ulp_distance(got, reference) <= 4, "ln(2^{k}) = {got}, 参考 {reference}");
    }
    // `exp` 与 `ln` 互逆：往返误差不超过两者界之和，参数限于 `exp` 可表示的域。
    for value in [0.5, 1.0, 2.0, 10.0, 123.456, -100.0, 700.0] {
        let round_trip = numeric::ln(numeric::exp(value));
        assert!((round_trip - value).abs() <= 1e-10 * value.abs().max(1.0), "ln(exp({value})) = {round_trip}");
    }
}

#[test]
fn exp_stays_within_four_ulps_of_the_reference() {
    let mut rng = Lcg(0x5eed_1234_abcd_ef01);
    for _ in 0..20_000 {
        let value = (rng.next_u64() % 140_000) as f64 / 200.0 - 700.0; // [-700, 700]
        let got = numeric::exp(value);
        let reference = value.exp();
        assert!(ulp_distance(got, reference) <= 4, "exp({value}) = {got}, 参考 {reference}");
    }
}

#[test]
fn exp_matches_known_values() {
    assert_eq!(numeric::exp(0.0), 1.0);
    assert_eq!(numeric::exp(core::f64::consts::LN_2), 2.0);
    assert!((numeric::exp(1.0) - core::f64::consts::E).abs() <= 4.0 * f64::EPSILON);
    // `k * ln 2` 本身带舍入，直接对照 `2^k` 会把输入误差放大进结果，
    // 因此与宿主 `exp` 在同一输入上比较，只计量内核自身的误差。
    let mut rng = Lcg(0x2b1d_9f3c_7a5e_1c08);
    for _ in 0..200 {
        let k = (rng.next_u64() % 2001) as i32 - 1000;
        let value = (k as f64) * core::f64::consts::LN_2;
        let got = numeric::exp(value);
        let reference = value.exp();
        assert!(ulp_distance(got, reference) <= 4, "exp({k} * ln2) = {got}, 参考 {reference}");
    }
}

#[test]
fn special_values_follow_the_frozen_contract() {
    // sqrt：负数 NaN，零（含 -0.0）原样，无穷与 NaN 原样。
    assert!(numeric::sqrt(-1.0).is_nan());
    assert!(numeric::sqrt(f64::NAN).is_nan());
    assert_eq!(numeric::sqrt(0.0), 0.0);
    assert!(numeric::sqrt(-0.0).is_sign_negative());
    assert_eq!(numeric::sqrt(f64::INFINITY), f64::INFINITY);

    // ln：零返回负无穷，负数 NaN，无穷与 NaN 原样。
    assert_eq!(numeric::ln(0.0), f64::NEG_INFINITY);
    assert_eq!(numeric::ln(-0.0), f64::NEG_INFINITY);
    assert!(numeric::ln(-1.0).is_nan());
    assert_eq!(numeric::ln(f64::INFINITY), f64::INFINITY);
    assert!(numeric::ln(f64::NAN).is_nan());

    // exp：NaN 与正无穷原样，负无穷返回零，上溢无穷、下溢零。
    assert!(numeric::exp(f64::NAN).is_nan());
    assert_eq!(numeric::exp(f64::INFINITY), f64::INFINITY);
    assert_eq!(numeric::exp(f64::NEG_INFINITY), 0.0);
    assert_eq!(numeric::exp(710.0), f64::INFINITY);
    assert_eq!(numeric::exp(-746.0), 0.0);
    assert_eq!(numeric::exp(0.0), 1.0);
}

#[test]
fn kernels_are_deterministic() {
    for value in [2.0, 1e-300, 1e300, 0.5] {
        assert_eq!(numeric::sqrt(value).to_bits(), numeric::sqrt(value).to_bits());
        assert_eq!(numeric::ln(value).to_bits(), numeric::ln(value).to_bits());
        assert_eq!(numeric::exp(value).to_bits(), numeric::exp(value).to_bits());
    }
}
