use crate::{CollectionError, Result};
use core::{
    ops::{Index, IndexMut},
    slice,
};

/// ArrayND 的稠密内存表示：静态 rank、构造时确定 extent 的连续行主序数组。
///
/// [`ArrayND`](crate::ArrayND) 是坐标域与读取能力的抽象，本类型是它的首个
/// 具体表示：自有、连续、行主序、`usize` 范围的 `Vec` 载体，不是 ArrayND
/// 的唯一或默认表示。`N` 是轴数（rank），不是任何一轴的长度。各轴长度在
/// 构造时给出，之后保持不变；元素区是单一连续缓冲，默认行主序，末轴相邻。
/// 允许写入已有坐标，但不提供行级 `push`/`pop`，也不交出能改变缓冲
/// 长度的逃逸入口。编译期形状的内联表示见 [`InlineArray2D`](crate::FixedArray2D)。
///
/// ```
/// use boost_collection::DenseArrayND;
///
/// let grid =
///     DenseArrayND::new([2, 3], |[row, column]| (row * 3 + column) as i32).expect("shape fits");
/// assert_eq!(grid.shape(), [2, 3]);
/// assert_eq!(grid[[1, 2]], 5);
/// assert_eq!(grid.as_slice(), &[0, 1, 2, 3, 4, 5]);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DenseArrayND<T, const N: usize> {
    data: Vec<T>,
    shape: [usize; N],
}

impl<T, const N: usize> DenseArrayND<T, N> {
    /// 按逻辑坐标逐个生成元素，行主序推进。
    ///
    /// 构造前先校验形状；零 extent 先被识别，不会因其它轴的乘积溢出而
    /// 误拒合法空数组。闭包不会被要求 `T: Default` 或 `T: Clone`；
    /// 闭包 panic 时已初始化元素随缓冲恰好释放一次。
    ///
    /// # Errors
    ///
    /// 形状乘积溢出返回 [`CollectionError::ShapeOverflow`]，连续存储分配
    /// 失败返回 [`CollectionError::Allocation`]。
    pub fn new(shape: [usize; N], mut init: impl FnMut([usize; N]) -> T) -> Result<Self> {
        let count = element_count(&shape)?;
        let mut data: Vec<T> = Vec::new();
        data.try_reserve_exact(count).map_err(|_| CollectionError::Allocation)?;
        let mut index = [0usize; N];
        for _ in 0..count {
            data.push(init(index));
            advance(&mut index, &shape);
        }
        Ok(Self { data, shape })
    }

    /// 用同一个值的克隆填充每个坐标。
    ///
    /// # Errors
    ///
    /// 与 [`DenseArrayND::new`] 相同。
    pub fn filled(shape: [usize; N], value: T) -> Result<Self>
    where
        T: Clone,
    {
        Self::new(shape, |_| value.clone())
    }

    /// 接管一维缓冲并校验元素数与形状一致。
    ///
    /// 不复制、不重排元素；失败时缓冲所有权归还调用方，不产生半成品数组。
    ///
    /// # Errors
    ///
    /// 元素数与形状乘积不符返回 [`CollectionError::ElementCountMismatch`]，
    /// 乘积溢出返回 [`CollectionError::ShapeOverflow`]。
    pub fn from_vec(data: Vec<T>, shape: [usize; N]) -> Result<Self> {
        let expected = element_count(&shape)?;
        if expected != data.len() {
            return Err(CollectionError::ElementCountMismatch { shape: expected, elements: data.len() });
        }
        Ok(Self { data, shape })
    }

    /// 各轴长度。任一 extent 为零时数组为空但形状信息仍保留。
    #[must_use]
    pub const fn shape(&self) -> [usize; N] {
        self.shape
    }

    /// 轴数，等于类型参数 `N`。
    #[must_use]
    pub const fn rank(&self) -> usize {
        N
    }

