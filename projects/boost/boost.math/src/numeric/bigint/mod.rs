//! 有符号任意精度整数语义。
//!
//! 公共合同（首批范围按 math 设计合同冻结）：
//!
//! - 表示不变量：零只有 [`BigInteger::ZERO`] 一种规范形态，不存在负零；
//!   非零幅值的最高 limb 恒非零、采用最短表示，limb 内部按小端字节序排列。
//!   单 limb、内联双 limb 与堆上多 limb 只是带标签的内部实现细节，不作为
//!   公共 API 暴露，`size_of` 与升档/降档成本不做承诺。
//! - 首批运算：符号与大小比较、加、减、乘、受检除法与余数。位运算、移位、
//!   字节编码、`gcd` 与幂运算按真实消费者另行冻结，本模块不提供。
//! - 除法语义为截断（truncating）：商向零取整，余数与被除数同号，与 Rust
//!   固定宽度整数的 `/` 与 `%` 一致。除数为零返回明确的 [`DivideByZero`]
//!   错误，不 panic，也不返回零值掩盖失败。
//! - 分配与别名：运算按引用读取输入、返回新值，不原地修改输入；多 limb
//!   结果按需堆分配，limb 数不设人为上限，分配失败遵循全局分配器的错误
//!   策略（中止），不存在部分失败的返回路径。
//! - 转换：`From` 覆盖 `u64`、`i64`、`u128` 与 `i128` 的无损构造；向固定
//!   宽度整数的回读一律 checked（`to_*` 返回 `Option`），越域返回 `None`，
//!   不做 `as` 截断，也不静默转 `f64`。
//!
//! 负的 `BigInteger`、无符号幅值、固定宽度整数与模整数上下文是不同对象，
//! 本模块不互相隐式替换。

use alloc::vec;
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::fmt;
use core::ops::{Add, Mul, Neg, Sub};

/// 符号。规范零恒为 `Positive`，不存在负零。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sign {
    Negative,
    Positive,
}

/// 幅值的分级 limb 表示，内部实现细节，不作为公共 API 暴露。
///
/// 不变量：
///
/// - `Zero` 当且仅当幅值为零，非零幅值绝不使用 `Zero`。
/// - `Inline` 的 `count` 为 1 或 2，小端存储，`limbs[count..]` 恒为 0，
///   最高有效 limb 非零。
/// - `Heap` 至少 3 个 limb，小端，最高 limb 非零，无前导零 limb。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Magnitude {
    Zero,
    Inline { limbs: [u64; 2], count: u8 },
    Heap(Vec<u64>),
}

impl Magnitude {
    /// 小端 limb 切片视图。
    fn limbs(&self) -> &[u64] {
        match self {
            Self::Zero => &[],
            Self::Inline { limbs, count } => &limbs[..usize::from(*count)],
            Self::Heap(limbs) => limbs,
        }
    }

    /// 由已去除前导零 limb 的小端向量构造分级表示。
    fn from_trimmed(limbs: Vec<u64>) -> Self {
        match limbs.len() {
            0 => Self::Zero,
            1 => Self::Inline { limbs: [limbs[0], 0], count: 1 },
            2 => Self::Inline { limbs: [limbs[0], limbs[1]], count: 2 },
            _ => Self::Heap(limbs),
        }
    }

    /// 去除前导零 limb 后构造分级表示。
    fn canonicalize(limbs: Vec<u64>) -> Self {
        let mut trimmed = limbs;
        while trimmed.last() == Some(&0) {
            trimmed.pop();
        }
        Self::from_trimmed(trimmed)
    }

    fn from_u64(value: u64) -> Self {
        if value == 0 {
            Self::Zero
        }
        else {
            Self::Inline { limbs: [value, 0], count: 1 }
        }
    }

    fn from_u128(value: u128) -> Self {
        let low = value as u64;
        let high = (value >> 64) as u64;
        if high == 0 {
            Self::from_u64(low)
        }
        else {
            // `high != 0` 保证最高 limb 非零，无需规范化。
            Self::Inline { limbs: [low, high], count: 2 }
        }
    }
}

