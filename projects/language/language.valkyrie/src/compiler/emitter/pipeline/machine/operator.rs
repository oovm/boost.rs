//! x86-64 语言 × 目标适配器：链接分类、槽载体判定与 operator 冻结裁决。
//!
//! 架构合同：`SemanticType` 槽载体判定（引用/标量/数组元素）、整数形状、
//! 浮点转整数边界、退出码类型与 `[x86_64_operator]` 链接分类归语言侧适配器；
//! emitter 的 machine 后端只保留助记符到指令表的目标私有冻结表
//! （`freeze_x86_64_operator_binding`）、绑定枚举与物理归一化原语。

use std::collections::BTreeMap;

use compiler_emitter::emitter_backend_machine::{X64ArrayOperation, X64ArrayOperatorPlan, X64ConsoleOperation, X86OperatorBinding, freeze_x86_64_operator_binding};
use compiler_target::ExternalImportLink;
use compiler_types::{ItemInstanceId, SemanticType};

use crate::compiler::emitter::pipeline::operator;

/// 8 字节指针载体：引用对象、unite（sum）、引用可空、元组与匿名 union。
///
/// 与 [`is_local_carrier_type`] 合起来定义 native 槽载体全集。库导出清单的
/// C ABI 载体也从这一处派生，禁止清单另写一套「什么能过界」的判定。
///
/// 布局与 sum 事实由调用方（语言 × 目标适配器）以目标私有表传入：本函数只读
/// 物理裁决所需的冻结事实，不接触跨目标共享程序。
pub(crate) fn is_reference_carrier(
    ty: &SemanticType,
    aggregate_layouts: &compiler_types::AggregateLayoutPlan,
    sum_layouts: &BTreeMap<SemanticType, compiler_types::SumTypeLayout>,
) -> bool {
    match ty {
        SemanticType::Nominal { .. } | SemanticType::Apply(..) => {
            if let Some(layout) = aggregate_layouts.frozen_layout(ty) {
                return layout.storage.is_reference();
            }
            // 闭合 unite 实例：物理载体是 sum 对象指针（`{u32 tag, i64 payload}`）。
            sum_layouts.contains_key(ty) || sum_layouts.contains_key(&ty.contract_key())
        }
        SemanticType::Union(_) => true,
        SemanticType::Nullable(inner) => {
            is_local_carrier_type(inner)
                || is_reference_carrier(inner, aggregate_layouts, sum_layouts)
                || matches!(**inner, SemanticType::Array(_) | SemanticType::Utf8 | SemanticType::Utf16)
        }
        // 元组：本 lane 以堆对象指针承载（元素槽合同见 `tuple_field_offset`，
        // 构造/读取在 `TupleNew`/`TupleGet` 发射点逐项校验）。
        SemanticType::Tuple(_) => true,
        _ => false,
    }
}

/// 可作为 native 8 字节标量槽的类型。
///
/// 浮点与整数共用 8 字节槽，但**载体语义不同**：整数读写走 `rax`，浮点读写走
/// `xmm` 的 `movsd/movss`；槽里保存的是位模式，`f32` 有效位在低 32 位。
fn is_slot_type(ty: &SemanticType) -> bool {
    matches!(
        ty,
        SemanticType::Boolean
            | SemanticType::Character
            | SemanticType::Integer8 { .. }
            | SemanticType::Integer16 { .. }
            | SemanticType::Integer32 { .. }
            | SemanticType::Integer64 { .. }
            | SemanticType::Float32
            | SemanticType::Float64
    )
}

/// 数组元素的结构候选：标量，或本 lane 以 8 字节指针承载的引用/聚合/sum/数组/元组/文本。
///
/// 本函数在 import 冻结阶段使用，早于计划建立，因此只能按形状判断；它必须覆盖
/// [`is_local_carrier_type`] 的全部元素形状（以及需要计划才能确认存储类的引用形状），
/// 否则同一元素类型会在构造入口被接受、在 operator 冻结被拒绝。值存储聚合等精确
/// 裁决留给语言侧机器码 lane 的 lowering 载体判断，避免把内联值当成指针搬运。
fn is_array_element_carrier_candidate(ty: &SemanticType) -> bool {
    is_slot_type(ty)
        || matches!(
            ty,
            SemanticType::Nominal { .. }
                | SemanticType::Apply(..)
                | SemanticType::Union(_)
                | SemanticType::Nullable(_)
                | SemanticType::Array(_)
                | SemanticType::Tuple(_)
                | SemanticType::Utf8
                | SemanticType::Utf16
        )
}

