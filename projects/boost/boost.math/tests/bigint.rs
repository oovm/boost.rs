//! `BigInteger` 首批合同的行为测试：规范零、比较、加减乘与受检截断除余。
//! 小规模用 `i128`/`u128` 原生运算作独立 oracle，大规模用代数恒等式
//! `q * d + r == a` 与 `|r| < |d|` 复核，不以被测实现自证。

use boost_math::numeric::bigint::{BigInteger, DivideByZero};

/// 确定性 xorshift，避免测试依赖外部随机库。
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// 生成绝对值不超过 `2^bits` 的随机 `i128`。
    fn signed(&mut self, bits: u32) -> i128 {
        let mask = if bits >= 128 { u128::MAX } else { (1u128 << bits) - 1 };
        let magnitude = (self.next_u64() as u128) << 64 | self.next_u64() as u128;
        let magnitude = magnitude & mask;
        if self.next_u64() & 1 == 0 { magnitude as i128 } else { -(magnitude as i128) }
    }

    fn unsigned128(&mut self) -> u128 {
        (self.next_u64() as u128) << 64 | self.next_u64() as u128
    }
}

fn seed() -> Rng {
    Rng(0x9E37_79B9_7F4A_7C15)
}

#[test]
fn fixed_width_roundtrips_are_lossless() {
    let values_u64 = [0u64, 1, u64::MAX, 1 << 63];
    for value in values_u64 {
        let big = BigInteger::from(value);
        assert_eq!(big.to_u64(), Some(value));
        assert_eq!(big.to_u128(), Some(value as u128));
        assert_eq!(&big, &BigInteger::from(value as u128));
    }

    let values_i64 = [0i64, 1, -1, i64::MIN, i64::MAX];
    for value in values_i64 {
        let big = BigInteger::from(value);
        assert_eq!(big.to_i64(), Some(value));
        assert_eq!(big.to_i128(), Some(value as i128));
    }

    let values_u128 = [0u128, 1, 1 << 64, u64::MAX as u128 + 1, u128::MAX];
    for value in values_u128 {
        let big = BigInteger::from(value);
        assert_eq!(big.to_u128(), Some(value));
    }

    let values_i128 = [0i128, 1, -1, i128::MIN, i128::MAX];
    for value in values_i128 {
        let big = BigInteger::from(value);
        assert_eq!(big.to_i128(), Some(value));
    }
}

#[test]
fn checked_conversions_reject_out_of_range_and_sign() {
    assert_eq!(BigInteger::from(-1i64).to_u64(), None);
    assert_eq!(BigInteger::from(-1i64).to_u128(), None);
    assert_eq!(BigInteger::from(u64::MAX).to_i64(), None);
    assert_eq!(BigInteger::from(u128::MAX).to_i128(), None);
    assert_eq!(BigInteger::from(i128::MIN).to_u128(), None);
    // 2^128 进入堆上多 limb，`u128` 视图必须拒绝而不是截断。
    let two64 = BigInteger::from(1u128 << 64);
    let two128 = &two64 * &two64;
    assert_eq!(two128.to_u128(), None);
    assert_eq!(two128.to_u64(), None);
    assert_eq!(two128.to_i128(), None);
}

#[test]
fn zero_is_canonical_and_has_no_negative_form() {
    assert_eq!(BigInteger::ZERO, BigInteger::from(0i64));
    assert_eq!(BigInteger::ZERO, BigInteger::from(0u128));
    assert_eq!(BigInteger::default(), BigInteger::ZERO);
    assert!(!BigInteger::ZERO.is_negative());
    assert!(-&BigInteger::ZERO == BigInteger::ZERO);
    assert_eq!(-BigInteger::ZERO, BigInteger::ZERO);

    let mut rng = seed();
    for _ in 0..64 {
        let value = BigInteger::from(rng.signed(100));
        assert_eq!(&value - &value, BigInteger::ZERO);
        assert!(!(&value - &value).is_negative());
    }
}

