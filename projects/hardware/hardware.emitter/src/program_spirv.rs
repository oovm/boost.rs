use std::collections::BTreeMap;

use hardware_program::{
    validate_program, BinOp, CfgBlock, CfgInstruction, CfgTerminator, Expr,
    Function as UcfFunction, Program, ScalarTy, Stmt, Ty, UnOp,
};

use crate::spv::{
    self, builtin, decoration, execution_mode, execution_model, glsl, inst, op, storage, Spv,
};

/// Errors returned when a generic UCF program is outside the first native GPU subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramSpirvError {
    /// The program has no valid entry function.
    MissingEntry,
    /// The program uses a type that this lowering slice cannot represent.
    UnsupportedType(String),
    /// The program uses an expression that this lowering slice cannot represent.
    UnsupportedExpression(String),
    /// The program uses a statement that this lowering slice cannot represent.
    UnsupportedStatement(String),
    /// The generated module could not be encoded.
    Backend(String),
}

impl std::fmt::Display for ProgramSpirvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingEntry => f.write_str("generic UCF program has no entry function"),
            Self::UnsupportedType(v) => write!(f, "unsupported UCF GPU type: {v}"),
            Self::UnsupportedExpression(v) => write!(f, "unsupported UCF GPU expression: {v}"),
            Self::UnsupportedStatement(v) => write!(f, "unsupported UCF GPU statement: {v}"),
            Self::Backend(v) => write!(f, "generic UCF SPIR-V backend error: {v}"),
        }
    }
}

impl std::error::Error for ProgramSpirvError {}

/// Lower a generic scalar UCF program directly to SPIR-V.
///
/// This path assembles SPIR-V words from the UCF program: it never parses shader
/// source and never calls an external shader compiler. The entry result is
/// written to storage binding zero, bounds-checked against the push-constant
/// dispatch count.
pub fn emit_program_spirv(program: &Program, entry: &str) -> Result<Vec<u8>, ProgramSpirvError> {
    validate_program(program).map_err(|e| {
        ProgramSpirvError::UnsupportedStatement(format!("invalid UCF program: {e}"))
    })?;
    let function = program.entry_fn().ok_or(ProgramSpirvError::MissingEntry)?;
    if function.ret != Ty::f32() && function.ret != Ty::f64() {
        return Err(ProgramSpirvError::UnsupportedType(format!(
            "return type {:?}",
            function.ret
        )));
    }
    let float_width: u32 = if function.ret == Ty::f64() { 8 } else { 4 };
    let uniform_count = function
        .body
        .iter()
        .flat_map(uniform_slots_stmt)
        .max()
        .map(|slot| slot + 1)
        .unwrap_or(0);
    let uniform_limit = if float_width == 8 { 15 } else { 16 };
    if uniform_count > uniform_limit {
        return Err(ProgramSpirvError::UnsupportedType(format!(
            "push-constant uniform count {uniform_count} exceeds {uniform_limit} for scalar width {float_width}"
        )));
    }

    let mut spv = Spv::new();
    let float_ty = if float_width == 8 { spv.f64() } else { spv.f32_ };
    let u32_ty = spv.u32_;
    let bool_ty = spv.bool_;
    let vec3_ty = spv.vec3_f32;
    let gid_ty = spv.vec3_u32;

    let mut members = vec![(u32_ty, 0)];
    members.extend((0..uniform_count).map(|slot| (float_ty, float_width * (slot + 1))));
    let push_struct = spv.block_struct(&members);
    let push_ptr = spv.pointer(storage::PUSH_CONSTANT, push_struct);
    let push_u32_ptr = spv.pointer(storage::PUSH_CONSTANT, u32_ty);
    let push_float_ptr = spv.pointer(storage::PUSH_CONSTANT, float_ty);
    let push_var = spv.global(push_ptr, storage::PUSH_CONSTANT);

    let output_array = spv.runtime_array(float_ty, float_width);
    let output_struct = spv.block_struct(&[(output_array, 0)]);
    let output_ptr = spv.pointer(storage::STORAGE_BUFFER, output_struct);
    let element_ptr = spv.pointer(storage::STORAGE_BUFFER, float_ty);
    let output_var = spv.global(output_ptr, storage::STORAGE_BUFFER);
    spv.decorate(output_var, decoration::DESCRIPTOR_SET, &[0]);
    spv.decorate(output_var, decoration::BINDING, &[0]);

    let resource_slots = program
        .functions
        .iter()
        .flat_map(|function| function.body.iter().flat_map(resource_slots_stmt))
        .collect::<std::collections::BTreeSet<_>>();
    let mut buffer_vars = BTreeMap::new();
    for resource in resource_slots {
        let array = spv.runtime_array(float_ty, float_width);
        let structure = spv.block_struct(&[(array, 0)]);
        let pointer = spv.pointer(storage::STORAGE_BUFFER, structure);
        let variable = spv.global(pointer, storage::STORAGE_BUFFER);
        spv.decorate(variable, decoration::DESCRIPTOR_SET, &[0]);
        spv.decorate(variable, decoration::BINDING, &[resource + 1]);
        buffer_vars.insert(resource, variable);
    }

    let function_ty = spv.function_type(spv.void, &[gid_ty]);
    let function_id = spv.fresh();
    let mut words = Vec::new();
    inst(&mut words, op::FUNCTION, &[spv.void, function_id, 0, function_ty]);
    let gid = spv.fresh();
    inst(&mut words, op::FUNCTION_PARAMETER, &[gid_ty, gid]);
    spv.decorate(gid, decoration::BUILT_IN, &[builtin::GLOBAL_INVOCATION_ID]);
    let label = spv.fresh();
    inst(&mut words, op::LABEL, &[label]);

    let zero = spv.const_u32(0);
    let mut body = Vec::new();
    let gid_x = spv::extract(&mut spv, &mut body, u32_ty, gid, 0);
    let count_ptr = spv::chain(&mut spv, &mut body, push_u32_ptr, push_var, &[zero]);
    let count = spv::load(&mut spv, &mut body, u32_ty, count_ptr);
    let in_bounds = spv::binary(&mut spv, &mut body, bool_ty, op::U_LESS_THAN, gid_x, count);

    // Inactive invocations return before touching any storage buffer.
    let active = spv.fresh();
    let inactive = spv.fresh();
    let continue_block = spv.fresh();
    inst(&mut body, op::SELECTION_MERGE, &[continue_block, 0]);
    inst(&mut body, op::BRANCH_CONDITIONAL, &[in_bounds, active, inactive]);
    inst(&mut body, op::LABEL, &[inactive]);
    inst(&mut body, op::RETURN, &[]);
    inst(&mut body, op::LABEL, &[active]);
    inst(&mut body, op::BRANCH, &[continue_block]);
    inst(&mut body, op::LABEL, &[continue_block]);

    let mut lower = Lowerer {
        spv: &mut spv,
        float_width,
        float_ty,
        u32_ty,
        bool_ty,
        vec3_ty,
        gid_x,
        push_var,
        push_float_ptr,
        elem_ptr: element_ptr,
        buffer_vars,
        locals: BTreeMap::new(),
        local_kinds: BTreeMap::new(),
        local_decls: Vec::new(),
        body,
    };
    lower.local_kinds.insert(gid_x, Kind::U32);
    let interface_buffers = lower.buffer_vars.values().copied().collect::<Vec<_>>();
    let outcome = lower.lower_statements(function);
    let local_decls = std::mem::take(&mut lower.local_decls);
    let mut body = std::mem::take(&mut lower.body);
    let result = outcome?;
    drop(lower);

    let output_element = spv::chain(&mut spv, &mut body, element_ptr, output_var, &[zero, gid_x]);
    spv::store(&mut body, output_element, result);
    inst(&mut body, op::RETURN, &[]);

    words.extend_from_slice(&local_decls);
    words.append(&mut body);
    inst(&mut words, op::FUNCTION_END, &[]);
    spv.push_function(words);

    let mut interface = vec![push_var, output_var];
    interface.extend(interface_buffers);
    spv.entry_point(execution_model::GL_COMPUTE, function_id, entry, &interface);
    spv.execution_mode(function_id, execution_mode::LOCAL_SIZE, &[64, 1, 1]);
    Ok(spv.finish())
}

