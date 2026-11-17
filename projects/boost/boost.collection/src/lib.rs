//! Boost 家族的规则多维集合能力。
//!
//! 本 crate 只负责规则多维数组的连续存储、形状、索引与所有权不变量，
//! 不承载矩阵运算、张量执行图、图邻接语义或 GPU 布局策略，也不复制
//! 标准库已有的 `Vec`、数组和 `slice` 能力。
//!
//! 两个作者面按“形状放在哪里”分工：
//!
//! - [`ArrayND`]：静态 rank、各轴 extent 在构造时确定（运行时形状）。
//! - [`Array2D`]：形状直接进类型的二维数组（编译期形状），用
//!   [`array!`] 宏接受元组书写：`type Grid = array!(f32, (3, 3));`。
//!
//! 真正不等长的分段数据（邻接表、变长字节串、独立增长的段落）不因词面
//! 嵌套而强迁这里；视图（裁剪、转置、可变别名）在后续阶段单独建模。

#![doc = include_str!("../readme.md")]
#![deny(missing_docs)]

mod array;
mod array2d;
mod error;

pub use array::ArrayND;
pub use array2d::Array2D;
pub use error::{CollectionError, Result};

/// 编译期固定形状的二维数组类型，接受元组书写。
///
/// 稳定 Rust 的 const 泛型参数不能是元组，类型参数位置无法直接写
/// `Array2D<f32, (3, 3)>`；本宏把元组拆成两个 const 参数，展开结果仍是
/// 普通类型别名，不引入额外包装。
///
/// ```
/// use boost_collection::array;
///
/// type Grid = array!(f32, (3, 3));
///
/// let mut grid = Grid::filled(0.0);
/// grid[[1, 2]] = 5.0;
/// assert_eq!(grid.shape(), [3, 3]);
/// assert_eq!(grid[[1, 2]], 5.0);
///
/// type Inline = array!(u8, 2, 2);
/// assert_eq!(Inline::filled(0).len(), 4);
/// ```
#[macro_export]
macro_rules! array {
    ($element:ty, ($rows:expr, $columns:expr)) => {
        $crate::Array2D<$element, { $rows }, { $columns }>
    };
    ($element:ty, $rows:expr, $columns:expr) => {
        $crate::Array2D<$element, { $rows }, { $columns }>
    };
}
