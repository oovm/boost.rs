//! CLR lane 逐操作 MSIL 发射：直读 `ValkyrieMirFunction` 的块/指令/终结符。
//!
//! 本模块把旧公共路径「MIR → 共享程序 → 解释」的两段映射合成一段：每个
//! `ValkyrieMirOperation` / `ValkyrieMirTerminator` 直接映射为 `MSIL` 指令序列
//! （载体裁决由本 lane 的 [`super::carrier`] 完成，emitter 只供 `ClrValueKind` /
//! 已结算形状 → opcode / operator 绑定 / 值聚合复制深度等目标私有编码原语），
//! 不经过任何跨目标共享程序载荷。逐操作语义与旧
//! `emit.rs` 解释公共 IR 完全一致：块形参逆序写回、入口 `ldarg→stloc` 前言、
//! 比较单 opcode、`i64` 移位 `conv.i4` 截窄、sum tag/payload 装箱、值聚合逐层
//! 新建、缺冻结布局/载体身份一律结构化拒绝。

use std::collections::BTreeMap;

use compiler_emitter::emitter_backend_clr::{
    CLR_SUM_PAYLOAD_FIELD, CLR_SUM_TAG_FIELD, ClrOperatorBinding, ClrOperatorCode, ClrValueKind, MAX_VALUE_AGGREGATE_COPY_DEPTH, MsilInstruction,
    MsilInstructionOperand, MsilMethodBody, MsilMethodRef, MsilMethodSignature, MsilOpcode, MsilType, TargetOperandStep, TargetOperatorPlan,
    TargetOperatorRecipe, branch_instruction, call_instruction, clr_array_load_opcode, clr_array_store_opcode, clr_layout_field_name,
    clr_layout_type_name, clr_sum_type_name, msil_instruction, msil_int_normalize_opcode, nested_value_aggregate_layout, opcode_instruction,
    value_aggregate_copy_depth,
};
use compiler_types::{AggregateLayout, CanonicalTypeKind, FieldId, ItemInstanceId, LayoutId, NominalInstanceId, SemanticType, VariantId};
use compiler_types::semantic_ids::{MirValueId, TypeId};

use super::carrier::{clr_array_element_type, msil_array_element_type, msil_int_shape, msil_parameter_type, msil_type};

use crate::compiler::emitter::pipeline::lane::{FunctionView, LaneContext};
use crate::compiler::valkyrie_mir::{
    ValkyrieMirArrayInitialization, ValkyrieMirBlock, ValkyrieMirBlockId, ValkyrieMirCallee, ValkyrieMirConstant, ValkyrieMirFunction,
    ValkyrieMirInstruction, ValkyrieMirOperand, ValkyrieMirOperation, ValkyrieMirTerminator,
};

/// 块标签：与旧公共路径同形（`block_{N}`，N 为块身份）。
fn block_label(block: ValkyrieMirBlockId) -> String {
    format!("block_{}", block.0)
}

/// 函数级 CLR 方法计划：稳定名字与冻结签名。
#[derive(Debug, Clone)]
pub(crate) struct ClrMethodPlan {
    pub(crate) name: String,
    pub(crate) signature: MsilMethodSignature,
}

/// 逐函数局部槽位结算：SSA 值 → `MSIL` local。
///
/// 分配顺序与旧公共路径的 `ExecutableSlotPlan::plan_semantic` 完全一致：入口块
/// 形参最先，其余块形参按块身份升序，然后全部指令结果按块升序逐指令登记；
/// 局部槽数量因此逐函数对齐旧字节码。
#[derive(Default)]
struct SlotPlan {
    local_types: Vec<SemanticType>,
    value_locals: BTreeMap<MirValueId, u16>,
    block_param_locals: BTreeMap<(ValkyrieMirBlockId, usize), u16>,
}

impl SlotPlan {
    fn alloc(&mut self, ty: SemanticType) -> u16 {
        let local = u16::try_from(self.local_types.len()).expect("CLR local 数量超出 u16");
        self.local_types.push(ty);
        local
    }