/// 比较两个小端幅值切片（均要求最高 limb 非零或为空）。
fn compare_limbs(left: &[u64], right: &[u64]) -> Ordering {
    left.len().cmp(&right.len()).then_with(|| {
        let mut index = left.len();
        while index > 0 {
            index -= 1;
            match left[index].cmp(&right[index]) {
                Ordering::Equal => {}
                other => return other,
            }
        }
        Ordering::Equal
    })
}

/// 逐 limb 相加。两个规范操作数之和仍为规范向量（可能多出一个进位 limb）。
fn add_limbs(left: &[u64], right: &[u64]) -> Vec<u64> {
    let width = left.len().max(right.len());
    let mut out = Vec::with_capacity(width + 1);
    let mut carry = 0u64;
    for index in 0..width {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        let sum = a as u128 + b as u128 + carry as u128;
        out.push(sum as u64);
        carry = (sum >> 64) as u64;
    }
    if carry != 0 {
        out.push(carry);
    }
    out
}

/// 要求 `left >= right`，返回差值向量（可能带前导零 limb，由调用方规范化）。
fn sub_limbs(left: &[u64], right: &[u64]) -> Vec<u64> {
    let mut out = Vec::with_capacity(left.len());
    let mut borrow = 0u64;
    for index in 0..left.len() {
        let a = left[index] as u128;
        let b = right.get(index).copied().unwrap_or(0) as u128 + borrow as u128;
        if a >= b {
            out.push((a - b) as u64);
            borrow = 0;
        }
        else {
            out.push((a + (1u128 << 64) - b) as u64);
            borrow = 1;
        }
    }
    out
}

/// 学校乘法。输出长度至多 `left.len() + right.len()`，可能带前导零 limb。
fn mul_limbs(left: &[u64], right: &[u64]) -> Vec<u64> {
    if left.is_empty() || right.is_empty() {
        return Vec::new();
    }
    let mut out = vec![0u64; left.len() + right.len()];
    for (i, &x) in left.iter().enumerate() {
        if x == 0 {
            continue;
        }
        let mut carry = 0u128;
        for (j, &y) in right.iter().enumerate() {
            let cur = out[i + j] as u128 + x as u128 * y as u128 + carry;
            out[i + j] = cur as u64;
            carry = cur >> 64;
        }
        let mut k = i + right.len();
        while carry != 0 {
            let cur = out[k] as u128 + carry;
            out[k] = cur as u64;
            carry = cur >> 64;
            k += 1;
        }
    }
    out
}

/// 整体左移 `shift`（0..64）个 bit 并追加一个高位 limb，保持小端。
fn shift_left(value: &[u64], shift: u32) -> Vec<u64> {
    let mut out = Vec::with_capacity(value.len() + 1);
    if shift == 0 {
        out.extend_from_slice(value);
        out.push(0);
        return out;
    }
    let mut prev = 0u64;
    for &limb in value {
        out.push((limb << shift) | prev);
        prev = limb >> (64 - shift);
    }
    out.push(prev);
    out
}

/// 整体右移 `shift`（0..64）个 bit，用于把规格化余数缩回原尺度。
fn shift_right(value: &[u64], shift: u32) -> Vec<u64> {
    if shift == 0 {
        return value.to_vec();
    }
    let mut out = Vec::with_capacity(value.len());
    for index in 0..value.len() {
        let low = value[index] >> shift;
        let high = if index + 1 < value.len() { value[index + 1] << (64 - shift) } else { 0 };
        out.push(low | high);
    }
    out
}

/// 单 limb 除数：逐 limb 长除，返回未规范化的商与余数。
fn div_rem_single(dividend: &[u64], divisor: u64) -> (Vec<u64>, u64) {
    let mut quotient = vec![0u64; dividend.len()];
    let mut remainder = 0u64;
    let mut index = dividend.len();
    while index > 0 {
        index -= 1;
        let cur = ((remainder as u128) << 64) | dividend[index] as u128;
        quotient[index] = (cur / divisor as u128) as u64;
        remainder = (cur % divisor as u128) as u64;
    }
    (quotient, remainder)
}

