use crate::{ArrayND, ArrayView2D};

/// 二维数组的只读访问能力。
///
/// 算法只声明自己需要的读取与形状，不假定存储连续、驻留内存或能返回
/// 普通切片。连续内存实例与视图分别实现该能力；后续稀疏、分块或磁盘
/// 载体按真实需求增加自己的能力，不把全部操作塞进一个 trait。
pub trait Array2DRead<T> {
    /// 逻辑形状 `[rows, columns]`。
    fn shape(&self) -> [usize; 2];

    /// 按逻辑坐标读取；越界返回 `None`。
    fn get(&self, index: [usize; 2]) -> Option<&T>;
}

impl<T> Array2DRead<T> for ArrayND<T, 2> {
    fn shape(&self) -> [usize; 2] {
        ArrayND::shape(self)
    }

    fn get(&self, index: [usize; 2]) -> Option<&T> {
        ArrayND::get(self, index)
    }
}

impl<T> Array2DRead<T> for ArrayView2D<'_, T> {
    fn shape(&self) -> [usize; 2] {
        ArrayView2D::shape(self)
    }

    fn get(&self, index: [usize; 2]) -> Option<&T> {
        ArrayView2D::get(self, index)
    }
}