/// Value classes the encoder distinguishes when choosing an opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Float,
    U32,
    Bool,
    Vec3,
}

struct Lowerer<'a> {
    spv: &'a mut Spv,
    float_width: u32,
    float_ty: u32,
    u32_ty: u32,
    bool_ty: u32,
    vec3_ty: u32,
    gid_x: u32,
    push_var: u32,
    push_float_ptr: u32,
    elem_ptr: u32,
    buffer_vars: BTreeMap<u32, u32>,
    locals: BTreeMap<u32, u32>,
    local_kinds: BTreeMap<u32, Kind>,
    local_decls: Vec<u32>,
    body: Vec<u32>,
}

impl<'a> Lowerer<'a> {
    fn scalar_type(&self) -> Ty {
        if self.float_width == 8 {
            Ty::f64()
        } else {
            Ty::f32()
        }
    }

    fn type_of(&self, kind: Kind) -> u32 {
        match kind {
            Kind::Float => self.float_ty,
            Kind::U32 => self.u32_ty,
            Kind::Bool => self.bool_ty,
            Kind::Vec3 => self.vec3_ty,
        }
    }

    fn kind_of(&self, value: u32) -> Kind {
        self.local_kinds.get(&value).copied().unwrap_or(Kind::Float)
    }

    fn local_type(&self, ty: &Ty) -> Option<(u32, Kind)> {
        if *ty == self.scalar_type() {
            return Some((self.float_ty, Kind::Float));
        }
        match ty {
            Ty::Vec3 => Some((self.vec3_ty, Kind::Vec3)),
            Ty::Scalar(ScalarTy::U32) => Some((self.u32_ty, Kind::U32)),
            Ty::Scalar(ScalarTy::Bool) => Some((self.bool_ty, Kind::Bool)),
            _ => None,
        }
    }

    fn lower_statements(
        &mut self,
        function: &UcfFunction,
    ) -> Result<u32, ProgramSpirvError> {
        self.lower_sequence(&function.body)?
            .ok_or_else(|| ProgramSpirvError::UnsupportedStatement("entry has no return".into()))
    }

    /// Lower `statements` into a fresh block and return that block's words.
    fn lower_nested(&mut self, statements: &[Stmt]) -> Result<Vec<u32>, ProgramSpirvError> {
        let saved = std::mem::replace(&mut self.body, Vec::new());
        let result = self.lower_sequence(statements);
        let nested = std::mem::replace(&mut self.body, saved);
        result?;
        Ok(nested)
    }

