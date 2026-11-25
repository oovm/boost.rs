//! Resolved Semantic MIR calls -> Wasm call forms.
//!
//! Owns direct / import / intrinsic / witness / indirect call emission and the
//! call-shaped helpers currently on the same dispatch path. This is Wasm
//! **call encoding**, not a second MIR. Semantic MIR remains call-resolution
//! authority; this module only maps already-resolved calls to Wasm opcodes.

use super::*;
use crate::{
    lowering::backends::wasm::cabi::CABI_HEAP_GLOBAL_INDEX,
    target_operator::{TargetOperandStep, TargetOperandType, TargetOperatorRecipe},
};

/// 计划冻结的目标标量载体 → wasm 栈类型字节。
fn wasm_stack_type(operand_type: TargetOperandType) -> u8 {
    match operand_type {
        TargetOperandType::I32 => VALTYPE_I32,
        TargetOperandType::I64 => VALTYPE_I64,
        TargetOperandType::F32 => VALTYPE_F32,
        TargetOperandType::F64 => VALTYPE_F64,
    }
}

impl<'a> WasmMirLowerer<'a> {
    /// Lowers a non-builtin MIR call to WASM bytecode.
    ///
    /// - Static dispatch emits `call` (0x10) with the resolved function index.
    /// - Witness dispatch emits `call_indirect` (0x11) using the witness operand as the table index.
    /// - `receiver_kind: Some(ByAddress)` means the first argument is a value-type receiver
    ///   whose linear-memory address is already an `i32` local in `value_locals`.
    ///   Because `emit_operand` emits `local.get` for value locals, the receiver's
    ///   linear-memory address is passed directly as the first WASM parameter -?no
    ///   special codegen is needed beyond emitting the argument normally.
    pub(super) fn emit_call_lowering(
        &mut self,
        callee: &WasmOperand,
        arguments: &[WasmOperand],
        dispatch: WasmDispatchKind,
        witness: Option<&WasmOperand>,
        receiver_kind: Option<WasmReceiverPassingKind>,
        output: Option<WasmValueKey>,
    ) -> crate::Result<()> {
        if let Some(WasmReceiverPassingKind::ByAddress) = receiver_kind {
            // Confirm the receiver exists as the first argument; it is emitted below.
        }
        let mut callee_return;
        match dispatch {
            WasmDispatchKind::Static => {
                // `[wasm_operator]` 绑定的 provider 直接在调用点编码为目标指令；
                // 不是 host import，也没有函数体，必须先于 import 解析处理。
                if self.try_emit_wasm_operator_call(callee, arguments, output)? {
                    return Ok(());
                }
                if let Some(import_index) = self.resolve_callee_import_index(callee, arguments) {
                    if self.try_emit_wasi_cli_write_via_stream(arguments, import_index, output)? {
                        return Ok(());
                    }
                    callee_return = self.resolve_callee_return_type(callee, Some(import_index))?;
                    let param_types = self.resolve_callee_param_types(callee, Some(import_index))?;
                    self.emit_abi_call_arguments(arguments, &param_types, None)?;
                    self.emit_call(import_index);
                }
                else if let Some(function_index) = self.resolve_callee_function_index(callee) {
                    // 计划事实缺失属可执行合同不一致，禁止 panic（与 `8050500c` 同款结构化失败）。
                    let return_type = *self
                        .return_types_by_function_index
                        .get(&function_index)
                        .ok_or_else(|| format!("WASM 函数下标缺少返回签名: function_index={function_index} in `{}`", self.mir_fn.symbol))?;
                    let param_types = self
                        .param_types_by_function_index
                        .get(&function_index)
                        .ok_or_else(|| format!("WASM 函数下标缺少参数签名: function_index={function_index} in `{}`", self.mir_fn.symbol))?
                        .clone();
                    callee_return = return_type;
                    let param_layouts = self.callee_param_layouts(callee);
                    self.emit_abi_call_arguments(arguments, &param_types, param_layouts.as_deref())?;
                    self.emit_call(function_index);
                }
                else {
                    // 计划事实缺失属可执行合同不一致，禁止 panic（与 `8050500c` 同款结构化失败）。
                    return Err(format!("WASM 静态调用缺少已解析 callable identity: {} callee={callee:?}", self.mir_fn.symbol));
                }
            }
            WasmDispatchKind::Witness => {
                callee_return = self.resolve_callee_return_type(callee, None)?;
                for argument in arguments {
                    let expected = self.operand_wasm_stack_type(argument)?;
                    self.emit_operand_coerced(argument, expected)?;
                }
                // 计划事实缺失属可执行合同不一致，禁止 panic（与 `8050500c` 同款结构化失败）。
                let Some(witness_operand) = witness
                else {
                    return Err(format!("WASM witness 调用缺少显式分派操作数: {}", self.mir_fn.symbol));
                };
                self.emit_operand(witness_operand)?;
                let type_index = self.resolve_callee_type_index(callee);
                self.emit_call_indirect(type_index, 0);
            }
            WasmDispatchKind::Indirect => {
                // callee 是函数值 SSA（闭包对象 anyref，合同 §5.3）。签名事实来自该
                // SSA 值冻结的 `WasmTypeFact::Function`：可见形参 coerce 与返回载体由它
                // 决定，`call_indirect` 的 functype 固定为「可见形参 + 一个尾部 anyref」。
                // 环境作为普通尾实参压栈，不占 callee 槽；缺事实即结构化失败。
                let (param_types, return_type, type_index) = self.resolve_indirect_callee_signature(callee)?;
                self.emit_abi_call_arguments(arguments, &param_types, None)?;
                self.emit_closure_callee(callee)?;
                self.emit_call_indirect(type_index, 0);
                callee_return = return_type;
            }
        }
        self.emit_store_call_output(output, callee_return)?;
        Ok(())
    }

