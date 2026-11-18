//! Science 公共 Rust 门面 — 对执行引擎与底层合同 crate 的稳定入口。
//!
//! ```text
//! 依赖链：`science-types` → `science-numeric` → `science-ir` → `science-rewrite` → `science-cas` → `science`
//! ```
//!
//! 本 crate **不**自行实现求值或 Session。每个公开模块只从**拥有方**再导出一次：
//! - 引擎能力 → [`science_cas`] 对应模块
//! - IR / types / numeric / rewrite → 各自真相源 crate（禁止经 `science-cas` 别名转手）
//!
//! 根级只保留 [`CasEngine`] / [`Session`]。

#![deny(missing_docs)]

/// 引擎句柄与选项。
pub use science_cas::api;
/// 诊断构造与表达式摘要。
pub use science_cas::diagnostics;
/// 数学领域 providers 与顶层域分派。
pub use science_cas::domains;
/// `ExecutionIR` 编译、reference / JIT backends 与 builtins。
pub use science_cas::execution;
/// 采样合同（非方言 render）。
pub use science_cas::plot;
/// 改写编排、M-Graph 与 solver 调度。
pub use science_cas::reasoning;
/// Session、语义表、值与对象。
pub use science_cas::runtime;

/// ScienceIR（真相源：`science-ir`）。
pub use science_ir as ir;
/// 数值塔（真相源：`science-numeric`）。
pub use science_numeric as numeric;
/// 改写器（真相源：`science-rewrite`）。
pub use science_rewrite as rewriter;
/// 共享身份 / 诊断 / wire 合同（真相源：`science-types`）。
pub use science_types as types;

pub use science_cas::{CasEngine, EvalOptions, Session, SimplifyOptions};
