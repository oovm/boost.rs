//! CPU f32 数学内核：逐元素广播、矩阵乘、激活与归约。
//!
//! 本 crate 不记录梯度，也不区分设备。自动微分在 `dl-autograd` 中组合这些原语，
//! 设备调度在 `hardware.*` 接入后由 DL 计划层调用同一内核合同。

#![warn(missing_docs)]

use dl_tensor::{Tensor, increment_index};
use dl_types::{DlError, Result, broadcast_shape, numel};

/// 按广播规则执行二元逐元素运算。
fn binary(op: &'static str, lhs: &Tensor, rhs: &Tensor, apply: impl Fn(f32, f32) -> f32) -> Result<Tensor> {
    let shape = broadcast_shape(lhs.shape(), rhs.shape(), op)?;
    let left_strides = lhs.broadcast_strides_to(&shape)?;
    let right_strides = rhs.broadcast_strides_to(&shape)?;
    let left = lhs.raw_storage();
    let right = rhs.raw_storage();
    let mut output = Vec::with_capacity(numel(&shape));
    let mut index = vec![0usize; shape.len()];
    for _ in 0..numel(&shape) {
        let left_position = lhs.offset() + dot(&index, &left_strides);
        let right_position = rhs.offset() + dot(&index, &right_strides);
        output.push(apply(left[left_position], right[right_position]));
        increment_index(&mut index, &shape);
    }
    Tensor::from_vec(output, shape)
}

fn dot(index: &[usize], strides: &[usize]) -> usize {
    index.iter().zip(strides).map(|(value, stride)| *value * *stride).sum()
}

/// 逐元素相加，支持广播。
pub fn add(lhs: &Tensor, rhs: &Tensor) -> Result<Tensor> {
    binary("add", lhs, rhs, |left, right| left + right)
}

/// 逐元素相减，支持广播。
pub fn sub(lhs: &Tensor, rhs: &Tensor) -> Result<Tensor> {
    binary("sub", lhs, rhs, |left, right| left - right)
}

/// 逐元素相乘，支持广播。
pub fn mul(lhs: &Tensor, rhs: &Tensor) -> Result<Tensor> {
    binary("mul", lhs, rhs, |left, right| left * right)
}

/// 逐元素相除，支持广播。
pub fn div(lhs: &Tensor, rhs: &Tensor) -> Result<Tensor> {
    binary("div", lhs, rhs, |left, right| left / right)
}

/// 加上标量。
pub fn add_scalar(input: &Tensor, value: f32) -> Result<Tensor> {
    map(input, |element| element + value)
}

/// 乘以标量。
pub fn mul_scalar(input: &Tensor, value: f32) -> Result<Tensor> {
    map(input, |element| element * value)
}

/// 取负。
pub fn neg(input: &Tensor) -> Result<Tensor> {
    map(input, |element| -element)
}

/// ReLU 激活。
pub fn relu(input: &Tensor) -> Result<Tensor> {
    map(input, |element| element.max(0.0))
}

/// 逐元素映射，保持形状。
pub fn map(input: &Tensor, apply: impl Fn(f32) -> f32) -> Result<Tensor> {
    let values = input.to_vec();
    Tensor::from_vec(values.into_iter().map(apply).collect(), input.shape().to_vec())
}

/// 二维矩阵乘，语义为 `[m, k] × [k, n] -> [m, n]`。
pub fn matmul(lhs: &Tensor, rhs: &Tensor) -> Result<Tensor> {
    if lhs.rank() != 2 || rhs.rank() != 2 {
        return Err(DlError::NotContiguous { op: "matmul" });
    }
    let (rows, inner) = (lhs.shape()[0], lhs.shape()[1]);
    let (rhs_inner, columns) = (rhs.shape()[0], rhs.shape()[1]);
    if inner != rhs_inner {
        return Err(DlError::ShapeMismatch { op: "matmul", lhs: lhs.shape().to_vec(), rhs: rhs.shape().to_vec() });
    }
    let left = lhs.to_vec();
    let right = rhs.to_vec();
    let mut output = vec![0.0f32; rows * columns];
    for row in 0..rows {
        for k in 0..inner {
            let value = left[row * inner + k];
            if value == 0.0 {
                continue;
            }
            for column in 0..columns {
                output[row * columns + column] += value * right[k * columns + column];
            }
        }
    }
    Tensor::from_vec(output, [rows, columns])
}

