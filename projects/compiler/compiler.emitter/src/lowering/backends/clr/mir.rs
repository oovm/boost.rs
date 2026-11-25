//! CLR 数值指令绑定的唯一助记符表。
//!
//! `[clr_operator("i64.div")]` 只声明目标 operator 助记符；助记符到**冻结配方**
//! 的映射归 CLR backend 所有，并在 `TargetExecutionPlan` 构造时一次性写入计划
//! （arity、操作数发射配方、操作数/结果载体与 MSIL opcode）。emit 侧只编码计划
//! 里的配方，不再从助记符字符串推断 arity 或操作数改写。
//!
//! MSIL 算术/比较 opcode 不区分数值载体，助记符因此必须携带载体限定名
//! （`i32.add` / `i64.add` / `f64.add`），冻结表按限定名给出唯一配方。
//! 比较冻结 `eq/lt/gt` 的单 opcode（`ceq`/`clt`/`cgt`）；`ne`/`le`/`ge` 在 IL 上
//! 没有独立指令，冻结为「一条比较 opcode + `ldc.i4.0` + `ceq`」的取反组合：
//! 第二次判等必须用 `ceq`（0/1 与 0 判等即逻辑非），不能复用首次比较 opcode，
//! 否则 `cgt`/`clt` 与 0 比较会得到「结果 > 0 / < 0」，把 `le`/`ge` 误算成
//! `gt`/`lt`。
//!
//! 整数载体没有 NaN，`le`/`ge` 直接用有符号 `cgt`/`clt` 的取反组合。`f64` 有 NaN，
//! IEEE 要求任一操作数为 NaN 时 `<=`/`>=` 为 false；而有符号 `cgt`/`clt` 遇 NaN 得
//! 0，取反会错误地得到 true，因此 `f64.le`/`f64.ge` 必须使用无序变体
//! `cgt.un`/`clt.un`（它们在无序时得 1，取反后为 0）。
//!
//! 无符号有序比较同理：IL 没有独立的 `le.un`/`ge.un`，无符号 `le = !(a > b)`、
//! `ge = !(a < b)`，首次比较用无符号 `cgt.un`/`clt.un`，第二次判等仍固定 `ceq`。
//!
//! 浮点取余不是缺口：ECMA-335 的 `rem` 对浮点操作数定义为截断余数，符号随被除数，
//! 与 C# `%`、Java `%`、C `fmod` 一致。实测 .NET 8 上 `(-5.5) % 2.0 = -1.5`、
//! `5.5 % 2.0 = 1.5`，且 C# 的两处 `%` 都编译成 IL `rem`（字节 `0x5D`）；
//! `Math.IEEERemainder` 才是 IEEE 754 remainder，与本表无关。
//!
//! `f32` 独立载体暂无冻结项：emit 侧槽位/形参/返回已支持 `float32`，但 MSIL 模型
//! 没有 `ldc.r4`（也没有 `conv.r4`），`float32` 槽无法接收浮点常量，故不冻结
//! `f32.*`，待常量载体补齐后再加。

use crate::{
    target_operator::{TargetOperandStep, TargetOperandType, TargetOperatorPlan, TargetOperatorRecipe},
    transitional::msil::MsilOpcode,
};

/// CLR operator 的冻结编码。
///
/// 算术/比较多数是单条 MSIL opcode；`ne`/`le`/`ge` 需要把 0/1 结果精确取反，
/// 因此编码必须显式区分「单指令」与「取反组合」，emit 只机械展开，不得按助记符
/// 或名字前缀推断序列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClrOperatorCode {
    /// 单条 MSIL opcode。
    Single(MsilOpcode),
    /// 布尔取反：先执行 `compare` 得到 0/1，再压 `ldc.i4.0` 并用 `ceq` 与 0 判等。
    ///
    /// 取反必须用 `ceq`，不能复用 `compare`：`cgt`/`clt` 与 0 比较得到的是
    /// 「结果 > 0 / 结果 < 0」而不是「结果 == 0」，复用会把 `le`/`ge` 误算成
    /// `gt`/`lt`。`ceq` 对 0/1 恰是逻辑非，对任意 `compare` 都成立。
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

