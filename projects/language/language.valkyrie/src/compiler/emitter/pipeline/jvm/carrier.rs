//! JVM lane 的类型载体裁决：`SemanticType` → 已选定 JVM descriptor。
//!
//! 架构门 D4：源语言类型语义（`Character` 投 32 位 `int`、`Utf8`/`Utf16` 投
//! `java/lang/String`、闭合 `Apply` 按冻结 `SemanticType → LayoutId` 索引取
//! companion 布局类）只属于语言 × 目标适配器；emitter 后端不再接收
//! `SemanticType`，只消费本模块结算提交的 descriptor、类引用与槽位事实。
//! 布局身份按实例索引（`LayoutId`）查冻结计划，绝不按名字回退猜布局。

use compiler_emitter::emitter_backend_jvm::{JvmTypeDescriptor, jvm_layout_class_name};
use compiler_types::{AggregateLayoutPlan, SemanticType};

/// 局部 / 形参位置的 JVM 载体：`Unit`/`Bottom` 没有值槽。
pub(crate) fn jvm_parameter_descriptor(layouts: &AggregateLayoutPlan, ty: &SemanticType) -> Result<JvmTypeDescriptor, String> {
    let descriptor = jvm_type_descriptor(layouts, ty)?;
    if descriptor == JvmTypeDescriptor::Void {
        return Err(format!("JVM 最小 lowering：`{ty}` 是 void 形态，不能作为方法形参或局部值"));
    }
    Ok(descriptor)
}

/// 返回位置的 JVM 载体。
pub(crate) fn jvm_return_descriptor(layouts: &AggregateLayoutPlan, ty: &SemanticType) -> Result<JvmTypeDescriptor, String> {
    jvm_type_descriptor(layouts, ty)
}

/// 数组元素位置的 JVM 载体。
///
/// data-binary-jvm 没有 `laload`/`lastore`/`faload`/`daload`，因此 i64/f32/f64 元素
/// 必须用装箱引用数组（`java/lang/Long` / `Double` / `Float`），这是块 3 存取
/// 指令的前提；标量元素选 `NewArray` atype 可直接承载的基本类型。
pub(crate) fn jvm_array_element_descriptor(layouts: &AggregateLayoutPlan, element: &SemanticType) -> Result<JvmTypeDescriptor, String> {
    Ok(match element {
        SemanticType::Boolean => JvmTypeDescriptor::Boolean,
        SemanticType::Integer8 { .. } => JvmTypeDescriptor::Byte,
        SemanticType::Integer16 { .. } => JvmTypeDescriptor::Short,
        SemanticType::Integer32 { .. } => JvmTypeDescriptor::Int,
        // V 的 `char` 是 u32 码位载体；JVM `char` 只有 16 位，会把 U+10000 以上截断。
        // 物理载体固定为 32 位 int，UTF-16 code unit 转换只在显式编码边界发生。
        SemanticType::Character => JvmTypeDescriptor::Int,
        SemanticType::Integer64 { .. } => JvmTypeDescriptor::Long,
        SemanticType::Float32 => JvmTypeDescriptor::Float,
        SemanticType::Float64 => JvmTypeDescriptor::Double,
        SemanticType::Utf8 | SemanticType::Utf16 => JvmTypeDescriptor::Object("java/lang/String".to_string()),
        // 名义布局按冻结身份取 companion 类名；值存储聚合没有单槽 JVM 载体。
        SemanticType::Nominal { layout, spelling } => {
            let Some(layout) = layouts.layouts.iter().find(|candidate| candidate.id == *layout)
            else {
                return Err(format!("JVM 最小 lowering：`{spelling}` 的布局身份 `{layout}` 未在计划中注册：缺少数组元素 JVM 载体合同"));
            };
            if layout.storage.is_reference() {
                JvmTypeDescriptor::Object(jvm_layout_class_name(layout))
            }
            else {
                return Err(format!("JVM 最小 lowering 不支持值存储聚合 `{spelling}` 作为数组元素：缺少单槽 JVM 载体合同"));
            }
        }
        // 闭合泛型实例的布局身份只来自计划冻结的 `SemanticType → LayoutId` 索引；
        // 缺映射或映射悬空都回到计划生产者修，开放泛型仍在这里失败。
        SemanticType::Apply(..) => {
            let Some(layout) = layouts.frozen_layout(element) else {
                return Err(format!(
                    "JVM 最小 lowering：泛型实例 `{element}` 缺少冻结布局映射：计划未把该闭合实例登记到 `SemanticType → LayoutId`（开放泛型不得进入发射）"
                ));
            };
            if layout.storage.is_reference() {
                JvmTypeDescriptor::Object(jvm_layout_class_name(layout))
            }
            else {
                return Err(format!("JVM 最小 lowering 不支持值存储泛型实例 `{element}` 作为数组元素：缺少单槽 JVM 载体合同"));
            }
        }
        SemanticType::Array(inner) | SemanticType::FixedArray { element: inner, .. } => {
            JvmTypeDescriptor::array(jvm_array_element_descriptor(layouts, inner)?)
        }
        // 可空元素只能复用引用载体（`null` 表示空）；标量需要 sum 表示。
        SemanticType::Nullable(inner) => {
            let descriptor = jvm_array_element_descriptor(layouts, inner)?;
            if matches!(descriptor, JvmTypeDescriptor::Object(_) | JvmTypeDescriptor::Array(_)) {
                descriptor
            }
            else {
                return Err(format!("JVM 最小 lowering：可空标量 `{element}` 需要 sum 表示，不能擦成引用"));
            }
        }
        other => {
            return Err(format!("JVM 最小 lowering 不支持数组元素类型 `{other}`：缺少该形状的 JVM 载体合同"));
        }
    })
}

