//! OVM 目标私有的 operator 绑定与整数载体事实。
//!
//! 本模块只消费冻结的导入链接与语义类型，不读取任何跨目标共享程序
//! 载荷类型：
//! 声明侧 `[ovm_operator("...")]` 助记符在这里一次冻结成 Odin 头码配方，
//! 语言侧 OVM lane 按同一合同发射指令序列。

use compiler_target::ExternalImportLink;
use compiler_types::{ItemInstanceId, SemanticType};
use data_binary_odin::OdinHeadCode;

use crate::target_operator::{TargetOperandStep, TargetOperandType, TargetOperatorPlan, TargetOperatorRecipe};

/// 整数取负没有原生头码：计划显式冻结 `0 - x` 的发射配方。
pub const I32_NEGATE_STEPS: &[TargetOperandStep] =
    &[TargetOperandStep::IntegerConstant { operand_type: TargetOperandType::I32, value: 0 }, TargetOperandStep::Arguments];

/// OVM operator 的冻结编码。
///
/// 多数助记符是单条头码；IL-less 的 OVM 上 `f32.ne`/`f32.gt` 没有原生头码，但可
/// 用现有头码**保真组合**：组合必须是显式编码变体，emit 只机械展开，不得按助记符
/// 推断序列。`f32.ne` 的取反必须走分支（运行时比较压 `Value::Bool`，`I32Ne` 的
/// `pop_i32` 拒绝 Bool，不存在 Bool 取反头码）。`le`/`ge` 无法用有序 `f32.lt`
/// 组合保真（NaN 时 `!(a<b)` ≠ `a<=b`，而 OR 组合需要同一对实参参与两次比较，
/// 超出单尾码配方模型），保持缺口待头码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OvmOperatorCode {
    /// 单条 OVM 头码。
    Single(OdinHeadCode),
    /// 交换实参顺序后执行头码：`f32.gt(a,b)` = `F32Lt(b,a)`。
    ///
    /// 有序 `lt` 任一操作数为 NaN 得 0，与 IEEE `gt` 的 NaN 裁决一致，交换保真。
    SwapCompare(OdinHeadCode),
    /// 比较头码后用分支取反：`JumpIfFalse` 分岔到常量 1/0，汇合处留一个 i32。
    ///
    /// OVM 运行时比较头码压 `Value::Bool`，`I32Ne` 的 `pop_i32` 严格拒绝 Bool，
    /// 故不得用按位取反组合；`JumpIfFalse` 经 `to_bool` 消费比较结果是唯一保真路径。
    /// `f32.ne(a,b)` = `NOT(F32Eq(a,b))`，NaN 时 `eq` 得 0、取反得 1，与 IEEE `!=` 一致。
    InvertBoolean(OdinHeadCode),
}

