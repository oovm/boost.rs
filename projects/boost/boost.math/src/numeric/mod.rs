//! 自有标量数值内核与大整数机器核。
//!
//! 本模块只实现下游真实需要的 `sqrt`、`ln`、`exp`，不链接平台 `libm`，也不
//! 引入外部数学库，因此 `no_std` 目标不会因宿主数学库不同而改变最后几位。
//! 函数体只用 IEEE 浮点运算，精度合同写成相对误差界，而不是跨目标 bitwise
//! 重现承诺：逐次舍入策略一致的会得到相同位，不同舍入策略下只能保证落在
//! 误差界内。
//!
//! 特殊值逐函数冻结：负输入、零、无穷与 `NaN` 按各自文档返回，调用方不应
//! 把 `NaN` 统一改成零，也不需要把这些函数包装成 `Result`。误差界由
//! `tests/numeric.rs` 用独立已知值校验，注释本身不构成证明。
//!
//! 大整数子层（自科学数值塔下沉，依赖方向 science→math 单向）：
//! - [`bigint`]：有符号任意精度整数语义。
//! - [`kernel`]：limb 机器核（portable / x86_64 `KernelTable`）与缓冲。
//! - [`algorithm`]：宽度分段的数学策略规划器。
//! - [`budget`]：分配与增长准入预算。
//! - [`capability`]：Machine / Algorithm / Resource 能力三分。
//! - [`ordered`]：需要 `Eq`/顺序的 `f64` 键语义（`NaN` 相等、`NaN` 最大）。

pub mod algorithm;
pub mod budget;
pub mod capability;
pub mod kernel;
pub mod ordered;

pub mod bigint;

pub use ordered::OrderedF64;

/// `ln 2` 的高位部分。
///
/// 拆分是为了在指数很大时不丢失 `ln 2` 自身的低位：只用单个舍入常量会在
/// 指数项上放大成相对误差。
const LN_2_HI: f64 = core::f64::consts::LN_2;

/// `ln 2` 中超出一位 `f64` 的剩余部分。
const LN_2_LO: f64 = 2.319_046_813_846_299_6e-17;

/// 读取 `f64` 的指数域。
fn exponent_of(value: f64) -> i32 {
    ((value.to_bits() >> 52) & 0x7ff) as i32
}

/// 替换 `f64` 的指数域，保留符号与尾数。
fn with_exponent(value: f64, exponent: i32) -> f64 {
    let sign = value.to_bits() & 0x8000_0000_0000_0000;
    f64::from_bits(sign | ((exponent as u64) << 52) | (value.to_bits() & 0x000f_ffff_ffff_ffff))
}

/// 返回 `value` 的平方根。
///
/// 负数返回 `NaN`；零（含 `-0.0`）、无穷与 `NaN` 原样返回，与标准函数的定义域
/// 一致。次正规数先放大到正规范围再迭代、最后缩回，避免直接在次正规上计算
/// 丢精度。位级初值把指数折半，随后 6 次牛顿迭代每步约翻一倍有效位，结果
/// 收敛到正确舍入值相邻的位置，实测相对误差不超过 1 个 ULP。
pub fn sqrt(value: f64) -> f64 {
    if value.is_nan() || value < 0.0 || value == f64::INFINITY || value == 0.0 {
        return if value < 0.0 { f64::NAN } else { value };
    }

    // 次正规数的指数域无可用值，先放大 2^54，结果再缩回 2^27。
    if exponent_of(value) == 0 {
        const SCALE: f64 = 18_014_398_509_481_984.0; // 2^54
        return sqrt(value * SCALE) / 134_217_728.0; // 2^27
    }

    // 经典的位级初值把指数减半，再用牛顿迭代细化。
    let mut guess = f64::from_bits(0x1ff7_ffff_ffff_ffff + (value.to_bits() >> 1));
    for _ in 0..6 {
        guess = 0.5 * (guess + value / guess);
    }
    guess
}

