//! Science CAS 执行引擎 — 在 [`science_vm`] **之上**的综合体（怎么规划与准入）。
//!
//! ```text
//! science-types → science-gc → science-numeric → science-ir → science-rewrite → science-vm → science-cas → science
//! ```
//!
//! 本 crate 拥有 Session、编译、M-Graph、solver、改写编排、域分派与 `SCIENCE_*` 诊断，并把 `VmExit`
//! 映射为 `ComputationResult`。**解释执行归属 `science-vm`**（过渡期 SSA `ReferenceExecutor` 仍暂住本 crate
//! 作 host adapter）。不解析方言、不渲染字符串、也不绑定 N-API/WASM。
//!
//! 根级只保留模块树与极薄入口（[`CasEngine`] / [`Session`]）。`science-types` /
//! `science-numeric` / `science-ir` / `science-rewrite` / `science-vm` **不**在此再导出 — 由依赖方或
//! `science` facade 直接引用真相源 crate。

#![deny(missing_docs)]

pub mod api;
pub mod diagnostics;
pub mod domains;
pub mod execution;
pub mod plot;
pub mod reasoning;
pub mod runtime;

pub use api::{CasEngine, EvalOptions, SimplifyOptions};
pub use runtime::{Session, TypedEgraphAdmitReport};
