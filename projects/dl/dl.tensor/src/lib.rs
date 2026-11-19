//! 稠密 f32 张量：唯一存储、形状与步长视图。
//!
//! 本 crate 只负责数据布局，不包含算子、梯度或设备语义。所有视图共享同一份
//! [`std::sync::Arc`] 存储；需要可写连续区的路径必须显式通过 [`Tensor::copy_values`]。
//!
//! 与 `science-ndarray` 的边界：ndarray 是 CAS 数组与外存分块，不是神经网络张量；
//! `dl.*` 的张量布局以本 crate 为唯一权威。

#![deny(missing_docs)]

use std::{fmt, sync::Arc};

use dl_types::{DlError, Result, broadcast_shape, contiguous_strides, numel};

/// 稠密 f32 张量或其只读视图。
#[derive(Clone)]
pub struct Tensor {
    storage: Arc<Vec<f32>>,
    offset: usize,
    shape: Vec<usize>,
    strides: Vec<usize>,
}

impl Tensor {
    /// 用行主序数据构造张量。
    pub fn from_vec(data: Vec<f32>, shape: impl Into<Vec<usize>>) -> Result<Self> {
        let shape = shape.into();
        if numel(&shape) != data.len() {
            return Err(DlError::ElementCount { expected: numel(&shape), actual: data.len() });
        }
        let strides = contiguous_strides(&shape);
        Ok(Self { storage: Arc::new(data), offset: 0, shape, strides })
    }

    /// 从切片复制数据构造张量。
    pub fn from_slice(data: &[f32], shape: impl Into<Vec<usize>>) -> Result<Self> {
        Self::from_vec(data.to_vec(), shape)
    }

    /// 构造全零张量。
    pub fn zeros(shape: impl Into<Vec<usize>>) -> Result<Self> {
        let shape = shape.into();
        Self::from_vec(vec![0.0; numel(&shape)], shape)
    }

    /// 构造填充常量的张量。
    pub fn full(shape: impl Into<Vec<usize>>, value: f32) -> Result<Self> {
        let shape = shape.into();
        Self::from_vec(vec![value; numel(&shape)], shape)
    }

    /// 返回形状。
    pub fn shape(&self) -> &[usize] {
        &self.shape
    }

    /// 返回各轴步长。
    pub fn strides(&self) -> &[usize] {
        &self.strides
    }

    /// 返回元素总数。
    pub fn numel(&self) -> usize {
        numel(&self.shape)
    }

    /// 返回阶数。
    pub fn rank(&self) -> usize {
        self.shape.len()
    }

    /// 返回底层存储起始偏移。
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// 返回完整底层存储，仅供算子实现做步长寻址。
    ///
    /// 调用方必须结合 [`Tensor::offset`]、[`Tensor::strides`] 与形状寻址，不得把该切片
    /// 当作逻辑数据本身。
    pub fn raw_storage(&self) -> &[f32] {
        &self.storage
    }

    /// 判断当前视图是否为行主序连续布局。
    pub fn is_contiguous(&self) -> bool {
        let mut expected = 1usize;
        for axis in (0..self.shape.len()).rev() {
            if self.shape[axis] != 1 && self.strides[axis] != expected {
                return false;
            }
            expected *= self.shape[axis];
        }
        true
    }

    /// 计算给定逻辑下标的底层线性下标。
    pub fn linear_index(&self, index: &[usize]) -> usize {
        debug_assert_eq!(index.len(), self.shape.len());
        let mut position = self.offset;
        for (axis, value) in index.iter().enumerate() {
            position += value * self.strides[axis];
        }
        position
    }

    /// 读取单个逻辑元素。
    pub fn at(&self, index: &[usize]) -> f32 {
        self.storage[self.linear_index(index)]
    }

    /// 按行主序物化为连续 `Vec`。
    pub fn to_vec(&self) -> Vec<f32> {
        if self.is_contiguous() {
            let start = self.offset;
            return self.storage[start..start + self.numel()].to_vec();
        }
        let mut output = Vec::with_capacity(self.numel());
        self.for_each_element(|_, value| output.push(value));
        output
    }

    /// 以行主序遍历逻辑元素。
    pub fn for_each_element<F: FnMut(&[usize], f32)>(&self, mut visit: F) {
        let mut index = vec![0usize; self.rank()];
        for _ in 0..self.numel() {
            visit(&index, self.at(&index));
            increment_index(&mut index, &self.shape);
        }
    }

    /// 返回连续视图的底层切片。
    pub fn as_slice(&self) -> Result<&[f32]> {
        if !self.is_contiguous() {
            return Err(DlError::NotContiguous { op: "as_slice" });
        }
        Ok(&self.storage[self.offset..self.offset + self.numel()])
    }

    /// 原地覆写全部元素；只允许连续视图。
    pub fn copy_values(&mut self, values: &[f32]) -> Result<()> {
        if values.len() != self.numel() {
            return Err(DlError::ElementCount { expected: self.numel(), actual: values.len() });
        }
        if !self.is_contiguous() {
            return Err(DlError::NotContiguous { op: "copy_values" });
        }
        let start = self.offset;
        let end = start + self.numel();
        let storage = Arc::make_mut(&mut self.storage);
        storage[start..end].copy_from_slice(values);
        Ok(())
    }

