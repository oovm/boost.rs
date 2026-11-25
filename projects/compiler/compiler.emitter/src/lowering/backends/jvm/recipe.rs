//! JVM 数值指令绑定的唯一 operator 配方表。
//!
//! `[jvm_operator("long_divide")]` 只声明目标 operator 名；operator 名到**冻结配方**
//! 的映射归 JVM backend 所有，并在 `TargetExecutionPlan` 构造时一次性写入计划（arity、
//! 操作数发射配方、操作数/结果载体与冻结码）。emit 侧只编码计划里的配方，不再
//! 从 operator 名字符串推断 arity 或操作数改写。
//!
//! 本表只接受下列出的 operator 名，未知名字返回 `None`，计划据此冻结为
//! `对应 lane 的 `Unfrozen` 绑定`，由 emit 结构化失败，禁止猜默认指令。
//!
//! double/float 比较在 JVM 上没有单条指令，由后端展开为 `dcmpl`/`dcmpg`
//! （`fcmpl`/`fcmpg`）+ 分支序列；NaN 语义按 Java 语言固定（见
//! [`super::JvmOperatorCode::instructions`]）。`dneg`/`drem`/`fneg`/`frem` 都是
//! 真实 JVM 指令，直接单条编码。

use super::JvmOperatorCode;
use crate::target_operator::{TargetOperandType, TargetOperatorPlan, TargetOperatorRecipe};

const fn binary(operand_type: TargetOperandType, result_type: TargetOperandType, code: JvmOperatorCode) -> TargetOperatorPlan<JvmOperatorCode> {
    TargetOperatorPlan { arity: 2, operand_type, result_type, recipe: TargetOperatorRecipe::Operands, encoding: code }
}

const fn unary(operand_type: TargetOperandType, code: JvmOperatorCode) -> TargetOperatorPlan<JvmOperatorCode> {
    TargetOperatorPlan { arity: 1, operand_type, result_type: operand_type, recipe: TargetOperatorRecipe::Operands, encoding: code }
}

/// `long` 移位：JVM `lshl`/`lshr`/`lushr` 的位移量操作数必须是 `int`；声明签名携带
/// `i64` 位移量。`l2i` 截窄是本 lane 的物理事实，冻结在
/// [`super::JvmOperatorCode::instructions`] 的编码展开里，不借公共指令步骤表达。
const fn long_shift(code: JvmOperatorCode) -> TargetOperatorPlan<JvmOperatorCode> {
    TargetOperatorPlan {
        arity: 2,
        operand_type: TargetOperandType::I64,
        result_type: TargetOperandType::I64,
        recipe: TargetOperatorRecipe::Operands,
        encoding: code,
    }
}

