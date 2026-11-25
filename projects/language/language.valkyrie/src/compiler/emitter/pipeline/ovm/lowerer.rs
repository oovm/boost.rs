//! OVM lane 的函数级 lowering：MIR → 目标私有 `.odin` 字节码。
//!
//! 本模块直读 `ValkyrieMirFunction` 的块/指令/终结符，逐操作映射为 OVM 头码序列
//! （经 emitter 的 `BytecodeEmitter`/`encode_odin_function` 目标私有编码原语），
//! 不经过任何跨目标共享程序载荷（旧路径的 submission 解释阶段已删除）。
//! 旧公共路径只是把 MIR 直投影成公共 IR 再解释，本文件把
//! 两段映射合成一段，语义逐操作对齐（含入口槽合同一次结算、外部调用按冻结签名
//! 校验、文本载体逐值冻结、sum 物理布局 `[tag, payload]` 等审查点名合同）。

use std::collections::BTreeMap;

use compiler_emitter::emitter_backend_vm::{
    BytecodeEmitter, OvmIntShape, OvmOperatorBinding, OvmOperatorCode, ResolvedAggregateLayout, ResolvedLayoutPlan, TEXT_FROM_BYTES_IMPORT,
    TEXT_TO_BYTES_IMPORT, encode_odin_function, ovm_host_import_target, ovm_int_shape, select_unique_target,
};
use compiler_emitter::target_operator::{TargetOperandStep, TargetOperandType, TargetOperatorRecipe};
use compiler_emitter::ProgramTextRepresentation;
use compiler_target::{ExternalImportLink, NamePath};
use compiler_types::{FieldId, ItemInstanceId, LayoutId, SemanticType};
use compiler_types::semantic_ids::{MirValueId, NominalInstanceId, TypeId, VariantId};
use data_binary_odin::{IntrinsicOpcode, OdinConstant, OdinHeadCode, OdinLayout};

use crate::compiler::emitter::pipeline::lane::FunctionView;
use crate::compiler::emitter::pipeline::lane::LaneContext;
use crate::compiler::emitter::pipeline::target_plan::{AdapterStage, TargetPlanError};
use crate::compiler::valkyrie_mir::{
    ValkyrieMirBlock, ValkyrieMirBlockId, ValkyrieMirCallee, ValkyrieMirConstant, ValkyrieMirEffectKind, ValkyrieMirFunction,
    ValkyrieMirInstruction, ValkyrieMirOperand, ValkyrieMirOperation, ValkyrieMirTerminator,
};

/// `utf8` 槽在 OVM 上的物理载体事实。
///
/// 每个 `utf8` SSA 值的载体由 [`freeze_text_representations`] 在发射前逐行
/// 冻结为显式计划事实：托管布局、入口形参槽、字面量 ABI、嵌套槽目标合同与宿主
/// 边界各归各的规则；分类不到的值是 `missing` 事实——发射前结构化错误，绝不
/// 解释为 raw。边界发射只读冻结表，运行期不看 `Value` 的实际类型兜底。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum TextCarrier {
    /// 宿主 ABI raw 串（`Value::String`）。
    Raw,
    /// 托管文本对象（`Value::Object`），携带计划冻结的布局身份。
    Managed(LayoutId),
}