/// 返回 `value` 的自然对数。
///
/// 零返回负无穷，负数返回 `NaN`，无穷与 `NaN` 原样返回。参数被拆成
/// `m * 2^k`，其中 `m` 落在 `[sqrt(1/2), sqrt(2))`，因此下面的级数收敛很快，
/// 指数部分贡献 `Ln2` 的精确整数倍。次正规数先放大到正规范围。级数截断到
/// 第 10 项，实测相对误差不超过 4 个 ULP。
pub fn ln(value: f64) -> f64 {
    if value.is_nan() || value == f64::INFINITY || value < 0.0 {
        return if value < 0.0 { f64::NAN } else { value };
    }
    if value == 0.0 {
        return f64::NEG_INFINITY;
    }

    let mut scaled = value;
    let mut exponent = 0i32;
    if exponent_of(scaled) == 0 {
        const SCALE: f64 = 18_014_398_509_481_984.0; // 2^54
        scaled *= SCALE;
        exponent = -54;
    }

    let exponent_field = exponent_of(scaled);
    exponent += exponent_field - 1023;
    // 先把尾数归一到 [1, 2)，再归一到 [sqrt(1/2), sqrt(2))，让 `t` 足够小。
    let mut mantissa = with_exponent(scaled, 1023);
    if mantissa > SQRT_2 {
        mantissa *= 0.5;
        exponent += 1;
    }

    let t = (mantissa - 1.0) / (mantissa + 1.0);
    let squared = t * t;
    // `ln(m) = 2 * atanh(t) = 2 * t * (1 + t^2/3 + t^4/5 + ...)`，这里
    // `|t| < 0.18`，所以截断后的余项低于结果的最后一位。
    let mut term = 1.0;
    let mut sum = 0.0;
    let mut denominator = 1.0;
    for _ in 0..10 {
        sum += term / denominator;
        term *= squared;
        denominator += 2.0;
    }
    let adjustment = 2.0 * t * sum;

    (exponent as f64) * LN_2_HI + (exponent as f64) * LN_2_LO + adjustment
}

/// 二的正平方根，用于把对数级数的参数压小。
const SQRT_2: f64 = core::f64::consts::SQRT_2;

/// `ln 2` 的倒数，用于把指数参数拆成二的幂与余项。
const INVERSE_LN_2: f64 = core::f64::consts::LOG2_E;

/// 指数仍可表示的最大参数。
const EXP_OVERFLOW: f64 = 709.782_712_893_384;

/// 指数舍入后仍大于零的最小参数。
const EXP_UNDERFLOW: f64 = -745.133_219_101_941_1;

/// 返回 `e` 的 `value` 次幂。
///
/// 参数被拆成 `k * ln 2` 与落在 `[-ln 2 / 2, ln 2 / 2]` 的余项，因此下面的
/// 泰勒级数几项就收敛，二的幂在最后精确应用。上溢返回无穷，下溢返回零，
/// 与该格式自身的极限一致；`NaN` 与无穷原样返回，负无穷返回零。级数截断到
/// 第 16 项，实测相对误差不超过 4 个 ULP。
pub fn exp(value: f64) -> f64 {
    if value.is_nan() || value == f64::INFINITY {
        return value;
    }
    if value == f64::NEG_INFINITY {
        return 0.0;
    }
    if value > EXP_OVERFLOW {
        return f64::INFINITY;
    }
    if value < EXP_UNDERFLOW {
        return 0.0;
    }

    // 舍入到最近的 `ln 2` 整数倍，让余项在零两侧都足够小。
    let steps = if value >= 0.0 { (value * INVERSE_LN_2 + 0.5) as i32 } else { (value * INVERSE_LN_2 - 0.5) as i32 };
    // 按 `ln 2` 的整数倍归约要相减两个接近的值，所以乘积保持精确：下面用
    // Dekker 拆分补回被舍入掉的残差，余项只带最后一位的误差。`f64::mul_add`
    // 能直接融合，但它属于标准库，`no_std` 目标不能用。
    let steps_as_float = f64::from(steps);
    let product = steps_as_float * LN_2_HI;
    let product_error = product_error(steps_as_float, LN_2_HI, product);
    let remainder = (value - product) - (product_error + steps_as_float * LN_2_LO);

    let mut term = 1.0;
    let mut sum = 1.0;
    for index in 1..=16 {
        term *= remainder / f64::from(index);
        sum += term;
    }
    sum * power_of_two(steps)
}

/// 返回二的整数次幂，利用该格式自身的精确编码。
fn power_of_two(exponent: i32) -> f64 {
    if exponent >= -1022 {
        f64::from_bits(((exponent + 1023) as u64) << 52)
    }
    else if exponent >= -1074 {
        f64::from_bits(1u64 << (exponent + 1074))
    }
    else {
        0.0
    }
}

/// 把 `value` 拆成两段，和精确且各自的有效位不超过 26 位。
///
/// 拆分因子取 `2^27 + 1`，这是 `f64` 的经典选择，要求中间乘积仍在格式范围内；
/// 本模块拆分的每个值都满足这一前提。
fn split(value: f64) -> (f64, f64) {
    const SPLITTER: f64 = 134_217_729.0;
    let scaled = SPLITTER * value;
    let high = scaled - (scaled - value);
    (high, value - high)
}

/// 返回 `left * right` 的舍入结果 `product` 丢掉的残差。
///
/// Dekker 算法用拆分后的两半恢复残差，而不是依赖融合乘加，因此指数参数的
/// 归约在不用标准库的前提下仍能保住最后几位。
fn product_error(left: f64, right: f64, product: f64) -> f64 {
    let (left_high, left_low) = split(left);
    let (right_high, right_low) = split(right);
    ((left_high * right_high - product) + left_high * right_low + left_low * right_high) + left_low * right_low
}