/// 唯一 operator 配方表。未知名字返回 `None`。
pub(crate) fn freeze_jvm_operator_binding(operator_name: &str) -> Option<TargetOperatorPlan<JvmOperatorCode>> {
    Some(match operator_name {
        // integer 算术：JVM 单指令（`iadd`/`isub`/`imul`/`idiv`/`irem`/`ineg`）。
        "integer.add" => binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerAdd),
        "integer.subtract" => binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerSubtract),
        "integer.multiply" => binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerMultiply),
        "integer.divide" => binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerDivide),
        "integer.remainder" => binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerRemainder),
        "integer.negate" => unary(TargetOperandType::I32, JvmOperatorCode::IntegerNegate),
        // integer 位运算：JVM 单指令（`iand`/`ior`/`ixor`）。
        "integer.and" => binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerBitAnd),
        "integer.or" => binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerBitOr),
        "integer.xor" => binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerBitXor),
        // integer 移位：有符号右移是算术 `ishr`，`u32` 右移是逻辑 `iushr`，两者不可互换。
        "integer.shift_left" => binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerShiftLeft),
        "integer.shift_right" => binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerShiftRight),
        "integer.shift_right_unsigned" => binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerShiftRightUnsigned),
        // 无符号 `u32` 除/余：JVM 没有无符号 `idiv`/`irem`，冻结为 Java 8 标准库静态方法。
        "integer.divide_unsigned" => binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerDivideUnsigned),
        "integer.remainder_unsigned" => binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerRemainderUnsigned),
        // long 算术：JVM 单指令（`ladd`/`lsub`/`lmul`/`ldiv`/`lrem`/`lneg`）。
        "long.add" => binary(TargetOperandType::I64, TargetOperandType::I64, JvmOperatorCode::LongAdd),
        "long.subtract" => binary(TargetOperandType::I64, TargetOperandType::I64, JvmOperatorCode::LongSubtract),
        "long.multiply" => binary(TargetOperandType::I64, TargetOperandType::I64, JvmOperatorCode::LongMultiply),
        "long.divide" => binary(TargetOperandType::I64, TargetOperandType::I64, JvmOperatorCode::LongDivide),
        "long.remainder" => binary(TargetOperandType::I64, TargetOperandType::I64, JvmOperatorCode::LongRemainder),
        "long.negate" => unary(TargetOperandType::I64, JvmOperatorCode::LongNegate),
        // long 位运算：JVM 单指令（`land`/`lor`/`lxor`）。
        "long.and" => binary(TargetOperandType::I64, TargetOperandType::I64, JvmOperatorCode::LongBitAnd),
        "long.or" => binary(TargetOperandType::I64, TargetOperandType::I64, JvmOperatorCode::LongBitOr),
        "long.xor" => binary(TargetOperandType::I64, TargetOperandType::I64, JvmOperatorCode::LongBitXor),
        // long 移位：`l2i` 截窄冻结在编码展开内，再执行 `lshl`/`lshr`。
        "long.shift_left" => long_shift(JvmOperatorCode::LongShiftLeft),
        "long.shift_right" => long_shift(JvmOperatorCode::LongShiftRight),
        // 无符号 `u64` 右移：逻辑右移 `lushr`，位移量同样先截窄。
        "long.shift_right_unsigned" => long_shift(JvmOperatorCode::LongShiftRightUnsigned),
        // 无符号 `u64` 除/余：JVM 没有无符号 `ldiv`/`lrem`，冻结为 Java 8 标准库静态方法。
        "long.divide_unsigned" => binary(TargetOperandType::I64, TargetOperandType::I64, JvmOperatorCode::LongDivideUnsigned),
        "long.remainder_unsigned" => binary(TargetOperandType::I64, TargetOperandType::I64, JvmOperatorCode::LongRemainderUnsigned),
        // double 算术：JVM 单指令（`dadd`/`dsub`/`dmul`/`ddiv`/`drem`/`dneg`）。
        "double.add" => binary(TargetOperandType::F64, TargetOperandType::F64, JvmOperatorCode::DoubleAdd),
        "double.subtract" => binary(TargetOperandType::F64, TargetOperandType::F64, JvmOperatorCode::DoubleSubtract),
        "double.multiply" => binary(TargetOperandType::F64, TargetOperandType::F64, JvmOperatorCode::DoubleMultiply),
        "double.divide" => binary(TargetOperandType::F64, TargetOperandType::F64, JvmOperatorCode::DoubleDivide),
        "double.remainder" => binary(TargetOperandType::F64, TargetOperandType::F64, JvmOperatorCode::DoubleRemainder),
        "double.negate" => unary(TargetOperandType::F64, JvmOperatorCode::DoubleNegate),
        // float 算术：JVM 单指令（`fadd`/`fsub`/`fmul`/`fdiv`/`frem`/`fneg`）。
        // 单精度运算不得提升到 double 再降回。
        "float.add" => binary(TargetOperandType::F32, TargetOperandType::F32, JvmOperatorCode::FloatAdd),
        "float.subtract" => binary(TargetOperandType::F32, TargetOperandType::F32, JvmOperatorCode::FloatSubtract),
        "float.multiply" => binary(TargetOperandType::F32, TargetOperandType::F32, JvmOperatorCode::FloatMultiply),
        "float.divide" => binary(TargetOperandType::F32, TargetOperandType::F32, JvmOperatorCode::FloatDivide),
        "float.remainder" => binary(TargetOperandType::F32, TargetOperandType::F32, JvmOperatorCode::FloatRemainder),
        "float.negate" => unary(TargetOperandType::F32, JvmOperatorCode::FloatNegate),
        // integer 比较：结果固定为 I32（0/1），由后端展开 `if_icmpxx` 分支序列。
        "integer.compare_equal" => binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerCompareEqual),
        "integer.compare_not_equal" => binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerCompareNotEqual),
        "integer.compare_less_than" => binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerCompareLessThan),
        // 无符号 `i32` 小于：JVM 没有单条无符号比较指令，冻结为符号位翻转序列。
        "integer.compare_less_than_unsigned" => {
            binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerCompareLessThanUnsigned)
        }
        "integer.compare_less_or_equal" => binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerCompareLessOrEqual),
        "integer.compare_greater_than" => binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerCompareGreaterThan),
        "integer.compare_greater_or_equal" => {
            binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerCompareGreaterOrEqual)
        }
        // 无符号 `i32` 的其余有序比较：同样冻结为符号位翻转 + 有符号分支序列。
        "integer.compare_greater_than_unsigned" => {
            binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerCompareGreaterThanUnsigned)
        }
        "integer.compare_less_or_equal_unsigned" => {
            binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerCompareLessOrEqualUnsigned)
        }
        "integer.compare_greater_or_equal_unsigned" => {
            binary(TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerCompareGreaterOrEqualUnsigned)
        }
        // long 比较：结果固定为 I32（0/1），由后端展开 `lcmp` + `ifxx` 序列。
        "long.compare_equal" => binary(TargetOperandType::I64, TargetOperandType::I32, JvmOperatorCode::LongCompareEqual),
        "long.compare_not_equal" => binary(TargetOperandType::I64, TargetOperandType::I32, JvmOperatorCode::LongCompareNotEqual),
        "long.compare_less_than" => binary(TargetOperandType::I64, TargetOperandType::I32, JvmOperatorCode::LongCompareLessThan),
        // 无符号 `u64` 小于：JVM 没有无符号 `lcmp`，冻结为符号位翻转序列（展开见
        // `JvmOperatorCode::LongCompareLessThanUnsigned`）；翻转中途经 scratch long 槽周转。
        "long.compare_less_than_unsigned" => {
            binary(TargetOperandType::I64, TargetOperandType::I32, JvmOperatorCode::LongCompareLessThanUnsigned)
        }
        "long.compare_less_or_equal" => binary(TargetOperandType::I64, TargetOperandType::I32, JvmOperatorCode::LongCompareLessOrEqual),
        "long.compare_greater_than" => binary(TargetOperandType::I64, TargetOperandType::I32, JvmOperatorCode::LongCompareGreaterThan),
        "long.compare_greater_or_equal" => binary(TargetOperandType::I64, TargetOperandType::I32, JvmOperatorCode::LongCompareGreaterOrEqual),
        // 无符号 `u64` 的其余有序比较：同样冻结为符号位翻转 + `lcmp` 序列；
        // 翻转中途经 scratch long 槽周转。
        "long.compare_greater_than_unsigned" => {
            binary(TargetOperandType::I64, TargetOperandType::I32, JvmOperatorCode::LongCompareGreaterThanUnsigned)
        }
        "long.compare_less_or_equal_unsigned" => {
            binary(TargetOperandType::I64, TargetOperandType::I32, JvmOperatorCode::LongCompareLessOrEqualUnsigned)
        }
        "long.compare_greater_or_equal_unsigned" => {
            binary(TargetOperandType::I64, TargetOperandType::I32, JvmOperatorCode::LongCompareGreaterOrEqualUnsigned)
        }
        // double 比较：结果固定为 I32（0/1），由后端展开 `dcmpl`/`dcmpg` + 分支序列。
        "double.compare_equal" => binary(TargetOperandType::F64, TargetOperandType::I32, JvmOperatorCode::DoubleCompareEqual),
        "double.compare_not_equal" => binary(TargetOperandType::F64, TargetOperandType::I32, JvmOperatorCode::DoubleCompareNotEqual),
        "double.compare_less_than" => binary(TargetOperandType::F64, TargetOperandType::I32, JvmOperatorCode::DoubleCompareLessThan),
        "double.compare_less_or_equal" => {
            binary(TargetOperandType::F64, TargetOperandType::I32, JvmOperatorCode::DoubleCompareLessOrEqual)
        }
        "double.compare_greater_than" => {
            binary(TargetOperandType::F64, TargetOperandType::I32, JvmOperatorCode::DoubleCompareGreaterThan)
        }
        "double.compare_greater_or_equal" => {
            binary(TargetOperandType::F64, TargetOperandType::I32, JvmOperatorCode::DoubleCompareGreaterOrEqual)
        }
        // float 比较：结果固定为 I32（0/1），由后端展开 `fcmpl`/`fcmpg` + 分支序列。
        "float.compare_equal" => binary(TargetOperandType::F32, TargetOperandType::I32, JvmOperatorCode::FloatCompareEqual),
        "float.compare_not_equal" => binary(TargetOperandType::F32, TargetOperandType::I32, JvmOperatorCode::FloatCompareNotEqual),
        "float.compare_less_than" => binary(TargetOperandType::F32, TargetOperandType::I32, JvmOperatorCode::FloatCompareLessThan),
        "float.compare_less_or_equal" => {
            binary(TargetOperandType::F32, TargetOperandType::I32, JvmOperatorCode::FloatCompareLessOrEqual)
        }
        "float.compare_greater_than" => {
            binary(TargetOperandType::F32, TargetOperandType::I32, JvmOperatorCode::FloatCompareGreaterThan)
        }
        "float.compare_greater_or_equal" => {
            binary(TargetOperandType::F32, TargetOperandType::I32, JvmOperatorCode::FloatCompareGreaterOrEqual)
        }
        _ => return None,
    })
}

