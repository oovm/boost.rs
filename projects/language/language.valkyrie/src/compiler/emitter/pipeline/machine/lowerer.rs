//! x86-64 lowering：直接消费 Valkyrie MIR，产出文本 LIR（[`X64ProgramFunction`]）。
//!
//! 该降低器归语言 × 机器码适配器所有：它读取语言自己的 SSA MIR 与冻结表示合同，
//! 产出目标私有的文本 LIR 函数，不经过任何跨目标公共 IR。字节图元、调用约定与
//! 容器编码仍由 emitter 的 x86-64 ISA 服务唯一实现；这里只决定「指令序列」。
//!
//! 只消费计划已冻结的事实：整数/布尔常量、SSA 局部槽、冻结 x86-64 operator、
//! 块标签与条件分支、同片段内部调用以及 `Return`。局部值统一占 8 字节栈槽
//! （`[rbp - 8 * (slot + 1)]`）；块参数与实参周转使用局部槽之后的临时帧槽。
//! 未冻结或超出边界的形状一律结构化失败。

use std::collections::BTreeMap;

use compiler_emitter::{
    emitter_backend_machine::{
        ARRAY_ELEMENT_BYTES, ARRAY_ELEMENT_OFFSET, SUM_CARRIER_BYTES, SUM_PAYLOAD_OFFSET, ArrayAllocationSlots, ArrayAllocator,
        ResolvedLayoutPlan, X64ArrayOperation, X64ArrayOperatorPlan, X64CallConv, X64ConsoleOperation, X64FloatCast, X64Platform,
        X64ProgramFunction, X64TextInstruction, X86OperatorBinding, X86_64OperatorCode, array_allocate, array_bounds_check,
        array_failure_exit, array_free, narrow_normalize, object_allocate, select_unique_target,
    },
    target_operator::TargetOperatorRecipe,
};
use super::operator::{float_int_target_bounds, freeze_x86_64_operator, heap_array_element_type, is_local_carrier_type, is_reference_carrier, x64_int_shape, x86_64_operator_target};
use compiler_types::{FieldId, ItemInstanceId, LayoutId, NominalInstanceId, SemanticType, StorageKind, VariantId, semantic_ids::MirValueId};

use crate::compiler::valkyrie_mir::{
    UTF8_TEXT_CARRIER_LAYOUT, ValkyrieMirArrayInitialization, ValkyrieMirBlock, ValkyrieMirBlockId, ValkyrieMirCallee, ValkyrieMirConstant,
    ValkyrieMirEffectKind, ValkyrieMirFunction, ValkyrieMirInstruction, ValkyrieMirOperation, ValkyrieMirTerminator,
};
use super::super::lane::{FunctionView, LaneContext};

/// 实参的寄存器落位：整数与浮点各自记录序位与宽度。
enum FloatPlacement {
    Integer { index: usize },
    Float { index: usize, single: bool },
}

/// MIR 函数的局部槽规划：与旧 `ExecutableSlotPlan::plan_semantic` 相同的分配顺序
/// （入口块形参 → 其余块形参按块号升序 → 指令结果按块序与指令序），保证帧布局
/// 与镜像字节和旧路径一致。
pub(crate) struct SlotPlan {
    local_types: Vec<SemanticType>,
    value_locals: BTreeMap<MirValueId, u16>,
    block_param_locals: BTreeMap<(ValkyrieMirBlockId, usize), u16>,
}

impl SlotPlan {
    fn plan(function: &ValkyrieMirFunction, view: &FunctionView) -> Result<Self, String> {
        let mut local_types = Vec::new();
        let mut value_locals: BTreeMap<MirValueId, u16> = BTreeMap::new();
        let mut block_param_locals = BTreeMap::new();
        let mut visit_block = |block_id: ValkyrieMirBlockId, block: &ValkyrieMirBlock| -> Result<(), String> {
            for (index, (parameter, _)) in block.parameters.iter().enumerate() {
                let ty = view
                    .value_types
                    .get(parameter)
                    .cloned()
                    .ok_or_else(|| format!("x86_64 lowering：块参数 {parameter:?} 缺少冻结语义类型：function={}", view.symbol))?;
                let slot = u16::try_from(local_types.len()).map_err(|_| "x86_64 lowering：局部槽数量超出 `u16`".to_string())?;
                local_types.push(ty);
                if value_locals.insert(*parameter, slot).is_some() {
                    return Err(format!("x86_64 lowering：SSA 值 {parameter:?} 重复定义，拒绝双槽规划：function={}", view.symbol));
                }
                block_param_locals.insert((block_id, index), slot);
            }
            for instruction in &block.instructions {
                for output in &instruction.results {
                    let ty = view
                        .value_types
                        .get(output)
                        .cloned()
                        .ok_or_else(|| format!("x86_64 lowering：SSA 值 {output:?} 缺少冻结语义类型：function={}", view.symbol))?;
                    let slot = u16::try_from(local_types.len()).map_err(|_| "x86_64 lowering：局部槽数量超出 `u16`".to_string())?;
                    local_types.push(ty);
                    if value_locals.insert(*output, slot).is_some() {
                        return Err(format!("x86_64 lowering：SSA 值 {output:?} 重复定义，拒绝双槽规划：function={}", view.symbol));
                    }
                }
            }
            Ok(())
        };
        let entry = function
            .blocks
            .get(&function.entry)
            .ok_or_else(|| format!("x86_64 lowering：函数 `{}` 缺少入口块", view.symbol))?;
        visit_block(function.entry, entry)?;
        for (block_id, block) in &function.blocks {
            if *block_id == function.entry {
                continue;
            }
            visit_block(*block_id, block)?;
        }
        Ok(Self { local_types, value_locals, block_param_locals })
    }

    fn instruction_output(&self, instruction: &ValkyrieMirInstruction) -> Option<MirValueId> {
        instruction.results.first().copied()
    }
}

/// 单函数降低器：把 MIR 事实转成文本 LIR，不产生任何字节。
struct FunctionLowerer<'a> {
    ctx: &'a LaneContext<'a>,
    function: &'a ValkyrieMirFunction,
    view: &'a FunctionView,
    slots: SlotPlan,
    reachable: &'a [ItemInstanceId],
    /// 实例 → 镜像符号名。
    names: &'a BTreeMap<ItemInstanceId, String>,
    /// 由规范目标 `os`/`abi` 冻结的调用约定。
    abi: X64CallConv,
    /// 由同一平台身份冻结的数组分配/释放实现。
    allocator: ArrayAllocator,
    /// 目标已解析的物理布局：对象尺寸与字段偏移只来自这里。
    layouts: &'a ResolvedLayoutPlan,
    /// 局部槽数量；临时槽从该编号起分配。
    local_count: u16,
    /// 本函数同时需要的最大临时槽数。
    temps_used: usize,
    instructions: Vec<X64TextInstruction>,
    /// 数组失败出口标签（块标签之后的一个保留编号）。
    failure_label: u32,
    /// 动态标签分配游标（失败出口之后起，供展开循环等按调用点取唯一标签）。
    next_label: u32,
    /// 是否已经发射过数组失败分支（决定是否需要失败出口）。
    uses_failure_exit: bool,
}