/// 帧槽可承载的类型：标量值、堆数组指针（`[T]` 的物理载体是 8 字节指针）、
/// 元组堆对象指针，以及文本对象指针。
///
/// `utf8` 语言值统一物化为 `Utf8Text` 对象 `{ [0] = _repr 数组 }`（见
/// `emit_text_constant`），std 的内容路径按对象读取。`utf16` 仍复用静态自相对
/// 描述符 `{ i32 self_relative_ptr; u32 len }`：本 lane 尚未闭合任何真实的
/// UTF-16 内容消费路径，语言值图里不会出现读取其内容的边。
pub(crate) fn is_local_carrier_type(ty: &SemanticType) -> bool {
    is_slot_type(ty) || heap_array_element_type(ty).is_some() || matches!(ty, SemanticType::Utf8 | SemanticType::Utf16)
}

/// 堆数组 `[T]` 的元素类型；定长值聚合 `[T; N]` 与标量返回 `None`。
///
/// 定长数组在语言里是栈上值聚合，不走本 lane 的堆数组表示，必须在调用点
/// 结构化拒绝而不是把它当指针搬运。
pub(crate) fn heap_array_element_type(ty: &SemanticType) -> Option<&SemanticType> {
    match ty {
        SemanticType::Array(element) => Some(element),
        _ => None,
    }
}

/// `Cast` 的整数表示：位宽 + 符号性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct X64IntShape {
    /// 位宽（8/16/32/64）。
    pub bits: u8,
    /// 符号性。
    pub signed: bool,
}

/// 整数族（含 `Character` 的 32 位无符号码位）的表示；浮点/布尔/文本返回 `None`。
pub(crate) fn x64_int_shape(ty: &SemanticType) -> Option<X64IntShape> {
    Some(match ty {
        SemanticType::Integer8 { signed } => X64IntShape { bits: 8, signed: *signed },
        SemanticType::Integer16 { signed } => X64IntShape { bits: 16, signed: *signed },
        SemanticType::Integer32 { signed } => X64IntShape { bits: 32, signed: *signed },
        SemanticType::Integer64 { signed } => X64IntShape { bits: 64, signed: *signed },
        SemanticType::Character => X64IntShape { bits: 32, signed: false },
        _ => return None,
    })
}

/// 浮点转整数的排他区间 `[lower, upper)`：两端都是 2 的幂，`f32`/`f64` 下精确可表示。
///
/// 有符号目标为 `[-2^(bits-1), 2^(bits-1))`，无符号目标为 `[0, 2^bits)`。
/// `u64` 目标在上游已结构化拒绝，这里只会收到可转换的形状。
pub(crate) fn float_int_target_bounds(shape: &X64IntShape) -> (f64, f64) {
    if shape.signed {
        let magnitude = (1u64 << (shape.bits - 1)) as f64;
        (-magnitude, magnitude)
    } else {
        (0.0, (1u128 << shape.bits) as f64)
    }
}

/// 可直接作为进程退出码的返回类型。
pub(crate) fn is_exit_code_type(ty: &SemanticType) -> bool {
    matches!(ty, SemanticType::Unit | SemanticType::Bottom) || is_slot_type(ty)
}

/// 源码 `[x86_64_operator("i64.add")]` 声明的 x86-64 目标 operator 助记符。
///
/// 注解按 **ISA** 声明，而不是按「native」：`native` 只是产物形态，`native` /
/// `node` / `python` 最终都会展开成规范目标四元组，而算子编码由四元组的 `arch`
/// 决定。只有显式声明 ISA 的绑定才能在此冻结编码；未声明 ISA 的绑定保持
/// `Unfrozen`，由 emit 结构化失败，禁止按宿主架构猜编码。
///
/// 只接受 platform tag `x86_64-operator` 且 locator 恰好一段；其余形态返回 `None`。
pub(crate) fn x86_64_operator_target(link: &ExternalImportLink) -> Option<&str> {
    if !link.matches_boundary("host") || !link.matches_platform_tag("x86_64-operator") {
        return None;
    }

    match link.locator_segments() {
        [mnemonic] => Some(mnemonic.as_str()),
        _ => None,
    }
}

