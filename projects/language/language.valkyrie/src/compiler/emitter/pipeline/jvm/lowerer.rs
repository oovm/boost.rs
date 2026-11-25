//! JVM lane 的函数级 lowering：MIR → 目标私有 JVM 字节码。
//!
//! 本模块直读 `ValkyrieMirFunction` 的块/指令/终结符，逐操作映射为 JVM 指令序列
//! （经 emitter 的 `JvmSlotKind` / descriptor 派生 / operator 绑定等目标私有编码
//! 原语），不经过任何跨目标共享程序载荷（旧路径的 submission 解释阶段已删除）。
//! 旧公共路径只是把 MIR 直投影成公共 IR 再解释，本文件把两段映射合成一段，
//! 语义逐操作对齐（含 `long` scratch 逐函数结算、值聚合拷贝深度预算、块形参
//! 拷贝的冻结槽位合同、operator 分支标签重标等审查点名合同）。

use std::collections::BTreeMap;

use compiler_emitter::emitter_backend_jvm::{
    JvmClassFile, JvmCodeBody, JvmFieldRef, JvmFieldSignature, JvmInstruction, JvmMethodDescriptor, JvmMethodRef, JvmMethodSignature,
    JvmOperatorBinding, JvmOperatorCode, JvmSlotKind, JvmTypeDescriptor, MAX_VALUE_AGGREGATE_COPY_DEPTH, jvm_array_element_descriptor,
    jvm_layout_class_name, jvm_parameter_descriptor, jvm_type_descriptor, nested_value_aggregate_layout, value_aggregate_copy_depth,
};
use compiler_emitter::target_operator::{TargetOperandStep, TargetOperatorPlan, TargetOperatorRecipe};
use compiler_types::{AggregateLayout, AggregateLayoutPlan, FieldId, ItemInstanceId, LayoutId, NominalInstanceId, SemanticType, VariantId};
use compiler_types::semantic_ids::{MirValueId, TypeId};

use crate::compiler::emitter::pipeline::lane::{FunctionView, LaneContext};
use crate::compiler::valkyrie_mir::{
    ValkyrieMirArrayInitialization, ValkyrieMirBlock, ValkyrieMirBlockId, ValkyrieMirCallee, ValkyrieMirConstant, ValkyrieMirFunction,
    ValkyrieMirInstruction, ValkyrieMirOperand, ValkyrieMirOperation, ValkyrieMirTerminator,
};

pub(crate) const JVM_ACC_PUBLIC: u16 = 0x0001;
pub(crate) const JVM_ACC_STATIC: u16 = 0x0008;

/// sum companion 类的物理字段名；字段顺序由后端合同固定，禁止按 variant 名推断槽位。
pub(crate) const JVM_SUM_TAG_FIELD: &str = "tag";
pub(crate) const JVM_SUM_PAYLOAD_FIELD: &str = "payload";

/// 引用存储形状在 JVM 上的擦除载体：最小 lowering 只搬运引用（`aload`/`astore`/
/// `areturn`/调用描述符），值的物化与字段访问由计划冻结的实例级布局
/// （discriminator + payload 槽）与绑定 operator 负责，backend 不展开字段。
pub(crate) const JVM_REFERENCE_CARRIER: &str = "java/lang/Object";

/// 块标签：与旧公共路径 `block_label(BlockRef(id))` 同形（`block_{N}`，N 为块身份）。
fn block_label(block: ValkyrieMirBlockId) -> String {
    format!("block_{}", block.0)
}

/// 函数级 JVM 方法计划：稳定名字与冻结签名。
#[derive(Debug, Clone)]
pub(crate) struct JvmMethodPlan {
    pub(crate) name: String,
    pub(crate) descriptor: JvmMethodDescriptor,
}

/// 逐函数局部槽位结算：SSA 值 → 目标私有 local。
///
/// 分配顺序与旧公共路径的 `ExecutableSlotPlan::plan_semantic` 完全一致：入口块
/// 形参最先，其余块形参按块身份升序，然后全部指令结果按块升序逐指令登记；
/// `local_count` 因此逐函数对齐旧字节码。
#[derive(Default)]
pub(crate) struct SlotPlan {
    pub(crate) local_types: Vec<SemanticType>,
    pub(crate) value_locals: BTreeMap<MirValueId, u16>,
    pub(crate) block_param_locals: BTreeMap<(ValkyrieMirBlockId, usize), u16>,
}

impl SlotPlan {
    fn alloc(&mut self, ty: SemanticType) -> u16 {
        let local = u16::try_from(self.local_types.len()).expect("JVM local 数量超出 u16");
        self.local_types.push(ty);
        local
    }

    /// 从 MIR 函数与冻结类型视图结算槽位表。
    pub(crate) fn plan(function: &ValkyrieMirFunction, view: &FunctionView) -> Result<Self, String> {
        let mut plan = Self::default();
        let entry = function.blocks.get(&function.entry).ok_or_else(|| {
            format!("JVM 入口槽合同破坏：symbol=`{}` 声明了入口块 {}，但块表中不存在该入口块", view.symbol, function.entry.0)
        })?;
        for (value, _) in &entry.parameters {
            let ty = view.value_types.get(value).cloned().ok_or_else(|| format!("JVM 入口形参 %{} 缺少冻结类型（symbol=`{}`）", value.index(), view.symbol))?;
            let local = plan.alloc(ty);
            plan.value_locals.insert(*value, local);
        }
        for block in function.blocks.values() {
            if block.id == function.entry {
                continue;
            }
            for (value, _) in &block.parameters {
                let ty = view.value_types.get(value).cloned().ok_or_else(|| format!("JVM 块形参 %{} 缺少冻结类型（symbol=`{}`）", value.index(), view.symbol))?;
                let local = plan.alloc(ty);
                plan.value_locals.insert(*value, local);
            }
        }
        for block in function.blocks.values() {
            for instruction in &block.instructions {
                for output in &instruction.results {
                    let ty = view
                        .value_types
                        .get(output)
                        .cloned()
                        .ok_or_else(|| format!("JVM 指令结果 %{} 缺少冻结类型（symbol=`{}`）", output.index(), view.symbol))?;
                    let local = plan.alloc(ty);
                    plan.value_locals.insert(*output, local);
                }
            }
        }
        // 块形参的 (块, 位置) 索引：与 value_locals 同一 local，供块参数拷贝回填。
        for block in function.blocks.values() {
            for (index, (value, _)) in block.parameters.iter().enumerate() {
                if let Some(local) = plan.value_locals.get(value).copied() {
                    plan.block_param_locals.insert((block.id, index), local);
                }
            }
        }
        Ok(plan)
    }

    /// 指令的主结果槽（旧公共路径同样只认第一个结果）。
    pub(crate) fn instruction_output(instruction: &ValkyrieMirInstruction) -> Option<MirValueId> {
        instruction.results.first().copied()
    }
}

/// 数据流标准逆后序（RPO）：块发射顺序与旧公共路径同源。
///
/// 顶点是 MIR 块身份（`u32`）；悬空跳转目标保留为无子顶点（发射期跳过不存在的块，
/// 与旧实现一致）。
pub(crate) fn collect_reachable_blocks(function: &ValkyrieMirFunction) -> Vec<ValkyrieMirBlockId> {
    let mut adjacency: Vec<(u32, u32)> = Vec::new();
    for block in function.blocks.values() {
        let from = block.id.0;
        let targets: Vec<u32> = match &block.terminator {
            ValkyrieMirTerminator::Return { .. } | ValkyrieMirTerminator::Unreachable | ValkyrieMirTerminator::YieldToRuntime { .. } => Vec::new(),
            ValkyrieMirTerminator::Jump { target, .. } => vec![target.0],
            ValkyrieMirTerminator::Branch { then_target, else_target, .. } => vec![then_target.0, else_target.0],
            ValkyrieMirTerminator::PerformEffect { resume_target, .. } => vec![resume_target.0],
            ValkyrieMirTerminator::StateDispatch { cases, default_target, .. } => {
                let mut targets = cases.iter().map(|(_, target)| target.0).collect::<Vec<_>>();
                targets.push(default_target.0);
                targets
            }
        };
        adjacency.extend(targets.into_iter().map(|target| (from, target)));
    }

    let mut graph = boost_collection::MemoryGraph::new(true);
    graph.add_vertex(function.entry.0).expect("fresh graph");
    for block in function.blocks.values() {
        graph.add_vertex(block.id.0).ok();
    }
    for (_, target) in &adjacency {
        graph.add_vertex(*target).ok();
    }
    for (from, target) in adjacency {
        graph.add_edge(from, target, ()).expect("both endpoints were added as vertices");
    }

    boost_math::graph_theory::reverse_postorder(&graph, &function.entry.0)
        .expect("the entry is a vertex of the graph")
        .into_iter()
        .map(ValkyrieMirBlockId)
        .collect()
}

/// 整数 `as` 的表示形状（位宽 + 符号性；`Character` 是 32 位无符号码位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct JvmIntShape {
    bits: u8,
    signed: bool,
}

fn jvm_int_shape(ty: &SemanticType) -> Option<JvmIntShape> {
    Some(match ty {
        SemanticType::Integer8 { signed } => JvmIntShape { bits: 8, signed: *signed },
        SemanticType::Integer16 { signed } => JvmIntShape { bits: 16, signed: *signed },
        SemanticType::Integer32 { signed } => JvmIntShape { bits: 32, signed: *signed },
        SemanticType::Integer64 { signed } => JvmIntShape { bits: 64, signed: *signed },
        SemanticType::Character => JvmIntShape { bits: 32, signed: false },
        _ => return None,
    })
}

/// 局部 / 形参位置的 JVM 槽位种类：`Unit`/`Bottom` 没有值槽。
fn jvm_local_kind(layouts: &AggregateLayoutPlan, ty: &SemanticType) -> Result<JvmSlotKind, String> {
    JvmSlotKind::of(&jvm_parameter_descriptor(layouts, ty)?)
}

/// 数组创建指令：标量元素走 `newarray`，引用/装箱元素走 `anewarray`。
fn jvm_array_creation_instruction(element: &JvmTypeDescriptor) -> Result<JvmInstruction, String> {
    Ok(match element {
        JvmTypeDescriptor::Boolean => JvmInstruction::NewArray(4),
        JvmTypeDescriptor::Char => JvmInstruction::NewArray(5),
        JvmTypeDescriptor::Byte => JvmInstruction::NewArray(8),
        JvmTypeDescriptor::Short => JvmInstruction::NewArray(9),
        JvmTypeDescriptor::Int => JvmInstruction::NewArray(10),
        // JVM 原生支持 `long`/`float`/`double` 基本类型数组；不再为编码器缺能力
        // 而改变语言数组的物理表示或分配行为。
        JvmTypeDescriptor::Long => JvmInstruction::NewArray(11),
        JvmTypeDescriptor::Float => JvmInstruction::NewArray(6),
        JvmTypeDescriptor::Double => JvmInstruction::NewArray(7),
        JvmTypeDescriptor::Object(name) => JvmInstruction::ANewArray(name.clone()),
        JvmTypeDescriptor::Array(_) => JvmInstruction::ANewArray(element.to_string()),
        JvmTypeDescriptor::Void => return Err("JVM 最小 lowering：`void` 不能作为数组元素载体".to_string()),
    })
}

/// 数组元素读写指令；宽值/引用元素统一走 `aaload`/`aastore`。
fn jvm_array_access_instructions(element: &JvmTypeDescriptor) -> Result<(JvmInstruction, JvmInstruction), String> {
    Ok(match element {
        JvmTypeDescriptor::Boolean | JvmTypeDescriptor::Byte => (JvmInstruction::BALoad, JvmInstruction::BAStore),
        JvmTypeDescriptor::Char => (JvmInstruction::CALoad, JvmInstruction::CAStore),
        JvmTypeDescriptor::Short => (JvmInstruction::SALoad, JvmInstruction::SAStore),
        JvmTypeDescriptor::Int => (JvmInstruction::IALoad, JvmInstruction::IAStore),
        JvmTypeDescriptor::Long => (JvmInstruction::LALoad, JvmInstruction::LAStore),
        JvmTypeDescriptor::Float => (JvmInstruction::FALoad, JvmInstruction::FAStore),
        JvmTypeDescriptor::Double => (JvmInstruction::DALoad, JvmInstruction::DAStore),
        JvmTypeDescriptor::Object(_) | JvmTypeDescriptor::Array(_) => (JvmInstruction::AALoad, JvmInstruction::AAStore),
        JvmTypeDescriptor::Void => return Err("JVM 最小 lowering：`void` 不能作为数组元素载体".to_string()),
    })
}