    fn lower_sequence(
        &mut self,
        statements: &[Stmt],
    ) -> Result<Option<u32>, ProgramSpirvError> {
        let mut result = None;
        for stmt in statements {
            match stmt {
                Stmt::Cfg { entry, blocks } => {
                    if result.is_some() || statements.len() != 1 {
                        return Err(ProgramSpirvError::UnsupportedStatement(
                            "CFG must be the complete function body".into(),
                        ));
                    }
                    result = Some(self.lower_pure_cfg(*entry, blocks)?);
                }
                Stmt::Let { id, ty, init } => {
                    let (local_ty, kind) = self.local_type(ty).ok_or_else(|| {
                        ProgramSpirvError::UnsupportedType(format!("local {ty:?}"))
                    })?;
                    let pointer_ty = self.spv.pointer(storage::FUNCTION, local_ty);
                    let local = self.spv.fresh();
                    inst(
                        &mut self.local_decls,
                        op::VARIABLE,
                        &[pointer_ty, local, storage::FUNCTION],
                    );
                    let value = self.lower_expr(init)?;
                    spv::store(&mut self.body, local, value);
                    self.locals.insert(*id, local);
                    self.local_kinds.insert(local, kind);
                }
                Stmt::StoreLocal { id, value } => {
                    let local = *self.locals.get(id).ok_or_else(|| {
                        ProgramSpirvError::UnsupportedStatement(format!("unknown local {id}"))
                    })?;
                    let value = self.lower_expr(value)?;
                    spv::store(&mut self.body, local, value);
                }
                Stmt::StoreBuffer { resource, index, value } => {
                    let global = *self.buffer_vars.get(resource).ok_or_else(|| {
                        ProgramSpirvError::UnsupportedStatement(format!(
                            "unknown buffer resource {resource}"
                        ))
                    })?;
                    let index = self.lower_expr(index)?;
                    let value = self.lower_expr(value)?;
                    let zero = self.spv.const_u32(0);
                    let element =
                        spv::chain(self.spv, &mut self.body, self.elem_ptr, global, &[zero, index]);
                    spv::store(&mut self.body, element, value);
                }
                Stmt::For { iter, body, limit } => self.lower_for(*iter, *limit, body)?,
                Stmt::While { cond, body, limit } => self.lower_while(cond, *limit, body)?,
                Stmt::Return(value) => result = Some(self.lower_expr(value)?),
                Stmt::Expr(expr) => {
                    let _ = self.lower_expr(expr)?;
                }
                other => {
                    return Err(ProgramSpirvError::UnsupportedStatement(format!(
                        "{other:?}"
                    )))
                }
            }
        }
        Ok(result)
    }

    fn lower_for(
        &mut self,
        iter: u32,
        limit: u32,
        body: &[Stmt],
    ) -> Result<(), ProgramSpirvError> {
        let pointer_ty = self.spv.pointer(storage::FUNCTION, self.u32_ty);
        let counter = self.spv.fresh();
        inst(
            &mut self.local_decls,
            op::VARIABLE,
            &[pointer_ty, counter, storage::FUNCTION],
        );
        let zero = self.spv.const_u32(0);
        let one = self.spv.const_u32(1);
        let limit_id = self.spv.const_u32(limit);
        spv::store(&mut self.body, counter, zero);

        let loop_label = self.spv.fresh();
        let body_label = self.spv.fresh();
        let continue_label = self.spv.fresh();
        let merge_label = self.spv.fresh();
        inst(&mut self.body, op::BRANCH, &[loop_label]);
        inst(&mut self.body, op::LABEL, &[loop_label]);
        inst(
            &mut self.body,
            op::LOOP_MERGE,
            &[merge_label, continue_label, 0],
        );
        let current = spv::load(self.spv, &mut self.body, self.u32_ty, counter);
        let done = spv::binary(
            self.spv,
            &mut self.body,
            self.bool_ty,
            op::U_GREATER_THAN_EQUAL,
            current,
            limit_id,
        );
        inst(
            &mut self.body,
            op::BRANCH_CONDITIONAL,
            &[done, merge_label, body_label],
        );
        inst(&mut self.body, op::LABEL, &[body_label]);

        let previous = self.locals.insert(iter, counter);
        let previous_kind = self.local_kinds.insert(counter, Kind::U32);
        let nested = self.lower_nested(body)?;
        self.body.extend_from_slice(&nested);
        if let Some(previous) = previous {
            self.locals.insert(iter, previous);
        } else {
            self.locals.remove(&iter);
        }
        if let Some(previous) = previous_kind {
            self.local_kinds.insert(counter, previous);
        }

        inst(&mut self.body, op::BRANCH, &[continue_label]);
        inst(&mut self.body, op::LABEL, &[continue_label]);
        let current = spv::load(self.spv, &mut self.body, self.u32_ty, counter);
        let next = spv::binary(self.spv, &mut self.body, self.u32_ty, op::I_ADD, current, one);
        spv::store(&mut self.body, counter, next);
        inst(&mut self.body, op::BRANCH, &[loop_label]);
        inst(&mut self.body, op::LABEL, &[merge_label]);
        Ok(())
    }

    fn lower_while(
        &mut self,
        cond: &Expr,
        limit: u32,
        body: &[Stmt],
    ) -> Result<(), ProgramSpirvError> {
        let pointer_ty = self.spv.pointer(storage::FUNCTION, self.u32_ty);
        let counter = self.spv.fresh();
        inst(
            &mut self.local_decls,
            op::VARIABLE,
            &[pointer_ty, counter, storage::FUNCTION],
        );
        let zero = self.spv.const_u32(0);
        let one = self.spv.const_u32(1);
        let limit_id = self.spv.const_u32(limit);
        spv::store(&mut self.body, counter, zero);

        let loop_label = self.spv.fresh();
        let body_label = self.spv.fresh();
        let continue_label = self.spv.fresh();
        let merge_label = self.spv.fresh();
        inst(&mut self.body, op::BRANCH, &[loop_label]);
        inst(&mut self.body, op::LABEL, &[loop_label]);
        inst(
            &mut self.body,
            op::LOOP_MERGE,
            &[merge_label, continue_label, 0],
        );
        let current = spv::load(self.spv, &mut self.body, self.u32_ty, counter);
        let exhausted = spv::binary(
            self.spv,
            &mut self.body,
            self.bool_ty,
            op::U_GREATER_THAN_EQUAL,
            current,
            limit_id,
        );
        let condition = self.lower_expr(cond)?;
        let negated = spv::unary(self.spv, &mut self.body, self.bool_ty, op::LOGICAL_NOT, condition);
        let stop = spv::binary(
            self.spv,
            &mut self.body,
            self.bool_ty,
            op::LOGICAL_OR,
            exhausted,
            negated,
        );
        inst(
            &mut self.body,
            op::BRANCH_CONDITIONAL,
            &[stop, merge_label, body_label],
        );
        inst(&mut self.body, op::LABEL, &[body_label]);

        let nested = self.lower_nested(body)?;
        self.body.extend_from_slice(&nested);

        inst(&mut self.body, op::BRANCH, &[continue_label]);
        inst(&mut self.body, op::LABEL, &[continue_label]);
        let current = spv::load(self.spv, &mut self.body, self.u32_ty, counter);
        let next = spv::binary(self.spv, &mut self.body, self.u32_ty, op::I_ADD, current, one);
        spv::store(&mut self.body, counter, next);
        inst(&mut self.body, op::BRANCH, &[loop_label]);
        inst(&mut self.body, op::LABEL, &[merge_label]);
        Ok(())
    }

