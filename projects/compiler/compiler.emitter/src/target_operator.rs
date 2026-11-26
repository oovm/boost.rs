//! 目标 operator 能力绑定的**通用配方词汇**。
//!
//! 这里只保留与具体 ISA 无关的机械步骤（arity、操作数/结果载体、发射配方）；
//! 具体编码 `E` 由各 lane 自己的类型参数提供，本模块不得出现囊括多平台的
//! tagged encoding 枚举。位宽转换与栈动作同样是 lane 私有物理事实：JVM 的
//! `l2i`、MSIL 的 `conv.i4` 冻结在各自编码展开里，公共词汇不携带截窄步骤。`operator` 表示**目标后端能力**，不是语言前端层的
//! operator：一个绑定可以是一条指令、多条指令组合、参数重排或常量注入。
//!
//! 助记符到配方的冻结发生在**各 lane 自己的入口**；共享计划只携带逻辑导入
//! 链接，不携带任何平台的冻结结果。

/// 目标标量载体。
///
/// `F32` 是**独立载体**，不是 `F64` 的别名，也不是 `I32` 的别名：
/// 32 位浮点的宽度、NaN payload 宽度与 `-0.0` 符号位都由它自己承诺。
/// 任何目标都不得把 `f32` 值存进 `F64` 槽再搬回，也不得借 `I32` 槽做数值转换
/// （位模式重解释是唯一允许的 `F32 ↔ I32` 通路，且必须显式冻结在配方里）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOperandType {
    /// 32 位整数载体。
    I32,
    /// 64 位整数载体。
    I64,
    /// 32 位浮点载体。
    F32,
    /// 64 位浮点载体。
    F64,
}

/// 已冻结配方中的一步发射动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOperandStep {
    /// 按实参顺序发射全部实参。
    Arguments,
    /// 发射一个载体化的整数常量；整数取负所需的零常量在此显式冻结。
    IntegerConstant {
        /// 常量在目标栈上的标量载体。
        operand_type: TargetOperandType,
        /// 常量值。
        value: i32,
    },
}

/// 已冻结的 operator 发射配方。
///
/// operator 不保证是单条指令：配方是 backend 唯一允许执行的机械步骤序列。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOperatorRecipe {
    /// 按实参顺序发射全部实参，再执行冻结编码。
    Operands,
    /// 按给定顺序发射每一步，再执行冻结编码；backend 只机械执行配方，不按助记符
    /// 推断取负、零常量或减法。
    Steps(&'static [TargetOperandStep]),
}

/// 一个已冻结的目标 operator 计划。
///
/// 计划冻结调用点所需的全部物理事实：实参个数、操作数载体、结果载体、发射配方与
/// 编码。`E` 是该 lane 自己的编码类型；backend 不得在此基础上再做任何推断或补全。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetOperatorPlan<E> {
    /// 冻结的实参个数；编码前必须与调用实参个数一致。
    pub arity: usize,
    /// 实参的目标标量载体。
    pub operand_type: TargetOperandType,
    /// 结果的目标标量载体。
    pub result_type: TargetOperandType,
    /// 操作数发射配方。
    pub recipe: TargetOperatorRecipe,
    /// operator 编码（lane 私有类型）。
    pub encoding: E,
}

#[cfg(test)]
mod frozen_operator_tests {
    use super::TargetOperandType;
    use crate::{
        lowering::backends::{
            clr::mir::{ClrOperatorCode, freeze_clr_operator_binding},
            jvm::{JvmOperatorCode, recipe::freeze_jvm_operator_binding},
            machine::x86_64::{X86_64OperatorCode, freeze_x86_64_operator_binding},
        },
        transitional::msil::MsilOpcode,
    };

