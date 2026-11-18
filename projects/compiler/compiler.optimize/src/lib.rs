//! 语言无关的 `E-Graph` 重写底座：结构化项、模式匹配、饱和与成本抽取。
//!
//! 本 crate 只拥有可复用的重写机制：hashcons `E-Graph`、union-find、congruence rebuild、
//! 模式重写饱和、由消费者显式提交的常量折叠解释与基于成本模型的代表元抽取。
//! 语言语义、操作命名空间、目标投影规划与程序边界模型都不在此处，仍由对应语言或产品持有。
//!
//! 分层：[`contract`]（最小中立名称合同） · [`term`] · [`pattern`] · [`egraph`] · [`extract`] · [`theory`]。

mod contract;
mod egraph;
mod extract;
mod pattern;
mod term;
mod theory;

pub use contract::{CapabilityTag, Identifier, QualifiedName};
pub use egraph::{ConstantInterpreter, EClassId, EGraphHost, EGraphStats, StructuredRewrite};
pub use extract::{ExtractCost, extract_class};
pub use pattern::{EGraphMatchView, Substitution, TermPattern, atom_pattern, instantiate_pattern, match_pattern, parse_pattern};
pub use term::{Term, parse_term, qualified_from_dotted};
pub use theory::{
    RewriteEquation, RewritePhase, TermRewrite, filter_equations, filter_term_rewrites, structured_rewrite_from_equation,
    structured_rewrite_from_term_rewrite,
};
