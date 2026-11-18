//! Machine kernel：无所有权、无 GC，只写调用方持有的 limb 缓冲。
//!
//! ```text
//! kernel/
//!   buffer.rs      — LimbBuffer / ScratchWorkspace
//!   error.rs       — KernelError（结构化失败，宿主映射为自己的诊断）
//!   limb.rs        — 默认再导出（值层 / executor 入口）
//!   portable/      — ISA-agnostic 算法实现（语义基线）
//!   table.rs       — KernelTable 绑定
//!   x86_64 / …     — ISA KernelTable
//! ```
//!
//! 算法策略见 [`crate::numeric::algorithm`]；执行预算见
//! [`crate::numeric::budget::ExecutionBudget`]。

pub(crate) mod buffer;
pub mod error;
pub mod limb;
pub(crate) mod portable;
pub(crate) mod table;
pub mod token;

#[cfg(all(target_arch = "x86_64", not(target_family = "wasm")))]
pub(crate) mod x86_64;

pub use buffer::{LimbBuffer, ScratchWorkspace};
pub use table::KernelTable;
pub use token::{ExecutionToken, KernelPreconditions};
