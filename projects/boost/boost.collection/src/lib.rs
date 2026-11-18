//! Boost 家族的规则集合能力。
//!
//! ArrayND 的逻辑模型是规则 N 维坐标域与元素的对应关系；连续、dense、
//! 编译期形状或常驻内存属于具体表示与载体，不是 ArrayND 的普遍前提。
//! 作者面 [`Array1D`]、[`Array2D`]、[`Array3D`]、[`ArrayND`] 是按 rank
//! 划分的只读访问能力 trait，与 [`GraphRead`] 同构：算法只声明需要的
//! 能力，不绑定数据住在哪种容器里。图部分提供逻辑能力与内存/CSR 表示。
//! 不承载矩阵运算、张量执行图、图表布局/标签/路由、数据库 schema 或
//! GPU 布局策略，也不复制标准库已有的 `Vec`、数组和 `slice` 能力。
//!
//! 当前交付的具体内存表示：
//!
//! - [`DenseArrayND`]：静态 rank、各轴 extent 在构造时确定（运行时形状）、
//!   连续行主序 `Vec` 载体。
//! - [`InlineArray2D`] / [`InlineArray3D`]：形状直接进类型的编译期内联
//!   载体，用 [`array!`] 宏接受元组书写：`type Grid = array!(f32, (3, 3));`、
//!   `type Cube = array!(f32, (2, 3, 4));`。
//! - [`ArrayView2D`] / [`ArrayViewMut2D`]：借用 strided 的二维只读与可写
//!   视图（ROI、转置），实现 [`Array2D`] 读取能力。
//!
//! 稀疏、分块、磁盘或设备载体尚未交付，等原因类型真实出现再增加能力，
//! 不预先建设 God trait；真正不等长的分段数据（邻接表、变长字节串、
//! 独立增长的段落）不因词面嵌套而强迁这里。可写视图要求独占借用与
//! 单射映射；`DenseArrayND::reshaped` 复用缓冲并保持行主序线性化。
//! 三维视图与非 affine 布局留到后续阶段。

#![doc = include_str!("../readme.md")]
#![deny(missing_docs)]

mod array_nd;
mod array_view;
mod error;
mod graph;

pub use array_nd::{Array1D, Array2D, Array3D, ArrayND, DenseArrayND, InlineArray2D, InlineArray3D};
pub use array_view::{ArrayView2D, ArrayViewMut2D};
pub use boost_collection_macro::array;
pub use error::{CollectionError, Result};
pub use graph::{CsrGraph, GraphError, GraphRead, GraphResult, IncomingEdges, MemoryEdge, MemoryGraph, OutgoingEdges};