/// 源码 `[jvm_operator("ldiv")]` 声明的 JVM 目标 operator 助记符。
///
/// 只接受 platform tag `jvm-operator` 且 locator 恰好一段；其余形态返回 `None`，
/// 由调用方按「缺少可编码目标绑定」结构化失败。
pub fn jvm_operator_target(link: &compiler_target::ExternalImportLink) -> Option<&str> {
    if !link.matches_boundary("host") || !link.matches_platform_tag("jvm-operator") {
        return None;
    }

    match link.locator_segments() {
        [mnemonic] => Some(mnemonic.as_str()),
        _ => None,
    }
}

/// JVM 平台私有的 operator 绑定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JvmOperatorBinding {
    /// 已冻结的 JVM 配方。
    Frozen(TargetOperatorPlan<JvmOperatorCode>),
    /// 声明了 `[jvm_operator]`，但没有 JVM 可编码配方；emit 必须结构化失败。
    Unfrozen,
}

/// 按声明的导入签名冻结 JVM operator 绑定。
///
/// `None` 表示该导入不是 JVM operator；`Some(Unfrozen)` 表示绑定了 JVM operator
/// 但名字不在本 lane 的配方表内。两者都必须保留，禁止回落为宿主导入或默认成功。
pub fn freeze_jvm_operator(
    link: &compiler_target::ExternalImportLink,
    instance: compiler_types::ItemInstanceId,
    symbol: &str,
    parameter_types: &[compiler_types::SemanticType],
    return_type: &compiler_types::SemanticType,
) -> crate::Result<Option<JvmOperatorBinding>> {
    let Some(mnemonic) = jvm_operator_target(link)
    else {
        return Ok(None);
    };
    let Some(plan) = freeze_jvm_operator_binding(mnemonic)
    else {
        return Ok(Some(JvmOperatorBinding::Unfrozen));
    };
    crate::target_operator::validate_frozen_operator(mnemonic, instance, symbol, parameter_types, return_type, &plan)?;
    Ok(Some(JvmOperatorBinding::Frozen(plan)))
}