/// `ne`/`le`/`ge` 系列：IL 没有独立指令，冻结为单条比较 opcode 的精确逻辑取反组合。
const fn inverted(operand_type: TargetOperandType, result_type: TargetOperandType, opcode: MsilOpcode) -> TargetOperatorPlan<ClrOperatorCode> {
    TargetOperatorPlan {
        arity: 2,
        operand_type,
        result_type,
        recipe: TargetOperatorRecipe::Operands,
        encoding: ClrOperatorCode::InvertCompare(opcode),
    }
}

/// `ne` 系列：IL 没有独立不等指令，冻结为 `ceq` 的精确逻辑取反组合。
const fn not_equal(operand_type: TargetOperandType, result_type: TargetOperandType) -> TargetOperatorPlan<ClrOperatorCode> {
    inverted(operand_type, result_type, MsilOpcode::Ceq)
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
        // 有符号 `i32` 右移是算术 `shr`；`u32` 必须走 `shr.un`，两者不可互换。
        "i32.shr" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::Shr),
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
        // 浮点取余：ECMA-335 `rem` 对浮点操作数就是截断余数（符号随被除数），与
        // C# `%`/Java `%`/`fmod` 一致；实测 .NET 8 `(-5.5) % 2.0 = -1.5` 且编译为 `rem`。
        "f64.rem" => binary(TargetOperandType::F64, TargetOperandType::F64, MsilOpcode::Rem),
        "f64.neg" => unary(TargetOperandType::F64, MsilOpcode::Neg),
        // 比较：结果固定为 I32（0/1）；eq/lt/gt 是单 opcode，ne/le/ge 是取反组合。
        "i32.eq" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::Ceq),
        "i32.ne" => not_equal(TargetOperandType::I32, TargetOperandType::I32),
        "i32.lt" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::Clt),
        // 无符号 `int32` 小于：MSIL 原生 `clt.un`。
        "i32.lt.un" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::CltUn),
        "i32.gt" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::Cgt),
        // 整数载体没有 NaN，`le = !(a > b)`、`ge = !(a < b)`，取反有符号 `cgt`/`clt`。
        "i32.le" => inverted(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::Cgt),
        "i32.ge" => inverted(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::Clt),
        // 无符号有序比较：IL 没有独立 `le.un`/`ge.un`，无符号 `le = !(a > b)`、
        // `ge = !(a < b)`，首次比较取无符号 `cgt.un`/`clt.un`，第二次判等仍固定 `ceq`。
        "i32.gt.un" => binary(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::CgtUn),
        "i32.le.un" => inverted(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::CgtUn),
        "i32.ge.un" => inverted(TargetOperandType::I32, TargetOperandType::I32, MsilOpcode::CltUn),
        "i64.eq" => binary(TargetOperandType::I64, TargetOperandType::I32, MsilOpcode::Ceq),
        "i64.ne" => not_equal(TargetOperandType::I64, TargetOperandType::I32),
        "i64.lt" => binary(TargetOperandType::I64, TargetOperandType::I32, MsilOpcode::Clt),
        // 无符号 `int64` 小于：MSIL `clt.un` 对宽载体同形（ECMA-335 的 `clt.un` 接受 int64）。
        "i64.lt.un" => binary(TargetOperandType::I64, TargetOperandType::I32, MsilOpcode::CltUn),
        "i64.gt" => binary(TargetOperandType::I64, TargetOperandType::I32, MsilOpcode::Cgt),
        // int64 同样没有 NaN，`le`/`ge` 取反有符号 `cgt`/`clt`，结果槽仍是 I32。
        "i64.le" => inverted(TargetOperandType::I64, TargetOperandType::I32, MsilOpcode::Cgt),
        "i64.ge" => inverted(TargetOperandType::I64, TargetOperandType::I32, MsilOpcode::Clt),
        // int64 无符号有序比较：`clt.un`/`cgt.un` 对宽载体同形，结果槽仍是 I32。
        "i64.gt.un" => binary(TargetOperandType::I64, TargetOperandType::I32, MsilOpcode::CgtUn),
        "i64.le.un" => inverted(TargetOperandType::I64, TargetOperandType::I32, MsilOpcode::CgtUn),
        "i64.ge.un" => inverted(TargetOperandType::I64, TargetOperandType::I32, MsilOpcode::CltUn),
        "f64.eq" => binary(TargetOperandType::F64, TargetOperandType::I32, MsilOpcode::Ceq),
        "f64.ne" => not_equal(TargetOperandType::F64, TargetOperandType::I32),
        "f64.lt" => binary(TargetOperandType::F64, TargetOperandType::I32, MsilOpcode::Clt),
        "f64.gt" => binary(TargetOperandType::F64, TargetOperandType::I32, MsilOpcode::Cgt),
        // 浮点有 NaN：IEEE 要求任一操作数为 NaN 时 `<=`/`>=` 为 false，而有符号
        // `cgt`/`clt` 遇 NaN 得 0、取反得 1（错误）；`cgt.un`/`clt.un` 在无序时得 1，
        // 取反后才是 0，因此 `f64.le`/`f64.ge` 必须用无序变体。
        "f64.le" => inverted(TargetOperandType::F64, TargetOperandType::I32, MsilOpcode::CgtUn),
        "f64.ge" => inverted(TargetOperandType::F64, TargetOperandType::I32, MsilOpcode::CltUn),
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
    /// `ne` 冻结为 `ceq; ldc.i4.0; ceq` 取反组合，`le`/`ge` 冻结为取反组合且浮点
    /// 与无符号比较必须用无序 `cgt.un`/`clt.un`；`f64.rem` 冻结为 `rem`。
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

        // `le`/`ge` 没有独立 IL 指令：整数取反有符号 `cgt`/`clt`，浮点必须取反无序
        // `cgt.un`/`clt.un`，否则 NaN 会被错误判为 true。
        for (mnemonic, operand, result, encoding) in [
            ("i32.le", i32_ty.clone(), SemanticType::Boolean, ClrOperatorCode::InvertCompare(MsilOpcode::Cgt)),
            ("i32.ge", i32_ty.clone(), SemanticType::Boolean, ClrOperatorCode::InvertCompare(MsilOpcode::Clt)),
            ("i64.le", i64_ty.clone(), SemanticType::Boolean, ClrOperatorCode::InvertCompare(MsilOpcode::Cgt)),
            ("i64.ge", i64_ty.clone(), SemanticType::Boolean, ClrOperatorCode::InvertCompare(MsilOpcode::Clt)),
            ("f64.le", SemanticType::Float64, SemanticType::Boolean, ClrOperatorCode::InvertCompare(MsilOpcode::CgtUn)),
            ("f64.ge", SemanticType::Float64, SemanticType::Boolean, ClrOperatorCode::InvertCompare(MsilOpcode::CltUn)),
        ] {
            let plan = freeze_clr_operator(&link(mnemonic), instance, "infix cmp", &[operand.clone(), operand], &result)
                .unwrap_or_else(|error| panic!("`{mnemonic}` 冻结不得失败: {error}"))
                .unwrap_or_else(|| panic!("`{mnemonic}` 必须是 CLR operator 绑定"));
            let ClrOperatorBinding::Frozen(plan) = plan
            else {
                panic!("`{mnemonic}` 必须冻结");
            };
            assert_eq!(plan.encoding, encoding, "`{mnemonic}` 编码");
            assert_eq!(plan.result_type, crate::target_operator::TargetOperandType::I32, "`{mnemonic}` 结果槽");
        }

        // 无符号有序比较：`gt` 是单条 `cgt.un`，`le = !(a > b)`、`ge = !(a < b)` 取反
        // 无符号 `cgt.un`/`clt.un`，结果槽仍是 I32。
        let u32_ty = SemanticType::Integer32 { signed: false };
        let u64_ty = SemanticType::Integer64 { signed: false };
        for (mnemonic, operand, encoding) in [
            ("i32.gt.un", u32_ty.clone(), ClrOperatorCode::Single(MsilOpcode::CgtUn)),
            ("i32.le.un", u32_ty.clone(), ClrOperatorCode::InvertCompare(MsilOpcode::CgtUn)),
            ("i32.ge.un", u32_ty.clone(), ClrOperatorCode::InvertCompare(MsilOpcode::CltUn)),
            ("i64.gt.un", u64_ty.clone(), ClrOperatorCode::Single(MsilOpcode::CgtUn)),
            ("i64.le.un", u64_ty.clone(), ClrOperatorCode::InvertCompare(MsilOpcode::CgtUn)),
            ("i64.ge.un", u64_ty.clone(), ClrOperatorCode::InvertCompare(MsilOpcode::CltUn)),
        ] {
            let plan = freeze_clr_operator(&link(mnemonic), instance, "infix cmp", &[operand.clone(), operand], &SemanticType::Boolean)
                .unwrap_or_else(|error| panic!("`{mnemonic}` 冻结不得失败: {error}"))
                .unwrap_or_else(|| panic!("`{mnemonic}` 必须是 CLR operator 绑定"));
            let ClrOperatorBinding::Frozen(plan) = plan
            else {
                panic!("`{mnemonic}` 必须冻结");
            };
            assert_eq!(plan.encoding, encoding, "`{mnemonic}` 编码");
            assert_eq!(plan.result_type, crate::target_operator::TargetOperandType::I32, "`{mnemonic}` 结果槽");
        }

        // 浮点 `rem` 不是缺口：ECMA-335 `rem` 对浮点就是截断余数（符号随被除数），
        // 与 C# `%`/Java `%`/`fmod` 一致；实测 .NET 8 `(-5.5) % 2.0 = -1.5` 且编译为 `rem`。
        let remainder = freeze_clr_operator(&link("f64.rem"), instance, "infix %", &[SemanticType::Float64, SemanticType::Float64], &SemanticType::Float64)
            .expect("冻结不得失败")
            .expect("必须是 CLR operator 绑定");
        let ClrOperatorBinding::Frozen(remainder) = remainder
        else {
            panic!("`f64.rem` 必须冻结");
        };
        assert_eq!(remainder.encoding, ClrOperatorCode::Single(MsilOpcode::Rem));
        assert_eq!(remainder.result_type, crate::target_operator::TargetOperandType::F64);

        // 有符号右移是算术 `shr`，无符号右移必须是 `shr.un`：两者不可互换，否则
        // `u32 >> n` 会被错误地按符号位填充。
        for (mnemonic, operand, encoding) in [
            ("i32.shl", i32_ty.clone(), MsilOpcode::Shl),
            ("i32.shr", i32_ty.clone(), MsilOpcode::Shr),
            ("i32.shr.un", SemanticType::Integer32 { signed: false }, MsilOpcode::ShrUn),
        ] {
            let plan = freeze_clr_operator(&link(mnemonic), instance, "infix shift", &[operand.clone(), operand.clone()], &operand)
                .unwrap_or_else(|error| panic!("`{mnemonic}` 冻结不得失败: {error}"))
                .unwrap_or_else(|| panic!("`{mnemonic}` 必须是 CLR operator 绑定"));
            let ClrOperatorBinding::Frozen(plan) = plan
            else {
                panic!("`{mnemonic}` 必须冻结");
            };
            assert_eq!(plan.encoding, ClrOperatorCode::Single(encoding), "`{mnemonic}` 编码");
        }

        // 真正未知的助记符仍然只能得到 Unfrozen。
        let unfrozen = freeze_clr_operator(&link("f64.rem.unknown"), instance, "infix %", &[SemanticType::Float64, SemanticType::Float64], &SemanticType::Float64)
            .expect("冻结不得失败")
            .expect("必须是 CLR operator 绑定");
        assert!(matches!(unfrozen, ClrOperatorBinding::Unfrozen));
    }
}