/// 按声明的导入签名冻结 x86-64 operator 绑定。
pub(crate) fn freeze_x86_64_operator(
    link: &ExternalImportLink,
    instance: ItemInstanceId,
    symbol: &str,
    parameter_types: &[SemanticType],
    return_type: &SemanticType,
) -> Result<Option<X86OperatorBinding>, String> {
    let Some(mnemonic) = x86_64_operator_target(link)
    else {
        return Ok(None);
    };
    if let Some(plan) = freeze_x86_64_array_operator(mnemonic, instance, symbol, parameter_types, return_type)? {
        return Ok(Some(X86OperatorBinding::Array(plan)));
    }
    if let Some(operation) = freeze_x86_64_console_operator(mnemonic, instance, symbol, parameter_types, return_type)? {
        return Ok(Some(X86OperatorBinding::Console(operation)));
    }
    let Some(plan) = freeze_x86_64_operator_binding(mnemonic)
    else {
        return Ok(Some(X86OperatorBinding::Unfrozen));
    };
    operator::validate_frozen_operator(mnemonic, instance, symbol, parameter_types, return_type, &plan)?;
    Ok(Some(X86OperatorBinding::Frozen(plan)))
}

/// 按声明签名冻结 x86-64 数组 operator。
///
/// 数组 operator 的物理形状不是标量载体：`[T]` 与 `usize` 都占 8 字节槽，但
/// 载体种类不同，不能用 `validate_frozen_operator` 的标量规则判定。这里只校验
/// arity、数组位置的类型身份与元素可为 8 字节槽，未知助记符返回 `None` 交回
/// 标量表处理。
fn freeze_x86_64_array_operator(
    mnemonic: &str,
    instance: ItemInstanceId,
    symbol: &str,
    parameter_types: &[SemanticType],
    return_type: &SemanticType,
) -> Result<Option<X64ArrayOperatorPlan>, String> {
    let operation = match mnemonic {
        "array.new" => X64ArrayOperation::New,
        "array.length" => X64ArrayOperation::Length,
        "array.get" => X64ArrayOperation::Get,
        "array.set" => X64ArrayOperation::Set,
        _ => return Ok(None),
    };
    let arity = match operation {
        X64ArrayOperation::New | X64ArrayOperation::Length => 1,
        X64ArrayOperation::Get => 2,
        X64ArrayOperation::Set => 3,
    };
    if parameter_types.len() != arity {
        return Err(format!(
            "x86_64 数组 operator `{mnemonic}` 的声明 arity 与冻结计划不符: function={symbol} expected={arity} actual={} instance={instance:?}",
            parameter_types.len()
        ));
    }
    let require_slot = |index: usize, ty: &SemanticType| -> Result<(), String> {
        if !is_slot_type(ty) {
            return Err(format!(
                "x86_64 数组 operator `{mnemonic}` 的参数 {index} 类型 `{ty}` 没有 8 字节槽载体: function={symbol} instance={instance:?}"
            ));
        }
        Ok(())
    };
    let require_array = |index: usize| -> Result<&SemanticType, String> {
        let ty = &parameter_types[index];
        let Some(element) = heap_array_element_type(ty)
        else {
            return Err(format!(
                "x86_64 数组 operator `{mnemonic}` 的参数 {index} 类型 `{ty}` 不是堆数组 `[T]`: function={symbol} instance={instance:?}"
            ));
        };
        if !is_array_element_carrier_candidate(element) {
            return Err(format!(
                "x86_64 数组 operator `{mnemonic}` 的元素类型 `{element}` 没有 8 字节槽载体: function={symbol} instance={instance:?}"
            ));
        }
        Ok(element)
    };
    match operation {
        X64ArrayOperation::New => {
            require_slot(0, &parameter_types[0])?;
            require_array_return(mnemonic, symbol, instance, return_type)?;
        }
        X64ArrayOperation::Length => {
            require_array(0)?;
            if !is_slot_type(return_type) {
                return Err(format!(
                    "x86_64 数组 operator `{mnemonic}` 的返回类型 `{return_type}` 没有 8 字节槽载体: function={symbol} instance={instance:?}"
                ));
            }
        }
        X64ArrayOperation::Get => {
            let element = require_array(0)?;
            require_slot(1, &parameter_types[1])?;
            if return_type != element {
                return Err(format!(
                    "x86_64 数组 operator `{mnemonic}` 的返回类型 `{return_type}` 与元素类型 `{element}` 不一致: function={symbol} instance={instance:?}"
                ));
            }
        }
        X64ArrayOperation::Set => {
            let element = require_array(0)?;
            require_slot(1, &parameter_types[1])?;
            if &parameter_types[2] != element {
                return Err(format!(
                    "x86_64 数组 operator `{mnemonic}` 的写入值类型 `{}` 与元素类型 `{element}` 不一致: function={symbol} instance={instance:?}",
                    parameter_types[2]
                ));
            }
            if !return_type.is_unitish() {
                return Err(format!(
                    "x86_64 数组 operator `{mnemonic}` 的返回类型 `{return_type}` 必须是 unit: function={symbol} instance={instance:?}"
                ));
            }
        }
    }
    Ok(Some(X64ArrayOperatorPlan { arity, operation }))
}