#[cfg(test)]
mod operator_binding_tests {
    use compiler_target::{ExternalImportLink, Identifier, SemanticType};
    use compiler_types::ItemInstanceId;

    use super::JvmOperatorCode;
    use super::{JvmOperatorBinding, freeze_jvm_operator};
    use crate::target_operator::TargetOperandType;
    use crate::transitional::binary::class::JvmInstruction;

    fn link(mnemonic: &str) -> ExternalImportLink {
        ExternalImportLink::host(Some(Identifier::new("jvm-operator")), vec![mnemonic.to_string()])
    }

    /// 声明侧 `[jvm_operator("…")]` 的真实助记符必须冻结成 JVM 指令，
    /// 而不是 Unfrozen；未知助记符仍失败闭合。
    #[test]
    fn scalar_operator_mnemonics_freeze_to_jvm_instructions() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let i32_ty = SemanticType::Integer32 { signed: true };

        let add = freeze_jvm_operator(&link("integer.add"), instance, "infix +", &[i32_ty.clone(), i32_ty.clone()], &i32_ty)
            .expect("冻结不得失败")
            .expect("必须是 JVM operator 绑定");
        let JvmOperatorBinding::Frozen(add) = add
        else {
            panic!("`integer.add` 必须冻结");
        };
        assert_eq!(add.arity, 2);
        assert_eq!(add.encoding, JvmOperatorCode::IntegerAdd);
        assert_eq!(add.encoding.arithmetic_instruction(), Some(JvmInstruction::IAdd));

        let less = freeze_jvm_operator(&link("integer.compare_less_than"), instance, "infix <", &[i32_ty.clone(), i32_ty.clone()], &SemanticType::Boolean)
            .expect("冻结不得失败")
            .expect("必须是 JVM operator 绑定");
        let JvmOperatorBinding::Frozen(less) = less
        else {
            panic!("`integer.compare_less_than` 必须冻结");
        };
        assert_eq!(less.result_type, TargetOperandType::I32);
        assert_eq!(less.encoding, JvmOperatorCode::IntegerCompareLessThan);
        let instructions = less.encoding.instructions(0);
        assert!(
            instructions.iter().any(|instruction| matches!(instruction, JvmInstruction::IfICmpGe(_))),
            "i32 `<` 必须展开为 if_icmp 分支序列: {instructions:?}"
        );

