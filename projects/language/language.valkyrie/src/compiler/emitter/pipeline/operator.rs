//! 语言 × 目标适配器的共享 operator 裁决词汇：源语义 → 目标载体与冻结形状核对。
//!
//! 架构合同：`语言 AST → HIR → MIR → 语言×目标适配器 → 目标私有输入 → 后端编码`。
//! 声明签名的载体裁决（[`operand_carrier`] / [`result_carrier`]）与冻结计划的
//! 语义签名核对（[`validate_frozen_operator`]）属于语言侧适配器职责，不再由
//! emitter 后端持有；各 lane 的 `freeze_*_operator` 入口经本模块消费同一形状
//! 规则，避免每个后端重复实现。词汇类型（`TargetOperandType` 等）仍归
//! `compiler_emitter::target_operator`，emitter 后端只消费冻结后的目标事实。

use compiler_emitter::target_operator::{TargetOperandStep, TargetOperandType, TargetOperatorPlan, TargetOperatorRecipe};

/// 声明签名里标量类型对应的目标载体。
///
/// 载体事实只由类型身份决定，与本模块无关；各 lane 的冻结入口共用这一判定，
/// 避免每个后端重复实现同一形状规则。
pub(crate) fn operand_carrier(ty: &compiler_types::SemanticType) -> Option<TargetOperandType> {
    match ty {
        compiler_types::SemanticType::Integer32 { .. } => Some(TargetOperandType::I32),
        // `char`（u16 code unit）在这些 lane 的物理载体是 i32（ABI 层 `AbiCarrier::I32` 同构）；
        // 等值/比较配方按零扩展后的码位判定，不引入截断语义。
        compiler_types::SemanticType::Character => Some(TargetOperandType::I32),
        compiler_types::SemanticType::Integer64 { .. } => Some(TargetOperandType::I64),
        compiler_types::SemanticType::Float32 => Some(TargetOperandType::F32),
        compiler_types::SemanticType::Float64 => Some(TargetOperandType::F64),
        _ => None,
    }
}

/// 返回槽的载体；布尔值在所有当前 lane 都占 `i32` 槽。
pub(crate) fn result_carrier(ty: &compiler_types::SemanticType) -> Option<TargetOperandType> {
    match ty {
        compiler_types::SemanticType::Boolean => Some(TargetOperandType::I32),
        _ => operand_carrier(ty),
    }
}

/// 校验 lane 冻结的 operator 计划与导入声明签名一致。
///
/// 导入声明是 Compiler 已绑定的调用合同，冻结计划是目标 operator 的物理形状；
/// 二者在 arity、操作数载体、结果槽载体或常量配方载体上不一致时，计划不成立。
/// 本函数只做平台无关的形状核对，助记符仅用于诊断；具体冻结表由各 lane 拥有。
pub(crate) fn validate_frozen_operator<E>(
    mnemonic: &str,
    instance: compiler_types::ItemInstanceId,
    function: &str,
    parameter_types: &[compiler_types::SemanticType],
    return_type: &compiler_types::SemanticType,
    plan: &TargetOperatorPlan<E>,
) -> Result<(), String> {
    if parameter_types.len() != plan.arity {
        return Err(format!(
            "目标 operator 绑定 `{mnemonic}` 的声明 arity 与冻结计划不符: function={function} expected={} actual={} instance={instance:?}",
            plan.arity,
            parameter_types.len()
        ));
    }
    for (index, ty) in parameter_types.iter().enumerate() {
        let Some(carrier) = operand_carrier(ty)
        else {
            return Err(format!(
                "目标 operator 绑定 `{mnemonic}` 的参数 {index} 类型没有目标载体: function={function} type={ty:?} instance={instance:?}"
            ));
        };
        if carrier != plan.operand_type {
            return Err(format!(
                "目标 operator 绑定 `{mnemonic}` 的参数 {index} 操作数载体与冻结计划不符: function={function} expected={:?} actual={:?} instance={instance:?}",
                plan.operand_type, carrier
            ));
        }
    }
    let Some(result) = result_carrier(return_type)
    else {
        return Err(format!(
            "目标 operator 绑定 `{mnemonic}` 的返回类型没有结果槽载体: function={function} type={return_type:?} instance={instance:?}"
        ));
    };
    if result != plan.result_type {
        return Err(format!(
            "目标 operator 绑定 `{mnemonic}` 的结果槽载体与冻结计划不符: function={function} expected={:?} actual={:?} instance={instance:?}",
            plan.result_type, result
        ));
    }
    if let TargetOperatorRecipe::Steps(steps) = plan.recipe {
        for step in steps {
            if let TargetOperandStep::IntegerConstant { operand_type, .. } = step {
                if *operand_type != plan.operand_type {
                    return Err(format!(
                        "目标 operator 绑定 `{mnemonic}` 的常量配方载体与冻结操作数载体不符: function={function} expected={:?} actual={:?} instance={instance:?}",
                        plan.operand_type, operand_type
                    ));
                }
            }
        }
    }
    Ok(())
}