/// 标量载体的装箱类：i8/i16/i32 统一 `Integer`，其余按精确标量盒。
fn jvm_box_class_name(descriptor: &JvmTypeDescriptor) -> Result<String, String> {
    Ok(match descriptor {
        JvmTypeDescriptor::Long => "java/lang/Long".to_string(),
        JvmTypeDescriptor::Float => "java/lang/Float".to_string(),
        JvmTypeDescriptor::Double => "java/lang/Double".to_string(),
        JvmTypeDescriptor::Boolean => "java/lang/Boolean".to_string(),
        JvmTypeDescriptor::Char => "java/lang/Character".to_string(),
        JvmTypeDescriptor::Int | JvmTypeDescriptor::Byte | JvmTypeDescriptor::Short => "java/lang/Integer".to_string(),
        JvmTypeDescriptor::Object(name) => name.clone(),
        JvmTypeDescriptor::Array(_) => descriptor.to_string(),
        JvmTypeDescriptor::Void => return Err("JVM 最小 lowering：`void` 没有装箱载体".to_string()),
    })
}

fn jvm_box_value_of(descriptor: &JvmTypeDescriptor) -> Result<JvmMethodRef, String> {
    let (owner, parameter, return_type) = match descriptor {
        JvmTypeDescriptor::Long => ("java/lang/Long", JvmTypeDescriptor::Long, JvmTypeDescriptor::Object("java/lang/Long".to_string())),
        JvmTypeDescriptor::Float => ("java/lang/Float", JvmTypeDescriptor::Float, JvmTypeDescriptor::Object("java/lang/Float".to_string())),
        JvmTypeDescriptor::Double => ("java/lang/Double", JvmTypeDescriptor::Double, JvmTypeDescriptor::Object("java/lang/Double".to_string())),
        JvmTypeDescriptor::Boolean => {
            ("java/lang/Boolean", JvmTypeDescriptor::Boolean, JvmTypeDescriptor::Object("java/lang/Boolean".to_string()))
        }
        JvmTypeDescriptor::Char => {
            ("java/lang/Character", JvmTypeDescriptor::Char, JvmTypeDescriptor::Object("java/lang/Character".to_string()))
        }
        JvmTypeDescriptor::Int | JvmTypeDescriptor::Byte | JvmTypeDescriptor::Short => {
            ("java/lang/Integer", JvmTypeDescriptor::Int, JvmTypeDescriptor::Object("java/lang/Integer".to_string()))
        }
        JvmTypeDescriptor::Object(_) | JvmTypeDescriptor::Array(_) | JvmTypeDescriptor::Void => {
            return Err(format!("JVM 最小 lowering：`{descriptor}` 不是可装箱标量载体"));
        }
    };
    Ok(JvmMethodRef {
        owner: owner.to_string(),
        name: "valueOf".to_string(),
        descriptor: JvmMethodDescriptor::new(vec![parameter], return_type),
    })
}

fn jvm_unbox_method(descriptor: &JvmTypeDescriptor) -> Result<JvmMethodRef, String> {
    let (owner, name, return_type) = match descriptor {
        JvmTypeDescriptor::Long => ("java/lang/Long", "longValue", JvmTypeDescriptor::Long),
        JvmTypeDescriptor::Float => ("java/lang/Float", "floatValue", JvmTypeDescriptor::Float),
        JvmTypeDescriptor::Double => ("java/lang/Double", "doubleValue", JvmTypeDescriptor::Double),
        JvmTypeDescriptor::Boolean => ("java/lang/Boolean", "booleanValue", JvmTypeDescriptor::Boolean),
        JvmTypeDescriptor::Char => ("java/lang/Character", "charValue", JvmTypeDescriptor::Char),
        JvmTypeDescriptor::Int | JvmTypeDescriptor::Byte | JvmTypeDescriptor::Short => {
            ("java/lang/Integer", "intValue", JvmTypeDescriptor::Int)
        }
        JvmTypeDescriptor::Object(_) | JvmTypeDescriptor::Array(_) | JvmTypeDescriptor::Void => {
            return Err(format!("JVM 最小 lowering：`{descriptor}` 不是可解箱的标量载体"));
        }
    };
    Ok(JvmMethodRef { owner: owner.to_string(), name: name.to_string(), descriptor: JvmMethodDescriptor::new(Vec::new(), return_type) })
}

/// 数组类型 `Array<T>` / `FixedArray { element, .. }` 的元素类型。
fn jvm_array_element_type(array_type: &SemanticType) -> Result<&SemanticType, String> {
    match array_type {
        SemanticType::Array(element) => Ok(element),
        SemanticType::FixedArray { element, .. } => Ok(element),
        other => Err(format!("JVM 最小 lowering：`{other}` 不是数组类型，缺少元素载体")),
    }
}

/// 布局字段在 companion 类中的唯一名字；构造与字段访问必须共用这一条规则。
pub(crate) fn jvm_layout_field_name(slot: usize) -> String {
    format!("field{slot}")
}

/// sum companion 类的稳定内部名：只由 nominal 身份派生，禁止按 variant 名/短名推断。
pub(crate) fn jvm_sum_class_name(nominal: NominalInstanceId) -> String {
    format!("nyar/sum/${}", nominal.index())
}

/// 布局/sum companion 类的默认构造器签名。
///
/// `JvmClassFile::to_bytes` 对 `code: None` 的 `"<init>"` 方法自动合成
/// `aload_0; invokespecial java/lang/Object.<init>; return`，因此这里不手写空体。
pub(crate) fn jvm_default_constructor() -> JvmMethodSignature {
    JvmMethodSignature {
        name: "<init>".to_string(),
        descriptor: JvmMethodDescriptor::new(Vec::new(), JvmTypeDescriptor::Void),
        access_flags: JVM_ACC_PUBLIC,
        code: None,
    }
}

pub(crate) fn jvm_constructor_ref(owner: &str) -> JvmMethodRef {
    JvmMethodRef {
        owner: owner.to_string(),
        name: "<init>".to_string(),
        descriptor: JvmMethodDescriptor::new(Vec::new(), JvmTypeDescriptor::Void),
    }
}

fn jvm_layout_field_ref(layout: &AggregateLayout, slot: usize, descriptor: JvmTypeDescriptor) -> JvmFieldRef {
    JvmFieldRef { owner: jvm_layout_class_name(layout), name: jvm_layout_field_name(slot), descriptor }
}

/// 把冻结比较序列的占位标签替换为方法内唯一标签。
fn relabel_operator_branch(instruction: JvmInstruction, prefix: &str) -> JvmInstruction {
    let relabel = |label: &str| format!("{prefix}_{label}");
    match instruction {
        JvmInstruction::Label(label) => JvmInstruction::Label(relabel(&label)),
        JvmInstruction::Goto(label) => JvmInstruction::Goto(relabel(&label)),
        JvmInstruction::IfEq(label) => JvmInstruction::IfEq(relabel(&label)),
        JvmInstruction::IfNe(label) => JvmInstruction::IfNe(relabel(&label)),
        JvmInstruction::IfLt(label) => JvmInstruction::IfLt(relabel(&label)),
        JvmInstruction::IfLe(label) => JvmInstruction::IfLe(relabel(&label)),
        JvmInstruction::IfGt(label) => JvmInstruction::IfGt(relabel(&label)),
        JvmInstruction::IfGe(label) => JvmInstruction::IfGe(relabel(&label)),
        JvmInstruction::IfICmpEq(label) => JvmInstruction::IfICmpEq(relabel(&label)),
        JvmInstruction::IfICmpNe(label) => JvmInstruction::IfICmpNe(relabel(&label)),
        JvmInstruction::IfICmpLt(label) => JvmInstruction::IfICmpLt(relabel(&label)),
        JvmInstruction::IfICmpLe(label) => JvmInstruction::IfICmpLe(relabel(&label)),
        JvmInstruction::IfICmpGt(label) => JvmInstruction::IfICmpGt(relabel(&label)),
        JvmInstruction::IfICmpGe(label) => JvmInstruction::IfICmpGe(relabel(&label)),
        other => other,
    }
}

struct JvmMethodEmitter<'a> {
    ctx: &'a LaneContext<'a>,
    internal_name: &'a str,
    function: &'a ValkyrieMirFunction,
    view: &'a FunctionView,
    return_descriptor: JvmTypeDescriptor,
    slots: SlotPlan,
    slot_offsets: Vec<u16>,
    method_plans: &'a BTreeMap<ItemInstanceId, JvmMethodPlan>,
    operator_by_instance: &'a BTreeMap<ItemInstanceId, (String, JvmOperatorBinding)>,
    instructions: Vec<JvmInstruction>,
    current_block: ValkyrieMirBlockId,
    comparison_ordinal: usize,
    /// 无符号 `u64` 比较展开使用的 scratch `long` 槽（2 槽宽），仅当函数实际引用
    /// [`JvmOperatorCode::requires_scratch_long_slot`] 为真的码时在 `max_locals` 尾部
    /// 预留；未预留即未引用。
    scratch_long_slot: Option<u16>,
    /// 值聚合递归复制使用的临时引用槽：每层复制占 2 槽（源/目标），
    /// 依据冻结布局的嵌套深度在 `max_locals` 尾部预留。
    copy_scratch: Vec<u16>,
    /// 编译器内部标签序号（递归复制空值分支使用）。
    label_ordinal: usize,
}

/// 降低单个函数为 JVM 方法签名。
pub(crate) fn lower_function(
    ctx: &LaneContext<'_>,
    internal_name: &str,
    function: &ValkyrieMirFunction,
    view: &FunctionView,
    method: &JvmMethodPlan,
    method_plans: &BTreeMap<ItemInstanceId, JvmMethodPlan>,
    operator_by_instance: &BTreeMap<ItemInstanceId, (String, JvmOperatorBinding)>,
) -> Result<JvmMethodSignature, String> {
    let slots = SlotPlan::plan(function, view)?;
    let mut slot_offsets = Vec::with_capacity(slots.local_types.len());
    let mut local_count = 0u16;
    for ty in &slots.local_types {
        slot_offsets.push(local_count);
        local_count = local_count.saturating_add(jvm_local_kind(ctx.aggregate_layouts(), ty)?.slot_width());
    }

    // 无符号 `u64` 比较的冻结展开需要 2 槽 scratch long 周转（JVM 没有 `swap2`）；
    // 只在函数实际引用该 operator 时预留，避免无关方法的 `max_locals` 虚增。
    let needs_scratch_long = function.blocks.values().flat_map(|block| &block.instructions).any(|instruction| {
        let ValkyrieMirOperation::Invoke { callee: ValkyrieMirCallee::Item(instance), .. } = &instruction.operation
        else {
            return false;
        };
        operator_by_instance.get(instance).is_some_and(
            |(_, binding)| matches!(binding, JvmOperatorBinding::Frozen(plan) if plan.encoding.requires_scratch_long_slot()),
        )
    });
    let scratch_long_slot = if needs_scratch_long {
        let slot = local_count;
        local_count = local_count.saturating_add(2);
        Some(slot)
    }
    else {
        None
    };

    // 值聚合递归复制：按冻结布局的嵌套深度预留 2 槽/层的临时引用槽。
    let layout_plan = ctx.aggregate_layouts();
    let mut copy_depth = 0u32;
    for instruction in function.blocks.values().flat_map(|block| &block.instructions) {
        if let ValkyrieMirOperation::AggregateCopy { source, .. } = &instruction.operation
            && let Some(layout) = function
                .value_types
                .get(source)
                .and_then(|type_id| ctx.aggregate_layout_by_type().get(type_id).copied())
                .and_then(|layout_id| layout_plan.layouts.iter().find(|candidate| candidate.id == layout_id))
        {
            copy_depth = copy_depth.max(value_aggregate_copy_depth(layout_plan, layout));
        }
    }
    if copy_depth > MAX_VALUE_AGGREGATE_COPY_DEPTH {
        return Err(format!(
            "JVM 最小 lowering：值聚合复制嵌套 {copy_depth} 层超过上限 {MAX_VALUE_AGGREGATE_COPY_DEPTH}: function={}",
            view.symbol
        ));
    }
    let mut copy_scratch = Vec::with_capacity((copy_depth as usize).saturating_mul(2));
    for _ in 0..(copy_depth as usize).saturating_mul(2) {
        copy_scratch.push(local_count);
        local_count = local_count.saturating_add(1);
    }

    let mut emitter = JvmMethodEmitter {
        ctx,
        internal_name,
        function,
        view,
        return_descriptor: method.descriptor.return_type.clone(),
        slots,
        slot_offsets,
        method_plans,
        operator_by_instance,
        instructions: Vec::new(),
        current_block: function.entry,
        comparison_ordinal: 0,
        scratch_long_slot,
        copy_scratch,
        label_ordinal: 0,
    };

    for block_id in collect_reachable_blocks(function) {
        let Some(block) = function.blocks.get(&block_id)
        else {
            continue;
        };
        emitter.emit_block(block)?;
    }

    let max_stack = u16::try_from(emitter.instructions.len().saturating_mul(2).saturating_add(2)).unwrap_or(u16::MAX);
    Ok(JvmMethodSignature {
        name: method.name.clone(),
        descriptor: method.descriptor.clone(),
        access_flags: JVM_ACC_PUBLIC | JVM_ACC_STATIC,
        code: Some(JvmCodeBody { max_stack, max_locals: local_count, instructions: emitter.instructions }),
    })
}