        // `dneg` / `drem` 是真实 JVM 指令：必须冻结为单条指令，而不是 Unfrozen。
        let negate = freeze_jvm_operator(&link("double.negate"), instance, "prefix -", &[SemanticType::Float64], &SemanticType::Float64)
            .expect("冻结不得失败")
            .expect("必须是 JVM operator 绑定");
        let JvmOperatorBinding::Frozen(negate) = negate
        else {
            panic!("`double.negate` 必须冻结");
        };
        assert_eq!(negate.encoding, JvmOperatorCode::DoubleNegate);
        assert_eq!(negate.encoding.arithmetic_instruction(), Some(JvmInstruction::DNeg));

        let remainder = freeze_jvm_operator(
            &link("double.remainder"),
            instance,
            "infix %",
            &[SemanticType::Float64, SemanticType::Float64],
            &SemanticType::Float64,
        )
        .expect("冻结不得失败")
        .expect("必须是 JVM operator 绑定");
        let JvmOperatorBinding::Frozen(remainder) = remainder
        else {
            panic!("`double.remainder` 必须冻结");
        };
        assert_eq!(remainder.encoding.arithmetic_instruction(), Some(JvmInstruction::DRem));

        // double 比较：`==`/`!=` 用 `dcmpl`（NaN → -1 已给出 Java 语义），
        // `<` 必须用 `dcmpg`（NaN → 1 才会被 `ifge` 判为 false）。
        let f64_pair = [SemanticType::Float64, SemanticType::Float64];
        let compare = freeze_jvm_operator(&link("double.compare_equal"), instance, "infix ==", &f64_pair, &SemanticType::Boolean)
            .expect("冻结不得失败")
            .expect("必须是 JVM operator 绑定");
        let JvmOperatorBinding::Frozen(compare) = compare
        else {
            panic!("`double.compare_equal` 必须冻结");
        };
        assert_eq!(compare.encoding, JvmOperatorCode::DoubleCompareEqual);
        let equal_instructions = compare.encoding.instructions(0);
        assert!(
            equal_instructions.iter().any(|instruction| matches!(instruction, JvmInstruction::DCmpL)),
            "`==` 必须用 dcmpl 处理 NaN: {equal_instructions:?}"
        );
        assert!(equal_instructions.iter().any(|instruction| matches!(instruction, JvmInstruction::IfNe(_))));

        let compare = freeze_jvm_operator(&link("double.compare_not_equal"), instance, "infix !=", &f64_pair, &SemanticType::Boolean)
            .expect("冻结不得失败")
            .expect("必须是 JVM operator 绑定");
        let JvmOperatorBinding::Frozen(compare) = compare
        else {
            panic!("`double.compare_not_equal` 必须冻结");
        };
        let not_equal_instructions = compare.encoding.instructions(0);
        assert!(not_equal_instructions.iter().any(|instruction| matches!(instruction, JvmInstruction::DCmpL)));
        assert!(not_equal_instructions.iter().any(|instruction| matches!(instruction, JvmInstruction::IfEq(_))));

        let compare = freeze_jvm_operator(&link("double.compare_less_than"), instance, "infix <", &f64_pair, &SemanticType::Boolean)
            .expect("冻结不得失败")
            .expect("必须是 JVM operator 绑定");
        let JvmOperatorBinding::Frozen(compare) = compare
        else {
            panic!("`double.compare_less_than` 必须冻结");
        };
        assert_eq!(compare.encoding, JvmOperatorCode::DoubleCompareLessThan);
        let less_instructions = compare.encoding.instructions(0);
        assert!(
            less_instructions.iter().any(|instruction| matches!(instruction, JvmInstruction::DCmpG)),
            "`<` 必须用 dcmpg 处理 NaN: {less_instructions:?}"
        );
        assert!(less_instructions.iter().any(|instruction| matches!(instruction, JvmInstruction::IfGe(_))));