    /// `u64` 无符号小于的托管目标配方：CLR 冻结为原生 `clt.un`，JVM 冻结为符号位
    /// 翻转展开码；操作数载体 `I64`、结果槽 `I32`，与 `__u64_lt` 的声明签名一致。
    #[test]
    fn unsigned_u64_less_than_freezes_managed_operator_plans() {
        let clr_plan = freeze_clr_operator_binding("i64.lt.un").expect("`i64.lt.un` 必须冻结");
        assert_eq!(clr_plan.arity, 2);
        assert_eq!(clr_plan.operand_type, TargetOperandType::I64);
        assert_eq!(clr_plan.result_type, TargetOperandType::I32);
        assert_eq!(clr_plan.encoding, ClrOperatorCode::Single(MsilOpcode::CltUn));

        let jvm_plan = freeze_jvm_operator_binding("long.compare_less_than_unsigned").expect("`long.compare_less_than_unsigned` 必须冻结");
        assert_eq!(jvm_plan.arity, 2);
        assert_eq!(jvm_plan.operand_type, TargetOperandType::I64);
        assert_eq!(jvm_plan.result_type, TargetOperandType::I32);
        assert_eq!(jvm_plan.encoding, JvmOperatorCode::LongCompareLessThanUnsigned);
    }

    /// `x86_64_operator(...)` 按 ISA 冻结编码：`long.*` 冻结为 x86-64 码，
    /// 未实现的 `integer.*` 与未声明 ISA 的标签不得被猜成任何指令集。
    #[test]
    fn x86_64_operator_links_freeze_isa_encodings() {
        let plan = freeze_x86_64_operator_binding("long.add").expect("`long.add` 必须冻结");
        assert_eq!(plan.arity, 2);
        assert_eq!(plan.operand_type, TargetOperandType::I64);
        assert_eq!(plan.result_type, TargetOperandType::I64);
        assert_eq!(plan.encoding, X86_64OperatorCode::I64Add);

        let plan = freeze_x86_64_operator_binding("long.compare_less_than").expect("`long.compare_less_than` 必须冻结");
        assert_eq!(plan.result_type, TargetOperandType::I32);
        assert_eq!(plan.encoding, X86_64OperatorCode::I64CompareLessThan);

        // 32 位整数算术：载体 `I32` → `I32`。
        let plan = freeze_x86_64_operator_binding("integer.add").expect("`integer.add` 必须冻结");
        assert_eq!(plan.operand_type, TargetOperandType::I32);
        assert_eq!(plan.result_type, TargetOperandType::I32);
        assert_eq!(plan.encoding, X86_64OperatorCode::I32Add);

        // 无符号除余：已冻结为 `div`/`idiv` 形态，与有符号除余同宽同载体。
        let plan = freeze_x86_64_operator_binding("integer.divide_unsigned").expect("`integer.divide_unsigned` 必须冻结");
        assert_eq!(plan.operand_type, TargetOperandType::I32);
        assert_eq!(plan.encoding, X86_64OperatorCode::I32DivideUnsigned);
        let plan = freeze_x86_64_operator_binding("long.remainder").expect("`long.remainder` 必须冻结");
        assert_eq!(plan.operand_type, TargetOperandType::I64);
        assert_eq!(plan.encoding, X86_64OperatorCode::I64Remainder);

        // 浮点载体已进入 x86-64 冻结集：`double.*` / `float.*` 冻结为 SSE2 码。
        let plan = freeze_x86_64_operator_binding("double.add").expect("`double.add` 必须冻结");
        assert_eq!(plan.operand_type, TargetOperandType::F64);
        assert_eq!(plan.result_type, TargetOperandType::F64);
        assert_eq!(plan.encoding, X86_64OperatorCode::F64Add);
        let plan = freeze_x86_64_operator_binding("float.compare_less_than").expect("`float.compare_less_than` 必须冻结");
        assert_eq!(plan.operand_type, TargetOperandType::F32);
        assert_eq!(plan.result_type, TargetOperandType::I32);
        assert_eq!(plan.encoding, X86_64OperatorCode::F32CompareLessThan);
        // 未冻结的助记符仍不得被猜成任何指令集。
        assert!(freeze_x86_64_operator_binding("half.add").is_none(), "未实现 provider 不得冻结");
    }