/// `[ovm_operator("...")]` 目标助记符到**冻结配方**的唯一映射表。
///
/// 只覆盖 `OdinHeadCode` 已有的 i32 算术/位运算/比较变体，以及单精度 f32 块；
/// i64、f64 的算术与比较没有对应头码（`data.binary.odin` 的 `OdinHeadCode` 除
/// `I64ReinterpretF64`/`F64ReinterpretI64` 两个位搬运变体外，只有 `I32*`（0x30–0x49）
/// 与 `F32*`（0x64–0x6C），不存在 `I64Add`/`I64Eq`/`F64Add`/`F64Lt` 之类变体），
/// 必须保持宿主导入路径（由 runtime `parse_host_op` 决定是否存在）。
/// 映射在语言侧 OVM lane 按计划导入链接冻结时消费；未知助记符返回 `None`，由
/// lane 冻结为 `Unfrozen` 绑定，emit 结构化失败，禁止回落到宿主导入或默认指令。
///
/// 位模式重解释（`i64.reinterpret_f64` / `f64.reinterpret_i64`）登记为
/// `I64ReinterpretF64` / `F64ReinterpretI64` 头码：语义是纯位搬运，属于指令层，
/// 不引入宿主函数、不新增 capability；载体按声明签名冻结（`f64 ↔ i64`），不得降级
/// 成数值转换，也不得回流到宿主。
///
/// f32 同样走指令层，且**载体独立**：
///
/// 1. **载体身份**：f32 助记符的操作数/结果载体是 `TargetOperandType::F32`，
///    运行时值只存在于 `Value::F32`；禁止借 `I32` 或 `F64` 槽。
/// 2. **转换规则**：`f32.demote_f64` / `f64.promote_f32` 分别是 `F32DemoteF64` /
///    `F64PromoteF32` 单步头码（RNE / 精确）；本表不冻结隐式拓宽。
/// 3. **运算与位模式**：f32 算术/比较在单精度上做；`i32.reinterpret_f32` /
///    `f32.reinterpret_i32` 是 32 位 `i32` 载体的纯位搬运，NaN payload 23 位保位。
/// 4. **显式组合**：`f32.ne`/`f32.gt` 无原生头码但可保真组合，冻结为
///    `OvmOperatorCode::InvertBoolean`（分支取反，只用现有头码）/`SwapCompare`；
///    `f32.le`/`f32.ge` 在 NaN 下无法用有序 `F32Lt` 组合保真，保持缺口待头码，
///    不得回落宿主导入。
pub fn freeze_ovm_operator_binding(mnemonic: &str) -> Option<TargetOperatorPlan<OvmOperatorCode>> {
    // 跨载体助记符显式点名两侧载体；其余既有助记符两侧都是 i32。
    let (operand_type, result_type) = match mnemonic {
        "i64.reinterpret_f64" => (TargetOperandType::F64, TargetOperandType::I64),
        "f64.reinterpret_i64" => (TargetOperandType::I64, TargetOperandType::F64),
        "i32.reinterpret_f32" => (TargetOperandType::F32, TargetOperandType::I32),
        "f32.reinterpret_i32" => (TargetOperandType::I32, TargetOperandType::F32),
        "f64.promote_f32" => (TargetOperandType::F32, TargetOperandType::F64),
        "f32.demote_f64" => (TargetOperandType::F64, TargetOperandType::F32),
        m if m.starts_with("f32.") => (TargetOperandType::F32, TargetOperandType::F32),
        _ => (TargetOperandType::I32, TargetOperandType::I32),
    };
    let (arity, recipe, code) = match mnemonic {
        "i32.add" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32Add)),
        "i32.sub" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32Sub)),
        "i32.mul" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32Mul)),
        "i32.div" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32DivS)),
        "i32.rem" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32RemS)),
        "i32.and" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32And)),
        "i32.or" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32Or)),
        "i32.xor" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32Xor)),
        "i32.shl" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32Shl)),
        "i32.shr_s" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32ShrS)),
        "i32.shr_u" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32ShrU)),
        "i32.eq" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32Eq)),
        "i32.ne" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32Ne)),
        "i32.lt" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32LtS)),
        "i32.le" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32LeS)),
        "i32.gt" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32GtS)),
        "i32.ge" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32GeS)),
        "i32.lt_u" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32LtU)),
        "i32.le_u" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32LeU)),
        "i32.gt_u" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32GtU)),
        "i32.ge_u" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32GeU)),
        "i32.neg" => (1, TargetOperatorRecipe::Steps(I32_NEGATE_STEPS), OvmOperatorCode::Single(OdinHeadCode::I32Sub)),
        "i64.reinterpret_f64" => (1, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I64ReinterpretF64)),
        "f64.reinterpret_i64" => (1, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::F64ReinterpretI64)),
        "i32.reinterpret_f32" => (1, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::I32ReinterpretF32)),
        "f32.reinterpret_i32" => (1, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::F32ReinterpretI32)),
        "f32.add" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::F32Add)),
        "f32.sub" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::F32Sub)),
        "f32.mul" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::F32Mul)),
        "f32.div" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::F32Div)),
        "f32.neg" => (1, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::F32Neg)),
        "f32.eq" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::F32Eq)),
        "f32.lt" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::F32Lt)),
        // 无原生头码但可保真组合：`ne` 用分支取反 `eq` 结果，`gt` 交换操作数用 `lt`。
        "f32.ne" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::InvertBoolean(OdinHeadCode::F32Eq)),
        "f32.gt" => (2, TargetOperatorRecipe::Operands, OvmOperatorCode::SwapCompare(OdinHeadCode::F32Lt)),
        "f32.demote_f64" => (1, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::F32DemoteF64)),
        "f64.promote_f32" => (1, TargetOperatorRecipe::Operands, OvmOperatorCode::Single(OdinHeadCode::F64PromoteF32)),
        _ => return None,
    };
    // 比较结果固定为 i32 布尔：载体身份由助记符语义决定，不靠默认 i32 蒙混。
    let result_type = match mnemonic {
        "f32.eq" | "f32.lt" | "f32.ne" | "f32.gt" => TargetOperandType::I32,
        _ => result_type,
    };
    Some(TargetOperatorPlan { arity, operand_type, result_type, recipe, encoding: code })
}