        // `frem`/`fneg` 与 `fcmpl`/`fcmpg` 同样是真实指令：float 族按同一结构冻结。
        let float_remainder = freeze_jvm_operator(
            &link("float.remainder"),
            instance,
            "infix %",
            &[SemanticType::Float32, SemanticType::Float32],
            &SemanticType::Float32,
        )
        .expect("冻结不得失败")
        .expect("必须是 JVM operator 绑定");
        let JvmOperatorBinding::Frozen(float_remainder) = float_remainder
        else {
            panic!("`float.remainder` 必须冻结");
        };
        assert_eq!(float_remainder.encoding.arithmetic_instruction(), Some(JvmInstruction::FRem));

        let float_less = freeze_jvm_operator(
            &link("float.compare_less_than"),
            instance,
            "infix <",
            &[SemanticType::Float32, SemanticType::Float32],
            &SemanticType::Boolean,
        )
        .expect("冻结不得失败")
        .expect("必须是 JVM operator 绑定");
        let JvmOperatorBinding::Frozen(float_less) = float_less
        else {
            panic!("`float.compare_less_than` 必须冻结");
        };
        assert!(
            float_less.encoding.instructions(0).iter().any(|instruction| matches!(instruction, JvmInstruction::FCmpG)),
            "float `<` 必须用 fcmpg 处理 NaN"
        );

        // 未登记的比较助记符仍保持 Unfrozen，禁止按名字前缀猜指令。
        let unfrozen = freeze_jvm_operator(
            &link("double.compare_less_than_unsigned"),
            instance,
            "infix <=",
            &f64_pair,
            &SemanticType::Boolean,
        )
        .expect("冻结不得失败")
        .expect("必须是 JVM operator 绑定");
        assert!(matches!(unfrozen, JvmOperatorBinding::Unfrozen));

        // `<=` / `>` / `>=`：NaN 必须分支到 false，故 `<`/`<=` 用 `?cmpg`、`>`/`>=` 用 `?cmpl`。
        let double_less_or_equal = freeze_jvm_operator(
            &link("double.compare_less_or_equal"),
            instance,
            "infix <=",
            &f64_pair,
            &SemanticType::Boolean,
        )
        .expect("冻结不得失败")
        .expect("必须是 JVM operator 绑定");
        let JvmOperatorBinding::Frozen(double_less_or_equal) = double_less_or_equal
        else {
            panic!("`double.compare_less_or_equal` 必须冻结");
        };
        assert!(
            double_less_or_equal
                .encoding
                .instructions(0)
                .iter()
                .any(|instruction| matches!(instruction, JvmInstruction::DCmpG)),
            "double `<=` 必须用 dcmpg 处理 NaN"
        );
        assert!(double_less_or_equal
            .encoding
            .instructions(0)
            .iter()
            .any(|instruction| matches!(instruction, JvmInstruction::IfGt(_))));

        let double_greater_than = freeze_jvm_operator(
            &link("double.compare_greater_than"),
            instance,
            "infix >",
            &f64_pair,
            &SemanticType::Boolean,
        )
        .expect("冻结不得失败")
        .expect("必须是 JVM operator 绑定");
        let JvmOperatorBinding::Frozen(double_greater_than) = double_greater_than
        else {
            panic!("`double.compare_greater_than` 必须冻结");
        };
        assert!(
            double_greater_than
                .encoding
                .instructions(0)
                .iter()
                .any(|instruction| matches!(instruction, JvmInstruction::DCmpL)),
            "double `>` 必须用 dcmpl 处理 NaN"
        );
        assert!(double_greater_than
            .encoding
            .instructions(0)
            .iter()
            .any(|instruction| matches!(instruction, JvmInstruction::IfLe(_))));

        let double_greater_or_equal = freeze_jvm_operator(
            &link("double.compare_greater_or_equal"),
            instance,
            "infix >=",
            &f64_pair,
            &SemanticType::Boolean,
        )
        .expect("冻结不得失败")
        .expect("必须是 JVM operator 绑定");
        let JvmOperatorBinding::Frozen(double_greater_or_equal) = double_greater_or_equal
        else {
            panic!("`double.compare_greater_or_equal` 必须冻结");
        };
        assert!(
            double_greater_or_equal
                .encoding
                .instructions(0)
                .iter()
                .any(|instruction| matches!(instruction, JvmInstruction::DCmpL)),
            "double `>=` 必须用 dcmpl 处理 NaN"
        );
        assert!(double_greater_or_equal
            .encoding
            .instructions(0)
            .iter()
            .any(|instruction| matches!(instruction, JvmInstruction::IfLt(_))));