impl JvmMethodEmitter<'_> {
    fn emit_block(&mut self, block: &ValkyrieMirBlock) -> Result<(), String> {
        self.current_block = block.id;
        self.instructions.push(JvmInstruction::Label(block_label(block.id)));
        for instruction in &block.instructions {
            self.emit_instruction(instruction)?;
        }
        self.emit_terminator(block)
    }

    fn emit_instruction(&mut self, instruction: &ValkyrieMirInstruction) -> Result<(), String> {
        if instruction.results.len() > 1 {
            return Err(format!(
                "JVM 最小 lowering 不支持多结果指令（results={}）: function={}",
                instruction.results.len(),
                self.view.symbol
            ));
        }
        match &instruction.operation {
            ValkyrieMirOperation::LoadConstant { constant } => {
                let (slot, kind) = self.require_output(instruction)?;
                self.emit_constant(constant, Some(kind))?;
                self.instructions.push(kind.store(slot));
            }
            ValkyrieMirOperation::Copy { source } => {
                let (slot, kind) = self.require_output(instruction)?;
                self.emit_operand(&ValkyrieMirOperand::Value(*source), Some(kind))?;
                self.instructions.push(kind.store(slot));
            }
            ValkyrieMirOperation::Cast { source, ty } => {
                // 语言层 `as` 是表示转换：源/目标位宽与符号性只由冻结的 `SemanticType`
                // 决定，不得退化成 Copy（`i2l`/`l2i` 与窄化规范化必须显式发射）。
                let (slot, target_kind) = self.require_output(instruction)?;
                let operand = ValkyrieMirOperand::Value(*source);
                let source_ty = self.operand_semantic_type(&operand)?;
                let target_ty = self
                    .ctx
                    .lower_type(*ty)
                    .map_err(|reason| format!("JVM 最小 lowering：`Cast` 目标类型缺少冻结语义: {reason}: function={}", self.view.symbol))?;
                let source_kind = self.value_slot(*source)?.1;
                self.emit_operand(&operand, Some(source_kind))?;
                self.emit_cast_conversion(&source_ty, &target_ty, source_kind, target_kind)?;
                self.instructions.push(target_kind.store(slot));
            }
            ValkyrieMirOperation::StoreVar { value, .. } => {
                let output = self.output_of(instruction)?;
                let operand = ValkyrieMirOperand::Value(*value);
                let expected = match &output {
                    Some((_, kind)) => Some(*kind),
                    None => Some(self.value_slot(*value)?.1),
                };
                self.emit_operand(&operand, expected)?;
                match output {
                    Some((slot, kind)) => self.instructions.push(kind.store(slot)),
                    None => self.emit_pop(expected)?,
                }
            }
            ValkyrieMirOperation::Invoke { callee, arguments } => {
                let output = self.output_of(instruction)?;
                self.emit_call(callee, arguments, &output)?;
            }
            ValkyrieMirOperation::StructNew { nominal, fields } => {
                let layout = self.layout_by_nominal(*nominal)?;
                let output = self.require_output(instruction)?;
                self.emit_layout_new(&layout, output)?;
                for (field, value) in fields {
                    let operand = ValkyrieMirOperand::Value(*value);
                    let Some((layout_id, slot)) = self.ctx.aggregate_layout_by_field().get(field).copied()
                    else {
                        return Err(format!("JVM 最小 lowering：字段身份 `{field:?}` 未登记布局索引: function={}", self.view.symbol));
                    };
                    if layout_id != layout.id {
                        return Err(format!(
                            "JVM 最小 lowering：字段 `{field:?}` 属于布局身份 `{layout_id}`，与构造的 nominal `{nominal:?}` 布局 `{}` 不符: function={}",
                            layout.id, self.view.symbol
                        ));
                    }
                    self.emit_layout_field_write(&layout, output.0, slot as usize, &operand)?;
                }
            }
            ValkyrieMirOperation::TupleGet { object, tuple_type, index } => {
                // 公共路径的 layout_id 由 tuple 类型身份冻结：位置越界先按 canonical
                // 元素合同拒绝，再按冻结布局的字段槽读取（标签只在类型身份层）。
                let layout_id = self.frozen_tuple_layout(*tuple_type)?;
                let layout = self.layout_by_id(layout_id)?;
                if (*index as usize) >= layout.fields.len() {
                    return Err(format!(
                        "JVM 最小 lowering：TupleGet 位置索引 {index} 越界（layout `{}` 长度 {}）: function={}",
                        layout.name,
                        layout.fields.len(),
                        self.view.symbol
                    ));
                }
                let (descriptor, kind) = self.layout_field_descriptor(&layout, *index)?;
                let field_ref = jvm_layout_field_ref(&layout, *index as usize, descriptor);
                self.emit_operand(&ValkyrieMirOperand::Value(*object), Some(JvmSlotKind::Reference))?;
                self.instructions.push(JvmInstruction::GetField(field_ref));
                match self.output_of(instruction)? {
                    Some((output, output_kind)) => {
                        if output_kind != kind {
                            return Err(format!(
                                "JVM 最小 lowering：TupleGet 位置 {index} 载体 {kind:?} 与结果槽 {output_kind:?} 不符: function={}",
                                self.view.symbol
                            ));
                        }
                        self.instructions.push(kind.store(output));
                    }
                    None => match kind {
                        JvmSlotKind::Long | JvmSlotKind::Double => {
                            return Err(format!(
                                "JVM 最小 lowering：丢弃 2 槽 tuple 元素需要 `pop2` 编码，当前 JVM 指令模型不提供: function={}",
                                self.view.symbol
                            ));
                        }
                        _ => self.instructions.push(JvmInstruction::Pop),
                    },
                }
            }
            ValkyrieMirOperation::TupleNew { element_types, fields } => {
                let result = SlotPlan::instruction_output(instruction)
                    .ok_or_else(|| "JVM 最小 lowering：TupleNew 缺少结果 SSA 值: function={}".to_string() + &self.view.symbol)?;
                let result_type = self
                    .function
                    .value_types
                    .get(&result)
                    .copied()
                    .ok_or_else(|| format!("JVM 最小 lowering：聚合值缺少 Semantic MIR 类型事实：`{result:?}`: function={}", self.view.symbol))?;
                if !matches!(
                    self.ctx.linked().types.get(&result_type).map(|row| &row.kind),
                    Some(compiler_types::CanonicalTypeKind::Tuple(_)) | Some(compiler_types::CanonicalTypeKind::LabeledTuple { .. })
                ) {
                    return Err(format!("JVM 最小 lowering：TupleNew 结果不是 tuple 类型：`{result_type:?}`: function={}", self.view.symbol));
                }
                if fields.len() != element_types.len() {
                    return Err(format!(
                        "JVM 最小 lowering：TupleNew 元素合同长度不一致：contract={} fields={}: function={}",
                        element_types.len(),
                        fields.len(),
                        self.view.symbol
                    ));
                }
                let layout_id = self.frozen_tuple_layout(result_type)?;
                let layout = self.layout_by_id(layout_id)?;
                if fields.len() != layout.fields.len() {
                    return Err(format!(
                        "JVM 最小 lowering：tuple 布局 `{}` 字段数量与操作数不符: function={} expected={} actual={}",
                        layout.id,
                        self.view.symbol,
                        layout.fields.len(),
                        fields.len()
                    ));
                }
                let output = self.require_output(instruction)?;
                self.emit_layout_new(&layout, output)?;
                for (slot, value) in fields.iter().enumerate() {
                    let operand = ValkyrieMirOperand::Value(*value);
                    self.emit_layout_field_write(&layout, output.0, slot, &operand)?;
                }
            }
            ValkyrieMirOperation::AggregateCopy { source, destination } => {
                // 公共路径按源 SSA 类型冻结布局身份；源/目标类型不一致在旧计划构造
                // 阶段即失败，这里逐操作保持同一拒绝合同（TypeId 粒度逐字对齐）。
                let source_type = self
                    .function
                    .value_types
                    .get(source)
                    .copied()
                    .ok_or_else(|| format!("聚合值 `{source:?}` 缺少 Semantic MIR 类型事实: function={}", self.view.symbol))?;
                let destination_type = self
                    .function
                    .value_types
                    .get(destination)
                    .copied()
                    .ok_or_else(|| format!("聚合值 `{destination:?}` 缺少 Semantic MIR 类型事实: function={}", self.view.symbol))?;
                if source_type != destination_type {
                    return Err(format!(
                        "AggregateCopy 源与目标类型不一致: {source_type:?} != {destination_type:?}: function={}",
                        self.view.symbol
                    ));
                }
                let layout_id = self
                    .ctx
                    .aggregate_layout_by_type()
                    .get(&source_type)
                    .copied()
                    .ok_or_else(|| format!("AggregateCopy 缺少编译器绑定的布局身份: {source_type:?}: function={}", self.view.symbol))?;
                let layout = self.layout_by_id(layout_id)?;
                let source_operand = ValkyrieMirOperand::Value(*source);
                let destination_operand = ValkyrieMirOperand::Value(*destination);
                if layout.fields.is_empty() {
                    self.emit_operand(&source_operand, Some(JvmSlotKind::Reference))?;
                    self.store_operand(&destination_operand, JvmSlotKind::Reference)?;
                }
                else {
                    let dest_slot = self.reference_operand_slot(&destination_operand)?;
                    let source_slot = self.reference_operand_slot(&source_operand)?;
                    self.emit_value_aggregate_copy(&layout, source_slot, dest_slot, 0)?;
                }
            }
            ValkyrieMirOperation::FieldGet { object, field } => {
                let (layout, slot) = self.field_location(*field)?;
                let (descriptor, kind) = self.layout_field_descriptor(&layout, slot)?;
                let field_ref = jvm_layout_field_ref(&layout, slot as usize, descriptor);
                self.emit_operand(&ValkyrieMirOperand::Value(*object), Some(JvmSlotKind::Reference))?;
                self.instructions.push(JvmInstruction::GetField(field_ref));
                match self.output_of(instruction)? {
                    Some((output, output_kind)) => {
                        if output_kind != kind {
                            return Err(format!(
                                "JVM 最小 lowering：字段 `{field:?}` 载体 {kind:?} 与结果槽 {output_kind:?} 不符: function={}",
                                self.view.symbol
                            ));
                        }
                        self.instructions.push(kind.store(output));
                    }
                    None => match kind {
                        JvmSlotKind::Long | JvmSlotKind::Double => {
                            return Err(format!(
                                "JVM 最小 lowering：丢弃 2 槽字段需要 `pop2` 编码，当前 JVM 指令模型不提供: function={}",
                                self.view.symbol
                            ));
                        }
                        _ => self.instructions.push(JvmInstruction::Pop),
                    },
                }
            }
            ValkyrieMirOperation::FieldSet { object, field, value } => {
                let (layout, slot) = self.field_location(*field)?;
                let (descriptor, kind) = self.layout_field_descriptor(&layout, slot)?;
                let field_ref = jvm_layout_field_ref(&layout, slot as usize, descriptor);
                self.emit_operand(&ValkyrieMirOperand::Value(*object), Some(JvmSlotKind::Reference))?;
                self.emit_operand(&ValkyrieMirOperand::Value(*value), Some(kind))?;
                self.instructions.push(JvmInstruction::PutField(field_ref));
            }
            ValkyrieMirOperation::SumNew { nominal, variant, payload } => {
                let payload = payload.map(ValkyrieMirOperand::Value);
                self.emit_sum_new(instruction, *nominal, *variant, payload.as_ref())?;
            }
            ValkyrieMirOperation::SumPayloadGet { object, nominal, variant } => {
                self.emit_sum_payload_get(instruction, *nominal, *variant, &ValkyrieMirOperand::Value(*object))?;
            }
            ValkyrieMirOperation::SumVariantIs { object, nominal, variant } => {
                self.emit_sum_variant_is(instruction, *nominal, *variant, &ValkyrieMirOperand::Value(*object))?;
            }
            ValkyrieMirOperation::ArrayFromElements { array_type, elements } => {
                let array_type = self
                    .ctx
                    .lower_type(*array_type)
                    .map_err(|reason| format!("JVM 最小 lowering：ArrayFromElements 数组类型缺少目标合同: {reason}: function={}", self.view.symbol))?;
                let operands = elements.iter().copied().map(ValkyrieMirOperand::Value).collect::<Vec<_>>();
                self.emit_array_from_elements(instruction, &array_type, &operands)?;
            }
            ValkyrieMirOperation::ArrayNew { array_type, length, initialization } => {
                let array_type = self
                    .ctx
                    .lower_type(*array_type)
                    .map_err(|reason| format!("JVM 最小 lowering：ArrayNew 数组类型缺少目标合同: {reason}: function={}", self.view.symbol))?;
                match initialization {
                    ValkyrieMirArrayInitialization::Default => self.emit_array_new(instruction, &array_type, &ValkyrieMirOperand::Value(*length))?,
                    ValkyrieMirArrayInitialization::Fill(_) => {
                        return Err("JVM 最小 lowering 不支持数组 Fill 初始化".to_string());
                    }
                }
            }
            ValkyrieMirOperation::ArrayGet { array, index } => {
                self.emit_array_get(instruction, &ValkyrieMirOperand::Value(*array), &ValkyrieMirOperand::Value(*index))?;
            }
            ValkyrieMirOperation::ArraySet { array, index, value } => {
                self.emit_array_set(
                    &ValkyrieMirOperand::Value(*array),
                    &ValkyrieMirOperand::Value(*index),
                    &ValkyrieMirOperand::Value(*value),
                )?;
            }
            ValkyrieMirOperation::ArrayLength { array } => {
                self.emit_operand(&ValkyrieMirOperand::Value(*array), Some(JvmSlotKind::Reference))?;
                self.instructions.push(JvmInstruction::ArrayLength);
                match self.output_of(instruction)? {
                    Some((slot, kind)) => {
                        if kind != JvmSlotKind::Int {
                            return Err(format!(
                                "JVM 最小 lowering：`ArrayLength` 结果必须是 `int` 槽: function={} actual={kind:?}",
                                self.view.symbol
                            ));
                        }
                        self.instructions.push(JvmInstruction::IStore(slot));
                    }
                    None => self.instructions.push(JvmInstruction::Pop),
                }
            }
            ValkyrieMirOperation::NullIsNull { value } => {
                self.emit_operand(&ValkyrieMirOperand::Value(*value), Some(JvmSlotKind::Reference))?;
                let true_label = self.fresh_comparison_label("null_true");
                let end_label = self.fresh_comparison_label("null_end");
                self.instructions.push(JvmInstruction::IfNull(true_label.clone()));
                self.instructions.push(JvmInstruction::IConst(0));
                self.instructions.push(JvmInstruction::Goto(end_label.clone()));
                self.instructions.push(JvmInstruction::Label(true_label));
                self.instructions.push(JvmInstruction::IConst(1));
                self.instructions.push(JvmInstruction::Label(end_label));
                self.store_or_pop_boolean(instruction)?;
            }
            ValkyrieMirOperation::NullUnwrap { value } => {
                let output = self.output_of(instruction)?;
                self.emit_operand(&ValkyrieMirOperand::Value(*value), Some(JvmSlotKind::Reference))?;
                match output {
                    Some((slot, kind)) => {
                        if kind != JvmSlotKind::Reference {
                            return Err("JVM 最小 lowering：可空标量需要 sum 表示，不能按引用直接解包".to_string());
                        }
                        self.instructions.push(JvmInstruction::AStore(slot));
                    }
                    None => self.instructions.push(JvmInstruction::Pop),
                }
            }
            // 闭包捕获环境在 JVM 没有载体：必须点名 lambda 实例与捕获名结构化失败，
            // 不得丢弃环境、把捕获提升为模块全局或用直调替代。
            ValkyrieMirOperation::LoadFunctionRef { instance, environment: Some(_), captured } => {
                return Err(format!(
                    "JVM 目标没有闭包载体：lambda instance `{instance:?}` 捕获 [{}]；function={}",
                    captured.join(", "),
                    self.view.symbol
                ));
            }
            other => {
                return Err(format!("JVM 最小 lowering 尚不支持指令 `{}`：function={}", operation_name(other), self.view.symbol));
            }
        }
        Ok(())
    }

    fn emit_terminator(&mut self, block: &ValkyrieMirBlock) -> Result<(), String> {
        match &block.terminator {
            ValkyrieMirTerminator::Return { value } => {
                let return_kind =
                    if self.return_descriptor == JvmTypeDescriptor::Void { None } else { Some(JvmSlotKind::of(&self.return_descriptor)?) };
                match (value, return_kind) {
                    (Some(value), Some(kind)) => {
                        self.emit_operand(&ValkyrieMirOperand::Value(*value), Some(kind))?;
                        self.instructions.push(JvmMethodDescriptor::new(Vec::new(), self.return_descriptor.clone()).return_instruction());
                    }
                    (None, None) => self.instructions.push(JvmInstruction::Return),
                    (Some(_), None) => {
                        return Err(format!("JVM 最小 lowering：函数 `{}` 返回了值但签名是 void", self.view.symbol));
                    }
                    (None, Some(_)) => {
                        return Err(format!("JVM 最小 lowering：非 void 函数 `{}` 缺少返回值", self.view.symbol));
                    }
                }
            }
            ValkyrieMirTerminator::Jump { target, arguments } => {
                let target_label = block_label(*target);
                let (parameter_count, kinds) = {
                    let target_block = self
                        .function
                        .blocks
                        .get(target)
                        .ok_or_else(|| format!("JVM 最小 lowering：函数 `{}` 跳转目标块 {target:?} 不存在", self.view.symbol))?;
                    let kinds = target_block
                        .parameters
                        .iter()
                        .map(|(parameter, _)| {
                            self.view
                                .value_types
                                .get(parameter)
                                .cloned()
                                .ok_or_else(|| format!("JVM 最小 lowering：块参数缺少冻结类型: function={}", self.view.symbol))
                                .and_then(|ty| jvm_local_kind(self.ctx.aggregate_layouts(), &ty))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    (target_block.parameters.len(), kinds)
                };
                if arguments.len() != parameter_count {
                    return Err(format!(
                        "JVM 最小 lowering：跳转目标块 {target:?} 形参数量与实参不符: function={} expected={} actual={}",
                        self.view.symbol,
                        parameter_count,
                        arguments.len()
                    ));
                }
                for (argument, kind) in arguments.iter().zip(&kinds) {
                    self.emit_operand(&ValkyrieMirOperand::Value(*argument), Some(*kind))?;
                }
                for index in (0..arguments.len()).rev() {
                    let local = *self
                        .slots
                        .block_param_locals
                        .get(&(*target, index))
                        .ok_or_else(|| format!("JVM 最小 lowering：跳转目标块 {target:?} 形参 {index} 缺少局部槽"))?;
                    self.instructions.push(kinds[index].store(self.slot_offsets[local as usize]));
                }
                self.instructions.push(JvmInstruction::Goto(target_label));
            }
            ValkyrieMirTerminator::Branch { condition, then_target, else_target } => {
                let targets_have_parameters = self
                    .function
                    .blocks
                    .values()
                    .filter(|block| block.id == *then_target || block.id == *else_target)
                    .any(|block| !block.parameters.is_empty());
                if targets_have_parameters {
                    return Err(format!("JVM 最小 lowering 只支持无块参数的条件分支：function={}", self.view.symbol));
                }
                self.emit_operand(&ValkyrieMirOperand::Value(*condition), Some(JvmSlotKind::Int))?;
                self.instructions.push(JvmInstruction::IfEq(block_label(*else_target)));
                self.instructions.push(JvmInstruction::Goto(block_label(*then_target)));
            }
            ValkyrieMirTerminator::Unreachable => {
                if self.return_descriptor == JvmTypeDescriptor::Void {
                    self.instructions.push(JvmInstruction::Return);
                }
                else {
                    return Err(format!("JVM 最小 lowering：非 void 函数 `{}` 的 `Unreachable` 终结缺少返回指令", self.view.symbol));
                }
            }
            other => {
                return Err(format!("JVM 最小 lowering 尚不支持终结符 `{other:?}`: function={}", self.view.symbol));
            }
        }
        Ok(())
    }

    fn emit_call(&mut self, callee: &ValkyrieMirCallee, arguments: &[MirValueId], output: &Option<(u16, JvmSlotKind)>) -> Result<(), String> {
        let instance = match callee {
            ValkyrieMirCallee::Item(instance) => *instance,
            ValkyrieMirCallee::Value(_) | ValkyrieMirCallee::Witness(_) => {
                return Err(format!("JVM 最小 lowering 只支持 `Item` 身份的静态调用: function={} callee={callee:?}", self.view.symbol));
            }
        };
        let operands = arguments.iter().copied().map(ValkyrieMirOperand::Value).collect::<Vec<_>>();
        if let Some((mnemonic, binding)) = self.operator_by_instance.get(&instance) {
            return self.emit_operator_call(mnemonic, binding, &operands, *output);
        }
        if let Some(import) = self.ctx.imports().get(&instance) {
            let links = import.links.iter().map(|link| link.to_string()).collect::<Vec<_>>().join(", ");
            return Err(format!(
                "JVM 最小 lowering 尚不支持宿主导入 `{instance:?}`（链接 `{links}`）：缺少 JVM host 编码，function={}",
                self.view.symbol
            ));
        }
        let Some(method) = self.method_plans.get(&instance).cloned()
        else {
            return Err(format!("JVM 最小 lowering：同片段调用目标 `{instance:?}` 不在可达方法集中: function={}", self.view.symbol));
        };
        let parameter_kinds = method.descriptor.parameter_types.iter().map(JvmSlotKind::of).collect::<Result<Vec<_>, _>>()?;
        if arguments.len() != parameter_kinds.len() {
            return Err(format!(
                "JVM 最小 lowering：调用 `{}` 实参数量与冻结签名不符: function={} expected={} actual={}",
                method.name,
                self.view.symbol,
                parameter_kinds.len(),
                arguments.len()
            ));
        }
        for (argument, kind) in operands.iter().zip(&parameter_kinds) {
            self.emit_operand(argument, Some(*kind))?;
        }
        self.instructions.push(JvmInstruction::InvokeStatic(JvmMethodRef {
            owner: self.internal_name.to_string(),
            name: method.name.clone(),
            descriptor: method.descriptor.clone(),
        }));
        self.store_call_result(instance, &method, *output)
    }

    /// `[jvm_operator("...")]` 绑定：只机械展开计划冻结的配方。
    fn emit_operator_call(
        &mut self,
        mnemonic: &str,
        binding: &JvmOperatorBinding,
        arguments: &[ValkyrieMirOperand],
        output: Option<(u16, JvmSlotKind)>,
    ) -> Result<(), String> {
        let JvmOperatorBinding::Frozen(plan) = binding
        else {
            return Err(format!("JVM 目标 `jvm_operator` 绑定缺少可编码助记符 `{mnemonic}`: function={}", self.view.symbol));
        };
        let code = plan.encoding;
        if arguments.len() != plan.arity {
            return Err(format!(
                "JVM 目标 operator `{mnemonic}` 实参数量与冻结 arity 不符: function={} expected={} actual={}",
                self.view.symbol,
                plan.arity,
                arguments.len()
            ));
        }
        let Some((slot, result_kind)) = output
        else {
            return Err(format!(
                "JVM 目标 operator `{mnemonic}` 调用必须绑定结果槽：缺少 `pop2` 编码，拒绝静默丢弃结果: function={}",
                self.view.symbol
            ));
        };
        let expected_kind = JvmSlotKind::of_operand_type(plan.operand_type);
        if result_kind != JvmSlotKind::of_operand_type(plan.result_type) {
            return Err(format!(
                "JVM 目标 operator `{mnemonic}` 结果槽载体与冻结 result_type 不符: function={} expected={:?} actual={result_kind:?}",
                self.view.symbol, plan.result_type
            ));
        }
        self.emit_operator_operands(mnemonic, expected_kind, plan, arguments)?;

        // 无符号 `u64` 比较的展开要消费方法级 scratch `long` 槽；槽在 `lower_function`
        // 按同一绑定表预留，未预留说明计划与发射扫描不一致，必须失败闭合。
        let scratch_long_slot = if code.requires_scratch_long_slot() {
            self.scratch_long_slot.ok_or_else(|| {
                format!("JVM 目标 operator `{mnemonic}` 的展开需要 scratch long 槽，但方法级预留缺失: function={}", self.view.symbol)
            })?
        }
        else {
            0
        };
        let expansion = code.instructions(scratch_long_slot);
        if expansion.is_empty() {
            return Err(format!("JVM 目标 operator `{mnemonic}` 的冻结编码 `{code:?}` 没有指令展开: function={}", self.view.symbol));
        }
        let label_prefix = self.operator_label_prefix();
        self.comparison_ordinal += 1;
        for instruction in expansion {
            self.instructions.push(relabel_operator_branch(instruction, &label_prefix));
        }
        self.instructions.push(result_kind.store(slot));
        Ok(())
    }

    fn emit_operator_operands(
        &mut self,
        mnemonic: &str,
        expected_kind: JvmSlotKind,
        plan: &TargetOperatorPlan<JvmOperatorCode>,
        arguments: &[ValkyrieMirOperand],
    ) -> Result<(), String> {
        match plan.recipe {
            TargetOperatorRecipe::Operands => self.emit_operator_arguments(mnemonic, expected_kind, arguments),
            TargetOperatorRecipe::Steps(steps) => {
                for step in steps {
                    match step {
                        TargetOperandStep::Arguments => self.emit_operator_arguments(mnemonic, expected_kind, arguments)?,
                        TargetOperandStep::IntegerConstant { operand_type, value } => {
                            let kind = JvmSlotKind::of_operand_type(*operand_type);
                            if kind != expected_kind {
                                return Err(format!(
                                    "JVM 目标 operator `{mnemonic}` 的常量配方载体与冻结操作数载体不符: function={} expected={expected_kind:?} actual={kind:?}",
                                    self.view.symbol
                                ));
                            }
                            match kind {
                                JvmSlotKind::Int => self.instructions.push(JvmInstruction::IConst(*value)),
                                JvmSlotKind::Long => self.instructions.push(JvmInstruction::LdcLong(i64::from(*value))),
                                JvmSlotKind::Float | JvmSlotKind::Double | JvmSlotKind::Reference => {
                                    return Err(format!(
                                        "JVM 目标 operator `{mnemonic}` 冻结了非整数常量配方 {operand_type:?}: function={}",
                                        self.view.symbol
                                    ));
                                }
                            }
                        }
                    }
                }
                Ok(())
            }
        }
    }

    fn emit_operator_arguments(&mut self, mnemonic: &str, expected_kind: JvmSlotKind, arguments: &[ValkyrieMirOperand]) -> Result<(), String> {
        for argument in arguments {
            self.emit_operand(argument, Some(expected_kind)).map_err(|error| {
                format!("JVM 目标 operator `{mnemonic}` 实参载体与冻结 operand_type 不符: function={} detail={error}", self.view.symbol)
            })?;
        }
        Ok(())
    }

    fn store_call_result(&mut self, instance: ItemInstanceId, method: &JvmMethodPlan, output: Option<(u16, JvmSlotKind)>) -> Result<(), String> {
        if method.descriptor.return_type == JvmTypeDescriptor::Void {
            if output.is_some() {
                return Err(format!(
                    "JVM 最小 lowering：void 调用 `{}` 不得绑定结果槽: function={} callee={instance:?}",
                    method.name, self.view.symbol
                ));
            }
            return Ok(());
        }
        let return_kind = JvmSlotKind::of(&method.descriptor.return_type)?;
        match output {
            Some((slot, kind)) => {
                if kind != return_kind {
                    return Err(format!(
                        "JVM 最小 lowering：调用 `{}` 结果载体与冻结返回签名不符: function={} expected={return_kind:?} actual={kind:?}",
                        method.name, self.view.symbol
                    ));
                }
                self.instructions.push(kind.store(slot));
                Ok(())
            }
            None => match return_kind {
                JvmSlotKind::Int | JvmSlotKind::Reference | JvmSlotKind::Float => {
                    self.instructions.push(JvmInstruction::Pop);
                    Ok(())
                }
                JvmSlotKind::Long | JvmSlotKind::Double => Err(format!(
                    "JVM 最小 lowering：调用 `{}` 返回 2 槽值但缺少 `pop2` 编码，拒绝静默丢弃: function={}",
                    method.name, self.view.symbol
                )),
            },
        }
    }

    fn emit_operand(&mut self, operand: &ValkyrieMirOperand, expected: Option<JvmSlotKind>) -> Result<(), String> {
        match operand {
            ValkyrieMirOperand::Value(value) => {
                let (slot, kind) = self.value_slot(*value)?;
                if let Some(expected) = expected
                    && expected != kind
                {
                    return Err(format!(
                        "JVM 最小 lowering：SSA 值 {:?} 的槽载体 {kind:?} 与期望 {expected:?} 不符: function={}",
                        value, self.view.symbol
                    ));
                }
                self.instructions.push(kind.load(slot));
                Ok(())
            }
            ValkyrieMirOperand::Constant(constant) => self.emit_constant(constant, expected),
        }
    }

    fn emit_constant(&mut self, constant: &ValkyrieMirConstant, expected: Option<JvmSlotKind>) -> Result<(), String> {
        match constant {
            ValkyrieMirConstant::Int(value) => match expected {
                Some(JvmSlotKind::Long) => {
                    self.instructions.push(JvmInstruction::LdcLong(*value));
                    Ok(())
                }
                Some(JvmSlotKind::Int) | None => {
                    let value = i32::try_from(*value).map_err(|_| {
                        format!(
                            "JVM 最小 lowering：整数常量 {value} 超出 `int` 载体，且目标槽位未声明 `long`: function={}",
                            self.view.symbol
                        )
                    })?;
                    self.instructions.push(JvmInstruction::IConst(value));
                    Ok(())
                }
                Some(other) => Err(format!("JVM 最小 lowering：整数常量不能进入 {other:?} 槽: function={}", self.view.symbol)),
            },
            ValkyrieMirConstant::Float64(bits) => match expected {
                Some(JvmSlotKind::Float) => {
                    Err(format!("JVM 最小 lowering 不支持把 f64 常量压入 `float` 槽：function={}", self.view.symbol))
                }
                Some(JvmSlotKind::Int | JvmSlotKind::Long | JvmSlotKind::Reference) => {
                    Err(format!("JVM 最小 lowering：f64 常量不能进入 {expected:?} 槽: function={}", self.view.symbol))
                }
                Some(JvmSlotKind::Double) => {
                    self.instructions.push(JvmInstruction::LdcDouble(*bits));
                    Ok(())
                }
                None => Err(format!("JVM 最小 lowering：f64 常量缺少明确的 `double` 结果槽，拒绝猜测载体: function={}", self.view.symbol)),
            },
            ValkyrieMirConstant::Bool(value) => {
                if matches!(expected, Some(JvmSlotKind::Long | JvmSlotKind::Float | JvmSlotKind::Double | JvmSlotKind::Reference)) {
                    return Err(format!("JVM 最小 lowering：布尔常量不能进入 {expected:?} 槽: function={}", self.view.symbol));
                }
                self.instructions.push(JvmInstruction::IConst(i32::from(*value)));
                Ok(())
            }
            // `T?` 的空值就是 JVM `null` 引用；只有引用槽能承载。
            // 可空标量在类型合同已拒绝（需要 sum 表示），这里不为其静默装箱。
            ValkyrieMirConstant::Null => match expected {
                None | Some(JvmSlotKind::Reference) => {
                    self.instructions.push(JvmInstruction::AConstNull);
                    Ok(())
                }
                Some(other) => Err(format!(
                    "JVM 最小 lowering：空值常量不能进入 {other:?} 槽（可空标量需要 sum 表示，不得静默装箱）: function={}",
                    self.view.symbol
                )),
            },
            other => Err(format!("JVM 最小 lowering 尚不支持常量 `{other:?}`：缺少 JVM 常量池载体合同，function={}", self.view.symbol)),
        }
    }

    fn emit_pop(&mut self, kind: Option<JvmSlotKind>) -> Result<(), String> {
        match kind {
            None | Some(JvmSlotKind::Int | JvmSlotKind::Reference | JvmSlotKind::Float) => {
                self.instructions.push(JvmInstruction::Pop);
                Ok(())
            }
            Some(JvmSlotKind::Long | JvmSlotKind::Double) => {
                Err(format!("JVM 最小 lowering：丢弃 2 槽值需要 `pop2` 编码，当前 JVM 指令模型不提供: function={}", self.view.symbol))
            }
        }
    }

    /// 操作数在计划中冻结的语义类型（`Cast` 的转换语义必须由此决定）。
    fn operand_semantic_type(&self, operand: &ValkyrieMirOperand) -> Result<SemanticType, String> {
        match operand {
            ValkyrieMirOperand::Value(value) => self
                .view
                .value_types
                .get(value)
                .cloned()
                .ok_or_else(|| format!("JVM `Cast` 源 SSA 值 {value:?} 缺少冻结语义类型: function={}", self.view.symbol)),
            ValkyrieMirOperand::Constant(constant) => match constant {
                ValkyrieMirConstant::Int(_) => Ok(SemanticType::Integer64 { signed: true }),
                ValkyrieMirConstant::Bool(_) => Ok(SemanticType::Boolean),
                ValkyrieMirConstant::Float64(_) => Ok(SemanticType::Float64),
                ValkyrieMirConstant::Utf8(_) => Ok(SemanticType::Utf8),
                ValkyrieMirConstant::Utf16(_) => Ok(SemanticType::Utf16),
                ValkyrieMirConstant::Unit => Ok(SemanticType::Unit),
                ValkyrieMirConstant::Null => Err(format!("JVM `Cast` 的空值操作数没有源类型: function={}", self.view.symbol)),
            },
        }
    }

    /// 语言层 `as` 的整数表示转换（`int`/`long` 操作数栈）。
    fn emit_cast_conversion(
        &mut self,
        source: &SemanticType,
        target: &SemanticType,
        source_kind: JvmSlotKind,
        target_kind: JvmSlotKind,
    ) -> Result<(), String> {
        if source == target {
            return Ok(());
        }
        if source.is_float() || target.is_float() {
            return self.emit_float_cast_conversion(source, target, source_kind, target_kind);
        }
        if source_kind == JvmSlotKind::Reference || target_kind == JvmSlotKind::Reference {
            return Err(format!("JVM 最小 lowering：`{source}` → `{target}` 的引用 cast 缺少目标合同: function={}", self.view.symbol));
        }
        let (Some(source_shape), Some(target_shape)) = (jvm_int_shape(source), jvm_int_shape(target))
        else {
            return Err(format!("JVM 最小 lowering：`{source}` → `{target}` 的 cast 缺少整数表示合同: function={}", self.view.symbol));
        };
        // 源在 int 载体上先按源位宽/符号性规范化（8/16 位可能带着越界高位）。
        if source_kind == JvmSlotKind::Int {
            self.emit_int_normalize(source_shape);
        }
        match (source_kind, target_kind) {
            (JvmSlotKind::Int, JvmSlotKind::Long) => {
                self.instructions.push(JvmInstruction::I2L);
                if !source_shape.signed && source_shape.bits == 32 {
                    // `i2l` 对 32 位做符号扩展；u32 必须再抹掉高 32 位。
                    self.instructions.push(JvmInstruction::LdcLong(0xFFFF_FFFF));
                    self.instructions.push(JvmInstruction::LAnd);
                }
            }
            (JvmSlotKind::Long, JvmSlotKind::Int) => {
                self.instructions.push(JvmInstruction::L2I);
            }
            _ => {}
        }
        if target_kind == JvmSlotKind::Int {
            self.emit_int_normalize(target_shape);
        }
        Ok(())
    }

    /// 浮点 `as` 转换的 lowering：只发射 JVM 原生、两侧宽度对应的单条转换指令。
    ///
    /// `float`/`double`/`long` 之间的舍入与饱和语义由 JVM 转换指令本身承诺
    /// （与语言 `as` 合同一致），backend 不发明舍入。`int` 系源先按源形状在
    /// `int` 载体上归一（8/16 位载体携带越界高位），整型结果再按目标形状归一。
    /// 非数值载体（bool/reference）保持失败闭合，不做隐式转换。
    fn emit_float_cast_conversion(
        &mut self,
        source: &SemanticType,
        target: &SemanticType,
        source_kind: JvmSlotKind,
        target_kind: JvmSlotKind,
    ) -> Result<(), String> {
        let step = match (source, target) {
            (SemanticType::Float32, SemanticType::Float64) => Some(JvmInstruction::F2D),
            (SemanticType::Float64, SemanticType::Float32) => Some(JvmInstruction::D2F),
            (SemanticType::Float32, SemanticType::Integer64 { .. }) => Some(JvmInstruction::F2L),
            (SemanticType::Float64, SemanticType::Integer64 { .. }) => Some(JvmInstruction::D2L),
            (SemanticType::Integer64 { .. }, SemanticType::Float32) => Some(JvmInstruction::L2F),
            (SemanticType::Integer64 { .. }, SemanticType::Float64) => Some(JvmInstruction::L2D),
            _ => None,
        };
        if let Some(instruction) = step {
            self.instructions.push(instruction);
            return Ok(());
        }
        if source.is_float() {
            // float → 32 位整型系：F2I/D2I 后按目标形状归一（i64 结果已在上面单步完成）。
            if !matches!(target_kind, JvmSlotKind::Int) {
                return Err(format!("JVM 最小 lowering：`{source}` → `{target}` 的浮点 cast 目标没有整型载体: function={}", self.view.symbol));
            }
            let Some(shape) = jvm_int_shape(target) else {
                return Err(format!("JVM 最小 lowering：`{source}` → `{target}` 的浮点 cast 目标没有可归一的整型形状: function={}", self.view.symbol));
            };
            self.instructions.push(if matches!(source, SemanticType::Float32) { JvmInstruction::F2I } else { JvmInstruction::D2I });
            self.emit_int_normalize(shape);
            return Ok(());
        }
        // 32 位整型系 → float：源归一后 I2F/I2D（i64 源已在上面单步完成）。
        if !matches!(source_kind, JvmSlotKind::Int) {
            return Err(format!("JVM 最小 lowering：`{source}` → `{target}` 的浮点 cast 源没有数值载体: function={}", self.view.symbol));
        }
        let Some(shape) = jvm_int_shape(source) else {
            return Err(format!("JVM 最小 lowering：`{source}` → `{target}` 的浮点 cast 源没有可归一的整型形状: function={}", self.view.symbol));
        };
        self.emit_int_normalize(shape);
        self.instructions.push(if matches!(target, SemanticType::Float32) { JvmInstruction::I2F } else { JvmInstruction::I2D });
        Ok(())
    }

    /// 把 `int` 载体上的值按位宽/符号性规范化（8/16 位用移位或掩码）。
    fn emit_int_normalize(&mut self, shape: JvmIntShape) {
        match (shape.bits, shape.signed) {
            (8, true) => {
                self.instructions.push(JvmInstruction::IConst(24));
                self.instructions.push(JvmInstruction::IShl);
                self.instructions.push(JvmInstruction::IConst(24));
                self.instructions.push(JvmInstruction::IShr);
            }
            (16, true) => {
                self.instructions.push(JvmInstruction::IConst(16));
                self.instructions.push(JvmInstruction::IShl);
                self.instructions.push(JvmInstruction::IConst(16));
                self.instructions.push(JvmInstruction::IShr);
            }
            (8, false) => {
                self.instructions.push(JvmInstruction::IConst(0xFF));
                self.instructions.push(JvmInstruction::IAnd);
            }
            (16, false) => {
                self.instructions.push(JvmInstruction::IConst(0xFFFF));
                self.instructions.push(JvmInstruction::IAnd);
            }
            _ => {}
        }
    }

    fn output_of(&self, instruction: &ValkyrieMirInstruction) -> Result<Option<(u16, JvmSlotKind)>, String> {
        match SlotPlan::instruction_output(instruction) {
            None => Ok(None),
            Some(value) => self.value_slot(value).map(Some),
        }
    }

    fn require_output(&self, instruction: &ValkyrieMirInstruction) -> Result<(u16, JvmSlotKind), String> {
        self.output_of(instruction)?
            .ok_or_else(|| format!("JVM 最小 lowering：指令缺少 SSA 结果槽，无法确定局部载体: function={}", self.view.symbol))
    }

    fn value_slot(&self, value: MirValueId) -> Result<(u16, JvmSlotKind), String> {
        let local = *self
            .slots
            .value_locals
            .get(&value)
            .ok_or_else(|| format!("JVM 最小 lowering：SSA 值 {value:?} 缺少规划的局部槽: function={}", self.view.symbol))?;
        let ty = self
            .slots
            .local_types
            .get(local as usize)
            .ok_or_else(|| format!("JVM 最小 lowering：局部槽 {local} 超出规划表: function={}", self.view.symbol))?;
        let kind = jvm_local_kind(self.ctx.aggregate_layouts(), ty)?;
        Ok((self.slot_offsets[local as usize], kind))
    }

    /// 比较展开标签前缀：当前块标签 + 方法内比较序号，保证方法内唯一。
    fn operator_label_prefix(&self) -> String {
        format!("{}_{}", block_label(self.current_block), self.comparison_ordinal)
    }

    /// 为布尔化展开分配方法内唯一标签。
    fn fresh_comparison_label(&mut self, suffix: &str) -> String {
        let label = format!("{}_{}_{}", block_label(self.current_block), self.comparison_ordinal, suffix);
        self.comparison_ordinal += 1;
        label
    }

    /// tuple 的冻结布局身份：只由 `TypeId → LayoutId` 计划索引结算，禁止按形状猜。
    fn frozen_tuple_layout(&self, type_id: TypeId) -> Result<LayoutId, String> {
        self.ctx
            .aggregate_layout_by_type()
            .get(&type_id)
            .copied()
            .ok_or_else(|| format!("JVM 最小 lowering：tuple 类型 `{type_id:?}` 缺少编译器绑定布局身份: function={}", self.view.symbol))
    }

    fn layout_by_id(&self, layout_id: LayoutId) -> Result<AggregateLayout, String> {
        self.ctx
            .aggregate_layouts()
            .layouts
            .iter()
            .find(|layout| layout.id == layout_id)
            .cloned()
            .ok_or_else(|| format!("JVM 最小 lowering：布局身份 `{layout_id}` 未在计划中注册: function={}", self.view.symbol))
    }

    fn layout_by_nominal(&self, nominal: NominalInstanceId) -> Result<AggregateLayout, String> {
        let Some(layout_id) = self.ctx.aggregate_layout_by_nominal().get(&nominal).copied()
        else {
            return Err(format!("JVM 最小 lowering：nominal `{nominal:?}` 没有冻结的布局身份: function={}", self.view.symbol));
        };
        self.layout_by_id(layout_id)
    }

    fn layout_field_descriptor(&self, layout: &AggregateLayout, slot: u32) -> Result<(JvmTypeDescriptor, JvmSlotKind), String> {
        let Some(field) = layout.fields.get(slot as usize)
        else {
            return Err(format!("JVM 最小 lowering：布局 `{}` 缺少字段槽 {slot}: function={}", layout.name, self.view.symbol));
        };
        let layouts = self.ctx.aggregate_layouts();
        let descriptor = jvm_type_descriptor(layouts, &field.ty)?;
        Ok((descriptor.clone(), JvmSlotKind::of(&descriptor)?))
    }

    fn field_location(&self, field: FieldId) -> Result<(AggregateLayout, u32), String> {
        let Some((layout_id, slot)) = self.ctx.aggregate_layout_by_field().get(&field).copied()
        else {
            return Err(format!("JVM 最小 lowering：字段身份 `{field:?}` 未登记布局索引: function={}", self.view.symbol));
        };
        Ok((self.layout_by_id(layout_id)?, slot))
    }

    fn operand_type(&self, operand: &ValkyrieMirOperand) -> Result<SemanticType, String> {
        match operand {
            ValkyrieMirOperand::Value(value) => self
                .view
                .value_types
                .get(value)
                .cloned()
                .ok_or_else(|| format!("JVM 最小 lowering：SSA 值 {value:?} 缺少冻结类型: function={}", self.view.symbol)),
            ValkyrieMirOperand::Constant(constant) => Ok(match constant {
                ValkyrieMirConstant::Int(_) => SemanticType::Integer32 { signed: true },
                ValkyrieMirConstant::Float64(_) => SemanticType::Float64,
                ValkyrieMirConstant::Bool(_) => SemanticType::Boolean,
                ValkyrieMirConstant::Utf8(_) | ValkyrieMirConstant::Utf16(_) => SemanticType::Utf8,
                other => {
                    return Err(format!("JVM 最小 lowering：常量 `{other:?}` 没有可装箱的冻结类型: function={}", self.view.symbol));
                }
            }),
        }
    }

    fn output_type(&self, instruction: &ValkyrieMirInstruction) -> Result<Option<SemanticType>, String> {
        let Some(value) = SlotPlan::instruction_output(instruction)
        else {
            return Ok(None);
        };
        self.view
            .value_types
            .get(&value)
            .cloned()
            .map(Some)
            .ok_or_else(|| format!("JVM 最小 lowering：SSA 值 {value:?} 缺少冻结类型: function={}", self.view.symbol))
    }

    fn reference_operand_slot(&self, operand: &ValkyrieMirOperand) -> Result<u16, String> {
        let ValkyrieMirOperand::Value(value) = operand
        else {
            return Err(format!("JVM 最小 lowering：只有 SSA 值可作为引用局部目标: function={} dest={operand:?}", self.view.symbol));
        };
        let (slot, kind) = self.value_slot(*value)?;
        if kind != JvmSlotKind::Reference {
            return Err(format!("JVM 最小 lowering：聚合拷贝目标必须是引用载体: function={} actual={kind:?}", self.view.symbol));
        }
        Ok(slot)
    }

    fn store_operand(&mut self, operand: &ValkyrieMirOperand, kind: JvmSlotKind) -> Result<(), String> {
        let ValkyrieMirOperand::Value(value) = operand
        else {
            return Err(format!("JVM 最小 lowering：只有 SSA 值可作为局部目标: function={} dest={operand:?}", self.view.symbol));
        };
        let (slot, actual) = self.value_slot(*value)?;
        if actual != kind {
            return Err(format!(
                "JVM 最小 lowering：目标 SSA 值 {value:?} 的槽载体 {actual:?} 与期望 {kind:?} 不符: function={}",
                self.view.symbol
            ));
        }
        self.instructions.push(kind.store(slot));
        Ok(())
    }

    /// 发射一个可能已处于标量槽的 payload/元素，并按目标类型按需装箱。
    fn emit_boxed_operand(&mut self, operand: &ValkyrieMirOperand, ty: &SemanticType) -> Result<(), String> {
        let descriptor = {
            let layouts = self.ctx.aggregate_layouts();
            jvm_type_descriptor(layouts, ty)?
        };
        match &descriptor {
            JvmTypeDescriptor::Object(_) | JvmTypeDescriptor::Array(_) => self.emit_operand(operand, Some(JvmSlotKind::Reference)),
            JvmTypeDescriptor::Void => {
                Err(format!("JVM 最小 lowering：`{ty}` 是 void 形态，不能作为需要引用的值: function={}", self.view.symbol))
            }
            scalar => {
                let kind = JvmSlotKind::of(scalar)?;
                self.emit_operand(operand, Some(kind))?;
                let boxing = jvm_box_value_of(scalar)?;
                self.instructions.push(JvmInstruction::InvokeStatic(boxing));
                Ok(())
            }
        }
    }

    fn emit_array_element_value(&mut self, operand: &ValkyrieMirOperand, element: &SemanticType) -> Result<(), String> {
        let (target, element_descriptor) = {
            let layouts = self.ctx.aggregate_layouts();
            (jvm_type_descriptor(layouts, element)?, jvm_array_element_descriptor(layouts, element)?)
        };
        let target_kind = JvmSlotKind::of(&target)?;
        if matches!(element_descriptor, JvmTypeDescriptor::Object(_) | JvmTypeDescriptor::Array(_)) && target_kind != JvmSlotKind::Reference {
            return self.emit_boxed_operand(operand, element);
        }
        self.emit_operand(operand, Some(target_kind))
    }

    fn emit_layout_new(&mut self, layout: &AggregateLayout, output: (u16, JvmSlotKind)) -> Result<(), String> {
        if output.1 != JvmSlotKind::Reference {
            return Err(format!(
                "JVM 最小 lowering：布局 `{}` 的构造结果必须是引用槽: function={} actual={:?}",
                layout.name, self.view.symbol, output.1
            ));
        }
        let class_name = jvm_layout_class_name(layout);
        self.instructions.push(JvmInstruction::New(class_name.clone()));
        self.instructions.push(JvmInstruction::Dup);
        self.instructions.push(JvmInstruction::InvokeSpecial(jvm_constructor_ref(&class_name)));
        self.instructions.push(JvmSlotKind::Reference.store(output.0));
        Ok(())
    }

    fn emit_layout_field_write(&mut self, layout: &AggregateLayout, output_slot: u16, slot: usize, value: &ValkyrieMirOperand) -> Result<(), String> {
        let (descriptor, kind) = self.layout_field_descriptor(layout, u32::try_from(slot).unwrap_or(u32::MAX))?;
        let field_ref = jvm_layout_field_ref(layout, slot, descriptor);
        self.instructions.push(JvmInstruction::ALoad(output_slot));
        self.emit_operand(value, Some(kind))?;
        self.instructions.push(JvmInstruction::PutField(field_ref));
        Ok(())
    }

    /// 递归复制值语义聚合：目标槽得到独立对象，嵌套值聚合逐层新建。
    ///
    /// 引用语义字段（`class`、数组、闭包、引用 sum 等）按共享分类直接搬运；
    /// 空嵌套字段保持 `null`。分类事实来自共享 `value_copy` 模块，不在本后端
    /// 按类型名或字段名特判。
    fn emit_value_aggregate_copy(&mut self, layout: &AggregateLayout, source_slot: u16, dest_slot: u16, depth: u32) -> Result<(), String> {
        if depth > MAX_VALUE_AGGREGATE_COPY_DEPTH {
            return Err(format!(
                "JVM 最小 lowering：值聚合复制嵌套 {depth} 层超过上限 {MAX_VALUE_AGGREGATE_COPY_DEPTH}: function={}",
                self.view.symbol
            ));
        }
        let class_name = jvm_layout_class_name(layout);
        self.instructions.push(JvmInstruction::New(class_name.clone()));
        self.instructions.push(JvmInstruction::Dup);
        self.instructions.push(JvmInstruction::InvokeSpecial(jvm_constructor_ref(&class_name)));
        self.instructions.push(JvmInstruction::AStore(dest_slot));
        for slot in 0..layout.fields.len() {
            let (descriptor, _) = self.layout_field_descriptor(layout, u32::try_from(slot).unwrap_or(u32::MAX))?;
            let field_ref = jvm_layout_field_ref(layout, slot, descriptor);
            let Some(nested_id) = nested_value_aggregate_layout(self.ctx.aggregate_layouts(), &layout.fields[slot].ty)
            else {
                // `putfield` 栈序是 `object, value`：先压目标对象，再从源对象读字段。
                self.instructions.push(JvmInstruction::ALoad(dest_slot));
                self.instructions.push(JvmInstruction::ALoad(source_slot));
                self.instructions.push(JvmInstruction::GetField(field_ref.clone()));
                self.instructions.push(JvmInstruction::PutField(field_ref));
                continue;
            };
            let nested_layout = self.layout_by_id(nested_id)?;
            let nested_source = self.copy_scratch_slot(depth, 0)?;
            let nested_dest = self.copy_scratch_slot(depth, 1)?;
            self.instructions.push(JvmInstruction::ALoad(dest_slot));
            self.instructions.push(JvmInstruction::ALoad(source_slot));
            self.instructions.push(JvmInstruction::GetField(field_ref.clone()));
            self.instructions.push(JvmInstruction::AStore(nested_source));
            let null_label = self.fresh_copy_label("null");
            let end_label = self.fresh_copy_label("end");
            self.instructions.push(JvmInstruction::ALoad(nested_source));
            self.instructions.push(JvmInstruction::IfNull(null_label.clone()));
            self.emit_value_aggregate_copy(&nested_layout, nested_source, nested_dest, depth + 1)?;
            self.instructions.push(JvmInstruction::ALoad(nested_dest));
            self.instructions.push(JvmInstruction::Goto(end_label.clone()));
            self.instructions.push(JvmInstruction::Label(null_label));
            self.instructions.push(JvmInstruction::AConstNull);
            self.instructions.push(JvmInstruction::Label(end_label));
            self.instructions.push(JvmInstruction::PutField(field_ref));
        }
        Ok(())
    }

    fn copy_scratch_slot(&self, depth: u32, offset: usize) -> Result<u16, String> {
        let index = (depth as usize).saturating_mul(2).saturating_add(offset);
        self.copy_scratch
            .get(index)
            .copied()
            .ok_or_else(|| format!("JVM 最小 lowering：值聚合复制临时槽 {index} 未预留: function={}", self.view.symbol))
    }

    fn fresh_copy_label(&mut self, suffix: &str) -> String {
        self.label_ordinal = self.label_ordinal.saturating_add(1);
        format!("copy_{suffix}_{}", self.label_ordinal)
    }

    fn emit_sum_new(
        &mut self,
        instruction: &ValkyrieMirInstruction,
        nominal: NominalInstanceId,
        variant: VariantId,
        payload: Option<&ValkyrieMirOperand>,
    ) -> Result<(), String> {
        let Some(sum) = self.ctx.sum_representations().get(&nominal).cloned()
        else {
            return Err(format!(
                "JVM 最小 lowering：sum nominal `{nominal:?}` 没有冻结的 `SumRepresentation`: function={}",
                self.view.symbol
            ));
        };
        let Some(variant_rep) = sum.variants.get(&variant).copied()
        else {
            return Err(format!(
                "JVM 最小 lowering：variant `{variant:?}` 不属于 sum nominal `{nominal:?}`: function={}",
                self.view.symbol
            ));
        };
        if payload.is_some() != variant_rep.payload_type.is_some() {
            return Err(format!(
                "JVM 最小 lowering：variant `{variant:?}` 的 payload 操作数与冻结 payload 类型不一致: function={} nominal={nominal:?} payload={} expected={}",
                self.view.symbol,
                payload.is_some(),
                variant_rep.payload_type.is_some()
            ));
        }
        let tag = i32::try_from(variant_rep.tag)
            .map_err(|_| format!("JVM 最小 lowering：variant `{variant:?}` 的 tag 超出 `int` 载体: function={}", self.view.symbol))?;
        let output = self.require_output(instruction)?;
        if output.1 != JvmSlotKind::Reference {
            return Err(format!("JVM 最小 lowering：sum 构造结果必须是引用槽: function={} actual={:?}", self.view.symbol, output.1));
        }
        let class_name = jvm_sum_class_name(nominal);
        self.instructions.push(JvmInstruction::New(class_name.clone()));
        self.instructions.push(JvmInstruction::Dup);
        self.instructions.push(JvmInstruction::InvokeSpecial(jvm_constructor_ref(&class_name)));
        self.instructions.push(JvmSlotKind::Reference.store(output.0));

        self.instructions.push(JvmInstruction::ALoad(output.0));
        self.instructions.push(JvmInstruction::IConst(tag));
        self.instructions.push(JvmInstruction::PutField(JvmFieldRef {
            owner: class_name.clone(),
            name: JVM_SUM_TAG_FIELD.to_string(),
            descriptor: JvmTypeDescriptor::Int,
        }));

        if let Some(payload) = payload {
            let payload_type = self.operand_type(payload)?;
            self.instructions.push(JvmInstruction::ALoad(output.0));
            self.emit_boxed_operand(payload, &payload_type)?;
            self.instructions.push(JvmInstruction::PutField(JvmFieldRef {
                owner: class_name,
                name: JVM_SUM_PAYLOAD_FIELD.to_string(),
                descriptor: JvmTypeDescriptor::Object(JVM_REFERENCE_CARRIER.to_string()),
            }));
        }
        Ok(())
    }

    fn emit_sum_variant_is(
        &mut self,
        instruction: &ValkyrieMirInstruction,
        nominal: NominalInstanceId,
        variant: VariantId,
        object: &ValkyrieMirOperand,
    ) -> Result<(), String> {
        let Some(sum) = self.ctx.sum_representations().get(&nominal).cloned()
        else {
            return Err(format!(
                "JVM 最小 lowering：sum nominal `{nominal:?}` 没有冻结的 `SumRepresentation`: function={}",
                self.view.symbol
            ));
        };
        let Some(variant_rep) = sum.variants.get(&variant).copied()
        else {
            return Err(format!(
                "JVM 最小 lowering：variant `{variant:?}` 不属于 sum nominal `{nominal:?}`: function={}",
                self.view.symbol
            ));
        };
        let tag = i32::try_from(variant_rep.tag)
            .map_err(|_| format!("JVM 最小 lowering：variant `{variant:?}` 的 tag 超出 `int` 载体: function={}", self.view.symbol))?;
        let class_name = jvm_sum_class_name(nominal);
        self.emit_operand(object, Some(JvmSlotKind::Reference))?;
        self.instructions.push(JvmInstruction::GetField(JvmFieldRef {
            owner: class_name,
            name: JVM_SUM_TAG_FIELD.to_string(),
            descriptor: JvmTypeDescriptor::Int,
        }));
        self.instructions.push(JvmInstruction::IConst(tag));
        let true_label = self.fresh_comparison_label("sum_true");
        let end_label = self.fresh_comparison_label("sum_end");
        self.instructions.push(JvmInstruction::IfICmpEq(true_label.clone()));
        self.instructions.push(JvmInstruction::IConst(0));
        self.instructions.push(JvmInstruction::Goto(end_label.clone()));
        self.instructions.push(JvmInstruction::Label(true_label));
        self.instructions.push(JvmInstruction::IConst(1));
        self.instructions.push(JvmInstruction::Label(end_label));
        self.store_or_pop_boolean(instruction)
    }

    fn emit_sum_payload_get(
        &mut self,
        instruction: &ValkyrieMirInstruction,
        nominal: NominalInstanceId,
        variant: VariantId,
        object: &ValkyrieMirOperand,
    ) -> Result<(), String> {
        let Some(sum) = self.ctx.sum_representations().get(&nominal).cloned()
        else {
            return Err(format!(
                "JVM 最小 lowering：sum nominal `{nominal:?}` 没有冻结的 `SumRepresentation`: function={}",
                self.view.symbol
            ));
        };
        let Some(variant_rep) = sum.variants.get(&variant).copied()
        else {
            return Err(format!(
                "JVM 最小 lowering：variant `{variant:?}` 不属于 sum nominal `{nominal:?}`: function={}",
                self.view.symbol
            ));
        };
        if variant_rep.payload_type.is_none() {
            return Err(format!(
                "JVM 最小 lowering：variant `{variant:?}` 没有 payload，不能取载荷: function={} nominal={nominal:?}",
                self.view.symbol
            ));
        }
        let class_name = jvm_sum_class_name(nominal);
        self.emit_operand(object, Some(JvmSlotKind::Reference))?;
        self.instructions.push(JvmInstruction::GetField(JvmFieldRef {
            owner: class_name,
            name: JVM_SUM_PAYLOAD_FIELD.to_string(),
            descriptor: JvmTypeDescriptor::Object(JVM_REFERENCE_CARRIER.to_string()),
        }));

        let output = self.output_of(instruction)?;
        let Some((slot, kind)) = output
        else {
            self.instructions.push(JvmInstruction::Pop);
            return Ok(());
        };
        let Some(target_type) = self.output_type(instruction)?
        else {
            return Err(format!("JVM 最小 lowering：sum payload 结果缺少冻结类型: function={}", self.view.symbol));
        };
        let target = {
            let layouts = self.ctx.aggregate_layouts();
            jvm_type_descriptor(layouts, &target_type)?
        };
        let target_kind = JvmSlotKind::of(&target)?;
        if target_kind != kind {
            return Err(format!(
                "JVM 最小 lowering：sum payload 目标载体 {target_kind:?} 与结果槽 {kind:?} 不符: function={}",
                self.view.symbol
            ));
        }
        self.instructions.push(JvmInstruction::CheckCast(jvm_box_class_name(&target)?));
        if target_kind != JvmSlotKind::Reference {
            self.instructions.push(JvmInstruction::InvokeVirtual(jvm_unbox_method(&target)?));
        }
        self.instructions.push(kind.store(slot));
        Ok(())
    }

    fn store_or_pop_boolean(&mut self, instruction: &ValkyrieMirInstruction) -> Result<(), String> {
        match self.output_of(instruction)? {
            Some((slot, kind)) => {
                if kind != JvmSlotKind::Int {
                    return Err(format!("JVM 最小 lowering：布尔结果槽载体必须是 `int`: function={} actual={kind:?}", self.view.symbol));
                }
                self.instructions.push(JvmInstruction::IStore(slot));
                Ok(())
            }
            None => {
                self.instructions.push(JvmInstruction::Pop);
                Ok(())
            }
        }
    }

    fn emit_array_from_elements(&mut self, instruction: &ValkyrieMirInstruction, array_type: &SemanticType, elements: &[ValkyrieMirOperand]) -> Result<(), String> {
        let element = jvm_array_element_type(array_type)?.clone();
        let (creation, store) = {
            let layouts = self.ctx.aggregate_layouts();
            let element_descriptor = jvm_array_element_descriptor(layouts, &element)?;
            let creation = jvm_array_creation_instruction(&element_descriptor)?;
            let (_, store) = jvm_array_access_instructions(&element_descriptor)?;
            (creation, store)
        };
        let length = i32::try_from(elements.len())
            .map_err(|_| format!("JVM 最小 lowering：数组元素数量超出 `int` 载体: function={}", self.view.symbol))?;
        self.instructions.push(JvmInstruction::IConst(length));
        self.instructions.push(creation);
        for (index, value) in elements.iter().enumerate() {
            let index = i32::try_from(index)
                .map_err(|_| format!("JVM 最小 lowering：数组元素下标超出 `int` 载体: function={}", self.view.symbol))?;
            self.instructions.push(JvmInstruction::Dup);
            self.instructions.push(JvmInstruction::IConst(index));
            self.emit_array_element_value(value, &element)?;
            self.instructions.push(store.clone());
        }
        match self.output_of(instruction)? {
            Some((slot, kind)) => {
                if kind != JvmSlotKind::Reference {
                    return Err(format!("JVM 最小 lowering：数组结果必须是引用槽: function={} actual={kind:?}", self.view.symbol));
                }
                self.instructions.push(JvmInstruction::AStore(slot));
                Ok(())
            }
            None => {
                self.instructions.push(JvmInstruction::Pop);
                Ok(())
            }
        }
    }

    fn emit_array_new(&mut self, instruction: &ValkyrieMirInstruction, array_type: &SemanticType, length: &ValkyrieMirOperand) -> Result<(), String> {
        let element = jvm_array_element_type(array_type)?.clone();
        let creation = {
            let layouts = self.ctx.aggregate_layouts();
            jvm_array_creation_instruction(&jvm_array_element_descriptor(layouts, &element)?)?
        };
        self.emit_operand(length, Some(JvmSlotKind::Int))?;
        self.instructions.push(creation);
        match self.output_of(instruction)? {
            Some((slot, kind)) => {
                if kind != JvmSlotKind::Reference {
                    return Err(format!("JVM 最小 lowering：数组结果必须是引用槽: function={} actual={kind:?}", self.view.symbol));
                }
                self.instructions.push(JvmInstruction::AStore(slot));
                Ok(())
            }
            None => {
                self.instructions.push(JvmInstruction::Pop);
                Ok(())
            }
        }
    }

    fn emit_array_get(&mut self, instruction: &ValkyrieMirInstruction, array: &ValkyrieMirOperand, index: &ValkyrieMirOperand) -> Result<(), String> {
        let array_type = self.operand_type(array)?;
        let element = jvm_array_element_type(&array_type)?.clone();
        let (element_descriptor, target, load) = {
            let layouts = self.ctx.aggregate_layouts();
            let element_descriptor = jvm_array_element_descriptor(layouts, &element)?;
            let target = jvm_type_descriptor(layouts, &element)?;
            let (load, _) = jvm_array_access_instructions(&element_descriptor)?;
            (element_descriptor, target, load)
        };
        let target_kind = JvmSlotKind::of(&target)?;
        self.emit_operand(array, Some(JvmSlotKind::Reference))?;
        self.emit_operand(index, Some(JvmSlotKind::Int))?;
        self.instructions.push(load);
        // `baload` / `saload` 执行符号扩展；无符号 8/16 位元素必须显式零扩展，
        // 否则 `255` / `65535` 会读成 `-1`。物理 descriptor 只决定存取指令，
        // 逻辑元素的 signedness 仍来自语言类型。
        let zero_extension_mask = match &element {
            SemanticType::Integer8 { signed: false } => Some(0xFF),
            SemanticType::Integer16 { signed: false } => Some(0xFFFF),
            _ => None,
        };
        if let Some(mask) = zero_extension_mask {
            self.instructions.push(JvmInstruction::IConst(mask));
            self.instructions.push(JvmInstruction::IAnd);
        }
        if matches!(element_descriptor, JvmTypeDescriptor::Object(_) | JvmTypeDescriptor::Array(_)) && target_kind != JvmSlotKind::Reference {
            self.instructions.push(JvmInstruction::CheckCast(jvm_box_class_name(&target)?));
            self.instructions.push(JvmInstruction::InvokeVirtual(jvm_unbox_method(&target)?));
        }
        match self.output_of(instruction)? {
            Some((slot, kind)) => {
                if kind != target_kind {
                    return Err(format!(
                        "JVM 最小 lowering：数组元素目标载体 {target_kind:?} 与结果槽 {kind:?} 不符: function={}",
                        self.view.symbol
                    ));
                }
                self.instructions.push(kind.store(slot));
                Ok(())
            }
            None => match target_kind {
                JvmSlotKind::Long | JvmSlotKind::Double => Err(format!(
                    "JVM 最小 lowering：丢弃 2 槽数组元素需要 `pop2` 编码，当前 JVM 指令模型不提供: function={}",
                    self.view.symbol
                )),
                _ => {
                    self.instructions.push(JvmInstruction::Pop);
                    Ok(())
                }
            },
        }
    }

    fn emit_array_set(&mut self, array: &ValkyrieMirOperand, index: &ValkyrieMirOperand, value: &ValkyrieMirOperand) -> Result<(), String> {
        let array_type = self.operand_type(array)?;
        let element = jvm_array_element_type(&array_type)?.clone();
        let store = {
            let layouts = self.ctx.aggregate_layouts();
            jvm_array_access_instructions(&jvm_array_element_descriptor(layouts, &element)?)?.1
        };
        self.emit_operand(array, Some(JvmSlotKind::Reference))?;
        self.emit_operand(index, Some(JvmSlotKind::Int))?;
        self.emit_array_element_value(value, &element)?;
        self.instructions.push(store);
        Ok(())
    }
}

/// MIR 操作的稳定诊断名（不依赖 Debug 输出细节）。
fn operation_name(operation: &ValkyrieMirOperation) -> &'static str {
    match operation {
        ValkyrieMirOperation::LoadConstant { .. } => "LoadConstant",
        ValkyrieMirOperation::Copy { .. } => "Copy",
        ValkyrieMirOperation::Cast { .. } => "Cast",
        ValkyrieMirOperation::StoreVar { .. } => "StoreVar",
        ValkyrieMirOperation::Invoke { .. } => "Invoke",
        ValkyrieMirOperation::LoadFunctionRef { .. } => "LoadFunctionRef",
        ValkyrieMirOperation::StructNew { .. } => "StructNew",
        ValkyrieMirOperation::TupleNew { .. } => "TupleNew",
        ValkyrieMirOperation::TupleGet { .. } => "TupleGet",
        ValkyrieMirOperation::AggregateCopy { .. } => "AggregateCopy",
        ValkyrieMirOperation::FieldGet { .. } => "FieldGet",
        ValkyrieMirOperation::FieldSet { .. } => "FieldSet",
        ValkyrieMirOperation::SumNew { .. } => "SumNew",
        ValkyrieMirOperation::SumPayloadGet { .. } => "SumPayloadGet",
        ValkyrieMirOperation::SumVariantIs { .. } => "SumVariantIs",
        ValkyrieMirOperation::ArrayNew { .. } => "ArrayNew",
        ValkyrieMirOperation::ArrayFromElements { .. } => "ArrayFromElements",
        ValkyrieMirOperation::ArrayGet { .. } => "ArrayGet",
        ValkyrieMirOperation::ArraySet { .. } => "ArraySet",
        ValkyrieMirOperation::ArrayLength { .. } => "ArrayLength",
        ValkyrieMirOperation::NullIsNull { .. } => "NullIsNull",
        ValkyrieMirOperation::NullUnwrap { .. } => "NullUnwrap",
    }
}