/// Knuth 算法 D。要求 `dividend > divisor` 且 `divisor.len() >= 2`，
/// 两者均为规范切片；返回未规范化的商与余数（余数已缩回原尺度）。
fn div_rem_knuth(dividend: &[u64], divisor: &[u64]) -> (Vec<u64>, Vec<u64>) {
    let n = divisor.len();
    let shift = divisor[n - 1].leading_zeros();
    let mut u = shift_left(dividend, shift);
    let v = shift_left(divisor, shift);
    let mut quotient = vec![0u64; u.len() - n];
    for j in (0..quotient.len()).rev() {
        // 试商：用被除数高两位除以除数最高 limb。
        let (mut qhat, mut rhat);
        if u[j + n] == v[n - 1] {
            qhat = u64::MAX;
            rhat = 1u128 << 64; // 基作为哨兵，使 refine 循环立即接受试商。
        }
        else {
            let cur = ((u[j + n] as u128) << 64) | u[j + n - 1] as u128;
            qhat = (cur / v[n - 1] as u128) as u64;
            rhat = cur % v[n - 1] as u128;
        }
        loop {
            let lhs = qhat as u128 * v[n - 2] as u128;
            let rhs = if rhat >= (1u128 << 64) { u128::MAX } else { (rhat << 64) | u[j + n - 2] as u128 };
            if lhs > rhs {
                qhat -= 1;
                rhat += v[n - 1] as u128;
                if rhat >= (1u128 << 64) {
                    break;
                }
            }
            else {
                break;
            }
        }
        // u[j..=j+n] -= qhat * v。
        let mut borrow = 0u64;
        for i in 0..n {
            let product = qhat as u128 * v[i] as u128;
            let low = product as u64;
            let high = (product >> 64) as u64;
            let (d1, b1) = u[j + i].overflowing_sub(low);
            let (d2, b2) = d1.overflowing_sub(borrow);
            u[j + i] = d2;
            borrow = high + b1 as u64 + b2 as u64;
        }
        let (top, b3) = u[j + n].overflowing_sub(borrow);
        u[j + n] = top;
        if b3 {
            // 试商偏大，回加除数并减一。
            qhat -= 1;
            let mut carry = 0u64;
            for i in 0..n {
                let (s1, c1) = u[j + i].overflowing_add(v[i]);
                let (s2, c2) = s1.overflowing_add(carry);
                u[j + i] = s2;
                carry = c1 as u64 + c2 as u64;
            }
            u[j + n] += carry;
        }
        quotient[j] = qhat;
    }
    let remainder = shift_right(&u[..n], shift);
    (quotient, remainder)
}

/// 要求 `divisor` 非零。返回未规范化的商与余数幅值。
fn div_rem_magnitudes(dividend: &[u64], divisor: &[u64]) -> (Vec<u64>, Vec<u64>) {
    match compare_limbs(dividend, divisor) {
        Ordering::Less => (Vec::new(), dividend.to_vec()),
        Ordering::Equal => (vec![1], Vec::new()),
        Ordering::Greater => {
            if divisor.len() == 1 {
                let (quotient, remainder) = div_rem_single(dividend, divisor[0]);
                (quotient, vec![remainder])
            }
            else {
                div_rem_knuth(dividend, divisor)
            }
        }
    }
}

/// 除数为零的错误。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DivideByZero;

impl fmt::Display for DivideByZero {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("division by zero")
    }
}

/// 有符号任意精度整数。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BigInteger {
    sign: Sign,
    magnitude: Magnitude,
}

impl BigInteger {
    /// 规范零。
    pub const ZERO: Self = Self { sign: Sign::Positive, magnitude: Magnitude::Zero };

    /// 是否为规范零。
    pub fn is_zero(&self) -> bool {
        matches!(self.magnitude, Magnitude::Zero)
    }

    /// 是否为负数（规范零返回 `false`）。
    pub fn is_negative(&self) -> bool {
        self.sign == Sign::Negative
    }

    /// 由符号与幅值构造，零一律归一到规范零。
    fn from_sign_magnitude(sign: Sign, magnitude: Magnitude) -> Self {
        if magnitude.is_zero() {
            Self::ZERO
        }
        else {
            Self { sign, magnitude }
        }
    }

