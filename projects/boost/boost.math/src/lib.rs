//! Boost 家族的数学能力。
//!
//! 数学对象与运算意义归本 crate；集合表示、存储载体与领域语义留在各自
//! owner。三个模块按能力分层，不提供万能 `Scalar`，也不复制 `ArrayND`。
//!
//! - [`bigint`]：有符号任意精度整数语义。内部表示（单 limb、内联双 limb、
//!   堆上多 limb）是带标签的实现细节，公共合同冻结规范零、最短表示、
//!   最高 limb 非零、符号规范与容量上限。核心只需 `alloc`。
//! - [`graph_theory`]：消费 `boost-collection` 的图访问能力的图论算法
//!   （`std` feature）。容器身份、边属性与布局不进入本模块。
//! - [`numeric`]：自有标量数值内核，不链接平台 `libm`，供 `boost-random`
//!   等下游单向消费。
//! - [`vector`]：点积与稳定 `L2` 范数，消费切片，固定数组与动态向量共用
//!   同一实现。
//!
//! 误差、特殊值、溢出与失败语义按模块分别冻结；运算符可用不证明代数定律。

#![doc = include_str!("../readme.md")]
#![deny(missing_docs)]
#![no_std]

extern crate alloc;

// `graph_theory` 的 `HashMap` / `HashSet` 只在 `std` 特性下可用。
#[cfg(feature = "std")]
extern crate std;

pub mod bigint;
#[cfg(feature = "std")]
pub mod graph_theory;
pub mod numeric;
pub mod vector;
