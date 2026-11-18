use core::{
    array,
    ops::{Index, IndexMut},
};

/// 编译期固定形状的二维内联内存表示。
///
/// [`Array2D`](crate::Array2D) 抽象只规定规则二维坐标域与读取能力；本类型
/// 是它的一种具体表示：自有、连续行主序、无堆元数据的固定尺寸载体。
/// 形状直接进类型：`InlineArray2D<f32, 3, 3>` 是 3 行 3 列，存储为
/// `[[T; COLUMNS]; ROWS]`。稳定 Rust 的 const 泛型参数不能是元组，所以
/// `InlineArray2D<f32, (3, 3)>` 不成立；需要元组书写时用 [`array!`](crate::array) 宏：
/// `type Grid = array!(f32, (3, 3));`。
///
/// 形状运行时才确定时用 [`DenseArrayND<T, 2>`](crate::DenseArrayND)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineArray2D<T, const ROWS: usize, const COLUMNS: usize> {
    cells: [[T; COLUMNS]; ROWS],
}

impl<T, const ROWS: usize, const COLUMNS: usize> InlineArray2D<T, ROWS, COLUMNS> {
    /// 按逻辑坐标 `[row, column]` 逐个生成元素，行主序推进。
    ///
    /// 生成直接写入最终存储，不经过逐行临时集合；闭包 panic 时已初始化
    /// 元素按标准数组的初始化守卫恰好释放一次。
    pub fn new(mut init: impl FnMut([usize; 2]) -> T) -> Self {
        Self { cells: array::from_fn(|row| array::from_fn(|column| init([row, column]))) }
    }

    /// 用同一个值的克隆填充每个坐标。
    pub fn filled(value: T) -> Self
    where
        T: Clone,
    {
        Self::new(|_| value.clone())
    }

    /// 各轴长度，等于 `[ROWS, COLUMNS]`。
    #[must_use]
    pub const fn shape(&self) -> [usize; 2] {
        [ROWS, COLUMNS]
    }

    /// 行数。
    #[must_use]
    pub const fn rows(&self) -> usize {
        ROWS
    }

    /// 列数。
    #[must_use]
    pub const fn columns(&self) -> usize {
        COLUMNS
    }

    /// 逻辑元素数。
    #[must_use]
    pub const fn len(&self) -> usize {
        ROWS * COLUMNS
    }

    /// 是否没有元素（任一轴长度为 0）。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        ROWS * COLUMNS == 0
    }

    /// 按坐标读取；越界返回 `None`。
    #[must_use]
    pub fn get(&self, [row, column]: [usize; 2]) -> Option<&T> {
        self.cells.get(row).and_then(|cells| cells.get(column))
    }

    /// 按坐标写入；越界返回 `None`。
    pub fn get_mut(&mut self, [row, column]: [usize; 2]) -> Option<&mut T> {
        self.cells.get_mut(row).and_then(|cells| cells.get_mut(column))
    }

    /// 一行的只读切片；行号越界返回 `None`。
    #[must_use]
    pub fn row(&self, row: usize) -> Option<&[T; COLUMNS]> {
        self.cells.get(row)
    }

    /// 一行的可写切片；行号越界返回 `None`。
    pub fn row_mut(&mut self, row: usize) -> Option<&mut [T; COLUMNS]> {
        self.cells.get_mut(row)
    }

    /// 行主序遍历元素。
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.cells.iter().flatten()
    }

    /// 底层行主序存储。
    #[must_use]
    pub const fn as_inner(&self) -> &[[T; COLUMNS]; ROWS] {
        &self.cells
    }

    /// 交出行主序存储。
    #[must_use]
    pub fn into_inner(self) -> [[T; COLUMNS]; ROWS] {
        self.cells
    }
}

impl<T, const ROWS: usize, const COLUMNS: usize> From<[[T; COLUMNS]; ROWS]> for InlineArray2D<T, ROWS, COLUMNS> {
    fn from(cells: [[T; COLUMNS]; ROWS]) -> Self {
        Self { cells }
    }
}

impl<T, const ROWS: usize, const COLUMNS: usize> Index<[usize; 2]> for InlineArray2D<T, ROWS, COLUMNS> {
    type Output = T;

    fn index(&self, [row, column]: [usize; 2]) -> &Self::Output {
        &self.cells[row][column]
    }
}

impl<T, const ROWS: usize, const COLUMNS: usize> IndexMut<[usize; 2]> for InlineArray2D<T, ROWS, COLUMNS> {
    fn index_mut(&mut self, [row, column]: [usize; 2]) -> &mut Self::Output {
        &mut self.cells[row][column]
    }
}