impl FunctionLowerer<'_> {
    fn push(&mut self, instruction: X64TextInstruction) {
        self.instructions.push(instruction);
    }

    /// 预留 `count` 个临时帧槽（局部槽之后起分配，独立的周转序列之间复用），
    /// 返回首临时槽编号。
    fn reserve_temps(&mut self, count: usize) -> u16 {
        self.temps_used = self.temps_used.max(count);
        self.local_count
    }

    /// 第 `index` 个临时槽的帧槽编号。
    fn temp_slot(&self, base: u16, index: usize) -> Result<u16, String> {
        base.checked_add(u16::try_from(index).map_err(|_| "x86_64 lowering：临时槽序号溢出".to_string())?)
            .ok_or_else(|| "x86_64 lowering：临时槽溢出帧槽编号".to_string())
    }

    /// 分配一个函数内唯一标签（失败出口之后起，同函数多次展开调用互不冲突）。
    fn fresh_label(&mut self) -> u32 {
        let label = self.next_label;
        self.next_label += 1;
        label
    }

    fn store_entry_parameters(&mut self) -> Result<(), String> {
        let entry_block = self
            .function
            .blocks
            .get(&self.function.entry)
            .ok_or_else(|| format!("x86_64 lowering：函数 `{}` 缺少入口块", self.view.symbol))?;
        if entry_block.parameters.len() != self.view.param_types.len() {
            return Err(format!(
                "x86_64 lowering：函数 `{}` 入口块形参 {} 个与冻结签名 {} 个不一致",
                self.view.symbol,
                entry_block.parameters.len(),
                self.view.param_types.len()
            ));
        }
        let register_limit = if self.abi.shares_register_files() {
            self.abi.integer_argument_limit().max(self.abi.float_argument_limit())
        } else {
            self.abi.integer_argument_limit() + self.abi.float_argument_limit()
        };
        if entry_block.parameters.len() > register_limit {
            return Err(format!(
                "x86_64 lowering：函数 `{}` 有 {} 个形参，超过 {} 寄存器传参上限（整数 {} + 浮点 {}；栈上传参未实现）",
                self.view.symbol,
                entry_block.parameters.len(),
                self.abi.name(),
                self.abi.integer_argument_limit(),
                self.abi.float_argument_limit()
            ));
        }
        // Win64 按形参位置同时消耗整数与浮点寄存器序位（第 i 个形参用 `rcx+i`
        // 或 `xmm0+i`）；SysV64 两个寄存器文件各自独立编号。混合签名不得按类别另算序位。
        let shared_files = self.abi.shares_register_files();
        let mut integer_index = 0u8;
        let mut float_index = 0u8;
        for (index, (parameter, _)) in entry_block.parameters.iter().enumerate() {
            let local = self
                .slots
                .block_param_locals
                .get(&(self.function.entry, index))
                .copied()
                .ok_or_else(|| format!("x86_64 lowering：函数 `{}` 入口形参 {index} 缺少局部槽", self.view.symbol))?;
            let ty = self
                .view
                .value_types
                .get(parameter)
                .or_else(|| self.view.param_types.get(index))
                .cloned();
            if let Some(ty) = &ty
                && !self.is_local_carrier(ty)
            {
                return Err(format!("x86_64 lowering：函数 `{}` 的形参类型 `{ty}` 没有 x86_64 槽载体", self.view.symbol));
            }
            if matches!(ty, Some(SemanticType::Float32 | SemanticType::Float64)) {
                let single = matches!(ty, Some(SemanticType::Float32));
                let register_index = if shared_files {
                    u8::try_from(index).map_err(|_| "x86_64 lowering：入口形参序位溢出 u8".to_string())?
                } else {
                    float_index
                };
                if usize::from(register_index) >= self.abi.float_argument_limit() {
                    return Err(format!(
                        "x86_64 lowering：函数 `{}` 的浮点形参超过 {} 寄存器传参上限 {}（栈上传参未实现）",
                        self.view.symbol,
                        self.abi.name(),
                        self.abi.float_argument_limit()
                    ));
                }
                self.push(X64TextInstruction::LoadParamXmm { index: register_index, slot: local, single });
                float_index += 1;
            } else {
                let register_index = if shared_files {
                    u8::try_from(index).map_err(|_| "x86_64 lowering：入口形参序位溢出 u8".to_string())?
                } else {
                    integer_index
                };
                if usize::from(register_index) >= self.abi.integer_argument_limit() {
                    return Err(format!(
                        "x86_64 lowering：函数 `{}` 的整数形参超过 {} 寄存器传参上限 {}（栈上传参未实现）",
                        self.view.symbol,
                        self.abi.name(),
                        self.abi.integer_argument_limit()
                    ));
                }
                self.push(X64TextInstruction::LoadParam { index: register_index, slot: local });
                integer_index += 1;
            }
        }
        Ok(())
    }

    fn lower_block(&mut self, block: &ValkyrieMirBlock) -> Result<(), String> {
        self.push(X64TextInstruction::Label(block.id.0));
        for instruction in &block.instructions {
            self.lower_instruction(instruction)?;
        }
        self.lower_terminator(&block.terminator)
    }

    fn lower_instruction(&mut self, instruction: &ValkyrieMirInstruction) -> Result<(), String> {
        if instruction.results.len() > 1 {
            return Err(format!(
                "x86_64 lowering 不支持多结果指令（results={}）: function={}",
                instruction.results.len(),
                self.view.symbol
            ));
        }
        match &instruction.operation {
            ValkyrieMirOperation::LoadConstant { constant } => {
                let local = self.require_output_local(instruction)?;
                // 浮点常量按结果槽的冻结宽度落位：`f64` 常量进 `f32` 槽时按目标精度
                // 舍入，避免把 64 位位模式原样塞进 32 位语义的槽。
                if let ValkyrieMirConstant::Float64(bits) = constant {
                    let single = matches!(self.slots.local_types.get(local as usize), Some(SemanticType::Float32));
                    let bits = if single { u64::from((f64::from_bits(*bits) as f32).to_bits()) } else { *bits };
                    self.push(X64TextInstruction::LoadI64(bits as i64));
                    self.push(X64TextInstruction::StoreSlot(local));
                    return Ok(());
                }
                self.emit_constant_to_rax(constant)?;
                self.push(X64TextInstruction::StoreSlot(local));
                Ok(())
            }
            ValkyrieMirOperation::Copy { source } => {
                let local = self.require_output_local(instruction)?;
                self.emit_operand_to_rax(*source)?;
                self.push(X64TextInstruction::StoreSlot(local));
                Ok(())
            }
            ValkyrieMirOperation::StoreVar { value, .. } => {
                if self.output_local(instruction)?.is_none() {
                    // 具名局部不在 native 后端物化；操作数本身是纯值，丢弃不产生副作用。
                    return Ok(());
                }
                let local = self.require_output_local(instruction)?;
                self.emit_operand_to_rax(*value)?;
                self.push(X64TextInstruction::StoreSlot(local));
                Ok(())
            }
            ValkyrieMirOperation::Cast { source, ty } => {
                // 语言层 `as` 是表示转换：转换语义只由计划冻结的两端 `SemanticType`
                // 决定（槽位宽与符号性无法从 64 位帧槽反推），不得退化为 Copy。
                let local = self.require_output_local(instruction)?;
                let source_ty = self.operand_semantic_type(*source)?;
                let target = self
                    .ctx
                    .lower_type(*ty)
                    .map_err(|reason| format!("x86_64 lowering：`Cast` 目标类型投影失败：{reason}：function={}", self.view.symbol))?;
                self.emit_operand_to_rax(*source)?;
                self.emit_cast_conversion(&source_ty, &target)?;
                if matches!(target, SemanticType::Float32 | SemanticType::Float64) {
                    // 浮点目标的结果在 `xmm0`（整数目标在 `rax`）。
                    self.push(X64TextInstruction::StoreXmm0ToSlot { slot: local, single: matches!(target, SemanticType::Float32) });
                } else {
                    self.push(X64TextInstruction::StoreSlot(local));
                }
                Ok(())
            }
            ValkyrieMirOperation::Invoke { callee, arguments } => self.emit_call(instruction, callee, arguments),
            ValkyrieMirOperation::ArrayNew { array_type, length, initialization } => {
                let array_type = self
                    .ctx
                    .lower_type(*array_type)
                    .map_err(|reason| format!("x86_64 lowering：`ArrayNew` 类型投影失败：{reason}：function={}", self.view.symbol))?;
                let element = heap_array_element_type(&array_type).ok_or_else(|| {
                    format!("x86_64 lowering：`ArrayNew` 的类型 `{array_type}` 不是堆数组 `[T]`：function={}", self.view.symbol)
                })?;
                if !self.is_local_carrier(element) {
                    return Err(format!(
                        "x86_64 lowering：数组元素类型 `{element}` 没有 8 字节槽载体：function={}",
                        self.view.symbol
                    ));
                }
                match initialization {
                    ValkyrieMirArrayInitialization::Default => {
                        // `HeapAlloc(HEAP_ZERO_MEMORY)` 已把元素槽置零；标量元素的
                        // 语言默认值就是零，无需第二遍填充。
                    }
                    ValkyrieMirArrayInitialization::Fill(_) => {
                        return Err(format!(
                            "x86_64 lowering 尚不支持 `ArrayNew` 的 `Fill` 初始化：function={}",
                            self.view.symbol
                        ));
                    }
                }
                let local = self.require_output_local(instruction)?;
                self.emit_array_allocation(*length)?;
                self.push(X64TextInstruction::StoreSlot(local));
                Ok(())
            }
            ValkyrieMirOperation::ArrayFromElements { array_type, elements } => {
                let array_type = self
                    .ctx
                    .lower_type(*array_type)
                    .map_err(|reason| format!("x86_64 lowering：`ArrayFromElements` 类型投影失败：{reason}：function={}", self.view.symbol))?;
                let element = heap_array_element_type(&array_type).ok_or_else(|| {
                    format!(
                        "x86_64 lowering：`ArrayFromElements` 的类型 `{array_type}` 不是堆数组 `[T]`：function={}",
                        self.view.symbol
                    )
                })?;
                if !self.is_local_carrier(element) {
                    return Err(format!(
                        "x86_64 lowering：数组元素类型 `{element}` 没有 8 字节槽载体：function={}",
                        self.view.symbol
                    ));
                }
                let count = i64::try_from(elements.len())
                    .map_err(|_| format!("x86_64 lowering：数组字面量长度超出 `i64`：function={}", self.view.symbol))?;
                let local = self.require_output_local(instruction)?;
                self.emit_array_allocation_constant(count)?;
                self.push(X64TextInstruction::StoreSlot(local));
                // 逐元素写入：值先落临时槽，避免常量求值借用 `rax` 覆盖数组指针。
                // 下标是编译期常量且恒小于刚分配的元素个数，范围证明成立，无需运行时检查。
                let base = self.reserve_temps(2);
                let value_slot = self.temp_slot(base, 0)?;
                let index_slot = self.temp_slot(base, 1)?;
                for (index, element) in elements.iter().enumerate() {
                    self.emit_operand_to_rax(*element)?;
                    self.push(X64TextInstruction::StoreSlot(value_slot));
                    self.push(X64TextInstruction::LoadI64(index as i64));
                    self.push(X64TextInstruction::StoreSlot(index_slot));
                    self.push(X64TextInstruction::LoadSlot(local));
                    self.push(X64TextInstruction::LoadSlotToRcx(index_slot));
                    self.push(X64TextInstruction::LoadSlotToRdx(value_slot));
                    self.push(X64TextInstruction::StoreIndexedRdxToRaxRcx);
                }
                Ok(())
            }
            ValkyrieMirOperation::ArrayLength { array } => {
                let local = self.require_output_local(instruction)?;
                self.require_heap_array_operand(*array)?;
                self.emit_operand_to_rax(*array)?;
                self.push(X64TextInstruction::LoadFromRax);
                self.push(X64TextInstruction::StoreSlot(local));
                Ok(())
            }
            ValkyrieMirOperation::ArrayGet { array, index } => {
                let local = self.require_output_local(instruction)?;
                self.require_heap_array_operand(*array)?;
                self.emit_array_bounds_check(*array, *index)?;
                // 下标先装载：常量下标会借用 `rax` 作暂存，数组指针必须最后装入。
                self.emit_operand_to_rcx(*index)?;
                self.emit_operand_to_rax(*array)?;
                self.push(X64TextInstruction::LoadIndexedFromRaxRcx);
                self.push(X64TextInstruction::StoreSlot(local));
                Ok(())
            }
            ValkyrieMirOperation::ArraySet { array, index, value } => {
                if self.output_local(instruction)?.is_some() {
                    return Err(format!("x86_64 lowering：`ArraySet` 不得绑定结果槽：function={}", self.view.symbol));
                }
                self.require_heap_array_operand(*array)?;
                self.emit_array_bounds_check(*array, *index)?;
                self.emit_operand_to_rcx(*index)?;
                self.emit_operand_to_rdx(*value)?;
                self.emit_operand_to_rax(*array)?;
                self.push(X64TextInstruction::StoreIndexedRdxToRaxRcx);
                Ok(())
            }
            ValkyrieMirOperation::StructNew { nominal, fields } => self.emit_struct_new(instruction, *nominal, fields),
            ValkyrieMirOperation::TupleNew { fields, .. } => {
                let result = instruction
                    .results
                    .first()
                    .copied()
                    .ok_or_else(|| format!("x86_64 lowering：TupleNew 缺少结果值：function={}", self.view.symbol))?;
                let result_type = self
                    .function
                    .value_types
                    .get(&result)
                    .copied()
                    .ok_or_else(|| format!("x86_64 lowering：聚合值 `{result:?}` 缺少 Semantic MIR 类型事实：function={}", self.view.symbol))?;
                if !matches!(
                    self.ctx.linked().types.get(&result_type).map(|row| &row.kind),
                    Some(compiler_types::CanonicalTypeKind::Tuple(_)) | Some(compiler_types::CanonicalTypeKind::LabeledTuple { .. })
                ) {
                    return Err(format!(
                        "x86_64 lowering：TupleNew 结果不是 tuple 类型: {result_type:?}: function={}",
                        self.view.symbol
                    ));
                }
                let layout_id = self
                    .ctx
                    .linked()
                    .aggregate_layout_by_type
                    .get(&result_type)
                    .copied()
                    .ok_or_else(|| format!("x86_64 lowering：TupleNew 缺少编译器绑定的布局身份: {result_type:?}: function={}", self.view.symbol))?;
                self.emit_tuple_new(instruction, layout_id, fields)
            }
            ValkyrieMirOperation::TupleGet { object, tuple_type, index } => {
                let layout_id = self
                    .ctx
                    .linked()
                    .aggregate_layout_by_type
                    .get(tuple_type)
                    .copied()
                    .ok_or_else(|| format!("x86_64 lowering：TupleGet 缺少编译器绑定的布局身份: {tuple_type:?}: function={}", self.view.symbol))?;
                self.emit_tuple_get(instruction, *object, layout_id, *index)
            }
            ValkyrieMirOperation::AggregateCopy { source, destination } => {
                let source_type = self
                    .function
                    .value_types
                    .get(source)
                    .copied()
                    .ok_or_else(|| format!("x86_64 lowering：聚合值 `{source:?}` 缺少 Semantic MIR 类型事实：function={}", self.view.symbol))?;
                let destination_type = self
                    .function
                    .value_types
                    .get(destination)
                    .copied()
                    .ok_or_else(|| format!("x86_64 lowering：聚合值 `{destination:?}` 缺少 Semantic MIR 类型事实：function={}", self.view.symbol))?;
                if source_type != destination_type {
                    return Err(format!(
                        "x86_64 lowering：AggregateCopy 源与目标类型不一致: {source_type:?} != {destination_type:?}: function={}",
                        self.view.symbol
                    ));
                }
                let layout_id = self
                    .ctx
                    .linked()
                    .aggregate_layout_by_type
                    .get(&source_type)
                    .copied()
                    .ok_or_else(|| format!("x86_64 lowering：AggregateCopy 缺少编译器绑定的布局身份: {source_type:?}: function={}", self.view.symbol))?;
                self.emit_aggregate_copy(instruction, layout_id, *source, *destination)
            }
            ValkyrieMirOperation::FieldGet { object, field } => self.emit_field_get(instruction, *object, *field),
            ValkyrieMirOperation::FieldSet { object, field, value } => self.emit_field_set(instruction, *object, *field, *value),
            ValkyrieMirOperation::SumNew { nominal, variant, payload } => self.emit_sum_new(instruction, *nominal, *variant, payload.as_ref()),
            ValkyrieMirOperation::SumVariantIs { nominal, variant, object } => {
                self.emit_sum_variant_is(instruction, *nominal, *variant, *object)
            }
            ValkyrieMirOperation::SumPayloadGet { nominal, variant, object } => {
                self.emit_sum_payload_get(instruction, *nominal, *variant, *object)
            }
            ValkyrieMirOperation::NullIsNull { value } => {
                let local = self.require_output_local(instruction)?;
                let base = self.reserve_temps(2);
                let value_slot = self.temp_slot(base, 0)?;
                let zero_slot = self.temp_slot(base, 1)?;
                self.emit_operand_to_rax(*value)?;
                self.push(X64TextInstruction::StoreSlot(value_slot));
                self.push(X64TextInstruction::LoadI64(0));
                self.push(X64TextInstruction::StoreSlot(zero_slot));
                self.push(X64TextInstruction::LoadSlot(value_slot));
                self.push(X64TextInstruction::LoadSlotToRcx(zero_slot));
                self.push(X64TextInstruction::Operator(X86_64OperatorCode::I64CompareEqual));
                self.push(X64TextInstruction::StoreSlot(local));
                Ok(())
            }
            ValkyrieMirOperation::NullUnwrap { value } => {
                let local = self.require_output_local(instruction)?;
                let ty = self.operand_semantic_type(*value)?;
                match &ty {
                    SemanticType::Nullable(inner)
                        if matches!(
                            **inner,
                            SemanticType::Nominal { .. } | SemanticType::Apply(..) | SemanticType::Array(_) | SemanticType::Union(_)
                                | SemanticType::Utf8
                                | SemanticType::Utf16
                        ) => {}
                    _ => {
                        return Err(format!(
                            "x86_64 lowering：`NullUnwrap` 首期只开放引用可空（值可空需要装箱合同）：type={ty} function={}",
                            self.view.symbol
                        ));
                    }
                }
                // 引用可空：非空值就是同一个指针，语言已保证非空。
                self.emit_operand_to_rax(*value)?;
                self.push(X64TextInstruction::StoreSlot(local));
                Ok(())
            }
            ValkyrieMirOperation::LoadFunctionRef { .. } => {
                Err(format!("x86_64 lowering 尚不支持指令 `LoadFunctionRef`：function={}", self.view.symbol))
            }
        }
    }

    fn lower_terminator(&mut self, terminator: &ValkyrieMirTerminator) -> Result<(), String> {
        match terminator {
            ValkyrieMirTerminator::Return { value } => self.emit_return(value.as_ref()),
            ValkyrieMirTerminator::Jump { target, arguments } => self.emit_jump(*target, arguments),
            ValkyrieMirTerminator::Branch { condition, then_target, else_target } => {
                let targets_have_parameters = [then_target, else_target]
                    .into_iter()
                    .filter_map(|target| self.function.blocks.get(target))
                    .any(|block| !block.parameters.is_empty());
                if targets_have_parameters {
                    return Err(format!("x86_64 lowering 只支持无块参数的条件分支：function={}", self.view.symbol));
                }
                self.emit_operand_to_rax(*condition)?;
                self.push(X64TextInstruction::JumpIfZero(else_target.0));
                self.push(X64TextInstruction::Jump(then_target.0));
                Ok(())
            }
            // 无人接手的 `raise`（panic 路径）：原生没有 effect 运行时，与 wasm 同构
            // 以 `ud2` 终止；不伪造返回，也不尝试格式化消息（原生文本载体尚未冻结）。
            ValkyrieMirTerminator::PerformEffect { effect: ValkyrieMirEffectKind::Raise, .. } => {
                self.push(X64TextInstruction::Trap);
                Ok(())
            }
            ValkyrieMirTerminator::Unreachable => {
                // `ud2`：到达即故障，不伪造返回。
                self.push(X64TextInstruction::Trap);
                Ok(())
            }
            other => Err(format!("x86_64 lowering 尚不支持终结符 `{other:?}`: function={}", self.view.symbol)),
        }
    }

    fn emit_return(&mut self, value: Option<&MirValueId>) -> Result<(), String> {
        let returns_unit = matches!(self.view.return_type, SemanticType::Unit | SemanticType::Bottom);
        match (value, returns_unit) {
            (None, true) => self.push(X64TextInstruction::LoadI64(0)),
            (None, false) => {
                return Err(format!("x86_64 lowering：非 void 函数 `{}` 缺少返回值", self.view.symbol));
            }
            (Some(value), false) => {
                if matches!(self.view.return_type, SemanticType::Float32 | SemanticType::Float64) {
                    let single = matches!(self.view.return_type, SemanticType::Float32);
                    let slot = self.emit_float_return_value(*value, single)?;
                    self.push(X64TextInstruction::LoadSlotToXmm0 { slot, single });
                } else {
                    self.emit_operand_to_rax(*value)?;
                }
            }
            (Some(_), true) => {
                return Err(format!("x86_64 lowering：函数 `{}` 返回了值但签名是 unit", self.view.symbol));
            }
        }
        // 尾声由程序装配器按 LIR `Return` 统一产生。
        self.push(X64TextInstruction::Return);
        Ok(())
    }

    fn emit_jump(&mut self, target: ValkyrieMirBlockId, arguments: &[MirValueId]) -> Result<(), String> {
        let target_block = self
            .function
            .blocks
            .get(&target)
            .ok_or_else(|| format!("x86_64 lowering：函数 `{}` 跳转目标块 {target:?} 不存在", self.view.symbol))?;
        if arguments.len() != target_block.parameters.len() {
            return Err(format!(
                "x86_64 lowering：跳转目标块 {target:?} 形参数量与实参不符: function={} expected={} actual={}",
                self.view.symbol,
                target_block.parameters.len(),
                arguments.len()
            ));
        }
        if arguments.len() > self.abi.integer_argument_limit() {
            return Err(format!(
                "x86_64 lowering：块 {target:?} 的跳转实参 {} 个超过临时槽上限 {}（栈上传参未实现）: function={}",
                arguments.len(),
                self.abi.integer_argument_limit(),
                self.view.symbol
            ));
        }
        // 先按序落到临时帧槽，再从临时槽装入目标块形参槽：
        // 这样源与目标槽重叠时也不会互相覆盖。
        let base = self.reserve_temps(arguments.len());
        for (index, argument) in arguments.iter().enumerate() {
            self.emit_operand_to_rax(*argument)?;
            let slot = self.temp_slot(base, index)?;
            self.push(X64TextInstruction::StoreSlot(slot));
        }
        for index in 0..arguments.len() {
            let local = self
                .slots
                .block_param_locals
                .get(&(target, index))
                .copied()
                .ok_or_else(|| format!("x86_64 lowering：跳转目标块 {target:?} 形参 {index} 缺少局部槽"))?;
            let slot = self.temp_slot(base, index)?;
            self.push(X64TextInstruction::LoadSlot(slot));
            self.push(X64TextInstruction::StoreSlot(local));
        }
        self.push(X64TextInstruction::Jump(target.0));
        Ok(())
    }

    fn emit_call(&mut self, instruction: &ValkyrieMirInstruction, callee: &ValkyrieMirCallee, arguments: &[MirValueId]) -> Result<(), String> {
        let ValkyrieMirCallee::Item(instance) = callee else {
            if let ValkyrieMirCallee::Witness(witness) = callee {
                // witness 必须在实例化固定点消去为 Item；到达目标层即合同破口。
                return Err(format!(
                    "canonical 未消去的 trait witness 分派到达目标层：trait={:?} method={:?} self_type={:?}；具体 witness 必须在实例化固定点冻结 `ItemInstanceId`",
                    witness.trait_declaration, witness.method_declaration, witness.self_type
                ));
            }
            return Err(format!(
                "x86_64 lowering 只支持 `Item` 身份的调用: function={} callee={callee:?}",
                self.view.symbol
            ));
        };
        if self.ctx.imports().contains_key(instance) {
            return self.emit_operator_call(instruction, *instance, arguments);
        }
        if !self.reachable.contains(instance) {
            return Err(format!(
                "x86_64 lowering：同片段调用目标 `{instance:?}` 不在可达函数集中: function={}",
                self.view.symbol
            ));
        }
        let callee_view = self
            .ctx
            .view(instance)
            .ok_or_else(|| format!("x86_64 lowering：调用目标 `{instance:?}` 缺少函数体: function={}", self.view.symbol))?;
        if callee_view.param_types.len() != arguments.len() {
            return Err(format!(
                "x86_64 lowering：调用 `{}` 实参数量与冻结签名不符: function={} expected={} actual={}",
                callee_view.symbol,
                self.view.symbol,
                callee_view.param_types.len(),
                arguments.len()
            ));
        }
        let register_limit = if self.abi.shares_register_files() {
            self.abi.integer_argument_limit().max(self.abi.float_argument_limit())
        } else {
            self.abi.integer_argument_limit() + self.abi.float_argument_limit()
        };
        if arguments.len() > register_limit {
            return Err(format!(
                "x86_64 lowering：调用 `{}` 传递 {} 个实参，超过 {} 寄存器传参上限（整数 {} + 浮点 {}；栈上传参未实现）",
                callee_view.symbol,
                arguments.len(),
                self.abi.name(),
                self.abi.integer_argument_limit(),
                self.abi.float_argument_limit()
            ));
        }
        // 整数与浮点实参的寄存器序位与入口形参同一规则：Win64 按形参位置共享
        // 序位，SysV64 各自独立编号。
        let shared_files = self.abi.shares_register_files();
        let mut placements = Vec::with_capacity(arguments.len());
        let mut integer_index = 0usize;
        let mut float_index = 0usize;
        for (index, argument) in arguments.iter().enumerate() {
            let ty = callee_view.param_types.get(index).cloned();
            if matches!(ty, Some(SemanticType::Float32 | SemanticType::Float64)) {
                let single = matches!(ty, Some(SemanticType::Float32));
                let register_index = if shared_files { index } else { float_index };
                if register_index >= self.abi.float_argument_limit() {
                    return Err(format!(
                        "x86_64 lowering：调用 `{}` 的浮点实参超过 {} 寄存器传参上限 {}（栈上传参未实现）",
                        callee_view.symbol,
                        self.abi.name(),
                        self.abi.float_argument_limit()
                    ));
                }
                placements.push(FloatPlacement::Float { index: register_index, single });
                float_index += 1;
            } else {
                let register_index = if shared_files { index } else { integer_index };
                if register_index >= self.abi.integer_argument_limit() {
                    return Err(format!(
                        "x86_64 lowering：调用 `{}` 的整数实参超过 {} 寄存器传参上限 {}（栈上传参未实现）",
                        callee_view.symbol,
                        self.abi.name(),
                        self.abi.integer_argument_limit()
                    ));
                }
                placements.push(FloatPlacement::Integer { index: register_index });
                integer_index += 1;
            }
            let _ = argument;
        }
        let name = self
            .names
            .get(instance)
            .cloned()
            .ok_or_else(|| format!("x86_64 lowering：调用目标 `{instance:?}` 缺少镜像符号名: function={}", self.view.symbol))?;
        // 与块参数周转同理：先全部落临时槽，再按序装入实参寄存器。
        let base = self.reserve_temps(arguments.len());
        for (index, argument) in arguments.iter().enumerate() {
            let slot = self.temp_slot(base, index)?;
            self.emit_operand_to_rax(*argument)?;
            self.push(X64TextInstruction::StoreSlot(slot));
        }
        for (slot_index, placement) in placements.iter().enumerate() {
            let slot = self.temp_slot(base, slot_index)?;
            match placement {
                FloatPlacement::Integer { index } => {
                    self.push(X64TextInstruction::LoadSlotToArg { slot, index: *index as u8 });
                }
                FloatPlacement::Float { index, single } => {
                    self.push(X64TextInstruction::LoadSlotToXmmArg { slot, index: *index as u8, single: *single });
                }
            }
        }
        self.push(X64TextInstruction::Call(name));

        if matches!(callee_view.return_type, SemanticType::Unit | SemanticType::Bottom) {
            if self.output_local(instruction)?.is_some() {
                return Err(format!(
                    "x86_64 lowering：void 调用 `{}` 不得绑定结果槽: function={}",
                    callee_view.symbol, self.view.symbol
                ));
            }
            return Ok(());
        }
        let local = self.require_output_local(instruction)?;
        if matches!(callee_view.return_type, SemanticType::Float32 | SemanticType::Float64) {
            let single = matches!(callee_view.return_type, SemanticType::Float32);
            self.push(X64TextInstruction::StoreXmm0ToSlot { slot: local, single });
        } else {
            self.push(X64TextInstruction::StoreSlot(local));
        }
        Ok(())
    }

    /// `[native_operator(...)]` 冻结绑定：只机械展开计划冻结的编码。
    fn emit_operator_call(&mut self, instruction: &ValkyrieMirInstruction, instance: ItemInstanceId, arguments: &[MirValueId]) -> Result<(), String> {
        let (binding, link, array_element) = {
            let import = self
                .ctx
                .imports()
                .get(&instance)
                .ok_or_else(|| format!("x86_64 lowering：导入实例 `{instance:?}` 缺失: function={}", self.view.symbol))?;
            let symbol = self.ctx.abi_name(&instance).map(|name| name.to_string()).unwrap_or_default();
            let selected = select_unique_target(&import.links, x86_64_operator_target)?;
            let binding = match selected {
                Some((link, _)) => freeze_x86_64_operator(link, instance, &symbol, &import.parameter_types, &import.return_type)?,
                None => None,
            };
            // 数组 operator 的元素载体必须由计划裁决：值存储聚合等虽然形状像指针，
            // 在本 lane 没有内联槽合同，不能当元素搬运。
            let array_element = match &binding {
                Some(X86OperatorBinding::Array(plan)) => {
                    let ty = match plan.operation {
                        X64ArrayOperation::New => &import.return_type,
                        X64ArrayOperation::Length | X64ArrayOperation::Get | X64ArrayOperation::Set => &import.parameter_types[0],
                    };
                    heap_array_element_type(ty).cloned()
                }
                _ => None,
            };
            let links = import.links.iter().map(|link| link.to_string()).collect::<Vec<_>>().join(", ");
            (binding, links, array_element)
        };
        if let Some(element) = &array_element
            && !self.is_local_carrier(element)
        {
            return Err(format!(
                "x86_64 lowering：数组 operator 元素类型 `{element}` 没有 8 字节槽载体：function={}",
                self.view.symbol
            ));
        }
        let Some(binding) = binding else {
            return Err(format!(
                "x86_64 lowering 尚不支持宿主导入 `{instance:?}`（链接 `{link}`）：缺少宿主导入编码，function={}",
                self.view.symbol
            ));
        };
        let plan = match binding {
            X86OperatorBinding::Array(plan) => return self.emit_array_operator_call(instruction, plan, arguments),
            X86OperatorBinding::Console(operation) => return self.emit_console_write(instruction, operation, arguments),
            X86OperatorBinding::Frozen(plan) => plan,
            X86OperatorBinding::Unfrozen => {
                return Err(format!(
                    "x86_64 目标 operator `{instance:?}` 绑定未冻结（链接 `{link}`）：拒绝补造指令: function={}",
                    self.view.symbol
                ));
            }
        };
        let code = plan.encoding;
        if arguments.len() != plan.arity || plan.arity != code.arity() {
            return Err(format!(
                "x86_64 目标 operator `{instance:?}` 实参数量与冻结 arity 不符: function={} expected={} actual={}",
                self.view.symbol,
                plan.arity,
                arguments.len()
            ));
        }
        if plan.recipe != TargetOperatorRecipe::Operands {
            return Err(format!(
                "x86_64 目标 operator `{instance:?}` 冻结了非 `Operands` 配方：x86_64 只执行机械实参发射: function={}",
                self.view.symbol
            ));
        }
        let expected_operand = code.operand_type();
        if plan.operand_type != expected_operand {
            return Err(format!(
                "x86_64 目标 operator `{instance:?}` 的 operand_type={:?} 与 x86_64 冻结码不符（期望 {expected_operand:?}）: function={}",
                plan.operand_type, self.view.symbol
            ));
        }
        let expected_result = code.result_type();
        if plan.result_type != expected_result {
            return Err(format!(
                "x86_64 目标 operator `{instance:?}` 的 result_type={:?} 与冻结码不符（期望 {expected_result:?}）: function={}",
                plan.result_type, self.view.symbol
            ));
        }
        let local = self.require_output_local(instruction)?;
        if code.is_float() {
            // 浮点载体：操作数在 `xmm0`/`xmm1`，比较结果回到 `rax` 的 0/1，
            // 其余结果留在 `xmm0` 并按其宽度写回 8 字节槽。
            let single = code.is_single_precision();
            self.emit_float_operand_to_xmm(0, arguments[0], single)?;
            if plan.arity == 2 {
                self.emit_float_operand_to_xmm(1, arguments[1], single)?;
            }
            self.push(X64TextInstruction::Operator(code));
            if code.is_comparison() {
                self.push(X64TextInstruction::StoreSlot(local));
            } else {
                self.push(X64TextInstruction::StoreXmm0ToSlot { slot: local, single });
            }
            return Ok(());
        }
        self.emit_operand_to_rax(arguments[0])?;
        if plan.arity == 2 {
            self.emit_operand_to_rcx(arguments[1])?;
        }
        self.push(X64TextInstruction::Operator(code));
        self.push(X64TextInstruction::StoreSlot(local));
        Ok(())
    }

    /// 把浮点操作数装入 `xmm0`/`xmm1`。
    ///
    /// MIR 操作数恒为 SSA 值：常量只经 `LoadConstant` 落槽，这里不再有常量分支。
    fn emit_float_operand_to_xmm(&mut self, register: u8, value: MirValueId, single: bool) -> Result<(), String> {
        let slot = self.value_local(value)?;
        match register {
            0 => self.push(X64TextInstruction::LoadSlotToXmm0 { slot, single }),
            1 => self.push(X64TextInstruction::LoadSlotToXmm1 { slot, single }),
            other => {
                return Err(format!(
                    "x86_64 lowering：浮点算子只使用 xmm0/xmm1，收到 xmm{other}: function={}",
                    self.view.symbol
                ));
            }
        }
        Ok(())
    }

    /// 浮点返回值落槽：值操作数直接复用其局部槽。
    fn emit_float_return_value(&mut self, value: MirValueId, _single: bool) -> Result<u16, String> {
        self.value_local(value)
    }

    fn emit_constant_to_rax(&mut self, constant: &ValkyrieMirConstant) -> Result<(), String> {
        match constant {
            ValkyrieMirConstant::Int(value) => {
                self.push(X64TextInstruction::LoadI64(*value));
                Ok(())
            }
            ValkyrieMirConstant::Float64(bits) => {
                // 浮点常量以位模式进入通用寄存器槽；消费方按宽度用 `movsd/movss` 装载。
                self.push(X64TextInstruction::LoadI64(*bits as i64));
                Ok(())
            }
            ValkyrieMirConstant::Bool(value) => {
                self.push(X64TextInstruction::LoadI64(i64::from(*value)));
                Ok(())
            }
            // 编译期文本物化为真实 `Utf8Text` 对象（见 `emit_text_constant`）。
            ValkyrieMirConstant::Utf8(value) => self.emit_text_constant(value),
            // `utf16` 复用静态描述符：本 lane 尚未闭合任何真实的 UTF-16 消费路径
            // （见 `is_local_carrier_type`），语言值图里不会出现读取其内容的边。
            ValkyrieMirConstant::Utf16(value) => {
                self.push(X64TextInstruction::LoadString(value.clone()));
                Ok(())
            }
            // 引用可空的空值就是空指针；标量可空没有内联载体，仍需装箱合同。
            ValkyrieMirConstant::Null => {
                self.push(X64TextInstruction::LoadI64(0));
                Ok(())
            }
            other => Err(format!(
                "x86_64 lowering 尚不支持常量 `{other:?}`：缺少 x86_64 常量载体合同，function={}",
                self.view.symbol
            )),
        }
    }

    /// 把编译期文本物化为 `Utf8Text` 对象：`{ [0] = _repr 数组指针 }`，数组按
    /// `[u8]` 合同逐字节占一个 8 字节槽（见 `array_runtime`）。
    ///
    /// 这是 guest 里语言文本值的唯一载体：std 的内容路径（`FieldGet _repr`、
    /// `equals_repr`、`byte_at`）都按对象读取，静态 `{self_rel,len}` 描述符只
    /// 留给宿主桥的镜像文本，不再充当语言值。分配失败走统一定义好的失败出口。
    fn emit_text_constant(&mut self, value: &str) -> Result<(), String> {
        let bytes = value.as_bytes();
        let base = self.reserve_temps(5);
        let count_slot = self.temp_slot(base, 0)?;
        let size_slot = self.temp_slot(base, 1)?;
        let auxiliary = self.temp_slot(base, 2)?;
        let condition = self.temp_slot(base, 3)?;
        let array_slot = self.temp_slot(base, 4)?;
        self.push(X64TextInstruction::LoadI64(bytes.len() as i64));
        self.uses_failure_exit = true;
        for item in array_allocate(
            self.allocator,
            ArrayAllocationSlots { count: count_slot, bytes: size_slot, auxiliary, condition },
            self.failure_label,
        ) {
            self.push(item);
        }
        self.push(X64TextInstruction::StoreSlot(array_slot));
        // 逐字节写入 8 字节元素槽（常量长度编译期已知，直接展开）。
        for (index, byte) in bytes.iter().enumerate() {
            self.push(X64TextInstruction::LoadI64(i64::from(*byte)));
            self.push(X64TextInstruction::StoreSlot(condition));
            self.push(X64TextInstruction::LoadSlot(array_slot));
            self.push(X64TextInstruction::LoadSlotToRdx(condition));
            self.push(X64TextInstruction::StoreRdxToRaxDisplacement(ARRAY_ELEMENT_OFFSET + 8 * index as i32));
        }
        // 对象块按语言结算的文本布局事实分配（宿主桥消费同一事实）。
        self.push(X64TextInstruction::LoadI64(ARRAY_ELEMENT_BYTES * UTF8_TEXT_CARRIER_LAYOUT.object_field_slots as i64));
        for item in object_allocate(self.allocator, size_slot, auxiliary, condition, self.failure_label) {
            self.push(item);
        }
        // `rax` 是对象指针，`rcx` 装入数组指针后写入 `_repr` 字段。
        self.push(X64TextInstruction::LoadSlotToRcx(array_slot));
        self.push(X64TextInstruction::StoreRcxToRaxDisplacement(UTF8_TEXT_CARRIER_LAYOUT.repr_field_byte_offset as i32));
        Ok(())
    }

    fn emit_operand_to_rax(&mut self, value: MirValueId) -> Result<(), String> {
        let local = self.value_local(value)?;
        self.push(X64TextInstruction::LoadSlot(local));
        Ok(())
    }

    /// 把操作数装入算子右操作数寄存器 `rcx`。
    fn emit_operand_to_rcx(&mut self, value: MirValueId) -> Result<(), String> {
        let local = self.value_local(value)?;
        self.push(X64TextInstruction::LoadSlotToRcx(local));
        Ok(())
    }

    /// 把操作数装入数组写入使用的 `rdx`（值槽）。
    fn emit_operand_to_rdx(&mut self, value: MirValueId) -> Result<(), String> {
        let local = self.value_local(value)?;
        self.push(X64TextInstruction::LoadSlotToRdx(local));
        Ok(())
    }

    /// 数组指令的操作数必须是堆数组 `[T]`。
    fn require_heap_array_operand(&self, value: MirValueId) -> Result<(), String> {
        let ty = self.operand_semantic_type(value)?;
        if heap_array_element_type(&ty).is_none() {
            return Err(format!(
                "x86_64 lowering：数组指令的操作数类型 `{ty}` 不是堆数组 `[T]`：function={}",
                self.view.symbol
            ));
        }
        Ok(())
    }

    /// 字段的冻结归属：布局身份、物理偏移与槽宽。
    fn field_physical(&self, field: FieldId) -> Result<(LayoutId, u32, u32), String> {
        let (layout_id, slot) = self
            .ctx
            .linked()
            .aggregate_layout_by_field
            .get(&field)
            .ok_or_else(|| format!("x86_64 lowering：字段 {field:?} 缺少冻结布局绑定：function={}", self.view.symbol))?;
        let layout = self
            .layouts
            .layout(*layout_id)
            .ok_or_else(|| format!("x86_64 lowering：字段 {field:?} 的布局 {layout_id:?} 缺少物理解析：function={}", self.view.symbol))?;
        let field_layout = layout
            .fields
            .get(*slot as usize)
            .ok_or_else(|| format!("x86_64 lowering：字段 {field:?} 的槽 {slot} 超出布局字段数：function={}", self.view.symbol))?;
        Ok((*layout_id, field_layout.offset, field_layout.size))
    }

    /// 引用存储聚合的物理布局；值存储聚合首期没有内联槽合同。
    fn reference_layout(&self, layout_id: LayoutId, operation: &str) -> Result<(u32, StorageKind, Vec<u32>), String> {
        let layout = self.layouts.layout(layout_id).ok_or_else(|| {
            format!("x86_64 lowering：{operation} 的布局 {layout_id:?} 缺少物理解析：function={}", self.view.symbol)
        })?;
        if !layout.storage.is_reference() {
            return Err(format!(
                "x86_64 lowering：{operation} 首期只开放引用存储聚合（值存储需要内联槽合同）：layout={} function={}",
                layout.name, self.view.symbol
            ));
        }
        Ok((layout.size, layout.storage, layout.fields.iter().map(|field| field.offset).collect()))
    }

    /// 操作数静态类型解析出的冻结聚合身份（引用/值存储都可能是聚合）。
    fn operand_layout_id(&self, value: MirValueId) -> Result<Option<LayoutId>, String> {
        let ty = self.operand_semantic_type(value)?;
        Ok(self.ctx.linked().aggregate_layouts.frozen_layout_id(&ty))
    }

    /// 冻结 sum 表示里的 variant tag；本 lane 物理 tag 固定为 4 字节槽。
    fn sum_variant_tag(&self, nominal: NominalInstanceId, variant: VariantId, operation: &str) -> Result<(u32, bool), String> {
        let rep = self
            .ctx
            .sum_representations()
            .get(&nominal)
            .ok_or_else(|| format!("x86_64 lowering：{operation} 的 sum 实例 {nominal:?} 缺少冻结表示：function={}", self.view.symbol))?;
        let variant_rep = rep
            .variants
            .get(&variant)
            .ok_or_else(|| format!("x86_64 lowering：{operation} 的 variant {variant:?} 不在 sum 表示内：function={}", self.view.symbol))?;
        Ok((variant_rep.tag, variant_rep.payload_type.is_some()))
    }

    /// `StructNew`：按物理布局分配零初始化对象，再逐字段写入。
    fn emit_struct_new(&mut self, instruction: &ValkyrieMirInstruction, nominal: NominalInstanceId, fields: &[(FieldId, MirValueId)]) -> Result<(), String> {
        let local = self.require_output_local(instruction)?;
        let layout_id = *self
            .ctx
            .linked()
            .aggregate_layout_by_nominal
            .get(&nominal)
            .ok_or_else(|| {
                format!("x86_64 lowering：`StructNew` 的名义实例 {nominal:?} 缺少冻结布局身份：function={}", self.view.symbol)
            })?;
        let (size, _, offsets) = self.reference_layout(layout_id, "`StructNew`")?;
        // 引用存储布局的 `size` 是「作为字段/句柄时占用的引用槽」，不是堆对象 body
        // 大小（共享解析合同，见 physical_layout 的自引用测试）。堆对象的实际字节数
        // 必须由字段物理区间的末端决定，否则多字段对象只分到 8 字节、字段写入即越界。
        let object_bytes = offsets.iter().copied().max().map(|offset| offset.max(8) + 8).unwrap_or(8);
        debug_assert!(object_bytes >= size || size == 8, "引用布局对象字节数不应小于引用槽");
        if offsets.iter().any(|offset| offset % 8 != 0) {
            return Err(format!(
                "x86_64 lowering：`StructNew` 布局 {layout_id:?} 含非 8 字节对齐字段，首期只开放 8 字节槽：function={}",
                self.view.symbol
            ));
        }
        if let Some(offset) = offsets.iter().find(|offset| **offset as u64 + 8 > object_bytes as u64) {
            return Err(format!(
                "x86_64 lowering：`StructNew` 布局 {layout_id:?} 的字段偏移 {offset} 越过对象大小 {object_bytes}：function={}",
                self.view.symbol
            ));
        }
        let mut slot_values: Vec<Option<MirValueId>> = vec![None; offsets.len()];
        for (field, value) in fields {
            let (owner, _, width) = self.field_physical(*field)?;
            if owner != layout_id {
                return Err(format!(
                    "x86_64 lowering：`StructNew` 字段 {field:?} 属于布局 {owner:?}，与构造布局 {layout_id:?} 冲突：function={}",
                    self.view.symbol
                ));
            }
            if width != 8 {
                return Err(format!(
                    "x86_64 lowering：`StructNew` 字段 {field:?} 宽 {width} 字节，首期只开放 8 字节槽字段：function={}",
                    self.view.symbol
                ));
            }
            let (_, layout_slot) = self
                .ctx
                .linked()
                .aggregate_layout_by_field
                .get(field)
                .copied()
                .ok_or_else(|| format!("x86_64 lowering：字段 {field:?} 缺少冻结槽位：function={}", self.view.symbol))?;
            let target = slot_values.get_mut(layout_slot as usize).ok_or_else(|| {
                format!(
                    "x86_64 lowering：`StructNew` 字段 {field:?} 的槽 {layout_slot} 越界（布局 {layout_id:?} 有 {} 个字段）：function={}",
                    offsets.len(),
                    self.view.symbol
                )
            })?;
            if target.replace(*value).is_some() {
                return Err(format!("x86_64 lowering：`StructNew` 字段 {field:?} 重复初始化：function={}", self.view.symbol));
            }
        }
        if let Some(index) = slot_values.iter().position(Option::is_none) {
            return Err(format!(
                "x86_64 lowering：`StructNew` 缺少字段槽 {index} 的初始化值（不静默补默认值）：layout={layout_id:?} function={}",
                self.view.symbol
            ));
        }
        let base = self.reserve_temps(4);
        let pointer_slot = self.temp_slot(base, 0)?;
        let size_slot = self.temp_slot(base, 1)?;
        let auxiliary = self.temp_slot(base, 2)?;
        let condition = self.temp_slot(base, 3)?;
        self.push(X64TextInstruction::LoadI64(object_bytes as i64));
        self.uses_failure_exit = true;
        for item in object_allocate(self.allocator, size_slot, auxiliary, condition, self.failure_label) {
            self.push(item);
        }
        self.push(X64TextInstruction::StoreSlot(pointer_slot));
        for (index, value) in slot_values.iter().enumerate() {
            let value = value.expect("字段完整性已校验");
            self.emit_operand_to_rdx(value)?;
            self.push(X64TextInstruction::LoadSlot(pointer_slot));
            self.push(X64TextInstruction::StoreRdxToRaxDisplacement(offsets[index] as i32));
        }
        self.push(X64TextInstruction::LoadSlot(pointer_slot));
        self.push(X64TextInstruction::StoreSlot(local));
        Ok(())
    }

    /// `FieldGet`：按冻结字段偏移读取一个 8 字节槽。
    fn emit_field_get(&mut self, instruction: &ValkyrieMirInstruction, object: MirValueId, field: FieldId) -> Result<(), String> {
        let local = self.require_output_local(instruction)?;
        let (layout_id, offset, width) = self.field_physical(field)?;
        self.reference_layout(layout_id, "`FieldGet`")?;
        if width != 8 {
            return Err(format!(
                "x86_64 lowering：`FieldGet` 字段 {field:?} 宽 {width} 字节，首期只开放 8 字节槽字段：function={}",
                self.view.symbol
            ));
        }
        if let Some(object_layout) = self.operand_layout_id(object)?
            && object_layout != layout_id
        {
            return Err(format!(
                "x86_64 lowering：`FieldGet` 字段 {field:?} 属于布局 {layout_id:?}，对象布局却是 {object_layout:?}：function={}",
                self.view.symbol
            ));
        }
        self.emit_operand_to_rax(object)?;
        self.push(X64TextInstruction::LoadFromRaxDisplacement(offset as i32));
        self.push(X64TextInstruction::StoreSlot(local));
        Ok(())
    }

    /// `FieldSet`：按冻结字段偏移写入一个 8 字节槽（`mut self` 修改同一实例）。
    fn emit_field_set(&mut self, instruction: &ValkyrieMirInstruction, object: MirValueId, field: FieldId, value: MirValueId) -> Result<(), String> {
        if self.output_local(instruction)?.is_some() {
            return Err(format!("x86_64 lowering：`FieldSet` 不得绑定结果槽：function={}", self.view.symbol));
        }
        let (layout_id, offset, width) = self.field_physical(field)?;
        self.reference_layout(layout_id, "`FieldSet`")?;
        if width != 8 {
            return Err(format!(
                "x86_64 lowering：`FieldSet` 字段 {field:?} 宽 {width} 字节，首期只开放 8 字节槽字段：function={}",
                self.view.symbol
            ));
        }
        if let Some(object_layout) = self.operand_layout_id(object)?
            && object_layout != layout_id
        {
            return Err(format!(
                "x86_64 lowering：`FieldSet` 字段 {field:?} 属于布局 {layout_id:?}，对象布局却是 {object_layout:?}：function={}",
                self.view.symbol
            ));
        }
        self.emit_operand_to_rdx(value)?;
        self.emit_operand_to_rax(object)?;
        self.push(X64TextInstruction::StoreRdxToRaxDisplacement(offset as i32));
        Ok(())
    }

    /// 一个元组元素槽的物理合同：偏移 8 字节对齐、与下一元素不重叠、
    /// 载体宽度与类型一致。
    ///
    /// 元组 SSA 值的载体是堆对象指针（8 字节），元素一律按 8 字节槽存取：
    /// 4 字节标量（`bool`/`char`/窄整数）在槽内按本 lane 的帧槽表示加宽，
    /// 高位置零由分配实现保证。浮点元素（载体寄存器类不同）与值存储聚合
    /// （没有内联槽合同）首期不开放，调用点结构化失败。
    fn tuple_field_offset(&self, layout_id: LayoutId, index: usize, operation: &str) -> Result<u32, String> {
        let layout = self.layouts.layout(layout_id).ok_or_else(|| {
            format!("x86_64 lowering：{operation} 的布局 {layout_id:?} 缺少物理解析：function={}", self.view.symbol)
        })?;
        let field = layout.fields.get(index).ok_or_else(|| {
            format!(
                "x86_64 lowering：{operation} 元素槽 {index} 超出布局字段数 {}：function={}",
                layout.fields.len(),
                self.view.symbol
            )
        })?;
        if field.offset % 8 != 0 {
            return Err(format!(
                "x86_64 lowering：{operation} 元素 {index} 偏移 {} 不是 8 字节对齐：function={}",
                field.offset, self.view.symbol
            ));
        }
        if let Some(next) = layout.fields.get(index + 1)
            && next.offset < field.offset + 8
        {
            return Err(format!(
                "x86_64 lowering：{operation} 元素 {index} 的 8 字节槽与下一元素重叠（offset={} next={}）：function={}",
                field.offset,
                next.offset,
                self.view.symbol
            ));
        }
        let width_ok = match &field.ty {
            SemanticType::Boolean | SemanticType::Character | SemanticType::Integer8 { .. } | SemanticType::Integer16 { .. } | SemanticType::Integer32 { .. } => field.size == 4,
            SemanticType::Integer64 { .. }
            | SemanticType::Array(_)
            | SemanticType::Utf8
            | SemanticType::Utf16
            | SemanticType::Union(_)
            | SemanticType::Nullable(_)
            | SemanticType::Nominal { .. }
            | SemanticType::Apply(..) => field.size == 8,
            other => {
                return Err(format!(
                    "x86_64 lowering：{operation} 元素 {index} 类型 `{other}` 没有本 lane 开放的元组槽合同：function={}",
                    self.view.symbol
                ));
            }
        };
        if !width_ok {
            return Err(format!(
                "x86_64 lowering：{operation} 元素 {index} 宽度 {} 与类型载体不符：function={}",
                field.size, self.view.symbol
            ));
        }
        Ok(field.offset)
    }

    /// `TupleNew`：分配零初始化的元组堆对象，按冻结偏移逐元素写入 8 字节槽。
    fn emit_tuple_new(&mut self, instruction: &ValkyrieMirInstruction, layout_id: LayoutId, fields: &[MirValueId]) -> Result<(), String> {
        let local = self.require_output_local(instruction)?;
        let field_count = self
            .layouts
            .layout(layout_id)
            .ok_or_else(|| format!("x86_64 lowering：`TupleNew` 的布局 {layout_id:?} 缺少物理解析：function={}", self.view.symbol))?
            .fields
            .len();
        if field_count != fields.len() {
            return Err(format!(
                "x86_64 lowering：`TupleNew` 元素个数 {} 与布局字段数 {field_count} 不符：function={}",
                fields.len(),
                self.view.symbol
            ));
        }
        let mut offsets = Vec::with_capacity(fields.len());
        for index in 0..fields.len() {
            offsets.push(self.tuple_field_offset(layout_id, index, "`TupleNew`")?);
        }
        let object_bytes = offsets.iter().copied().max().map(|offset| offset + 8).unwrap_or(8);
        let base = self.reserve_temps(4);
        let pointer_slot = self.temp_slot(base, 0)?;
        let size_slot = self.temp_slot(base, 1)?;
        let auxiliary = self.temp_slot(base, 2)?;
        let condition = self.temp_slot(base, 3)?;
        self.push(X64TextInstruction::LoadI64(object_bytes as i64));
        self.uses_failure_exit = true;
        for item in object_allocate(self.allocator, size_slot, auxiliary, condition, self.failure_label) {
            self.push(item);
        }
        self.push(X64TextInstruction::StoreSlot(pointer_slot));
        for (index, value) in fields.iter().enumerate() {
            self.emit_operand_to_rdx(*value)?;
            self.push(X64TextInstruction::LoadSlot(pointer_slot));
            self.push(X64TextInstruction::StoreRdxToRaxDisplacement(offsets[index] as i32));
        }
        self.push(X64TextInstruction::LoadSlot(pointer_slot));
        self.push(X64TextInstruction::StoreSlot(local));
        Ok(())
    }

    /// `TupleGet`：按冻结偏移读取一个 8 字节元素槽。
    fn emit_tuple_get(&mut self, instruction: &ValkyrieMirInstruction, object: MirValueId, layout_id: LayoutId, index: u32) -> Result<(), String> {
        let local = self.require_output_local(instruction)?;
        let offset = self.tuple_field_offset(layout_id, index as usize, "`TupleGet`")?;
        self.emit_operand_to_rax(object)?;
        self.push(X64TextInstruction::LoadFromRaxDisplacement(offset as i32));
        self.push(X64TextInstruction::StoreSlot(local));
        Ok(())
    }

    /// `AggregateCopy`：按冻结布局把聚合逐 8 字节槽复制进一个新堆对象。
    ///
    /// 值复制语义：结果指针指向新对象，与源不共享可变存储；元素槽合同与
    /// `tuple_field_offset` 一致（8 字节对齐、不重叠、宽度匹配），不满足的
    /// 布局（浮点、值存储内联体）结构化失败，不做按位浅拷贝冒充。
    fn emit_aggregate_copy(&mut self, instruction: &ValkyrieMirInstruction, layout_id: LayoutId, source: MirValueId, dest: MirValueId) -> Result<(), String> {
        // 目标槽与结果槽至少有一个：两者都绑定时写入同一个新对象指针（与 OVM 一致）。
        let output_local = self.output_local(instruction)?;
        let dest_local = Some(self.value_local(dest)?);
        if output_local.is_none() && dest_local.is_none() {
            return Err(format!(
                "x86_64 lowering：`AggregateCopy` 没有可写目标（目标与结果槽均缺失）：function={}",
                self.view.symbol
            ));
        }
        let field_count = self
            .layouts
            .layout(layout_id)
            .ok_or_else(|| format!("x86_64 lowering：`AggregateCopy` 的布局 {layout_id:?} 缺少物理解析：function={}", self.view.symbol))?
            .fields
            .len();
        let mut offsets = Vec::with_capacity(field_count);
        for index in 0..field_count {
            offsets.push(self.tuple_field_offset(layout_id, index, "`AggregateCopy`")?);
        }
        let object_bytes = offsets.iter().copied().max().map(|offset| offset + 8).unwrap_or(8);
        let base = self.reserve_temps(6);
        let source_slot = self.temp_slot(base, 0)?;
        let pointer_slot = self.temp_slot(base, 1)?;
        let size_slot = self.temp_slot(base, 2)?;
        let auxiliary = self.temp_slot(base, 3)?;
        let condition = self.temp_slot(base, 4)?;
        let field_slot = self.temp_slot(base, 5)?;
        self.emit_operand_to_rax(source)?;
        self.push(X64TextInstruction::StoreSlot(source_slot));
        self.push(X64TextInstruction::LoadI64(object_bytes as i64));
        self.uses_failure_exit = true;
        for item in object_allocate(self.allocator, size_slot, auxiliary, condition, self.failure_label) {
            self.push(item);
        }
        self.push(X64TextInstruction::StoreSlot(pointer_slot));
        for offset in &offsets {
            self.push(X64TextInstruction::LoadSlot(source_slot));
            self.push(X64TextInstruction::LoadFromRaxDisplacement(*offset as i32));
            self.push(X64TextInstruction::StoreSlot(field_slot));
            self.push(X64TextInstruction::LoadSlot(pointer_slot));
            self.push(X64TextInstruction::LoadSlotToRdx(field_slot));
            self.push(X64TextInstruction::StoreRdxToRaxDisplacement(*offset as i32));
        }
        for local in dest_local.into_iter().chain(output_local) {
            self.push(X64TextInstruction::LoadSlot(pointer_slot));
            self.push(X64TextInstruction::StoreSlot(local));
        }
        Ok(())
    }

    /// `SumNew`：分配 `{u32 tag, i64 payload}` 载体并写入 variant。
    fn emit_sum_new(&mut self, instruction: &ValkyrieMirInstruction, nominal: NominalInstanceId, variant: VariantId, payload: Option<&MirValueId>) -> Result<(), String> {
        let local = self.require_output_local(instruction)?;
        let (tag, has_payload) = self.sum_variant_tag(nominal, variant, "`SumNew`")?;
        if payload.is_some() != has_payload {
            return Err(format!(
                "x86_64 lowering：`SumNew` variant {variant:?} 的载荷事实与冻结表示不一致（payload={} expected={has_payload}）：function={}",
                payload.is_some(),
                self.view.symbol
            ));
        }
        let base = self.reserve_temps(4);
        let pointer_slot = self.temp_slot(base, 0)?;
        let size_slot = self.temp_slot(base, 1)?;
        let auxiliary = self.temp_slot(base, 2)?;
        let condition = self.temp_slot(base, 3)?;
        self.push(X64TextInstruction::LoadI64(SUM_CARRIER_BYTES));
        self.uses_failure_exit = true;
        for item in object_allocate(self.allocator, size_slot, auxiliary, condition, self.failure_label) {
            self.push(item);
        }
        self.push(X64TextInstruction::StoreSlot(pointer_slot));
        self.push(X64TextInstruction::LoadI64(tag as i64));
        self.push(X64TextInstruction::StoreSlot(condition));
        self.push(X64TextInstruction::LoadSlotToRcx(condition));
        self.push(X64TextInstruction::LoadSlot(pointer_slot));
        self.push(X64TextInstruction::StoreDwordRcxToRax);
        if let Some(payload) = payload {
            self.emit_operand_to_rdx(*payload)?;
            self.push(X64TextInstruction::LoadSlot(pointer_slot));
            self.push(X64TextInstruction::StoreRdxToRaxDisplacement(SUM_PAYLOAD_OFFSET));
        }
        self.push(X64TextInstruction::LoadSlot(pointer_slot));
        self.push(X64TextInstruction::StoreSlot(local));
        Ok(())
    }

    /// `SumVariantIs`：比较当前 tag 与冻结 discriminant。
    fn emit_sum_variant_is(
        &mut self,
        instruction: &ValkyrieMirInstruction,
        nominal: NominalInstanceId,
        variant: VariantId,
        object: MirValueId,
    ) -> Result<(), String> {
        let local = self.require_output_local(instruction)?;
        let (tag, _) = self.sum_variant_tag(nominal, variant, "`SumVariantIs`")?;
        let base = self.reserve_temps(2);
        let value_slot = self.temp_slot(base, 0)?;
        let expected_slot = self.temp_slot(base, 1)?;
        self.emit_operand_to_rax(object)?;
        self.push(X64TextInstruction::LoadDwordFromRax);
        self.push(X64TextInstruction::StoreSlot(value_slot));
        self.push(X64TextInstruction::LoadI64(tag as i64));
        self.push(X64TextInstruction::StoreSlot(expected_slot));
        self.push(X64TextInstruction::LoadSlot(value_slot));
        self.push(X64TextInstruction::LoadSlotToRcx(expected_slot));
        self.push(X64TextInstruction::Operator(X86_64OperatorCode::I64CompareEqual));
        self.push(X64TextInstruction::StoreSlot(local));
        Ok(())
    }

    /// `SumPayloadGet`：按冻结偏移读取载荷。
    fn emit_sum_payload_get(
        &mut self,
        instruction: &ValkyrieMirInstruction,
        nominal: NominalInstanceId,
        variant: VariantId,
        object: MirValueId,
    ) -> Result<(), String> {
        let local = self.require_output_local(instruction)?;
        let (_, has_payload) = self.sum_variant_tag(nominal, variant, "`SumPayloadGet`")?;
        if !has_payload {
            return Err(format!(
                "x86_64 lowering：`SumPayloadGet` 的 variant {variant:?} 没有载荷：function={}",
                self.view.symbol
            ));
        }
        self.emit_operand_to_rax(object)?;
        self.push(X64TextInstruction::LoadFromRaxDisplacement(SUM_PAYLOAD_OFFSET));
        self.push(X64TextInstruction::StoreSlot(local));
        Ok(())
    }

    /// 分配 `length` 个元素槽；`rax` 返回数组指针。
    ///
    /// 字节数换算、分配实现、失败出口与头部写入全部来自 `array_runtime`：
    /// guest lowering 不再自己算字节数、写数组头或直接调用 `HeapAlloc`。
    /// 元素个数先求值进 `rax`；容量乘加溢出与分配失败都跳到本函数的定义失败出口。
    fn emit_array_allocation(&mut self, length: MirValueId) -> Result<(), String> {
        let base = self.reserve_temps(4);
        let slots = ArrayAllocationSlots {
            count: self.temp_slot(base, 0)?,
            bytes: self.temp_slot(base, 1)?,
            auxiliary: self.temp_slot(base, 2)?,
            condition: self.temp_slot(base, 3)?,
        };
        self.emit_operand_to_rax(length)?;
        self.uses_failure_exit = true;
        for instruction in array_allocate(self.allocator, slots, self.failure_label) {
            self.push(instruction);
        }
        Ok(())
    }

    /// 字面量长度的数组分配：`count` 是编译期常量，直接落 `rax`。
    fn emit_array_allocation_constant(&mut self, count: i64) -> Result<(), String> {
        let base = self.reserve_temps(4);
        let slots = ArrayAllocationSlots {
            count: self.temp_slot(base, 0)?,
            bytes: self.temp_slot(base, 1)?,
            auxiliary: self.temp_slot(base, 2)?,
            condition: self.temp_slot(base, 3)?,
        };
        self.push(X64TextInstruction::LoadI64(count));
        self.uses_failure_exit = true;
        for instruction in array_allocate(self.allocator, slots, self.failure_label) {
            self.push(instruction);
        }
        Ok(())
    }

    /// `array.get` / `array.set` 的唯一定义边界检查：下标必须落在 `[0, [0])`。
    ///
    /// 这是两个访问入口（MIR `ArrayGet/Set` 与 provider `array.get/set`）共同经过的
    /// 检查；失败跳到本函数的数组失败出口，不把越界访问留成后续内存故障。
    fn emit_array_bounds_check(&mut self, array: MirValueId, index: MirValueId) -> Result<(), String> {
        let base = self.reserve_temps(3);
        let array_slot = self.temp_slot(base, 0)?;
        let index_slot = self.temp_slot(base, 1)?;
        let condition = self.temp_slot(base, 2)?;
        self.emit_operand_to_rax(array)?;
        self.push(X64TextInstruction::StoreSlot(array_slot));
        self.emit_operand_to_rax(index)?;
        self.push(X64TextInstruction::StoreSlot(index_slot));
        self.uses_failure_exit = true;
        for instruction in array_bounds_check(array_slot, index_slot, condition, self.failure_label) {
            self.push(instruction);
        }
        Ok(())
    }

    /// 数组 operator 调用的机械发射（`[host_provider]` 冻结的 `array.*`）。
    fn emit_array_operator_call(&mut self, instruction: &ValkyrieMirInstruction, plan: X64ArrayOperatorPlan, arguments: &[MirValueId]) -> Result<(), String> {
        if arguments.len() != plan.arity {
            return Err(format!(
                "x86_64 数组 operator 实参数量与冻结 arity 不符: function={} expected={} actual={}",
                self.view.symbol,
                plan.arity,
                arguments.len()
            ));
        }
        match plan.operation {
            X64ArrayOperation::New => {
                let local = self.require_output_local(instruction)?;
                self.emit_array_allocation(arguments[0])?;
                self.push(X64TextInstruction::StoreSlot(local));
            }
            X64ArrayOperation::Length => {
                let local = self.require_output_local(instruction)?;
                self.emit_operand_to_rax(arguments[0])?;
                self.push(X64TextInstruction::LoadFromRax);
                self.push(X64TextInstruction::StoreSlot(local));
            }
            X64ArrayOperation::Get => {
                let local = self.require_output_local(instruction)?;
                self.emit_array_bounds_check(arguments[0], arguments[1])?;
                self.emit_operand_to_rcx(arguments[1])?;
                self.emit_operand_to_rax(arguments[0])?;
                self.push(X64TextInstruction::LoadIndexedFromRaxRcx);
                self.push(X64TextInstruction::StoreSlot(local));
            }
            X64ArrayOperation::Set => {
                if self.output_local(instruction)?.is_some() {
                    return Err(format!("x86_64 lowering：数组 `set` 不得绑定结果槽：function={}", self.view.symbol));
                }
                self.emit_array_bounds_check(arguments[0], arguments[1])?;
                self.emit_operand_to_rcx(arguments[1])?;
                self.emit_operand_to_rdx(arguments[2])?;
                self.emit_operand_to_rax(arguments[0])?;
                self.push(X64TextInstruction::StoreIndexedRdxToRaxRcx);
            }
        }
        Ok(())
    }

    /// 发射控制台 operator：把 `Utf8Text` 对象的 `_repr` 数组 gather 成连续 UTF-8
    /// 字节后按目标平台的句柄通道写出。
    ///
    /// staging 副本复用数组运行时的分配/释放合同（同一分配器、同一定义失败出口），
    /// 写出后立即释放，有效期就是这一次调用。写出本身是尽力而为（unit 返回、无错误
    /// 通道），但分配失败仍走定义好的失败出口，不拿后续故障充当错误。
    fn emit_console_write(&mut self, instruction: &ValkyrieMirInstruction, operation: X64ConsoleOperation, arguments: &[MirValueId]) -> Result<(), String> {
        match operation {
            X64ConsoleOperation::Write => {}
        }
        if arguments.len() != 2 {
            return Err(format!(
                "x86_64 控制台 operator 实参数量与冻结 arity 不符: function={} expected=2 actual={}",
                self.view.symbol,
                arguments.len()
            ));
        }
        if self.output_local(instruction)?.is_some() {
            return Err(format!("x86_64 lowering：控制台 `write` 不得绑定结果槽：function={}", self.view.symbol));
        }
        let base = self.reserve_temps(13);
        let handle_slot = self.temp_slot(base, 0)?;
        let obj_slot = self.temp_slot(base, 1)?;
        let array_slot = self.temp_slot(base, 2)?;
        let count_slot = self.temp_slot(base, 3)?;
        let bytes_slot = self.temp_slot(base, 4)?;
        let aux_slot = self.temp_slot(base, 5)?;
        let cond_slot = self.temp_slot(base, 6)?;
        let buf_slot = self.temp_slot(base, 7)?;
        let index_slot = self.temp_slot(base, 8)?;
        let cursor_slot = self.temp_slot(base, 9)?;
        let byte_slot = self.temp_slot(base, 10)?;
        let written_slot = self.temp_slot(base, 11)?;
        let zero_slot = self.temp_slot(base, 12)?;
        let slots = ArrayAllocationSlots { count: count_slot, bytes: bytes_slot, auxiliary: aux_slot, condition: cond_slot };
        // 物化句柄与消息对象指针。
        self.emit_operand_to_rax(arguments[0])?;
        self.push(X64TextInstruction::StoreSlot(handle_slot));
        self.emit_operand_to_rax(arguments[1])?;
        self.push(X64TextInstruction::StoreSlot(obj_slot));
        // 按语言结算的文本布局事实取 `_repr` 字段；数组头部第 0 格是字节个数。
        self.push(X64TextInstruction::LoadSlot(obj_slot));
        self.push(X64TextInstruction::LoadFromRaxDisplacement(UTF8_TEXT_CARRIER_LAYOUT.repr_field_byte_offset as i32));
        self.push(X64TextInstruction::StoreSlot(array_slot));
        self.push(X64TextInstruction::LoadSlot(array_slot));
        self.push(X64TextInstruction::LoadFromRax);
        self.push(X64TextInstruction::StoreSlot(count_slot));
        // staging：按同一数组合同分配 `count` 个元素槽（每槽只用低字节，连续写出）。
        self.push(X64TextInstruction::LoadSlot(count_slot));
        self.uses_failure_exit = true;
        for item in array_allocate(self.allocator, slots, self.failure_label) {
            self.push(item);
        }
        self.push(X64TextInstruction::StoreSlot(buf_slot));
        // gather：`[array + 16 + 8*i]` 的低字节写入 `[buf + 16 + i]`。
        let loop_label = self.fresh_label();
        let end_label = self.fresh_label();
        self.push(X64TextInstruction::LoadI64(0));
        self.push(X64TextInstruction::StoreSlot(index_slot));
        self.push(X64TextInstruction::LoadSlot(buf_slot));
        self.push(X64TextInstruction::AddRaxImmediate(ARRAY_ELEMENT_OFFSET));
        self.push(X64TextInstruction::StoreSlot(cursor_slot));
        self.push(X64TextInstruction::Label(loop_label));
        self.push(X64TextInstruction::LoadSlot(index_slot));
        self.push(X64TextInstruction::LoadSlotToRcx(count_slot));
        self.push(X64TextInstruction::Operator(X86_64OperatorCode::I64CompareLessThan));
        self.push(X64TextInstruction::JumpIfZero(end_label));
        self.push(X64TextInstruction::LoadSlot(array_slot));
        self.push(X64TextInstruction::LoadSlotToRcx(index_slot));
        self.push(X64TextInstruction::LoadIndexedFromRaxRcx);
        self.push(X64TextInstruction::StoreSlot(byte_slot));
        self.push(X64TextInstruction::LoadSlot(cursor_slot));
        self.push(X64TextInstruction::LoadSlotToRdx(byte_slot));
        self.push(X64TextInstruction::StoreByteAtRaxFromRdx);
        self.push(X64TextInstruction::LoadSlot(cursor_slot));
        self.push(X64TextInstruction::AddRaxImmediate(1));
        self.push(X64TextInstruction::StoreSlot(cursor_slot));
        self.push(X64TextInstruction::LoadSlot(index_slot));
        self.push(X64TextInstruction::AddRaxImmediate(1));
        self.push(X64TextInstruction::StoreSlot(index_slot));
        self.push(X64TextInstruction::Jump(loop_label));
        self.push(X64TextInstruction::Label(end_label));
        match self.allocator {
            ArrayAllocator::WindowsProcessHeap => {
                // `SetConsoleOutputCP(CP_UTF8)`：控制台句柄按 UTF-8 呈现；重定向管道不受影响。
                self.push(X64TextInstruction::LoadI64(65001));
                self.push(X64TextInstruction::StoreSlot(aux_slot));
                self.push(X64TextInstruction::LoadSlotToArg { slot: aux_slot, index: 0 });
                self.push(X64TextInstruction::CallImport { module: "kernel32.dll".to_string(), field: "SetConsoleOutputCP".to_string() });
                // `GetStdHandle(nStdHandle)`：-11 标准输出 / -12 标准错误伪句柄。
                self.push(X64TextInstruction::LoadSlotToArg { slot: handle_slot, index: 0 });
                self.push(X64TextInstruction::CallImport { module: "kernel32.dll".to_string(), field: "GetStdHandle".to_string() });
                self.push(X64TextInstruction::StoreSlot(handle_slot));
                // `WriteFile(hFile, buf, count, &written, NULL)`：对管道与控制台句柄统一按 UTF-8 字节写出。
                self.push(X64TextInstruction::LoadSlotToArg { slot: handle_slot, index: 0 });
                self.push(X64TextInstruction::LoadSlot(buf_slot));
                self.push(X64TextInstruction::AddRaxImmediate(ARRAY_ELEMENT_OFFSET));
                self.push(X64TextInstruction::StoreSlot(aux_slot));
                self.push(X64TextInstruction::LoadSlotToArg { slot: aux_slot, index: 1 });
                self.push(X64TextInstruction::LoadSlotToArg { slot: count_slot, index: 2 });
                self.push(X64TextInstruction::LoadSlotAddress(written_slot));
                self.push(X64TextInstruction::StoreSlot(aux_slot));
                self.push(X64TextInstruction::LoadSlotToArg { slot: aux_slot, index: 3 });
                self.push(X64TextInstruction::ZeroStackArg(0));
                self.push(X64TextInstruction::CallImport { module: "kernel32.dll".to_string(), field: "WriteFile".to_string() });
            }
            ArrayAllocator::LinuxMmap => {
                // `write(fd, buf, count)`：句柄即文件描述符（1 标准输出 / 2 标准错误）。
                self.push(X64TextInstruction::LoadSlotToArg { slot: handle_slot, index: 0 });
                self.push(X64TextInstruction::LoadSlot(buf_slot));
                self.push(X64TextInstruction::AddRaxImmediate(ARRAY_ELEMENT_OFFSET));
                self.push(X64TextInstruction::StoreSlot(aux_slot));
                self.push(X64TextInstruction::LoadSlotToArg { slot: aux_slot, index: 1 });
                self.push(X64TextInstruction::LoadSlotToArg { slot: count_slot, index: 2 });
                self.push(X64TextInstruction::SyscallWrite);
            }
        }
        // staging 是一次性副本：写出后立即释放。
        self.push(X64TextInstruction::LoadI64(0));
        self.push(X64TextInstruction::StoreSlot(zero_slot));
        self.push(X64TextInstruction::LoadSlot(buf_slot));
        for item in array_free(self.allocator, slots, zero_slot) {
            self.push(item);
        }
        Ok(())
    }

    /// 操作数在计划中冻结的语义类型。
    ///
    /// `Cast` 的转换语义必须由此决定：`Integer32{signed}` 与 `Integer32{signed:false}`
    /// 占用同一 8 字节帧槽，只有冻结类型能区分符号扩展方向。
    fn operand_semantic_type(&self, value: MirValueId) -> Result<SemanticType, String> {
        self.view
            .value_types
            .get(&value)
            .cloned()
            .ok_or_else(|| format!("x86_64 `Cast` 源 SSA 值 {value:?} 缺少冻结语义类型：function={}", self.view.symbol))
    }

    /// 语言层 `as` 的表示转换（整数走 `rax`，浮点走 `xmm0`，结果回写 64 位帧槽）。
    ///
    /// 帧槽不保证处于规范形态：常量以完整 `i64` 落入，32 位运算零扩展结果。
    /// 因此这里**不假设**高位正确，而是显式「按源位宽/符号性规范化 → 按目标位宽/符号性收窄」。
    /// 浮点参与走 [`Self::emit_float_cast_conversion`] 的 SSE2 单指令转换。
    fn emit_cast_conversion(&mut self, source: &SemanticType, target: &SemanticType) -> Result<(), String> {
        if source == target {
            return Ok(());
        }
        if source.is_float() || target.is_float() {
            return self.emit_float_cast_conversion(source, target);
        }
        if matches!(target, SemanticType::Boolean) {
            self.push(X64TextInstruction::ToBool);
            return Ok(());
        }
        let (Some(source_shape), Some(target_shape)) = (x64_int_shape(source), x64_int_shape(target))
        else {
            return Err(format!(
                "x86_64 后端没有 `{source}` → `{target}` 的确定转换语义：布尔/文本与数值之间的 `as` 缺少目标合同，function={}",
                self.view.symbol
            ));
        };
        if source_shape == target_shape {
            return Ok(());
        }
        // 源规范化：窄整数按源符号性符号/零扩展到 64 位；64 位源已是完整值。
        if source_shape.bits < 64 {
            let step = narrow_normalize(source_shape.bits, source_shape.signed).ok_or_else(|| {
                format!("x86_64 后端没有位宽 {} 的整数载体：function={}", source_shape.bits, self.view.symbol)
            })?;
            self.push(step);
        }
        // 目标表示：窄目标按目标符号性截断并规范化；64 位目标保留当前 64 位值。
        if target_shape.bits < 64 {
            let step = narrow_normalize(target_shape.bits, target_shape.signed).ok_or_else(|| {
                format!("x86_64 后端没有位宽 {} 的整数载体：function={}", target_shape.bits, self.view.symbol)
            })?;
            self.push(step);
        }
        Ok(())
    }

    /// 浮点参与的 `as`：f32/f64 互转、整数 ↔ 浮点与显式截断语义。
    ///
    /// 整数侧先按自身符号性规范化到 64 位再转换；`u64` 与浮点互转缺少单指令合同
    /// （`cvtsi2sd` 只按有符号解释），结构化失败，不用错误符号性冒充。
    fn emit_float_cast_conversion(&mut self, source: &SemanticType, target: &SemanticType) -> Result<(), String> {
        let source_float = matches!(source, SemanticType::Float32 | SemanticType::Float64);
        let target_float = matches!(target, SemanticType::Float32 | SemanticType::Float64);
        let target_single = matches!(target, SemanticType::Float32);
        if source_float {
            // 源位模式已由 `emit_operand_to_rax` 搬进 `rax`；SSE 转换统一从 `xmm0` 取源。
            self.push(X64TextInstruction::LoadRaxToXmm0 { single: matches!(source, SemanticType::Float32) });
        }
        if source_float && target_float {
            return match (source, target) {
                (SemanticType::Float32, SemanticType::Float64) => {
                    self.push(X64TextInstruction::CastFloat(X64FloatCast::F32ToF64));
                    Ok(())
                }
                (SemanticType::Float64, SemanticType::Float32) => {
                    self.push(X64TextInstruction::CastFloat(X64FloatCast::F64ToF32));
                    Ok(())
                }
                _ => Ok(()),
            };
        }
        if source_float {
            let target_shape = x64_int_shape(target).ok_or_else(|| {
                format!(
                    "x86_64 后端没有 `{source}` → `{target}` 的确定转换语义：浮点只转换为整数族，function={}",
                    self.view.symbol
                )
            })?;
            if target_shape.bits == 64 && !target_shape.signed {
                return Err(format!(
                    "x86_64 后端不支持 `{source}` → `{target}`：`cvttsd2si` 只产生有符号 64 位值，`u64` 目标缺少饱和/检查合同，function={}",
                    self.view.symbol
                ));
            }
            // 越界与 NaN 走定义好的失败出口：`cvttsi2sd` 族的非值（INT64_MIN）不得
            // 冒充合法转换结果。边界都是 2 的幂，在 `f32`/`f64` 下精确可表示。
            let single = matches!(source, SemanticType::Float32);
            let base = self.reserve_temps(2);
            let src_slot = self.temp_slot(base, 0)?;
            let bound_slot = self.temp_slot(base, 1)?;
            // `rax` 仍持有源位模式（`LoadRaxToXmm0` 不破坏它），先落槽供逐次比较。
            self.push(X64TextInstruction::StoreSlot(src_slot));
            let failure = self.failure_label;
            self.uses_failure_exit = true;
            let (lower, upper) = float_int_target_bounds(&target_shape);
            let encode_bound = |value: f64| -> i64 {
                if single { u64::from((value as f32).to_bits()) as i64 } else { value.to_bits() as i64 }
            };
            let (ne_code, lt_code, ge_code) = if single {
                (X86_64OperatorCode::F32CompareNotEqual, X86_64OperatorCode::F32CompareLessThan, X86_64OperatorCode::F32CompareGreaterOrEqual)
            } else {
                (X86_64OperatorCode::F64CompareNotEqual, X86_64OperatorCode::F64CompareLessThan, X86_64OperatorCode::F64CompareGreaterOrEqual)
            };
            // NaN：`x != x` 为真。
            let not_nan_label = self.fresh_label();
            self.push(X64TextInstruction::LoadSlotToXmm0 { slot: src_slot, single });
            self.push(X64TextInstruction::LoadSlotToXmm1 { slot: src_slot, single });
            self.push(X64TextInstruction::Operator(ne_code));
            self.push(X64TextInstruction::JumpIfZero(not_nan_label));
            self.push(X64TextInstruction::Jump(failure));
            self.push(X64TextInstruction::Label(not_nan_label));
            // 下界：`x < lower` 越界。
            let above_lower_label = self.fresh_label();
            self.push(X64TextInstruction::LoadI64(encode_bound(lower)));
            self.push(X64TextInstruction::StoreSlot(bound_slot));
            self.push(X64TextInstruction::LoadSlotToXmm0 { slot: src_slot, single });
            self.push(X64TextInstruction::LoadSlotToXmm1 { slot: bound_slot, single });
            self.push(X64TextInstruction::Operator(lt_code));
            self.push(X64TextInstruction::JumpIfZero(above_lower_label));
            self.push(X64TextInstruction::Jump(failure));
            self.push(X64TextInstruction::Label(above_lower_label));
            // 上界：`x >= upper` 越界（上界为排他边界）。
            let below_upper_label = self.fresh_label();
            self.push(X64TextInstruction::LoadI64(encode_bound(upper)));
            self.push(X64TextInstruction::StoreSlot(bound_slot));
            self.push(X64TextInstruction::LoadSlotToXmm0 { slot: src_slot, single });
            self.push(X64TextInstruction::LoadSlotToXmm1 { slot: bound_slot, single });
            self.push(X64TextInstruction::Operator(ge_code));
            self.push(X64TextInstruction::JumpIfZero(below_upper_label));
            self.push(X64TextInstruction::Jump(failure));
            self.push(X64TextInstruction::Label(below_upper_label));
            // 源重新装入 `xmm0` 后按截断转换。
            self.push(X64TextInstruction::LoadSlotToXmm0 { slot: src_slot, single });
            self.push(X64TextInstruction::CastFloat(if single { X64FloatCast::F32ToI64 } else { X64FloatCast::F64ToI64 }));
            if target_shape.bits < 64 {
                let step = narrow_normalize(target_shape.bits, target_shape.signed).ok_or_else(|| {
                    format!("x86_64 后端没有位宽 {} 的整数载体：function={}", target_shape.bits, self.view.symbol)
                })?;
                self.push(step);
            }
            return Ok(());
        }
        let source_shape = x64_int_shape(source).ok_or_else(|| {
            format!(
                "x86_64 后端没有 `{source}` → `{target}` 的确定转换语义：浮点目标只接受整数族源，function={}",
                self.view.symbol
            )
        })?;
        if source_shape.bits == 64 && !source_shape.signed {
            return Err(format!(
                "x86_64 后端不支持 `{source}` → `{target}`：`cvtsi2sd` 只按有符号解释 `rax`，`u64` 源缺少无符号转换合同，function={}",
                self.view.symbol
            ));
        }
        if source_shape.bits < 64 {
            let step = narrow_normalize(source_shape.bits, source_shape.signed).ok_or_else(|| {
                format!("x86_64 后端没有位宽 {} 的整数载体：function={}", source_shape.bits, self.view.symbol)
            })?;
            self.push(step);
        }
        self.push(X64TextInstruction::CastFloat(if target_single { X64FloatCast::I64ToF32 } else { X64FloatCast::I64ToF64 }));
        Ok(())
    }

    fn output_local(&self, instruction: &ValkyrieMirInstruction) -> Result<Option<u16>, String> {
        match self.slots.instruction_output(instruction) {
            None => Ok(None),
            Some(value) => self.value_local(value).map(Some),
        }
    }

    fn require_output_local(&self, instruction: &ValkyrieMirInstruction) -> Result<u16, String> {
        self.output_local(instruction)?
            .ok_or_else(|| format!("x86_64 lowering：指令缺少 SSA 结果槽: function={}", self.view.symbol))
    }

    fn value_local(&self, value: MirValueId) -> Result<u16, String> {
        let local = *self
            .slots
            .value_locals
            .get(&value)
            .ok_or_else(|| format!("x86_64 lowering：SSA 值 {value:?} 缺少规划的局部槽: function={}", self.view.symbol))?;
        if let Some(ty) = self.slots.local_types.get(local as usize)
            && !self.is_local_carrier(ty)
        {
            return Err(format!(
                "x86_64 lowering：SSA 值 {value:?} 的类型 `{ty}` 没有 x86_64 槽载体: function={}",
                self.view.symbol
            ));
        }
        Ok(local)
    }

    /// 帧槽可承载的类型。
    ///
    /// 在标量/数组/文本之外，x86-64 引用对象、unite（sum）与引用可空都是 8 字节
    /// 指针载体；值存储聚合没有内联槽合同，仍在这里结构化失败。
    fn is_local_carrier(&self, ty: &SemanticType) -> bool {
        is_local_carrier_type(ty) || is_reference_carrier(ty, &self.ctx.linked().aggregate_layouts, self.ctx.sum_layout_by_semantic_type())
    }
}

