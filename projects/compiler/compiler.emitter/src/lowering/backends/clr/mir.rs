//! CLR 数值指令绑定的唯一助记符表。
//!
//! `[clr_operator("i64.div")]` 只声明目标 operator 助记符；助记符到**冻结配方**
//! 的映射归 CLR backend 所有，并在 `TargetExecutionPlan` 构造时一次性写入计划
//! （arity、操作数发射配方、操作数/结果载体与 MSIL opcode）。emit 侧只编码计划
//! 里的配方，不再从助记符字符串推断 arity 或操作数改写。
//!
//! MSIL 算术/比较 opcode 不区分数值载体，助记符因此必须携带载体限定名
//! （`i32.add` / `i64.add` / `f64.add`），冻结表按限定名给出唯一配方。
//! 比较冻结 `eq/lt/gt` 的单 opcode（`ceq`/`clt`/`cgt`）；`ne` 在 IL 上没有独立
//! 指令，冻结为 `ceq` 之后 `ldc.i4.0` 再 `ceq` 的取反组合（第二条 `ceq` 复用同一
//! 比较指令，不是新指令）。`f64.rem` 保持缺口：.NET 没有与 Java `%`/fmod 语义
//! 一致的 IL 指令，`Math.IEEERemainder` 是 IEEE 754 remainder，语义不同。

use crate::{
    target_operator::{TargetOperandStep, TargetOperandType, TargetOperatorPlan, TargetOperatorRecipe},
    transitional::msil::MsilOpcode,
};

/// CLR operator 的冻结编码。
///
/// 算术/比较多数是单条 MSIL opcode；`ne` 需要用两条 `ceq` 把 0/1 结果精确取反，
/// 因此编码必须显式区分「单指令」与「取反组合」，emit 只机械展开，不得按助记符
/// 或名字前缀推断序列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClrOperatorCode {
    /// 单条 MSIL opcode。
    Single(MsilOpcode),
    /// 布尔取反：先执行 `compare` 得到 0/1，再压 `ldc.i4.0` 并用同一条 `compare`
    /// 与之比较；这是取反组合，不是新指令。
    InvertCompare(MsilOpcode),
}

const fn binary(operand_type: TargetOperandType, result_type: TargetOperandType, opcode: MsilOpcode) -> TargetOperatorPlan<ClrOperatorCode> {
    TargetOperatorPlan {
        arity: 2,
        operand_type,
        result_type,
        recipe: TargetOperatorRecipe::Operands,
        encoding: ClrOperatorCode::Single(opcode),
    }
}

const fn unary(operand_type: TargetOperandType, opcode: MsilOpcode) -> TargetOperatorPlan<ClrOperatorCode> {
    TargetOperatorPlan {
        arity: 1,
        operand_type,
        result_type: operand_type,
        recipe: TargetOperatorRecipe::Operands,
        encoding: ClrOperatorCode::Single(opcode),
    }
}

/// `ne` 系列：IL 没有独立不等指令，冻结为 `ceq` 的精确逻辑取反组合。
const fn not_equal(operand_type: TargetOperandType, result_type: TargetOperandType) -> TargetOperatorPlan<ClrOperatorCode> {
    TargetOperatorPlan {
        arity: 2,
        operand_type,
        result_type,
        recipe: TargetOperatorRecipe::Steps(&[TargetOperandStep::Arguments]),
        encoding: ClrOperatorCode::InvertCompare(MsilOpcode::Ceq),
    }
}

/// `i64` 移位：MSIL `shl`/`shr` 的位移量操作数必须是 `int32`；声明签名携带 `i64`
/// 位移量，截窄（`conv.i4`）由冻结配方显式给出，emit 只机械执行。
const I64_SHIFT_STEPS: &[TargetOperandStep] = &[TargetOperandStep::Arguments, TargetOperandStep::NarrowTopI64ToI32];

const fn i64_shift(steps: &'static [TargetOperandStep], opcode: MsilOpcode) -> TargetOperatorPlan<ClrOperatorCode> {
    TargetOperatorPlan {
        arity: 2,
        operand_type: TargetOperandType::I64,
        result_type: TargetOperandType::I64,
        recipe: TargetOperatorRecipe::Steps(steps),
        encoding: ClrOperatorCode::Single(opcode),
    }
}