        let float_pair = [SemanticType::Float32, SemanticType::Float32];
        let float_less_or_equal = freeze_jvm_operator(
            &link("float.compare_less_or_equal"),
            instance,
            "infix <=",
            &float_pair,
            &SemanticType::Boolean,
        )
        .expect("冻结不得失败")
        .expect("必须是 JVM operator 绑定");
        let JvmOperatorBinding::Frozen(float_less_or_equal) = float_less_or_equal
        else {
            panic!("`float.compare_less_or_equal` 必须冻结");
        };
        assert!(
            float_less_or_equal
                .encoding
                .instructions(0)
                .iter()
                .any(|instruction| matches!(instruction, JvmInstruction::FCmpG)),
            "float `<=` 必须用 fcmpg 处理 NaN"
        );
        assert!(float_less_or_equal
            .encoding
            .instructions(0)
            .iter()
            .any(|instruction| matches!(instruction, JvmInstruction::IfGt(_))));

        let float_greater_than = freeze_jvm_operator(
            &link("float.compare_greater_than"),
            instance,
            "infix >",
            &float_pair,
            &SemanticType::Boolean,
        )
        .expect("冻结不得失败")
        .expect("必须是 JVM operator 绑定");
        let JvmOperatorBinding::Frozen(float_greater_than) = float_greater_than
        else {
            panic!("`float.compare_greater_than` 必须冻结");
        };
        assert!(
            float_greater_than
                .encoding
                .instructions(0)
                .iter()
                .any(|instruction| matches!(instruction, JvmInstruction::FCmpL)),
            "float `>` 必须用 fcmpl 处理 NaN"
        );
        assert!(float_greater_than
            .encoding
            .instructions(0)
            .iter()
            .any(|instruction| matches!(instruction, JvmInstruction::IfLe(_))));