/// 把一个可达函数实例降低为文本 LIR 程序函数。
pub(crate) fn lower_function(
    ctx: &LaneContext,
    instance: &ItemInstanceId,
    reachable: &[ItemInstanceId],
    names: &BTreeMap<ItemInstanceId, String>,
    platform: X64Platform,
    layouts: &ResolvedLayoutPlan,
) -> Result<X64ProgramFunction, String> {
    let function = ctx
        .function(instance)
        .ok_or_else(|| format!("x86_64 lowering：可达操作 `{instance:?}` 缺少函数体: function={instance:?}"))?;
    let view = ctx.view(instance).ok_or_else(|| "x86_64 lowering：可达操作缺少预解析视图".to_string())?;
    let slots = SlotPlan::plan(function, view)?;
    let local_count = u16::try_from(slots.local_types.len()).map_err(|_| "x86_64 lowering：局部槽数量超出 `u16`".to_string())?;
    // 数组失败出口使用块标签之后的保留标签，避免与 MIR 块编号冲突。
    let failure_label = function
        .blocks
        .values()
        .map(|block| block.id.0)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| "x86_64 lowering：块标签编号已耗尽，无法分配数组失败出口".to_string())?;
    let mut lowerer = FunctionLowerer {
        ctx,
        function,
        view,
        slots,
        reachable,
        names,
        abi: platform.abi(),
        allocator: ArrayAllocator::for_platform(platform),
        layouts,
        local_count,
        temps_used: 0,
        instructions: Vec::new(),
        failure_label,
        next_label: failure_label + 1,
        uses_failure_exit: false,
    };
    lowerer.store_entry_parameters()?;

    let mut order = vec![function.entry];
    for block in collect_reachable_blocks(function) {
        if block != function.entry {
            order.push(block);
        }
    }
    for block_id in order {
        let Some(block) = function.blocks.get(&block_id) else {
            return Err(format!("x86_64 lowering：函数 `{}` 引用了不存在的块 {block_id:?}", view.symbol));
        };
        lowerer.lower_block(block)?;
    }

    if lowerer.uses_failure_exit {
        // 定义好的数组失败出口：显式陷入，不伪造返回、不把空指针留到后续故障。
        for instruction in array_failure_exit(lowerer.failure_label) {
            lowerer.push(instruction);
        }
    }

    let temps = u16::try_from(lowerer.temps_used).map_err(|_| "x86_64 lowering：临时槽数量超出 `u16`".to_string())?;
    let slot_count = local_count.checked_add(temps).ok_or_else(|| "x86_64 lowering：帧槽数量溢出".to_string())?;
    Ok(X64ProgramFunction {
        name: names.get(instance).expect("可达操作有符号名").clone(),
        slot_count,
        instructions: lowerer.instructions,
    })
}

