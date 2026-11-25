//! planner 侧条件求值入口：全部语义委托语言侧 `syntax::conditions`。
//!
//! 条件规则（when 匹配、gate fail-closed 聚合）的唯一实现归语言所有；本模块只保留
//! provider 组合便利函数，供 host inventory 选择实现时调用。

use std::collections::BTreeSet;

use compiler_target::CanonicalTarget;
use language_valkyrie::compiler::syntax::conditions::{gate_axis_passes, target_axis_passes};

use super::types::PlannedHostProvider;

pub(crate) fn provider_passes_target_axis(provider: &PlannedHostProvider, target: CanonicalTarget) -> bool {
    target_axis_passes(&provider.when_attributes, target)
}

pub(crate) fn provider_passes_gate_axis(provider: &PlannedHostProvider, enabled_features: Option<&BTreeSet<String>>) -> bool {
    gate_axis_passes(&provider.gate_attributes, enabled_features)
}
