//! CLR lane 的类型载体裁决：`SemanticType` → 已选定 `MSIL` 载体。
//!
//! 架构门 D4：源语言类型语义（`Character` 投 32 位无符号 `int32`、`Utf8`/`Utf16`
//! 投 `System.String`、闭合 `Apply` 按冻结 `SemanticType → LayoutId` 索引取布局
//! 类型名、整数 `Cast` 形状）只属于语言 × 目标适配器；emitter 后端不再接收
//! `SemanticType`，只消费本模块结算提交的 signature、类引用与字段载体。
//! 布局身份按实例索引（`LayoutId`）查冻结计划，绝不按名字回退猜布局。

use std::collections::BTreeMap;

use compiler_emitter::emitter_backend_clr::{MsilIntShape, MsilType, clr_layout_type_name};
use compiler_types::{AggregateLayoutPlan, LayoutId, SemanticType};

/// 局部 / 形参位置的 CLR 载体：`Unit`/`Bottom` 没有值槽。
pub(crate) fn msil_parameter_type(layouts: &AggregateLayoutPlan, ty: &SemanticType) -> Result<MsilType, String> {
    let msil_type = msil_type(layouts, ty)?;
    if msil_type == MsilType::Void {
        return Err(format!("CLR 最小 lowering：`{ty}` 是 void 形态，不能作为方法形参或局部值"));
    }
    Ok(msil_type)
}

/// 返回位置的 CLR 载体。
pub(crate) fn msil_return_type(layouts: &AggregateLayoutPlan, ty: &SemanticType) -> Result<MsilType, String> {
    msil_type(layouts, ty)
}

/// `Cast` 的整数形状裁决：位宽 + 符号性（`Character` 是 32 位无符号码位）。
pub(crate) fn msil_int_shape(ty: &SemanticType) -> Option<MsilIntShape> {
    Some(match ty {
        SemanticType::Integer8 { signed } => MsilIntShape { bits: 8, signed: *signed },
        SemanticType::Integer16 { signed } => MsilIntShape { bits: 16, signed: *signed },
        SemanticType::Integer32 { signed } => MsilIntShape { bits: 32, signed: *signed },
        SemanticType::Integer64 { signed } => MsilIntShape { bits: 64, signed: *signed },
        SemanticType::Character => MsilIntShape { bits: 32, signed: false },
        _ => return None,
    })
}

/// 数组类型 `Array<T>` / `FixedArray { element, .. }` 的元素类型。
pub(crate) fn clr_array_element_type(array_type: &SemanticType) -> Result<&SemanticType, String> {
    match array_type {
        SemanticType::Array(element) => Ok(element),
        SemanticType::FixedArray { element, .. } => Ok(element),
        other => Err(format!("CLR 最小 lowering：`{other}` 不是数组形态，缺少元素类型合同")),
    }
}

/// 数组元素位置的 CLR 载体。
///
/// data-binary-pe 没有 `ldelem.i8`/`stelem.i8`，i64 元素由块 4 用
/// `ldelem.any`/`stelem.any` + `Type("int64")` 存取；载体本身仍是 `Int64`。
pub(crate) fn msil_array_element_type(layouts: &AggregateLayoutPlan, element: &SemanticType) -> Result<MsilType, String> {
    Ok(match element {
        SemanticType::Boolean => MsilType::Bool,
        SemanticType::Integer8 { signed } => MsilType::Int8 { signed: *signed },
        SemanticType::Integer16 { signed } => MsilType::Int16 { signed: *signed },
        SemanticType::Integer32 { signed } => MsilType::Int32 { signed: *signed },
        // V 的 `char` 是 u32 码位载体；CLR `Char` 只有 16 位，物理载体固定 32 位 int32。
        SemanticType::Character => MsilType::Int32 { signed: false },
        SemanticType::Integer64 { signed } => MsilType::Int64 { signed: *signed },
        SemanticType::Float32 => MsilType::Float32,
        SemanticType::Float64 => MsilType::Float64,
        SemanticType::Utf8 | SemanticType::Utf16 => MsilType::String,
        // 闭合泛型实例的布局身份只来自计划冻结的 `SemanticType → LayoutId` 索引；
        // 缺映射或映射悬空都回到计划生产者修，开放泛型仍在这里失败。
        SemanticType::Apply(..) => {
            let Some(layout) = layouts.frozen_layout(element) else {
                return Err(format!(
                    "CLR 最小 lowering：泛型实例 `{element}` 缺少冻结布局映射：计划未把该闭合实例登记到 `SemanticType → LayoutId`（开放泛型不得进入发射）"
                ));
            };
            if layout.storage.is_reference() {
                MsilType::Named(clr_layout_type_name(layout))
            }
            else {
                return Err(format!("CLR 最小 lowering 不支持值存储泛型实例 `{element}` 作为数组元素：缺少单槽 CLR 载体合同"));
            }
        }
        // 名义布局按冻结身份取类型定义名；值存储聚合没有单槽 CLR 载体。
        SemanticType::Nominal { layout, spelling } => {
            let Some(layout) = layouts.layouts.iter().find(|candidate| candidate.id == *layout)
            else {
                return Err(format!("CLR 最小 lowering：`{spelling}` 的布局身份 `{layout}` 未在计划中注册：缺少数组元素 CLR 载体合同"));
            };
            if layout.storage.is_reference() {
                MsilType::Named(clr_layout_type_name(layout))
            }
            else {
                return Err(format!("CLR 最小 lowering 不支持值存储聚合 `{spelling}` 作为数组元素：缺少单槽 CLR 载体合同"));
            }
        }
        SemanticType::Array(inner) | SemanticType::FixedArray { element: inner, .. } => {
            MsilType::sz_array(msil_array_element_type(layouts, inner)?)
        }
        // 可空元素只能复用引用载体（`null` 表示空）；标量需要 sum 表示。
        SemanticType::Nullable(inner) => {
            let ty = msil_array_element_type(layouts, inner)?;
            if matches!(ty, MsilType::String | MsilType::Object | MsilType::Named(_) | MsilType::SzArray(_)) {
                ty
            }
            else {
                return Err(format!("CLR 最小 lowering：可空标量 `{element}` 需要 sum 表示，不能擦成引用"));
            }
        }
        other => {
            return Err(format!("CLR 最小 lowering 不支持数组元素类型 `{other}`：缺少该形状的 CLR 载体合同"));
        }
    })
}

