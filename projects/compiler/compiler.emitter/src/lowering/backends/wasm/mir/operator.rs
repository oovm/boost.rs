//! Wasm 平台私有的 `[wasm_operator("...")]` 冻结入口。
//!
//! 助记符表与数组操作身份归 Wasm backend 所有：跨目标公共计划已删除，目标私有
//! 输入只保存逻辑导入链接，不再携带任何平台的冻结结果。计划构造阶段由本模块一次性冻结，
//! 发射期只消费 [`WasmOperatorBinding`]，不得再从助记符字符串推断 arity 或配方。

use data_binary_wasm::WasmOpcode;

use super::{WasmCallableKey, WasmTypeFact, operand_carrier, validate_frozen_operator};
use crate::target_operator::{TargetOperandType, TargetOperatorPlan};

use super::{
    array::{WasmGcArrayOperation, WasmGcArrayOperatorPlan, freeze_wasm_array_binding},
    numeric::freeze_wasm_float_remainder,
    numeric::freeze_wasm_numeric_binding,
};

/// 已冻结的 Wasm operator 绑定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WasmOperatorBinding {
    /// 单指令数值/比较 operator。
    Numeric(TargetOperatorPlan<WasmOpcode>),
    /// 多步 GC 数组 operator（`ref.cast` + `array.*`）。
    GcArray(WasmGcArrayOperatorPlan),
    /// 浮点余数：wasm 没有原生指令，冻结为精确截断余数的正式 lowering 载体，
    /// emit 按固定序列机械展开（见 `freeze_wasm_float_remainder` 的合同说明）。
    FloatRemainder(TargetOperandType),
    /// 声明了目标 operator 绑定，但没有可编码配方；emit 必须结构化失败。
    Unfrozen,
}

/// 按声明的导入签名冻结 Wasm operator 绑定。
///
/// `None` 表示该导入不是 `[wasm_operator]` 绑定；`Some(Unfrozen)` 表示绑定了
/// Wasm operator 但助记符不在 Wasm 私有表内。两种状态都必须由调用方保留，不允许
/// 回落为宿主导入或默认成功。
pub(crate) fn freeze_wasm_operator_binding(
    instance: WasmCallableKey,
    symbol: &str,
    mnemonic: &str,
    parameter_types: &[WasmTypeFact],
    return_type: &WasmTypeFact,
) -> crate::Result<Option<WasmOperatorBinding>> {
    if let Some(plan) = freeze_wasm_numeric_binding(mnemonic) {
        validate_frozen_operator(mnemonic, instance, symbol, parameter_types, return_type, &plan)?;
        return Ok(Some(WasmOperatorBinding::Numeric(plan)));
    }
    if let Some(plan) = freeze_wasm_array_binding(mnemonic) {
        validate_frozen_array_operator(instance, symbol, mnemonic, parameter_types, return_type, &plan)?;
        return Ok(Some(WasmOperatorBinding::GcArray(plan)));
    }
    if let Some(carrier) = freeze_wasm_float_remainder(mnemonic) {
        validate_frozen_float_remainder(instance, symbol, mnemonic, parameter_types, return_type, carrier)?;
        return Ok(Some(WasmOperatorBinding::FloatRemainder(carrier)));
    }
    Ok(Some(WasmOperatorBinding::Unfrozen))
}

/// 校验浮点余数 lowering 的声明签名：二元、实参与结果都是同一浮点载体。
///
/// 余数是同型二元运算：`f32.rem` 两侧必须是 `f32` 载体并返回 `f32`，`f64.rem`
/// 同理；任何混宽（`f32` 实参配 `f64` 结果）都不是该助记符的合同，失败闭合。
fn validate_frozen_float_remainder(
    instance: WasmCallableKey,
    function: &str,
    mnemonic: &str,
    parameter_types: &[WasmTypeFact],
    return_type: &WasmTypeFact,
    carrier: TargetOperandType,
) -> crate::Result<()> {
    let fail = |reason: String| -> crate::Result<()> {
        Err(format!("目标 operator 绑定 `{mnemonic}` 的声明签名与冻结 lowering 不符: function={function} {reason} instance={instance:?}"))
    };
    if parameter_types.len() != 2 {
        return fail(format!("expected arity=2 actual={}", parameter_types.len()));
    }
    for (index, ty) in parameter_types.iter().enumerate() {
        if operand_carrier(ty) != Some(carrier) {
            return fail(format!("参数 {index} 载体必须与冻结余数载体一致: expected={carrier:?} actual={ty:?}"));
        }
    }
    if operand_carrier(return_type) != Some(carrier) {
        return fail(format!("返回载体必须与冻结余数载体一致: expected={carrier:?} actual={return_type:?}"));
    }
    Ok(())
}