#[test]
fn add_and_sub_match_i128_oracle() {
    let mut rng = seed();
    for _ in 0..512 {
        let left = rng.signed(100);
        let right = rng.signed(100);
        let a = BigInteger::from(left);
        let b = BigInteger::from(right);
        assert_eq!(&a + &b, BigInteger::from(left.wrapping_add(right)), "left={left} right={right}");
        assert_eq!(&a - &b, BigInteger::from(left.wrapping_sub(right)), "left={left} right={right}");
        assert_eq!(a.clone() + b.clone(), BigInteger::from(left.wrapping_add(right)));
        assert_eq!(a.clone() - b.clone(), BigInteger::from(left.wrapping_sub(right)));
    }
}

#[test]
fn arithmetic_crosses_limb_boundaries() {
    let max64 = BigInteger::from(u64::MAX);
    let one = BigInteger::from(1u64);
    let two64 = BigInteger::from(1u128 << 64);
    assert_eq!(&max64 + &one, two64);
    assert_eq!(&two64 - &one, max64);
    assert_eq!(&two64 - &two64, BigInteger::ZERO);

    let max128 = BigInteger::from(u128::MAX);
    let two128 = &two64 * &two64;
    assert_eq!(&max128 + &one, two128);
    assert_eq!(&two128 - &one, max128);

    // 同一数值经不同构造路径必须相等（内联双 limb 与堆上乘法结果）。
    let via_mul = &BigInteger::from(1u128 << 32) * &BigInteger::from(1u128 << 32);
    assert_eq!(via_mul, two64);
}

#[test]
fn mul_matches_i128_oracle_within_range() {
    let mut rng = seed();
    for _ in 0..512 {
        let left = rng.signed(60);
        let right = rng.signed(60);
        let a = BigInteger::from(left);
        let b = BigInteger::from(right);
        assert_eq!(&a * &b, BigInteger::from(left.wrapping_mul(right)), "left={left} right={right}");
        assert_eq!(a.clone() * b.clone(), BigInteger::from(left.wrapping_mul(right)));
    }
}

#[test]
fn div_rem_matches_i128_oracle_with_truncating_signs() {
    let mut rng = seed();
    for _ in 0..512 {
        let dividend = rng.signed(100);
        let divisor = loop {
            let candidate = rng.signed(50);
            if candidate != 0 {
                break candidate;
            }
        };
        let a = BigInteger::from(dividend);
        let b = BigInteger::from(divisor);
        let (quotient, remainder) = a.div_rem(&b).unwrap();
        assert_eq!(quotient, BigInteger::from(dividend / divisor), "dividend={dividend} divisor={divisor}");
        assert_eq!(remainder, BigInteger::from(dividend % divisor), "dividend={dividend} divisor={divisor}");
    }

    // 冻结的符号样例：商向零取整，余数与被除数同号。
    let seven = BigInteger::from(7i64);
    let two = BigInteger::from(2i64);
    let (q, r) = (-&seven).div_rem(&two).unwrap();
    assert_eq!((q, r), (BigInteger::from(-3i64), BigInteger::from(-1i64)));
    let (q, r) = seven.div_rem(&-&two).unwrap();
    assert_eq!((q, r), (BigInteger::from(-3i64), BigInteger::from(1i64)));
    let (q, r) = (-&seven).div_rem(&-&two).unwrap();
    assert_eq!((q, r), (BigInteger::from(3i64), BigInteger::from(-1i64)));
}

#[test]
fn div_rem_by_zero_is_an_explicit_error() {
    let result = BigInteger::from(1i64).div_rem(&BigInteger::ZERO);
    assert_eq!(result.err(), Some(DivideByZero));
    let result = BigInteger::ZERO.div_rem(&BigInteger::ZERO);
    assert_eq!(result.err(), Some(DivideByZero));
}