/// 唯一助记符表。未知助记符返回 `None`。
pub(crate) fn freeze_clr_operator_binding(mnemonic: &str) -> Option<TargetOperatorPlan<ClrOperatorCode>> {
    Some(match mnemonic {
        // int32 算术：MSIL 单 opcode。
        "i32.add" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::Add),
        "i32.sub" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::Sub),
        "i32.mul" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::Mul),
        "i32.div" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::Div),
        "i32.rem" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::Rem),
        // 无符号 `u32` 除/余：MSIL 原生 `div.un`/`rem.un`（零除数抛 DivideByZeroException）。
        "i32.div.un" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::DivUn),
        "i32.rem.un" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::RemUn),
        "i32.neg" => unary(TargetOperandType::I32, MsilOpcode::Neg),
        // int32 位运算：MSIL 单 opcode，按位结果与 `u32` 载体等价。
        "i32.and" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::And),
        "i32.or" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::Or),
        "i32.xor" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::Xor),
        "i32.shl" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::Shl),
        // 无符号 `u32` 逻辑右移：MSIL `shr.un`（有符号 `shr` 会做算术右移）。
        "i32.shr.un" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::ShrUn),
        // int64 算术：同一批 opcode 对宽载体同形。
        "i64.add" => binary(TargetOperandType::I64, TargetOperandType::I64, MsilOpcode::Add),
        "i64.sub" => binary(TargetOperandType::I64, TargetOperandType::I64, MsilOpcode::Sub),
        "i64.mul" => binary(TargetOperandType::I64, TargetOperandType::I64, MsilOpcode::Mul),
        "i64.div" => binary(TargetOperandType::I64, TargetOperandType::I64, MsilOpcode::Div),
        "i64.rem" => binary(TargetOperandType::I64, TargetOperandType::I64, MsilOpcode::Rem),
        // 无符号 `u64` 除/余：MSIL `div.un`/`rem.un` 对 int64 载体同形。
        "i64.div.un" => binary(TargetOperandType::I64, TargetOperandType::I64, MsilOpcode::DivUn),
        "i64.rem.un" => binary(TargetOperandType::I64, TargetOperandType::I64, MsilOpcode::RemUn),
        "i64.neg" => unary(TargetOperandType::I64, MsilOpcode::Neg),
        // int64 位运算：MSIL 单 opcode，载体无关。
        "i64.and" => binary(TargetOperandType::I64, TargetOperandType::I64, MsilOpcode::And),
        "i64.or" => binary(TargetOperandType::I64, TargetOperandType::I64, MsilOpcode::Or),
        "i64.xor" => binary(TargetOperandType::I64, TargetOperandType::I64, MsilOpcode::Xor),
        // int64 移位：位移量按冻结配方先 `conv.i4` 截窄，再执行 `shl`/`shr`。
        "i64.shl" => i64_shift(I64_SHIFT_STEPS, MsilOpcode::Shl),
        "i64.shr" => i64_shift(I64_SHIFT_STEPS, MsilOpcode::Shr),
        // 无符号 `u64` 逻辑右移：`shr.un`，位移量同样先 `conv.i4` 截窄。
        "i64.shr.un" => i64_shift(I64_SHIFT_STEPS, MsilOpcode::ShrUn),
        // float64 算术：MSIL `neg` 对浮点保留 -0.0，与语义一致。
        "f64.add" => binary(TargetOperandType::F64, TargetOperandType::F64, MsilOpcode::Add),
        "f64.sub" => binary(TargetOperandType::F64, TargetOperandType::F64, MsilOpcode::Sub),
        "f64.mul" => binary(TargetOperandType::F64, TargetOperandType::F64, MsilOpcode::Mul),
        "f64.div" => binary(TargetOperandType::F64, TargetOperandType::F64, MsilOpcode::Div),
        "f64.neg" => unary(TargetOperandType::F64, MsilOpcode::Neg),
        // 比较：结果固定为 I32（0/1）；eq/lt/gt 是单 opcode，ne 是取反组合。
        "i32.eq" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::Ceq),
        "i32.ne" => not_equal(TargetOperandType::I32, TargetOperandType::I32),
        "i32.lt" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::Clt),
        // 无符号 `int32` 小于：MSIL 原生 `clt.un`。
        "i32.lt.un" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::CltUn),
        "i32.gt" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::Cgt),
        "i64.eq" => binary(TargetOperandType::I64, TargetOperandType::I32, MsilOpcode::Ceq),
        "i64.ne" => not_equal(TargetOperandType::I64, TargetOperandType::I32),
        "i64.lt" => binary(TargetOperandType::I64, TargetOperandType::I32, MsilOpcode::Clt),
        // 无符号 `int64` 小于：MSIL `clt.un` 对宽载体同形（ECMA-335 的 `clt.un` 接受 int64）。
        "i64.lt.un" => binary(TargetOperandType::I64, TargetOperandType::I32, MsilOpcode::CltUn),
        "i64.gt" => binary(TargetOperandType::I64, TargetOperandType::I32, MsilOpcode::Cgt),
        "f64.eq" => binary(TargetOperandType::F64, TargetOperandType::I32, MsilOpcode::Ceq),
        "f64.ne" => not_equal(TargetOperandType::F64, TargetOperandType::I32),
        "f64.lt" => binary(TargetOperandType::F64, TargetOperandType::I32, MsilOpcode::Clt),
        "f64.gt" => binary(TargetOperandType::F64, TargetOperandType::I32, MsilOpcode::Cgt),
        _ => return None,
    })
}

/// CLR 平台私有的 operator 绑定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClrOperatorBinding {
    /// 已冻结的 MSIL 配方。
    Frozen(TargetOperatorPlan<ClrOperatorCode>),
    /// 声明了 `[clr_operator]`，但没有 CLR 可编码配方；emit 必须结构化失败。
    Unfrozen,
}