    /// 比较符号与幅值，实现全序。
    fn compare(&self, other: &Self) -> Ordering {
        match (self.sign, other.sign) {
            (Sign::Negative, Sign::Negative) => compare_limbs(other.magnitude.limbs(), self.magnitude.limbs()),
            (Sign::Positive, Sign::Positive) => compare_limbs(self.magnitude.limbs(), other.magnitude.limbs()),
            (Sign::Negative, Sign::Positive) => Ordering::Less,
            (Sign::Positive, Sign::Negative) => Ordering::Greater,
        }
    }

    /// 幅值是否为零。
    fn magnitude_is_zero(magnitude: &Magnitude) -> bool {
        matches!(magnitude, Magnitude::Zero)
    }

    fn add_ref(&self, other: &Self) -> Self {
        if self.sign == other.sign {
            let magnitude = Magnitude::canonicalize(add_limbs(self.magnitude.limbs(), other.magnitude.limbs()));
            return Self::from_sign_magnitude(self.sign, magnitude);
        }
        match compare_limbs(self.magnitude.limbs(), other.magnitude.limbs()) {
            Ordering::Equal => Self::ZERO,
            Ordering::Greater => {
                let magnitude = Magnitude::canonicalize(sub_limbs(self.magnitude.limbs(), other.magnitude.limbs()));
                Self::from_sign_magnitude(self.sign, magnitude)
            }
            Ordering::Less => {
                let magnitude = Magnitude::canonicalize(sub_limbs(other.magnitude.limbs(), self.magnitude.limbs()));
                Self::from_sign_magnitude(other.sign, magnitude)
            }
        }
    }

    fn mul_ref(&self, other: &Self) -> Self {
        let magnitude = Magnitude::canonicalize(mul_limbs(self.magnitude.limbs(), other.magnitude.limbs()));
        if Self::magnitude_is_zero(&magnitude) {
            return Self::ZERO;
        }
        let sign = if self.sign == other.sign { Sign::Positive } else { Sign::Negative };
        Self { sign, magnitude }
    }

    /// 受检截断除余：商向零取整，余数与被除数同号。
    pub fn div_rem(&self, divisor: &Self) -> Result<(Self, Self), DivideByZero> {
        if divisor.is_zero() {
            return Err(DivideByZero);
        }
        let (quotient, remainder) = div_rem_magnitudes(self.magnitude.limbs(), divisor.magnitude.limbs());
        let quotient = Magnitude::canonicalize(quotient);
        let remainder = Magnitude::canonicalize(remainder);
        let quotient_sign = if Self::magnitude_is_zero(&quotient) || self.sign == divisor.sign {
            Sign::Positive
        }
        else {
            Sign::Negative
        };
        let remainder_sign = if Self::magnitude_is_zero(&remainder) { Sign::Positive } else { self.sign };
        Ok((Self { sign: quotient_sign, magnitude: quotient }, Self { sign: remainder_sign, magnitude: remainder }))
    }

    /// 幅值的 `u128` 视图（忽略符号），装不下返回 `None`。
    fn magnitude_u128(&self) -> Option<u128> {
        match &self.magnitude {
            Magnitude::Zero => Some(0),
            Magnitude::Inline { limbs, count } => match *count {
                1 => Some(limbs[0] as u128),
                2 => Some(limbs[0] as u128 | (limbs[1] as u128) << 64),
                _ => None,
            },
            Magnitude::Heap(_) => None,
        }
    }

    /// 无损回读为 `u64`，负数或越域返回 `None`。
    pub fn to_u64(&self) -> Option<u64> {
        if self.sign == Sign::Negative {
            return None;
        }
        match &self.magnitude {
            Magnitude::Zero => Some(0),
            Magnitude::Inline { limbs, count: 1 } => Some(limbs[0]),
            _ => None,
        }
    }

    /// 无损回读为 `u128`，负数或越域返回 `None`。
    pub fn to_u128(&self) -> Option<u128> {
        if self.sign == Sign::Negative {
            return None;
        }
        self.magnitude_u128()
    }

