//! JVM 目标私有输入与装配原语。
//!
//! 公共 IR 解释阶段已拆除：字节码发射与计划事实结算（方法命名、ABI 契约、
//! launcher、布局/sum companion 类）全部由语言侧 JVM lane 直读 MIR 完成。
//! 本模块只保留 JVM 载体合同原语（类型 descriptor 派生、槽位种类）与装配服务：
//! [`JvmProgramInput`] 是语言侧一次结算的目标私有输入，[`assemble_jvm_class`]
//! 只接受这个形状，做 companion 载体唯一性自检并封装主类，不再接触任何
//! 跨目标共享程序载荷。

use std::collections::BTreeSet;

use crate::{
    Result,
    managed_abi::ManagedAbiContract,
    target_operator::TargetOperandType,
    transitional::binary::class::{JvmClassFile, JvmInstruction, JvmMethodSignature, JvmTypeDescriptor},
};
use compiler_types::{AggregateLayout, AggregateLayoutPlan, SemanticType};

/// JVM 槽位种类：决定 `iload`/`lload`/`dload` 等指令与宽度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JvmSlotKind {
    /// `int` / `boolean` / `byte` / `char` / `short`，占 1 槽。
    Int,
    /// `long`，占 2 槽。
    Long,
    /// `float`，占 1 槽。
    Float,
    /// `double`，占 2 槽。
    Double,
    /// 引用，占 1 槽。
    Reference,
}

impl JvmSlotKind {
    pub fn of(descriptor: &JvmTypeDescriptor) -> Result<Self> {
        Ok(match descriptor {
            JvmTypeDescriptor::Void => return Err("`void` 不能作为局部变量或操作数载体".to_string()),
            JvmTypeDescriptor::Long => Self::Long,
            JvmTypeDescriptor::Float => Self::Float,
            JvmTypeDescriptor::Double => Self::Double,
            JvmTypeDescriptor::Object(_) | JvmTypeDescriptor::Array(_) => Self::Reference,
            JvmTypeDescriptor::Byte
            | JvmTypeDescriptor::Char
            | JvmTypeDescriptor::Int
            | JvmTypeDescriptor::Short
            | JvmTypeDescriptor::Boolean => Self::Int,
        })
    }

    pub fn of_operand_type(operand_type: TargetOperandType) -> Self {
        match operand_type {
            TargetOperandType::I32 => Self::Int,
            TargetOperandType::I64 => Self::Long,
            TargetOperandType::F32 => Self::Float,
            TargetOperandType::F64 => Self::Double,
        }
    }

    pub fn slot_width(self) -> u16 {
        match self {
            Self::Long | Self::Double => 2,
            Self::Int | Self::Float | Self::Reference => 1,
        }
    }

    pub fn load(self, slot: u16) -> JvmInstruction {
        match self {
            Self::Int => JvmInstruction::ILoad(slot),
            Self::Long => JvmInstruction::LLoad(slot),
            Self::Float => JvmInstruction::FLoad(slot),
            Self::Double => JvmInstruction::DLoad(slot),
            Self::Reference => JvmInstruction::ALoad(slot),
        }
    }

    pub fn store(self, slot: u16) -> JvmInstruction {
        match self {
            Self::Int => JvmInstruction::IStore(slot),
            Self::Long => JvmInstruction::LStore(slot),
            Self::Float => JvmInstruction::FStore(slot),
            Self::Double => JvmInstruction::DStore(slot),
            Self::Reference => JvmInstruction::AStore(slot),
        }
    }
}

/// JVM lowering 结果：主类、launcher companion 类与 managed ABI 契约。
#[derive(Debug, Clone)]
pub struct JvmLoweredFragment {
    /// 片段主类。
    pub class_file: JvmClassFile,
    /// 每个导出入口独立的 argv launcher 类。
    pub companion_classes: Vec<JvmClassFile>,
    /// 导出入口 ABI 契约；没有可导出入口时为 `None`。
    pub managed_abi: Option<ManagedAbiContract>,
    /// 程序入口（`[main]`）的 argv launcher 内部类名。
    ///
    /// 入口身份来自冻结的计划，而不是扫描主类里是否恰好有名为 `main` 的方法。
    pub entry_launcher: Option<String>,
}

/// JVM 目标私有输入：一次冻结的方法计划、已降低方法体与 companion 载体。
///
/// 计划事实（可达导出面、方法命名与 descriptor、布局/sum 载体、managed ABI、
/// 入口 launcher）由语言侧 JVM lane 一次结算；装配服务 [`assemble_jvm_class`]
/// 只接受这个形状，不再接触跨目标的共享程序载荷。
pub struct JvmProgramInput {
    internal_name: String,
    methods: Vec<JvmMethodSignature>,
    companion_classes: Vec<JvmClassFile>,
    managed_abi: Option<ManagedAbiContract>,
    entry_launcher: Option<String>,
}