/// 入口块先访问，其余块按 RPO 遍历；与旧公共 IR 路径相同的图构造与遍历顺序。
fn collect_reachable_blocks(function: &ValkyrieMirFunction) -> Vec<ValkyrieMirBlockId> {
    let mut adjacency = Vec::new();
    for block in function.blocks.values() {
        let from = block.id;
        let targets: Vec<ValkyrieMirBlockId> = match &block.terminator {
            ValkyrieMirTerminator::Return { .. } | ValkyrieMirTerminator::Unreachable => Vec::new(),
            ValkyrieMirTerminator::Jump { target, .. } => vec![*target],
            ValkyrieMirTerminator::Branch { then_target, else_target, .. } => vec![*then_target, *else_target],
            ValkyrieMirTerminator::PerformEffect { effect: ValkyrieMirEffectKind::Raise, resume_target, .. } => vec![*resume_target],
            _ => Vec::new(),
        };
        adjacency.extend(targets.into_iter().map(|target| (from, target)));
    }
    let mut graph = boost_collection::MemoryGraph::new(true);
    graph.add_vertex(function.entry).expect("fresh graph");
    for id in function.blocks.keys() {
        graph.add_vertex(*id).ok();
    }
    for (_, target) in &adjacency {
        graph.add_vertex(*target).ok();
    }
    for (from, target) in adjacency {
        graph.add_edge(from, target, ()).expect("both endpoints were added as vertices");
    }
    boost_math::graph_theory::reverse_postorder(&graph, &function.entry)
        .expect("the entry is a vertex of the graph")
        .into_iter()
        .collect()
}