    fn lower_pure_cfg(
        &mut self,
        entry: u32,
        blocks: &[CfgBlock],
    ) -> Result<u32, ProgramSpirvError> {
        let entry_block = blocks.iter().find(|block| block.id == entry).ok_or_else(|| {
            ProgramSpirvError::UnsupportedStatement("CFG entry block is missing".into())
        })?;
        if blocks.len() == 1 {
            let CfgTerminator::Return(value) = entry_block.terminator else {
                return Err(ProgramSpirvError::UnsupportedStatement(
                    "single-block CFG must return".into(),
                ));
            };
            let mut values = BTreeMap::new();
            for instruction in &entry_block.instructions {
                self.lower_cfg_instruction(instruction, &mut values)?;
            }
            return values.get(&value).copied().ok_or_else(|| {
                ProgramSpirvError::UnsupportedStatement(format!(
                    "CFG return value {value} is not defined"
                ))
            });
        }
        let CfgTerminator::ConditionalBranch {
            condition,
            then_block,
            then_args,
            else_block,
            else_args,
        } = &entry_block.terminator
        else {
            return Err(ProgramSpirvError::UnsupportedStatement(
                "GPU CFG subset requires one conditional entry branch".into(),
            ));
        };
        let mut values = BTreeMap::new();
        for instruction in &entry_block.instructions {
            self.lower_cfg_instruction(instruction, &mut values)?;
        }
        let condition = *values.get(condition).ok_or_else(|| {
            ProgramSpirvError::UnsupportedStatement(
                "CFG condition is not defined in entry block".into(),
            )
        })?;
        let then_value = self.lower_cfg_return_block(*then_block, then_args, blocks, &mut values)?;
        let else_value = self.lower_cfg_return_block(*else_block, else_args, blocks, &mut values)?;
        Ok(spv::select(
            self.spv,
            &mut self.body,
            self.float_ty,
            condition,
            then_value,
            else_value,
        ))
    }

    fn lower_cfg_return_block(
        &mut self,
        block_id: u32,
        args: &[u32],
        blocks: &[CfgBlock],
        values: &mut BTreeMap<u32, u32>,
    ) -> Result<u32, ProgramSpirvError> {
        let block = blocks.iter().find(|block| block.id == block_id).ok_or_else(|| {
            ProgramSpirvError::UnsupportedStatement(format!(
                "CFG target block {block_id} is missing"
            ))
        })?;
        if block.parameters.len() != args.len() {
            return Err(ProgramSpirvError::UnsupportedStatement(
                "CFG branch argument count does not match target parameters".into(),
            ));
        }
        for (parameter, argument) in block.parameters.iter().zip(args) {
            let value = *values.get(argument).ok_or_else(|| {
                ProgramSpirvError::UnsupportedExpression(format!(
                    "unknown CFG branch argument {argument}"
                ))
            })?;
            values.insert(parameter.value, value);
        }
        for instruction in &block.instructions {
            self.lower_cfg_instruction(instruction, values)?;
        }
        let CfgTerminator::Return(value) = block.terminator else {
            return Err(ProgramSpirvError::UnsupportedStatement(
                "GPU CFG subset requires branch blocks to return directly".into(),
            ));
        };
        values.get(&value).copied().ok_or_else(|| {
            ProgramSpirvError::UnsupportedStatement(format!(
                "CFG return value {value} is not defined"
            ))
        })
    }