impl JvmProgramInput {
    /// 结算完成的 JVM 目标私有输入：主类内部名、已降低方法体与 companion 载体。
    pub fn new(
        internal_name: String,
        methods: Vec<JvmMethodSignature>,
        companion_classes: Vec<JvmClassFile>,
        managed_abi: Option<ManagedAbiContract>,
        entry_launcher: Option<String>,
    ) -> Self {
        Self { internal_name, methods, companion_classes, managed_abi, entry_launcher }
    }

    /// 主类内部名（由模块名与片段身份一次冻结）。
    pub fn internal_name(&self) -> &str {
        &self.internal_name
    }

    /// 已降低的方法体（按可达导出面的稳定顺序）。
    pub fn methods(&self) -> &[JvmMethodSignature] {
        &self.methods
    }
}

/// JVM 目标私有输入 → 片段产物：只做 companion 载体唯一性自检并封装主类。
///
/// 签名里没有跨目标共享程序：类编码（`JvmClassFile::to_bytes`）与 JAR 容器由
/// [`crate::backend::jvm`] 在同一份目标私有输入上完成。
pub fn assemble_jvm_class(input: &JvmProgramInput) -> Result<JvmLoweredFragment> {
    let mut companion_names = BTreeSet::new();
    for companion in &input.companion_classes {
        if !companion_names.insert(companion.internal_name.clone()) {
            return Err(format!(
                "JVM 最小 lowering：companion 类内部名 `{}` 冲突：布局类、sum 类与 launcher 类必须各有唯一载体",
                companion.internal_name
            ));
        }
    }
    let mut class_file = JvmClassFile::new(input.internal_name().to_string());
    class_file.methods = input.methods().to_vec();
    Ok(JvmLoweredFragment {
        class_file,
        companion_classes: input.companion_classes.clone(),
        managed_abi: input.managed_abi.clone(),
        entry_launcher: input.entry_launcher.clone(),
    })
}

/// 局部 / 形参位置的 JVM 载体：`Unit`/`Bottom` 没有值槽。
pub fn jvm_parameter_descriptor(layouts: &AggregateLayoutPlan, ty: &SemanticType) -> Result<JvmTypeDescriptor> {
    let descriptor = jvm_type_descriptor(layouts, ty)?;
    if descriptor == JvmTypeDescriptor::Void {
        return Err(format!("JVM 最小 lowering：`{ty}` 是 void 形态，不能作为方法形参或局部值"));
    }
    Ok(descriptor)
}

pub fn jvm_return_descriptor(layouts: &AggregateLayoutPlan, ty: &SemanticType) -> Result<JvmTypeDescriptor> {
    jvm_type_descriptor(layouts, ty)
}

/// 布局 companion 类的稳定内部名：`nyar/layout/<sanitized(name)>$<layout id>`。
///
/// 同一 layout id 稳定、不同布局不撞名；名字只用于 companion 类，不是语义身份。
pub fn jvm_layout_class_name(layout: &AggregateLayout) -> String {
    format!("nyar/layout/{}${}", crate::lowering::sanitize_symbol(&layout.name), layout.id)
}

/// 数组元素位置的 JVM 载体。
///
/// data-binary-jvm 没有 `laload`/`lastore`/`faload`/`daload`，因此 i64/f32/f64 元素
/// 必须用装箱引用数组（`java/lang/Long` / `Double` / `Float`），这是块 3 存取
/// 指令的前提；标量元素选 `NewArray` atype 可直接承载的基本类型。
pub fn jvm_array_element_descriptor(layouts: &AggregateLayoutPlan, element: &SemanticType) -> Result<JvmTypeDescriptor> {
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

pub fn jvm_type_descriptor(layouts: &AggregateLayoutPlan, ty: &SemanticType) -> Result<JvmTypeDescriptor> {
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
mod apply_layout_tests {
    use super::*;
    use compiler_target::Identifier;
    use compiler_types::{FieldLayout, StorageKind};

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

    /// 两个拼写相同的 nominals 必须是两个载体：身份由 layout 记录结算，拼写不参与。
    #[test]
    fn same_spelling_nominals_keep_distinct_carriers() {
        let left = AggregateLayout {
            id: 11,
            name: "Point".to_string(),
            namespace: "app::geometry".to_string(),
            key: None,
            storage: StorageKind::Reference,
            fields: Vec::new(),
        };
        let right = AggregateLayout {
            id: 12,
            name: "Point".to_string(),
            namespace: "app::physics".to_string(),
            key: None,
            storage: StorageKind::Reference,
            fields: Vec::new(),
        };
        assert_ne!(jvm_layout_class_name(&left), jvm_layout_class_name(&right), "同名 nominal 不得折叠到同一载体");
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
}
