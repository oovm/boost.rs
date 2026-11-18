//! 向量与范数。
//!
//! 本模块提供基础向量运算与稳定范数，消费 `&[f64]` 切片，因此固定大小数组
//! （如 `[f64; 3]`）与动态切片（如 `Vec<f64>`）共用同一实现。维度由运行时
//! 检查保证，禁止 zip 截短；长度不匹配返回 `None`，不静默截断。
//!
//! 稳定范数采用 hypot 式缩放：先除以最大绝对值，再乘回，避免中间平方溢出或
//! 下溢。`NaN` 与 `±inf` 按冻结语义处理：任何 `NaN` 返回 `NaN`，任何 `±inf`
//! 返回 `+inf`，全零返回 `0.0`。开方复用 [`crate::numeric::sqrt`] 自有内核，
//! 整体误差随该内核的 `≤1 ULP` 界，`no_std` 目标不链接平台数学库。

/// 返回 `left` 与 `right` 的点积。
///
/// 长度不匹配返回 `None`，不截断。空切片返回 `Some(0.0)`。`NaN` 与无穷按
/// 浮点运算规则传播，不特殊处理。
pub fn dot(left: &[f64], right: &[f64]) -> Option<f64> {
    if left.len() != right.len() {
        return None;
    }
    let mut sum = 0.0;
    for i in 0..left.len() {
        sum += left[i] * right[i];
    }
    Some(sum)
}

/// 返回 `values` 的 `L2` 范数（欧几里得长度）。
///
/// 空切片返回 `0.0`。任何 `NaN` 返回 `NaN`，任何 `±inf` 返回 `+inf`，全零
/// 返回 `0.0`。中间平方不溢出：先除以最大绝对值，求和后再乘回。
pub fn norm(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }

    let mut max_abs = 0.0f64;
    for &v in values {
        let abs_v = v.abs();
        // `NaN.abs() > max_abs` 为假，全 `NaN` 输入会停在 `max_abs == 0.0`，
        // 必须显式返回 `NaN`，不能误报零范数。
        if abs_v.is_nan() {
            return f64::NAN;
        }
        if abs_v > max_abs {
            max_abs = abs_v;
        }
    }

    if max_abs == 0.0 {
        return 0.0;
    }
    if max_abs.is_infinite() {
        return f64::INFINITY;
    }

    let mut sum_sq = 0.0;
    for &v in values {
        let scaled = v / max_abs;
        sum_sq += scaled * scaled;
    }
    // 复用自有 `sqrt` 内核，`no_std` 目标不依赖平台数学库。
    max_abs * crate::numeric::sqrt(sum_sq)
}