/// 源码 `[ovm_operator("i32.add")]` 声明的 OVM 目标 operator 助记符。
///
/// 只接受 platform tag `ovm-operator` 且 locator 恰好一段；其余形态返回 `None`，
/// 由调用方按「缺少可编码目标绑定」结构化失败。
pub fn ovm_operator_target(link: &ExternalImportLink) -> Option<&str> {
    if !link.matches_boundary("host") || !link.matches_platform_tag("ovm-operator") {
        return None;
    }

    match link.locator_segments() {
        [mnemonic] => Some(mnemonic.as_str()),
        _ => None,
    }
}

/// 源码 `[ovm("symbol")]` 声明的 OVM host provider 符号。
///
/// OVM 的宿主 provider 走 `CallImport`，由 runtime `parse_host_op` 判定；只接受
/// host 边界 + platform tag `ovm` + 恰好一段符号，其余形态返回 `None`。
pub fn ovm_host_import_target(link: &ExternalImportLink) -> Option<&str> {
    if !link.matches_boundary("host") || !link.matches_platform_tag("ovm") {
        return None;
    }

    match link.locator_segments() {
        [symbol] => Some(symbol.as_str()),
        _ => None,
    }
}

/// OVM 平台私有的 operator 绑定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OvmOperatorBinding {
    /// 已冻结的 Odin 头码配方（含显式组合编码）。
    Frozen(TargetOperatorPlan<OvmOperatorCode>),
    /// 声明了 `[ovm_operator]`，但没有 OVM 可编码配方；emit 必须结构化失败。
    Unfrozen,
}

/// 按声明的导入签名冻结 OVM operator 绑定。
pub fn freeze_ovm_operator(
    link: &ExternalImportLink,
    instance: ItemInstanceId,
    symbol: &str,
    parameter_types: &[SemanticType],
    return_type: &SemanticType,
) -> crate::Result<Option<OvmOperatorBinding>> {
    let Some(mnemonic) = ovm_operator_target(link)
    else {
        return Ok(None);
    };
    let Some(plan) = freeze_ovm_operator_binding(mnemonic)
    else {
        return Ok(Some(OvmOperatorBinding::Unfrozen));
    };
    crate::target_operator::validate_frozen_operator(mnemonic, instance, symbol, parameter_types, return_type, &plan)?;
    Ok(Some(OvmOperatorBinding::Frozen(plan)))
}

/// 整数表示形状：只携带位宽与符号性，`Character` 视为 32 位无符号码位。
///
/// 转换规则必须由这两项决定，不得按 `SemanticType` 变体名逐条特判。
#[derive(Clone, Copy)]
pub struct OvmIntShape {
    /// 位宽。
    pub bits: u16,
    /// 是否有符号。
    pub signed: bool,
}

/// 语义类型 → 整数表示形状；非整数形状返回 `None`。
pub fn ovm_int_shape(ty: &SemanticType) -> Option<OvmIntShape> {
    match ty {
        SemanticType::Integer8 { signed } => Some(OvmIntShape { bits: 8, signed: *signed }),
        SemanticType::Integer16 { signed } => Some(OvmIntShape { bits: 16, signed: *signed }),
        SemanticType::Integer32 { signed } => Some(OvmIntShape { bits: 32, signed: *signed }),
        SemanticType::Integer64 { signed } => Some(OvmIntShape { bits: 64, signed: *signed }),
        SemanticType::Character => Some(OvmIntShape { bits: 32, signed: false }),
        _ => None,
    }
}

#[cfg(test)]
mod operator_binding_tests {
    use super::{OvmOperatorBinding, OvmOperatorCode, freeze_ovm_operator};
    use compiler_target::{ExternalImportLink, Identifier};
    use compiler_types::{ItemInstanceId, SemanticType};
    use data_binary_odin::OdinHeadCode;

    fn link(mnemonic: &str) -> ExternalImportLink {
        ExternalImportLink::host(Some(Identifier::new("ovm-operator")), vec![mnemonic.to_string()])
    }

    /// 声明侧 `[ovm_operator("…")]` 的真实助记符必须冻结成 Odin 头码，
    /// 而不是 Unfrozen；没有配方的助记符仍失败闭合。
    #[test]
    fn scalar_operator_mnemonics_freeze_to_odin_head_codes() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let i32_ty = SemanticType::Integer32 { signed: true };