    fn lower_cfg_instruction(
        &mut self,
        instruction: &CfgInstruction,
        values: &mut BTreeMap<u32, u32>,
    ) -> Result<(), ProgramSpirvError> {
        if let CfgInstruction::StoreBuffer { resource, index, value } = instruction {
            let global = *self.buffer_vars.get(resource).ok_or_else(|| {
                ProgramSpirvError::UnsupportedStatement(format!("unknown buffer resource {resource}"))
            })?;
            let index = *values.get(index).ok_or_else(|| {
                ProgramSpirvError::UnsupportedExpression(format!("unknown CFG value {index}"))
            })?;
            let value = *values.get(value).ok_or_else(|| {
                ProgramSpirvError::UnsupportedExpression(format!("unknown CFG value {value}"))
            })?;
            let zero = self.spv.const_u32(0);
            let element =
                spv::chain(self.spv, &mut self.body, self.elem_ptr, global, &[zero, index]);
            spv::store(&mut self.body, element, value);
            return Ok(());
        }
        let (handle, kind) = match instruction {
            CfgInstruction::ConstF32 { value, .. } => {
                (self.spv.const_f32(*value), Kind::Float)
            }
            CfgInstruction::ConstF64 { value, .. } => {
                if self.float_width != 8 {
                    return Err(ProgramSpirvError::UnsupportedType(
                        "F64 constant in an F32 program".into(),
                    ));
                }
                (self.spv.const_f64(*value), Kind::Float)
            }
            CfgInstruction::ConstU32 { value, .. } => {
                (self.spv.const_u32(*value), Kind::U32)
            }
            CfgInstruction::LoadUniform { slot, ty, .. } => {
                if *ty != self.scalar_type() {
                    return Err(ProgramSpirvError::UnsupportedType(format!(
                        "uniform {slot}: {ty:?}"
                    )));
                }
                let index = self.spv.const_u32(*slot + 1);
                let field = spv::chain(
                    self.spv,
                    &mut self.body,
                    self.push_float_ptr,
                    self.push_var,
                    &[index],
                );
                (
                    spv::load(self.spv, &mut self.body, self.float_ty, field),
                    Kind::Float,
                )
            }
            CfgInstruction::GlobalId { .. } => (self.gid_x, Kind::U32),
            CfgInstruction::LoadBuffer { resource, index, ty, .. } => {
                if *ty != self.scalar_type() {
                    return Err(ProgramSpirvError::UnsupportedType(format!(
                        "buffer {resource}: {ty:?}"
                    )));
                }
                let global = *self.buffer_vars.get(resource).ok_or_else(|| {
                    ProgramSpirvError::UnsupportedStatement(format!(
                        "unknown buffer resource {resource}"
                    ))
                })?;
                let index = *values.get(index).ok_or_else(|| {
                    ProgramSpirvError::UnsupportedExpression(format!("unknown CFG value {index}"))
                })?;
                let zero = self.spv.const_u32(0);
                let element =
                    spv::chain(self.spv, &mut self.body, self.elem_ptr, global, &[zero, index]);
                (
                    spv::load(self.spv, &mut self.body, self.float_ty, element),
                    Kind::Float,
                )
            }
            CfgInstruction::Binary { op, lhs, rhs, .. } => {
                let left = *values.get(lhs).ok_or_else(|| {
                    ProgramSpirvError::UnsupportedExpression(format!("unknown CFG value {lhs}"))
                })?;
                let right = *values.get(rhs).ok_or_else(|| {
                    ProgramSpirvError::UnsupportedExpression(format!("unknown CFG value {rhs}"))
                })?;
                self.lower_binary(*op, left, right)?
            }
            CfgInstruction::Select { cond, then_, else_, .. } => {
                let condition = *values.get(cond).ok_or_else(|| {
                    ProgramSpirvError::UnsupportedExpression(format!("unknown CFG value {cond}"))
                })?;
                let accept = *values.get(then_).ok_or_else(|| {
                    ProgramSpirvError::UnsupportedExpression(format!("unknown CFG value {then_}"))
                })?;
                let reject = *values.get(else_).ok_or_else(|| {
                    ProgramSpirvError::UnsupportedExpression(format!("unknown CFG value {else_}"))
                })?;
                let kind = self.kind_of(accept);
                let ty = self.type_of(kind);
                (
                    spv::select(self.spv, &mut self.body, ty, condition, accept, reject),
                    kind,
                )
            }
            CfgInstruction::Unary { op, arg, .. } => {
                let value = *values.get(arg).ok_or_else(|| {
                    ProgramSpirvError::UnsupportedExpression(format!("unknown CFG value {arg}"))
                })?;
                self.lower_unary(*op, value)?
            }
            _ => {
                return Err(ProgramSpirvError::UnsupportedStatement(
                    "CFG instruction is outside the pure F32 GPU subset".into(),
                ))
            }
        };
        let result = match instruction {
            CfgInstruction::ConstF32 { result, .. }
            | CfgInstruction::ConstF64 { result, .. }
            | CfgInstruction::ConstU32 { result, .. }
            | CfgInstruction::LoadUniform { result, .. }
            | CfgInstruction::LoadBuffer { result, .. }
            | CfgInstruction::GlobalId { result }
            | CfgInstruction::Binary { result, .. }
            | CfgInstruction::Unary { result, .. }
            | CfgInstruction::Select { result, .. } => *result,
            CfgInstruction::StoreBuffer { .. } => unreachable!(),
            _ => unreachable!("unsupported CFG instruction did not produce a value"),
        };
        if kind != Kind::Float {
            self.local_kinds.insert(handle, kind);
        }
        values.insert(result, handle);
        Ok(())
    }

    fn lower_expr(&mut self, expr: &Expr) -> Result<u32, ProgramSpirvError> {
        match expr {
            Expr::ConstF32(v) => Ok(self.spv.const_f32(*v)),
            Expr::ConstF64(v) if self.float_width == 8 => Ok(self.spv.const_f64(*v)),
            Expr::ConstBool(v) => Ok(self.spv.const_bool(*v)),
            Expr::ConstU32(v) => {
                let id = self.spv.const_u32(*v);
                self.local_kinds.insert(id, Kind::U32);
                Ok(id)
            }
            Expr::GlobalId => Ok(self.gid_x),
            Expr::LoadUniform(slot) => {
                let index = self.spv.const_u32(slot + 1);
                let field = spv::chain(
                    self.spv,
                    &mut self.body,
                    self.push_float_ptr,
                    self.push_var,
                    &[index],
                );
                Ok(spv::load(self.spv, &mut self.body, self.float_ty, field))
            }
            Expr::LoadLocal(id) => {
                let local = *self.locals.get(id).ok_or_else(|| {
                    ProgramSpirvError::UnsupportedExpression(format!("unknown local {id}"))
                })?;
                let kind = self.local_kinds.get(&local).copied().unwrap_or(Kind::Float);
                let ty = self.type_of(kind);
                Ok(spv::load(self.spv, &mut self.body, ty, local))
            }
            Expr::LoadBuffer { resource, index } => {
                let global = *self.buffer_vars.get(resource).ok_or_else(|| {
                    ProgramSpirvError::UnsupportedExpression(format!(
                        "unknown buffer resource {resource}"
                    ))
                })?;
                let index = self.lower_expr(index)?;
                let zero = self.spv.const_u32(0);
                let element =
                    spv::chain(self.spv, &mut self.body, self.elem_ptr, global, &[zero, index]);
                Ok(spv::load(self.spv, &mut self.body, self.float_ty, element))
            }
            Expr::BinOp { op, lhs, rhs } => {
                let left = self.lower_expr(lhs)?;
                let right = self.lower_expr(rhs)?;
                Ok(self.lower_binary(*op, left, right)?.0)
            }
            Expr::Vec3Compose { x, y, z } => {
                let x = self.lower_expr(x)?;
                let y = self.lower_expr(y)?;
                let z = self.lower_expr(z)?;
                let id = spv::construct(self.spv, &mut self.body, self.vec3_ty, &[x, y, z]);
                self.local_kinds.insert(id, Kind::Vec3);
                Ok(id)
            }
            Expr::UnOp { op, arg } => {
                let value = self.lower_expr(arg)?;
                Ok(self.lower_unary(*op, value)?.0)
            }
            Expr::Select { cond, then_, else_ } => {
                let condition = self.lower_expr(cond)?;
                let accept = self.lower_expr(then_)?;
                let reject = self.lower_expr(else_)?;
                let kind = self.kind_of(accept);
                let ty = self.type_of(kind);
                Ok(spv::select(
                    self.spv,
                    &mut self.body,
                    ty,
                    condition,
                    accept,
                    reject,
                ))
            }
            other => Err(ProgramSpirvError::UnsupportedExpression(format!(
                "{other:?}"
            ))),
        }
    }