/// 按声明签名冻结 x86-64 控制台 operator。
///
/// 控制台 operator 的载体既不是纯标量也不是数组头：`handle` 是平台解释的 64 位
/// 整数（Win64 伪句柄 / Linux fd），`message` 是 `Utf8Text` 对象指针。形状在此
/// 一次冻结，emit 不再按名字猜。未知助记符返回 `None` 交回标量表。
fn freeze_x86_64_console_operator(
    mnemonic: &str,
    instance: ItemInstanceId,
    symbol: &str,
    parameter_types: &[SemanticType],
    return_type: &SemanticType,
) -> Result<Option<X64ConsoleOperation>, String> {
    let operation = match mnemonic {
        "console.write" => X64ConsoleOperation::Write,
        _ => return Ok(None),
    };
    let failure = |reason: String| Err(format!("x86_64 控制台 operator `{mnemonic}` {reason}: function={symbol} instance={instance:?}"));
    match operation {
        X64ConsoleOperation::Write => {
            if parameter_types.len() != 2 {
                return failure(format!("声明 arity 与冻结计划不符: expected=2 actual={}", parameter_types.len()));
            }
            if !matches!(parameter_types[0], SemanticType::Integer64 { signed: true }) {
                return failure(format!("句柄参数类型 `{}` 不是有符号 64 位整数", parameter_types[0]));
            }
            if !matches!(parameter_types[1], SemanticType::Utf8) {
                return failure(format!("消息参数类型 `{}` 不是 utf8 文本", parameter_types[1]));
            }
            if !return_type.is_unitish() {
                return failure(format!("返回类型 `{return_type}` 必须是 unit"));
            }
        }
    }
    Ok(Some(operation))
}