/// 全部元素求和，返回标量张量。
pub fn sum_all(input: &Tensor) -> Result<Tensor> {
    let total: f32 = input.to_vec().iter().sum();
    Tensor::from_vec(vec![total], Vec::<usize>::new())
}

/// 全部元素求平均，返回标量张量。
pub fn mean_all(input: &Tensor) -> Result<Tensor> {
    let count = input.numel();
    if count == 0 {
        return Err(DlError::ZeroDimension { op: "mean_all" });
    }
    let total: f32 = input.to_vec().iter().sum();
    Tensor::from_vec(vec![total / count as f32], Vec::<usize>::new())
}

/// 把广播后的梯度归约回目标形状。
pub fn sum_to_shape(input: &Tensor, target: &[usize]) -> Result<Tensor> {
    if input.shape() == target {
        return Ok(input.clone());
    }
    let merged = broadcast_shape(target, input.shape(), "sum_to_shape")?;
    if merged != input.shape() {
        return Err(DlError::ShapeMismatch { op: "sum_to_shape", lhs: input.shape().to_vec(), rhs: target.to_vec() });
    }
    let mut output = vec![0.0f32; numel(target)];
    let shape = input.shape();
    let mut index = vec![0usize; shape.len()];
    let target_offset = shape.len() - target.len();
    for _ in 0..input.numel() {
        let position = input.linear_index(&index);
        let mut target_index = vec![0usize; target.len()];
        for axis in 0..target.len() {
            target_index[axis] = if target[axis] == 1 { 0 } else { index[target_offset + axis] };
        }
        output[dot(&target_index, &dl_types::contiguous_strides(target))] += input.raw_storage()[position];
        increment_index(&mut index, shape);
    }
    Tensor::from_vec(output, target.to_vec())
}

/// 对最后一维做数值稳定的 softmax，用于推理。
pub fn softmax_last(input: &Tensor) -> Result<Tensor> {
    if input.rank() == 0 {
        return Err(DlError::ZeroDimension { op: "softmax_last" });
    }
    let columns = *input.shape().last().expect("rank > 0");
    let rows = input.numel() / columns;
    let values = input.to_vec();
    let mut output = vec![0.0f32; values.len()];
    for row in 0..rows {
        let slice = &values[row * columns..(row + 1) * columns];
        let maximum = slice.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let mut total = 0.0f32;
        for (column, value) in slice.iter().enumerate() {
            let exponent = (value - maximum).exp();
            output[row * columns + column] = exponent;
            total += exponent;
        }
        for column in 0..columns {
            output[row * columns + column] /= total;
        }
    }
    Tensor::from_vec(output, input.shape().to_vec())
}

/// 多分类 softmax 交叉熵；输入 `[rows, classes]`，目标为同形状 one-hot，返回按行平均的标量。
pub fn softmax_cross_entropy(logits: &Tensor, targets: &Tensor) -> Result<Tensor> {
    let rows = checked_classification_pair("softmax_cross_entropy", logits, targets)?;
    let classes = logits.shape()[1];
    let logits = logits.to_vec();
    let targets = targets.to_vec();
    let mut total = 0.0f32;
    for row in 0..rows {
        let offset = row * classes;
        let slice = &logits[offset..offset + classes];
        let maximum = slice.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let shifted: f32 = slice.iter().map(|value| (value - maximum).exp()).sum();
        let logsumexp = maximum + shifted.ln();
        let mut weighted = 0.0f32;
        for column in 0..classes {
            weighted += targets[offset + column] * slice[column];
        }
        total += logsumexp - weighted;
    }
    Tensor::from_vec(vec![total / rows as f32], Vec::<usize>::new())
}