        let float_greater_or_equal = freeze_jvm_operator(
            &link("float.compare_greater_or_equal"),
            instance,
            "infix >=",
            &float_pair,
            &SemanticType::Boolean,
        )
        .expect("冻结不得失败")
        .expect("必须是 JVM operator 绑定");
        let JvmOperatorBinding::Frozen(float_greater_or_equal) = float_greater_or_equal
        else {
            panic!("`float.compare_greater_or_equal` 必须冻结");
        };
        assert!(
            float_greater_or_equal
                .encoding
                .instructions(0)
                .iter()
                .any(|instruction| matches!(instruction, JvmInstruction::FCmpL)),
            "float `>=` 必须用 fcmpl 处理 NaN"
        );
        assert!(float_greater_or_equal
            .encoding
            .instructions(0)
            .iter()
            .any(|instruction| matches!(instruction, JvmInstruction::IfLt(_))));
    }

    /// 分支指令的稳定名（仅比较序列出现的方向分支）。
    fn branch_kind(instruction: &JvmInstruction) -> Option<&'static str> {
        Some(match instruction {
            JvmInstruction::IfICmpLt(_) => "if_icmplt",
            JvmInstruction::IfICmpLe(_) => "if_icmple",
            JvmInstruction::IfICmpGt(_) => "if_icmpgt",
            JvmInstruction::IfICmpGe(_) => "if_icmpge",
            JvmInstruction::IfLt(_) => "iflt",
            JvmInstruction::IfLe(_) => "ifle",
            JvmInstruction::IfGt(_) => "ifgt",
            JvmInstruction::IfGe(_) => "ifge",
            _ => return None,
        })
    }

    /// 断言符号位翻转发生在其后的比较分支之前（`If*` 之外至少含一次 `IXor`/`LXor`）。
    fn assert_sign_bit_flip_precedes_branch(instructions: &[JvmInstruction], flip: fn(&JvmInstruction) -> bool) {
        let flip_index = instructions
            .iter()
            .position(|instruction| flip(instruction))
            .unwrap_or_else(|| panic!("无符号有序比较必须含符号位翻转: {instructions:?}"));
        let branch_index = instructions
            .iter()
            .position(|instruction| branch_kind(instruction).is_some())
            .unwrap_or_else(|| panic!("无符号有序比较必须含分支指令: {instructions:?}"));
        assert!(flip_index < branch_index, "符号位翻转必须在比较分支之前: {instructions:?}");
    }

    /// 无符号有序比较的其余三项：`u32`/`u64` 都冻结为「翻转符号位后按有符号比较」，
    /// 分支方向取翻转后条件的反（跳到 false），结果载体固定 `I32`。
    #[test]
    fn unsigned_ordered_comparison_mnemonics_freeze_with_sign_bit_flip() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let i32_pair = [SemanticType::Integer32 { signed: false }, SemanticType::Integer32 { signed: false }];
        let i64_pair = [SemanticType::Integer64 { signed: false }, SemanticType::Integer64 { signed: false }];

        for (mnemonic, code, expected_branch) in [
            ("integer.compare_greater_than_unsigned", JvmOperatorCode::IntegerCompareGreaterThanUnsigned, "if_icmple"),
            ("integer.compare_less_or_equal_unsigned", JvmOperatorCode::IntegerCompareLessOrEqualUnsigned, "if_icmpgt"),
            ("integer.compare_greater_or_equal_unsigned", JvmOperatorCode::IntegerCompareGreaterOrEqualUnsigned, "if_icmplt"),
        ] {
            let binding = freeze_jvm_operator(&link(mnemonic), instance, "infix cmp", &i32_pair, &SemanticType::Boolean)
                .expect("冻结不得失败")
                .expect("必须是 JVM operator 绑定");
            let JvmOperatorBinding::Frozen(plan) = binding
            else {
                panic!("`{mnemonic}` 必须冻结");
            };
            assert_eq!(plan.encoding, code, "`{mnemonic}` 编码");
            assert_eq!(plan.operand_type, TargetOperandType::I32);
            assert_eq!(plan.result_type, TargetOperandType::I32, "`{mnemonic}` 结果载体必须是 I32");
            let instructions = plan.encoding.instructions(0);
            assert!(
                instructions.iter().any(|instruction| matches!(instruction, JvmInstruction::Swap)),
                "`{mnemonic}` 必须经 `swap` 周转翻转两侧: {instructions:?}"
            );
            assert_sign_bit_flip_precedes_branch(&instructions, |instruction| matches!(instruction, JvmInstruction::IXor));
            let branch = instructions.iter().find_map(branch_kind);
            assert_eq!(branch, Some(expected_branch), "`{mnemonic}` 分支方向: {instructions:?}");
        }

        const SCRATCH_SLOT: u16 = 11;
        for (mnemonic, code, expected_branch) in [
            ("long.compare_greater_than_unsigned", JvmOperatorCode::LongCompareGreaterThanUnsigned, "ifle"),
            ("long.compare_less_or_equal_unsigned", JvmOperatorCode::LongCompareLessOrEqualUnsigned, "ifgt"),
            ("long.compare_greater_or_equal_unsigned", JvmOperatorCode::LongCompareGreaterOrEqualUnsigned, "iflt"),
        ] {
            let binding = freeze_jvm_operator(&link(mnemonic), instance, "infix cmp", &i64_pair, &SemanticType::Boolean)
                .expect("冻结不得失败")
                .expect("必须是 JVM operator 绑定");
            let JvmOperatorBinding::Frozen(plan) = binding
            else {
                panic!("`{mnemonic}` 必须冻结");
            };
            assert_eq!(plan.encoding, code, "`{mnemonic}` 编码");
            assert_eq!(plan.operand_type, TargetOperandType::I64);
            assert_eq!(plan.result_type, TargetOperandType::I32, "`{mnemonic}` 结果载体必须是 I32");
            assert!(plan.encoding.requires_scratch_long_slot(), "`{mnemonic}` 必须声明 scratch long 槽");
            let instructions = plan.encoding.instructions(SCRATCH_SLOT);
            assert_sign_bit_flip_precedes_branch(&instructions, |instruction| matches!(instruction, JvmInstruction::LXor));
            assert!(
                instructions.iter().any(|instruction| matches!(instruction, JvmInstruction::LStore(SCRATCH_SLOT))),
                "`{mnemonic}` 必须把翻转结果周转进 scratch 槽: {instructions:?}"
            );
            assert!(
                instructions.iter().any(|instruction| matches!(instruction, JvmInstruction::LLoad(SCRATCH_SLOT))),
                "`{mnemonic}` 必须从 scratch 槽取回翻转结果: {instructions:?}"
            );
            let lcmp_index = instructions
                .iter()
                .position(|instruction| matches!(instruction, JvmInstruction::LCmp))
                .unwrap_or_else(|| panic!("`{mnemonic}` 必须 `lcmp` 后分支: {instructions:?}"));
            let branch = instructions.iter().position(|instruction| branch_kind(instruction).is_some());
            assert!(branch.is_some_and(|index| lcmp_index < index), "`{mnemonic}` 必须先 `lcmp` 再分支: {instructions:?}");
            assert_eq!(instructions.iter().find_map(branch_kind), Some(expected_branch), "`{mnemonic}` 分支方向: {instructions:?}");
        }
    }
}