/// `array.new` 的返回必须是元素可承载的堆数组。
fn require_array_return(mnemonic: &str, symbol: &str, instance: ItemInstanceId, return_type: &SemanticType) -> Result<(), String> {
    let Some(element) = heap_array_element_type(return_type)
    else {
        return Err(format!(
            "x86_64 数组 operator `{mnemonic}` 的返回类型 `{return_type}` 不是堆数组 `[T]`: function={symbol} instance={instance:?}"
        ));
    };
    if !is_array_element_carrier_candidate(element) {
        return Err(format!(
            "x86_64 数组 operator `{mnemonic}` 的元素类型 `{element}` 没有 8 字节槽载体: function={symbol} instance={instance:?}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod array_element_carrier_tests {
    use super::*;

    /// import 冻结早于计划建立：候选筛查必须覆盖所有无计划即可判定为元素载体的形状，
    /// 否则同一元素类型会在构造入口（`is_local_carrier_type`）被接受、在 operator
    /// 冻结被拒绝——这正是两条入口合同漂移的形态。
    #[test]
    fn operator_candidate_screen_covers_every_plan_free_element_carrier() {
        let shapes = vec![
            SemanticType::Boolean,
            SemanticType::Character,
            SemanticType::Integer8 { signed: true },
            SemanticType::Integer16 { signed: false },
            SemanticType::Integer32 { signed: true },
            SemanticType::Integer64 { signed: false },
            SemanticType::Float32,
            SemanticType::Float64,
            SemanticType::Utf8,
            SemanticType::Utf16,
            SemanticType::Array(Box::new(SemanticType::Integer8 { signed: false })),
            SemanticType::Tuple(vec![SemanticType::Integer64 { signed: true }]),
        ];
        for ty in shapes {
            if is_local_carrier_type(&ty) {
                assert!(
                    is_array_element_carrier_candidate(&ty),
                    "`{ty}` 在构造入口可作数组元素，却被 operator 候选筛查拒绝"
                );
            }
        }
    }
}

#[cfg(test)]
mod wrapper_tests {
    use super::{freeze_x86_64_operator, x86_64_operator_target};
    use compiler_emitter::emitter_backend_machine::{X64ArrayOperation, X64ConsoleOperation, X86OperatorBinding};
    use compiler_target::{ExternalImportLink, Identifier};
    use compiler_types::{FunctionType, ItemInstanceId, SemanticType};

    fn link(mnemonic: &str) -> ExternalImportLink {
        ExternalImportLink::host(Some(Identifier::new("x86_64-operator")), vec![mnemonic.to_string()])
    }

    #[test]
    fn link_mismatch_returns_none() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let foreign = ExternalImportLink::host(Some(Identifier::new("jvm-operator")), vec!["integer.add".into()]);
        assert_eq!(x86_64_operator_target(&foreign), None);
        assert!(freeze_x86_64_operator(&foreign, instance, "infix +", &[], &SemanticType::Unit).unwrap().is_none());
    }

    #[test]
    fn scalar_and_array_and_console_freeze() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let i64_ty = SemanticType::Integer64 { signed: true };
        let binding = freeze_x86_64_operator(&link("long.add"), instance, "infix +", &[i64_ty.clone(), i64_ty.clone()], &i64_ty)
            .expect("冻结不得失败")
            .expect("必须是 x86-64 operator 绑定");
        assert!(matches!(binding, X86OperatorBinding::Frozen(_)), "`long.add` 必须冻结");

        let array = SemanticType::Array(Box::new(i64_ty.clone()));
        let binding = freeze_x86_64_operator(&link("array.get"), instance, "get", &[array, i64_ty.clone()], &i64_ty)
            .expect("数组冻结不得失败")
            .expect("必须是 x86-64 operator 绑定");
        let X86OperatorBinding::Array(plan) = binding
        else {
            panic!("`array.get` 必须冻结为数组计划");
        };
        assert_eq!(plan.operation, X64ArrayOperation::Get);
        assert_eq!(plan.arity, 2);

        let binding = freeze_x86_64_operator(&link("console.write"), instance, "write", &[i64_ty, SemanticType::Utf8], &SemanticType::Unit)
            .expect("console 冻结不得失败")
            .expect("必须是 x86-64 operator 绑定");
        let X86OperatorBinding::Console(operation) = binding
        else {
            panic!("`console.write` 必须冻结为 console 计划");
        };
        assert_eq!(operation, X64ConsoleOperation::Write);
    }

    #[test]
    fn unknown_mnemonic_stays_unfrozen() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let f64_ty = SemanticType::Float64;
        let binding = freeze_x86_64_operator(&link("double.rem.unknown"), instance, "infix %", &[f64_ty.clone(), f64_ty.clone()], &f64_ty)
            .expect("冻结不得失败")
            .expect("必须是 x86-64 operator 绑定");
        assert!(matches!(binding, X86OperatorBinding::Unfrozen), "近似但未登记的助记符不得被猜成编码");
    }

    #[test]
    fn arity_or_carrier_mismatch_fails() {
        let instance = ItemInstanceId::from_index(0).expect("实例下标 0 必须可构造");
        let i64_ty = SemanticType::Integer64 { signed: true };
        let error = freeze_x86_64_operator(&link("integer.add"), instance, "infix +", &[i64_ty.clone()], &i64_ty).expect_err("arity 不符必须结构化失败");
        assert!(error.contains("arity"), "{error}");
        let f64_ty = SemanticType::Float64;
        let error = freeze_x86_64_operator(&link("integer.add"), instance, "infix +", &[f64_ty.clone(), f64_ty.clone()], &f64_ty).expect_err("载体不符必须结构化失败");
        assert!(error.contains("载体"), "{error}");
        // 数组计划同样失败闭合：arity 不符与元素形状不符都是结构化错误。
        let array = SemanticType::Array(Box::new(i64_ty.clone()));
        let error = freeze_x86_64_operator(&link("array.get"), instance, "get", &[array], &i64_ty).expect_err("数组 arity 不符必须结构化失败");
        assert!(error.contains("arity"), "{error}");
        let function = SemanticType::Function(Box::new(FunctionType { params: vec![i64_ty.clone()], return_type: SemanticType::Unit }));
        let error = freeze_x86_64_operator(
            &link("array.get"),
            instance,
            "get",
            &[SemanticType::Array(Box::new(function)), i64_ty.clone()],
            &i64_ty,
        )
        .expect_err("函数指针元素形状必须结构化失败");
        assert!(error.contains("8 字节槽载体"), "{error}");
    }
}