/// softmax 交叉熵对 logits 的梯度：`(softmax(logits) - targets) / rows`。
pub fn softmax_cross_entropy_backward(logits: &Tensor, targets: &Tensor) -> Result<Tensor> {
    checked_classification_pair("softmax_cross_entropy_backward", logits, targets)?;
    let probabilities = softmax_last(logits)?;
    let rows = logits.shape()[0] as f32;
    let scale = mul_scalar(&probabilities, 1.0 / rows)?;
    let scaled_targets = mul_scalar(targets, 1.0 / rows)?;
    sub(&scale, &scaled_targets)
}

fn checked_classification_pair(op: &'static str, logits: &Tensor, targets: &Tensor) -> Result<usize> {
    if logits.rank() != 2 || targets.rank() != 2 {
        return Err(DlError::Format { detail: format!("{op} expects rank-2 logits and targets") });
    }
    if logits.shape() != targets.shape() {
        return Err(DlError::ShapeMismatch { op, lhs: logits.shape().to_vec(), rhs: targets.shape().to_vec() });
    }
    let rows = logits.shape()[0];
    if rows == 0 {
        return Err(DlError::Format { detail: format!("{op} received zero rows") });
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_add_matches_numpy() {
        let matrix = Tensor::from_vec(vec![1.0, 2.0, 3.0, 4.0], [2, 2]).unwrap();
        let vector = Tensor::from_vec(vec![10.0, 20.0], [2]).unwrap();
        assert_eq!(add(&matrix, &vector).unwrap().to_vec(), vec![11.0, 22.0, 13.0, 24.0]);
    }

    #[test]
    fn matmul_uses_row_major() {
        let left = Tensor::from_vec(vec![1.0, 2.0, 3.0, 4.0], [2, 2]).unwrap();
        let right = Tensor::from_vec(vec![5.0, 6.0, 7.0, 8.0], [2, 2]).unwrap();
        assert_eq!(matmul(&left, &right).unwrap().to_vec(), vec![19.0, 22.0, 43.0, 50.0]);
    }

    #[test]
    fn softmax_cross_entropy_matches_the_analytic_value() {
        let logits = Tensor::from_vec(vec![0.0, 0.0, 0.0, 0.0], [2, 2]).unwrap();
        let targets = Tensor::from_vec(vec![1.0, 0.0, 0.0, 1.0], [2, 2]).unwrap();
        let loss = softmax_cross_entropy(&logits, &targets).unwrap();
        assert!((loss.as_slice().unwrap()[0] - 2.0f32.ln()).abs() < 1e-6);
    }

    #[test]
    fn softmax_cross_entropy_backward_is_probability_minus_target() {
        let logits = Tensor::from_vec(vec![1.0, 0.0, 0.0, 1.0], [2, 2]).unwrap();
        let targets = Tensor::from_vec(vec![1.0, 0.0, 0.0, 1.0], [2, 2]).unwrap();
        let gradient = softmax_cross_entropy_backward(&logits, &targets).unwrap();
        let probabilities = softmax_last(&logits).unwrap().to_vec();
        let expected: Vec<f32> = probabilities.iter().zip(targets.to_vec()).map(|(probability, target)| (probability - target) / 2.0).collect();
        for (actual, expected) in gradient.to_vec().iter().zip(expected) {
            assert!((actual - expected).abs() < 1e-6);
        }
    }

    #[test]
    fn sum_to_shape_folds_broadcast_bias() {
        let gradient = Tensor::from_vec(vec![1.0, 2.0, 3.0, 4.0], [2, 2]).unwrap();
        let folded = sum_to_shape(&gradient, &[2]).unwrap();
        assert_eq!(folded.to_vec(), vec![4.0, 6.0]);
    }
}