/// 文本边界目标槽的冻结表示事实。
///
/// `Raw` 与 `Managed` 都是显式声明：嵌套槽（字段/元素/载荷）与宿主边界由目标
/// 合同固定 raw 串（与 `ovm_abi::Projection::project_text` 的 `TextSlot::Nested`
/// 同源）；托管槽携带计划冻结的布局身份。「没有事实」不允许到达发射，由
/// [`freeze_text_representations`] 在发射前拒绝。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TextSlotTarget {
    /// 目标槽是宿主 raw 串：嵌套槽目标合同 / 宿主导入实参 / 程序级显式 raw 计划事实的形参槽。
    Raw,
    /// 目标槽是托管文本对象，布局身份由计划冻结。
    Managed(LayoutId),
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
        let local = u16::try_from(self.local_types.len()).expect("OVM local 数量超出 u16");
        self.local_types.push(ty);
        local
    }

    /// 从 MIR 函数与冻结类型视图结算槽位表。
    pub(crate) fn plan(function: &ValkyrieMirFunction, view: &FunctionView) -> Result<Self, String> {
        let mut plan = Self::default();
        let entry = function.blocks.get(&function.entry).ok_or_else(|| {
            format!("OVM 入口槽合同破坏：symbol=`{}` 声明了入口块 {}，但块表中不存在该入口块", view.symbol, function.entry.0)
        })?;
        for (value, _) in &entry.parameters {
            let ty = view.value_types.get(value).cloned().ok_or_else(|| format!("OVM 入口形参 %{} 缺少冻结类型（symbol=`{}`）", value.index(), view.symbol))?;
            let local = plan.alloc(ty);
            plan.value_locals.insert(*value, local);
        }
        for block in function.blocks.values() {
            if block.id == function.entry {
                continue;
            }
            for (value, _) in &block.parameters {
                let ty = view.value_types.get(value).cloned().ok_or_else(|| format!("OVM 块形参 %{} 缺少冻结类型（symbol=`{}`）", value.index(), view.symbol))?;
                let local = plan.alloc(ty);
                plan.value_locals.insert(*value, local);
            }
        }
        for block in function.blocks.values() {
            for instruction in &block.instructions {
                for (index, output) in instruction.results.iter().enumerate() {
                    let ty = view
                        .value_types
                        .get(output)
                        .cloned()
                        .ok_or_else(|| format!("OVM 指令结果 %{} 缺少冻结类型（symbol=`{}`）", output.index(), view.symbol))?;
                    let local = plan.alloc(ty);
                    if index == 0 {
                        plan.value_locals.insert(*output, local);
                    }
                    else {
                        plan.value_locals.insert(*output, local);
                    }
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

    fn value_local(&self, value: &MirValueId) -> Option<u16> {
        self.value_locals.get(value).copied()
    }
}

/// 数据流标准逆后序（RPO）：块发射顺序与旧公共路径同源。
///
/// 顶点是 MIR 块身份（`u32`）；悬空跳转目标保留为无子顶点（发射期跳过不存在的块，
/// 与旧实现一致）。
pub(crate) fn collect_reachable_blocks(function: &ValkyrieMirFunction) -> Vec<u32> {
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

    boost_math::graph_theory::reverse_postorder(&graph, &function.entry.0).expect("the entry is a vertex of the graph").into_iter().collect()
}

/// 入口形参槽的冻结布局身份表。
///
/// 这是「导出槽文本表示」与 lowering 入口槽表的唯一派生点，OVM ABI 合同与
/// 字节码发射都从这里取事实，禁止任何一方另写一份回落，否则合同与运行时实际
/// 布局会分叉。
///
/// 事实来源与旧公共路径完全一致：
/// 1. SSA 值类型（`TypeId`）→ `aggregate_layout_by_type`；
/// 2. 折叠后的槽类型 → `aggregate_layout_by_semantic_type`。
///
/// 两处都没有 = 目标计划没有为该槽冻结托管布局；`None` 只有在程序级表示计划
/// 显式声明 raw 时才是合法 raw 宿主串事实，否则由文本边界发射与 ABI 投影失败闭合。
pub(crate) fn frozen_entry_parameter_layouts(ctx: &LaneContext, function: &ValkyrieMirFunction, view: &FunctionView, slots: &SlotPlan) -> Vec<Option<LayoutId>> {
    let entry = function.blocks.get(&function.entry);
    entry
        .map(|block| {
            block
                .parameters
                .iter()
                .map(|(value, type_id)| {
                    ctx.aggregate_layout_by_type().get(type_id).copied().or_else(|| {
                        slots
                            .value_local(value)
                            .and_then(|local| slots.local_types.get(usize::from(local)))
                            .and_then(|ty| ctx.aggregate_layout_by_semantic_type().get(ty).copied())
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// SSA 值 → 冻结布局身份表（旧公共 IR 的 `value_layouts` 同源派生）。
pub(crate) fn value_layout_table(ctx: &LaneContext, function: &ValkyrieMirFunction) -> BTreeMap<MirValueId, LayoutId> {
    function
        .value_types
        .iter()
        .filter_map(|(value, type_id)| ctx.aggregate_layout_by_type().get(type_id).copied().map(|layout| (*value, layout)))
        .collect()
}

/// OVM 后端私有计划：为函数内每个 `utf8` SSA 值逐行冻结载体事实（raw/managed）。
///
/// 每条规则都是显式计划事实，发射期不得再沿 SSA 定义链恢复载体。定义形态事实
/// 先于类型登记事实：字面量、嵌套槽读取与宿主导入返回的物理内容就是 raw 宿主串，
/// 不因该值的 `TypeId` 在布局注册表里有托管布局而改写。
/// 1. 字面量定义（`LoadConstant Utf8`）→ `Raw`（OVM 常量 ABI：字面量编码为
///    `OdinConstant::String` 宿主 raw 串）；
/// 2. 嵌套槽读取定义（`FieldGet`/`TupleGet`/`ArrayGet`/`SumPayloadGet`/`NullUnwrap`）→ `Raw`，
///    与 `ovm_abi::Projection::project_text` 对 `TextSlot::Nested` 的显式声明同源：
///    嵌套槽由目标合同固定为 raw 宿主串，不是发射器推断；
/// 3. 外部宿主导入调用结果 → `Raw`（宿主边界 ABI 事实）；
/// 4. 入口形参槽 → 与 OVM ABI 合同同一派生点 [`frozen_entry_parameter_layouts`]：
///    解析到布局 → `Managed`；程序级表示计划显式声明 raw → `Raw`；
///    未显式声明 raw 而该槽解析不到布局 → `missing`，发射前拒收；
/// 5. `value_layouts` 已登记 → `Managed(layout)`（前端类型身份事实：构造/调用产出的
///    托管对象槽）；
/// 6. `Copy`/`StoreVar`/恒等 `Cast` → 继承源操作数事实（搬运不改载体，迭代至不动点）；
/// 7. 程序级表示计划显式声明 raw → 其余 `utf8` 值一律 `Raw`；布局注册表为空
///    **不是**该事实的来源；
/// 8. 其余 `utf8` 值是 `missing`：发射前结构化错误，不得解释为 raw
///    （对齐 wasm 后端 `32c5081e` 的缺事实拒收先例）。
pub(crate) fn freeze_text_representations(
    ctx: &LaneContext,
    function: &ValkyrieMirFunction,
    view: &FunctionView,
    entry_layouts: &[Option<LayoutId>],
    value_definitions: &BTreeMap<MirValueId, &ValkyrieMirInstruction>,
    value_layouts: &BTreeMap<MirValueId, LayoutId>,
    external_link_by_instance: &BTreeMap<ItemInstanceId, Vec<ExternalImportLink>>,
) -> Result<BTreeMap<MirValueId, TextCarrier>, String> {
    // 程序级 raw 模式只来自正式表示计划事实：计划显式声明 raw 才授权 raw 槽。
    // 禁止由布局注册表为空反推——上游漏登记整类布局时 missing 会被吸收成合法表示。
    let program_raw = ctx.program_text_representation() == Some(ProgramTextRepresentation::Raw);
    let text_values: Vec<MirValueId> = view.value_types.iter().filter(|(_, ty)| matches!(**ty, SemanticType::Utf8)).map(|(value, _)| *value).collect();
    let entry_parameters: Vec<MirValueId> =
        function.blocks.get(&function.entry).map(|block| block.parameters.iter().map(|(value, _)| *value).collect()).unwrap_or_default();

    let mut table: BTreeMap<MirValueId, TextCarrier> = BTreeMap::new();
    let mut pending_moves: Vec<MirValueId> = Vec::new();
    for value in &text_values {
        // 规则 1-3：定义形态的显式 raw 事实先于类型登记事实。
        match value_definitions.get(value).map(|instruction| &instruction.operation) {
            Some(ValkyrieMirOperation::LoadConstant { constant: ValkyrieMirConstant::Utf8(_) }) => {
                table.insert(*value, TextCarrier::Raw);
                continue;
            }
            Some(
                ValkyrieMirOperation::FieldGet { .. }
                | ValkyrieMirOperation::TupleGet { .. }
                | ValkyrieMirOperation::ArrayGet { .. }
                | ValkyrieMirOperation::SumPayloadGet { .. }
                | ValkyrieMirOperation::NullUnwrap { .. },
            ) => {
                table.insert(*value, TextCarrier::Raw);
                continue;
            }
            Some(ValkyrieMirOperation::Invoke { callee: ValkyrieMirCallee::Item(instance), .. }) if external_link_by_instance.contains_key(instance) => {
                table.insert(*value, TextCarrier::Raw);
                continue;
            }
            Some(ValkyrieMirOperation::Copy { .. } | ValkyrieMirOperation::StoreVar { .. }) => {
                pending_moves.push(*value);
                continue;
            }
            Some(ValkyrieMirOperation::Cast { ty, .. }) if ctx.lower_type(*ty).is_ok_and(|ty| matches!(ty, SemanticType::Utf8)) => {
                pending_moves.push(*value);
                continue;
            }
            _ => {}
        }
        // 规则 4：入口形参槽与 OVM ABI 合同同一派生点。
        if let Some(position) = entry_parameters.iter().position(|parameter| parameter == value) {
            match entry_layouts.get(position).copied().flatten() {
                Some(layout) => {
                    table.insert(*value, TextCarrier::Managed(layout));
                }
                None if program_raw => {
                    table.insert(*value, TextCarrier::Raw);
                }
                None => {
                    return Err(format!(
                        "OVM 文本表示计划缺少 raw 事实：入口形参 #{position}（%{}，symbol=`{}`）的 `utf8` 槽没有解析到目标计划已冻结的文本布局，\
                         程序级表示计划也没有显式声明 raw 表示；缺登记不是「程序选择 raw」的证据，missing 事实不得解释为 raw",
                        value.index(),
                        view.symbol
                    ));
                }
            }
            continue;
        }
        // 规则 5：类型身份登记的托管对象槽。
        if let Some(layout) = value_layouts.get(value).copied() {
            table.insert(*value, TextCarrier::Managed(layout));
        }
    }
    // 规则 7：程序级显式 raw 计划事实兜底其余文本值；未经声明不得吸收成 raw。
    if program_raw {
        for value in &text_values {
            table.entry(*value).or_insert(TextCarrier::Raw);
        }
    }
    // 规则 6：搬运事实不动点传播；源操作数先于目标分类（SSA 定义先于使用）。
    loop {
        let before = table.len();
        let mut unresolved = Vec::new();
        for value in std::mem::take(&mut pending_moves) {
            let source = match value_definitions.get(&value).map(|instruction| &instruction.operation) {
                Some(ValkyrieMirOperation::Copy { source }) | Some(ValkyrieMirOperation::StoreVar { value: source, .. }) => *source,
                Some(ValkyrieMirOperation::Cast { source, .. }) => *source,
                _ => {
                    unresolved.push(value);
                    continue;
                }
            };
            match table.get(&source).copied() {
                Some(carrier) => {
                    table.insert(value, carrier);
                }
                None => unresolved.push(value),
            }
        }
        pending_moves = unresolved;
        if table.len() == before {
            break;
        }
    }
    let missing: Vec<u32> = text_values.iter().filter(|value| !table.contains_key(value)).map(|value| value.index()).collect();
    if !missing.is_empty() {
        return Err(format!(
            "OVM 文本表示计划缺少载体事实：`utf8` SSA 值 {missing:?}（symbol=`{}`）没有已冻结的 raw/managed 行；\
             缺布局是 missing 事实，发射前结构化拒收，不得解释为 raw（对齐 wasm 后端缺事实拒收先例）",
            view.symbol
        ));
    }
    Ok(table)
}

/// 模块级已结算事实：函数表、入口槽合同、外部签名与 operator 绑定。
pub(crate) struct OvmModuleFacts<'a> {
    pub ctx: &'a LaneContext<'a>,
    pub layouts: &'a ResolvedLayoutPlan,
    pub function_index_by_instance: &'a BTreeMap<ItemInstanceId, i32>,
    pub function_entry_arities: &'a BTreeMap<i32, usize>,
    pub param_types_by_function_index: &'a BTreeMap<i32, Vec<SemanticType>>,
    pub param_layouts_by_function_index: &'a BTreeMap<i32, Vec<Option<LayoutId>>>,
    pub external_link_by_instance: &'a BTreeMap<ItemInstanceId, Vec<ExternalImportLink>>,
    pub external_param_types_by_instance: &'a BTreeMap<ItemInstanceId, Vec<SemanticType>>,
    pub external_opcode_by_instance: &'a BTreeMap<ItemInstanceId, (String, OvmOperatorBinding)>,
    pub sum_layout_index_by_nominal: &'a BTreeMap<NominalInstanceId, i32>,
}

/// 逐函数 lowering：MIR → 目标私有 LIR。
pub(crate) struct OvmFunctionLowering<'a> {
    services: &'a OvmModuleFacts<'a>,
    instance: ItemInstanceId,
    view: &'a FunctionView,
    function: &'a ValkyrieMirFunction,
    slots: SlotPlan,
    value_layouts: BTreeMap<MirValueId, LayoutId>,
    return_layout: Option<LayoutId>,
    entry_layouts: Vec<Option<LayoutId>>,
    text_representations: BTreeMap<MirValueId, TextCarrier>,
    emitter: &'a mut BytecodeEmitter<'a>,
    layouts: &'a mut Vec<OdinLayout>,
    layout_index_by_id: &'a mut BTreeMap<LayoutId, i32>,
    layout_index_by_field_count: &'a mut BTreeMap<i32, i32>,
}

impl<'a> OvmFunctionLowering<'a> {
    pub(crate) fn new(
        services: &'a OvmModuleFacts<'a>,
        instance: ItemInstanceId,
        emitter: &'a mut BytecodeEmitter<'a>,
        layouts: &'a mut Vec<OdinLayout>,
        layout_index_by_id: &'a mut BTreeMap<LayoutId, i32>,
        layout_index_by_field_count: &'a mut BTreeMap<i32, i32>,
    ) -> Result<Self, TargetPlanError> {
        let ctx = services.ctx;
        let view = ctx.view(&instance).expect("operations 只含已结算函数视图的实例");
        let function = ctx.function(&instance).expect("operations 只含带函数体的实例");
        let fail = |reason: String| TargetPlanError::Unsound { stage: AdapterStage::Function, instance: Some(instance), symbol: Some(view.symbol.clone()), reason };
        let slots = SlotPlan::plan(function, view).map_err(fail)?;
        let value_layouts = value_layout_table(ctx, function);
        let return_layout = ctx.aggregate_layout_by_type().get(&function.return_type).copied();
        let entry_layouts = frozen_entry_parameter_layouts(ctx, function, view, &slots);
        let value_definitions = build_value_definitions(function);
        // 发射前冻结文本表示计划：缺载体事实的 `utf8` 值在此结构化拒收，
        // 发射期只读冻结表，不再沿 SSA 定义链恢复载体。
        let text_representations = freeze_text_representations(
            ctx,
            function,
            view,
            &entry_layouts,
            &value_definitions,
            &value_layouts,
            &services.external_link_by_instance,
        )
        .map_err(fail)?;
        Ok(Self {
            services,
            instance,
            view,
            function,
            slots,
            value_layouts,
            return_layout,
            entry_layouts,
            text_representations,
            emitter,
            layouts,
            layout_index_by_id,
            layout_index_by_field_count,
        })
    }

    /// 入口块 SSA 形参数量：调用约定 arity 的唯一来源（`param_types` 可能含废弃 ABI 槽）。
    pub(crate) fn entry_arity(&self) -> usize {
        self.function.blocks.get(&self.function.entry).expect("入口块存在性已在入口槽结算时校验").parameters.len()
    }

    /// 已结算的 local 总数（与旧公共路径同一分配顺序）。
    pub(crate) fn local_count(&self) -> usize {
        self.slots.local_types.len()
    }

    /// 降低整个函数体：RPO 块序 + 逐块指令 + 终结符，并编码为 `.odin` 函数体字节。
    ///
    /// 编码在降低完成后一次完成（目标私有 LIR → 字节），emitter 借用随之结束。
    pub(crate) fn lower(mut self) -> Result<(Vec<u8>, Vec<OdinConstant>), TargetPlanError> {
        let block_order = collect_reachable_blocks(self.function);
        for block_id in block_order {
            self.emitter.emit_block_label(block_id);
            if let Some(block) = self.function.blocks.get(&ValkyrieMirBlockId(block_id)) {
                self.emit_block(block)?;
            }
        }
        let code = encode_odin_function(&self.emitter.instructions).map_err(|reason| self.fail(reason))?;
        // 常量池随编码结果一并交给调用方并入模块：emitter 借用被不变性钉到迭代末尾，不能在其存活时移出字段。
        Ok((code, std::mem::take(&mut self.emitter.constants)))
    }

    fn fail(&self, reason: impl Into<String>) -> TargetPlanError {
        TargetPlanError::Unsound { stage: AdapterStage::Function, instance: Some(self.instance), symbol: Some(self.view.symbol.clone()), reason: reason.into() }
    }

    fn emit_block(&mut self, block: &ValkyrieMirBlock) -> Result<(), TargetPlanError> {
        for instruction in &block.instructions {
            self.emit_instruction(instruction)?;
        }
        self.emit_terminator(block)?;
        Ok(())
    }

    fn emit_instruction(&mut self, instruction: &ValkyrieMirInstruction) -> Result<(), TargetPlanError> {
        let output = SlotPlan::instruction_output(instruction);
        match &instruction.operation {
            ValkyrieMirOperation::LoadConstant { constant } => {
                // 常量位宽只由结果槽的冻结类型决定；槽缺失时回落到 semantic MIR
                // 对 `Constant::Int` 的合同（Integer64），绝不按数值大小猜。
                let output_ty = output.and_then(|value| self.value_slot_type(value));
                self.emit_load_constant(constant, output_ty.as_ref())?;
                if let Some(output) = output {
                    self.store_to_local(output);
                }
            }
            ValkyrieMirOperation::LoadFunctionRef { instance, environment, captured } => {
                // 函数值 = 两槽闭包对象：槽 0 = 本模块函数表下标（code i32），
                // 槽 1 = 捕获环境聚合或 null。跨模块/导入实例没有稳定下标，
                // 只能结构化失败：不回落符号、不猜，也不把闭包降回裸 i32。
                let Some(index) = self.services.function_index_by_instance.get(instance).copied()
                else {
                    return Err(self.fail(format!(
                        "OVM 闭包载体要求 instance 属于本模块函数表（跨模块/导入函数值未支持）：{instance:?}"
                    )));
                };
                let Some(output) = output
                else {
                    return Err(self.fail(format!("OVM 闭包载体必须有结果槽：instance={instance:?} captured=[{}]", captured.join(", "))));
                };
                let object = ValkyrieMirOperand::Value(output);
                let layout = self.ensure_nyar_layout_for_field_count(2);
                self.emitter.emit_imm1(OdinHeadCode::ObjectNew, layout);
                self.store_to_local(output);
                // 槽 0：code 下标。Odin 槽无类型，常量位宽由这里固定为 i32。
                self.emit_field_set_slot(
                    &object,
                    0,
                    &ValkyrieMirOperand::Constant(ValkyrieMirConstant::Int(index as i64)),
                    Some(&SemanticType::Integer32 { signed: true }),
                )?;
                self.emitter.emit_plain(OdinHeadCode::Pop);
                // 槽 1：环境聚合或显式 null（无捕获也必须仍是闭包对象）。
                match environment {
                    Some(environment) => {
                        let environment_ty = match environment {
                            ValkyrieMirOperand::Value(value) => self.value_semantic_type(*value),
                            ValkyrieMirOperand::Constant(_) => None,
                        };
                        self.emit_field_set_slot(&object, 1, environment, environment_ty.as_ref())?;
                    }
                    None => {
                        self.emit_operand(&object)?;
                        self.emitter.emit_const_null();
                        self.emitter.emit_imm1(OdinHeadCode::FieldSet, 1);
                    }
                }
                self.emitter.emit_plain(OdinHeadCode::Pop);
            }
            ValkyrieMirOperation::StoreVar { value, .. } => {
                let output_ty = output.and_then(|value| self.value_slot_type(value));
                self.emit_operand_typed(&ValkyrieMirOperand::Value(*value), output_ty.as_ref())?;
                if let Some(output) = output {
                    self.store_to_local(output);
                }
                else {
                    self.emitter.emit_plain(OdinHeadCode::Pop);
                }
            }
            ValkyrieMirOperation::Copy { source } => {
                let output_ty = output.and_then(|value| self.value_slot_type(value));
                self.emit_operand_typed(&ValkyrieMirOperand::Value(*source), output_ty.as_ref())?;
                if let Some(output) = output {
                    self.store_to_local(output);
                }
            }
            ValkyrieMirOperation::Cast { source, ty, .. } => {
                // 源类型只从 semantic MIR 的冻结表读取（常量位宽回落同一合同），
                // 不按数值大小或目标类型反推。
                let source_operand = ValkyrieMirOperand::Value(*source);
                let source_ty = self.operand_semantic_type(&source_operand)?;
                let target_ty = ctx_lower(self.services.ctx, *ty)
                    .map_err(|reason| self.fail(format!("OVM `Cast` 目标类型缺少目标合同：{reason}（symbol=`{}`）", self.view.symbol)))?;
                self.emit_operand_typed(&source_operand, Some(&source_ty))?;
                self.emit_cast_conversion(&source_ty, &target_ty)?;
                if let Some(output) = output {
                    self.store_to_local(output);
                }
                else {
                    self.emitter.emit_plain(OdinHeadCode::Pop);
                }
            }
            ValkyrieMirOperation::StructNew { nominal, fields } => {
                let layout = self
                    .services
                    .ctx
                    .aggregate_layout_by_nominal()
                    .get(nominal)
                    .and_then(|id| self.services.layouts.layout(*id))
                    .cloned()
                    .ok_or_else(|| self.fail(format!("OVM 聚合身份缺少布局合同：nominal {nominal:?} 不在计划的 AggregateLayoutPlan")))?;
                let layout_index = self.ensure_nyar_layout(&layout);
                self.emitter.emit_imm1(OdinHeadCode::ObjectNew, layout_index);
                let output = output.ok_or_else(|| self.fail("OVM StructNew 必须产出值槽"))?;
                self.store_to_local(output);
                let output_operand = ValkyrieMirOperand::Value(output);
                for (field, value) in fields.iter() {
                    let slot = self.nyar_field_slot(*field)?;
                    let value_ty = self.field_value_type(*field);
                    self.emit_field_set_slot(&output_operand, slot, &ValkyrieMirOperand::Value(*value), value_ty.as_ref())?;
                    self.emitter.emit_plain(OdinHeadCode::Pop);
                }
            }
            ValkyrieMirOperation::TupleGet { object, tuple_type, index } => {
                // 元组身份与索引界先按 canonical 类型事实校验（旧公共投影同源），
                // 布局身份来自被索引对象的冻结类型。
                let arity = match self.services.ctx.linked().types.get(tuple_type).map(|row| &row.kind) {
                    Some(compiler_types::CanonicalTypeKind::Tuple(members)) => members.len(),
                    Some(compiler_types::CanonicalTypeKind::LabeledTuple { fields }) => fields.len(),
                    _ => return Err(self.fail(format!("TupleGet 对象不是元组类型: {tuple_type:?}"))),
                };
                if (*index as usize) >= arity {
                    return Err(self.fail(format!("TupleGet 位置索引 {index} 越界（元组长度 {arity}）")));
                }
                let layout_id = self
                    .services
                    .ctx
                    .aggregate_layout_by_type()
                    .get(tuple_type)
                    .copied()
                    .ok_or_else(|| self.fail(format!("TupleGet 缺少编译器绑定的布局身份: {tuple_type:?}")))?;
                let layout = self
                    .services
                    .layouts
                    .layout(layout_id)
                    .cloned()
                    .ok_or_else(|| self.fail(format!("OVM TupleGet 布局身份缺少布局合同：layout_id={layout_id:?}")))?;
                if (*index as usize) >= layout.fields.len() {
                    return Err(self.fail(format!(
                        "OVM TupleGet 位置索引 {index} 越界：layout `{}` 长度 {}",
                        layout.name,
                        layout.fields.len()
                    )));
                }
                let slot = i32::try_from(*index)
                    .map_err(|_| self.fail(format!("OVM TupleGet 位置索引 {index} 超出 i32 槽合同")))?;
                // 目标对象按位置槽读取；标签只在类型身份层，不能作为 OVM 槽位来源。
                self.emit_operand(&ValkyrieMirOperand::Value(*object))?;
                self.emitter.emit_imm1(OdinHeadCode::FieldGet, slot);
                if let Some(output) = output {
                    self.store_to_local(output);
                }
                else {
                    self.emitter.emit_plain(OdinHeadCode::Pop);
                }
            }
            ValkyrieMirOperation::TupleNew { element_types, fields } => {
                // 结果必须是 tuple 形状且字段数与类型合同一致（旧公共投影的同源校验，
                // 禁止按 fields 猜布局）。标签只属于 canonical 身份；目标布局按位置。
                let result = SlotPlan::instruction_output(instruction).ok_or_else(|| self.fail("TupleNew 缺少结果值"))?;
                let result_type_id = self
                    .function
                    .value_types
                    .get(&result)
                    .copied()
                    .ok_or_else(|| self.fail(format!("聚合值缺少 Semantic MIR 类型事实: {result:?}")))?;
                if !matches!(
                    self.services.ctx.linked().types.get(&result_type_id).map(|row| &row.kind),
                    Some(compiler_types::CanonicalTypeKind::Tuple(_)) | Some(compiler_types::CanonicalTypeKind::LabeledTuple { .. })
                ) {
                    return Err(self.fail(format!("TupleNew 结果不是 tuple 类型: {result_type_id:?}")));
                }
                if fields.len() != element_types.len() {
                    return Err(self.fail("TupleNew 元素合同长度不一致"));
                }
                let layout_id = self
                    .services
                    .ctx
                    .aggregate_layout_by_type()
                    .get(&result_type_id)
                    .copied()
                    .ok_or_else(|| self.fail(format!("TupleNew 缺少编译器绑定的布局身份: {result_type_id:?}")))?;
                let layout = self
                    .services
                    .layouts
                    .layout(layout_id)
                    .cloned()
                    .ok_or_else(|| self.fail(format!("OVM tuple 布局身份缺少布局合同：layout_id={layout_id:?}")))?;
                let layout_index = self.ensure_nyar_layout(&layout);
                self.emitter.emit_imm1(OdinHeadCode::ObjectNew, layout_index);
                let output = output.ok_or_else(|| self.fail("OVM TupleNew 必须产出值槽"))?;
                self.store_to_local(output);
                let output_operand = ValkyrieMirOperand::Value(output);
                for (index, value) in fields.iter().enumerate() {
                    let value_ty = layout.fields.get(index).map(|field| field.ty.clone());
                    self.emit_field_set_slot(&output_operand, index as i32, &ValkyrieMirOperand::Value(*value), value_ty.as_ref())?;
                    self.emitter.emit_plain(OdinHeadCode::Pop);
                }
            }
            ValkyrieMirOperation::ArrayFromElements { array_type, elements } => {
                let Some(output) = output
                else {
                    return Err(self.fail("OVM ArrayFromElements 必须产出值槽"));
                };
                let array_type = ctx_lower(self.services.ctx, *array_type)
                    .map_err(|reason| self.fail(format!("ArrayFromElements 数组类型缺少目标合同: {reason}")))?;
                // 合成的定长数组布局身份不保证进入 `AggregateLayoutPlan`；Odin 布局只记录
                // 字段数且槽位无类型，所以按元素个数铸造后端私有布局，禁止依赖未冻结的
                // `layout_id`，更不允许 panic 杀进程。
                if let SemanticType::FixedArray { length, .. } = &array_type
                    && *length != elements.len()
                {
                    return Err(self.fail(format!(
                        "OVM ArrayFromElements 的元素个数与冻结的定长数组类型不符：type={array_type} declared={length} actual={}",
                        elements.len()
                    )));
                }
                let field_count = i32::try_from(elements.len())
                    .map_err(|_| self.fail(format!("OVM ArrayFromElements 元素个数超出 i32 布局槽位：{}", elements.len())))?;
                let element_ty = match &array_type {
                    SemanticType::FixedArray { element, .. } | SemanticType::Array(element) => Some((**element).clone()),
                    _ => None,
                };
                let layout_index = self.ensure_nyar_layout_for_field_count(field_count);
                self.emitter.emit_imm1(OdinHeadCode::ObjectNew, layout_index);
                self.store_to_local(output);
                let output_operand = ValkyrieMirOperand::Value(output);
                for (index, value) in elements.iter().enumerate() {
                    self.emit_field_set_slot(&output_operand, index as i32, &ValkyrieMirOperand::Value(*value), element_ty.as_ref())?;
                    self.emitter.emit_plain(OdinHeadCode::Pop);
                }
            }
            ValkyrieMirOperation::AggregateCopy { source, destination } => {
                let source_type_id = self
                    .function
                    .value_types
                    .get(source)
                    .copied()
                    .ok_or_else(|| self.fail(format!("聚合复制源值缺少 Semantic MIR 类型事实: {source:?}")))?;
                let dest_type_id = self
                    .function
                    .value_types
                    .get(destination)
                    .copied()
                    .ok_or_else(|| self.fail(format!("聚合复制目标值缺少 Semantic MIR 类型事实: {destination:?}")))?;
                let source_type = ctx_lower(self.services.ctx, source_type_id).map_err(|reason| self.fail(format!("聚合复制源类型缺少目标合同: {reason}")))?;
                let destination_type = ctx_lower(self.services.ctx, dest_type_id).map_err(|reason| self.fail(format!("聚合复制目标类型缺少目标合同: {reason}")))?;
                if source_type != destination_type {
                    return Err(self.fail(format!("AggregateCopy 源与目标类型不一致: {source_type:?} != {destination_type:?}")));
                }
                let layout_id = self
                    .services
                    .ctx
                    .aggregate_layout_by_type()
                    .get(&source_type_id)
                    .copied()
                    .ok_or_else(|| self.fail(format!("AggregateCopy 缺少编译器绑定的布局身份: {source_type_id:?}")))?;
                let layout = self
                    .services
                    .layouts
                    .layout(layout_id)
                    .cloned()
                    .ok_or_else(|| self.fail(format!("OVM aggregate-copy 布局身份缺少布局合同：layout_id={layout_id:?}")))?;
                let field_count = layout.fields.len() as i32;
                let layout_index = self.ensure_nyar_layout(&layout);
                self.emitter.emit_imm1(OdinHeadCode::ObjectNew, layout_index);
                let dest = ValkyrieMirOperand::Value(*destination);
                self.store_to_local(*destination);
                for slot in 0..field_count {
                    self.emit_operand(&dest)?;
                    self.emit_operand(&ValkyrieMirOperand::Value(*source))?;
                    self.emitter.emit_imm1(OdinHeadCode::FieldGet, slot);
                    self.emitter.emit_imm1(OdinHeadCode::FieldSet, slot);
                    self.emitter.emit_plain(OdinHeadCode::Pop);
                }
            }
            ValkyrieMirOperation::FieldGet { object, field } => {
                let slot = self.nyar_field_slot(*field)?;
                self.emit_operand(&ValkyrieMirOperand::Value(*object))?;
                self.emitter.emit_imm1(OdinHeadCode::FieldGet, slot);
                if let Some(output) = output {
                    self.store_to_local(output);
                }
                else {
                    // 无 SSA 绑定的 FieldGet 不得污染后续 Call 的操作数栈；实参由 `emit_call_operand` 再求值。
                    self.emitter.emit_plain(OdinHeadCode::Pop);
                }
            }
            ValkyrieMirOperation::FieldSet { object, field, value } => {
                let slot = self.nyar_field_slot(*field)?;
                let value_ty = self.field_value_type(*field);
                self.emit_field_set_slot(
                    &ValkyrieMirOperand::Value(*object),
                    slot,
                    &ValkyrieMirOperand::Value(*value),
                    value_ty.as_ref(),
                )?;
                self.emitter.emit_plain(OdinHeadCode::Pop);
            }
            // sum 的物理载体是后端私有的 `[i32 tag, payload]` 对象；语义身份（nominal /
            // variant）只来自计划冻结的 `SumRepresentation`，绝不按名字或字面 tag 推断。
            // OVM 的 Odin 对象字段是无类型槽，标量载荷直接入槽；wasm 的 payload 槽是
            // anyref 才需要装箱，这里不得照抄 wasm 的装箱路径。
            ValkyrieMirOperation::SumNew { nominal, variant, payload } => {
                let (tag, planned_payload_type) = {
                    let sum = self
                        .services
                        .ctx
                        .sum_representations()
                        .get(nominal)
                        .ok_or_else(|| self.fail(format!("OVM sum 表示缺失：nominal {nominal:?} 没有冻结的 SumTypeLayout")))?;
                    let variant_layout = sum
                        .variants
                        .get(variant)
                        .ok_or_else(|| self.fail(format!("OVM SumNew 的 variant {variant:?} 不属于 nominal {nominal:?} 的冻结变体表")))?;
                    (variant_layout.tag, variant_layout.payload_type)
                };
                if planned_payload_type.is_some() != payload.is_some() {
                    return Err(self.fail(format!(
                        "OVM SumNew 的 variant {variant:?} 载荷与计划合同不一致：计划 payload_type={planned_payload_type:?}，指令 payload={}",
                        payload.is_some()
                    )));
                }
                let Some(output) = output
                else {
                    return Err(self.fail("OVM SumNew 必须产出值槽"));
                };
                let sum_layout_index = self.ensure_sum_layout(*nominal)?;
                self.emitter.emit_imm1(OdinHeadCode::ObjectNew, sum_layout_index);
                self.store_to_local(output);
                let object = ValkyrieMirOperand::Value(output);
                // tag 槽的物理宽度是 OVM 自己冻结的 i32，discriminant 超界即失败。
                let tag_value = i32::try_from(tag).map_err(|_| {
                    self.fail(format!("OVM SumNew 的 tag {tag} 超出 i32 槽合同（nominal={nominal:?}, variant={variant:?})"))
                })?;
                let tag_ty = SemanticType::Integer32 { signed: true };
                self.emit_field_set_slot(&object, 0, &ValkyrieMirOperand::Constant(ValkyrieMirConstant::Int(i64::from(tag_value))), Some(&tag_ty))?;
                self.emitter.emit_plain(OdinHeadCode::Pop);
                if let Some(payload) = payload {
                    // payload 槽本身不带类型；SSA 载荷用其冻结语义类型，常量载荷
                    // 回落到 semantic MIR 合同（`Constant::Int` = Integer64）。
                    let payload_ty = self.value_semantic_type(*payload);
                    self.emit_field_set_slot(&object, 1, &ValkyrieMirOperand::Value(*payload), payload_ty.as_ref())?;
                    self.emitter.emit_plain(OdinHeadCode::Pop);
                }
            }
            ValkyrieMirOperation::SumVariantIs { nominal, variant, object } => {
                let tag = self.sum_variant_tag(*nominal, *variant, "SumVariantIs")?;
                self.emit_operand(&ValkyrieMirOperand::Value(*object))?;
                self.emitter.emit_imm1(OdinHeadCode::FieldGet, 0);
                self.emitter.emit_const_i32(tag as i32);
                self.emitter.emit_plain(OdinHeadCode::I32Eq);
                if let Some(output) = output {
                    self.store_to_local(output);
                }
                else {
                    self.emitter.emit_plain(OdinHeadCode::Pop);
                }
            }
            ValkyrieMirOperation::SumPayloadGet { nominal, variant, object } => {
                let payload_type = self.sum_variant_payload_type(*nominal, *variant, "SumPayloadGet")?;
                if payload_type.is_none() {
                    return Err(self.fail(format!("OVM SumPayloadGet 对无 payload 的 variant {variant:?} 取值（nominal={nominal:?}）")));
                }
                self.emit_operand(&ValkyrieMirOperand::Value(*object))?;
                self.emitter.emit_imm1(OdinHeadCode::FieldGet, 1);
                if let Some(output) = output {
                    self.store_to_local(output);
                }
                else {
                    self.emitter.emit_plain(OdinHeadCode::Pop);
                }
            }
            ValkyrieMirOperation::Invoke { callee, arguments } => {
                let argument_operands: Vec<ValkyrieMirOperand> = arguments.iter().copied().map(ValkyrieMirOperand::Value).collect();
                match callee {
                    ValkyrieMirCallee::Item(instance) => {
                        if let Some(links) = self.services.external_link_by_instance.get(instance).cloned() {
                            let opcode = self.services.external_opcode_by_instance.get(instance);
                            self.emit_external_call(&links, *instance, opcode, &argument_operands, output)?;
                            return Ok(());
                        }
                        let index = self.services.function_index_by_instance.get(instance).copied().ok_or_else(|| {
                            self.fail(format!("已校验的可调用身份没有 Nyar 函数下标（闭包未登记函数体）: {instance:?}"))
                        })?;
                        let name = self.services.ctx.abi_name(instance).ok_or_else(|| {
                            self.fail(format!("已校验的可调用身份没有诊断名: {instance:?}"))
                        })?;
                        let path = NamePath::new(name.parts().to_vec());
                        self.emit_direct_call(index, &path, &argument_operands, output)?;
                        return Ok(());
                    }
                    ValkyrieMirCallee::Witness(witness) => {
                        // witness 必须在实例化固定点消去为 Item；到达目标层即合同破口。
                        return Err(self.fail(format!(
                            "canonical 未消去的 trait witness 分派到达目标层：trait={:?} method={:?} self_type={:?}",
                            witness.trait_declaration, witness.method_declaration, witness.self_type
                        )));
                    }
                    ValkyrieMirCallee::Value(callee_value) => {
                        // 函数值间接调用：callee 是携带冻结 `Function` 签名的 SSA 值，
                        // 运行期目标是槽里的函数索引；实参位宽由签名决定，绝不按字面量猜。
                        let semantic = self.value_semantic_type(*callee_value);
                        let Some(SemanticType::Function(signature)) = semantic.clone()
                        else {
                            let actual = semantic.as_ref().map(ToString::to_string).unwrap_or_else(|| "<未冻结>".to_string());
                            return Err(self.fail(format!(
                                "OVM 间接调用要求 callee 携带冻结 Function 签名，实际类型 {actual}"
                            )));
                        };
                        if arguments.len() != signature.params.len() {
                            return Err(self.fail(format!(
                                "OVM 间接调用实参个数 {} 与冻结签名形参个数 {} 不符",
                                arguments.len(),
                                signature.params.len()
                            )));
                        }
                        // 先压实参、再压环境槽、最后压 code：`CallIndirect` 只弹 callee，
                        // 实参与环境槽留在栈上；Odin 没有 swap，顺序错了取不出来。
                        // 环境槽是合成 lambda 的尾形参，因此 argc = 可见实参 + 1。
                        for (index, argument) in argument_operands.iter().enumerate() {
                            let param_ty = signature.params.get(index);
                            match argument {
                                ValkyrieMirOperand::Value(_) => {
                                    if !self.emit_call_operand(argument)? {
                                        return Err(self.fail(format!("OVM 间接调用实参缺少预规划槽：{argument:?}")));
                                    }
                                }
                                ValkyrieMirOperand::Constant(_) => self.emit_operand_typed(argument, param_ty)?,
                            }
                        }
                        // 闭包载体两槽：0 = code i32 函数下标，1 = env 聚合对象或 null。
                        let callee_operand = ValkyrieMirOperand::Value(*callee_value);
                        self.emit_operand_typed(&callee_operand, None)?;
                        self.emitter.emit_imm1(OdinHeadCode::FieldGet, 1);
                        self.emit_operand_typed(&callee_operand, None)?;
                        self.emitter.emit_imm1(OdinHeadCode::FieldGet, 0);
                        self.emitter.emit_imm1(OdinHeadCode::CallIndirect, arguments.len() as i32 + 1);
                        if let Some(output) = output {
                            self.store_to_local(output);
                        }
                        else {
                            self.emitter.emit_plain(OdinHeadCode::Pop);
                        }
                        return Ok(());
                    }
                }
            }
            ValkyrieMirOperation::ArrayGet { array, index } => {
                self.emit_call_operand(&ValkyrieMirOperand::Value(*array))?;
                let index_ty = SemanticType::Integer32 { signed: true };
                self.emit_operand_typed(&ValkyrieMirOperand::Value(*index), Some(&index_ty))?;
                self.emitter.emit_call_intrinsic(IntrinsicOpcode::ArrayGet, 2);
                if let Some(output) = output {
                    self.store_to_local(output);
                }
            }
            ValkyrieMirOperation::ArraySet { array, index, value } => {
                let array_operand = ValkyrieMirOperand::Value(*array);
                self.emit_call_operand(&array_operand)?;
                let index_ty = SemanticType::Integer32 { signed: true };
                self.emit_operand_typed(&ValkyrieMirOperand::Value(*index), Some(&index_ty))?;
                let value_ty = self.array_element_type(&array_operand);
                // 数组元素槽是嵌套文本槽：目标合同固定 raw 宿主串，托管实参先抽取成 raw 再入槽。
                if value_ty.as_ref() == Some(&SemanticType::Utf8) {
                    self.emit_text_boundary_operand(&ValkyrieMirOperand::Value(*value), &SemanticType::Utf8, TextSlotTarget::Raw, "数组元素槽")?;
                }
                else {
                    self.emit_operand_typed(&ValkyrieMirOperand::Value(*value), value_ty.as_ref())?;
                }
                self.emitter.emit_call_intrinsic(IntrinsicOpcode::ArraySet, 3);
                self.emitter.emit_plain(OdinHeadCode::Pop);
            }
            ValkyrieMirOperation::ArrayLength { array } => {
                // 禁止 Const(0) 回退：会把 ArrayLen 的 array 实参变成 i32，运行时报 expected object。
                let array_operand = ValkyrieMirOperand::Value(*array);
                if !self.emit_call_operand(&array_operand)? {
                    return Err(self.fail(format!("ovm ArrayLength operand not materializable: {array_operand:?}")));
                }
                self.emitter.emit_call_intrinsic(IntrinsicOpcode::ArrayLen, 1);
                if let Some(output) = output {
                    self.store_to_local(output);
                }
            }
            ValkyrieMirOperation::NullIsNull { value } => {
                let operand = ValkyrieMirOperand::Value(*value);
                if !self.emit_call_operand(&operand)? {
                    return Err(self.fail(format!("ovm NullIsNull operand not materializable: {operand:?}")));
                }
                self.emitter.emit_call_intrinsic(IntrinsicOpcode::IsNull, 1);
                if let Some(output) = output {
                    self.store_to_local(output);
                }
            }
            ValkyrieMirOperation::NullUnwrap { value } => {
                let operand = ValkyrieMirOperand::Value(*value);
                if !self.emit_call_operand(&operand)? {
                    return Err(self.fail(format!("ovm NullUnwrap operand not materializable: {operand:?}")));
                }
                self.emitter.emit_call_intrinsic(IntrinsicOpcode::UnwrapNull, 1);
                if let Some(output) = output {
                    self.store_to_local(output);
                }
            }
            // fail-closed：未实现的指令必须报错，绝不静默吞掉（静默会让输出槽保持
            // `Value::Null`，把编译期错误推迟成运行期错值）。
            kind => {
                return Err(self.fail(format!("OVM 后端未实现指令 {kind:?}")));
            }
        }
        Ok(())
    }

    fn emit_terminator(&mut self, block: &ValkyrieMirBlock) -> Result<(), TargetPlanError> {
        match &block.terminator {
            ValkyrieMirTerminator::Return { value } => {
                if let Some(value) = value {
                    // 返回槽类型是函数签名的冻结事实；常量必须按它选位宽。
                    let return_ty = self.view.return_type.clone();
                    let operand = ValkyrieMirOperand::Value(*value);
                    if matches!(return_ty, SemanticType::Utf8) {
                        let Some(return_layout) = self.return_layout
                        else {
                            return Err(self.fail(format!(
                                "OVM 文本返回值缺少目标计划冻结的 `return_layout`（返回类型 `{return_ty}`）：\
                                 `utf8` 返回值必须按冻结布局物化，禁止把宿主 raw 串直接逃逸出函数边界"
                            )));
                        };
                        self.emit_text_boundary_operand(&operand, &return_ty, TextSlotTarget::Managed(return_layout), "返回值")?;
                    }
                    else {
                        self.emit_operand_typed(&operand, Some(&return_ty))?;
                    }
                }
                else {
                    // Call 约定固定压回 1 个返回值；unit / void 用 i32(0) 占位，供调用方 Pop。
                    self.emitter.emit_const_i32(0);
                }
                self.emitter.emit_plain(OdinHeadCode::Return);
            }
            ValkyrieMirTerminator::Jump { target, arguments } => {
                let argument_operands: Vec<ValkyrieMirOperand> = arguments.iter().copied().map(ValkyrieMirOperand::Value).collect();
                self.emit_block_argument_copies(*target, &argument_operands)?;
                self.emitter.emit_jump_to_block(OdinHeadCode::Jump, target.0);
            }
            ValkyrieMirTerminator::Branch { condition, then_target, else_target } => {
                let condition_ty = SemanticType::Boolean;
                self.emit_operand_typed(&ValkyrieMirOperand::Value(*condition), Some(&condition_ty))?;
                self.emitter.emit_jump_to_block(OdinHeadCode::JumpIfFalse, else_target.0);
                self.emit_block_argument_copies(*then_target, &[])?;
                self.emitter.emit_jump_to_block(OdinHeadCode::Jump, then_target.0);
            }
            ValkyrieMirTerminator::PerformEffect { effect, payload, resume_target } => {
                if !matches!(effect, ValkyrieMirEffectKind::Raise) {
                    return Err(self.fail(format!("canonical effect terminator 尚无目标合同: {effect:?}")));
                }
                // `raise expr`：将 payload 压栈后发射 `PerformEffect` opcode。
                // operand1 = 常量池索引（effect 的 method_name 字符串）。
                // VM 用此字符串在 witness_entries 中查找 handler。
                // handler 可选择 resume（控制流回到 resume_target，resume 值在栈顶）；
                // 若未 resume 则按未处理效果传播。
                // 对 `Effectful::Resume = !`：resume 不可达时 VM 不必保留 resume 续体。
                if let Some(payload) = payload {
                    self.emit_operand(&ValkyrieMirOperand::Value(*payload))?;
                }
                else {
                    // 无 payload 的 raise：压 i32(0) 作为 unit 占位。
                    self.emitter.emit_const_i32(0);
                }
                let effect_name_index = self.emitter.intern_string("raise");
                self.emitter.emit_imm1(OdinHeadCode::PerformEffect, effect_name_index);
                // resume 时 VM 把 resume 值压到栈顶；resume 块的唯一形参直接从栈顶落槽。
                // 零形参 resume 块（`Resume = !` 的未捕获 raise）没有落槽对象，
                // 栈顶值由 VM 的续体语义持有，本层不补造操作数。
                let resume_block = self
                    .function
                    .blocks
                    .get(resume_target)
                    .ok_or_else(|| self.fail(format!("OVM `raise` 的 resume 目标块 {} 不在块表中", resume_target.0)))?;
                match resume_block.parameters.len() {
                    0 => {}
                    1 => {
                        let local = *self
                            .slots
                            .block_param_locals
                            .get(&(*resume_target, 0))
                            .ok_or_else(|| self.fail("OVM `raise` resume 块形参缺少预规划槽"))?;
                        self.emit_store_local(local);
                    }
                    count => {
                        return Err(self.fail(format!("ovm `raise` resume 块只接受至多一个 resume 形参，实际 {count}")));
                    }
                }
                self.emitter.emit_jump_to_block(OdinHeadCode::Jump, resume_target.0);
            }
            ValkyrieMirTerminator::Unreachable => {
                // 不可达块不得物化为裸 `Return`：加载期 verifier 按 CFG 可达性把每条
                // `Return` 计入返回形状汇合（与位置无关），空栈 `Return` 会与值返回
                // 冲突。改为发射真 `Trap`：无后继、不贡献返回形状、不落入下一函数；
                // 语义正确，且一旦可达性保证被破坏（前端把可达块标成 `Unreachable`），
                // 运行期会得到带 function/pc 的结构化 trap 失败，而不是自旋挂死。
                self.emitter.emit_plain(OdinHeadCode::Trap);
            }
            ValkyrieMirTerminator::StateDispatch { .. } | ValkyrieMirTerminator::YieldToRuntime { .. } => {
                return Err(self.fail(format!("canonical effect terminator 尚无目标合同: {:?}", block.terminator)));
            }
        }
        Ok(())
    }

    /// 发射 `ctx.imports()` 中的外部 provider 调用。
    ///
    /// 先按计划冻结的 `[ovm_operator("...")]` 配方内联为指令：未冻结的绑定与 arity
    /// 不符必须结构化失败，不得回落为宿主导入或默认成功。其余已绑定目标 provider
    /// （`host` 边界 + `vm` 目标标签 + 恰好一段符号）保持 `CallImport`，由 runtime
    /// `parse_host_op` 判定。未绑定 `[host_contract]` 或其它目标标签必须失败，
    /// 不得回落为空体、默认值或 `0`。
    fn emit_external_call(
        &mut self,
        links: &[ExternalImportLink],
        instance: ItemInstanceId,
        opcode: Option<&(String, OvmOperatorBinding)>,
        arguments: &[ValkyrieMirOperand],
        output: Option<MirValueId>,
    ) -> Result<(), TargetPlanError> {
        let callee_name = self
            .services
            .ctx
            .abi_name(&instance)
            .map(|name| name.to_string())
            .unwrap_or_else(|| format!("{instance:?}"));

        if let Some((mnemonic, binding)) = opcode {
            let OvmOperatorBinding::Frozen(plan) = binding
            else {
                return Err(self.fail(format!(
                    "Nyar 目标 `ovm_operator` 绑定缺少可编码助记符 `{mnemonic}`: callee=`{callee_name}`"
                )));
            };
            let code = plan.encoding;
            if arguments.len() != plan.arity {
                return Err(self.fail(format!(
                    "Nyar 目标外部数值调用 `{callee_name}` 实参数量与 `{mnemonic}` 冻结 arity 不符: expected={} actual={}",
                    plan.arity,
                    arguments.len()
                )));
            }
            match (plan.recipe, code) {
                (TargetOperatorRecipe::Operands, OvmOperatorCode::Single(code)) => {
                    self.emit_external_arguments(&callee_name, instance, arguments)?;
                    self.emitter.emit_plain(code);
                }
                // 交换实参顺序执行有序比较：`f32.gt(a,b)` = `F32Lt(b,a)`，NaN 裁决一致。
                (TargetOperatorRecipe::Operands, OvmOperatorCode::SwapCompare(code)) => {
                    let reversed: Vec<ValkyrieMirOperand> = arguments.iter().rev().cloned().collect();
                    self.emit_external_arguments(&callee_name, instance, &reversed)?;
                    self.emitter.emit_plain(code);
                }
                // 比较后用分支取反：OVM 运行时比较头码压 `Value::Bool`，`I32Ne` 的
                // `pop_i32` 严格拒绝 Bool，按位取反组合不成立；改用 `JumpIfFalse`
                // （经 `to_bool` 消费比较结果）分岔到常量 1/0，两条路径在汇合标签处
                // 都留下一个 i32 结果。只用现有头码，不触宿主导入。
                (TargetOperatorRecipe::Operands, OvmOperatorCode::InvertBoolean(code)) => {
                    self.emit_external_arguments(&callee_name, instance, arguments)?;
                    self.emitter.emit_plain(code);
                    let true_label = self.emitter.alloc_label();
                    let end_label = self.emitter.alloc_label();
                    self.emitter.emit_jump_to_label(OdinHeadCode::JumpIfFalse, true_label);
                    let zero = self.emitter.intern_i32(0);
                    self.emitter.emit_imm1(OdinHeadCode::Constant, zero);
                    self.emitter.emit_jump_to_label(OdinHeadCode::Jump, end_label);
                    self.emitter.emit_label(true_label);
                    let one = self.emitter.intern_i32(1);
                    self.emitter.emit_imm1(OdinHeadCode::Constant, one);
                    self.emitter.emit_label(end_label);
                }
                (TargetOperatorRecipe::Steps(steps), OvmOperatorCode::Single(code)) => {
                    for step in steps {
                        match step {
                            TargetOperandStep::Arguments => self.emit_external_arguments(&callee_name, instance, arguments)?,
                            TargetOperandStep::IntegerConstant { operand_type: TargetOperandType::I32, value } => {
                                let index = self.emitter.intern_i32(*value);
                                self.emitter.emit_imm1(OdinHeadCode::Constant, index);
                            }
                            TargetOperandStep::IntegerConstant { operand_type, .. } => {
                                return Err(self.fail(format!(
                                    "Nyar 目标 `ovm_operator` 绑定 `{mnemonic}` 冻结了非 i32 常量配方 {operand_type:?}，OVM 没有该常量载体: callee=`{callee_name}`"
                                )));
                            }
                        }
                    }
                    self.emitter.emit_plain(code);
                }
                (TargetOperatorRecipe::Steps(_), _) => {
                    return Err(self.fail(format!(
                        "Nyar 目标 `ovm_operator` 绑定 `{mnemonic}` 的组合编码不得携带操作数重写配方，拒绝隐式组合: callee=`{callee_name}`"
                    )));
                }
            }
        }
        else {
            let Some((_, symbol)) = select_unique_target(links, ovm_host_import_target).map_err(|reason| self.fail(reason))?
            else {
                let declared = links.iter().map(|link| link.to_string()).collect::<Vec<_>>().join(", ");
                return Err(self.fail(format!(
                    "Nyar 目标不支持外部调用 `{callee_name}`：链接合同 {declared}（contract/tag）不是已绑定的目标 provider；\
                     需要 host 边界 + 目标标签 `ovm-operator` + 恰好一段符号，未绑定目标 provider 不得回落"
                )));
            };
            self.emit_external_arguments(&callee_name, instance, arguments)?;
            // 调用 arity 取自已冻结的 `BackendImport` 签名，不取调用方实参数量：
            // 缺签名事实即失败闭合，实参数量与签名 arity 不符（少传同样）也在此拒绝，
            // 否则「查不到 arity 就用 arguments.len()」会让缺失事实通过该次检查。
            let Some(frozen_arity) = self.services.external_param_types_by_instance.get(&instance).map(Vec::len) else {
                return Err(self.fail(format!(
                    "OVM 外部调用 `{callee_name}`（instance={instance:?}）缺少已冻结的导入签名：无法确定调用 arity，禁止回落到调用方实参数量"
                )));
            };
            if arguments.len() != frozen_arity {
                return Err(self.fail(format!(
                    "OVM 外部调用 `{callee_name}`（instance={instance:?}）实参数量 {} 与冻结导入签名 arity {frozen_arity} 不符",
                    arguments.len()
                )));
            }
            self.emitter.emit_call_import(symbol, frozen_arity as i32);
        }

        if let Some(output) = output {
            self.store_to_local(output);
        }
        else {
            self.emitter.emit_plain(OdinHeadCode::Pop);
        }
        Ok(())
    }

    /// 按计划冻结的 `BackendImport.parameter_types` 发射外部调用实参。
    ///
    /// 位宽权威与内部调用同源（签名表），此处不按字面量值域猜测：签名表缺项时
    /// 结构化失败，绝不回落到 semantic MIR 的 `Integer64` 默认。
    fn emit_external_arguments(&mut self, callee_name: &str, instance: ItemInstanceId, arguments: &[ValkyrieMirOperand]) -> Result<(), TargetPlanError> {
        let parameter_types = self.services.external_param_types_by_instance.get(&instance).cloned();
        let import_symbol = self
            .services
            .external_link_by_instance
            .get(&instance)
            .map(|links| links.iter().map(|link| link.locator_segments().join("::")).collect::<Vec<_>>().join(" | "))
            .unwrap_or_default();
        for (index, argument) in arguments.iter().enumerate() {
            let actual = self.operand_actual_type(argument);
            let Some(expected) = parameter_types.as_ref().and_then(|types| types.get(index)).cloned()
            else {
                return Err(self.fail(format!(
                    "OVM 外部调用 `{callee_name}`（import={import_symbol} instance={instance:?}）的实参 #{index} 缺少已规划签名类型：\
                     期望类型=<`BackendImport.parameter_types` 未覆盖该参数>，实际类型={actual}；\
                     禁止回落到 semantic MIR 的 `Integer64` 默认"
                )));
            };
            if matches!(expected, SemanticType::Utf8) {
                let site = format!("OVM 外部调用 `{callee_name}`（import={import_symbol} instance={instance:?}）的实参 #{index}");
                // 宿主边界 ABI：实参槽是 raw 宿主串，托管文本必须先抽取成 raw。
                self.emit_text_boundary_operand(argument, &expected, TextSlotTarget::Raw, &site)?;
                continue;
            }
            match argument {
                ValkyrieMirOperand::Value(_) => {
                    if !self.emit_call_operand(argument)? {
                        return Err(self.fail(format!("unmaterialized Nyar external call operand in `{callee_name}`: {argument:?}")));
                    }
                }
                ValkyrieMirOperand::Constant(_) => self.emit_operand_typed(argument, Some(&expected)).map_err(|error| match error {
                    TargetPlanError::Unsound { reason, .. } => self.fail(format!(
                        "OVM 外部调用 `{callee_name}`（import={import_symbol} instance={instance:?}）的实参 #{index} 发射失败：\
                         期望类型={expected}，实际类型={actual}；{reason}"
                    )),
                    other => other,
                })?,
            }
        }
        Ok(())
    }

    /// 实参当前的实际载体/字面量描述，只用于结构化失败文本，不参与发射决策。
    fn operand_actual_type(&self, operand: &ValkyrieMirOperand) -> String {
        match operand {
            ValkyrieMirOperand::Value(value) => self
                .value_slot_type(*value)
                .or_else(|| self.value_semantic_type(*value))
                .map(|ty| ty.to_string())
                .unwrap_or_else(|| format!("%{}（未冻结类型）", value.index())),
            ValkyrieMirOperand::Constant(constant) => format!("{constant:?}（无槽字面量）"),
        }
    }

    fn emit_direct_call(
        &mut self,
        function_index: i32,
        path: &NamePath,
        arguments: &[ValkyrieMirOperand],
        output: Option<MirValueId>,
    ) -> Result<(), TargetPlanError> {
        // 入口 arity 是被调函数入口块 SSA 形参的冻结事实；查不到即模块构造
        // 不一致，必须结构化失败，绝不回落到调用方实参数量让缺失事实通过检查。
        let expected = self.services.function_entry_arities.get(&function_index).copied().ok_or_else(|| {
            self.fail(format!("OVM 内部调用 `{path:?}`（function index={function_index}）缺少已冻结的入口 arity：禁止回落到调用方实参数量"))
        })?;
        if arguments.len() != expected {
            return Err(self.fail(format!(
                "OVM 内部调用 `{path:?}`（function index={function_index}）实参数量 {} 与冻结入口 arity {expected} 不符",
                arguments.len()
            )));
        }
        for (index, argument) in arguments.iter().enumerate() {
            // 常量实参的位宽来自被调函数已冻结的入口形参槽类型；没有该槽时
            // 才回落到 semantic MIR 合同，绝不按字面量大小选位宽。
            let param_ty = self.services.param_types_by_function_index.get(&function_index).and_then(|types| types.get(index)).cloned();
            if param_ty.as_ref() == Some(&SemanticType::Utf8) {
                let slot_layout =
                    self.services.param_layouts_by_function_index.get(&function_index).and_then(|layouts| layouts.get(index)).copied().flatten();
                // 入口槽没有布局身份时，只有程序级表示计划显式声明 raw 才是合法的
                // raw 槽事实；布局注册表为空不是「程序选择 raw」的证据，必须失败关闭。
                if slot_layout.is_none() && self.services.ctx.program_text_representation() != Some(ProgramTextRepresentation::Raw) {
                    return Err(self.fail(format!(
                        "OVM 内部调用 `{path:?}`（function index={function_index}）的文本实参 #{index} 未能解析入口槽的冻结 layout：\
                         程序级表示计划没有显式声明 raw 表示，同一签名槽必须解析到已冻结的托管布局身份，missing 事实不得解释为 raw"
                    )));
                }
                let target = match slot_layout {
                    Some(layout) => TextSlotTarget::Managed(layout),
                    None => TextSlotTarget::Raw,
                };
                let site = format!("内部调用 `{path:?}` 实参 #{index}");
                self.emit_text_boundary_operand(argument, &SemanticType::Utf8, target, &site)?;
                continue;
            }
            match argument {
                ValkyrieMirOperand::Value(_) => {
                    if !self.emit_call_operand(argument)? {
                        return Err(self.fail(format!("unmaterialized Nyar call operand in `{path:?}`: {argument:?}")));
                    }
                }
                ValkyrieMirOperand::Constant(_) => self.emit_operand_typed(argument, param_ty.as_ref())?,
            }
        }
        self.emitter.emit_imm1(OdinHeadCode::Call, function_index);
        if let Some(output) = output {
            self.store_to_local(output);
        }
        else {
            self.emitter.emit_plain(OdinHeadCode::Pop);
        }
        Ok(())
    }

    /// Call 实参只能读取已规划的 SSA/local 值；操作数位常量按 semantic MIR 合同发射。
    fn emit_call_operand(&mut self, operand: &ValkyrieMirOperand) -> Result<bool, TargetPlanError> {
        match operand {
            ValkyrieMirOperand::Value(value) => {
                let Some(local) = self.slots.value_local(value)
                else {
                    return Ok(false);
                };
                self.emit_load_local(local);
                Ok(true)
            }
            ValkyrieMirOperand::Constant(_) => {
                self.emit_operand(operand)?;
                Ok(true)
            }
        }
    }

    /// 字段布局槽位：只来自计划冻结的 `aggregate_layout_by_field` 表。
    fn nyar_field_slot(&self, field: FieldId) -> Result<i32, TargetPlanError> {
        self.services
            .ctx
            .aggregate_layout_by_field()
            .get(&field)
            .map(|(_, slot)| *slot as i32)
            .ok_or_else(|| self.fail(format!("OVM 字段身份缺少布局槽位合同：field={field:?}")))
    }

    fn ensure_nyar_layout(&mut self, aggregate: &ResolvedAggregateLayout) -> i32 {
        if let Some(index) = self.layout_index_by_id.get(&aggregate.id) {
            return *index;
        }
        let index = self.layouts.len() as i32;
        self.layouts.push(OdinLayout { field_count: aggregate.fields.len() as i32 });
        self.layout_index_by_id.insert(aggregate.id, index);
        index
    }

    /// 定长数组用的后端私有布局：Odin 对象只有字段数，没有字段类型，因此同字段数的
    /// 数组可以共享布局；身份只用于物理容纳，不参与语义匹配。
    fn ensure_nyar_layout_for_field_count(&mut self, field_count: i32) -> i32 {
        if let Some(index) = self.layout_index_by_field_count.get(&field_count) {
            return *index;
        }
        let index = self.layouts.len() as i32;
        self.layouts.push(OdinLayout { field_count });
        self.layout_index_by_field_count.insert(field_count, index);
        index
    }

    /// 注册阶段已为每个出现过的 nominal 铸造布局；此查询是后端自洽性护栏。
    fn ensure_sum_layout(&self, nominal: NominalInstanceId) -> Result<i32, TargetPlanError> {
        self.services
            .sum_layout_index_by_nominal
            .get(&nominal)
            .copied()
            .ok_or_else(|| self.fail(format!("OVM sum 表示缺失：nominal {nominal:?} 没有冻结的 SumTypeLayout")))
    }

    fn sum_variant_tag(&self, nominal: NominalInstanceId, variant: VariantId, operation: &str) -> Result<u32, TargetPlanError> {
        self.services
            .ctx
            .sum_representations()
            .get(&nominal)
            .ok_or_else(|| self.fail(format!("OVM sum 表示缺失：nominal {nominal:?} 没有冻结的 SumTypeLayout")))?
            .variants
            .get(&variant)
            .map(|variant_layout| variant_layout.tag)
            .ok_or_else(|| self.fail(format!("OVM {operation} 的 variant {variant:?} 不属于 nominal {nominal:?} 的冻结变体表")))
    }

    fn sum_variant_payload_type(
        &self,
        nominal: NominalInstanceId,
        variant: VariantId,
        operation: &str,
    ) -> Result<Option<TypeId>, TargetPlanError> {
        self.services
            .ctx
            .sum_representations()
            .get(&nominal)
            .ok_or_else(|| self.fail(format!("OVM sum 表示缺失：nominal {nominal:?} 没有冻结的 SumTypeLayout")))?
            .variants
            .get(&variant)
            .map(|variant_layout| variant_layout.payload_type)
            .ok_or_else(|| self.fail(format!("OVM {operation} 的 variant {variant:?} 不属于 nominal {nominal:?} 的冻结变体表")))
    }

    /// `FieldSet` 栈效果为 `[obj, value] -> [obj]`；调用方随后 `Pop`。
    /// `value_ty` 是制品槽的冻结类型（Odin 槽本身无类型），常量位宽必须由它决定。
    fn emit_field_set_slot(
        &mut self,
        object: &ValkyrieMirOperand,
        field_slot: i32,
        value: &ValkyrieMirOperand,
        value_ty: Option<&SemanticType>,
    ) -> Result<(), TargetPlanError> {
        self.emit_operand(object)?;
        // 嵌套槽是 raw 宿主串（目标合同显式声明，读取侧由表示冻结规则 2 同源分类）：
        // 托管文本实参入槽前必须抽取成 raw；否则字段槽里混入托管对象，读取方按
        // raw 解释就会在运行期炸出类型错。
        if value_ty == Some(&SemanticType::Utf8) {
            self.emit_text_boundary_operand(value, &SemanticType::Utf8, TextSlotTarget::Raw, "嵌套文本槽")?;
        }
        else {
            self.emit_operand_typed(value, value_ty)?;
        }
        self.emitter.emit_imm1(OdinHeadCode::FieldSet, field_slot);
        Ok(())
    }

    fn emit_block_argument_copies(&mut self, target: ValkyrieMirBlockId, arguments: &[ValkyrieMirOperand]) -> Result<(), TargetPlanError> {
        let target_block = self
            .function
            .blocks
            .get(&target)
            .ok_or_else(|| self.fail(format!("OVM 跳转目标块 {} 不在块表中", target.0)))?;
        if arguments.len() != target_block.parameters.len() {
            return Err(self.fail(format!(
                "OVM 块参数拷贝 arity 不符：目标块 {} 形参 {} 实参 {}",
                target.0,
                target_block.parameters.len(),
                arguments.len()
            )));
        }
        for (index, argument) in arguments.iter().enumerate() {
            let param_local = *self
                .slots
                .block_param_locals
                .get(&(target, index))
                .ok_or_else(|| self.fail(format!("OVM 块参数 (block {}, {index}) 缺少预规划槽", target.0)))?;
            let param_ty = self.slots.local_types[usize::from(param_local)].clone();
            // SSA 合并块参数是文本载体边界的一种：值位置 `if` 的一臂给 raw 字面量、另一臂给
            // 拼接产出的托管文本对象时，块参数槽按目标计划冻结的载体事实决定其物理表示。与
            // `Return`／内部调用实参共用 `emit_text_boundary_operand`，让 raw 入边物化为托管对象，
            // 否则下游 `self._repr` 字段访问会以 `expected object, got string` 崩溃。
            if matches!(param_ty, SemanticType::Utf8) {
                let parameter = target_block.parameters[index].0;
                let target = match self.text_representations.get(&parameter).copied() {
                    Some(TextCarrier::Managed(layout)) => TextSlotTarget::Managed(layout),
                    Some(TextCarrier::Raw) => TextSlotTarget::Raw,
                    None => {
                        return Err(self.fail(format!(
                            "OVM 块参数 %{} 缺少冻结的文本载体事实：表示计划必须为每个 `utf8` 槽给出 raw/managed 行，\
                             missing 事实不得解释为 raw",
                            parameter.index()
                        )));
                    }
                };
                self.emit_text_boundary_operand(argument, &param_ty, target, "块参数")?;
            }
            else {
                self.emit_operand_typed(argument, Some(&param_ty))?;
            }
        }
        for index in (0..arguments.len()).rev() {
            let param_local = *self
                .slots
                .block_param_locals
                .get(&(target, index))
                .ok_or_else(|| self.fail(format!("OVM 块参数 (block {}, {index}) 缺少预规划槽", target.0)))?;
            self.emit_store_local(param_local);
        }
        Ok(())
    }

    /// 发射常量。整数位宽只由目标槽的冻结类型决定：
    /// - `Some(Integer32)`：必须落在 i32 表示范围内，否则结构化失败；
    /// - `Some(Integer64)`：发 `Integer64` 载体；
    /// - `None`（操作数位没有槽上下文）：回落到 semantic MIR 合同
    ///   `Constant::Int(_) => Integer64 { signed: true }`，**不按数值大小选位宽**；
    /// - 其它槽类型：结构化失败。
    fn emit_load_constant(&mut self, constant: &ValkyrieMirConstant, slot_ty: Option<&SemanticType>) -> Result<(), TargetPlanError> {
        match constant {
            ValkyrieMirConstant::Int(value) => match slot_ty {
                Some(SemanticType::Integer32 { .. }) => {
                    let narrowed = i32::try_from(*value).map_err(|_| {
                        self.fail(format!(
                            "OVM 整数常量 {value} 超出目标 Integer32 槽的表示范围（slot={}）",
                            slot_ty.map(ToString::to_string).unwrap_or_else(|| "<none>".to_string())
                        ))
                    })?;
                    self.emitter.emit_const_i32(narrowed);
                }
                Some(SemanticType::Integer64 { .. }) | None => {
                    self.emitter.emit_const_i64(*value);
                }
                // 窄整数（`u8`/`i8`/`u16`/`i16`）与字符码位槽在 OVM 里以 i32 载体承载；
                // 位宽只影响后续转换，这里按 i32 常量发射（不得发 i64，运行期会拒绝）。
                Some(other) if ovm_int_shape(other).is_some_and(|shape| shape.bits <= 32) => {
                    let narrowed = i32::try_from(*value).map_err(|_| {
                        self.fail(format!("OVM 整数常量 {value} 超出目标 {other} 槽的 i32 载体范围"))
                    })?;
                    self.emitter.emit_const_i32(narrowed);
                }
                Some(other) => {
                    return Err(self.fail(format!(
                        "OVM 整数常量 {value} 与已规划槽类型不匹配：slot={other}；\
                         禁止把整数载荷塞进非整数槽"
                    )));
                }
            },
            ValkyrieMirConstant::Float64(bits) => {
                self.emitter.emit_const_f64(*bits);
            }
            ValkyrieMirConstant::Bool(value) => {
                self.emitter.emit_const_bool(*value);
            }
            ValkyrieMirConstant::Utf8(text) => {
                let index = self.emitter.intern_string(text);
                self.emitter.emit_imm1(OdinHeadCode::Constant, index);
            }
            ValkyrieMirConstant::Utf16(_) => {
                return Err(self.fail("OVM 常量 Utf16 缺少显式 UTF-16 文本 ABI 合同"));
            }
            // 可空引用 ABI 合同已成立：`OdinConstant::Null` 编码/解码齐备，VM 物化为
            // `Value::Null`，`IsNull`/`UnwrapNull` 内在指令与 JSON 桥双向 null 序列化均已接线。
            // 仍禁止 `i32 0` 冒充 null；非空消费者拿到 `Value::Null` 会按运行期类型错误失败闭合。
            ValkyrieMirConstant::Null => {
                self.emitter.emit_const_null();
            }
            ValkyrieMirConstant::Unit => {
                self.emitter.emit_const_i32(0);
            }
        }
        Ok(())
    }

    fn emit_operand(&mut self, operand: &ValkyrieMirOperand) -> Result<(), TargetPlanError> {
        self.emit_operand_typed(operand, None)
    }

    fn emit_operand_typed(&mut self, operand: &ValkyrieMirOperand, slot_ty: Option<&SemanticType>) -> Result<(), TargetPlanError> {
        match operand {
            ValkyrieMirOperand::Value(value) => {
                let local = self
                    .slots
                    .value_local(value)
                    .ok_or_else(|| self.fail(format!("OVM SSA 值缺少预规划槽：%{}", value.index())))?;
                self.emit_load_local(local);
                Ok(())
            }
            ValkyrieMirOperand::Constant(constant) => self.emit_load_constant(constant, slot_ty),
        }
    }

    /// SSA 值的物理槽类型（来自 `SlotPlan` 的冻结类型）。
    fn value_slot_type(&self, value: MirValueId) -> Option<SemanticType> {
        self.slots.value_local(&value).and_then(|local| self.slots.local_types.get(usize::from(local))).cloned()
    }

    /// SSA 值的语义类型（操作数位常量回落用的同一张冻结表）。
    fn value_semantic_type(&self, value: MirValueId) -> Option<SemanticType> {
        self.view.value_types.get(&value).cloned()
    }

    /// 文本操作数的冻结载体；非文本操作数返回 `None`。
    ///
    /// 只读发射前 `freeze_text_representations` 冻结的逐值事实表，不沿
    /// SSA 定义链恢复载体，也不看运行期 `Value` 的实际类型。字面量操作数本身
    /// 就是 OVM 常量 ABI 的宿主 raw 串（`OdinConstant::String`），不是定义链推断。
    fn operand_text_carrier(&self, operand: &ValkyrieMirOperand) -> Option<TextCarrier> {
        match operand {
            ValkyrieMirOperand::Value(value) => self.text_representations.get(value).copied(),
            ValkyrieMirOperand::Constant(ValkyrieMirConstant::Utf8(_)) => Some(TextCarrier::Raw),
            _ => None,
        }
    }

    /// 在文本边界（字段 / 数组元素 / sum 载荷 / 块参数 / 内部调用实参 / `Return` /
    /// 宿主导入实参）按冻结事实发射。
    ///
    /// `target = Managed(layout)` 表示目标槽是托管对象：raw 必须物化为该布局，
    /// 已托管对象必须与该布局同一身份。`target = Raw` 表示目标槽由目标合同固定为
    /// 宿主 raw 串：托管对象必须抽取成 raw。两种目标事实都必须显式给出，
    /// 「没有事实」在发射前的表示冻结里已结构化拒收。
    fn emit_text_boundary_operand(
        &mut self,
        operand: &ValkyrieMirOperand,
        expected: &SemanticType,
        target: TextSlotTarget,
        site: &str,
    ) -> Result<(), TargetPlanError> {
        if !matches!(expected, SemanticType::Utf8) {
            return self.emit_operand_typed(operand, Some(expected));
        }
        let Some(carrier) = self.operand_text_carrier(operand)
        else {
            return Err(self.fail(format!(
                "OVM 文本边界要求 `utf8` 操作数，实际类型 {}（{site}）：\
                 raw 宿主串与文本对象只在冻结的 `utf8` 载体边界上互转，禁止按名字或运行期类型猜探",
                self.operand_actual_type(operand)
            )));
        };
        match (target, carrier) {
            (TextSlotTarget::Managed(target), TextCarrier::Raw) => self.emit_text_materialize(target, operand, site),
            (TextSlotTarget::Managed(target), TextCarrier::Managed(actual)) if actual == target => self.emit_text_raw_operand(operand, site),
            (TextSlotTarget::Managed(target), TextCarrier::Managed(actual)) => Err(self.fail(format!(
                "OVM 文本载体布局不匹配：{} 的冻结 layout 是 {actual:?}，目标槽冻结 layout 是 {target:?}（{site}）；\
                 不同文本布局不得跨边界直接传递",
                self.operand_actual_type(operand)
            ))),
            (TextSlotTarget::Raw, TextCarrier::Managed(_)) => self.emit_text_extract(operand, site),
            (TextSlotTarget::Raw, TextCarrier::Raw) => self.emit_text_raw_operand(operand, site),
        }
    }

    /// 发射 raw 载体操作数（Value 读槽，常量按 `Utf8` 发射）。
    fn emit_text_raw_operand(&mut self, operand: &ValkyrieMirOperand, _site: &str) -> Result<(), TargetPlanError> {
        self.emit_operand_typed(operand, Some(&SemanticType::Utf8))
    }

    /// raw → 托管：`ObjectNew(layout)` → raw 串 → `utf8.to_bytes` → `FieldSet 0`。
    fn emit_text_materialize(&mut self, layout_id: LayoutId, operand: &ValkyrieMirOperand, site: &str) -> Result<(), TargetPlanError> {
        let layout = self.services.layouts.layout(layout_id).cloned().ok_or_else(|| {
            self.fail(format!(
                "OVM 文本载体 layout id={layout_id:?} 不在目标计划的布局表（{site}）：\
                 物化配方必须来自冻结布局，禁止猜测物理槽形状"
            ))
        })?;
        let layout_index = self.ensure_text_repr_layout(&layout, site)?;
        self.emitter.emit_imm1(OdinHeadCode::ObjectNew, layout_index);
        self.emit_text_raw_operand(operand, site)?;
        self.emitter.emit_call_import(TEXT_TO_BYTES_IMPORT, 1);
        self.emitter.emit_imm1(OdinHeadCode::FieldSet, 0);
        Ok(())
    }

    /// 托管 → raw：操作数 → `FieldGet 0` → `utf8.from_bytes`。
    fn emit_text_extract(&mut self, operand: &ValkyrieMirOperand, site: &str) -> Result<(), TargetPlanError> {
        self.emit_text_raw_operand(operand, site)?;
        self.emitter.emit_imm1(OdinHeadCode::FieldGet, 0);
        self.emitter.emit_call_import(TEXT_FROM_BYTES_IMPORT, 1);
        Ok(())
    }

    /// 校验并登记文本布局：唯一冻结配方是单个 `[u8]` 字段槽，返回 OVM 布局下标。
    fn ensure_text_repr_layout(&mut self, layout: &ResolvedAggregateLayout, site: &str) -> Result<i32, TargetPlanError> {
        let is_byte_slice = matches!(
            layout.fields.as_slice(),
            [field]
                if matches!(
                    &field.ty,
                    SemanticType::Array(element) | SemanticType::FixedArray { element, .. }
                        if matches!(element.as_ref(), SemanticType::Integer8 { signed: false })
                )
        );
        if !is_byte_slice {
            let shape = layout.fields.iter().map(|field| format!("{}:{}", field.name, field.ty)).collect::<Vec<_>>().join(", ");
            return Err(self.fail(format!(
                "OVM 文本载体 layout `{}`（id={:?}）缺少冻结的单一 `[u8]` 槽配方：实际 fields=[{shape}]（{site}）；\
                 禁止用非冻结形状物化或抽取文本",
                layout.name, layout.id
            )));
        }
        Ok(self.ensure_nyar_layout(layout))
    }

    /// 制品字段的冻结类型；Odin 槽无类型，常量位宽只能由布局合同给出。
    fn field_value_type(&self, field: FieldId) -> Option<SemanticType> {
        let (layout_id, slot) = self.services.ctx.aggregate_layout_by_field().get(&field).copied()?;
        self.services.layouts.layout(layout_id)?.fields.get(slot as usize).map(|field| field.ty.clone())
    }

    /// 数组元素类型：只从 SSA 值的冻结语义类型读取，不回查名字。
    fn array_element_type(&self, array: &ValkyrieMirOperand) -> Option<SemanticType> {
        let ValkyrieMirOperand::Value(value) = array
        else {
            return None;
        };
        match self.view.value_types.get(value)? {
            SemanticType::Array(element) | SemanticType::FixedArray { element, .. } => Some((**element).clone()),
            _ => None,
        }
    }

    fn store_to_local(&mut self, value: MirValueId) {
        let local = self.slots.value_locals[&value];
        self.emit_store_local(local);
    }

    fn emit_load_local(&mut self, local: u16) {
        self.emitter.emit_imm1(OdinHeadCode::LoadLocal, local as i32);
    }

    fn emit_store_local(&mut self, local: u16) {
        self.emitter.emit_imm1(OdinHeadCode::StoreLocal, local as i32);
    }

    /// 操作数的 semantic MIR 冻结类型。
    ///
    /// `Value` 读 [`Self::value_semantic_type`]；常量回落 `semantic_mir_contract`
    /// 的同一张合同（`Int → Integer64{signed}` 等）。
    fn operand_semantic_type(&self, operand: &ValkyrieMirOperand) -> Result<SemanticType, TargetPlanError> {
        match operand {
            ValkyrieMirOperand::Value(value) => self
                .value_semantic_type(*value)
                .ok_or_else(|| self.fail(format!("OVM `Cast` 源 SSA 值缺少冻结语义类型：%{}", value.index()))),
            ValkyrieMirOperand::Constant(constant) => match constant {
                ValkyrieMirConstant::Int(_) => Ok(SemanticType::Integer64 { signed: true }),
                ValkyrieMirConstant::Float64(_) => Ok(SemanticType::Float64),
                ValkyrieMirConstant::Bool(_) => Ok(SemanticType::Boolean),
                ValkyrieMirConstant::Utf8(_) => Ok(SemanticType::Utf8),
                ValkyrieMirConstant::Utf16(_) => Ok(SemanticType::Utf16),
                ValkyrieMirConstant::Unit => Ok(SemanticType::Unit),
                ValkyrieMirConstant::Null => Err(self.fail("OVM `Cast` 的空值操作数没有源类型，无法确定转换语义")),
            },
        }
    }

    /// `as` 的表示转换：只使用「源有符号性 + 目标位宽」的通用规则，不按类型名特判。
    ///
    /// 整数↔整数（含 `Character` 的 32 位无符号码位）、整数↔`f64` 与 `f32`↔`f64`
    /// 有确定语义；布尔/文本↔数值与整数↔`f32` 没有目标合同，必须结构化拒绝，
    /// 禁止编造近似转换或借另一宽度载体中转。
    fn emit_cast_conversion(&mut self, source_ty: &SemanticType, target_ty: &SemanticType) -> Result<(), TargetPlanError> {
        if source_ty == target_ty {
            return Ok(());
        }
        // 浮点宽度之间的转换是显式单步头码，不是载体恒等：
        // `f64 → f32` 用 `F32DemoteF64`（RNE，溢出 `inf`、下溢 0/次正规）；
        // `f32 → f64` 用 `F64PromoteF32`（精确无损）。两者都不经过其他精度。
        match (source_ty, target_ty) {
            (SemanticType::Float32, SemanticType::Float64) => {
                self.emitter.emit_plain(OdinHeadCode::F64PromoteF32);
                return Ok(());
            }
            (SemanticType::Float64, SemanticType::Float32) => {
                self.emitter.emit_plain(OdinHeadCode::F32DemoteF64);
                return Ok(());
            }
            // 整数↔f32 的 `as` 需要独立的转换指令/宿主合同，本轮未冻结；
            // 必须响亮失败，不得先转 f64 再降（double rounding）或借 i32 载体冒充。
            (_, SemanticType::Float32) => {
                return Err(self.fail(format!(
                    "OVM 后端没有 `{source_ty}` → `f32` 的确定转换语义：\
                     整数→`f32` 的精确单步转换尚未冻结，禁止先转 `f64` 再降回（double rounding）"
                )));
            }
            (SemanticType::Float32, _) => {
                return Err(self.fail(format!(
                    "OVM 后端没有 `f32` → `{target_ty}` 的确定转换语义：\
                     `f32` 是独立载体，禁止借 `f64` 中转或按 i32 位模式冒充数值转换"
                )));
            }
            _ => {}
        }
        if matches!(target_ty, SemanticType::Float64) {
            if let Some(source) = ovm_int_shape(source_ty) {
                self.emit_int_to_f64(source);
                return Ok(());
            }
        }
        if matches!(source_ty, SemanticType::Float64) {
            if let Some(target) = ovm_int_shape(target_ty) {
                self.emit_f64_to_int(target);
                return Ok(());
            }
        }
        let (Some(source), Some(target)) = (ovm_int_shape(source_ty), ovm_int_shape(target_ty))
        else {
            return Err(self.fail(format!(
                "OVM 后端没有 `{source_ty}` → `{target_ty}` 的确定转换语义：\
                 当前只冻结整数↔整数（含 `Character`）、整数↔`f64` 与 `f32`↔`f64` 的位宽/符号/截断规则；\
                 布尔/文本与数值之间的 `as` 缺少目标合同，不得退化为近似转换"
            )));
        };
        self.emit_int_cast_steps(source, target);
        Ok(())
    }

    /// 整数 → `f64` 的落地序列。
    ///
    /// 无符号窄整数先在 `i32` 载体上规范化（掩码成非负），再按符号性选择
    /// `i32_to_f64` / `u32_to_f64`，64 位同理选择 `i64_to_f64` / `u64_to_f64`。
    /// 有符号与无符号必须分开：混用会让 `u64` 的 2^63 以上值静默变成负数。
    fn emit_int_to_f64(&mut self, source: OvmIntShape) {
        if source.bits < 32 {
            self.emit_i32_canonicalize(source);
        }
        let symbol = match (source.bits == 64, source.signed) {
            (true, true) => "i64_to_f64",
            (true, false) => "u64_to_f64",
            (false, true) => "i32_to_f64",
            (false, false) => "u32_to_f64",
        };
        self.emitter.emit_call_import(symbol, 1);
    }

    /// `f64` → 整数的落地序列：checked 转换（朝零截断，NaN / 越界结构化失败），
    /// 窄目标再按目标位宽规范化（与整数收窄的位截断口径一致）。
    fn emit_f64_to_int(&mut self, target: OvmIntShape) {
        let symbol = match (target.bits == 64, target.signed) {
            (true, true) => "f64_to_i64_checked",
            (true, false) => "f64_to_u64_checked",
            (false, true) => "f64_to_i32_checked",
            (false, false) => "f64_to_u32_checked",
        };
        self.emitter.emit_call_import(symbol, 1);
        if target.bits < 32 {
            self.emit_i32_canonicalize(target);
        }
    }

    /// 整数表示转换的落地指令序列。
    ///
    /// 位宽 ≤ 32 的值以 `i32` 为载体（窄整数必须处于自身类型的规范形态），
    /// 位宽 64 以 `i64` 为载体。规则只看源的符号性与两端位宽：
    /// 拓宽按源符号性做符号/零扩展，收窄按目标位宽截断，等宽换符号性保持位模式。
    fn emit_int_cast_steps(&mut self, source: OvmIntShape, target: OvmIntShape) {
        if source.bits < 32 {
            self.emit_i32_canonicalize(source);
        }
        if target.bits == 64 {
            if source.bits == 64 {
                // `u64 ↔ i64`：载体与位模式不变，只有解释变。
            }
            else if source.bits == 32 && !source.signed {
                self.emit_u32_to_i64();
            }
            else {
                // 源已处于规范形态：有符号按符号扩展，窄无符号值非负，符号扩展即零扩展。
                self.emitter.emit_call_import("i32_to_i64", 1);
            }
        }
        else {
            if source.bits == 64 {
                self.emit_i64_truncate_to_i32();
            }
            if target.bits < 32 {
                self.emit_i32_canonicalize(target);
            }
        }
    }

    /// 把 `i32` 载体上的值按 8/16 位宽规范化：有符号做符号扩展，无符号做掩码截断。
    fn emit_i32_canonicalize(&mut self, shape: OvmIntShape) {
        debug_assert!(shape.bits == 8 || shape.bits == 16);
        let shift = 32 - i32::from(shape.bits);
        if shape.signed {
            // `(x << (32-b)) >>s (32-b)`：先保留低 b 位，再按第 b-1 位符号扩展。
            self.emitter.emit_const_i32(shift);
            self.emitter.emit_plain(OdinHeadCode::I32Shl);
            self.emitter.emit_const_i32(shift);
            self.emitter.emit_plain(OdinHeadCode::I32ShrS);
        }
        else {
            let mask = (1i32 << shape.bits) - 1;
            self.emitter.emit_const_i32(mask);
            self.emitter.emit_plain(OdinHeadCode::I32And);
        }
    }

    /// `i64` → `i32` 的位截断。
    ///
    /// 宿主的 `i64_to_i32` 带范围校验（溢出即结构化失败），直接调用会把 `as i32`
    /// 的位截断误编成运行期错误。先 `(x << 32) >>s 32` 把低 32 位符号扩展进
    /// `i32` 表示范围，再走范围校验必定成功的 `i64_to_i32`。
    fn emit_i64_truncate_to_i32(&mut self) {
        self.emitter.emit_const_i64(32);
        self.emitter.emit_call_import("i64_shl", 2);
        self.emitter.emit_const_i64(32);
        self.emitter.emit_call_import("i64_shr", 2);
        self.emitter.emit_call_import("i64_to_i32", 1);
    }

    /// `u32` → `i64` 的零扩展。
    ///
    /// `i32_to_i64` 恒为符号扩展；`u32` 值 ≥ 2^31 时以负数落在 `i32` 载体上，
    /// 半程必须补 2^32。无分支配方：`sext(x) + ((sext(x) >>>u 63) * 2^32)`。
    fn emit_u32_to_i64(&mut self) {
        self.emitter.emit_call_import("i32_to_i64", 1);
        self.emitter.emit_plain(OdinHeadCode::Dup);
        self.emitter.emit_const_i64(63);
        self.emitter.emit_call_import("i64_shr_u", 2);
        self.emitter.emit_const_i64(1i64 << 32);
        self.emitter.emit_call_import("i64_mul", 2);
        self.emitter.emit_call_import("i64_add", 2);
    }
}

/// 当前函数内 SSA 值 → 定义指令。仅供发射前的表示冻结扫描定义形态，
/// 发射期不得沿定义链恢复载体。
fn build_value_definitions(function: &ValkyrieMirFunction) -> BTreeMap<MirValueId, &ValkyrieMirInstruction> {
    let mut map = BTreeMap::new();
    for block in function.blocks.values() {
        for instruction in &block.instructions {
            if let Some(output) = SlotPlan::instruction_output(instruction) {
                map.insert(output, instruction);
            }
        }
    }
    map
}

/// 语义类型投影（与旧公共路径 `lower_type` 同一张 canonical 表）。
fn ctx_lower(ctx: &LaneContext, type_id: TypeId) -> Result<SemanticType, String> {
    ctx.lower_type(type_id)
}

/// 为库导出面结算 OVM ABI 合同的目标私有导出事实。
///
/// 入口形参槽布局与字节码发射共用同一派生点 [`frozen_entry_parameter_layouts`]；
/// `source_name` 是 `[export]` 冻结声明名，缺失即失败闭合，绝不拿逻辑名复制冒充。
pub(crate) fn abi_export_facts(
    ctx: &LaneContext,
    exports: &[(ItemInstanceId, String)],
) -> Result<Vec<compiler_emitter::ovm_abi::OvmAbiExportFacts>, TargetPlanError> {
    let mut facts = Vec::with_capacity(exports.len());
    for (instance, logical_name) in exports {
        let fail = |reason: String| TargetPlanError::Unsound { stage: AdapterStage::Function, instance: Some(*instance), symbol: None, reason };
        let view = ctx.view(instance).ok_or_else(|| fail(format!("库导出 `{logical_name}` 缺少 Compiler 函数体，无法派生 OVM ABI 合同")))?;
        let function = ctx.function(instance).expect("view 存在即带函数体");
        let slots = SlotPlan::plan(function, view).map_err(fail)?;
        let source_name = ctx
            .fragment()
            .export_names
            .get(instance)
            .cloned()
            .ok_or_else(|| fail(format!("库导出 `{logical_name}` 缺少计划冻结的源码声明名，无法派生 OVM ABI 合同")))?;
        facts.push(compiler_emitter::ovm_abi::OvmAbiExportFacts {
            logical_name: logical_name.clone(),
            source_name,
            param_types: view.param_types.clone(),
            param_names: view.parameter_names.clone(),
            return_type: view.return_type.clone(),
            entry_parameter_layouts: frozen_entry_parameter_layouts(ctx, function, view, &slots),
            return_layout: ctx.aggregate_layout_by_type().get(&function.return_type).copied(),
        });
    }
    Ok(facts)
}