    fn lower_binary(
        &mut self,
        op: BinOp,
        lhs: u32,
        rhs: u32,
    ) -> Result<(u32, Kind), ProgramSpirvError> {
        let kind = self.kind_of(lhs);
        let float = kind != Kind::U32;
        if self.float_width == 8 && matches!(op, BinOp::Pow | BinOp::Atan2) {
            return Err(ProgramSpirvError::UnsupportedExpression(format!(
                "F64 {op:?} is not implemented"
            )));
        }
        let id = match op {
            BinOp::Min => spv::ext(self.spv, &mut self.body, self.float_ty, glsl::F_MIN, &[lhs, rhs]),
            BinOp::Max => spv::ext(self.spv, &mut self.body, self.float_ty, glsl::F_MAX, &[lhs, rhs]),
            BinOp::Pow => spv::ext(self.spv, &mut self.body, self.float_ty, glsl::POW, &[lhs, rhs]),
            BinOp::Atan2 => {
                spv::ext(self.spv, &mut self.body, self.float_ty, glsl::ATAN2, &[lhs, rhs])
            }
            BinOp::Add => self.float_binary(op::F_ADD, op::I_ADD, lhs, rhs, float),
            BinOp::Sub => self.float_binary(op::F_SUB, op::I_SUB, lhs, rhs, float),
            BinOp::Mul => self.float_binary(op::F_MUL, op::I_MUL, lhs, rhs, float),
            BinOp::Div => self.float_binary(op::F_DIV, op::U_DIV, lhs, rhs, float),
            BinOp::Mod => self.float_binary(op::F_MOD, op::U_MOD, lhs, rhs, float),
            BinOp::Lt => self.compare(op::F_ORD_LESS_THAN, op::U_LESS_THAN, lhs, rhs, float),
            BinOp::Le => self.compare(
                op::F_ORD_LESS_THAN_EQUAL,
                op::U_LESS_THAN_EQUAL,
                lhs,
                rhs,
                float,
            ),
            BinOp::Gt => self.compare(op::F_ORD_GREATER_THAN, op::U_GREATER_THAN, lhs, rhs, float),
            BinOp::Ge => self.compare(
                op::F_ORD_GREATER_THAN_EQUAL,
                op::U_GREATER_THAN_EQUAL,
                lhs,
                rhs,
                float,
            ),
            BinOp::Eq => self.compare(op::F_ORD_EQUAL, op::I_EQUAL, lhs, rhs, float),
            BinOp::Ne => self.compare(op::F_ORD_NOT_EQUAL, op::I_NOT_EQUAL, lhs, rhs, float),
            other => {
                return Err(ProgramSpirvError::UnsupportedExpression(format!(
                    "binary {other:?}"
                )))
            }
        };
        let result = if matches!(
            op,
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::Eq | BinOp::Ne
        ) {
            Kind::Bool
        } else if float {
            Kind::Float
        } else {
            Kind::U32
        };
        if result != Kind::Float {
            self.local_kinds.insert(id, result);
        }
        Ok((id, result))
    }

    fn float_binary(&mut self, float_op: u16, int_op: u16, lhs: u32, rhs: u32, float: bool) -> u32 {
        let ty = if float { self.float_ty } else { self.u32_ty };
        let opcode = if float { float_op } else { int_op };
        let id = spv::binary(self.spv, &mut self.body, ty, opcode, lhs, rhs);
        if float {
            self.spv.decorate(id, decoration::NO_CONTRACTION, &[]);
        }
        id
    }

    fn compare(&mut self, float_op: u16, int_op: u16, lhs: u32, rhs: u32, float: bool) -> u32 {
        let opcode = if float { float_op } else { int_op };
        spv::binary(self.spv, &mut self.body, self.bool_ty, opcode, lhs, rhs)
    }

