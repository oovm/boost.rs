//! 语言无关的 `E-Graph` 重写底座：结构化项、模式匹配、饱和、成本抽取与优化会话。
//!
//! 本 crate 只拥有可复用的重写机制：hashcons `E-Graph`、union-find、congruence rebuild、
//! 模式重写饱和、由消费者显式提交的常量折叠解释与基于成本模型的代表元抽取，
//! 以及把上述机制组装的优化会话与目标投影计划。
//!
//! 语言语义与操作命名空间由语言提交：底座不内置任何语言的操作名，
//! 默认的 `core.*` 代数恒等式作为可替换的内建理论提供。
//!
//! 分层：[`egraph`] · [`term`] · [`pattern`] · [`extract`] · [`theory`] · [`session`] · [`core_theory`]。
#![warn(missing_docs)]

mod core_theory;
mod egraph;
mod extract;
mod pattern;
mod session;
mod term;
mod theory;

pub use compiler_types::{
    CapabilityTag, EntryReturnProjection, FutamuraProjectionFamily, HostProjectionBoundary, Identifier, ProjectionPlan, ProjectionPolicy,
    QualifiedName, ReferenceManagement,
};
pub use core_theory::{CoreConstantInterpreter, builtin_core_rewrite_theory, builtin_core_term_rewrites, core_lit_name};
pub use egraph::{ConstantInterpreter, EClassId, EGraphHost, EGraphStats, StructuredRewrite};
pub use extract::{ExtractCost, extract_class};
pub use pattern::{EGraphMatchView, Substitution, TermPattern, atom_pattern, instantiate_pattern, match_pattern, parse_pattern};
pub use session::{
    AlgebraicTerm, EGraphSnapshot, ObjectAlgebraicBuilder, ObjectAlgebraicDimension, ObjectAlgebraicInterpreter, ObjectAlgebraicProgram,
    OptimizationRequest, OptimizationResult, OptimizationSession, RewriteRule, RewriteTheory, TheoryBundle,
};
pub use term::{Term, parse_term, qualified_from_dotted};
pub use theory::{
    RewriteEquation, RewritePhase, TermRewrite, filter_equations, filter_term_rewrites, structured_rewrite_from_equation,
    structured_rewrite_from_term_rewrite,
};