/// 只为 MIR 中实际出现 sum 指令的 nominal 生成 tag/payload companion 类。
pub(crate) fn build_sum_classes(ctx: &LaneContext<'_>) -> Result<Vec<JvmClassFile>, String> {
    let mut nominals = std::collections::BTreeSet::new();
    for instance in ctx.reachable() {
        let Some(function) = ctx.function(instance)
        else {
            continue;
        };
        for block in function.blocks.values() {
            for instruction in &block.instructions {
                match &instruction.operation {
                    ValkyrieMirOperation::SumNew { nominal, .. }
                    | ValkyrieMirOperation::SumPayloadGet { nominal, .. }
                    | ValkyrieMirOperation::SumVariantIs { nominal, .. } => {
                        nominals.insert(*nominal);
                    }
                    _ => {}
                }
            }
        }
    }

    let mut classes = Vec::with_capacity(nominals.len());
    for nominal in nominals {
        if !ctx.sum_representations().contains_key(&nominal) {
            return Err(format!("JVM 最小 lowering：sum nominal `{nominal:?}` 没有冻结的 `SumRepresentation`"));
        }
        let mut class_file = JvmClassFile::new(jvm_sum_class_name(nominal));
        class_file.fields.push(JvmFieldSignature {
            name: JVM_SUM_TAG_FIELD.to_string(),
            descriptor: JvmTypeDescriptor::Int,
            access_flags: JVM_ACC_PUBLIC,
        });
        class_file.fields.push(JvmFieldSignature {
            name: JVM_SUM_PAYLOAD_FIELD.to_string(),
            descriptor: JvmTypeDescriptor::Object(JVM_REFERENCE_CARRIER.to_string()),
            access_flags: JVM_ACC_PUBLIC,
        });
        class_file.methods.push(jvm_default_constructor());
        classes.push(class_file);
    }
    Ok(classes)
}

