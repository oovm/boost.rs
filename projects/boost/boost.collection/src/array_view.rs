use crate::ArrayND;
use core::ops::Range;

/// 二维数组的只读借用视图。
///
/// 视图记录坐标映射而不是裸指针：底层借用、起始偏移、形状与元素单位
/// strides。裁剪与转置只改变映射，不复制元素；越界坐标返回 `None`。
/// 只有逻辑连续且顺序吻合时才交出普通 `slice`，跨行间隙或转置视图不会
/// 伪造连续缓冲。可变视图需要独占且不重叠的证明，尚未提供。
#[derive(Clone, Copy, Debug)]
pub struct ArrayView2D<'a, T> {
    data: &'a [T],
    offset: usize,
    shape: [usize; 2],
    strides: [usize; 2],
}

impl<'a, T> ArrayView2D<'a, T> {
    /// 校验地址范围后构造视图。
    ///
    /// 这里只接受非负元素 strides；负 stride 与任意用户自定义 stride 尚未
    /// 进入安全 API。
    fn from_parts(data: &'a [T], offset: usize, shape: [usize; 2], strides: [usize; 2]) -> Option<Self> {
        let span = span_of(shape, strides)?;
        let end = offset.checked_add(span)?;
        (end <= data.len()).then_some(Self { data, offset, shape, strides })
    }

    /// 逻辑形状 `[rows, columns]`。
    #[must_use]
    pub const fn shape(&self) -> [usize; 2] {
        self.shape
    }

    /// 两个轴的元素单位 strides，顺序与 [`shape`](Self::shape) 对应。
    #[must_use]
    pub const fn strides(&self) -> [usize; 2] {
        self.strides
    }

    /// 按逻辑坐标读取；越界返回 `None`。
    #[must_use]
    pub fn get(&self, [row, column]: [usize; 2]) -> Option<&'a T> {
        if row >= self.shape[0] || column >= self.shape[1] {
            return None;
        }
        let offset = row.checked_mul(self.strides[0])?.checked_add(column.checked_mul(self.strides[1])?)?;
        self.data.get(self.offset.checked_add(offset)?)
    }

    /// 行列互换的视图；元素位置只改变坐标映射，不重排数据。
    #[must_use]
    pub fn transpose(&self) -> Self {
        Self { data: self.data, offset: self.offset, shape: [self.shape[1], self.shape[0]], strides: [self.strides[1], self.strides[0]] }
    }

    /// 裁剪出矩形子视图；区间越界或起点大于终点返回 `None`。
    #[must_use]
    pub fn rect(&self, rows: Range<usize>, columns: Range<usize>) -> Option<Self> {
        if rows.start > rows.end || rows.end > self.shape[0] || columns.start > columns.end || columns.end > self.shape[1] {
            return None;
        }
        let row_offset = rows.start.checked_mul(self.strides[0])?;
        let column_offset = columns.start.checked_mul(self.strides[1])?;
        let offset = self.offset.checked_add(row_offset)?.checked_add(column_offset)?;
        let shape = [rows.end - rows.start, columns.end - columns.start];
        Self::from_parts(self.data, offset, shape, self.strides)
    }

    /// 逻辑顺序是否正好是连续行主序。
    ///
    /// 单行、单列或空视图不依赖多余的 stride；否则要求列 stride 为 1 且
    /// 行 stride 等于列数。
    #[must_use]
    pub fn is_contiguous(&self) -> bool {
        let [rows, columns] = self.shape;
        if rows == 0 || columns == 0 {
            return true;
        }
        if self.strides[1] != 1 {
            return false;
        }
        rows == 1 || self.strides[0] == columns
    }

    /// 仅当视图连续且顺序吻合时返回普通只读切片。
    #[must_use]
    pub fn as_slice(&self) -> Option<&'a [T]> {
        if !self.is_contiguous() {
            return None;
        }
        let length = self.shape[0].checked_mul(self.shape[1])?;
        self.data.get(self.offset..self.offset.checked_add(length)?)
    }

    /// 按逻辑行主序读取元素，不受物理 strides 顺序影响。
    ///
    /// 非连续视图同样按逻辑坐标顺序产出元素；跨行间隙不会漏读或重排。
    pub fn iter(&self) -> impl Iterator<Item = &'a T> + 'a
    where
        T: 'a,
    {
        let data = self.data;
        let offset = self.offset;
        let [rows, columns] = self.shape;
        let [row_stride, column_stride] = self.strides;
        (0..rows).flat_map(move |row| (0..columns).map(move |column| &data[offset + row * row_stride + column * column_stride]))
    }
}

impl<T> ArrayND<T, 2> {
    /// 整个数组的只读视图。
    #[must_use]
    pub fn view(&self) -> ArrayView2D<'_, T> {
        let [rows, columns] = self.shape();
        ArrayView2D { data: self.as_slice(), offset: 0, shape: [rows, columns], strides: [columns, 1] }
    }

    /// 矩形子区域的只读视图；区间越界返回 `None`。
    #[must_use]
    pub fn view_rect(&self, rows: Range<usize>, columns: Range<usize>) -> Option<ArrayView2D<'_, T>> {
        self.view().rect(rows, columns)
    }
}

/// 视图需要覆盖的最大元素偏移。
///
/// 任一轴为零时没有元素，span 为 0；否则按 `(extent - 1) * stride` 计算，
/// 所有乘法与加法均为检查运算。
fn span_of(shape: [usize; 2], strides: [usize; 2]) -> Option<usize> {
    if shape[0] == 0 || shape[1] == 0 {
        return Some(0);
    }
    let rows = shape[0].checked_sub(1)?.checked_mul(strides[0])?;
    let columns = shape[1].checked_sub(1)?.checked_mul(strides[1])?;
    rows.checked_add(columns)
}