/// 按声明的导入签名冻结 CLR operator 绑定。
pub(crate) fn freeze_clr_operator(
    link: &compiler_target::ExternalImportLink,
    instance: compiler_types::ItemInstanceId,
    symbol: &str,
    parameter_types: &[compiler_types::SemanticType],
    return_type: &compiler_types::SemanticType,
) -> crate::Result<Option<ClrOperatorBinding>> {
    let Some(mnemonic) = crate::lowering::shared::interop::clr_operator_target(link)
    else {
        return Ok(None);
    };
    let Some(plan) = freeze_clr_operator_binding(mnemonic)
    else {
        return Ok(Some(ClrOperatorBinding::Unfrozen));
    };
    crate::target_operator::validate_frozen_operator(mnemonic, instance, symbol, parameter_types, return_type, &plan)?;
    Ok(Some(ClrOperatorBinding::Frozen(plan)))
}

#[cfg(test)]
mod operator_binding_tests {
    use compiler_target::{ExternalImportLink, Identifier};
    use compiler_types::{ItemInstanceId, SemanticType};

    use super::{ClrOperatorBinding, ClrOperatorCode, MsilOpcode, freeze_clr_operator};

    fn link(mnemonic: &str) -> ExternalImportLink {
        ExternalImportLink::host(Some(Identifier::new("clr-operator")), vec![mnemonic.to_string()])
    }

    /// 声明侧 `[clr_operator("…")]` 的真实助记符必须冻结成 MSIL opcode；
    /// `ne` 冻结为 `ceq; ldc.i4.0; ceq` 取反组合；`f64.rem` 仍保持 Unfrozen。
    #[test]
    fn scalar_operator_mnemonics_freeze_to_msil_opcodes() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let i32_ty = SemanticType::Integer32 { signed: true };

        let add = freeze_clr_operator(&link("i32.add"), instance, "infix +", &[i32_ty.clone(), i32_ty.clone()], &i32_ty)
            .expect("冻结不得失败")
            .expect("必须是 CLR operator 绑定");
        let ClrOperatorBinding::Frozen(add) = add
        else {
            panic!("`i32.add` 必须冻结");
        };
        assert_eq!(add.arity, 2);
        assert_eq!(add.result_type, crate::target_operator::TargetOperandType::I32);
        assert_eq!(add.encoding, ClrOperatorCode::Single(MsilOpcode::Add));

        let less = freeze_clr_operator(&link("i32.lt"), instance, "infix <", &[i32_ty.clone(), i32_ty.clone()], &SemanticType::Boolean)
            .expect("冻结不得失败")
            .expect("必须是 CLR operator 绑定");
        let ClrOperatorBinding::Frozen(less) = less
        else {
            panic!("`i32.lt` 必须冻结");
        };
        assert_eq!(less.encoding, ClrOperatorCode::Single(MsilOpcode::Clt));

        // `ne` 没有独立 IL 指令：冻结为 `ceq` 的逻辑取反组合，仍是精确编码。
        let not_equal = freeze_clr_operator(&link("i32.ne"), instance, "infix !=", &[i32_ty.clone(), i32_ty.clone()], &SemanticType::Boolean)
            .expect("冻结不得失败")
            .expect("必须是 CLR operator 绑定");
        let ClrOperatorBinding::Frozen(not_equal) = not_equal
        else {
            panic!("`i32.ne` 必须冻结");
        };
        assert_eq!(not_equal.encoding, ClrOperatorCode::InvertCompare(MsilOpcode::Ceq));

        let i64_ty = SemanticType::Integer64 { signed: true };
        for (mnemonic, operand, result) in [
            ("i64.ne", i64_ty.clone(), SemanticType::Boolean),
            ("f64.ne", SemanticType::Float64, SemanticType::Boolean),
        ] {
            let plan = freeze_clr_operator(&link(mnemonic), instance, "infix !=", &[operand.clone(), operand], &result)
                .unwrap_or_else(|error| panic!("`{mnemonic}` 冻结不得失败: {error}"))
                .unwrap_or_else(|| panic!("`{mnemonic}` 必须是 CLR operator 绑定"));
            let ClrOperatorBinding::Frozen(plan) = plan
            else {
                panic!("`{mnemonic}` 必须冻结");
            };
            assert_eq!(plan.encoding, ClrOperatorCode::InvertCompare(MsilOpcode::Ceq), "`{mnemonic}` 编码");
            assert_eq!(plan.result_type, crate::target_operator::TargetOperandType::I32);
        }

        // .NET 没有与 Java `%`/fmod 语义一致的 IL 指令：`f64.rem` 必须保持缺口。
        let unfrozen = freeze_clr_operator(&link("f64.rem"), instance, "infix %", &[SemanticType::Float64, SemanticType::Float64], &SemanticType::Float64)
            .expect("冻结不得失败")
            .expect("必须是 CLR operator 绑定");
        assert!(matches!(unfrozen, ClrOperatorBinding::Unfrozen));
    }
}