/// 类型的 JVM 载体：标量取 descriptor 原语，数组取元素载体数组，引用布局取 companion 类名。
pub(crate) fn jvm_type_descriptor(layouts: &AggregateLayoutPlan, ty: &SemanticType) -> Result<JvmTypeDescriptor, String> {
    Ok(match ty {
        SemanticType::Bottom | SemanticType::Unit => JvmTypeDescriptor::Void,
        SemanticType::Boolean => JvmTypeDescriptor::Boolean,
        // V 的 `char` 是 u32 码位，载体固定 32 位；不得用宿主 16 位 `Char`。
        SemanticType::Character => JvmTypeDescriptor::Int,
        SemanticType::Integer8 { .. } | SemanticType::Integer16 { .. } | SemanticType::Integer32 { .. } => JvmTypeDescriptor::Int,
        SemanticType::Integer64 { .. } => JvmTypeDescriptor::Long,
        SemanticType::Float32 => JvmTypeDescriptor::Float,
        SemanticType::Float64 => JvmTypeDescriptor::Double,
        SemanticType::Utf8 | SemanticType::Utf16 => JvmTypeDescriptor::Object("java/lang/String".to_string()),
        // 栈/堆数组的载体是元素载体数组；`[T; N]` 的长度由 `ArrayFromElements` 表达。
        SemanticType::Array(element) => JvmTypeDescriptor::array(jvm_array_element_descriptor(layouts, element)?),
        SemanticType::FixedArray { element, .. } => JvmTypeDescriptor::array(jvm_array_element_descriptor(layouts, element)?),
        // 布局身份只经 `TupleNew`/`AggregateCopy` 的 `layout_id` 进入后端，类型描述符
        // 没有该事实，禁止按形状猜布局。
        SemanticType::Tuple(..) => {
            return Err(format!(
                "JVM 最小 lowering：裸 tuple `{ty}` 没有布局身份：布局身份只经 `TupleNew`/`AggregateCopy` 的 `layout_id` 进入后端，类型描述符没有该事实"
            ));
        }
        // 可空引用直接复用引用载体（`null` 表示空）；标量需要 sum 表示，不得静默装箱。
        SemanticType::Nullable(inner) => {
            let descriptor = jvm_type_descriptor(layouts, inner)?;
            if matches!(descriptor, JvmTypeDescriptor::Object(_) | JvmTypeDescriptor::Array(_)) {
                descriptor
            }
            else if descriptor == JvmTypeDescriptor::Void {
                return Err(format!("JVM 最小 lowering：可空 `{ty}` 的内层是 void，没有可复用的引用载体"));
            }
            else {
                return Err(format!("JVM 最小 lowering：可空标量 `{ty}` 需要 sum 表示，不得静默装箱"));
            }
        }
        // 闭合泛型实例的布局身份只来自计划冻结的 `SemanticType → LayoutId` 索引。
        // 与 `Nominal` 分支同一存储规则；缺映射、悬空或身份冲突回计划生产者修。
        SemanticType::Apply(..) => {
            let Some(layout) = layouts.frozen_layout(ty) else {
                return Err(format!(
                    "JVM 最小 lowering：泛型实例 `{ty}` 缺少冻结布局映射：计划未把该闭合实例登记到 `SemanticType → LayoutId`（开放泛型不得进入发射）"
                ));
            };
            if layout.storage.is_reference() || !layout.fields.is_empty() {
                JvmTypeDescriptor::Object(jvm_layout_class_name(layout))
            }
            else {
                return Err(format!("JVM 最小 lowering 不支持零字段值存储泛型实例 `{ty}`：缺少单槽 JVM 载体合同"));
            }
        }
        // 携带身份的名义类型：存储类按 `layout` 身份解析，引用/值存储都指向
        // companion 布局类。JVM 没有内联结构体，逐字段拷贝由 `AggregateCopy` 承担，
        // 它是唯一允许的复制路径；零字段值布局与 wasm 标量语义一致，维持结构化失败。
        SemanticType::Nominal { layout, spelling } => {
            let Some(layout) = layouts.layouts.iter().find(|candidate| candidate.id == *layout)
            else {
                return Err(format!("JVM 最小 lowering：`{spelling}` 的布局身份 `{layout}` 未在计划中注册：缺少该形状的 JVM 载体合同"));
            };
            if layout.storage.is_reference() || !layout.fields.is_empty() {
                JvmTypeDescriptor::Object(jvm_layout_class_name(layout))
            }
            else {
                return Err(format!("JVM 最小 lowering 不支持零字段值存储聚合 `{spelling}`：缺少单槽 JVM 载体合同"));
            }
        }
        other => {
            return Err(format!("JVM 最小 lowering 不支持类型 `{other}`：缺少该形状的 JVM 载体合同"));
        }
    })
}