    fn lower_unary(&mut self, op: UnOp, arg: u32) -> Result<(u32, Kind), ProgramSpirvError> {
        if self.float_width == 8 && matches!(op, UnOp::Sin | UnOp::Cos | UnOp::Atan | UnOp::Ln) {
            return Err(ProgramSpirvError::UnsupportedExpression(format!(
                "F64 {op:?} is not implemented"
            )));
        }
        let id = match op {
            UnOp::Neg => {
                spv::unary(self.spv, &mut self.body, self.float_ty, op::F_NEGATE, arg)
            }
            UnOp::Sqrt => spv::ext(self.spv, &mut self.body, self.float_ty, glsl::SQRT, &[arg]),
            UnOp::Sin => spv::ext(self.spv, &mut self.body, self.float_ty, glsl::SIN, &[arg]),
            UnOp::Cos => spv::ext(self.spv, &mut self.body, self.float_ty, glsl::COS, &[arg]),
            UnOp::Atan => spv::ext(self.spv, &mut self.body, self.float_ty, glsl::ATAN, &[arg]),
            UnOp::Abs => spv::ext(self.spv, &mut self.body, self.float_ty, glsl::F_ABS, &[arg]),
            UnOp::Ln => spv::ext(self.spv, &mut self.body, self.float_ty, glsl::LOG, &[arg]),
            UnOp::Vec3Comp(index) => {
                return Ok((
                    spv::extract(self.spv, &mut self.body, self.float_ty, arg, u32::from(index)),
                    Kind::Float,
                ))
            }
            UnOp::IsFinite => {
                let same = spv::binary(
                    self.spv,
                    &mut self.body,
                    self.bool_ty,
                    op::F_ORD_EQUAL,
                    arg,
                    arg,
                );
                let absolute =
                    spv::ext(self.spv, &mut self.body, self.float_ty, glsl::F_ABS, &[arg]);
                let limit = if self.float_width == 8 {
                    self.spv.const_f64(f64::MAX)
                } else {
                    self.spv.const_f32(f32::MAX)
                };
                let below = spv::binary(
                    self.spv,
                    &mut self.body,
                    self.bool_ty,
                    op::F_ORD_LESS_THAN_EQUAL,
                    absolute,
                    limit,
                );
                let never = self.spv.const_bool(false);
                let id = spv::select(
                    self.spv,
                    &mut self.body,
                    self.bool_ty,
                    same,
                    below,
                    never,
                );
                return Ok((id, Kind::Bool));
            }
            other => {
                return Err(ProgramSpirvError::UnsupportedExpression(format!(
                    "unary {other:?}"
                )))
            }
        };
        Ok((id, Kind::Float))
    }
}

fn uniform_slots_stmt(stmt: &Stmt) -> Vec<u32> {
    let mut slots = Vec::new();
    fn visit_expr(value: &Expr, slots: &mut Vec<u32>) {
        match value {
            Expr::LoadUniform(slot) => slots.push(*slot),
            Expr::BinOp { lhs, rhs, .. } => {
                visit_expr(lhs, slots);
                visit_expr(rhs, slots);
            }
            Expr::UnOp { arg, .. } => visit_expr(arg, slots),
            Expr::Select { cond, then_, else_ } => {
                visit_expr(cond, slots);
                visit_expr(then_, slots);
                visit_expr(else_, slots);
            }
            _ => {}
        }
    }
    match stmt {
        Stmt::Let { init, .. }
        | Stmt::StoreLocal { value: init, .. }
        | Stmt::Return(init)
        | Stmt::Expr(init) => visit_expr(init, &mut slots),
        Stmt::If { cond, then_body, else_body } => {
            visit_expr(cond, &mut slots);
            for stmt in then_body.iter().chain(else_body) {
                slots.extend(uniform_slots_stmt(stmt));
            }
        }
        Stmt::For { body, .. } => {
            for stmt in body {
                slots.extend(uniform_slots_stmt(stmt));
            }
        }
        Stmt::While { cond, body, .. } => {
            visit_expr(cond, &mut slots);
            for stmt in body {
                slots.extend(uniform_slots_stmt(stmt));
            }
        }
        Stmt::StoreBuffer { index, value, .. } => {
            visit_expr(index, &mut slots);
            visit_expr(value, &mut slots);
        }
        Stmt::Break | Stmt::Continue => {}
        Stmt::Cfg { blocks, .. } => {
            for block in blocks {
                for instruction in &block.instructions {
                    if let CfgInstruction::LoadUniform { slot, .. } = instruction {
                        slots.push(*slot);
                    }
                }
            }
        }
    }
    slots
}

fn resource_slots_stmt(stmt: &Stmt) -> Vec<u32> {
    let mut slots = Vec::new();
    fn visit_expr(value: &Expr, slots: &mut Vec<u32>) {
        match value {
            Expr::LoadBuffer { resource, index } => {
                slots.push(*resource);
                visit_expr(index, slots);
            }
            Expr::BinOp { lhs, rhs, .. } => {
                visit_expr(lhs, slots);
                visit_expr(rhs, slots);
            }
            Expr::UnOp { arg, .. } => visit_expr(arg, slots),
            Expr::Select { cond, then_, else_ } => {
                visit_expr(cond, slots);
                visit_expr(then_, slots);
                visit_expr(else_, slots);
            }
            Expr::Vec3Compose { x, y, z } => {
                visit_expr(x, slots);
                visit_expr(y, slots);
                visit_expr(z, slots);
            }
            Expr::Call { args, .. } => {
                for arg in args {
                    visit_expr(arg, slots);
                }
            }
            _ => {}
        }
    }
    match stmt {
        Stmt::Let { init, .. }
        | Stmt::StoreLocal { value: init, .. }
        | Stmt::Return(init)
        | Stmt::Expr(init) => visit_expr(init, &mut slots),
        Stmt::StoreBuffer { resource, index, value } => {
            slots.push(*resource);
            visit_expr(index, &mut slots);
            visit_expr(value, &mut slots);
        }
        Stmt::If { cond, then_body, else_body } => {
            visit_expr(cond, &mut slots);
            for nested in then_body.iter().chain(else_body) {
                slots.extend(resource_slots_stmt(nested));
            }
        }
        Stmt::For { body, .. } | Stmt::While { body, .. } => {
            for nested in body {
                slots.extend(resource_slots_stmt(nested));
            }
        }
        Stmt::Break | Stmt::Continue => {}
        Stmt::Cfg { blocks, .. } => {
            for block in blocks {
                for instruction in &block.instructions {
                    match instruction {
                        CfgInstruction::LoadBuffer { resource, .. }
                        | CfgInstruction::StoreBuffer { resource, .. } => slots.push(*resource),
                        _ => {}
                    }
                }
            }
        }
    }
    slots
}