/// 类型的 CLR 载体：标量取原生类型，数组取 `SzArray`，引用布局取类型定义名。
pub(crate) fn msil_type(layouts: &AggregateLayoutPlan, ty: &SemanticType) -> Result<MsilType, String> {
    Ok(match ty {
        SemanticType::Bottom | SemanticType::Unit => MsilType::Void,
        SemanticType::Boolean => MsilType::Bool,
        // V 的 `char` 是 u32 码位，载体固定 32 位；不得用宿主 16 位 `Char`。
        SemanticType::Character => MsilType::Int32 { signed: false },
        SemanticType::Integer8 { signed } => MsilType::Int8 { signed: *signed },
        SemanticType::Integer16 { signed } => MsilType::Int16 { signed: *signed },
        SemanticType::Integer32 { signed } => MsilType::Int32 { signed: *signed },
        SemanticType::Integer64 { signed } => MsilType::Int64 { signed: *signed },
        SemanticType::Float32 => MsilType::Float32,
        SemanticType::Float64 => MsilType::Float64,
        SemanticType::Utf8 | SemanticType::Utf16 => MsilType::String,
        // 栈/堆数组的载体是元素载体数组；`[T; N]` 的长度由 `ArrayFromElements` 表达。
        SemanticType::Array(element) => MsilType::sz_array(msil_array_element_type(layouts, element)?),
        SemanticType::FixedArray { element, .. } => MsilType::sz_array(msil_array_element_type(layouts, element)?),
        // 布局身份只经 `TupleNew`/`AggregateCopy` 的冻结布局事实进入后端，类型描述符
        // 没有该事实，禁止按形状猜布局。
        SemanticType::Tuple(..) => {
            return Err(format!(
                "CLR 最小 lowering：裸 tuple `{ty}` 没有布局身份：布局身份只经 `TupleNew`/`AggregateCopy` 的冻结布局事实进入后端，类型描述符没有该事实"
            ));
        }
        // 可空引用直接复用引用载体（`null` 表示空）；标量需要 sum 表示，不得静默装箱。
        SemanticType::Nullable(inner) => {
            let ty = msil_type(layouts, inner)?;
            if matches!(ty, MsilType::String | MsilType::Object | MsilType::Named(_) | MsilType::SzArray(_)) {
                ty
            }
            else if ty == MsilType::Void {
                return Err(format!("CLR 最小 lowering：可空 `{ty}` 的内层是 void，没有可复用的引用载体"));
            }
            else {
                return Err(format!("CLR 最小 lowering：可空标量 `{ty}` 需要 sum 表示，不得静默装箱"));
            }
        }
        // 闭合泛型实例的布局身份只来自计划冻结的 `SemanticType → LayoutId` 索引。
        // 与 `Nominal` 分支同一存储规则；缺映射、悬空或身份冲突回计划生产者修。
        SemanticType::Apply(..) => {
            let Some(layout) = layouts.frozen_layout(ty) else {
                return Err(format!(
                    "CLR 最小 lowering：泛型实例 `{ty}` 缺少冻结布局映射：计划未把该闭合实例登记到 `SemanticType → LayoutId`（开放泛型不得进入发射）"
                ));
            };
            if layout.storage.is_reference() || !layout.fields.is_empty() {
                MsilType::Named(clr_layout_type_name(layout))
            }
            else {
                return Err(format!("CLR 最小 lowering 不支持零字段值存储泛型实例 `{ty}`：缺少单槽 CLR 载体合同"));
            }
        }
        // 携带身份的名义类型：存储类按 `layout` 身份解析，引用/值存储都指向
        // 布局类型定义。CLR 没有内联结构体，逐字段拷贝由 `AggregateCopy` 承担，
        // 它是唯一允许的复制路径；零字段值布局与 wasm 标量语义一致，维持结构化失败。
        SemanticType::Nominal { layout, spelling } => {
            let Some(layout) = layouts.layouts.iter().find(|candidate| candidate.id == *layout)
            else {
                return Err(format!("CLR 最小 lowering：`{spelling}` 的布局身份 `{layout}` 未在计划中注册：缺少该形状的 CLR 载体合同"));
            };
            if layout.storage.is_reference() || !layout.fields.is_empty() {
                MsilType::Named(clr_layout_type_name(layout))
            }
            else {
                return Err(format!("CLR 最小 lowering 不支持零字段值存储聚合 `{spelling}`：缺少单槽 CLR 载体合同"));
            }
        }
        other => {
            return Err(format!("CLR 最小 lowering 不支持类型 `{other}`：缺少该形状的 CLR 载体合同"));
        }
    })
}

