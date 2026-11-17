//! Boost 家族的规则集合能力。
//!
//! ArrayND 的逻辑模型是规则 N 维坐标域与元素的对应关系；连续、dense 或
//! 常驻内存属于具体表示与载体，不是 ArrayND 的普遍前提。本 crate 当前只交付
//! 首个**内存实现**：自有连续行主序存储、静态 rank、运行时 extent、`usize`
//! 范围；图部分提供逻辑能力与内存/CSR 表示。不承载矩阵运算、张量执行图、
//! 图表布局/标签/路由、数据库 schema 或 GPU 布局策略，也不复制标准库已有
//! 的 `Vec`、数组和 `slice` 能力。
//!
//! 三个作者面按“形状放在哪里”分工，三者都是内存实例：
//!
//! - [`ArrayND`]：静态 rank、各轴 extent 在构造时确定（运行时形状）。
//! - [`Array2D`] / [`Array3D`]：形状直接进类型的二维或三维数组（编译期形状），
//!   用 [`array!`] 宏接受元组书写：`type Grid = array!(f32, (3, 3));`、
//!   `type Cube = array!(f32, (2, 3, 4));`。
//!
//! 稀疏、strided/分块、磁盘或设备载体尚未交付，等原因类型真实出现再抽能力；
//! 真正不等长的分段数据（邻接表、变长字节串、独立增长的段落）不因词面
//! 嵌套而强迁这里。二维只读与可写视图（ROI、转置）已提供坐标映射，可写
//! 视图要求独占借用与单射映射；三维视图、reshape 与非 affine 布局留到
//! 后续阶段。

#![doc = include_str!("../readme.md")]
#![deny(missing_docs)]

mod array;
mod array2d;
mod array3d;
mod array_read;
mod array_view;
mod error;
mod graph;

pub use array::ArrayND;
pub use array_read::Array2DRead;
pub use array_view::{ArrayView2D, ArrayViewMut2D};
pub use array2d::Array2D;
pub use array3d::Array3D;
pub use boost_collection_macro::array;
pub use error::{CollectionError, Result};
pub use graph::{CsrGraph, GraphError, GraphRead, GraphResult, MemoryEdge, MemoryGraph, OutgoingEdges, topological_order};