#[cfg(test)]
mod tests {
    use super::*;
    use hardware_program::{FunctionBuilder, ProgramBuilder};

    /// Decode a module into `(opcode, operands)` pairs.
    fn decode(bytes: &[u8]) -> Vec<(u16, Vec<u32>)> {
        let words: Vec<u32> = bytes
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        let mut out = Vec::new();
        let mut index = 5;
        while index < words.len() {
            let word = words[index];
            let length = (word >> 16) as usize;
            let opcode = (word & 0xffff) as u16;
            let operands = words[index + 1..index + length].to_vec();
            out.push((opcode, operands));
            index += length;
        }
        out
    }

    #[test]
    fn float64_module_uses_native_width_and_rejects_unsupported_math() {
        let mut builder = FunctionBuilder::new("native_precision");
        let initial = builder.emit_const_f64(16777216.0).unwrap();
        let increment = builder.emit_const_f64(0.125).unwrap();
        let result = builder.emit_binary(BinOp::Add, initial, increment).unwrap();
        builder.return_value(result).unwrap();
        let program = ProgramBuilder::new().entry(builder).unwrap().finish().unwrap();
        let module = decode(&super::emit_program_spirv(&program, "native_precision").unwrap());
        assert!(module
            .iter()
            .any(|(opcode, operands)| *opcode == op::TYPE_FLOAT
                && operands.len() == 2
                && operands[1] == 64));
        let mut builder = FunctionBuilder::new("unsupported_math");
        let base = builder.emit_const_f64(2.0).unwrap();
        let exponent = builder.emit_const_f64(0.5).unwrap();
        let result = builder.emit_binary(BinOp::Pow, base, exponent).unwrap();
        builder.return_value(result).unwrap();
        let program = ProgramBuilder::new().entry(builder).unwrap().finish().unwrap();
        assert!(matches!(
            super::emit_program_spirv(&program, "unsupported_math"),
            Err(super::ProgramSpirvError::UnsupportedExpression(_))
        ));
    }

    #[test]
    fn floating_arithmetic_results_disallow_contraction() {
        let mut builder = hardware_program::FunctionBuilder::new("float_order");
        let value = builder.emit_load_uniform(0, Ty::f32()).unwrap();
        let one = builder.emit_const_f32(1.0).unwrap();
        let sum = builder.emit_binary(BinOp::Add, value, one).unwrap();
        let result = builder.emit_binary(BinOp::Sub, sum, value).unwrap();
        builder.return_value(result).unwrap();
        let program = hardware_program::ProgramBuilder::new()
            .entry(builder)
            .unwrap()
            .finish()
            .unwrap();
        let bytes = emit_program_spirv(&program, "precise_main").unwrap();
        let module = decode(&bytes);
        let targets: Vec<u32> = module
            .iter()
            .filter(|(opcode, _)| *opcode == op::F_ADD || *opcode == op::F_SUB)
            .map(|(_, operands)| operands[1])
            .collect();
        assert!(!targets.is_empty());
        for target in targets {
            assert!(module.iter().any(|(opcode, operands)| *opcode == op::DECORATE
                && operands.len() == 2
                && operands[0] == target
                && operands[1] == decoration::NO_CONTRACTION));
        }
    }

    #[test]
    fn rejects_f64_uniform_in_f32_returning_program() {
        let mut builder = hardware_program::FunctionBuilder::new("f64_uniform");
        let value = builder.emit_load_uniform(0, Ty::f64()).unwrap();
        let condition = builder.emit_binary(BinOp::Eq, value, value).unwrap();
        let one = builder.emit_const_f32(1.0).unwrap();
        let zero = builder.emit_const_f32(0.0).unwrap();
        let result = builder.emit_select(condition, one, zero).unwrap();
        builder.return_value(result).unwrap();
        let program = hardware_program::ProgramBuilder::new()
            .entry(builder)
            .unwrap()
            .finish()
            .unwrap();
        assert!(matches!(
            emit_program_spirv(&program, "unsupported_main"),
            Err(ProgramSpirvError::UnsupportedType(_))
        ));
    }

    #[test]
    fn lowers_scalar_program_without_shader_source() {
        let program = Program {
            version: 1,
            functions: vec![UcfFunction {
                name: "main".into(),
                params: Vec::new(),
                locals: Vec::new(),
                body: vec![Stmt::Return(Expr::BinOp {
                    op: BinOp::Add,
                    lhs: Box::new(Expr::ConstF32(1.0)),
                    rhs: Box::new(Expr::ConstF32(2.0)),
                })],
                ret: Ty::f32(),
            }],
            entry: 0,
        };
        let bytes = emit_program_spirv(&program, "hardware_program_main").expect("valid SPIR-V");
        assert_eq!(&bytes[..4], &[0x03, 0x02, 0x23, 0x07]);
        assert!(bytes.len() > 20);
    }

    #[test]
    fn lowers_push_constant_uniform() {
        let program = Program {
            version: 1,
            functions: vec![UcfFunction {
                name: "main".into(),
                params: Vec::new(),
                locals: Vec::new(),
                body: vec![Stmt::Return(Expr::LoadUniform(0))],
                ret: Ty::f32(),
            }],
            entry: 0,
        };
        let bytes = emit_program_spirv(&program, "ucf_uniform_main").expect("valid SPIR-V");
        assert_eq!(&bytes[..4], &[0x03, 0x02, 0x23, 0x07]);
    }
}
