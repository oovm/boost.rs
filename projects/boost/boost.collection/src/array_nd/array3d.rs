use core::{
    array,
    ops::{Index, IndexMut},
};

/// 编译期固定形状的三维内联内存表示。
///
/// [`Array3D`](crate::Array3D) 抽象只规定规则三维坐标域与读取能力；本类型
/// 是它的一种具体表示：自有、连续行主序、无堆元数据的固定尺寸载体。
/// 形状直接进类型：`InlineArray3D<f32, 2, 3, 4>` 是 2×3×4，存储为
/// `[[[T; D3]; D2]; D1]`，第一轴最外层、末轴相邻。需要元组书写时用
/// [`array!`](crate::array) 宏：`type Cube = array!(f32, (2, 3, 4));`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineArray3D<T, const D1: usize, const D2: usize, const D3: usize> {
    cells: [[[T; D3]; D2]; D1],
}

impl<T, const D1: usize, const D2: usize, const D3: usize> InlineArray3D<T, D1, D2, D3> {
    /// 按逻辑坐标 `[first, second, third]` 逐个生成元素，行主序推进。
    ///
    /// 生成直接写入最终存储，不经过逐层临时集合；闭包 panic 时已初始化
    /// 元素按标准数组的初始化守卫恰好释放一次。
    pub fn new(mut init: impl FnMut([usize; 3]) -> T) -> Self {
        Self { cells: array::from_fn(|first| array::from_fn(|second| array::from_fn(|third| init([first, second, third])))) }
    }

    /// 用同一个值的克隆填充每个坐标。
    pub fn filled(value: T) -> Self
    where
        T: Clone,
    {
        Self::new(|_| value.clone())
    }

    /// 各轴长度，等于 `[D1, D2, D3]`。
    #[must_use]
    pub const fn shape(&self) -> [usize; 3] {
        [D1, D2, D3]
    }

    /// 轴数，固定为 3。
    #[must_use]
    pub const fn rank(&self) -> usize {
        3
    }

    /// 逻辑元素数。
    #[must_use]
    pub const fn len(&self) -> usize {
        D1 * D2 * D3
    }

    /// 是否没有元素（任一轴长度为 0）。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        D1 * D2 * D3 == 0
    }

    /// 按坐标读取；越界返回 `None`。
    #[must_use]
    pub fn get(&self, [first, second, third]: [usize; 3]) -> Option<&T> {
        self.cells.get(first).and_then(|plane| plane.get(second)).and_then(|row| row.get(third))
    }

    /// 按坐标写入；越界返回 `None`。
    pub fn get_mut(&mut self, [first, second, third]: [usize; 3]) -> Option<&mut T> {
        self.cells.get_mut(first).and_then(|plane| plane.get_mut(second)).and_then(|row| row.get_mut(third))
    }

    /// 一个 `D2 × D3` 面的只读切片；越界返回 `None`。
    #[must_use]
    pub fn plane(&self, first: usize) -> Option<&[[T; D3]; D2]> {
        self.cells.get(first)
    }

    /// 一个 `D2 × D3` 面的可写切片；越界返回 `None`。
    pub fn plane_mut(&mut self, first: usize) -> Option<&mut [[T; D3]; D2]> {
        self.cells.get_mut(first)
    }

    /// 行主序遍历元素，末轴变化最快。
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.cells.iter().flatten().flatten()
    }

    /// 底层行主序存储。
    #[must_use]
    pub const fn as_inner(&self) -> &[[[T; D3]; D2]; D1] {
        &self.cells
    }

    /// 交出行主序存储。
    #[must_use]
    pub fn into_inner(self) -> [[[T; D3]; D2]; D1] {
        self.cells
    }
}

impl<T, const D1: usize, const D2: usize, const D3: usize> From<[[[T; D3]; D2]; D1]> for InlineArray3D<T, D1, D2, D3> {
    fn from(cells: [[[T; D3]; D2]; D1]) -> Self {
        Self { cells }
    }
}

impl<T, const D1: usize, const D2: usize, const D3: usize> Index<[usize; 3]> for InlineArray3D<T, D1, D2, D3> {
    type Output = T;

    fn index(&self, [first, second, third]: [usize; 3]) -> &Self::Output {
        &self.cells[first][second][third]
    }
}

impl<T, const D1: usize, const D2: usize, const D3: usize> IndexMut<[usize; 3]> for InlineArray3D<T, D1, D2, D3> {
    fn index_mut(&mut self, [first, second, third]: [usize; 3]) -> &mut Self::Output {
        &mut self.cells[first][second][third]
    }
}