    /// 无损回读为 `i64`，越域返回 `None`。
    pub fn to_i64(&self) -> Option<i64> {
        if self.is_zero() {
            return Some(0);
        }
        let magnitude = self.magnitude.limbs();
        if magnitude.len() != 1 {
            return None;
        }
        let value = magnitude[0];
        match self.sign {
            Sign::Positive => {
                if value <= i64::MAX as u64 {
                    Some(value as i64)
                }
                else {
                    None
                }
            }
            Sign::Negative => {
                if value <= 1u64 << 63 {
                    Some(value.wrapping_neg() as i64)
                }
                else {
                    None
                }
            }
        }
    }

    /// 无损回读为 `i128`，越域返回 `None`。
    pub fn to_i128(&self) -> Option<i128> {
        let magnitude = self.magnitude_u128()?;
        match self.sign {
            Sign::Positive => {
                if magnitude <= i128::MAX as u128 {
                    Some(magnitude as i128)
                }
                else {
                    None
                }
            }
            Sign::Negative => {
                if magnitude <= 1u128 << 127 {
                    Some(magnitude.wrapping_neg() as i128)
                }
                else {
                    None
                }
            }
        }
    }
}

impl Magnitude {
    fn is_zero(&self) -> bool {
        matches!(self, Self::Zero)
    }
}

impl From<u64> for BigInteger {
    fn from(value: u64) -> Self {
        Self::from_sign_magnitude(Sign::Positive, Magnitude::from_u64(value))
    }
}

impl From<i64> for BigInteger {
    fn from(value: i64) -> Self {
        let magnitude = Magnitude::from_u64(value.unsigned_abs());
        let sign = if value < 0 { Sign::Negative } else { Sign::Positive };
        Self::from_sign_magnitude(sign, magnitude)
    }
}

impl From<u128> for BigInteger {
    fn from(value: u128) -> Self {
        Self::from_sign_magnitude(Sign::Positive, Magnitude::from_u128(value))
    }
}

impl From<i128> for BigInteger {
    fn from(value: i128) -> Self {
        let magnitude = Magnitude::from_u128(value.unsigned_abs());
        let sign = if value < 0 { Sign::Negative } else { Sign::Positive };
        Self::from_sign_magnitude(sign, magnitude)
    }
}

impl Add<&BigInteger> for &BigInteger {
    type Output = BigInteger;

    fn add(self, rhs: &BigInteger) -> BigInteger {
        self.add_ref(rhs)
    }
}

impl Add<BigInteger> for BigInteger {
    type Output = BigInteger;

    fn add(self, rhs: BigInteger) -> BigInteger {
        &self + &rhs
    }
}

impl Sub<&BigInteger> for &BigInteger {
    type Output = BigInteger;

    fn sub(self, rhs: &BigInteger) -> BigInteger {
        self.add_ref(&rhs.neg_ref())
    }
}

impl Sub<BigInteger> for BigInteger {
    type Output = BigInteger;

    fn sub(self, rhs: BigInteger) -> BigInteger {
        &self - &rhs
    }
}

impl Mul<&BigInteger> for &BigInteger {
    type Output = BigInteger;

    fn mul(self, rhs: &BigInteger) -> BigInteger {
        self.mul_ref(rhs)
    }
}

impl Mul<BigInteger> for BigInteger {
    type Output = BigInteger;

    fn mul(self, rhs: BigInteger) -> BigInteger {
        &self * &rhs
    }
}

impl BigInteger {
    fn neg_ref(&self) -> Self {
        Self::from_sign_magnitude(if self.sign == Sign::Negative { Sign::Positive } else { Sign::Negative }, self.magnitude.clone())
    }
}

impl Neg for &BigInteger {
    type Output = BigInteger;

    fn neg(self) -> BigInteger {
        self.neg_ref()
    }
}

impl Neg for BigInteger {
    type Output = BigInteger;

    fn neg(self) -> BigInteger {
        self.neg_ref()
    }
}

impl PartialOrd for BigInteger {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for BigInteger {
    fn cmp(&self, other: &Self) -> Ordering {
        self.compare(other)
    }
}

impl Default for BigInteger {
    fn default() -> Self {
        Self::ZERO
    }
}