        let add = freeze_ovm_operator(&link("i32.add"), instance, "infix +", &[i32_ty.clone(), i32_ty.clone()], &i32_ty)
            .expect("冻结不得失败")
            .expect("必须是 OVM operator 绑定");
        let OvmOperatorBinding::Frozen(add) = add
        else {
            panic!("`i32.add` 必须冻结");
        };
        assert_eq!(add.arity, 2);
        assert_eq!(add.encoding, OvmOperatorCode::Single(OdinHeadCode::I32Add));

        let less =
            freeze_ovm_operator(&link("i32.lt"), instance, "infix <", &[i32_ty.clone(), i32_ty.clone()], &SemanticType::Boolean)
                .expect("冻结不得失败")
                .expect("必须是 OVM operator 绑定");
        let OvmOperatorBinding::Frozen(less) = less
        else {
            panic!("`i32.lt` 必须冻结");
        };
        assert_eq!(less.encoding, OvmOperatorCode::Single(OdinHeadCode::I32LtS));
        assert_eq!(less.result_type, crate::target_operator::TargetOperandType::I32);

        // `OdinHeadCode` 没有 i64/f64 算术与比较变体（只有 I32* / F32* 块与两个
        // 位重解释变体）：这些助记符必须保留 Unfrozen，禁止回落宿主导入或猜指令。
        let i64_ty = SemanticType::Integer64 { signed: true };
        for (mnemonic, operand, result) in [
            ("i64.add", i64_ty.clone(), i64_ty.clone()),
            ("i64.eq", i64_ty.clone(), SemanticType::Boolean),
            ("i64.lt", i64_ty.clone(), SemanticType::Boolean),
            ("f64.add", SemanticType::Float64, SemanticType::Float64),
            ("f64.lt", SemanticType::Float64, SemanticType::Boolean),
        ] {
            let unfrozen = freeze_ovm_operator(&link(mnemonic), instance, "scalar", &[operand.clone(), operand], &result)
                .unwrap_or_else(|error| panic!("`{mnemonic}` 冻结不得失败: {error}"))
                .unwrap_or_else(|| panic!("`{mnemonic}` 必须是 OVM operator 绑定"));
            assert!(matches!(unfrozen, OvmOperatorBinding::Unfrozen), "`{mnemonic}` 头码不存在，必须保持缺口");
        }
    }

    /// f32 无原生头码的比较必须走**显式组合编码**：`ne` 取反 `eq`、`gt` 交换操作数用
    /// `lt`；`le`/`ge` 在 NaN 下无法用有序 `lt` 保真组合（`!(a<b)` ≠ `a<=b`），
    /// 保持缺口待头码，不得用 OR 组合冒充（单尾码配方模型表达不了实参双消费）。
    #[test]
    fn f32_composite_comparisons_freeze_explicitly_and_le_ge_stay_unfrozen() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let f32_ty = SemanticType::Float32;

        let (ne, gt) = match [
            freeze_ovm_operator(&link("f32.ne"), instance, "infix !=", &[f32_ty.clone(), f32_ty.clone()], &SemanticType::Boolean)
                .expect("冻结不得失败")
                .expect("必须是 OVM operator 绑定"),
            freeze_ovm_operator(&link("f32.gt"), instance, "infix >", &[f32_ty.clone(), f32_ty.clone()], &SemanticType::Boolean)
                .expect("冻结不得失败")
                .expect("必须是 OVM operator 绑定"),
        ] {
            [
                OvmOperatorBinding::Frozen(ne),
                OvmOperatorBinding::Frozen(gt),
            ] => (ne, gt),
            other => panic!("f32.ne/f32.gt 必须冻结: {other:?}"),
        };
        assert_eq!(ne.encoding, OvmOperatorCode::InvertBoolean(OdinHeadCode::F32Eq));
        assert_eq!(ne.result_type, crate::target_operator::TargetOperandType::I32);
        assert_eq!(gt.encoding, OvmOperatorCode::SwapCompare(OdinHeadCode::F32Lt));
        assert_eq!(gt.result_type, crate::target_operator::TargetOperandType::I32);

        for mnemonic in ["f32.le", "f32.ge"] {
            let unfrozen = freeze_ovm_operator(&link(mnemonic), instance, "infix cmp", &[f32_ty.clone(), f32_ty.clone()], &SemanticType::Boolean)
                .unwrap_or_else(|error| panic!("`{mnemonic}` 冻结不得失败: {error}"))
                .unwrap_or_else(|| panic!("`{mnemonic}` 必须是 OVM operator 绑定"));
            assert!(matches!(unfrozen, OvmOperatorBinding::Unfrozen), "`{mnemonic}` 无法保真组合，必须保持缺口");
        }
    }
}
