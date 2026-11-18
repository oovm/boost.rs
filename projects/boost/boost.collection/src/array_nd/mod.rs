//! 数组访问能力与具体内存表示。
//!
//! ArrayND 的逻辑模型是规则 N 维坐标域与元素的对应关系；连续、dense、
//! 编译期形状或常驻内存属于具体表示与载体，不是 ArrayND 的普遍前提。
//! 作者面 [`Array1D`]、[`Array2D`]、[`Array3D`]、[`ArrayND`] 都是按 rank
//! 划分的只读访问能力 trait，与 [`GraphRead`](crate::GraphRead) 同构：
//! 算法只声明需要的能力，不绑定数据住在哪种容器里。

mod array2d;
mod array3d;
mod dense;

pub use array2d::InlineArray2D;
pub use array3d::InlineArray3D;
pub use dense::DenseArrayND;

use crate::{ArrayView2D, ArrayViewMut2D};

/// 规则 N 维坐标域的只读访问能力。
///
/// 只固定形状与坐标读取，不假定存储连续、驻留内存或能返回普通切片。
/// 连续内存实例、编译期形状实例与借用视图分别实现该能力；写入、遍历
/// 与结构变更不捆绑进来，各表示按真实需求提供自己的能力。
pub trait ArrayND<T, const N: usize> {
    /// 各轴长度。
    fn shape(&self) -> [usize; N];

    /// 按逻辑坐标读取；任一分量越界返回 `None`。
    fn get(&self, index: [usize; N]) -> Option<&T>;
}

/// 一维只读访问能力（向量形状的坐标域）；向量运算归 `boost-math`。
pub trait Array1D<T>: ArrayND<T, 1> {}

/// 二维只读访问能力（矩阵形状的坐标域）；矩阵运算归 `boost-math`。
pub trait Array2D<T>: ArrayND<T, 2> {}

/// 三维只读访问能力。
pub trait Array3D<T>: ArrayND<T, 3> {}

impl<T, A: ArrayND<T, 1> + ?Sized> Array1D<T> for A {}
impl<T, A: ArrayND<T, 2> + ?Sized> Array2D<T> for A {}
impl<T, A: ArrayND<T, 3> + ?Sized> Array3D<T> for A {}

impl<T, const N: usize> ArrayND<T, N> for DenseArrayND<T, N> {
    fn shape(&self) -> [usize; N] {
        DenseArrayND::shape(self)
    }

    fn get(&self, index: [usize; N]) -> Option<&T> {
        DenseArrayND::get(self, index)
    }
}

impl<T, const ROWS: usize, const COLUMNS: usize> ArrayND<T, 2> for InlineArray2D<T, ROWS, COLUMNS> {
    fn shape(&self) -> [usize; 2] {
        InlineArray2D::shape(self)
    }

    fn get(&self, index: [usize; 2]) -> Option<&T> {
        InlineArray2D::get(self, index)
    }
}

impl<T, const D1: usize, const D2: usize, const D3: usize> ArrayND<T, 3> for InlineArray3D<T, D1, D2, D3> {
    fn shape(&self) -> [usize; 3] {
        InlineArray3D::shape(self)
    }

    fn get(&self, index: [usize; 3]) -> Option<&T> {
        InlineArray3D::get(self, index)
    }
}

impl<T> ArrayND<T, 2> for ArrayView2D<'_, T> {
    fn shape(&self) -> [usize; 2] {
        ArrayView2D::shape(self)
    }

    fn get(&self, index: [usize; 2]) -> Option<&T> {
        ArrayView2D::get(self, index)
    }
}

impl<T> ArrayND<T, 2> for ArrayViewMut2D<'_, T> {
    fn shape(&self) -> [usize; 2] {
        ArrayViewMut2D::shape(self)
    }

    fn get(&self, index: [usize; 2]) -> Option<&T> {
        ArrayViewMut2D::get(self, index)
    }
}
