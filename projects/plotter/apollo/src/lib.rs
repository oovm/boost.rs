//! Apollo 是 `document.rs` 中 `plotter` 绘图引擎的下游消费方。
//!
//! 本 crate 不实现任何绘图逻辑，只重新导出上游门面，让仓库内的冒烟测试和
//! 调用方从同一入口接入。真实覆盖矩阵、图像基准和语料比对不放在 Rust 侧。
#![deny(missing_docs)]

pub use plotter::*;