    /// 逻辑元素数。
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// 是否没有元素；形状可能仍然非空（存在零 extent）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// 行主序连续元素视图，只读。
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.data
    }

    /// 行主序连续元素视图，可写。`slice` 不能改变数组长度。
    #[must_use]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.data
    }

    /// 交出一维缓冲，元素顺序保持行主序。
    #[must_use]
    pub fn into_vec(self) -> Vec<T> {
        self.data
    }

    /// 行主序遍历元素。
    pub fn iter(&self) -> slice::Iter<'_, T> {
        self.data.iter()
    }

    /// 按多维坐标读取；任一分量越界返回 `None`。
    #[must_use]
    pub fn get(&self, index: [usize; N]) -> Option<&T> {
        self.offset(index).and_then(|offset| self.data.get(offset))
    }

    /// 按多维坐标写入；任一分量越界返回 `None`。
    pub fn get_mut(&mut self, index: [usize; N]) -> Option<&mut T> {
        self.offset(index).and_then(|offset| self.data.get_mut(offset))
    }

    /// 复用当前缓冲，按行主序线性化后套用新形状。
    ///
    /// 元素顺序保持行主序，不复制、不重新分配；rank 可以改变，但逻辑元素数
    /// 必须与当前元素数一致。改变元素数要走单独的构建或重建动作，不在普通
    /// 访问路径上隐式分配。零 extent 的新形状同样保留完整形状信息。
    ///
    /// # Errors
    ///
    /// 新形状乘积溢出返回 [`CollectionError::ShapeOverflow`]，元素数不一致
    /// 返回 [`CollectionError::ElementCountMismatch`]；失败时原数组被消费，
    /// 不产生半成品。
    pub fn reshaped<const M: usize>(self, shape: [usize; M]) -> Result<DenseArrayND<T, M>> {
        let expected = element_count(&shape)?;
        if expected != self.data.len() {
            return Err(CollectionError::ElementCountMismatch { shape: expected, elements: self.data.len() });
        }
        Ok(DenseArrayND { data: self.data, shape })
    }

    /// 逐轴校验边界后计算行主序偏移，所有算术均为检查运算。
    fn offset(&self, index: [usize; N]) -> Option<usize> {
        let mut offset = 0usize;
        let mut stride = 1usize;
        for axis in (0..N).rev() {
            if index[axis] >= self.shape[axis] {
                return None;
            }
            offset = offset.checked_add(index[axis].checked_mul(stride)?)?;
            stride = stride.checked_mul(self.shape[axis])?;
        }
        Some(offset)
    }
}

impl<T> DenseArrayND<T, 2> {
    /// 一行的只读切片；行号越界返回 `None`。
    #[must_use]
    pub fn row(&self, row: usize) -> Option<&[T]> {
        let columns = self.shape[1];
        let start = row.checked_mul(columns)?;
        let end = start.checked_add(columns)?;
        (row < self.shape[0]).then(|| self.data.get(start..end)).flatten()
    }

    /// 一行的可写切片；行号越界返回 `None`。
    pub fn row_mut(&mut self, row: usize) -> Option<&mut [T]> {
        let columns = self.shape[1];
        let start = row.checked_mul(columns)?;
        let end = start.checked_add(columns)?;
        (row < self.shape[0]).then(|| self.data.get_mut(start..end)).flatten()
    }
}

impl<T, const N: usize> Index<[usize; N]> for DenseArrayND<T, N> {
    type Output = T;

    fn index(&self, index: [usize; N]) -> &Self::Output {
        self.get(index).unwrap_or_else(|| panic!("coordinate {index:?} is outside shape {:?}", self.shape))
    }
}

impl<T, const N: usize> IndexMut<[usize; N]> for DenseArrayND<T, N> {
    fn index_mut(&mut self, index: [usize; N]) -> &mut Self::Output {
        let shape = self.shape;
        self.get_mut(index).unwrap_or_else(|| panic!("coordinate {index:?} is outside shape {shape:?}"))
    }
}

impl<T, const N: usize> IntoIterator for DenseArrayND<T, N> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.into_iter()
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a DenseArrayND<T, N> {
    type Item = &'a T;
    type IntoIter = slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.iter()
    }
}

/// 形状的逻辑元素数。零 extent 优先识别，避免把合法空数组判成溢出。
fn element_count<const N: usize>(shape: &[usize; N]) -> Result<usize> {
    if shape.contains(&0) {
        return Ok(0);
    }
    let mut count = 1usize;
    for extent in shape {
        count = count.checked_mul(*extent).ok_or(CollectionError::ShapeOverflow)?;
    }
    Ok(count)
}

/// 行主序推进坐标：末轴先走，逐轴进位。
fn advance<const N: usize>(index: &mut [usize; N], shape: &[usize; N]) {
    for axis in (0..N).rev() {
        index[axis] += 1;
        if index[axis] < shape[axis] {
            return;
        }
        index[axis] = 0;
    }
}