    /// 从 MIR 函数与冻结类型视图结算槽位表。
    fn plan(function: &ValkyrieMirFunction, view: &FunctionView) -> Result<Self, String> {
        let mut plan = Self::default();
        let entry = function.blocks.get(&function.entry).ok_or_else(|| {
            format!("CLR 入口槽合同破坏：symbol=`{}` 声明了入口块 {}，但块表中不存在该入口块", view.symbol, function.entry.0)
        })?;
        for (value, _) in &entry.parameters {
            let ty = view
                .value_types
                .get(value)
                .cloned()
                .ok_or_else(|| format!("CLR 入口形参 %{} 缺少冻结类型（symbol=`{}`）", value.index(), view.symbol))?;
            let local = plan.alloc(ty);
            plan.value_locals.insert(*value, local);
        }
        for block in function.blocks.values() {
            if block.id == function.entry {
                continue;
            }
            for (value, _) in &block.parameters {
                let ty = view
                    .value_types
                    .get(value)
                    .cloned()
                    .ok_or_else(|| format!("CLR 块形参 %{} 缺少冻结类型（symbol=`{}`）", value.index(), view.symbol))?;
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
                        .ok_or_else(|| format!("CLR 指令结果 %{} 缺少冻结类型（symbol=`{}`）", output.index(), view.symbol))?;
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
    fn instruction_output(instruction: &ValkyrieMirInstruction) -> Option<MirValueId> {
        instruction.results.first().copied()
    }
}

/// 数据流标准逆后序（RPO）：块发射顺序与旧公共路径同源。
///
/// 顶点是 MIR 块身份（`u32`）；悬空跳转目标保留为无子顶点（发射期跳过不存在的块，
/// 与旧实现一致）。
fn collect_reachable_blocks(function: &ValkyrieMirFunction) -> Vec<ValkyrieMirBlockId> {
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

struct ClrMethodEmitter<'a> {
    ctx: &'a LaneContext<'a>,
    function: &'a ValkyrieMirFunction,
    view: &'a FunctionView,
    return_type: MsilType,
    slots: SlotPlan,
    method_plans: &'a BTreeMap<ItemInstanceId, ClrMethodPlan>,
    operator_by_instance: &'a BTreeMap<ItemInstanceId, (String, ClrOperatorBinding)>,
    instructions: Vec<MsilInstruction>,
    /// 值聚合递归复制使用的临时引用局部：每层复制占 2 槽（源/目标）。
    copy_scratch: Vec<u16>,
    /// 编译器内部标签序号（递归复制空值分支使用）。
    label_ordinal: usize,
}

/// 降低单个 MIR 函数为 `MSIL` 方法体。
pub(crate) fn lower_function(
    ctx: &LaneContext<'_>,
    function: &ValkyrieMirFunction,
    view: &FunctionView,
    method: &ClrMethodPlan,
    method_plans: &BTreeMap<ItemInstanceId, ClrMethodPlan>,
    operator_by_instance: &BTreeMap<ItemInstanceId, (String, ClrOperatorBinding)>,
    is_entry_point: bool,
) -> Result<MsilMethodBody, String> {
    let slots = SlotPlan::plan(function, view)?;
    let layouts = ctx.aggregate_layouts();
    let mut locals = slots.local_types.iter().map(|ty| msil_parameter_type(layouts, ty)).collect::<Result<Vec<_>, String>>()?;

    // 值聚合递归复制：按冻结布局的嵌套深度预留 2 槽/层的临时引用局部。
    let mut copy_depth = 0u32;
    for instruction in function.blocks.values().flat_map(|block| &block.instructions) {
        if let ValkyrieMirOperation::AggregateCopy { source, .. } = &instruction.operation
            && let Some(layout) = function
                .value_types
                .get(source)
                .and_then(|type_id| ctx.aggregate_layout_by_type().get(type_id).copied())
                .and_then(|layout_id| layouts.layouts.iter().find(|candidate| candidate.id == layout_id))
        {
            copy_depth = copy_depth.max(value_aggregate_copy_depth(layouts, layout));
        }
    }
    if copy_depth > MAX_VALUE_AGGREGATE_COPY_DEPTH {
        return Err(format!(
            "CLR 最小 lowering：值聚合复制嵌套 {copy_depth} 层超过上限 {MAX_VALUE_AGGREGATE_COPY_DEPTH}: function={}",
            view.symbol
        ));
    }
    let mut copy_scratch = Vec::with_capacity((copy_depth as usize).saturating_mul(2));
    for _ in 0..(copy_depth as usize).saturating_mul(2) {
        copy_scratch.push(u16::try_from(locals.len()).map_err(|_| format!("CLR 局部槽数量溢出: function={}", view.symbol))?);
        locals.push(MsilType::Object);
    }

    let mut emitter = ClrMethodEmitter {
        ctx,
        function,
        view,
        return_type: method.signature.return_type.clone(),
        slots,
        method_plans,
        operator_by_instance,
        instructions: Vec::new(),
        copy_scratch,
        label_ordinal: 0,
    };
    emitter.emit_entry_parameter_prologue()?;
    for block_id in collect_reachable_blocks(function) {
        let Some(block) = function.blocks.get(&block_id)
        else {
            continue;
        };
        emitter.emit_block(block)?;
    }

    let max_stack = u16::try_from(emitter.instructions.len().saturating_mul(2).saturating_add(2)).unwrap_or(u16::MAX);
    Ok(MsilMethodBody {
        method: MsilMethodRef { owner: None, name: method.name.clone(), signature: method.signature.clone() },
        locals,
        instructions: emitter.instructions,
        max_stack,
        is_entry_point,
        is_async: false,
    })
}

impl ClrMethodEmitter<'_> {
    /// 方法入口把 `ldarg` 写入 entry 块参数局部，保证后续 `ldloc` 有定义。
    fn emit_entry_parameter_prologue(&mut self) -> Result<(), String> {
        let entry = self
            .function
            .blocks
            .get(&self.function.entry)
            .ok_or_else(|| format!("CLR 最小 lowering：函数 `{}` 缺少入口块", self.view.symbol))?;
        for index in 0..entry.parameters.len() {
            let local = *self
                .slots
                .block_param_locals
                .get(&(self.function.entry, index))
                .ok_or_else(|| format!("CLR 最小 lowering：入口形参 {index} 缺少局部槽: function={}", self.view.symbol))?;
            self.local_kind(local)?;
            self.push_argument(index);
            self.store_local(local);
        }
        Ok(())
    }

    fn emit_block(&mut self, block: &ValkyrieMirBlock) -> Result<(), String> {
        self.instructions.push(MsilInstruction { label: Some(block_label(block.id)), opcode: MsilOpcode::Nop, operand: None });
        for instruction in &block.instructions {
            self.emit_instruction(instruction)?;
        }
        self.emit_terminator(block)
    }

    fn emit_instruction(&mut self, instruction: &ValkyrieMirInstruction) -> Result<(), String> {
        if instruction.results.len() > 1 {
            return Err(format!(
                "CLR 最小 lowering 不支持多结果指令（results={}）: function={}",
                instruction.results.len(),
                self.view.symbol
            ));
        }
        match &instruction.operation {
            ValkyrieMirOperation::LoadConstant { constant } => {
                let (slot, kind) = self.require_output(instruction)?;
                self.emit_constant(constant, Some(kind))?;
                self.store_local(slot);
            }
            ValkyrieMirOperation::Copy { source } => {
                let (slot, kind) = self.require_output(instruction)?;
                self.emit_operand(&ValkyrieMirOperand::Value(*source), Some(kind))?;
                self.store_local(slot);
            }
            ValkyrieMirOperation::Cast { source, ty } => {
                // 语言层 `as` 是表示转换：源/目标位宽与符号性只由冻结的 `SemanticType`
                // 决定，必须发射真实 conv 指令，不能退化成 Copy。
                let (slot, target_kind) = self.require_output(instruction)?;
                let source_operand = ValkyrieMirOperand::Value(*source);
                let source_ty = self.operand_semantic_type(&source_operand)?;
                let source_kind = self.value_slot(*source)?.1;
                self.emit_operand(&source_operand, Some(source_kind))?;
                let target_ty = self
                    .ctx
                    .lower_type(*ty)
                    .map_err(|reason| format!("CLR 最小 lowering：Cast 目标类型缺少语义合同: {reason}: function={}", self.view.symbol))?;
                self.emit_cast_conversion(&source_ty, &target_ty, source_kind, target_kind)?;
                self.store_local(slot);
            }
            ValkyrieMirOperation::StoreVar { value, .. } => {
                let output = self.output_of(instruction)?;
                let expected = match &output {
                    Some((_, kind)) => Some(*kind),
                    None => Some(self.value_slot(*value)?.1),
                };
                self.emit_operand(&ValkyrieMirOperand::Value(*value), expected)?;
                match output {
                    Some((slot, _)) => self.store_local(slot),
                    None => self.emit_pop(),
                }
            }
            ValkyrieMirOperation::Invoke { callee, arguments } => {
                let output = self.output_of(instruction)?;
                self.emit_call(callee, arguments, output)?;
            }
            ValkyrieMirOperation::StructNew { nominal, fields } => {
                let layout = self.layout_by_nominal(*nominal)?;
                let output = self.require_output(instruction)?;
                self.emit_layout_new(&layout, output)?;
                for (field, value) in fields {
                    let Some((layout_id, slot)) = self.ctx.aggregate_layout_by_field().get(field).copied()
                    else {
                        return Err(format!("CLR 最小 lowering：字段身份 `{field:?}` 未登记布局索引: function={}", self.view.symbol));
                    };
                    if layout_id != layout.id {
                        return Err(format!(
                            "CLR 最小 lowering：字段 `{field:?}` 属于布局身份 `{layout_id}`，与构造的 nominal `{nominal:?}` 布局 `{}` 不符: function={}",
                            layout.id, self.view.symbol
                        ));
                    }
                    self.emit_layout_field_write(&layout, output.0, slot as usize, &ValkyrieMirOperand::Value(*value))?;
                }
            }
            ValkyrieMirOperation::TupleGet { object, tuple_type, index } => {
                // 旧公共路径在计划层校验元组身份与位置索引，再按冻结布局取槽。
                let arity = match self.ctx.linked().types.get(tuple_type).map(|row| &row.kind) {
                    Some(CanonicalTypeKind::Tuple(members)) => members.len(),
                    Some(CanonicalTypeKind::LabeledTuple { fields }) => fields.len(),
                    _ => return Err(format!("CLR 最小 lowering：TupleGet 对象不是元组类型: {tuple_type:?}: function={}", self.view.symbol)),
                };
                if (*index as usize) >= arity {
                    return Err(format!("CLR 最小 lowering：TupleGet 位置索引 {index} 越界（元组长度 {arity}）: function={}", self.view.symbol));
                }
                let layout_id = self.layout_id_for_type(*tuple_type, "TupleGet")?;
                let layout = self.layout_by_id(layout_id)?;
                if (*index as usize) >= layout.fields.len() {
                    return Err(format!(
                        "CLR 最小 lowering：TupleGet 位置索引 {index} 越界（layout `{}` 长度 {}）: function={}",
                        layout.name,
                        layout.fields.len(),
                        self.view.symbol
                    ));
                }
                // 标签只在类型身份层；CLR 载体按冻结 layout 的字段槽读取。
                let (_, field_kind) = self.layout_field_type(&layout, *index)?;
                self.emit_operand(&ValkyrieMirOperand::Value(*object), Some(ClrValueKind::Reference))?;
                self.push_field_opcode(MsilOpcode::Ldfld, &clr_layout_type_name(&layout), &clr_layout_field_name(*index as usize));
                match self.output_of(instruction)? {
                    Some((output, output_kind)) => {
                        if output_kind != field_kind {
                            return Err(format!(
                                "CLR 最小 lowering：TupleGet 位置 {index} 载体 {field_kind:?} 与结果槽 {output_kind:?} 不符: function={}",
                                self.view.symbol
                            ));
                        }
                        self.store_local(output);
                    }
                    None => self.emit_pop(),
                }
            }
            ValkyrieMirOperation::TupleNew { element_types, fields } => {
                let result = instruction
                    .results
                    .first()
                    .copied()
                    .ok_or_else(|| format!("CLR 最小 lowering：TupleNew 缺少结果值: function={}", self.view.symbol))?;
                let type_id = self.value_type_id(result)?;
                // 标签只属于 canonical 身份；目标布局按位置，带标签元组与位置元组共享同一构造合同。
                if !matches!(
                    self.ctx.linked().types.get(&type_id).map(|row| &row.kind),
                    Some(CanonicalTypeKind::Tuple(_)) | Some(CanonicalTypeKind::LabeledTuple { .. })
                ) {
                    return Err(format!("CLR 最小 lowering：TupleNew 结果不是 tuple 类型: {type_id:?}: function={}", self.view.symbol));
                }
                if fields.len() != element_types.len() {
                    return Err(format!("CLR 最小 lowering：TupleNew 元素合同长度不一致: function={}", self.view.symbol));
                }
                let layout_id = self.layout_id_for_type(type_id, "TupleNew")?;
                let layout = self.layout_by_id(layout_id)?;
                if fields.len() != layout.fields.len() {
                    return Err(format!(
                        "CLR 最小 lowering：tuple 布局 `{}` 字段数量与操作数不符: function={} expected={} actual={}",
                        layout.id,
                        self.view.symbol,
                        layout.fields.len(),
                        fields.len()
                    ));
                }
                let output = self.require_output(instruction)?;
                self.emit_layout_new(&layout, output)?;
                for (slot, value) in fields.iter().enumerate() {
                    self.emit_layout_field_write(&layout, output.0, slot, &ValkyrieMirOperand::Value(*value))?;
                }
            }
            ValkyrieMirOperation::AggregateCopy { source, destination } => {
                let source_type_id = self.value_type_id(*source)?;
                let destination_type_id = self.value_type_id(*destination)?;
                if source_type_id != destination_type_id {
                    return Err(format!(
                        "CLR 最小 lowering：AggregateCopy 源与目标类型不一致: {source_type_id:?} != {destination_type_id:?}: function={}",
                        self.view.symbol
                    ));
                }
                let layout_id = self.layout_id_for_type(source_type_id, "AggregateCopy")?;
                let layout = self.layout_by_id(layout_id)?;
                if layout.fields.is_empty() {
                    self.emit_operand(&ValkyrieMirOperand::Value(*source), Some(ClrValueKind::Reference))?;
                    self.store_operand(&ValkyrieMirOperand::Value(*destination), ClrValueKind::Reference)?;
                }
                else {
                    let dest_slot = self.reference_operand_slot(&ValkyrieMirOperand::Value(*destination))?;
                    let source_slot = self.reference_operand_slot(&ValkyrieMirOperand::Value(*source))?;
                    self.emit_value_aggregate_copy(&layout, source_slot, dest_slot, 0)?;
                }
            }
            ValkyrieMirOperation::FieldGet { object, field } => {
                let (layout, slot) = self.field_location(*field)?;
                let (_, field_kind) = self.layout_field_type(&layout, slot)?;
                self.emit_operand(&ValkyrieMirOperand::Value(*object), Some(ClrValueKind::Reference))?;
                self.push_field_opcode(MsilOpcode::Ldfld, &clr_layout_type_name(&layout), &clr_layout_field_name(slot as usize));
                match self.output_of(instruction)? {
                    Some((output, output_kind)) => {
                        if output_kind != field_kind {
                            return Err(format!(
                                "CLR 最小 lowering：字段 `{field:?}` 载体 {field_kind:?} 与结果槽 {output_kind:?} 不符: function={}",
                                self.view.symbol
                            ));
                        }
                        self.store_local(output);
                    }
                    None => self.emit_pop(),
                }
            }
            ValkyrieMirOperation::FieldSet { object, field, value } => {
                let (layout, slot) = self.field_location(*field)?;
                let (_, field_kind) = self.layout_field_type(&layout, slot)?;
                self.emit_operand(&ValkyrieMirOperand::Value(*object), Some(ClrValueKind::Reference))?;
                self.emit_operand(&ValkyrieMirOperand::Value(*value), Some(field_kind))?;
                self.push_field_opcode(MsilOpcode::Stfld, &clr_layout_type_name(&layout), &clr_layout_field_name(slot as usize));
            }
            ValkyrieMirOperation::SumNew { nominal, variant, payload } => {
                self.emit_sum_new(instruction, *nominal, *variant, payload.as_ref())?;
            }
            ValkyrieMirOperation::SumPayloadGet { nominal, variant, object } => {
                self.emit_sum_payload_get(instruction, *nominal, *variant, object)?;
            }
            ValkyrieMirOperation::SumVariantIs { nominal, variant, object } => {
                self.emit_sum_variant_is(instruction, *nominal, *variant, object)?;
            }
            ValkyrieMirOperation::ArrayFromElements { array_type, elements } => {
                let array_type = self.ctx.lower_type(*array_type).map_err(|reason| {
                    format!("CLR 最小 lowering：ArrayFromElements 数组类型缺少语义合同: {reason}: function={}", self.view.symbol)
                })?;
                let operands = elements.iter().copied().map(ValkyrieMirOperand::Value).collect::<Vec<_>>();
                self.emit_array_from_elements(instruction, &array_type, &operands)?;
            }
            ValkyrieMirOperation::ArrayNew { array_type, length, initialization } => {
                let array_type = self
                    .ctx
                    .lower_type(*array_type)
                    .map_err(|reason| format!("CLR 最小 lowering：ArrayNew 数组类型缺少语义合同: {reason}: function={}", self.view.symbol))?;
                match initialization {
                    ValkyrieMirArrayInitialization::Default => self.emit_array_new(instruction, &array_type, &ValkyrieMirOperand::Value(*length))?,
                    ValkyrieMirArrayInitialization::Fill(_) => {
                        return Err(format!("CLR 最小 lowering 不支持数组 Fill 初始化: function={}", self.view.symbol));
                    }
                }
            }
            ValkyrieMirOperation::ArrayGet { array, index } => {
                self.emit_array_get(instruction, &ValkyrieMirOperand::Value(*array), &ValkyrieMirOperand::Value(*index))?;
            }
            ValkyrieMirOperation::ArraySet { array, index, value } => {
                self.emit_array_set(&ValkyrieMirOperand::Value(*array), &ValkyrieMirOperand::Value(*index), &ValkyrieMirOperand::Value(*value))?;
            }
            ValkyrieMirOperation::ArrayLength { array } => {
                self.emit_operand(&ValkyrieMirOperand::Value(*array), Some(ClrValueKind::Reference))?;
                self.push_opcode(MsilOpcode::Ldlen);
                match self.output_of(instruction)? {
                    Some((slot, kind)) => {
                        if kind != ClrValueKind::Int32 {
                            return Err(format!(
                                "CLR 最小 lowering：`ArrayLength` 结果必须是 `int32` 槽: function={} actual={kind:?}",
                                self.view.symbol
                            ));
                        }
                        self.store_local(slot);
                    }
                    None => self.emit_pop(),
                }
            }
            ValkyrieMirOperation::NullIsNull { value } => {
                self.emit_operand(&ValkyrieMirOperand::Value(*value), Some(ClrValueKind::Reference))?;
                self.push_opcode(MsilOpcode::Ldnull);
                self.push_opcode(MsilOpcode::Ceq);
                self.store_or_pop_boolean(instruction)?;
            }
            ValkyrieMirOperation::NullUnwrap { value } => {
                let output = self.output_of(instruction)?;
                self.emit_operand(&ValkyrieMirOperand::Value(*value), Some(ClrValueKind::Reference))?;
                match output {
                    Some((slot, kind)) => {
                        if kind != ClrValueKind::Reference {
                            return Err(format!(
                                "CLR 最小 lowering：可空标量需要 sum 表示，不能按引用直接解包: function={} actual={kind:?}",
                                self.view.symbol
                            ));
                        }
                        self.store_local(slot);
                    }
                    None => self.emit_pop(),
                }
            }
            // 闭包捕获环境在 CLR 没有载体：必须点名 lambda 实例与捕获名结构化失败，
            // 不得丢弃环境、把捕获提升为模块全局或用直调替代。
            ValkyrieMirOperation::LoadFunctionRef { instance, environment: Some(ValkyrieMirOperand::Value(_)), captured } => {
                return Err(format!(
                    "CLR 目标没有闭包载体：lambda instance `{instance:?}` 捕获 [{}]；function={}",
                    captured.join(", "),
                    self.view.symbol
                ));
            }
            ValkyrieMirOperation::LoadFunctionRef { environment: Some(ValkyrieMirOperand::Constant(_)), .. } => {
                return Err(format!("闭包捕获环境必须是已定义的 SSA 值: function={}", self.view.symbol));
            }
            other => {
                return Err(format!("CLR 最小 lowering 尚不支持指令 `{}`：function={}", mir_operation_name(other), self.view.symbol));
            }
        }
        Ok(())
    }

    fn emit_terminator(&mut self, block: &ValkyrieMirBlock) -> Result<(), String> {
        match &block.terminator {
            ValkyrieMirTerminator::Return { value } => {
                let is_void = self.return_type == MsilType::Void;
                match (value, is_void) {
                    (Some(value), false) => {
                        let kind = ClrValueKind::of_msil_type(&self.return_type)?;
                        self.emit_operand(&ValkyrieMirOperand::Value(*value), Some(kind))?;
                        self.push_opcode(MsilOpcode::Ret);
                    }
                    (None, true) => self.push_opcode(MsilOpcode::Ret),
                    (Some(_), true) => {
                        return Err(format!("CLR 最小 lowering：函数 `{}` 返回了值但签名是 void", self.view.symbol));
                    }
                    (None, false) => {
                        return Err(format!("CLR 最小 lowering：非 void 函数 `{}` 缺少返回值", self.view.symbol));
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
                        .ok_or_else(|| format!("CLR 最小 lowering：函数 `{}` 跳转目标块 {target:?} 不存在", self.view.symbol))?;
                    let kinds = target_block
                        .parameters
                        .iter()
                        .map(|(parameter, _)| {
                            let ty = self
                                .view
                                .value_types
                                .get(parameter)
                                .ok_or_else(|| format!("CLR 最小 lowering：块参数缺少冻结类型: function={}", self.view.symbol))?;
                            let kind = ClrValueKind::of_msil_type(&msil_parameter_type(self.ctx.aggregate_layouts(), ty)?)?;
                            Ok(kind)
                        })
                        .collect::<Result<Vec<_>, String>>()?;
                    (target_block.parameters.len(), kinds)
                };
                if arguments.len() != parameter_count {
                    return Err(format!(
                        "CLR 最小 lowering：跳转目标块 {target:?} 形参数量与实参不符: function={} expected={} actual={}",
                        self.view.symbol,
                        parameter_count,
                        arguments.len()
                    ));
                }
                let operands = arguments.iter().copied().map(ValkyrieMirOperand::Value).collect::<Vec<_>>();
                for (argument, kind) in operands.iter().zip(&kinds) {
                    self.emit_operand(argument, Some(*kind))?;
                }
                for index in (0..arguments.len()).rev() {
                    let local = *self
                        .slots
                        .block_param_locals
                        .get(&(*target, index))
                        .ok_or_else(|| format!("CLR 最小 lowering：跳转目标块 {target:?} 形参 {index} 缺少局部槽"))?;
                    self.store_local(local);
                }
                self.push_branch(MsilOpcode::Br, target_label);
            }
            ValkyrieMirTerminator::Branch { condition, then_target, else_target } => {
                let targets_have_parameters = self
                    .function
                    .blocks
                    .values()
                    .filter(|block| block.id == *then_target || block.id == *else_target)
                    .any(|block| !block.parameters.is_empty());
                if targets_have_parameters {
                    return Err(format!("CLR 最小 lowering 只支持无块参数的条件分支：function={}", self.view.symbol));
                }
                self.emit_operand(&ValkyrieMirOperand::Value(*condition), Some(ClrValueKind::Int32))?;
                self.push_branch(MsilOpcode::Brfalse, block_label(*else_target));
                self.push_branch(MsilOpcode::Br, block_label(*then_target));
            }
            ValkyrieMirTerminator::Unreachable => {
                if self.return_type == MsilType::Void {
                    self.push_opcode(MsilOpcode::Ret);
                }
                else {
                    return Err(format!("CLR 最小 lowering：非 void 函数 `{}` 的 `Unreachable` 终结缺少返回指令", self.view.symbol));
                }
            }
            other => {
                return Err(format!("CLR 最小 lowering 尚不支持终结符 `{other:?}`: function={}", self.view.symbol));
            }
        }
        Ok(())
    }

    fn emit_call(&mut self, callee: &ValkyrieMirCallee, arguments: &[MirValueId], output: Option<(u16, ClrValueKind)>) -> Result<(), String> {
        // witness 分派必须在实例化固定点消去；到达目标层即结构化失败，
        // 与旧公共路径 `resolve_witness_to_item` 同一合同。
        let instance = match callee {
            ValkyrieMirCallee::Item(instance) => *instance,
            ValkyrieMirCallee::Witness(witness) => {
                return Err(format!(
                    "canonical 未消去的 trait witness 分派到达目标层：trait={:?} method={:?} self_type={:?}；\
                     具体 witness 必须在实例化固定点冻结 `ItemInstanceId`: function={}",
                    witness.trait_declaration, witness.method_declaration, witness.self_type, self.view.symbol
                ));
            }
            ValkyrieMirCallee::Value(value) => {
                return Err(format!("CLR 最小 lowering 只支持 `Item` 身份的静态调用: function={} callee={value:?}", self.view.symbol));
            }
        };
        let operands = arguments.iter().copied().map(ValkyrieMirOperand::Value).collect::<Vec<_>>();
        if let Some((mnemonic, binding)) = self.operator_by_instance.get(&instance) {
            return self.emit_operator_call(mnemonic, binding, &operands, output);
        }
        if let Some(import) = self.ctx.imports().get(&instance) {
            let links = import.links.iter().map(|link| link.to_string()).collect::<Vec<_>>().join(", ");
            return Err(format!(
                "CLR 最小 lowering 尚不支持宿主导入 `{instance:?}`（链接 `{links}`）：缺少 CLR host 编码，function={}",
                self.view.symbol
            ));
        }
        let Some(method) = self.method_plans.get(&instance).cloned()
        else {
            return Err(format!(
                "CLR 最小 lowering：同片段调用目标 `{instance:?}` 不在可达方法集中: function={}",
                self.view.symbol
            ));
        };
        let parameter_kinds = method.signature.parameter_types.iter().map(ClrValueKind::of_msil_type).collect::<Result<Vec<_>, String>>()?;
        if operands.len() != parameter_kinds.len() {
            return Err(format!(
                "CLR 最小 lowering：调用 `{}` 实参数量与冻结签名不符: function={} expected={} actual={}",
                method.name,
                self.view.symbol,
                parameter_kinds.len(),
                operands.len()
            ));
        }
        for (argument, kind) in operands.iter().zip(&parameter_kinds) {
            self.emit_operand(argument, Some(*kind))?;
        }
        self.instructions.push(call_instruction(MsilMethodRef {
            owner: None,
            name: method.name.clone(),
            signature: method.signature.clone(),
        }));
        self.store_call_result(&method, output)
    }

    /// `[clr_operator("...")]` 绑定：只机械展开计划冻结的配方。
    fn emit_operator_call(
        &mut self,
        mnemonic: &str,
        binding: &ClrOperatorBinding,
        arguments: &[ValkyrieMirOperand],
        output: Option<(u16, ClrValueKind)>,
    ) -> Result<(), String> {
        let ClrOperatorBinding::Frozen(plan) = binding
        else {
            return Err(format!("CLR 目标 `clr_operator` 绑定缺少可编码助记符 `{mnemonic}`: function={}", self.view.symbol));
        };
        let encoding = plan.encoding;
        if arguments.len() != plan.arity {
            return Err(format!(
                "CLR 目标 operator `{mnemonic}` 实参数量与冻结 arity 不符: function={} expected={} actual={}",
                self.view.symbol,
                plan.arity,
                arguments.len()
            ));
        }
        let expected_kind = ClrValueKind::of_operand_type(plan.operand_type);
        self.emit_operator_operands(mnemonic, expected_kind, plan, arguments)?;
        match encoding {
            ClrOperatorCode::Single(opcode) => self.push_opcode(opcode),
            // `i64` 移位：位移量先 `conv.i4` 截窄（CLR lane 私有物理动作），再执行移位。
            ClrOperatorCode::NarrowShift(opcode) => {
                self.push_opcode(MsilOpcode::ConvI4);
                self.push_opcode(opcode);
            }
            // 布尔取反组合：`compare` 得到 0/1，再压 `ldc.i4.0` 并 `ceq` 判等取反。
            // 取反固定用 `ceq`，不能复用 `compare`（`cgt`/`clt` 与 0 比较会把
            // `le`/`ge` 误算成 `gt`/`lt`）。
            ClrOperatorCode::InvertCompare(compare) => {
                self.push_opcode(compare);
                self.push_opcode(MsilOpcode::LdcI4_0);
                self.push_opcode(MsilOpcode::Ceq);
            }
        }
        match output {
            Some((slot, result_kind)) => {
                if result_kind != ClrValueKind::of_operand_type(plan.result_type) {
                    return Err(format!(
                        "CLR 目标 operator `{mnemonic}` 结果槽载体与冻结 result_type 不符: function={} expected={:?} actual={result_kind:?}",
                        self.view.symbol, plan.result_type
                    ));
                }
                self.store_local(slot);
            }
            // CLR `pop` 与栈值宽度无关，允许按需丢弃 operator 结果。
            None => self.emit_pop(),
        }
        Ok(())
    }

    fn emit_operator_operands(
        &mut self,
        mnemonic: &str,
        expected_kind: ClrValueKind,
        plan: &TargetOperatorPlan<ClrOperatorCode>,
        arguments: &[ValkyrieMirOperand],
    ) -> Result<(), String> {
        match plan.recipe {
            TargetOperatorRecipe::Operands => self.emit_operator_arguments(mnemonic, expected_kind, arguments),
            TargetOperatorRecipe::Steps(steps) => {
                for step in steps {
                    match step {
                        TargetOperandStep::Arguments => self.emit_operator_arguments(mnemonic, expected_kind, arguments)?,
                        TargetOperandStep::IntegerConstant { operand_type, value } => {
                            let kind = ClrValueKind::of_operand_type(*operand_type);
                            if kind != expected_kind {
                                return Err(format!(
                                    "CLR 目标 operator `{mnemonic}` 的常量配方载体与冻结操作数载体不符: function={} expected={expected_kind:?} actual={kind:?}",
                                    self.view.symbol
                                ));
                            }
                            match kind {
                                ClrValueKind::Int32 => {
                                    self.push_integer_constant(i64::from(*value), MsilOpcode::LdcI4);
                                }
                                ClrValueKind::Int64 => {
                                    self.push_integer_constant(i64::from(*value), MsilOpcode::LdcI8);
                                }
                                ClrValueKind::Float32 | ClrValueKind::Float64 | ClrValueKind::Reference => {
                                    return Err(format!(
                                        "CLR 目标 operator `{mnemonic}` 冻结了非整数常量配方 {operand_type:?}: function={}",
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

    fn emit_operator_arguments(&mut self, mnemonic: &str, expected_kind: ClrValueKind, arguments: &[ValkyrieMirOperand]) -> Result<(), String> {
        for argument in arguments {
            self.emit_operand(argument, Some(expected_kind)).map_err(|error| {
                format!("CLR 目标 operator `{mnemonic}` 实参载体与冻结 operand_type 不符: function={} detail={error}", self.view.symbol)
            })?;
        }
        Ok(())
    }

    fn store_call_result(&mut self, method: &ClrMethodPlan, output: Option<(u16, ClrValueKind)>) -> Result<(), String> {
        if method.signature.return_type == MsilType::Void {
            if output.is_some() {
                return Err(format!("CLR 最小 lowering：void 调用 `{}` 不得绑定结果槽: function={}", method.name, self.view.symbol));
            }
            return Ok(());
        }
        let return_kind = ClrValueKind::of_msil_type(&method.signature.return_type)?;
        match output {
            Some((slot, kind)) => {
                if kind != return_kind {
                    return Err(format!(
                        "CLR 最小 lowering：调用 `{}` 结果载体与冻结返回签名不符: function={} expected={return_kind:?} actual={kind:?}",
                        method.name, self.view.symbol
                    ));
                }
                self.store_local(slot);
                Ok(())
            }
            None => {
                self.emit_pop();
                Ok(())
            }
        }
    }

    /// 完整类型身份 → 冻结布局身份（与旧公共路径 `layout_for_type` 同一合同）。
    fn layout_id_for_type(&self, type_id: TypeId, operation: &str) -> Result<LayoutId, String> {
        self.ctx
            .aggregate_layout_by_type()
            .get(&type_id)
            .copied()
            .ok_or_else(|| format!("CLR 最小 lowering：{operation} 缺少编译器绑定的布局身份: {type_id:?}: function={}", self.view.symbol))
    }

    fn value_type_id(&self, value: MirValueId) -> Result<TypeId, String> {
        self.function
            .value_types
            .get(&value)
            .copied()
            .ok_or_else(|| format!("CLR 最小 lowering：聚合值 `{value:?}` 缺少 Semantic MIR 类型事实: function={}", self.view.symbol))
    }

    fn layout_by_id(&self, layout_id: LayoutId) -> Result<AggregateLayout, String> {
        self.ctx
            .aggregate_layouts()
            .layouts
            .iter()
            .find(|layout| layout.id == layout_id)
            .cloned()
            .ok_or_else(|| format!("CLR 最小 lowering：布局身份 `{layout_id}` 未在计划中注册: function={}", self.view.symbol))
    }

    fn layout_by_nominal(&self, nominal: NominalInstanceId) -> Result<AggregateLayout, String> {
        let Some(layout_id) = self.ctx.aggregate_layout_by_nominal().get(&nominal).copied()
        else {
            return Err(format!("CLR 最小 lowering：nominal `{nominal:?}` 没有冻结的布局身份: function={}", self.view.symbol));
        };
        self.layout_by_id(layout_id)
    }

    fn layout_field_type(&self, layout: &AggregateLayout, slot: u32) -> Result<(MsilType, ClrValueKind), String> {
        let Some(field) = layout.fields.get(slot as usize)
        else {
            return Err(format!("CLR 最小 lowering：布局 `{}` 缺少字段槽 {slot}: function={}", layout.name, self.view.symbol));
        };
        let carrier = msil_type(self.ctx.aggregate_layouts(), &field.ty)?;
        let kind = ClrValueKind::of_msil_type(&carrier)?;
        Ok((carrier, kind))
    }

    fn field_location(&self, field: FieldId) -> Result<(AggregateLayout, u32), String> {
        let Some((layout_id, slot)) = self.ctx.aggregate_layout_by_field().get(&field).copied()
        else {
            return Err(format!("CLR 最小 lowering：字段身份 `{field:?}` 未登记布局索引: function={}", self.view.symbol));
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
                .ok_or_else(|| format!("CLR 最小 lowering：SSA 值 {value:?} 缺少冻结类型: function={}", self.view.symbol)),
            ValkyrieMirOperand::Constant(constant) => Ok(match constant {
                ValkyrieMirConstant::Int(_) => SemanticType::Integer32 { signed: true },
                ValkyrieMirConstant::Float64(_) => SemanticType::Float64,
                ValkyrieMirConstant::Bool(_) => SemanticType::Boolean,
                ValkyrieMirConstant::Utf8(_) | ValkyrieMirConstant::Utf16(_) => SemanticType::Utf8,
                other => {
                    return Err(format!("CLR 最小 lowering：常量 `{other:?}` 没有可装箱的冻结类型: function={}", self.view.symbol));
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
            .ok_or_else(|| format!("CLR 最小 lowering：SSA 值 {value:?} 缺少冻结类型: function={}", self.view.symbol))
    }

    fn reference_operand_slot(&self, operand: &ValkyrieMirOperand) -> Result<u16, String> {
        let ValkyrieMirOperand::Value(value) = operand
        else {
            return Err(format!("CLR 最小 lowering：只有 SSA 值可作为引用局部目标: function={} actual={operand:?}", self.view.symbol));
        };
        let (slot, kind) = self.value_slot(*value)?;
        if kind != ClrValueKind::Reference {
            return Err(format!("CLR 最小 lowering：聚合拷贝目标必须是引用载体: function={} actual={kind:?}", self.view.symbol));
        }
        Ok(slot)
    }

    fn store_operand(&mut self, operand: &ValkyrieMirOperand, kind: ClrValueKind) -> Result<(), String> {
        let ValkyrieMirOperand::Value(value) = operand
        else {
            return Err(format!("CLR 最小 lowering：只有 SSA 值可作为局部目标: function={} dest={operand:?}", self.view.symbol));
        };
        let (slot, actual) = self.value_slot(*value)?;
        if actual != kind {
            return Err(format!(
                "CLR 最小 lowering：目标 SSA 值 {value:?} 的槽载体 {actual:?} 与期望 {kind:?} 不符: function={}",
                self.view.symbol
            ));
        }
        self.store_local(slot);
        Ok(())
    }

    fn push_newobj(&mut self, type_name: &str) {
        self.instructions.push(msil_instruction(
            MsilOpcode::Newobj,
            Some(MsilInstructionOperand::Method(MsilMethodRef {
                owner: Some(type_name.to_string()),
                name: ".ctor".to_string(),
                signature: MsilMethodSignature::new_instance(MsilType::Void, Vec::new()),
            })),
        ));
    }

    fn push_type_opcode(&mut self, opcode: MsilOpcode, ty: &MsilType) {
        self.instructions.push(msil_instruction(opcode, Some(MsilInstructionOperand::Type(ty.to_string()))));
    }

    /// 数组元素加载：`ldelem.any` 必须携带元素类型 token，其它专用指令不带。
    fn push_array_load(&mut self, element: &MsilType) {
        let opcode = clr_array_load_opcode(element);
        if opcode == MsilOpcode::LdelemAny {
            self.push_type_opcode(opcode, element);
        }
        else {
            self.push_opcode(opcode);
        }
    }

    /// 与 [`Self::push_array_load`] 对称的元素写入。
    fn push_array_store(&mut self, element: &MsilType) {
        let opcode = clr_array_store_opcode(element);
        if opcode == MsilOpcode::StelemAny {
            self.push_type_opcode(opcode, element);
        }
        else {
            self.push_opcode(opcode);
        }
    }

    fn push_field_opcode(&mut self, opcode: MsilOpcode, owner: &str, name: &str) {
        self.instructions.push(msil_instruction(opcode, Some(MsilInstructionOperand::Field(owner.to_string(), name.to_string()))));
    }

    fn emit_layout_new(&mut self, layout: &AggregateLayout, output: (u16, ClrValueKind)) -> Result<(), String> {
        if output.1 != ClrValueKind::Reference {
            return Err(format!(
                "CLR 最小 lowering：布局 `{}` 的构造结果必须是引用槽: function={} actual={:?}",
                layout.name, self.view.symbol, output.1
            ));
        }
        self.push_newobj(&clr_layout_type_name(layout));
        self.store_local(output.0);
        Ok(())
    }

    /// 递归复制值语义聚合：目标槽得到独立对象，嵌套值聚合逐层新建。
    ///
    /// 引用语义字段按共享分类直接搬运；空嵌套字段保持 `null`。分类事实来自
    /// 共享 `value_copy` 模块，不在本后端按类型名或字段名特判。
    fn emit_value_aggregate_copy(&mut self, layout: &AggregateLayout, source_slot: u16, dest_slot: u16, depth: u32) -> Result<(), String> {
        if depth > MAX_VALUE_AGGREGATE_COPY_DEPTH {
            return Err(format!(
                "CLR 最小 lowering：值聚合复制嵌套 {depth} 层超过上限 {MAX_VALUE_AGGREGATE_COPY_DEPTH}: function={}",
                self.view.symbol
            ));
        }
        self.emit_layout_new(layout, (dest_slot, ClrValueKind::Reference))?;
        let type_name = clr_layout_type_name(layout);
        for slot in 0..layout.fields.len() {
            let field_name = clr_layout_field_name(slot);
            let Some(nested_id) = nested_value_aggregate_layout(self.ctx.aggregate_layouts(), &layout.fields[slot].ty)
            else {
                self.load_local(dest_slot);
                self.load_local(source_slot);
                self.push_field_opcode(MsilOpcode::Ldfld, &type_name, &field_name);
                self.push_field_opcode(MsilOpcode::Stfld, &type_name, &field_name);
                continue;
            };
            let nested_layout = self.layout_by_id(nested_id)?;
            let nested_source = self.copy_scratch_slot(depth, 0)?;
            let nested_dest = self.copy_scratch_slot(depth, 1)?;
            self.load_local(source_slot);
            self.push_field_opcode(MsilOpcode::Ldfld, &type_name, &field_name);
            self.store_local(nested_source);
            let null_label = self.fresh_copy_label("null");
            let end_label = self.fresh_copy_label("end");
            self.load_local(nested_source);
            self.push_branch(MsilOpcode::Brfalse, null_label.clone());
            self.emit_value_aggregate_copy(&nested_layout, nested_source, nested_dest, depth + 1)?;
            self.load_local(dest_slot);
            self.load_local(nested_dest);
            self.push_field_opcode(MsilOpcode::Stfld, &type_name, &field_name);
            self.push_branch(MsilOpcode::Br, end_label.clone());
            self.push_label(null_label);
            self.load_local(dest_slot);
            self.push_opcode(MsilOpcode::Ldnull);
            self.push_field_opcode(MsilOpcode::Stfld, &type_name, &field_name);
            self.push_label(end_label);
        }
        Ok(())
    }

    fn copy_scratch_slot(&self, depth: u32, offset: usize) -> Result<u16, String> {
        let index = (depth as usize).saturating_mul(2).saturating_add(offset);
        self.copy_scratch
            .get(index)
            .copied()
            .ok_or_else(|| format!("CLR 最小 lowering：值聚合复制临时槽 {index} 未预留: function={}", self.view.symbol))
    }

    fn fresh_copy_label(&mut self, suffix: &str) -> String {
        self.label_ordinal = self.label_ordinal.saturating_add(1);
        format!("copy_{suffix}_{}", self.label_ordinal)
    }

    fn push_label(&mut self, label: String) {
        self.instructions.push(MsilInstruction { label: Some(label), opcode: MsilOpcode::Nop, operand: None });
    }

    fn emit_layout_field_write(&mut self, layout: &AggregateLayout, output_slot: u16, slot: usize, value: &ValkyrieMirOperand) -> Result<(), String> {
        let (_, kind) = self.layout_field_type(layout, u32::try_from(slot).unwrap_or(u32::MAX))?;
        self.load_local(output_slot);
        self.emit_operand(value, Some(kind))?;
        self.push_field_opcode(MsilOpcode::Stfld, &clr_layout_type_name(layout), &clr_layout_field_name(slot));
        Ok(())
    }

    /// 发射一个可能已处于标量槽的 payload/元素，并按目标类型按需装箱。
    fn emit_boxed_operand(&mut self, operand: &ValkyrieMirOperand, ty: &SemanticType) -> Result<(), String> {
        let carrier = msil_type(self.ctx.aggregate_layouts(), ty).map_err(|error| format!("{error}: function={}", self.view.symbol))?;
        match &carrier {
            MsilType::String | MsilType::Object | MsilType::Named(_) | MsilType::SzArray(_) => {
                self.emit_operand(operand, Some(ClrValueKind::Reference))
            }
            MsilType::Void => Err(format!("CLR 最小 lowering：`{ty}` 是 void 形态，不能作为需要引用的值: function={}", self.view.symbol)),
            scalar => {
                let kind = ClrValueKind::of_msil_type(scalar)?;
                self.emit_operand(operand, Some(kind))?;
                self.push_type_opcode(MsilOpcode::Box, scalar);
                Ok(())
            }
        }
    }

    /// 发射数组元素值：标量进入引用元素槽时按槽的精确保鲜载体装箱。
    fn emit_array_element_value(&mut self, operand: &ValkyrieMirOperand, element: &SemanticType) -> Result<(), String> {
        let element_carrier = msil_array_element_type(self.ctx.aggregate_layouts(), element)
            .map_err(|error| format!("{error}: function={}", self.view.symbol))?;
        let element_kind = ClrValueKind::of_msil_type(&element_carrier)?;
        if element_kind == ClrValueKind::Reference {
            let operand_ty = self.operand_type(operand)?;
            let operand_carrier = msil_type(self.ctx.aggregate_layouts(), &operand_ty).map_err(|error| format!("{error}: function={}", self.view.symbol))?;
            let operand_kind = ClrValueKind::of_msil_type(&operand_carrier)?;
            if operand_kind != ClrValueKind::Reference {
                self.emit_operand(operand, Some(operand_kind))?;
                self.push_type_opcode(MsilOpcode::Box, &operand_carrier);
                return Ok(());
            }
        }
        self.emit_operand(operand, Some(element_kind))
    }

    fn emit_sum_new(
        &mut self,
        instruction: &ValkyrieMirInstruction,
        nominal: NominalInstanceId,
        variant: VariantId,
        payload: Option<&MirValueId>,
    ) -> Result<(), String> {
        let Some(sum) = self.ctx.sum_representations().get(&nominal).cloned()
        else {
            return Err(format!(
                "CLR 最小 lowering：sum nominal `{nominal:?}` 没有冻结的 `SumRepresentation`: function={}",
                self.view.symbol
            ));
        };
        let Some(variant_rep) = sum.variants.get(&variant).copied()
        else {
            return Err(format!(
                "CLR 最小 lowering：variant `{variant:?}` 不属于 sum nominal `{nominal:?}`: function={}",
                self.view.symbol
            ));
        };
        if payload.is_some() != variant_rep.payload_type.is_some() {
            return Err(format!(
                "CLR 最小 lowering：variant `{variant:?}` 的 payload 操作数与冻结 payload 类型不一致: function={} nominal={nominal:?} payload={} expected={}",
                self.view.symbol,
                payload.is_some(),
                variant_rep.payload_type.is_some()
            ));
        }
        let tag = i32::try_from(variant_rep.tag)
            .map_err(|_| format!("CLR 最小 lowering：variant `{variant:?}` 的 tag 超出 `int32` 载体: function={}", self.view.symbol))?;
        let output = self.require_output(instruction)?;
        if output.1 != ClrValueKind::Reference {
            return Err(format!("CLR 最小 lowering：sum 构造结果必须是引用槽: function={} actual={:?}", self.view.symbol, output.1));
        }
        let type_name = clr_sum_type_name(nominal);
        self.push_newobj(&type_name);
        self.store_local(output.0);
        self.load_local(output.0);
        self.push_integer_constant(i64::from(tag), MsilOpcode::LdcI4);
        self.push_field_opcode(MsilOpcode::Stfld, &type_name, CLR_SUM_TAG_FIELD);
        if let Some(payload) = payload {
            let payload_operand = ValkyrieMirOperand::Value(*payload);
            let payload_type = self.operand_type(&payload_operand)?;
            self.load_local(output.0);
            self.emit_boxed_operand(&payload_operand, &payload_type)?;
            self.push_field_opcode(MsilOpcode::Stfld, &type_name, CLR_SUM_PAYLOAD_FIELD);
        }
        Ok(())
    }

    fn emit_sum_variant_is(
        &mut self,
        instruction: &ValkyrieMirInstruction,
        nominal: NominalInstanceId,
        variant: VariantId,
        object: &MirValueId,
    ) -> Result<(), String> {
        let Some(sum) = self.ctx.sum_representations().get(&nominal).cloned()
        else {
            return Err(format!(
                "CLR 最小 lowering：sum nominal `{nominal:?}` 没有冻结的 `SumRepresentation`: function={}",
                self.view.symbol
            ));
        };
        let Some(variant_rep) = sum.variants.get(&variant).copied()
        else {
            return Err(format!(
                "CLR 最小 lowering：variant `{variant:?}` 不属于 sum nominal `{nominal:?}`: function={}",
                self.view.symbol
            ));
        };
        let tag = i32::try_from(variant_rep.tag)
            .map_err(|_| format!("CLR 最小 lowering：variant `{variant:?}` 的 tag 超出 `int32` 载体: function={}", self.view.symbol))?;
        let type_name = clr_sum_type_name(nominal);
        self.emit_operand(&ValkyrieMirOperand::Value(*object), Some(ClrValueKind::Reference))?;
        self.push_field_opcode(MsilOpcode::Ldfld, &type_name, CLR_SUM_TAG_FIELD);
        self.push_integer_constant(i64::from(tag), MsilOpcode::LdcI4);
        self.push_opcode(MsilOpcode::Ceq);
        self.store_or_pop_boolean(instruction)
    }

    fn emit_sum_payload_get(
        &mut self,
        instruction: &ValkyrieMirInstruction,
        nominal: NominalInstanceId,
        variant: VariantId,
        object: &MirValueId,
    ) -> Result<(), String> {
        let Some(sum) = self.ctx.sum_representations().get(&nominal).cloned()
        else {
            return Err(format!(
                "CLR 最小 lowering：sum nominal `{nominal:?}` 没有冻结的 `SumRepresentation`: function={}",
                self.view.symbol
            ));
        };
        let Some(variant_rep) = sum.variants.get(&variant).copied()
        else {
            return Err(format!(
                "CLR 最小 lowering：variant `{variant:?}` 不属于 sum nominal `{nominal:?}`: function={}",
                self.view.symbol
            ));
        };
        if variant_rep.payload_type.is_none() {
            return Err(format!(
                "CLR 最小 lowering：variant `{variant:?}` 没有 payload，不能取载荷: function={} nominal={nominal:?}",
                self.view.symbol
            ));
        }
        let type_name = clr_sum_type_name(nominal);
        self.emit_operand(&ValkyrieMirOperand::Value(*object), Some(ClrValueKind::Reference))?;
        self.push_field_opcode(MsilOpcode::Ldfld, &type_name, CLR_SUM_PAYLOAD_FIELD);

        let Some((slot, kind)) = self.output_of(instruction)?
        else {
            self.emit_pop();
            return Ok(());
        };
        let Some(target_type) = self.output_type(instruction)?
        else {
            return Err(format!("CLR 最小 lowering：sum payload 结果缺少冻结类型: function={}", self.view.symbol));
        };
        let target_carrier = msil_type(self.ctx.aggregate_layouts(), &target_type)?;
        let target_kind = ClrValueKind::of_msil_type(&target_carrier)?;
        if target_kind != kind {
            return Err(format!(
                "CLR 最小 lowering：sum payload 目标载体 {target_kind:?} 与结果槽 {kind:?} 不符: function={}",
                self.view.symbol
            ));
        }
        if target_kind != ClrValueKind::Reference {
            self.push_type_opcode(MsilOpcode::UnboxAny, &target_carrier);
        }
        else if matches!(target_carrier, MsilType::Named(_) | MsilType::SzArray(_)) {
            self.push_type_opcode(MsilOpcode::Castclass, &target_carrier);
        }
        self.store_local(slot);
        Ok(())
    }

    fn store_or_pop_boolean(&mut self, instruction: &ValkyrieMirInstruction) -> Result<(), String> {
        match self.output_of(instruction)? {
            Some((slot, kind)) => {
                if kind != ClrValueKind::Int32 {
                    return Err(format!("CLR 最小 lowering：布尔结果槽载体必须是 `int32`: function={} actual={kind:?}", self.view.symbol));
                }
                self.store_local(slot);
                Ok(())
            }
            None => {
                self.emit_pop();
                Ok(())
            }
        }
    }

    fn emit_array_from_elements(&mut self, instruction: &ValkyrieMirInstruction, array_type: &SemanticType, elements: &[ValkyrieMirOperand]) -> Result<(), String> {
        let element = clr_array_element_type(array_type).map_err(|error| format!("{error}: function={}", self.view.symbol))?.clone();
        let carrier = msil_array_element_type(self.ctx.aggregate_layouts(), &element).map_err(|error| format!("{error}: function={}", self.view.symbol))?;
        let length = i32::try_from(elements.len())
            .map_err(|_| format!("CLR 最小 lowering：数组元素数量超出 `int32` 载体: function={}", self.view.symbol))?;
        self.push_integer_constant(i64::from(length), MsilOpcode::LdcI4);
        self.push_type_opcode(MsilOpcode::Newarr, &carrier);
        for (index, value) in elements.iter().enumerate() {
            let index = i32::try_from(index)
                .map_err(|_| format!("CLR 最小 lowering：数组元素下标超出 `int32` 载体: function={}", self.view.symbol))?;
            self.push_opcode(MsilOpcode::Dup);
            self.push_integer_constant(i64::from(index), MsilOpcode::LdcI4);
            self.emit_array_element_value(value, &element)?;
            self.push_array_store(&carrier);
        }
        match self.output_of(instruction)? {
            Some((slot, kind)) => {
                if kind != ClrValueKind::Reference {
                    return Err(format!("CLR 最小 lowering：数组结果必须是引用槽: function={} actual={kind:?}", self.view.symbol));
                }
                self.store_local(slot);
                Ok(())
            }
            None => {
                self.emit_pop();
                Ok(())
            }
        }
    }

    fn emit_array_new(&mut self, instruction: &ValkyrieMirInstruction, array_type: &SemanticType, length: &ValkyrieMirOperand) -> Result<(), String> {
        let element = clr_array_element_type(array_type).map_err(|error| format!("{error}: function={}", self.view.symbol))?.clone();
        let carrier = msil_array_element_type(self.ctx.aggregate_layouts(), &element).map_err(|error| format!("{error}: function={}", self.view.symbol))?;
        self.emit_operand(length, Some(ClrValueKind::Int32))?;
        self.push_type_opcode(MsilOpcode::Newarr, &carrier);
        match self.output_of(instruction)? {
            Some((slot, kind)) => {
                if kind != ClrValueKind::Reference {
                    return Err(format!("CLR 最小 lowering：数组结果必须是引用槽: function={} actual={kind:?}", self.view.symbol));
                }
                self.store_local(slot);
                Ok(())
            }
            None => {
                self.emit_pop();
                Ok(())
            }
        }
    }

    fn emit_array_get(&mut self, instruction: &ValkyrieMirInstruction, array: &ValkyrieMirOperand, index: &ValkyrieMirOperand) -> Result<(), String> {
        let array_type = self.operand_type(array)?;
        let element = clr_array_element_type(&array_type).map_err(|error| format!("{error}: function={}", self.view.symbol))?.clone();
        let carrier = msil_array_element_type(self.ctx.aggregate_layouts(), &element).map_err(|error| format!("{error}: function={}", self.view.symbol))?;
        let carrier_kind = ClrValueKind::of_msil_type(&carrier)?;
        self.emit_operand(array, Some(ClrValueKind::Reference))?;
        self.emit_operand(index, Some(ClrValueKind::Int32))?;
        self.push_array_load(&carrier);
        let Some((slot, kind)) = self.output_of(instruction)?
        else {
            self.emit_pop();
            return Ok(());
        };
        let Some(target_type) = self.output_type(instruction)?
        else {
            return Err(format!("CLR 最小 lowering：数组元素结果缺少冻结类型: function={}", self.view.symbol));
        };
        let target_carrier = msil_type(self.ctx.aggregate_layouts(), &target_type)?;
        let target_kind = ClrValueKind::of_msil_type(&target_carrier)?;
        if target_kind != kind {
            return Err(format!(
                "CLR 最小 lowering：数组元素目标载体 {target_kind:?} 与结果槽 {kind:?} 不符: function={}",
                self.view.symbol
            ));
        }
        if carrier_kind == ClrValueKind::Reference && target_kind != ClrValueKind::Reference {
            self.push_type_opcode(MsilOpcode::UnboxAny, &target_carrier);
        }
        self.store_local(slot);
        Ok(())
    }

    fn emit_array_set(&mut self, array: &ValkyrieMirOperand, index: &ValkyrieMirOperand, value: &ValkyrieMirOperand) -> Result<(), String> {
        let array_type = self.operand_type(array)?;
        let element = clr_array_element_type(&array_type).map_err(|error| format!("{error}: function={}", self.view.symbol))?.clone();
        let carrier = msil_array_element_type(self.ctx.aggregate_layouts(), &element).map_err(|error| format!("{error}: function={}", self.view.symbol))?;
        self.emit_operand(array, Some(ClrValueKind::Reference))?;
        self.emit_operand(index, Some(ClrValueKind::Int32))?;
        self.emit_array_element_value(value, &element)?;
        self.push_array_store(&carrier);
        Ok(())
    }

    fn emit_operand(&mut self, operand: &ValkyrieMirOperand, expected: Option<ClrValueKind>) -> Result<(), String> {
        match operand {
            ValkyrieMirOperand::Value(value) => {
                let (slot, kind) = self.value_slot(*value)?;
                if let Some(expected) = expected
                    && expected != kind
                {
                    return Err(format!(
                        "CLR 最小 lowering：SSA 值 {value:?} 的槽载体 {kind:?} 与期望 {expected:?} 不符: function={}",
                        self.view.symbol
                    ));
                }
                self.load_local(slot);
                Ok(())
            }
            ValkyrieMirOperand::Constant(constant) => self.emit_constant(constant, expected),
        }
    }

    fn emit_constant(&mut self, constant: &ValkyrieMirConstant, expected: Option<ClrValueKind>) -> Result<(), String> {
        match constant {
            ValkyrieMirConstant::Int(value) => match expected {
                Some(ClrValueKind::Int64) => {
                    self.push_integer_constant(*value, MsilOpcode::LdcI8);
                    Ok(())
                }
                Some(ClrValueKind::Int32) | None => {
                    let narrowed = i32::try_from(*value).map_err(|_| {
                        format!(
                            "CLR 最小 lowering：整数常量 {value} 超出 `int32` 载体，且目标槽位未声明 `int64`: function={}",
                            self.view.symbol
                        )
                    })?;
                    self.push_integer_constant(i64::from(narrowed), MsilOpcode::LdcI4);
                    Ok(())
                }
                Some(other) => Err(format!("CLR 最小 lowering：整数常量不能进入 {other:?} 槽: function={}", self.view.symbol)),
            },
            ValkyrieMirConstant::Float64(bits) => match expected {
                Some(ClrValueKind::Float64) => {
                    let value = f64::from_bits(*bits);
                    if !value.is_finite() {
                        return Err(format!(
                            "CLR 最小 lowering：非有限 f64 常量 `{value}` 缺少文本载体，拒绝猜测编码: function={}",
                            self.view.symbol
                        ));
                    }
                    self.instructions.push(MsilInstruction {
                        label: None,
                        opcode: MsilOpcode::LdcR8,
                        operand: Some(MsilInstructionOperand::Float(format!("{value:?}"))),
                    });
                    Ok(())
                }
                Some(ClrValueKind::Float32) => Err(format!(
                    "CLR 最小 lowering 不支持把 f64 常量压入 `float32` 槽：当前 MSIL 模型没有 `ldc.r4`，function={}",
                    self.view.symbol
                )),
                Some(other) => Err(format!("CLR 最小 lowering：f64 常量不能进入 {other:?} 槽: function={}", self.view.symbol)),
                None => Err(format!("CLR 最小 lowering：f64 常量缺少明确的 `float64` 结果槽，拒绝猜测载体: function={}", self.view.symbol)),
            },
            ValkyrieMirConstant::Bool(value) => {
                if !matches!(expected, Some(ClrValueKind::Int32) | None) {
                    return Err(format!("CLR 最小 lowering：布尔常量不能进入 {expected:?} 槽: function={}", self.view.symbol));
                }
                self.push_integer_constant(i64::from(*value), MsilOpcode::LdcI4);
                Ok(())
            }
            ValkyrieMirConstant::Utf8(value) | ValkyrieMirConstant::Utf16(value) => match expected {
                Some(ClrValueKind::Reference) | None => {
                    self.instructions.push(MsilInstruction {
                        label: None,
                        opcode: MsilOpcode::Ldstr,
                        operand: Some(MsilInstructionOperand::StringLiteral(value.clone())),
                    });
                    Ok(())
                }
                Some(other) => Err(format!("CLR 最小 lowering：字符串常量不能进入 {other:?} 槽: function={}", self.view.symbol)),
            },
            // `T?` 的空值就是 CLR `null` 引用；只有引用槽能承载，可空标量走 sum 表示。
            ValkyrieMirConstant::Null => match expected {
                Some(ClrValueKind::Reference) | None => {
                    self.push_opcode(MsilOpcode::Ldnull);
                    Ok(())
                }
                Some(other) => Err(format!(
                    "CLR 最小 lowering：空值常量不能进入 {other:?} 槽（可空标量需要 sum 表示，不得静默装箱）: function={}",
                    self.view.symbol
                )),
            },
            other => Err(format!("CLR 最小 lowering 尚不支持常量 `{other:?}`：缺少 CLR 常量载体合同，function={}", self.view.symbol)),
        }
    }

    fn push_integer_constant(&mut self, value: i64, opcode: MsilOpcode) {
        self.instructions.push(msil_instruction(opcode, Some(MsilInstructionOperand::Integer(value))));
    }

    fn load_local(&mut self, slot: u16) {
        self.instructions.push(msil_instruction(MsilOpcode::Ldloc, Some(MsilInstructionOperand::Integer(i64::from(slot)))));
    }

    fn store_local(&mut self, slot: u16) {
        self.instructions.push(msil_instruction(MsilOpcode::Stloc, Some(MsilInstructionOperand::Integer(i64::from(slot)))));
    }

    /// 入口形参加载：`ldarg.N`（与旧公共路径同一编码）。
    fn push_argument(&mut self, index: usize) {
        self.instructions.push(msil_instruction(MsilOpcode::Ldarg, Some(MsilInstructionOperand::Integer(index as i64))));
    }

    fn push_opcode(&mut self, opcode: MsilOpcode) {
        self.instructions.push(opcode_instruction(opcode));
    }

    fn push_branch(&mut self, opcode: MsilOpcode, target: String) {
        self.instructions.push(branch_instruction(opcode, target));
    }

    fn emit_pop(&mut self) {
        self.push_opcode(MsilOpcode::Pop);
    }

    /// 操作数在计划中冻结的语义类型（`Cast` 的转换语义必须由此决定）。
    fn operand_semantic_type(&self, operand: &ValkyrieMirOperand) -> Result<SemanticType, String> {
        match operand {
            ValkyrieMirOperand::Value(value) => self
                .view
                .value_types
                .get(value)
                .cloned()
                .ok_or_else(|| format!("CLR `Cast` 源 SSA 值 {value:?} 缺少冻结语义类型: function={}", self.view.symbol)),
            ValkyrieMirOperand::Constant(constant) => match constant {
                ValkyrieMirConstant::Int(_) => Ok(SemanticType::Integer64 { signed: true }),
                ValkyrieMirConstant::Bool(_) => Ok(SemanticType::Boolean),
                ValkyrieMirConstant::Float64(_) => Ok(SemanticType::Float64),
                ValkyrieMirConstant::Utf8(_) => Ok(SemanticType::Utf8),
                ValkyrieMirConstant::Utf16(_) => Ok(SemanticType::Utf16),
                ValkyrieMirConstant::Unit => Ok(SemanticType::Unit),
                ValkyrieMirConstant::Null => Err(format!("CLR `Cast` 的空值操作数没有源类型: function={}", self.view.symbol)),
            },
        }
    }

    /// 语言层 `as` 的整数表示转换（`int32`/`int64` 求值栈）。
    fn emit_cast_conversion(
        &mut self,
        source: &SemanticType,
        target: &SemanticType,
        source_kind: ClrValueKind,
        target_kind: ClrValueKind,
    ) -> Result<(), String> {
        if source == target {
            return Ok(());
        }
        if source.is_float() || target.is_float() {
            return self.emit_float_cast_conversion(source, target, source_kind, target_kind);
        }
        if source_kind == ClrValueKind::Reference || target_kind == ClrValueKind::Reference {
            return Err(format!("CLR 最小 lowering：`{source}` → `{target}` 的引用 cast 缺少目标合同: function={}", self.view.symbol));
        }
        let (Some(source_shape), Some(target_shape)) = (msil_int_shape(source), msil_int_shape(target))
        else {
            return Err(format!("CLR 最小 lowering：`{source}` → `{target}` 的 cast 缺少整数表示合同: function={}", self.view.symbol));
        };
        // 先把源在 int32 载体上的越界高位裁掉（8/16 位必须显式符号/零扩展）。
        if source_kind == ClrValueKind::Int32 {
            self.push_opcode(msil_int_normalize_opcode(source_shape));
        }
        // 再按目标位宽/符号性转换（int64 目标用 `conv.i8`/`conv.u8`）。
        self.push_opcode(msil_int_normalize_opcode(target_shape));
        Ok(())
    }

    /// 浮点 `as` 转换的 lowering：ECMA-335 的 `conv.r4`/`conv.r8`/`conv.i1`…
    /// `conv.u8` 是数值栈载体之间的正式转换指令，舍入行为由运行时合同承诺。
    ///
    /// `int32` 系源先按源形状归一（8/16 位载体携带越界高位）；浮点→`int32` 系
    /// 结果由目标形状的 conv 指令一步完成（ECMA 允许浮点源直接 `conv.i1` 等）。
    /// `int64` 与浮点之间是单条 `conv.r4`/`conv.r8`/`conv.i8`。非数值载体
    /// （bool/reference）保持失败闭合，不做隐式转换。
    fn emit_float_cast_conversion(
        &mut self,
        source: &SemanticType,
        target: &SemanticType,
        source_kind: ClrValueKind,
        target_kind: ClrValueKind,
    ) -> Result<(), String> {
        let step = match (source, target) {
            (SemanticType::Float32, SemanticType::Float64) => Some(MsilOpcode::ConvR8),
            (SemanticType::Float64, SemanticType::Float32) => Some(MsilOpcode::ConvR4),
            (SemanticType::Float32, SemanticType::Integer64 { .. }) => Some(MsilOpcode::ConvI8),
            (SemanticType::Float64, SemanticType::Integer64 { .. }) => Some(MsilOpcode::ConvI8),
            (SemanticType::Integer64 { .. }, SemanticType::Float32) => Some(MsilOpcode::ConvR4),
            (SemanticType::Integer64 { .. }, SemanticType::Float64) => Some(MsilOpcode::ConvR8),
            _ => None,
        };
        if let Some(opcode) = step {
            self.push_opcode(opcode);
            return Ok(());
        }
        if source.is_float() {
            // 浮点 → int32 系：目标形状的 conv 指令一步完成（int64 已在上面单步）。
            if !matches!(target_kind, ClrValueKind::Int32) {
                return Err(format!("CLR 最小 lowering：`{source}` → `{target}` 的浮点 cast 目标没有整型载体: function={}", self.view.symbol));
            }
            let Some(shape) = msil_int_shape(target) else {
                return Err(format!("CLR 最小 lowering：`{source}` → `{target}` 的浮点 cast 目标没有可归一的整型形状: function={}", self.view.symbol));
            };
            self.push_opcode(msil_int_normalize_opcode(shape));
            return Ok(());
        }
        // int32 系 → 浮点：源归一后 conv.r4/conv.r8（int64 源已在上面单步）。
        if !matches!(source_kind, ClrValueKind::Int32) {
            return Err(format!("CLR 最小 lowering：`{source}` → `{target}` 的浮点 cast 源没有数值载体: function={}", self.view.symbol));
        }
        let Some(shape) = msil_int_shape(source) else {
            return Err(format!("CLR 最小 lowering：`{source}` → `{target}` 的浮点 cast 源没有可归一的整型形状: function={}", self.view.symbol));
        };
        self.push_opcode(msil_int_normalize_opcode(shape));
        self.push_opcode(if matches!(target, SemanticType::Float32) { MsilOpcode::ConvR4 } else { MsilOpcode::ConvR8 });
        Ok(())
    }

    fn output_of(&self, instruction: &ValkyrieMirInstruction) -> Result<Option<(u16, ClrValueKind)>, String> {
        match SlotPlan::instruction_output(instruction) {
            None => Ok(None),
            Some(value) => self.value_slot(value).map(Some),
        }
    }

    fn require_output(&self, instruction: &ValkyrieMirInstruction) -> Result<(u16, ClrValueKind), String> {
        self.output_of(instruction)?
            .ok_or_else(|| format!("CLR 最小 lowering：指令缺少 SSA 结果槽，无法确定局部载体: function={}", self.view.symbol))
    }

    fn value_slot(&self, value: MirValueId) -> Result<(u16, ClrValueKind), String> {
        let local = *self
            .slots
            .value_locals
            .get(&value)
            .ok_or_else(|| format!("CLR 最小 lowering：SSA 值 {value:?} 缺少规划的局部槽: function={}", self.view.symbol))?;
        Ok((local, self.local_kind(local)?))
    }

    fn local_kind(&self, local: u16) -> Result<ClrValueKind, String> {
        let ty = self
            .slots
            .local_types
            .get(local as usize)
            .ok_or_else(|| format!("CLR 最小 lowering：局部槽 {local} 超出规划表: function={}", self.view.symbol))?;
        ClrValueKind::of_msil_type(&msil_parameter_type(self.ctx.aggregate_layouts(), ty)?)
    }
}

/// MIR 操作的稳定诊断名（不依赖 Debug 输出细节）。
fn mir_operation_name(operation: &ValkyrieMirOperation) -> &'static str {
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