#[cfg(test)]
mod carrier_settlement_tests {
    use compiler_emitter::emitter_backend_jvm::jvm_layout_class_name;
    use compiler_target::Identifier;
    use compiler_types::{AggregateLayout, AggregateLayoutPlan, FieldLayout, StorageKind};

    use super::*;

    fn closed_apply() -> SemanticType {
        SemanticType::Apply(
            Box::new(SemanticType::Named(Identifier::new("std::collection::ArrayList"))),
            vec![SemanticType::Integer64 { signed: true }],
        )
    }

    fn plan_with_closed_apply(register: bool) -> AggregateLayoutPlan {
        let apply = closed_apply();
        let outer = AggregateLayout {
            id: 0,
            name: "Holder".to_string(),
            namespace: String::new(),
            key: None,
            storage: StorageKind::Reference,
            fields: vec![FieldLayout { name: "items".to_string(), ty: apply.clone() }],
        };
        let inner = AggregateLayout {
            id: 1,
            name: "ArrayList".to_string(),
            namespace: String::new(),
            key: None,
            storage: StorageKind::Reference,
            fields: vec![FieldLayout { name: "len".to_string(), ty: SemanticType::Integer64 { signed: true } }],
        };
        let mut plan = AggregateLayoutPlan { layouts: vec![outer, inner], ..Default::default() };
        if register {
            plan.layout_by_semantic_type.insert(apply, 1);
        }
        plan
    }