/// 适配器结算布局字段载体：把冻结计划的字段 `SemanticType` 投影为已选定 `MsilType`。
///
/// 只为需要类型定义的布局（引用存储或非零字段）结算；后端 `build_layout_types`
/// 只消费这份已结算载体，不再读字段语义类型。
pub(crate) fn settle_layout_field_carriers(layouts: &AggregateLayoutPlan) -> Result<BTreeMap<LayoutId, Vec<MsilType>>, String> {
    let mut carriers = BTreeMap::new();
    for layout in &layouts.layouts {
        if !(layout.storage.is_reference() || !layout.fields.is_empty()) {
            continue;
        }
        carriers.insert(
            layout.id,
            layout.fields.iter().map(|field| msil_type(layouts, &field.ty)).collect::<Result<Vec<_>, String>>()?,
        );
    }
    Ok(carriers)
}

#[cfg(test)]
mod carrier_settlement_tests {
    use compiler_target::Identifier;
    use compiler_types::{AggregateLayout, FieldLayout, StorageKind};

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
    fn closed_apply_resolves_to_frozen_type_definition() {
        let plan = plan_with_closed_apply(true);
        let ty = msil_type(&plan, &closed_apply()).expect("闭合 Apply 必须由冻结索引结算");
        assert_eq!(ty, MsilType::Named(clr_layout_type_name(&plan.layouts[1])));
    }

    #[test]
    fn missing_apply_mapping_fails_closed() {
        let plan = plan_with_closed_apply(false);
        let error = msil_type(&plan, &closed_apply()).expect_err("缺少映射必须失败");
        assert!(error.contains("缺少冻结布局映射"), "got: {error}");
    }

    #[test]
    fn open_generic_apply_fails_closed() {
        let open = SemanticType::Apply(
            Box::new(SemanticType::Named(Identifier::new("ArrayList"))),
            vec![SemanticType::Named(Identifier::new("T"))],
        );
        let error = msil_type(&AggregateLayoutPlan::default(), &open).expect_err("开放泛型不得发射");
        assert!(error.contains("缺少冻结布局映射"), "got: {error}");
    }

    /// 适配器直接结算标量载体：`Character` 固定 32 位无符号、字符串族落 `System.String`。
    #[test]
    fn scalar_and_string_carriers_settle_to_declared_physical_forms() {
        let plan = AggregateLayoutPlan::default();
        assert_eq!(msil_type(&plan, &SemanticType::Character).expect("char 必须有载体"), MsilType::Int32 { signed: false });
        assert_eq!(msil_type(&plan, &SemanticType::Utf16).expect("utf16 必须有载体"), MsilType::String);
        assert!(msil_parameter_type(&plan, &SemanticType::Unit).expect_err("unit 没有值槽").contains("void 形态"));
        assert_eq!(msil_int_shape(&SemanticType::Character), Some(MsilIntShape { bits: 32, signed: false }));
        assert_eq!(msil_int_shape(&SemanticType::Float64), None);
    }

    /// 字段载体结算只覆盖需要类型定义的布局，且逐槽对齐字段顺序。
    #[test]
    fn layout_field_carriers_settle_per_slot_in_field_order() {
        let plan = plan_with_closed_apply(true);
        let carriers = settle_layout_field_carriers(&plan).expect("字段载体必须由适配器结算");
        assert_eq!(carriers.len(), 2);
        assert_eq!(carriers[&0], vec![MsilType::Named(clr_layout_type_name(&plan.layouts[1]))]);
        assert_eq!(carriers[&1], vec![MsilType::Int64 { signed: true }]);
    }
}