#[test]
fn div_rem_small_cases_have_known_answers() {
    let one = BigInteger::from(1u64);
    let two64 = BigInteger::from(1u128 << 64);
    let two128 = &two64 * &two64;
    let minus_one = BigInteger::from(-1i64);

    // (2^128 - 1) / (2^64 + 1) = 2^64 - 1 余 0。
    let dividend = &two128 + &minus_one;
    let divisor = &two64 + &one;
    let (q, r) = dividend.div_rem(&divisor).unwrap();
    assert_eq!(q, BigInteger::from(u64::MAX));
    assert_eq!(r, BigInteger::ZERO);

    // 2^128 / (2^64 + 1) = 2^64 - 1 余 1。
    let (q, r) = two128.div_rem(&divisor).unwrap();
    assert_eq!(q, BigInteger::from(u64::MAX));
    assert_eq!(r, BigInteger::from(1u64));

    // 2^128 / 3：2^128 ≡ 1 (mod 3)。
    let (q, r) = two128.div_rem(&BigInteger::from(3u64)).unwrap();
    assert_eq!(r, BigInteger::from(1u64));
    let product = &q * &BigInteger::from(3u64);
    assert_eq!(&product + &r, two128);

    // 被除数小于除数时商为零、余数为被除数本身，且不存在负零商。
    let (q, r) = (-&one).div_rem(&two128).unwrap();
    assert_eq!(q, BigInteger::ZERO);
    assert_eq!(r, -&one);
}

#[test]
fn large_div_rem_satisfies_the_algebraic_identity() {
    let mut rng = seed();
    for _ in 0..128 {
        // 三 limb 被除数（约 2^128 量级）配双 limb 除数，覆盖算法 D 与回加路径。
        let high = rng.unsigned128();
        let extra = rng.next_u64();
        let shifted = &BigInteger::from(high) * &BigInteger::from(1u128 << 64);
        let a = &shifted + &BigInteger::from(extra);
        // 除数强制至少双 limb（bit 64 置位），保证走算法 D 路径。
        let d = BigInteger::from(rng.unsigned128() | (1u128 << 64));
        let (q, r) = a.div_rem(&d).unwrap();
        let product = &q * &d;
        assert_eq!(&product + &r, a);
        assert!(r < d);
        assert!(!r.is_negative());
    }
}

#[test]
fn ordering_and_equality_follow_numeric_value() {
    let mut values: Vec<BigInteger> = [5i64, -3, 0, 128, -128, 1]
        .into_iter()
        .map(BigInteger::from)
        .collect();
    values.push(BigInteger::from(u128::MAX));
    values.push(&BigInteger::from(1u128 << 64) * &BigInteger::from(1u128 << 64));
    values.sort();
    let mut previous: Option<&BigInteger> = None;
    for value in &values {
        if let Some(previous) = previous {
            assert!(previous <= value);
        }
        previous = Some(value);
    }
    let expected = [-128i64, -3, 0, 1, 5, 128]
        .into_iter()
        .map(BigInteger::from)
        .chain([BigInteger::from(u128::MAX), &BigInteger::from(1u128 << 64) * &BigInteger::from(1u128 << 64)])
        .collect::<Vec<_>>();
    assert_eq!(values, expected);
}