/// 校验冻结的 wasm GC 数组计划与导入声明的实例化签名一致。
///
/// 数组 operator 的物理编码是多步的（`ref.cast` + `array.*`），arraytype 索引属于
/// 模块装配期 RepresentationPlan 注册表事实，不在计划层冻结；此处校验的是声明签名
/// 与冻结 arity/操作身份的语义形状：聚合参数必须是堆数组 `[T]`，长度/索引参数必须
/// 携带 i32 载体，`get`/`set` 的元素槽必须与数组元素类型逐点一致。
fn validate_frozen_array_operator(
    instance: WasmCallableKey,
    function: &str,
    mnemonic: &str,
    parameter_types: &[WasmTypeFact],
    return_type: &WasmTypeFact,
    plan: &WasmGcArrayOperatorPlan,
) -> crate::Result<()> {
    use WasmGcArrayOperation::{Get, Length, New, Set};
    let fail = |reason: String| -> crate::Result<()> {
        Err(format!("目标数组 operator 绑定 `{mnemonic}` 的声明签名与冻结计划不符: function={function} {reason} instance={instance:?}"))
    };
    if parameter_types.len() != plan.arity {
        return fail(format!("expected arity={} actual={}", plan.arity, parameter_types.len()));
    }
    fn array_element(ty: &WasmTypeFact) -> Option<&WasmTypeFact> {
        match ty {
            WasmTypeFact::Array(element) => Some(element.as_ref()),
            _ => None,
        }
    }
    let index_carrier = |ty: &WasmTypeFact| operand_carrier(ty) == Some(crate::target_operator::TargetOperandType::I32);
    match plan.operation {
        New => {
            let Some(length) = parameter_types.first()
            else {
                return fail("arity 事实缺失".to_owned());
            };
            if !index_carrier(length) {
                return fail(format!("`array.new` 的长度参数必须携带 i32 载体: actual={length:?}"));
            }
            if array_element(return_type).is_none() {
                return fail(format!("`array.new` 的返回类型必须是堆数组 `[T]`: actual={return_type:?}"));
            }
        }
        Length => {
            let Some(array) = parameter_types.first()
            else {
                return fail("arity 事实缺失".to_owned());
            };
            if array_element(array).is_none() {
                return fail(format!("`array.length` 的聚合参数必须是堆数组 `[T]`: actual={array:?}"));
            }
            if !index_carrier(return_type) {
                return fail(format!("`array.length` 的返回类型必须携带 i32 载体: actual={return_type:?}"));
            }
        }
        Get => {
            let (Some(array), Some(index)) = (parameter_types.first(), parameter_types.get(1))
            else {
                return fail("arity 事实缺失".to_owned());
            };
            let Some(element) = array_element(array)
            else {
                return fail(format!("`array.get` 的聚合参数必须是堆数组 `[T]`: actual={array:?}"));
            };
            if !index_carrier(index) {
                return fail(format!("`array.get` 的索引参数必须携带 i32 载体: actual={index:?}"));
            }
            if return_type != element {
                return fail(format!("`array.get` 的返回类型必须与数组元素类型一致: element={element:?} actual={return_type:?}"));
            }
        }
        Set => {
            let (Some(array), Some(index), Some(value)) = (parameter_types.first(), parameter_types.get(1), parameter_types.get(2))
            else {
                return fail("arity 事实缺失".to_owned());
            };
            let Some(element) = array_element(array)
            else {
                return fail(format!("`array.set` 的聚合参数必须是堆数组 `[T]`: actual={array:?}"));
            };
            if !index_carrier(index) {
                return fail(format!("`array.set` 的索引参数必须携带 i32 载体: actual={index:?}"));
            }
            if value != element {
                return fail(format!("`array.set` 的值参数必须与数组元素类型一致: element={element:?} actual={value:?}"));
            }
            if *return_type != WasmTypeFact::Unit {
                return fail(format!("`array.set` 的返回类型必须是 unit: actual={return_type:?}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod operator_binding_tests {
    use super::{TargetOperandType, WasmCallableKey, WasmOpcode, WasmOperatorBinding, WasmTypeFact, freeze_wasm_operator_binding};

    /// 声明侧 `[wasm_operator("…")]` 的真实助记符必须冻结成 Wasm 原生 opcode；
    /// 没有原生指令但合同可兑现的 `f64.rem` 冻结为浮点余数 lowering，
    /// 表外助记符仍保持 Unfrozen。
    #[test]
    fn scalar_operator_mnemonics_freeze_to_wasm_opcodes() {
        let instance = WasmCallableKey(0);
        let i32_ty = WasmTypeFact::Integer32 { signed: true };

        let add = freeze_wasm_operator_binding(instance, "infix +", "i32.add", &[i32_ty.clone(), i32_ty.clone()], &i32_ty)
            .expect("冻结不得失败")
            .expect("必须是 Wasm operator 绑定");
        let WasmOperatorBinding::Numeric(add) = add
        else {
            panic!("`i32.add` 必须冻结为数值 operator");
        };
        assert_eq!(add.arity, 2);
        assert_eq!(add.encoding, WasmOpcode::I32Add);

        let less = freeze_wasm_operator_binding(instance, "infix <", "i32.lt_s", &[i32_ty.clone(), i32_ty.clone()], &WasmTypeFact::Boolean)
            .expect("冻结不得失败")
            .expect("必须是 Wasm operator 绑定");
        let WasmOperatorBinding::Numeric(less) = less
        else {
            panic!("`i32.lt_s` 必须冻结为数值 operator");
        };
        assert_eq!(less.encoding, WasmOpcode::I32LtS);

        let f64_rem =
            freeze_wasm_operator_binding(instance, "infix %", "f64.rem", &[WasmTypeFact::Float64, WasmTypeFact::Float64], &WasmTypeFact::Float64)
                .expect("冻结不得失败")
                .expect("必须是 Wasm operator 绑定");
        assert_eq!(f64_rem, WasmOperatorBinding::FloatRemainder(TargetOperandType::F64), "`f64.rem` 必须冻结为浮点余数 lowering");

        let f32_rem =
            freeze_wasm_operator_binding(instance, "infix %", "f32.rem", &[WasmTypeFact::Float32, WasmTypeFact::Float32], &WasmTypeFact::Float32)
                .expect("冻结不得失败")
                .expect("必须是 Wasm operator 绑定");
        assert_eq!(f32_rem, WasmOperatorBinding::FloatRemainder(TargetOperandType::F32), "`f32.rem` 必须冻结为浮点余数 lowering");

        // 混宽签名不是余数合同：`f32.rem` 配 f64 返回必须失败闭合，不得静默放宽。
        let mismatch = freeze_wasm_operator_binding(instance, "infix %", "f32.rem", &[WasmTypeFact::Float32, WasmTypeFact::Float32], &WasmTypeFact::Float64)
            .expect_err("混宽余数签名必须结构化失败");
        assert!(mismatch.contains("返回载体"), "错误必须点名载体不符: {mismatch}");

        let unfrozen = freeze_wasm_operator_binding(instance, "infix <", "f64.unknown", &[WasmTypeFact::Float64, WasmTypeFact::Float64], &WasmTypeFact::Boolean)
            .expect("冻结不得失败")
            .expect("必须是 Wasm operator 绑定");
        assert!(matches!(unfrozen, WasmOperatorBinding::Unfrozen));
    }
}