    /// 返回改变形状后的视图；要求当前视图连续且元素数一致。
    pub fn reshape(&self, shape: impl Into<Vec<usize>>) -> Result<Self> {
        let shape = shape.into();
        if numel(&shape) != self.numel() {
            return Err(DlError::ElementCount { expected: self.numel(), actual: numel(&shape) });
        }
        if !self.is_contiguous() {
            return Err(DlError::NotContiguous { op: "reshape" });
        }
        Ok(Self { storage: Arc::clone(&self.storage), offset: self.offset, strides: contiguous_strides(&shape), shape })
    }

    /// 交换两个轴，返回视图。
    pub fn transpose(&self, first: usize, second: usize) -> Result<Self> {
        let rank = self.rank();
        if first >= rank {
            return Err(DlError::InvalidAxis { axis: first, rank });
        }
        if second >= rank {
            return Err(DlError::InvalidAxis { axis: second, rank });
        }
        let mut shape = self.shape.clone();
        let mut strides = self.strides.clone();
        shape.swap(first, second);
        strides.swap(first, second);
        Ok(Self { storage: Arc::clone(&self.storage), offset: self.offset, shape, strides })
    }

    /// 按 NumPy 规则扩展到目标形状，返回零步长视图。
    pub fn broadcast_to(&self, target: &[usize]) -> Result<Self> {
        let strides = self.broadcast_strides_to(target)?;
        Ok(Self { storage: Arc::clone(&self.storage), offset: self.offset, shape: target.to_vec(), strides })
    }

    /// 计算对目标广播形状的步长；被扩展的维步长为 0。
    pub fn broadcast_strides_to(&self, target: &[usize]) -> Result<Vec<usize>> {
        let expected = broadcast_shape(&self.shape, target, "broadcast_to")?;
        if expected != target {
            return Err(DlError::ShapeMismatch { op: "broadcast_to", lhs: self.shape.clone(), rhs: target.to_vec() });
        }
        let mut strides = vec![0usize; target.len()];
        let offset = target.len() - self.shape.len();
        for axis in 0..self.shape.len() {
            let target_axis = offset + axis;
            strides[target_axis] = if self.shape[axis] == 1 && target[target_axis] > 1 { 0 } else { self.strides[axis] };
        }
        Ok(strides)
    }

    /// 截取二维张量的连续行区间 `[start, end)`。
    pub fn rows(&self, start: usize, end: usize) -> Result<Self> {
        if self.rank() != 2 {
            return Err(DlError::NotContiguous { op: "rows" });
        }
        if !self.is_contiguous() {
            return Err(DlError::NotContiguous { op: "rows" });
        }
        if start > end || end > self.shape[0] {
            return Err(DlError::ElementCount { expected: self.shape[0], actual: end });
        }
        let columns = self.shape[1];
        Ok(Self {
            storage: Arc::clone(&self.storage),
            offset: self.offset + start * self.strides[0],
            shape: vec![end - start, columns],
            strides: vec![self.strides[0], self.strides[1]],
        })
    }
}

impl fmt::Debug for Tensor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Tensor").field("shape", &self.shape).field("strides", &self.strides).field("values", &self.to_vec()).finish()
    }
}

/// 行主序递增下标；到达末尾后归零。
pub fn increment_index(index: &mut [usize], shape: &[usize]) {
    for axis in (0..index.len()).rev() {
        index[axis] += 1;
        if index[axis] < shape[axis] {
            return;
        }
        index[axis] = 0;
    }
}

/// 从扁平下标还原逻辑下标。
pub fn unravel_index(mut flat: usize, shape: &[usize]) -> Vec<usize> {
    let mut index = vec![0usize; shape.len()];
    for axis in (0..shape.len()).rev() {
        index[axis] = flat % shape[axis];
        flat /= shape[axis];
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reshape_and_transpose_share_storage() {
        let tensor = Tensor::from_vec(vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0], [2, 3]).unwrap();
        let reshaped = tensor.reshape([3, 2]).unwrap();
        assert_eq!(reshaped.shape(), &[3, 2]);
        assert_eq!(reshaped.to_vec(), vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        let transposed = tensor.transpose(0, 1).unwrap();
        assert_eq!(transposed.shape(), &[3, 2]);
        assert!(!transposed.is_contiguous());
        assert_eq!(transposed.to_vec(), vec![1.0, 4.0, 2.0, 5.0, 3.0, 6.0]);
    }

    #[test]
    fn broadcast_repeats_rows() {
        let bias = Tensor::from_vec(vec![10.0, 20.0, 30.0], [3]).unwrap();
        let expanded = bias.broadcast_to(&[2, 3]).unwrap();
        assert_eq!(expanded.to_vec(), vec![10.0, 20.0, 30.0, 10.0, 20.0, 30.0]);
        assert_eq!(expanded.strides(), &[0, 1]);
    }
}
