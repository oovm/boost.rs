use std::collections::BTreeSet;

use language_valkyrie::compiler::semantic::closure::{DeclarationKey, SemanticClosureError};

use super::types::{PlannedSemanticSourceGroup, PlannerError, PlannerResult};
use crate::legion::workspace::source_snapshot::compile_source_snapshot;

/// 入口语义闭包内被引用的声明身份（不含 host provider 物化）。
///
/// 无 `[main]` / `[export]` 入口时返回 `None`：library / 仅 inventory 规划仍按旧行为为全部 contract 选 provider。
pub(crate) fn demanded_declaration_keys(groups: &[PlannedSemanticSourceGroup], arch: &str) -> PlannerResult<Option<BTreeSet<DeclarationKey>>> {
    let compiler_groups = compile_source_snapshot(groups).map_err(|error| PlannerError::CompileFailed { message: error.to_string() })?;
    match language_valkyrie::compiler::semantic::closure::source_group_closure_with_host_bindings(&compiler_groups, &[], arch) {
        Ok(closure) => Ok(Some(closure.demanded_declarations)),
        Err(SemanticClosureError::MissingEntryDeclaration { .. }) => Ok(None),
        Err(error) => Err(PlannerError::CompileFailed { message: semantic_closure_message(&error) }),
    }
}

fn semantic_closure_message(error: &SemanticClosureError) -> String {
    error.to_string()
}

/// 入口组内显式 `[test]` 声明名（按声明顺序）；无 `[test]` 声明时为空。
///
/// 名字来自语义闭包 seed 的声明身份，是测试 run contract 的 `logical_entry` 唯一来源。
/// 入口完全缺失时返回空集，由编译阶段以同一 `MissingEntryDeclaration` 诊断失败，
/// 这里不得伪造测试名或回退 `main`。
pub(crate) fn test_entry_names(groups: &[PlannedSemanticSourceGroup], arch: &str) -> PlannerResult<Vec<String>> {
    let compiler_groups = compile_source_snapshot(groups).map_err(|error| PlannerError::CompileFailed { message: error.to_string() })?;
    match language_valkyrie::compiler::semantic::closure::source_group_closure_with_host_bindings(&compiler_groups, &[], arch) {
        Ok(closure) => Ok(closure.test_entries),
        Err(SemanticClosureError::MissingEntryDeclaration { .. }) => Ok(Vec::new()),
        Err(error) => Err(PlannerError::CompileFailed { message: semantic_closure_message(&error) }),
    }
}