#[test]
fn bytes_roundtrip_is_lossless_and_canonical() {
    assert_eq!(BigInteger::from_bytes_be(&[]), BigInteger::ZERO);
    assert_eq!(BigInteger::from_bytes_be(&[0, 0, 0]), BigInteger::ZERO);
    assert_eq!(BigInteger::ZERO.to_bytes_be(), Vec::<u8>::new());
    assert_eq!(BigInteger::from_bytes_be(&[0xFF]), BigInteger::from(255u64));
    assert_eq!(BigInteger::from_bytes_be(&[1, 0]), BigInteger::from(256u64));
    assert_eq!(BigInteger::from(255u64).to_bytes_be(), vec![0xFF]);
    assert_eq!(BigInteger::from(256u64).to_bytes_be(), vec![1, 0]);
    // 编码只序列化幅值，符号由 `is_negative` 单独承载。
    assert_eq!((-BigInteger::from(255i64)).to_bytes_be(), vec![0xFF]);

    // 2^128 走堆上多 limb：1 后跟 16 个零字节。
    let two64 = BigInteger::from(1u128 << 64);
    let two128 = &two64 * &two64;
    let mut expected = vec![0u8; 17];
    expected[0] = 1;
    assert_eq!(two128.to_bytes_be(), expected);
    assert_eq!(BigInteger::from_bytes_be(&expected), two128);

    let mut rng = seed();
    for _ in 0..256 {
        let value = rng.unsigned128();
        let big = BigInteger::from(value);
        let trimmed: Vec<u8> = value.to_be_bytes().into_iter().skip_while(|&byte| byte == 0).collect();
        assert_eq!(big.to_bytes_be(), trimmed, "value={value}");
        assert_eq!(BigInteger::from_bytes_be(&trimmed), big);
        // 前导零字节不改变数值。
        let mut padded = vec![0u8; 5];
        padded.extend_from_slice(&trimmed);
        assert_eq!(BigInteger::from_bytes_be(&padded), big);
    }
}

#[test]
fn bit_length_and_bit_access_match_u128_oracle() {
    assert_eq!(BigInteger::ZERO.bit_length(), 0);
    assert_eq!(BigInteger::from(1u64).bit_length(), 1);
    assert_eq!(BigInteger::from(255u64).bit_length(), 8);
    assert_eq!(BigInteger::from(256u64).bit_length(), 9);
    assert_eq!((-BigInteger::from(255i64)).bit_length(), 8);
    assert_eq!(BigInteger::from(u128::MAX).bit_length(), 128);
    let two64 = BigInteger::from(1u128 << 64);
    assert_eq!((&two64 * &two64).bit_length(), 129);

    let mut rng = seed();
    for _ in 0..256 {
        let value = rng.unsigned128();
        let big = BigInteger::from(value);
        assert_eq!(big.bit_length(), 128 - value.leading_zeros() as usize, "value={value}");
        for index in [0usize, 1, 63, 64, 65, 127] {
            let expected = value & (1u128 << index) != 0;
            assert_eq!(big.bit(index), expected, "value={value} index={index}");
        }
        assert!(!big.bit(128));
        assert!(!big.bit(usize::MAX));
    }
}

#[test]
fn shifts_match_i128_oracle_and_floor_semantics() {
    let mut rng = seed();
    for _ in 0..512 {
        let value = rng.signed(60);
        let big = BigInteger::from(value);
        for bits in [0usize, 1, 7, 63, 64] {
            assert_eq!(big.shl_bits(bits), BigInteger::from(value << bits), "value={value} shl={bits}");
        }
        for bits in [0usize, 1, 7, 63, 64, 65, 127] {
            assert_eq!(big.shr_bits(bits), BigInteger::from(value >> bits), "value={value} shr={bits}");
        }
    }

    // 跨 limb 边界与算术右移的负数向下取整语义。
    let two64 = BigInteger::from(1u128 << 64);
    let two128 = &two64 * &two64;
    assert_eq!(two128.shr_bits(64), two64);
    assert_eq!(two128.shr_bits(129), BigInteger::ZERO);
    assert_eq!((-BigInteger::from(1i64)).shr_bits(1), BigInteger::from(-1i64));
    assert_eq!((-BigInteger::from(3i64)).shr_bits(1), BigInteger::from(-2i64));
    assert_eq!((-BigInteger::from(4i64)).shr_bits(1), BigInteger::from(-2i64));
    // -2^128 是偶数，floor(-2^128 / 2) = -2^127，与正数路径同幅值。
    assert_eq!((-&two128).shr_bits(1), -two128.shr_bits(1));
    assert_eq!((-&two128 - BigInteger::from(1u64)).shr_bits(1), -(&two128.shr_bits(1) + &BigInteger::from(1u64)));
    assert_eq!(BigInteger::from(3i64).shl_bits(200).shr_bits(200), BigInteger::from(3i64));
}

