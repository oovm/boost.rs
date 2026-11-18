//! 重写理论的中立数据形状：阶段、等式与结构化项级重写。
//!
//! 底座不内置任何语言的操作命名空间；具体 rewrite theory（例如算术恒等式集合）
//! 由消费者构造后经 [`EGraphHost::saturate`](crate::EGraphHost::saturate) 消费。

use crate::{
    contract::{CapabilityTag, Identifier, QualifiedName},
    egraph::StructuredRewrite,
    pattern::{TermPattern, atom_pattern},
};

/// `E-Graph` 重写所在阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewritePhase {
    /// 规范化阶段。
    Normalize,
    /// 等价饱和阶段。
    Saturate,
    /// 为目标投影做收口的阶段。
    PreProjection,
}

/// 单条可进入 `E-Graph` 的 flat 等式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteEquation {
    /// 等式左侧操作。
    pub left: QualifiedName,
    /// 等式右侧操作。
    pub right: QualifiedName,
    /// 所属阶段。
    pub phase: RewritePhase,
    /// 触发等式所需能力。
    pub required_capabilities: Vec<CapabilityTag>,
}

/// 结构化项级重写。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermRewrite {
    /// 规则名。
    pub name: Identifier,
    /// 左侧模式。
    pub left: TermPattern,
    /// 右侧模式。
    pub right: TermPattern,
    /// 所属阶段。
    pub phase: RewritePhase,
    /// 触发规则所需能力。
    pub required_capabilities: Vec<CapabilityTag>,
}

impl TermRewrite {
    /// 构造一条结构化重写。
    pub fn new(name: impl Into<String>, left: TermPattern, right: TermPattern, phase: RewritePhase) -> Self {
        Self { name: Identifier::new(&name.into()), left, right, phase, required_capabilities: Vec::new() }
    }

    /// 附加所需能力。
    pub fn with_capabilities(mut self, capabilities: Vec<CapabilityTag>) -> Self {
        self.required_capabilities = capabilities;
        self
    }
}

/// 将 flat 等式转换为原子级结构化重写。
pub fn structured_rewrite_from_equation(name: impl Into<String>, left: QualifiedName, right: QualifiedName) -> StructuredRewrite {
    StructuredRewrite { name: Identifier::new(&name.into()), left: atom_pattern(left), right: atom_pattern(right) }
}

/// 将 [`TermRewrite`] 转为 [`StructuredRewrite`]。
pub fn structured_rewrite_from_term_rewrite(rewrite: &TermRewrite) -> StructuredRewrite {
    StructuredRewrite { name: rewrite.name.clone(), left: rewrite.left.clone(), right: rewrite.right.clone() }
}

/// 按阶段与能力过滤结构化重写。
pub fn filter_term_rewrites<'a>(
    rewrites: impl IntoIterator<Item = &'a TermRewrite>,
    phase: RewritePhase,
    capabilities: &[CapabilityTag],
) -> Vec<StructuredRewrite> {
    rewrites
        .into_iter()
        .filter(|rewrite| rewrite.phase == phase)
        .filter(|rewrite| rewrite.required_capabilities.iter().all(|capability| capabilities.iter().any(|provided| provided == capability)))
        .map(structured_rewrite_from_term_rewrite)
        .collect()
}

/// 按阶段与能力过滤 flat 等式。
pub fn filter_equations<'a>(
    equations: impl IntoIterator<Item = &'a RewriteEquation>,
    phase: RewritePhase,
    capabilities: &[CapabilityTag],
) -> Vec<(QualifiedName, QualifiedName)> {
    equations
        .into_iter()
        .filter(|equation| equation.phase == phase)
        .filter(|equation| equation.required_capabilities.iter().all(|capability| capabilities.iter().any(|provided| provided == capability)))
        .map(|equation| (equation.left.clone(), equation.right.clone()))
        .collect()
}