    /// `u64`/`u32` 无符号除、余、逻辑右移的托管目标配方：
    /// CLR 冻结为原生 `div.un`/`rem.un`/`shr.un`；JVM 冻结为 `lushr`/`iushr` 或
    /// `Integer/Long.divideUnsigned/remainderUnsigned` 静态调用。
    #[test]
    fn unsigned_div_rem_shift_freeze_managed_operator_plans() {
        for (mnemonic, operand, result, encoding) in [
            ("i32.div.un", TargetOperandType::I32, TargetOperandType::I32, ClrOperatorCode::Single(MsilOpcode::DivUn)),
            ("i32.rem.un", TargetOperandType::I32, TargetOperandType::I32, ClrOperatorCode::Single(MsilOpcode::RemUn)),
            ("i32.shr.un", TargetOperandType::I32, TargetOperandType::I32, ClrOperatorCode::Single(MsilOpcode::ShrUn)),
            ("i64.div.un", TargetOperandType::I64, TargetOperandType::I64, ClrOperatorCode::Single(MsilOpcode::DivUn)),
            ("i64.rem.un", TargetOperandType::I64, TargetOperandType::I64, ClrOperatorCode::Single(MsilOpcode::RemUn)),
            // `i64` 移位的 `conv.i4` 截窄是 CLR lane 私有物理事实，冻结在编码变体里。
            ("i64.shr.un", TargetOperandType::I64, TargetOperandType::I64, ClrOperatorCode::NarrowShift(MsilOpcode::ShrUn)),
        ] {
            let plan = freeze_clr_operator_binding(mnemonic).unwrap_or_else(|| panic!("`{mnemonic}` 必须冻结"));
            assert_eq!(plan.arity, 2);
            assert_eq!(plan.operand_type, operand);
            assert_eq!(plan.result_type, result);
            assert_eq!(plan.encoding, encoding, "`{mnemonic}` 编码");
        }

        for (mnemonic, operand, result, code) in [
            ("integer.divide_unsigned", TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerDivideUnsigned),
            ("integer.remainder_unsigned", TargetOperandType::I32, TargetOperandType::I32, JvmOperatorCode::IntegerRemainderUnsigned),
            ("long.divide_unsigned", TargetOperandType::I64, TargetOperandType::I64, JvmOperatorCode::LongDivideUnsigned),
            ("long.remainder_unsigned", TargetOperandType::I64, TargetOperandType::I64, JvmOperatorCode::LongRemainderUnsigned),
            ("long.shift_right_unsigned", TargetOperandType::I64, TargetOperandType::I64, JvmOperatorCode::LongShiftRightUnsigned),
        ] {
            let plan = freeze_jvm_operator_binding(mnemonic).unwrap_or_else(|| panic!("`{mnemonic}` 必须冻结"));
            assert_eq!(plan.arity, 2);
            assert_eq!(plan.operand_type, operand);
            assert_eq!(plan.result_type, result);
            assert_eq!(plan.encoding, code, "`{mnemonic}` 编码");
        }
    }

    /// 反例：近似但未冻结的助记符必须保持 `Unfrozen`，由 emit 侧结构化失败，
    /// 不得按名字前缀猜默认指令。
    #[test]
    fn near_miss_unsigned_mnemonics_stay_unfrozen() {
        for mnemonic in ["i64.div_unsigned", "i64.shr.un.extra"] {
            assert!(freeze_clr_operator_binding(mnemonic).is_none(), "`{mnemonic}` 不得冻结");
        }
        for mnemonic in ["long.div.un", "long.shift_right_unsigned.extra"] {
            assert!(freeze_jvm_operator_binding(mnemonic).is_none(), "`{mnemonic}` 不得冻结");
        }
    }
}