/// 以 `u128` 原生模乘做独立 oracle（模数 < 2^32 保证平方不溢出）。
fn native_mod_pow(base: u64, exponent: u64, modulus: u64) -> u64 {
    let m = modulus as u128;
    let mut result = 1u128;
    let mut factor = base as u128 % m;
    let mut exp = exponent;
    while exp > 0 {
        if exp & 1 == 1 {
            result = result * factor % m;
        }
        factor = factor * factor % m;
        exp >>= 1;
    }
    result as u64
}

#[test]
fn mod_pow_matches_known_answers_and_oracle() {
    // 教科书 RSA 已知答案：65^17 mod 3233 = 2790，且回代 2790^2753 mod 3233 = 65。
    let six_five = BigInteger::from(65u64);
    let n = BigInteger::from(3233u64);
    let cipher = six_five.mod_pow(&BigInteger::from(17u64), &n).unwrap();
    assert_eq!(cipher, BigInteger::from(2790u64));
    let plain = cipher.mod_pow(&BigInteger::from(2753u64), &n).unwrap();
    assert_eq!(plain, six_five);

    let mut rng = seed();
    for _ in 0..256 {
        let modulus = rng.next_u64() % ((1u64 << 32) - 2) + 2;
        let base = rng.next_u64();
        let exponent = rng.next_u64() % (1u64 << 40);
        let expected = native_mod_pow(base, exponent, modulus);
        let got = BigInteger::from(base).mod_pow(&BigInteger::from(exponent), &BigInteger::from(modulus)).unwrap();
        assert_eq!(got, BigInteger::from(expected), "base={base} exponent={exponent} modulus={modulus}");
    }

    // 边界：指数 0 得 1、模数 1 得 0、负底数归一到 [0, m)、非法参数返回 None。
    let nine = BigInteger::from(9u64);
    assert_eq!(BigInteger::from(123i64).mod_pow(&BigInteger::ZERO, &nine), Some(BigInteger::from(1u64)));
    assert_eq!(BigInteger::from(123i64).mod_pow(&nine, &BigInteger::from(1u64)), Some(BigInteger::ZERO));
    assert_eq!(BigInteger::ZERO.mod_pow(&nine, &nine), Some(BigInteger::ZERO));
    let neg_base = BigInteger::from(-1i64);
    assert_eq!(neg_base.mod_pow(&BigInteger::from(1u64), &nine), Some(BigInteger::from(8u64)));
    // (-2)^3 mod 9 = -8 mod 9 = 1：负底数先归一再按位扫描。
    assert_eq!(BigInteger::from(-2i64).mod_pow(&BigInteger::from(3u64), &nine), Some(BigInteger::from(1u64)));
    assert_eq!(nine.mod_pow(&BigInteger::from(-1i64), &nine), None);
    assert_eq!(nine.mod_pow(&nine, &BigInteger::ZERO), None);
    assert_eq!(nine.mod_pow(&nine, &BigInteger::from(-9i64)), None);

    // 大模数双路径复核：e=2 时与「先乘后除余」的独立路径一致。
    let two64 = BigInteger::from(1u128 << 64);
    let modulus = &two64 * &two64 - BigInteger::from(159i64);
    let base = BigInteger::from(rng.unsigned128() | (1u128 << 100));
    let got = base.mod_pow(&BigInteger::from(2u64), &modulus).unwrap();
    let square = &base * &base;
    assert_eq!(got, square.div_rem(&modulus).unwrap().1);
    assert!(got < modulus);
}