    #[test]
    fn closed_apply_resolves_to_frozen_companion_class() {
        let plan = plan_with_closed_apply(true);
        let descriptor = jvm_type_descriptor(&plan, &closed_apply()).expect("闭合 Apply 必须由冻结索引结算");
        assert_eq!(descriptor, JvmTypeDescriptor::Object(jvm_layout_class_name(&plan.layouts[1])));
    }

    #[test]
    fn missing_apply_mapping_fails_closed() {
        let plan = plan_with_closed_apply(false);
        let error = jvm_type_descriptor(&plan, &closed_apply()).expect_err("缺少映射必须失败");
        assert!(error.contains("缺少冻结布局映射"), "got: {error}");
    }

    #[test]
    fn open_generic_apply_fails_closed() {
        let open = SemanticType::Apply(
            Box::new(SemanticType::Named(Identifier::new("ArrayList"))),
            vec![SemanticType::Named(Identifier::new("T"))],
        );
        let error = jvm_type_descriptor(&AggregateLayoutPlan::default(), &open).expect_err("开放泛型不得发射");
        assert!(error.contains("缺少冻结布局映射"), "got: {error}");
    }

    /// 两个闭合泛型实例（`ArrayList<i64>` / `ArrayList<f64>`）必须各自命中冻结布局。
    #[test]
    fn distinct_closed_generic_instances_get_distinct_carriers() {
        let int_items = closed_apply();
        let float_items = SemanticType::Apply(
            Box::new(SemanticType::Named(Identifier::new("std::collection::ArrayList"))),
            vec![SemanticType::Float64],
        );
        let layout = |id: u32| AggregateLayout {
            id,
            name: "ArrayList".to_string(),
            namespace: String::new(),
            key: None,
            storage: StorageKind::Reference,
            fields: vec![FieldLayout { name: "len".to_string(), ty: SemanticType::Integer64 { signed: true } }],
        };
        let mut plan = AggregateLayoutPlan { layouts: vec![layout(1), layout(2)], ..Default::default() };
        // `layout_by_semantic_type` 的值是 `LayoutId` 而不是 `layouts` 下标：
        // 闭合实例必须各自指向自己的布局身份。
        plan.layout_by_semantic_type.insert(int_items.clone(), 1);
        plan.layout_by_semantic_type.insert(float_items.clone(), 2);
        let int_descriptor = jvm_type_descriptor(&plan, &int_items).expect("`ArrayList<i64>` 必须命中冻结布局");
        let float_descriptor = jvm_type_descriptor(&plan, &float_items).expect("`ArrayList<f64>` 必须命中冻结布局");
        assert_ne!(int_descriptor, float_descriptor, "两个闭合泛型实例不得共用一个载体");
    }

    /// 适配器直接结算单值载体：`Character` 固定 32 位 `int`、字符串族落 `java/lang/String`。
    #[test]
    fn scalar_and_string_carriers_settle_to_declared_physical_forms() {
        let plan = AggregateLayoutPlan::default();
        assert_eq!(jvm_type_descriptor(&plan, &SemanticType::Character).expect("char 必须有载体"), JvmTypeDescriptor::Int);
        assert_eq!(
            jvm_type_descriptor(&plan, &SemanticType::Utf16).expect("utf16 必须有载体"),
            JvmTypeDescriptor::Object("java/lang/String".to_string())
        );
        assert!(jvm_parameter_descriptor(&plan, &SemanticType::Unit).expect_err("unit 没有值槽").contains("void 形态"));
    }

    /// 数组元素位置按 data-binary-jvm 的存取指令合同落装箱引用。
    #[test]
    fn wide_array_elements_settle_to_boxed_references() {
        let plan = AggregateLayoutPlan::default();
        let element = SemanticType::Array(Box::new(SemanticType::Float64));
        assert_eq!(
            jvm_type_descriptor(&plan, &element).expect("f64 数组必须有载体"),
            JvmTypeDescriptor::array(JvmTypeDescriptor::Double)
        );
        assert_eq!(
            jvm_array_element_descriptor(&plan, &SemanticType::Integer64 { signed: true }).expect("i64 元素必须有载体"),
            JvmTypeDescriptor::Long
        );
    }
}