    fn emit_store_call_output(&mut self, output: Option<WasmValueKey>, callee_return: Option<u8>) -> crate::Result<()> {
        match output {
            Some(output) => {
                // 计划事实缺失属可执行合同不一致，禁止 panic（与 `8050500c` 同款结构化失败）。
                let Some(return_type) = callee_return
                else {
                    return Err(format!("WASM 有 SSA 结果的调用缺少返回合同: {}", self.mir_fn.symbol));
                };
                // 调用返回值边界：callee 的 ABI 返回 i32 宿主句柄，而结果 SSA 槽按
                // 已登记 gc struct layout 规划为 anyref 时，与实参边界消费同一套
                // 装配期冻结的句柄↔managed 配方。对所有调用形态（导入/静态/间接）
                // 一致生效，禁止把计划槽降级或放宽槽位自检。
                let return_type = if return_type == VALTYPE_I32 {
                    match self.host_import_return_materialize_layout(output) {
                        Some(layout_id) => {
                            self.emit_utf8_host_handle_materialize(layout_id)?;
                            WASM_GC_ANYREF
                        }
                        None => return_type,
                    }
                }
                else {
                    return_type
                };
                self.validate_output_slot_type(output, return_type)?;
                self.assign_output_local(output);
            }
            None if callee_return.is_some() => self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::Drop)),
            None => {}
        }
        Ok(())
    }

    /// 唯一 ABI 实参边界物化点（导入/静态/间接调用共用）：逐槽比较实际载体与
    /// 已解析签名声明，只在两者分叉时按冻结配方转换。managed `Utf8`（gc struct
    /// anyref）进入声明 i32 的宿主/adaptor 槽时消费 managed→host 句柄配方；其余
    /// 组合保持既有 coerce 语义（宽度、装箱由目标计划承诺），未知分叉结构化失败。
    fn emit_abi_call_arguments(
        &mut self,
        arguments: &[WasmOperand],
        param_types: &[u8],
        param_layouts: Option<&[Option<WasmLayoutKey>]>,
    ) -> crate::Result<()> {
        if arguments.len() != param_types.len() {
            return Err(format!(
                "WASM 调用实参与已解析签名不一致: 实参={} 形参={} in `{}`",
                arguments.len(),
                param_types.len(),
                self.mir_fn.symbol
            ));
        }
        for (index, (argument, expected)) in arguments.iter().zip(param_types).enumerate() {
            let layout = param_layouts.and_then(|layouts| layouts.get(index).copied().flatten());
            self.emit_operand_into_text_slot(argument, *expected, layout)?;
        }
        Ok(())
    }

    /// 语言函数被调方的形参 layout（entry 块参数上的冻结事实）。
    /// 仅静态调用可解析；导入/间接调用没有本地函数体，返回 `None`。
    fn callee_param_layouts(&self, callee: &WasmOperand) -> Option<Vec<Option<WasmLayoutKey>>> {
        let WasmOperand::Item(instance) = callee
        else {
            return None;
        };
        let function = self.input.function(instance)?;
        let entry = function.blocks.iter().find(|block| block.id == function.entry)?;
        Some(entry.parameters.iter().map(|parameter| function.value_layouts.get(parameter).copied()).collect())
    }

    /// 按冻结配方把 managed 字符串 gc struct 转为宿主 i32 句柄：
    /// `struct.get` 取字节数组 → bump 分配线性内存 → 逐字节 `array.get` + `i32.store8`
    /// → `call env::host_intern_utf8(ptr, len)`。栈效果：`[anyref] → [i32]`。
    pub(super) fn emit_managed_utf8_to_host_handle(&mut self, operand: &WasmOperand) -> crate::Result<()> {
        let WasmOperand::Value(value) = operand
        else {
            return Err(format!("WASM 宿主字符串边界操作数缺少 SSA 身份: {}", self.mir_fn.symbol));
        };
        let Some(semantic_type) = self.mir_fn.value_types.get(value)
        else {
            return Err(format!("WASM ABI 边界 %{} 缺少语义类型身份: {}", value.0, self.mir_fn.symbol));
        };
        if !is_js_glue_host_string_type(semantic_type) {
            return Err(format!(
                "WASM ABI 载体失配 in `{}`: slot=i32 actual=anyref operand=%{} ({semantic_type:?}) 不属宿主字符串族，禁止按字符串配方转换",
                self.mir_fn.symbol, value.0
            ));
        }
        let Some(layout_id) = self.mir_fn.value_layouts.get(value).copied()
        else {
            return Err(format!("WASM 宿主字符串边界 %{} 缺少 layout 身份: {}", value.0, self.mir_fn.symbol));
        };
        let Some(recipe) = self.managed_utf8_to_host_handle.get(&layout_id)
        else {
            return Err(format!(
                "WASM 宿主字符串边界 %{} (layout={layout_id:?}) 缺少目标计划冻结的 managed→host 句柄转换配方: {}",
                value.0, self.mir_fn.symbol
            ));
        };
        // managed struct → 字节数组引用
        self.emit_operand(operand)?;
        self.emit_ref_cast_struct(recipe.struct_type_index);
        self.emit_struct_get(recipe.struct_type_index, recipe.bytes_field_slot);
        self.emit_ref_cast_array_type(recipe.bytes_array_type_index);
        let array_local = self.alloc_anyref_local();
        self.emit_local_set(array_local);
        // 字节长度
        self.emit_local_get(array_local);
        self.emit_ref_cast_array_type(recipe.bytes_array_type_index);
        self.emit_array_len();
        let len_local = self.alloc_i32_local();
        self.emit_local_set(len_local);
        // 动态 bump 分配（global 0 堆指针），增长逻辑与 `bump_allocate` 同构；
        // scratch local 在本次转换内闭合，不与其它发射交叠。
        self.code.push(WasmBodyInstruction::U32 { opcode: WasmOpcode::GlobalGet, operand: CABI_HEAP_GLOBAL_INDEX });
        self.emit_local_set(self.stack_ptr_local);
        self.emit_local_get(self.stack_ptr_local);
        self.emit_local_get(len_local);
        self.emit_i32_add();
        self.emit_local_set(self.bump_end_local);
        self.emit_local_get(self.bump_end_local);
        self.emit_i32_const(65535);
        self.emit_i32_add();
        self.emit_i32_const(16);
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::I32ShrU));
        self.code.push(WasmBodyInstruction::MemorySize);
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::I32Sub));
        self.emit_local_tee(self.bump_pages_local);
        self.emit_i32_const(0);
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::I32GtS));
        self.emit_block_type(WasmOpcode::If);
        self.emit_local_get(self.bump_pages_local);
        self.code.push(WasmBodyInstruction::MemoryGrow);
        self.emit_i32_const(-1);
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::I32Eq));
        self.emit_block_type(WasmOpcode::If);
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::Unreachable));
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::End));
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::End));
        // commit heap cursor：global.set 必须先收到 bump_end 新值（与 `bump_allocate` 一致），
        // 否则 V8 编译期即拒绝（not enough arguments on the stack for global.set）。
        self.emit_local_get(self.bump_end_local);
        self.code.push(WasmBodyInstruction::U32 { opcode: WasmOpcode::GlobalSet, operand: CABI_HEAP_GLOBAL_INDEX });
        // 逐字节拷贝：array.get(u8→i32) → i32.store8
        let index_local = self.alloc_i32_local();
        self.emit_i32_const(0);
        self.emit_local_set(index_local);
        self.emit_block_type(WasmOpcode::Block);
        self.emit_block_type(WasmOpcode::Loop);
        self.emit_local_get(index_local);
        self.emit_local_get(len_local);
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::I32GeU));
        self.code.push(WasmBodyInstruction::U32 { opcode: WasmOpcode::BrIf, operand: 1 });
        self.emit_local_get(self.stack_ptr_local);
        self.emit_local_get(index_local);
        self.emit_i32_add();
        self.emit_local_get(array_local);
        self.emit_ref_cast_array_type(recipe.bytes_array_type_index);
        self.emit_local_get(index_local);
        self.emit_array_get(recipe.bytes_array_type_index);
        self.code.push(WasmBodyInstruction::MemArg { opcode: WasmOpcode::I32Store8, align: 0, offset: 0 });
        self.emit_local_get(index_local);
        self.emit_i32_const(1);
        self.emit_i32_add();
        self.emit_local_set(index_local);
        self.code.push(WasmBodyInstruction::U32 { opcode: WasmOpcode::Br, operand: 0 });
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::End));
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::End));
        // 宿主登记：(ptr, len) → i32 handle
        self.emit_local_get(self.stack_ptr_local);
        self.emit_local_get(len_local);
        self.emit_call(recipe.intern_import_index);
        Ok(())
    }

    /// 判定宿主导入返回值是否命中「i32 宿主句柄 → managed 字符串」边界：结果值类型是
    /// 宿主字符串，且目标计划已按冻结 layout 把该 SSA 槽规划为 anyref。纯类型/布局
    /// 事实判定，不按符号名特判；未命中即继续走原 i32 句柄 ABI。
    fn host_import_return_materialize_layout(&self, output: WasmValueKey) -> Option<WasmLayoutKey> {
        if !self.mir_fn.value_types.get(&output).is_some_and(is_js_glue_host_string_type) {
            return None;
        }
        if self.wasm_local_value_type(self.planned_value_local(output)) != WASM_GC_ANYREF {
            return None;
        }
        self.mir_fn.value_layouts.get(&output).copied()
    }

    /// 结果槽载体物化的唯一入口：编码阶段只验证预规划槽，不改变 SSA 表示。
    ///
    /// 唯一例外是宿主字符串边界：线性内存/`[i32]` 数组/宿主句柄产生的 i32 句柄
    /// 要落进已登记 managed `Utf8` layout 的 anyref 槽时，按装配期冻结的
    /// 宿主句柄→managed 配方物化（与实参方向的 managed→host 配方互为逆）。
    /// 其余槽位分歧是可执行合同分叉：返回结构化错误并点名函数、槽号、期望载体
    /// 与实际载体，禁止 panic。
    pub(super) fn validate_output_slot_type(&mut self, output: WasmValueKey, stack_ty: u8) -> crate::Result<()> {
        let local = self.planned_value_local(output);
        let actual_ty = self.wasm_local_value_type(local);
        if actual_ty == stack_ty {
            return Ok(());
        }
        if actual_ty == WASM_GC_ANYREF && stack_ty == VALTYPE_I32 {
            if let Some(layout_id) = self.utf8_host_handle_boundary_layout(output) {
                self.emit_utf8_host_handle_materialize(layout_id)?;
                return Ok(());
            }
        }
        Err(format!(
            "WASM 编码结果与预规划槽冲突 in `{}`: operand=%{} local={} expected={} actual={}",
            self.mir_fn.symbol, output.0, local, stack_ty, actual_ty
        ))
    }

    pub(super) fn assign_output_local(&mut self, output: WasmValueKey) {
        let local = self.planned_value_local(output);
        self.emit_local_set(local);
    }

    pub(super) fn resolve_callee_param_types(&self, callee: &WasmOperand, import_index: Option<u32>) -> crate::Result<Vec<u8>> {
        // 计划事实缺失属可执行合同不一致，禁止 panic（与 `8050500c` 同款结构化失败）。
        if let Some(index) = import_index {
            return self
                .import_param_types
                .get(index as usize)
                .ok_or_else(|| format!("WASM 导入缺少参数签名: import_index={index} in `{}`", self.mir_fn.symbol))
                .cloned();
        }
        let WasmOperand::Item(instance) = callee
        else {
            return Err(format!("WASM 普通调用缺少实例身份: {} callee={callee:?}", self.mir_fn.symbol));
        };
        self.param_types_by_instance
            .get(instance)
            .ok_or_else(|| format!("WASM 实例缺少参数签名: callee={callee:?} in `{}`", self.mir_fn.symbol))
            .cloned()
    }

    pub(super) fn resolve_callee_return_type(&self, callee: &WasmOperand, import_index: Option<u32>) -> crate::Result<Option<u8>> {
        // 计划事实缺失属可执行合同不一致，禁止 panic（与 `8050500c` 同款结构化失败）。
        if let Some(index) = import_index {
            return self
                .import_return_types
                .get(index as usize)
                .ok_or_else(|| format!("WASM 导入缺少返回签名: import_index={index} in `{}`", self.mir_fn.symbol))
                .copied();
        }
        let WasmOperand::Item(instance) = callee
        else {
            return Err(format!("WASM 普通调用缺少实例身份: {} callee={callee:?}", self.mir_fn.symbol));
        };
        self.return_types_by_instance
            .get(instance)
            .ok_or_else(|| format!("WASM 实例缺少返回签名: callee={callee:?} in `{}`", self.mir_fn.symbol))
            .copied()
    }

    /// 间接调用的签名解析：callee SSA 值必须携带冻结的 `WasmTypeFact::Function`。
    /// `call_indirect` 的 functype 固定为「可见形参 + 一个尾部 anyref」（环境槽），
    /// 必须命中模块已注册的 functype（按签名字节反查 typeidx）。缺类型或缺注册
    /// 条目都结构化失败，禁止按参数个数猜签名，也禁止退回无 env 的旧签名。
    pub(super) fn resolve_indirect_callee_signature(&self, callee: &WasmOperand) -> crate::Result<(Vec<u8>, Option<u8>, u32)> {
        let WasmOperand::Value(value) = callee
        else {
            return Err(format!("WASM 间接调用缺少函数值操作数: {}", self.mir_fn.symbol));
        };
        let Some(WasmTypeFact::Function(signature)) = self.mir_fn.value_types.get(value)
        else {
            return Err(format!("WASM 间接调用 callee %{} 缺少冻结 Function 签名: {}", value.0, self.mir_fn.symbol));
        };
        let visible_param_types: Vec<u8> = signature
            .params
            .iter()
            .map(|ty| wasm_function_abi_value_type_for(self.input, ty, self.gc_struct_type_indices, self.js_glue_utf8_as_anyref))
            .collect::<crate::Result<Vec<u8>>>()?;
        let return_type = match &signature.return_type {
            WasmTypeFact::Unit | WasmTypeFact::Bottom => None,
            other => Some(wasm_function_abi_value_type_for(self.input, other, self.gc_struct_type_indices, self.js_glue_utf8_as_anyref)?),
        };
        let mut indirect_param_types = visible_param_types.clone();
        indirect_param_types.push(WASM_GC_ANYREF);
        let type_index = *self.type_index_by_signature.get(&(indirect_param_types.clone(), return_type)).ok_or_else(|| {
            format!(
                "WASM 间接调用签名缺少含环境槽的已注册 functype（visible={visible_param_types:?} env={WASM_GC_ANYREF:#04X} return={return_type:?}）: {}",
                self.mir_fn.symbol
            )
        })?;
        Ok((visible_param_types, return_type, type_index))
    }

    /// 间接调用的 callee 操作数发射：闭包对象 → 环境尾实参 → code 槽位。
    /// `call_indirect` 的栈序是 `实参..., env, code`（callee 下标最后压入）；
    /// env 是普通实参，不占 callee 槽。闭包对象槽是 anyref，读取字段前必须
    /// `ref.cast` 到冻结的闭包 structtype。
    fn emit_closure_callee(&mut self, callee: &WasmOperand) -> crate::Result<()> {
        self.emit_operand_coerced(callee, WASM_GC_ANYREF)?;
        self.emit_ref_cast_struct(self.gc_closure_type_index);
        self.emit_struct_get(self.gc_closure_type_index, WASM_CLOSURE_ENV_FIELD);
        self.emit_operand_coerced(callee, WASM_GC_ANYREF)?;
        self.emit_ref_cast_struct(self.gc_closure_type_index);
        self.emit_struct_get(self.gc_closure_type_index, WASM_CLOSURE_CODE_FIELD);
        Ok(())
    }

    pub(super) fn operand_wasm_stack_type(&self, operand: &WasmOperand) -> crate::Result<u8> {
        Ok(match operand {
            WasmOperand::Value(value) => self.wasm_local_value_type(self.planned_value_local(*value)),
            WasmOperand::Constant(constant) => match constant {
                WasmConstant::Utf8(_) => VALTYPE_I32,
                WasmConstant::Utf16(_) => return Err(crate::lowering::features::physical_contract::UTF16_UNRESOLVED_TEXT_ABI.to_string()),
                WasmConstant::Unit => WASM_GC_ANYREF,
                // 可空空值是空引用，与 nullable 槽的 anyref 载体一致。
                WasmConstant::Null => WASM_GC_ANYREF,
                WasmConstant::Float64(_) => VALTYPE_F64,
                WasmConstant::Int(_) | WasmConstant::Bool(_) => VALTYPE_I32,
            },
            WasmOperand::Symbol(_) => panic!("WASM 值操作数缺少 SSA 身份"),
            WasmOperand::Item(_) => {
                panic!("WASM callable identity cannot be used as a value operand")
            }
        })
    }

    /// 操作数在计划中冻结的语义类型；显式 `as` 的转换语义必须由此决定，
    /// 不得退回到只携带位宽的栈类型字节。
    pub(super) fn operand_semantic_type(&self, operand: &WasmOperand) -> Option<&WasmTypeFact> {
        match operand {
            WasmOperand::Value(value) => self.mir_fn.value_types.get(value),
            _ => None,
        }
    }

    /// 按目标合同发射操作数。
    ///
    /// 常量不携带位宽（`WasmConstant::Int` 只有 `i64` 载荷），因此常量一律交给
    /// 目标槽 valtype 决定 `i32.const` / `i64.const` / `f64.const`。任何宽度转换
    /// 只对已规划 SSA 槽生效，避免把 i64 字面量先截断成 i32 再符号扩展。
    pub(super) fn emit_operand_coerced(&mut self, operand: &WasmOperand, expected: u8) -> crate::Result<()> {
        if let WasmOperand::Constant(constant) = operand {
            return self.emit_load_constant_for_slot(constant, expected);
        }
        let actual = self.operand_wasm_stack_type(operand)?;
        if actual == expected {
            return self.emit_operand(operand);
        }
        // Reference-local metadata is authoritative for GC aggregates：槽位已是引用，
        // 即便表面分类成 i32 也不得用 ref.null 顶替。
        if expected == WASM_GC_ANYREF && self.operand_reference_local(operand).is_some() {
            return self.emit_operand(operand);
        }
        // 边界物化的唯一规则：managed 宿主字符串（`Utf8` gc struct，栈 anyref）进入
        // 声明 i32 的槽（宿主/adaptor 形参、字段、数组元素、wasm_operator 配方）时，
        // 一律消费装配期冻结的 managed→host 句柄配方。所有调用/存储形态共享此处，
        // 禁止在导入名、字段或数组侧另立特判。
        if expected == VALTYPE_I32 && actual == WASM_GC_ANYREF {
            if self.operand_semantic_type(operand).is_some_and(is_js_glue_host_string_type) {
                return self.emit_managed_utf8_to_host_handle(operand);
            }
        }
        // 仅允许 TargetExecutionPlan 已承诺的显式宽度转换；禁止用 0 / null 掩盖 carrier 失配。
        if expected == VALTYPE_I64 && (actual == WASM_GC_ANYREF || actual == WASM_GC_EXTERNREF) {
            self.emit_operand(operand)?;
            self.emit_unbox_i64_payload();
            return Ok(());
        }
        if expected == VALTYPE_F64 && (actual == WASM_GC_ANYREF || actual == WASM_GC_EXTERNREF) {
            self.emit_operand(operand)?;
            self.emit_unbox_f64_payload();
            return Ok(());
        }
        // 宽度不同的整数载体只有语义类型能决定扩展/截断方向：i32→i64 需要
        // 有符号/无符号选择，i64→i32 需要显式收窄。这里没有语义类型，任何
        // `extend` / `wrap` 都是靠载体猜，一律结构化失败。
        if (expected == VALTYPE_I64 && actual == VALTYPE_I32) || (expected == VALTYPE_I32 && actual == VALTYPE_I64) {
            return Err(format!(
                "WASM ABI 宽度失配 in `{}`: slot={} actual={} operand={} origin={}; \
                 integer widen/narrow requires an explicit `Cast` driven by both semantic types — refusing to guess sign or truncate",
                self.mir_fn.symbol,
                self.abi_valtype_label(expected),
                self.abi_valtype_label(actual),
                self.describe_operand(operand),
                self.instruction_origin()
            ));
        }
        if expected == WASM_GC_ANYREF && actual == VALTYPE_I64 {
            return self.emit_box_i64_payload(operand);
        }
        if expected == WASM_GC_ANYREF && actual == VALTYPE_F64 {
            return self.emit_box_f64_payload(operand);
        }
        if expected == WASM_GC_EXTERNREF && actual == WASM_GC_ANYREF {
            return self.emit_operand(operand);
        }
        Err(format!(
            "WASM ABI carrier mismatch in `{}`: slot={} actual={} operand={} origin={}; \
             TargetExecutionPlan must provide an explicit conversion — refusing null/0 placeholders",
            self.mir_fn.symbol,
            self.abi_valtype_label(expected),
            self.abi_valtype_label(actual),
            self.describe_operand(operand),
            self.instruction_origin()
        ))
    }

    /// `[wasm_operator("...")]` 绑定：把契约调用直接编码为目标数值指令。
    ///
    /// 只有 callee 携带已绑定 host contract 的实例身份时才命中；未绑定 provider
    /// 的 host contract 与普通函数调用都返回 `false`，走既有 import / local call
    /// 路径。arity、操作数配方、结果载体与 opcode 全部来自计划冻结的
    /// `TargetOperatorPlan`；未冻结的绑定必须返回结构化错误，禁止从助记符猜语义。
    pub(super) fn try_emit_wasm_operator_call(
        &mut self,
        callee: &WasmOperand,
        arguments: &[WasmOperand],
        output: Option<WasmValueKey>,
    ) -> crate::Result<bool> {
        let WasmOperand::Item(instance) = callee
        else {
            return Ok(false);
        };
        let Some((mnemonic, binding)) = self.callee_wasm_operator.get(instance)
        else {
            return Ok(false);
        };
        let WasmOperatorBinding::Numeric(plan) = binding
        else {
            if let WasmOperatorBinding::GcArray(array_plan) = binding {
                return self.try_emit_wasm_gc_array_operator(mnemonic, *array_plan, arguments, output, instance).map(|()| true);
            }
            if let WasmOperatorBinding::FloatRemainder(carrier) = binding {
                return self.emit_wasm_float_remainder(mnemonic, *carrier, arguments, output).map(|()| true);
            }
            return Err(format!("WASM `wasm_operator` 绑定缺少可编码助记符 `{mnemonic}`: callee={instance:?} function={}", self.mir_fn.symbol));
        };
        let opcode = plan.encoding;
        if arguments.len() != plan.arity {
            return Err(format!(
                "WASM `wasm_operator` 绑定 `{mnemonic}` 实参数量与冻结 arity 不符: callee={instance:?} function={} expected={} actual={}",
                self.mir_fn.symbol,
                plan.arity,
                arguments.len()
            ));
        }
        let operand_type = wasm_stack_type(plan.operand_type);
        match plan.recipe {
            TargetOperatorRecipe::Operands => self.emit_wasm_operator_operands(mnemonic, operand_type, arguments)?,
            TargetOperatorRecipe::Steps(steps) => self.emit_wasm_operator_recipe(mnemonic, operand_type, steps, arguments)?,
        }
        self.code.push(WasmBodyInstruction::Opcode(opcode));
        let result_type = wasm_stack_type(plan.result_type);
        match output {
            None => self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::Drop)),
            Some(output) => {
                self.validate_output_slot_type(output, result_type)?;
                self.assign_output_local(output);
            }
        }
        Ok(true)
    }

    /// `[wasm_operator("array.*")]` 冻结绑定：分派到与 MIR `Array*` 臂共享的发射器。
    ///
    /// arity 来自计划冻结事实；arraytype 索引由共享发射器在编码期按元素类型从
    /// RepresentationPlan 注册表解析，与 MIR 臂同一事实源，此处不重新推断。
    fn try_emit_wasm_gc_array_operator(
        &mut self,
        mnemonic: &str,
        plan: WasmGcArrayOperatorPlan,
        arguments: &[WasmOperand],
        output: Option<WasmValueKey>,
        instance: &WasmCallableKey,
    ) -> crate::Result<()> {
        if arguments.len() != plan.arity {
            return Err(format!(
                "WASM `wasm_operator` 数组绑定 `{mnemonic}` 实参数量与冻结 arity 不符: callee={instance:?} function={} expected={} actual={}",
                self.mir_fn.symbol,
                plan.arity,
                arguments.len()
            ));
        }
        match plan.operation {
            WasmGcArrayOperation::New => {
                let Some(output) = output
                else {
                    return Err(format!(
                        "WASM `wasm_operator` 数组绑定 `{mnemonic}` 缺少结果槽: callee={instance:?} function={}",
                        self.mir_fn.symbol
                    ));
                };
                let Some(element_type) = self.mir_fn.value_types.get(&output).and_then(|ty| array_element_type(ty).cloned())
                else {
                    return Err(format!(
                        "WASM `wasm_operator` 数组绑定 `{mnemonic}` 的结果槽缺少堆数组类型事实: callee={instance:?} function={} output=%{}",
                        self.mir_fn.symbol, output.0
                    ));
                };
                self.emit_array_new_default_ops(&element_type, &arguments[0], output)
            }
            WasmGcArrayOperation::Length => self.emit_array_length_ops(&arguments[0], output),
            WasmGcArrayOperation::Get => self.emit_array_get_ops(arguments, output),
            WasmGcArrayOperation::Set => self.emit_array_set_ops(arguments, output),
        }
    }

    /// `[wasm_operator("f32.rem")]` / `("f64.rem")`：精确截断余数的正式 lowering。
    ///
    /// wasm 没有原生浮点余数指令；语言合同（与 JVM `drem`、CLR `rem` 一致）是
    /// **精确**余数：符号随被除数、`|r| < |b|`、`x % ±inf = x`（有限 x）、
    /// `±inf % y = NaN`、`x % 0 = NaN`、`fmod(-5, 5) = -0`。naive 展开
    /// `a - trunc(a/b)*b` 在商超过 2^53 时不精确（`1e17 % 7` 合同值是 5，展开得
    /// 0），不得用作兑现手段。本序列只用 MVP 指令做「指数对齐 + 二分归约」：
    /// 倍增/减半是精确操作；每次减法前不变式 `s <= r < 2s` 成立，由 Sterbenz
    /// 引理减法精确。算法事实冻结在本函数体内，签名（arity、载体）已由计划
    /// 校验，发射只机械展开，不按助记符推断任何形状。
    fn emit_wasm_float_remainder(&mut self, mnemonic: &str, carrier: TargetOperandType, arguments: &[WasmOperand], output: Option<WasmValueKey>) -> crate::Result<()> {
        if arguments.len() != 2 {
            return Err(format!(
                "WASM `wasm_operator` 绑定 `{mnemonic}` 实参数量与冻结 arity 不符: expected=2 actual={} function={}",
                arguments.len(),
                self.mir_fn.symbol
            ));
        }
        let stack_type = wasm_stack_type(carrier);
        let (op_abs, op_ne, op_eq, op_lt, op_le, op_ge, op_mul, op_div, op_sub, op_copysign) = match carrier {
            TargetOperandType::F32 => (
                WasmOpcode::F32Abs,
                WasmOpcode::F32Ne,
                WasmOpcode::F32Eq,
                WasmOpcode::F32Lt,
                WasmOpcode::F32Le,
                WasmOpcode::F32Ge,
                WasmOpcode::F32Mul,
                WasmOpcode::F32Div,
                WasmOpcode::F32Sub,
                WasmOpcode::F32Copysign,
            ),
            TargetOperandType::F64 => (
                WasmOpcode::F64Abs,
                WasmOpcode::F64Ne,
                WasmOpcode::F64Eq,
                WasmOpcode::F64Lt,
                WasmOpcode::F64Le,
                WasmOpcode::F64Ge,
                WasmOpcode::F64Mul,
                WasmOpcode::F64Div,
                WasmOpcode::F64Sub,
                WasmOpcode::F64Copysign,
            ),
            other => {
                return Err(format!(
                    "WASM `wasm_operator` 绑定 `{mnemonic}` 的冻结载体不是浮点: carrier={other:?} function={}",
                    self.mir_fn.symbol
                ));
            }
        };
        self.emit_wasm_operator_operands(mnemonic, stack_type, arguments)?;
        // 栈上 `[a, b]`（b 在顶）：先收进两个输入 local，后续序列只读 local。
        let ly = self.alloc_scalar_local_for_stack_type(stack_type);
        self.emit_local_set(ly);
        let lx = self.alloc_scalar_local_for_stack_type(stack_type);
        self.emit_local_set(lx);
        let lr = self.alloc_scalar_local_for_stack_type(stack_type);
        let ls = self.alloc_scalar_local_for_stack_type(stack_type);
        let la = self.alloc_scalar_local_for_stack_type(stack_type);
        let lt = self.alloc_scalar_local_for_stack_type(stack_type);
        let lres = self.alloc_scalar_local_for_stack_type(stack_type);

        // block $done：三个早退守卫 + 归约主循环，结果统一写入 lres。
        self.emit_block_type(WasmOpcode::Block);
        self.emit_local_get(lx);
        self.code.push(WasmBodyInstruction::Opcode(op_abs));
        self.emit_local_set(lr);
        self.emit_local_get(ly);
        self.code.push(WasmBodyInstruction::Opcode(op_abs));
        self.emit_local_set(ls);
        self.emit_local_get(ls);
        self.emit_local_set(la);
        // 守卫 1：`a != a | b != b | b == 0 | |a| == inf` → NaN。
        // `ne` 自反比较识别 NaN；`eq inf` 在 NaN 上为假，不会误捕。
        self.emit_local_get(lr);
        self.emit_local_get(lr);
        self.code.push(WasmBodyInstruction::Opcode(op_ne));
        self.emit_local_get(ls);
        self.emit_local_get(ls);
        self.code.push(WasmBodyInstruction::Opcode(op_ne));
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::I32Or));
        self.emit_local_get(ls);
        self.emit_float_const(stack_type, 0.0);
        self.code.push(WasmBodyInstruction::Opcode(op_eq));
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::I32Or));
        self.emit_local_get(lr);
        self.emit_float_const(stack_type, f64::INFINITY);
        self.code.push(WasmBodyInstruction::Opcode(op_eq));
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::I32Or));
        self.emit_block_type(WasmOpcode::If);
        self.emit_float_const(stack_type, f64::NAN);
        self.emit_local_set(lres);
        self.code.push(WasmBodyInstruction::U32 { opcode: WasmOpcode::Br, operand: 1 });
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::End));
        // 守卫 2：有限 `x % ±inf = x`（`|a| == inf` 已在守卫 1 判 NaN）。
        self.emit_local_get(ls);
        self.emit_float_const(stack_type, f64::INFINITY);
        self.code.push(WasmBodyInstruction::Opcode(op_eq));
        self.emit_block_type(WasmOpcode::If);
        self.emit_local_get(lx);
        self.emit_local_set(lres);
        self.code.push(WasmBodyInstruction::U32 { opcode: WasmOpcode::Br, operand: 1 });
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::End));
        // 守卫 3：`x == 0`（含 ±0，返回 x 保留符号）或 `|x| < |y|` → x。
        self.emit_local_get(lr);
        self.emit_float_const(stack_type, 0.0);
        self.code.push(WasmBodyInstruction::Opcode(op_eq));
        self.emit_local_get(lr);
        self.emit_local_get(ls);
        self.code.push(WasmBodyInstruction::Opcode(op_lt));
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::I32Or));
        self.emit_block_type(WasmOpcode::If);
        self.emit_local_get(lx);
        self.emit_local_set(lres);
        self.code.push(WasmBodyInstruction::U32 { opcode: WasmOpcode::Br, operand: 1 });
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::End));
        // 对齐：倍增 `s` 直到 `s <= r < 2s`（`s*2` 溢出为 inf 时比较为假，自然停止）。
        self.emit_block_type(WasmOpcode::Loop);
        self.emit_local_get(ls);
        self.emit_float_const(stack_type, 2.0);
        self.code.push(WasmBodyInstruction::Opcode(op_mul));
        self.emit_local_tee(lt);
        self.emit_local_get(lr);
        self.code.push(WasmBodyInstruction::Opcode(op_le));
        self.emit_block_type(WasmOpcode::If);
        self.emit_local_get(lt);
        self.emit_local_set(ls);
        self.code.push(WasmBodyInstruction::U32 { opcode: WasmOpcode::Br, operand: 1 });
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::End));
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::End));
        // 归约：减半回 `|b|`，每层 `r >= s` 时减一次；减法前 `s <= r < 2s`，
        // Sterbenz 引理保证精确。`s == la` 时归约完成。
        self.emit_block_type(WasmOpcode::Block);
        self.emit_block_type(WasmOpcode::Loop);
        self.emit_local_get(lr);
        self.emit_local_get(ls);
        self.code.push(WasmBodyInstruction::Opcode(op_ge));
        self.emit_block_type(WasmOpcode::If);
        self.emit_local_get(lr);
        self.emit_local_get(ls);
        self.code.push(WasmBodyInstruction::Opcode(op_sub));
        self.emit_local_set(lr);
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::End));
        self.emit_local_get(ls);
        self.emit_local_get(la);
        self.code.push(WasmBodyInstruction::Opcode(op_eq));
        self.emit_block_type(WasmOpcode::If);
        self.code.push(WasmBodyInstruction::U32 { opcode: WasmOpcode::Br, operand: 2 });
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::End));
        self.emit_local_get(ls);
        self.emit_float_const(stack_type, 2.0);
        self.code.push(WasmBodyInstruction::Opcode(op_div));
        self.emit_local_set(ls);
        self.code.push(WasmBodyInstruction::U32 { opcode: WasmOpcode::Br, operand: 0 });
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::End));
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::End));
        // 结果符号随被除数（`r` 由绝对值算出，恒 ≥ 0；零结果也取 x 的符号）。
        self.emit_local_get(lr);
        self.emit_local_get(lx);
        self.code.push(WasmBodyInstruction::Opcode(op_copysign));
        self.emit_local_set(lres);
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::End));
        self.emit_local_get(lres);
        match output {
            None => self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::Drop)),
            Some(output) => {
                self.validate_output_slot_type(output, stack_type)?;
                self.assign_output_local(output);
            }
        }
        Ok(())
    }

    /// 按栈类型发射浮点常量（`f64::INFINITY`/`f64::NAN` 在 f32 上即 f32 语义值）。
    fn emit_float_const(&mut self, stack_type: u8, value: f64) {
        if stack_type == VALTYPE_F32 {
            self.code.push(WasmBodyInstruction::F32Const(value as f32));
        } else {
            self.code.push(WasmBodyInstruction::F64Const(value));
        }
    }

    /// 按计划冻结的步骤顺序机械编码常量与实参，再执行冻结 opcode。
    ///
    /// `i32.neg` 的 `0 - x` 配方已在此前的计划构造期展开为常量步骤 + 实参步骤；
    /// backend 不再按助记符判断取负，也不自行决定零常量。
    fn emit_wasm_operator_recipe(
        &mut self,
        mnemonic: &str,
        expected: u8,
        steps: &'static [TargetOperandStep],
        arguments: &[WasmOperand],
    ) -> crate::Result<()> {
        for step in steps {
            match step {
                TargetOperandStep::Arguments => self.emit_wasm_operator_operands(mnemonic, expected, arguments)?,
                TargetOperandStep::IntegerConstant { operand_type, value } => {
                    let carrier = wasm_stack_type(*operand_type);
                    if carrier != expected {
                        return Err(format!(
                            "WASM `wasm_operator` 绑定 `{mnemonic}` 的常量配方载体与冻结操作数载体不符: function={} expected={expected:#x} actual={carrier:#x}",
                            self.mir_fn.symbol
                        ));
                    }
                    match operand_type {
                        TargetOperandType::I32 => self.emit_i32_const(*value),
                        TargetOperandType::I64 => self.emit_i64_const(i64::from(*value)),
                        TargetOperandType::F32 | TargetOperandType::F64 => {
                            return Err(format!(
                                "WASM `wasm_operator` 绑定 `{mnemonic}` 冻结了浮点常量配方，但配方只支持整数常量: function={}",
                                self.mir_fn.symbol
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// 按目标绑定载体发射实参。SSA 槽的栈类型必须与计划冻结的实参载体一致；
    /// 常量没有独立位宽，交给目标载体在 `emit_load_constant_for_slot` 里选指令。
    fn emit_wasm_operator_operands(&mut self, mnemonic: &str, expected: u8, arguments: &[WasmOperand]) -> crate::Result<()> {
        for argument in arguments {
            if let WasmOperand::Value(_) = argument {
                let actual = self.operand_wasm_stack_type(argument)?;
                if actual != expected {
                    return Err(format!(
                        "WASM `wasm_operator` 实参载体失配 in `{}`: mnemonic=`{mnemonic}` expected={expected} actual={actual}",
                        self.mir_fn.symbol
                    ));
                }
            }
            self.emit_contract_operand(argument, expected)?;
        }
        Ok(())
    }

    pub(super) fn resolve_callee_import_index(&self, callee: &WasmOperand, _arguments: &[WasmOperand]) -> Option<u32> {
        let WasmOperand::Item(instance) = callee
        else {
            return None;
        };
        self.callee_import_index.get(instance).copied()
    }

    /// `i64` → wasm-gc struct `[i64]`（anyref），供泛型 `T` 数组槽使用。
    fn emit_box_i64_payload(&mut self, operand: &WasmOperand) -> crate::Result<()> {
        let box_ty = self.gc_i64_box_type_index;
        self.emit_struct_new_default(box_ty);
        let tmp = self.alloc_anyref_local();
        self.emit_local_set(tmp);
        self.emit_local_get(tmp);
        self.emit_ref_cast_struct(box_ty);
        self.emit_contract_operand(operand, VALTYPE_I64)?;
        self.emit_struct_set(box_ty, 0);
        self.emit_local_get(tmp);
        Ok(())
    }

    /// 栈顶 anyref（i64 box）→ i64。
    ///
    /// 载荷是 null 时不得回落 `i64 0`：缺值不是有效值。取消 null 分支后，
    /// `ref.cast (ref $i64_box)` 在 null 上执行即 trap，
    /// 由上游 boxing / `ArrayNew` default 合同保证载荷已初始化。
    pub(super) fn emit_unbox_i64_payload(&mut self) {
        let box_ty = self.gc_i64_box_type_index;
        self.emit_ref_cast_struct(box_ty);
        self.emit_struct_get(box_ty, 0);
    }

    /// `f64` → wasm-gc struct `[f64]`（anyref），供 `Option<F64>` 的 niche 载荷槽使用。
    fn emit_box_f64_payload(&mut self, operand: &WasmOperand) -> crate::Result<()> {
        let box_ty = self.gc_f64_box_type_index;
        self.emit_struct_new_default(box_ty);
        let tmp = self.alloc_anyref_local();
        self.emit_local_set(tmp);
        self.emit_local_get(tmp);
        self.emit_ref_cast_struct(box_ty);
        self.emit_contract_operand(operand, VALTYPE_F64)?;
        self.emit_struct_set(box_ty, 0);
        self.emit_local_get(tmp);
        Ok(())
    }

    /// 栈顶 anyref（f64 box）→ f64；null 不为 0.0，语义与 i64 拆箱一致。
    pub(super) fn emit_unbox_f64_payload(&mut self) {
        let box_ty = self.gc_f64_box_type_index;
        self.emit_ref_cast_struct(box_ty);
        self.emit_struct_get(box_ty, 0);
    }

    /// p3 `write-via-stream`：utf8 线性句?→?`stream.new` / write / drop →?宿主?
    ///
    /// 顺序必须是：new →??reader 交给 write-via-stream →?write(writer) →?drop-writable →?drop future?
    /// ?write 再交 reader 会在 sync `stream.write` 上永久挂起?
    fn try_emit_wasi_cli_write_via_stream(
        &mut self,
        arguments: &[WasmOperand],
        import_index: u32,
        output: Option<WasmValueKey>,
    ) -> crate::Result<bool> {
        if !self.wasi_mode || arguments.len() != 1 {
            return Ok(false);
        }
        let Some((module, field)) = self.host_imports.get(import_index as usize)
        else {
            return Ok(false);
        };
        if field != "write-via-stream" {
            return Ok(false);
        }
        if !(module.contains("stdout") || module.contains("stderr")) {
            return Ok(false);
        }
        let Some(stream_new) = self.find_host_import_index(module, "[stream-new-0]write-via-stream")
        else {
            return Ok(false);
        };
        let Some(stream_write) = self.find_host_import_index(module, "[stream-write-0]write-via-stream")
        else {
            return Ok(false);
        };
        let Some(stream_drop_w) = self.find_host_import_index(module, "[stream-drop-writable-0]write-via-stream")
        else {
            return Ok(false);
        };
        let Some(future_drop) = self.find_host_import_index(module, "[future-drop-readable-1]write-via-stream")
        else {
            return Ok(false);
        };
        // utf8 handle: [len:u32 LE][bytes…]
        let handle = self.alloc_i32_local();
        let len = self.alloc_i32_local();
        let bytes_ptr = self.alloc_i32_local();
        let pair = self.alloc_i64_local();
        let writer = self.alloc_i32_local();
        let reader = self.alloc_i32_local();
        let fut = self.alloc_i32_local();

        self.emit_operand_coerced(&arguments[0], VALTYPE_I32)?;
        self.emit_local_set(handle);

        self.emit_local_get(handle);
        self.code.push(WasmBodyInstruction::MemArg { opcode: WasmOpcode::I32Load, align: 2, offset: 0 });
        self.emit_local_set(len);

        self.emit_local_get(handle);
        self.emit_i32_const(4);
        self.emit_i32_add();
        self.emit_local_set(bytes_ptr);

        self.emit_call(stream_new);
        self.emit_local_set(pair);

        // reader = low32, writer = high32（与 wit-bindgen raw_stream_new 同构?
        self.emit_local_get(pair);
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::I32WrapI64));
        self.emit_local_set(reader);
        self.emit_local_get(pair);
        self.code.push(WasmBodyInstruction::I64Const(32));
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::I64ShrU));
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::I32WrapI64));
        self.emit_local_set(writer);

        self.emit_local_get(reader);
        self.emit_call(import_index);
        self.emit_local_set(fut);

        self.emit_local_get(writer);
        self.emit_local_get(bytes_ptr);
        self.emit_local_get(len);
        self.emit_call(stream_write);
        self.code.push(WasmBodyInstruction::Opcode(WasmOpcode::Drop));

        self.emit_local_get(writer);
        self.emit_call(stream_drop_w);

        self.emit_local_get(fut);
        self.emit_call(future_drop);

        if let Some(out) = output {
            // console write →?unit；void≠unit，用 anyref null 占位?
            self.validate_output_slot_type(out, VALTYPE_ANYREF)?;
            self.emit_ref_null_anyref();
            self.assign_output_local(out);
        }
        Ok(true)
    }

    pub(super) fn find_host_import_index(&self, module: &str, field: &str) -> Option<u32> {
        self.host_imports.iter().position(|(m, f)| m == module && f == field).map(|index| index as u32)
    }
}