/// 为所有需要堆载体的布局生成 companion 类。
///
/// 类名只由 layout 身份派生；JVM 没有内联结构体，逐字段复制只能走
/// `AggregateCopy`。零字段值存储布局与块 2 的载体拒绝保持同一合同。
pub(crate) fn build_layout_classes(layouts: &AggregateLayoutPlan) -> Result<Vec<JvmClassFile>, String> {
    // 第一遍：先为全部可达布局登记目标类型身份（类名只由布局身份派生）。
    // 递归引用因此总能命中已注册的载体，不依赖布局遍历顺序。
    let mut classes = Vec::new();
    let mut class_index_by_layout = BTreeMap::new();
    for layout in &layouts.layouts {
        if !(layout.storage.is_reference() || !layout.fields.is_empty()) {
            continue;
        }
        class_index_by_layout.insert(layout.id, classes.len());
        classes.push(JvmClassFile::new(jvm_layout_class_name(layout)));
    }
    // 第二遍：在同一份冻结计划上解析字段与引用（含闭合 `Apply` 的布局映射）。
    for layout in &layouts.layouts {
        let Some(&class_index) = class_index_by_layout.get(&layout.id)
        else {
            continue;
        };
        let class_file = &mut classes[class_index];
        for (slot, field) in layout.fields.iter().enumerate() {
            class_file.fields.push(JvmFieldSignature {
                name: jvm_layout_field_name(slot),
                descriptor: jvm_type_descriptor(layouts, &field.ty)?,
                access_flags: JVM_ACC_PUBLIC,
            });
        }
        class_file.methods.push(jvm_default_constructor());
    }
    Ok(classes)
}
