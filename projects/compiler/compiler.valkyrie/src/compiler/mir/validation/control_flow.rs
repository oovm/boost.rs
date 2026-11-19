//! 可达块、跳转目标与跳转形状校验。

use super::{diagnostics::display_type, errors::MirValidationError, type_inference::infer_operand_static_type};
use crate::compiler::mir::{MirBlock, MirBlockRef, MirFunction, MirOperand, MirTerminator};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn collect_reachable_blocks(function: &MirFunction) -> BTreeSet<MirBlockRef> {
    // 可达性本身交给共享图论核：块 id 投影为借用视图，终点缺失的跳转边按旧
    // 语义忽略；入口即使不在 blocks 里也仍是顶点，与旧的 worklist 行为一致。
    let block_map: BTreeMap<MirBlockRef, &MirBlock> = function.blocks.iter().map(|block| (block.id, block)).collect();
    let mut view = boost_collection::GraphView::new(true);
    view.add_vertex(&function.entry);
    for block in &function.blocks {
        view.add_vertex(&block.id);
    }
    for block in &function.blocks {
        let targets: Vec<MirBlockRef> = match &block.terminator {
            MirTerminator::Jump { target, .. } => vec![*target],
            MirTerminator::Branch { then_target, else_target, .. } => vec![*then_target, *else_target],
            MirTerminator::PerformEffect { resume_target, .. } => vec![*resume_target],
            MirTerminator::StateDispatch { cases, default_target, .. } => {
                cases.iter().map(|(_, target)| *target).chain(std::iter::once(*default_target)).collect()
            }
            MirTerminator::YieldToRuntime { .. } | MirTerminator::Return { .. } | MirTerminator::Unreachable => Vec::new(),
        };
        for target in targets {
            if let Some(target_block) = block_map.get(&target) {
                view.add_edge(&block.id, &target_block.id);
            }
        }
    }
    let entry = &function.entry;
    boost_math::graph_theory::reachable_from(&view, &entry).expect("the entry is a vertex of the view").into_iter().copied().collect()
}

pub(super) fn validate_jump_target(
    function: &MirFunction,
    block: &MirBlock,
    target: MirBlockRef,
    arguments: &[MirOperand],
    blocks: &BTreeMap<MirBlockRef, usize>,
    block_map: &BTreeMap<MirBlockRef, &MirBlock>,
) -> Result<(), MirValidationError> {
    validate_jump_shape(&function.display_symbol, block.label.as_str(), target, arguments.len(), blocks)?;
    let Some(target_block) = block_map.get(&target)
    else {
        return Ok(());
    };
    for (index, (argument, parameter)) in arguments.iter().zip(target_block.parameters.iter()).enumerate() {
        let Some(argument_type) = infer_operand_static_type(function, argument)
        else {
            continue;
        };
        let Some(parameter_type) = function.value_types.get(parameter)
        else {
            continue;
        };
        if argument_type != *parameter_type {
            return Err(MirValidationError::JumpArgumentTypeMismatch {
                function: function.display_symbol.clone(),
                block: block.label.to_string(),
                target_block: target_block.label.to_string(),
                index,
                argument_type,
                parameter_type: parameter_type.clone(),
            });
        }
    }
    Ok(())
}
pub(super) fn validate_jump_shape(
    function_name: &str,
    block_label: &str,
    target: MirBlockRef,
    argument_count: usize,
    blocks: &BTreeMap<MirBlockRef, usize>,
) -> Result<(), MirValidationError> {
    let Some(expected_parameter_count) = blocks.get(&target).copied()
    else {
        return Err(MirValidationError::JumpTargetMissing { function: function_name.to_string(), block: block_label.to_string() });
    };
    if expected_parameter_count != argument_count {
        return Err(MirValidationError::JumpArgumentCountMismatch {
            function: function_name.to_string(),
            block: block_label.to_string(),
            expected: expected_parameter_count,
            actual: argument_count,
        });
    }
    Ok(())
}

pub(super) fn ensure_target_exists(
    function_name: &str,
    block_label: &str,
    target: MirBlockRef,
    blocks: &BTreeMap<MirBlockRef, usize>,
) -> Result<(), MirValidationError> {
    if blocks.contains_key(&target) {
        Ok(())
    }
    else {
        Err(MirValidationError::TargetBlockMissing { function: function_name.to_string(), block: block_label.to_string() })
    }
}
