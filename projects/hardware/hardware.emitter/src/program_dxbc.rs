//! Generic UCF program → DXBC `cs_5_1` bytecode.
//!
//! The module is assembled from SM5 tokens: no shader source is produced and no
//! graphics compiler is invoked. Uniform slots are read from `cb0`, which the
//! Direct3D 12 backend binds as 32-bit root constants; buffer resources become
//! raw SRVs (`t#`) and the entry result is written to the raw UAV `u0`.
//!
//! Supported subset: scalar `f32` / `u32` / `bool` values, `let` / assignment,
//! `if` / `else`, bounded `for` / `while`, the pure two-block CFG subset, and
//! the arithmetic, comparison and math intrinsics listed in [`Lowerer`]. Every
//! other construct returns a structured error instead of falling back to an
//! external compiler.

use std::collections::{BTreeMap, BTreeSet};

use hardware_program::{BinOp, CfgBlock, CfgInstruction, CfgTerminator, Expr, Function as UcfFunction, Program, ScalarTy, Stmt, Ty, UnOp, validate_program};

use crate::dxbc::{
    BoundBuffer, BufferRegister, Chunk, ConstantBuffer, Opcode, Operand, Reflection, Register, Select, ShaderWriter, Stage, TEST_NONZERO, Variable,
    container, reflection_chunk, signature_chunk,
};

/// Errors returned when a generic UCF program is outside the DXBC subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramDxbcError {
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

impl std::fmt::Display for ProgramDxbcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingEntry => f.write_str("generic UCF program has no entry function"),
            Self::UnsupportedType(v) => write!(f, "unsupported UCF DXBC type: {v}"),
            Self::UnsupportedExpression(v) => write!(f, "unsupported UCF DXBC expression: {v}"),
            Self::UnsupportedStatement(v) => write!(f, "unsupported UCF DXBC statement: {v}"),
            Self::Backend(v) => write!(f, "generic UCF DXBC backend error: {v}"),
        }
    }
}

impl std::error::Error for ProgramDxbcError {}

/// Threads per workgroup of every emitted module.
pub const WORKGROUP_SIZE: u32 = 64;

/// Buffer resources a generic UCF program touches.
///
/// Reads are bound as raw SRVs `t0..`, writes as raw UAVs `u1..`; `u0` always
/// carries the entry result. Both lists are sorted by resource slot.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProgramBindings {
    /// Resources read through `ld_raw`.
    pub reads: Vec<u32>,
    /// Resources written through `store_raw`.
    pub writes: Vec<u32>,
}

/// Collects the buffer resources a generic UCF program reads and writes.
pub fn program_bindings(program: &Program) -> ProgramBindings {
    let mut reads = BTreeSet::new();
    let mut writes = BTreeSet::new();
    for function in &program.functions {
        for stmt in &function.body {
            collect_resources(stmt, &mut reads, &mut writes);
        }
    }
    ProgramBindings { reads: reads.into_iter().collect(), writes: writes.into_iter().collect() }
}

/// Number of uniform slots the program reads (`slot + 1` of the highest slot).
pub fn program_uniform_count(program: &Program) -> u32 {
    program
        .functions
        .iter()
        .flat_map(|function| function.body.iter())
        .flat_map(uniform_slots_stmt)
        .max()
        .map(|slot| slot + 1)
        .unwrap_or(0)
}

/// Emit DXBC `cs_5_1` bytecode for a generic UCF program.
///
/// `uniform_count` is derived from the program: slot `n` is read from 32-bit
/// root constant `n + 1`, and root constant `0` carries the dispatch count.
pub fn emit_program_dxbc(program: &Program, entry: &str) -> Result<Vec<u8>, ProgramDxbcError> {
    validate_program(program).map_err(|error| ProgramDxbcError::UnsupportedStatement(format!("invalid UCF program: {error}")))?;
    let function = program.entry_fn().ok_or(ProgramDxbcError::MissingEntry)?;
    if function.ret != Ty::f32() {
        return Err(ProgramDxbcError::UnsupportedType(format!("return type {:?}", function.ret)));
    }
    let bindings = program_bindings(program);
    let uniform_count = program_uniform_count(program);
    let reads = bindings.reads.iter().enumerate().map(|(rank, resource)| (*resource, rank as u32)).collect::<BTreeMap<_, _>>();
    let writes = bindings.writes.iter().enumerate().map(|(rank, resource)| (*resource, rank as u32 + 1)).collect::<BTreeMap<_, _>>();

    let mut lower = Lowerer {
        uniforms: uniform_count,
        reads,
        writes,
        instructions: Vec::new(),
        temps: 0,
        locals: BTreeMap::new(),
        loop_depth: 0,
        synthetic: 0,
    };
    let condition = lower.active_check();
    lower.emit(Opcode::IfNz, TEST_NONZERO, vec![Operand::temp(condition, Select::Scalar(0))]);
    lower.emit(Opcode::Ret, 0, Vec::new());
    lower.emit(Opcode::EndIf, 0, Vec::new());

    let result = lower.lower_statements(function)?;
    let address = lower.temp();
    lower.emit(Opcode::IShl, 0, vec![Operand::temp(address, Select::Mask(1)), Operand::thread_id(Select::Scalar(0)), Operand::immediate(&[2])]);
    lower.emit(
        Opcode::StoreRaw,
        0,
        vec![
            Operand::bound(Register::Uav, 0, Select::Mask(1)),
            Operand::temp(address, Select::Scalar(0)),
            Operand::temp(result, Select::Scalar(0)),
        ],
    );
    lower.emit(Opcode::Ret, 0, Vec::new());

    let Lowerer { instructions, temps, .. } = lower;
    let mut shader = ShaderWriter::new(Stage::Compute, 1);
    shader.refactoring_allowed();
    shader.dcl_constant_buffer(0, (uniform_count + 1).div_ceil(4));
    shader.dcl_uav_raw(0);
    for register in 1..=bindings.writes.len() as u32 {
        shader.dcl_uav_raw(register);
    }
    for rank in 0..bindings.reads.len() as u32 {
        shader.dcl_resource_raw(rank);
    }
    shader.dcl_thread_id();
    shader.dcl_temps(temps.max(1));
    shader.dcl_thread_group(WORKGROUP_SIZE, 1, 1);
    for (opcode, control, operands) in &instructions {
        shader.op(*opcode, *control, operands);
    }

    let mut buffers = vec![BoundBuffer { name: "OutBuf".into(), register: BufferRegister::Uav, slot: 0 }];
    buffers.extend(bindings.reads.iter().enumerate().map(|(slot, _)| BoundBuffer {
        name: format!("InBuf{slot}"),
        register: BufferRegister::Srv,
        slot: slot as u32,
    }));
    buffers.extend(bindings.writes.iter().enumerate().map(|(slot, _)| BoundBuffer {
        name: format!("AuxBuf{slot}"),
        register: BufferRegister::Uav,
        slot: slot as u32 + 1,
    }));
    let reflection = Reflection {
        stage: Stage::Compute,
        minor: 1,
        creator: format!("hardware-emitter cs_5_1 {entry}"),
        constant_buffers: vec![ConstantBuffer {
            name: "UcfParams".into(),
            size: (uniform_count + 1).div_ceil(4) * 16,
            variables: (0..=uniform_count)
                .map(|index| Variable { name: if index == 0 { "count".into() } else { format!("uniform_{}", index - 1) }, offset: index * 4, used: true })
                .collect(),
        }],
        buffers,
    };
    Ok(container(&[
        Chunk::new(*b"RDEF", reflection_chunk(&reflection)),
        Chunk::new(*b"ISGN", signature_chunk(&[])),
        Chunk::new(*b"OSGN", signature_chunk(&[])),
        Chunk::new(*b"SHEX", shader.finish()),
    ]))
}

/// Value classes the encoder distinguishes when choosing an opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Float,
    U32,
    Bool,
    /// Three-component float vector held in the `xyz` lanes of one temporary.
    Vec3,
}

/// One buffered SM5 instruction.
type Instruction = (Opcode, u32, Vec<Operand>);

/// Ordering comparison requested by a [`BinOp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compare {
    Lt,
    Le,
    Gt,
    Ge,
}

struct Lowerer {
    uniforms: u32,
    /// Resource slot → raw SRV register.
    reads: BTreeMap<u32, u32>,
    /// Resource slot → raw UAV register (`u0` is reserved for the entry result).
    writes: BTreeMap<u32, u32>,
    instructions: Vec<Instruction>,
    temps: u32,
    locals: BTreeMap<u32, (u32, Kind)>,
    /// Number of enclosing `loop` blocks, used to reject stray `break` / `continue`.
    loop_depth: usize,
    /// Counter behind [`Lowerer::synthetic_local`].
    synthetic: u32,
}

impl Lowerer {
    fn emit(&mut self, opcode: Opcode, control: u32, operands: Vec<Operand>) {
        self.instructions.push((opcode, control, operands));
    }

    fn temp(&mut self) -> u32 {
        let slot = self.temps;
        self.temps += 1;
        slot
    }

    /// `if (vThreadID.x >= count) return;`
    fn active_check(&mut self) -> u32 {
        let active = self.temp();
        self.emit(
            Opcode::Uge,
            0,
            vec![Operand::temp(active, Select::Mask(1)), Operand::thread_id(Select::Scalar(0)), Operand::constant_buffer(0, 0, Select::Scalar(0))],
        );
        active
    }

    fn mov(&mut self, source: Operand) -> u32 {
        let slot = self.temp();
        self.emit(Opcode::Mov, 0, vec![Operand::temp(slot, Select::Mask(1)), source]);
        slot
    }

    fn literal(&mut self, bits: u32) -> u32 {
        self.mov(Operand::immediate(&[bits]))
    }

    fn uniform(&mut self, slot: u32) -> u32 {
        if slot >= self.uniforms {
            return 0;
        }
        let index = slot + 1;
        self.mov(Operand::constant_buffer(0, index / 4, Select::Scalar((index % 4) as u8)))
    }

    /// Reads a value. Vectors are read as `xyzw` swizzles so both element-wise
    /// and `dp3` operations see all lanes.
    fn scalar(&self, kind: Kind, slot: u32) -> Operand {
        match kind {
            Kind::Vec3 => Operand::temp(slot, Select::ALL),
            Kind::Float | Kind::U32 | Kind::Bool => Operand::temp(slot, Select::Scalar(0)),
        }
    }

    /// Destination operand matching the value class: `xyz` for vectors.
    fn dest_of(slot: u32, kind: Kind) -> Operand {
        match kind {
            Kind::Vec3 => Operand::temp(slot, Select::Mask(0b0111)),
            Kind::Float | Kind::U32 | Kind::Bool => Operand::temp(slot, Select::Mask(1)),
        }
    }

    /// Scalar destination operand.
    fn dest(slot: u32) -> Operand {
        Operand::temp(slot, Select::Mask(1))
    }

    fn local(&mut self, id: u32) -> Option<u32> {
        self.locals.get(&id).map(|(slot, _)| *slot)
    }

    fn lower_statements(&mut self, function: &UcfFunction) -> Result<u32, ProgramDxbcError> {
        self.lower_sequence(&function.body)?.ok_or_else(|| ProgramDxbcError::UnsupportedStatement("entry has no return".into()))
    }

    fn lower_sequence(&mut self, statements: &[Stmt]) -> Result<Option<u32>, ProgramDxbcError> {
        let mut result = None;
        for stmt in statements {
            match stmt {
                Stmt::Cfg { entry, blocks } => {
                    if result.is_some() || statements.len() != 1 {
                        return Err(ProgramDxbcError::UnsupportedStatement("CFG must be the complete function body".into()));
                    }
                    result = Some(self.lower_pure_cfg(*entry, blocks)?);
                }
                Stmt::Let { id, ty, init } => {
                    let kind = self.kind_for(ty)?;
                    let value = self.lower_expr(init)?;
                    let slot = self.temp();
                    self.emit(Opcode::Mov, 0, vec![Self::dest_of(slot, kind), self.scalar(value.1, value.0)]);
                    self.locals.insert(*id, (slot, kind));
                }
                Stmt::StoreLocal { id, value } => {
                    let (slot, local_kind) =
                        *self.locals.get(id).ok_or_else(|| ProgramDxbcError::UnsupportedStatement(format!("unknown local {id}")))?;
                    let value = self.lower_expr(value)?;
                    self.emit(Opcode::Mov, 0, vec![Self::dest_of(slot, local_kind), self.scalar(value.1, value.0)]);
                }
                Stmt::StoreBuffer { resource, index, value } => {
                    let index = self.lower_expr(index)?;
                    let value = self.lower_expr(value)?;
                    self.store_buffer(*resource, index, value)?;
                }
                Stmt::If { cond, then_body, else_body } => {
                    let value = self.lower_expr(cond)?;
                    self.emit(Opcode::IfNz, TEST_NONZERO, vec![self.scalar(value.1, value.0)]);
                    self.lower_sequence(then_body)?;
                    if !else_body.is_empty() {
                        self.emit(Opcode::Else, 0, Vec::new());
                        self.lower_sequence(else_body)?;
                    }
                    self.emit(Opcode::EndIf, 0, Vec::new());
                }
                Stmt::For { iter, limit, body } => self.lower_for(*iter, *limit, body)?,
                Stmt::While { cond, limit, body } => self.lower_while(cond, *limit, body)?,
                Stmt::Break | Stmt::Continue => {
                    if matches!(stmt, Stmt::Continue) {
                        return Err(ProgramDxbcError::UnsupportedStatement(
                            "`continue` is outside the DXBC structured subset".into(),
                        ));
                    }
                    if self.loop_depth == 0 {
                        return Err(ProgramDxbcError::UnsupportedStatement("`break` outside a loop".into()));
                    }
                    self.emit(Opcode::Break, 0, Vec::new());
                }
                Stmt::Return(value) => result = Some(self.lower_expr(value)?.0),
                Stmt::Expr(expr) => {
                    let _ = self.lower_expr(expr)?;
                }
            }
        }
        Ok(result)
    }

    fn kind_for(&self, ty: &Ty) -> Result<Kind, ProgramDxbcError> {
        match ty {
            Ty::Scalar(ScalarTy::F32) => Ok(Kind::Float),
            Ty::Scalar(ScalarTy::U32) => Ok(Kind::U32),
            Ty::Scalar(ScalarTy::Bool) => Ok(Kind::Bool),
            Ty::Vec3 => Ok(Kind::Vec3),
            other => Err(ProgramDxbcError::UnsupportedType(format!("local {other:?}"))),
        }
    }

    fn lower_for(&mut self, iter: u32, limit: u32, body: &[Stmt]) -> Result<(), ProgramDxbcError> {
        let counter = self.temp();
        self.emit(Opcode::Mov, 0, vec![Self::dest(counter), Operand::immediate(&[0])]);
        self.emit(Opcode::Loop, 0, Vec::new());
        self.loop_depth += 1;
        let done = self.temp();
        self.emit(
            Opcode::Uge,
            0,
            vec![Operand::temp(done, Select::Mask(1)), Operand::temp(counter, Select::Scalar(0)), Operand::immediate(&[limit])],
        );
        self.emit(Opcode::BreakCNz, TEST_NONZERO, vec![Operand::temp(done, Select::Scalar(0))]);

        let previous = self.locals.insert(iter, (counter, Kind::U32));
        let outcome = self.lower_loop_body(body);
        match previous {
            Some(previous) => self.locals.insert(iter, previous),
            None => self.locals.remove(&iter),
        };
        self.loop_depth -= 1;
        outcome?;

        self.emit(
            Opcode::IAdd,
            0,
            vec![Self::dest(counter), Operand::temp(counter, Select::Scalar(0)), Operand::immediate(&[1])],
        );
        self.emit(Opcode::EndLoop, 0, Vec::new());
        Ok(())
    }

    /// Lowers a loop body, rewriting `continue` into a skip flag first.
    ///
    /// SM5 `continue` jumps to the loop's continue point and re-enters the
    /// loop, so lowering it directly would skip the counter increment. The body
    /// is therefore rewritten so that everything after a `continue` runs inside
    /// `if (skip == false)`, which keeps the structured shape.
    fn lower_loop_body(&mut self, body: &[Stmt]) -> Result<(), ProgramDxbcError> {
        if !loop_flow(body).1 {
            return self.lower_sequence(body).map(|_| ());
        }
        let flag = self.synthetic_local();
        let (rewritten, _) = rewrite_continues(body, flag);
        let mut guarded = Vec::with_capacity(rewritten.len() + 1);
        guarded.push(Stmt::Let { id: flag, ty: Ty::Scalar(ScalarTy::Bool), init: Expr::ConstBool(false) });
        guarded.extend(rewritten);
        self.lower_sequence(&guarded).map(|_| ())
    }

    /// Allocates a local id that cannot collide with program-authored locals.
    fn synthetic_local(&mut self) -> u32 {
        self.synthetic += 1;
        u32::MAX - self.synthetic
    }

    fn lower_while(&mut self, cond: &Expr, limit: u32, body: &[Stmt]) -> Result<(), ProgramDxbcError> {
        let counter = self.temp();
        self.emit(Opcode::Mov, 0, vec![Self::dest(counter), Operand::immediate(&[0])]);
        self.emit(Opcode::Loop, 0, Vec::new());
        self.loop_depth += 1;
        let done = self.temp();
        self.emit(
            Opcode::Uge,
            0,
            vec![Operand::temp(done, Select::Mask(1)), Operand::temp(counter, Select::Scalar(0)), Operand::immediate(&[limit])],
        );
        let stop = self.temp();
        self.emit(Opcode::Mov, 0, vec![Self::dest(stop), Operand::temp(done, Select::Scalar(0))]);
        let value = self.lower_expr(cond)?;
        let negated = self.temp();
        self.emit(Opcode::Not, 0, vec![Self::dest(negated), self.scalar(value.1, value.0)]);
        self.emit(
            Opcode::Or,
            0,
            vec![Self::dest(stop), Operand::temp(stop, Select::Scalar(0)), Operand::temp(negated, Select::Scalar(0))],
        );
        self.emit(Opcode::BreakCNz, TEST_NONZERO, vec![Operand::temp(stop, Select::Scalar(0))]);

        let outcome = self.lower_loop_body(body);
        self.loop_depth -= 1;
        outcome?;

        self.emit(
            Opcode::IAdd,
            0,
            vec![Self::dest(counter), Operand::temp(counter, Select::Scalar(0)), Operand::immediate(&[1])],
        );
        self.emit(Opcode::EndLoop, 0, Vec::new());
        Ok(())
    }

    fn lower_pure_cfg(&mut self, entry: u32, blocks: &[CfgBlock]) -> Result<u32, ProgramDxbcError> {
        let entry_block = blocks
            .iter()
            .find(|block| block.id == entry)
            .ok_or_else(|| ProgramDxbcError::UnsupportedStatement("CFG entry block is missing".into()))?;
        if blocks.len() == 1 {
            let CfgTerminator::Return(value) = entry_block.terminator
            else {
                return Err(ProgramDxbcError::UnsupportedStatement("single-block CFG must return".into()));
            };
            let mut values = BTreeMap::new();
            for instruction in &entry_block.instructions {
                self.lower_cfg_instruction(instruction, &mut values)?;
            }
            return values
                .get(&value)
                .map(|(slot, _)| *slot)
                .ok_or_else(|| ProgramDxbcError::UnsupportedStatement(format!("CFG return value {value} is not defined")));
        }
        let CfgTerminator::ConditionalBranch { condition, then_block, then_args, else_block, else_args } = &entry_block.terminator
        else {
            return Err(ProgramDxbcError::UnsupportedStatement("DXBC CFG subset requires one conditional entry branch".into()));
        };
        let mut values = BTreeMap::new();
        for instruction in &entry_block.instructions {
            self.lower_cfg_instruction(instruction, &mut values)?;
        }
        let (condition, condition_kind) = *values
            .get(condition)
            .ok_or_else(|| ProgramDxbcError::UnsupportedStatement("CFG condition is not defined in the entry block".into()))?;
        let (accept, _) = self.lower_cfg_return_block(*then_block, then_args, blocks, &mut values)?;
        let (reject, _) = self.lower_cfg_return_block(*else_block, else_args, blocks, &mut values)?;
        let target = self.temp();
        self.emit(
            Opcode::MovC,
            0,
            vec![Self::dest(target), self.scalar(condition_kind, condition), Operand::temp(accept, Select::Scalar(0)), Operand::temp(reject, Select::Scalar(0))],
        );
        Ok(target)
    }

    fn lower_cfg_return_block(
        &mut self,
        block_id: u32,
        args: &[u32],
        blocks: &[CfgBlock],
        values: &mut BTreeMap<u32, (u32, Kind)>,
    ) -> Result<(u32, Kind), ProgramDxbcError> {
        let block = blocks
            .iter()
            .find(|block| block.id == block_id)
            .ok_or_else(|| ProgramDxbcError::UnsupportedStatement(format!("CFG target block {block_id} is missing")))?;
        if block.parameters.len() != args.len() {
            return Err(ProgramDxbcError::UnsupportedStatement("CFG branch argument count does not match target parameters".into()));
        }
        for (parameter, argument) in block.parameters.iter().zip(args) {
            let value = *values
                .get(argument)
                .ok_or_else(|| ProgramDxbcError::UnsupportedExpression(format!("unknown CFG branch argument {argument}")))?;
            values.insert(parameter.value, value);
        }
        for instruction in &block.instructions {
            self.lower_cfg_instruction(instruction, values)?;
        }
        let CfgTerminator::Return(value) = block.terminator
        else {
            return Err(ProgramDxbcError::UnsupportedStatement("DXBC CFG subset requires branch blocks to return directly".into()));
        };
        values
            .get(&value)
            .copied()
            .ok_or_else(|| ProgramDxbcError::UnsupportedStatement(format!("CFG return value {value} is not defined")))
    }

    fn lower_cfg_instruction(
        &mut self,
        instruction: &CfgInstruction,
        values: &mut BTreeMap<u32, (u32, Kind)>,
    ) -> Result<(), ProgramDxbcError> {
        if let CfgInstruction::StoreBuffer { resource, index, value } = instruction {
            let index = *values.get(index).ok_or_else(|| ProgramDxbcError::UnsupportedExpression(format!("unknown CFG value {index}")))?;
            let value = *values.get(value).ok_or_else(|| ProgramDxbcError::UnsupportedExpression(format!("unknown CFG value {value}")))?;
            self.store_buffer(*resource, index, value)?;
            return Ok(());
        }
        let value = match instruction {
            CfgInstruction::ConstF32 { value, .. } => (self.literal(value.to_bits()), Kind::Float),
            CfgInstruction::ConstF64 { .. } => return Err(ProgramDxbcError::UnsupportedType("F64 constant".into())),
            CfgInstruction::ConstU32 { value, .. } => (self.literal(*value), Kind::U32),
            CfgInstruction::LoadUniform { slot, ty, .. } => {
                if *ty != Ty::f32() {
                    return Err(ProgramDxbcError::UnsupportedType(format!("uniform {slot}: {ty:?}")));
                }
                (self.uniform(*slot), Kind::Float)
            }
            CfgInstruction::GlobalId { .. } => (self.mov(Operand::thread_id(Select::Scalar(0))), Kind::U32),
            CfgInstruction::LoadBuffer { resource, index, ty, .. } => {
                if *ty != Ty::f32() {
                    return Err(ProgramDxbcError::UnsupportedType(format!("buffer {resource}: {ty:?}")));
                }
                let index = *values.get(index).ok_or_else(|| ProgramDxbcError::UnsupportedExpression(format!("unknown CFG value {index}")))?;
                (self.load_buffer(*resource, index)?, Kind::Float)
            }
            CfgInstruction::Binary { op, lhs, rhs, .. } => {
                let left = *values.get(lhs).ok_or_else(|| ProgramDxbcError::UnsupportedExpression(format!("unknown CFG value {lhs}")))?;
                let right = *values.get(rhs).ok_or_else(|| ProgramDxbcError::UnsupportedExpression(format!("unknown CFG value {rhs}")))?;
                self.lower_binary(*op, left, right)?
            }
            CfgInstruction::Select { cond, then_, else_, .. } => {
                let condition =
                    *values.get(cond).ok_or_else(|| ProgramDxbcError::UnsupportedExpression(format!("unknown CFG value {cond}")))?;
                let accept = *values.get(then_).ok_or_else(|| ProgramDxbcError::UnsupportedExpression(format!("unknown CFG value {then_}")))?;
                let reject = *values.get(else_).ok_or_else(|| ProgramDxbcError::UnsupportedExpression(format!("unknown CFG value {else_}")))?;
                let target = self.temp();
                self.emit(
                    Opcode::MovC,
                    0,
                    vec![
                        Self::dest(target),
                        self.scalar(condition.1, condition.0),
                        Operand::temp(accept.0, Select::Scalar(0)),
                        Operand::temp(reject.0, Select::Scalar(0)),
                    ],
                );
                (target, accept.1)
            }
            CfgInstruction::Unary { op, arg, .. } => {
                let value = *values.get(arg).ok_or_else(|| ProgramDxbcError::UnsupportedExpression(format!("unknown CFG value {arg}")))?;
                self.lower_unary(*op, value)?
            }
            CfgInstruction::Vec3Compose { x, y, z, .. } => {
                let lanes = [x, y, z].map(|lane| {
                    values.get(lane).copied().ok_or_else(|| ProgramDxbcError::UnsupportedExpression(format!("unknown CFG value {lane}")))
                });
                let [x, y, z] = lanes;
                let lanes = [x?, y?, z?];
                let target = self.temp();
                for (component, value) in lanes.into_iter().enumerate() {
                    self.emit(
                        Opcode::Mov,
                        0,
                        vec![Operand::temp(target, Select::Mask(1 << (component as u8))), self.scalar(value.1, value.0)],
                    );
                }
                (target, Kind::Vec3)
            }
            _ => return Err(ProgramDxbcError::UnsupportedStatement("CFG instruction is outside the DXBC subset".into())),
        };
        let result = match instruction {
            CfgInstruction::ConstF32 { result, .. }
            | CfgInstruction::ConstU32 { result, .. }
            | CfgInstruction::LoadUniform { result, .. }
            | CfgInstruction::LoadBuffer { result, .. }
            | CfgInstruction::GlobalId { result }
            | CfgInstruction::Binary { result, .. }
            | CfgInstruction::Unary { result, .. }
            | CfgInstruction::Vec3Compose { result, .. }
            | CfgInstruction::Select { result, .. } => *result,
            CfgInstruction::ConstF64 { .. } | CfgInstruction::StoreBuffer { .. } => unreachable!(),
        };
        values.insert(result, value);
        Ok(())
    }

    fn lower_expr(&mut self, expr: &Expr) -> Result<(u32, Kind), ProgramDxbcError> {
        match expr {
            Expr::ConstF32(value) => Ok((self.literal(value.to_bits()), Kind::Float)),
            Expr::ConstU32(value) => Ok((self.literal(*value), Kind::U32)),
            Expr::ConstBool(value) => Ok((self.literal(if *value { u32::MAX } else { 0 }), Kind::Bool)),
            Expr::ConstF64(_) => Err(ProgramDxbcError::UnsupportedType("F64 constant".into())),
            Expr::GlobalId => Ok((self.mov(Operand::thread_id(Select::Scalar(0))), Kind::U32)),
            Expr::LoadUniform(slot) => Ok((self.uniform(*slot), Kind::Float)),
            Expr::LoadLocal(id) => {
                let (slot, kind) = *self
                    .locals
                    .get(id)
                    .ok_or_else(|| ProgramDxbcError::UnsupportedExpression(format!("unknown local {id}")))?;
                let copy = self.mov(Operand::temp(slot, Select::Scalar(0)));
                Ok((copy, kind))
            }
            Expr::LoadBuffer { resource, index } => {
                let index = self.lower_expr(index)?;
                let slot = self.load_buffer(*resource, index)?;
                Ok((slot, Kind::Float))
            }
            Expr::BinOp { op, lhs, rhs } => {
                let left = self.lower_expr(lhs)?;
                let right = self.lower_expr(rhs)?;
                self.lower_binary(*op, left, right)
            }
            Expr::UnOp { op, arg } => {
                let value = self.lower_expr(arg)?;
                self.lower_unary(*op, value)
            }
            Expr::Select { cond, then_, else_ } => {
                let condition = self.lower_expr(cond)?;
                let accept = self.lower_expr(then_)?;
                let reject = self.lower_expr(else_)?;
                let target = self.temp();
                self.emit(
                    Opcode::MovC,
                    0,
                    vec![
                        Self::dest(target),
                        self.scalar(condition.1, condition.0),
                        Operand::temp(accept.0, Select::Scalar(0)),
                        Operand::temp(reject.0, Select::Scalar(0)),
                    ],
                );
                Ok((target, accept.1))
            }
            Expr::Vec3Compose { x, y, z } => {
                let lanes = [self.lower_expr(x)?, self.lower_expr(y)?, self.lower_expr(z)?];
                let target = self.temp();
                for (component, value) in lanes.into_iter().enumerate() {
                    self.emit(
                        Opcode::Mov,
                        0,
                        vec![Operand::temp(target, Select::Mask(1 << (component as u8))), self.scalar(value.1, value.0)],
                    );
                }
                Ok((target, Kind::Vec3))
            }
            Expr::Call { .. } => Err(ProgramDxbcError::UnsupportedExpression("function call".into())),
        }
    }

    fn load_buffer(&mut self, resource: u32, index: (u32, Kind)) -> Result<u32, ProgramDxbcError> {
        if index.1 == Kind::Float {
            return Err(ProgramDxbcError::UnsupportedExpression("buffer index must be an integer".into()));
        }
        let rank = *self
            .reads
            .get(&resource)
            .ok_or_else(|| ProgramDxbcError::UnsupportedExpression(format!("unknown buffer resource {resource}")))?;
        let address = self.temp();
        self.emit(Opcode::IShl, 0, vec![Self::dest(address), self.scalar(index.1, index.0), Operand::immediate(&[2])]);
        let slot = self.temp();
        self.emit(
            Opcode::LdRaw,
            0,
            vec![Operand::temp(slot, Select::Mask(1)), Operand::temp(address, Select::Scalar(0)), Operand::bound(Register::Resource, rank, Select::UNUSED)],
        );
        Ok(slot)
    }

    fn store_buffer(&mut self, resource: u32, index: (u32, Kind), value: (u32, Kind)) -> Result<(), ProgramDxbcError> {
        if index.1 == Kind::Float {
            return Err(ProgramDxbcError::UnsupportedExpression("buffer index must be an integer".into()));
        }
        let rank = *self
            .writes
            .get(&resource)
            .ok_or_else(|| ProgramDxbcError::UnsupportedStatement(format!("unknown writable buffer resource {resource}")))?;
        let address = self.temp();
        self.emit(Opcode::IShl, 0, vec![Self::dest(address), self.scalar(index.1, index.0), Operand::immediate(&[2])]);
        self.emit(
            Opcode::StoreRaw,
            0,
            vec![
                Operand::bound(Register::Uav, rank, Select::Mask(1)),
                Operand::temp(address, Select::Scalar(0)),
                self.scalar(value.1, value.0),
            ],
        );
        Ok(())
    }

    fn lower_binary(&mut self, op: BinOp, lhs: (u32, Kind), rhs: (u32, Kind)) -> Result<(u32, Kind), ProgramDxbcError> {
        if lhs.1 == Kind::Bool || rhs.1 == Kind::Bool {
            // Boolean registers hold `0` / `0xffffffff`, so equality compares them as integers.
            return match op {
                BinOp::Eq => self.compare_equality(true, lhs, rhs),
                BinOp::Ne => self.compare_equality(false, lhs, rhs),
                other => Err(ProgramDxbcError::UnsupportedExpression(format!("bool operand of {other:?}"))),
            };
        }
        if lhs.1 == Kind::Vec3 || rhs.1 == Kind::Vec3 {
            return self.lower_vector_binary(op, lhs, rhs);
        }
        let float = lhs.1 == Kind::Float;
        if float != (rhs.1 == Kind::Float) {
            return Err(ProgramDxbcError::UnsupportedExpression(format!("mixed-width operand of {op:?}")));
        }
        let left = self.scalar(lhs.1, lhs.0);
        let right = self.scalar(rhs.1, rhs.0);
        match op {
            BinOp::Add => {
                let target = self.temp();
                self.emit(if float { Opcode::Add } else { Opcode::IAdd }, 0, vec![Self::dest(target), left, right]);
                Ok((target, lhs.1))
            }
            BinOp::Sub => {
                let negated = self.negate(rhs)?;
                let target = self.temp();
                self.emit(if float { Opcode::Add } else { Opcode::IAdd }, 0, vec![Self::dest(target), left, Operand::temp(negated, Select::Scalar(0))]);
                Ok((target, lhs.1))
            }
            BinOp::Mul => {
                let target = self.temp();
                if float {
                    self.emit(Opcode::Mul, 0, vec![Self::dest(target), left, right]);
                }
                else {
                    self.emit(Opcode::IMul, 0, vec![Operand::null(), Self::dest(target), left, right]);
                }
                Ok((target, lhs.1))
            }
            BinOp::Div => {
                let target = self.temp();
                if float {
                    self.emit(Opcode::Div, 0, vec![Self::dest(target), left, right]);
                }
                else {
                    self.emit(Opcode::UDiv, 0, vec![Self::dest(target), Operand::null(), left, right]);
                }
                Ok((target, lhs.1))
            }
            BinOp::Mod if !float => {
                let target = self.temp();
                self.emit(Opcode::UDiv, 0, vec![Operand::null(), Self::dest(target), left, right]);
                Ok((target, lhs.1))
            }
            BinOp::Mod => {
                // `x - floor(x / y) * y`, the `frem` shape the SPIR-V path emits.
                let quotient = self.temp();
                self.emit(Opcode::Div, 0, vec![Self::dest(quotient), left.clone(), right.clone()]);
                let floor = self.temp();
                self.emit(Opcode::RoundNI, 0, vec![Self::dest(floor), Operand::temp(quotient, Select::Scalar(0))]);
                let negated = self.temp();
                self.emit(Opcode::Mul, 0, vec![Self::dest(negated), right, Operand::immediate(&[(-1.0f32).to_bits()])]);
                let target = self.temp();
                self.emit(
                    Opcode::Mad,
                    0,
                    vec![
                        Self::dest(target),
                        Operand::temp(floor, Select::Scalar(0)),
                        Operand::temp(negated, Select::Scalar(0)),
                        left,
                    ],
                );
                Ok((target, Kind::Float))
            }
            BinOp::Min | BinOp::Max => {
                let target = self.temp();
                let opcode = match (op, float) {
                    (BinOp::Min, true) => Opcode::Min,
                    (BinOp::Max, true) => Opcode::Max,
                    (BinOp::Min, false) => Opcode::UMin,
                    _ => Opcode::UMax,
                };
                self.emit(opcode, 0, vec![Self::dest(target), left, right]);
                Ok((target, lhs.1))
            }
            BinOp::Pow if float => {
                let logarithm = self.temp();
                self.emit(Opcode::Log, 0, vec![Self::dest(logarithm), left]);
                let scaled = self.temp();
                self.emit(Opcode::Mul, 0, vec![Self::dest(scaled), Operand::temp(logarithm, Select::Scalar(0)), right]);
                let target = self.temp();
                self.emit(Opcode::Exp, 0, vec![Self::dest(target), Operand::temp(scaled, Select::Scalar(0))]);
                Ok((target, Kind::Float))
            }
            BinOp::Pow => Err(ProgramDxbcError::UnsupportedExpression("integer `pow`".into())),
            BinOp::Dot => Err(ProgramDxbcError::UnsupportedExpression("vector dot product".into())),
            BinOp::Atan2 => Err(ProgramDxbcError::UnsupportedExpression("`atan2`".into())),
            BinOp::Lt => self.compare(Compare::Lt, lhs, rhs),
            BinOp::Le => self.compare(Compare::Le, lhs, rhs),
            BinOp::Gt => self.compare(Compare::Gt, lhs, rhs),
            BinOp::Ge => self.compare(Compare::Ge, lhs, rhs),
            BinOp::Eq => self.compare_equality(true, lhs, rhs),
            BinOp::Ne => self.compare_equality(false, lhs, rhs),
        }
    }

    /// Element-wise vector arithmetic plus the scalar-producing `dot`.
    ///
    /// A scalar operand is broadcast over the vector lanes, which is what
    /// `Select::Scalar` encodes as a replicated swizzle.
    fn lower_vector_binary(&mut self, op: BinOp, lhs: (u32, Kind), rhs: (u32, Kind)) -> Result<(u32, Kind), ProgramDxbcError> {
        let left = self.scalar(lhs.1, lhs.0);
        let right = self.scalar(rhs.1, rhs.0);
        match op {
            BinOp::Dot if lhs.1 == Kind::Vec3 && rhs.1 == Kind::Vec3 => {
                let target = self.temp();
                self.emit(Opcode::Dp3, 0, vec![Self::dest(target), left, right]);
                Ok((target, Kind::Float))
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Min | BinOp::Max => {
                let target = self.temp();
                let setup = match op {
                    BinOp::Add => (Opcode::Add, None),
                    BinOp::Mul => (Opcode::Mul, None),
                    BinOp::Div => (Opcode::Div, None),
                    BinOp::Min => (Opcode::Min, None),
                    BinOp::Max => (Opcode::Max, None),
                    _ => (Opcode::Add, Some(self.negate(rhs)?)),
                };
                let right = match setup.1 {
                    Some(negated) => Operand::temp(negated, Select::Scalar(0)),
                    None => right,
                };
                self.emit(setup.0, 0, vec![Self::dest_of(target, Kind::Vec3), left, right]);
                Ok((target, Kind::Vec3))
            }
            other => Err(ProgramDxbcError::UnsupportedExpression(format!("vector `{other:?}`"))),
        }
    }

    /// Orders two scalars, using the unsigned opcodes for `u32` values.
    ///
    /// SM5 only has `lt` / `ge` per signedness: `lt`/`ge` for floats and signed
    /// integers, `ult`/`uge` for unsigned integers.
    fn compare(&mut self, comparison: Compare, lhs: (u32, Kind), rhs: (u32, Kind)) -> Result<(u32, Kind), ProgramDxbcError> {
        let unsigned = lhs.1 == Kind::U32;
        let (ge, lt) = if unsigned { (Opcode::Uge, Opcode::Ult) } else { (Opcode::Ge, Opcode::Lt) };
        let (opcode, swap) = match comparison {
            Compare::Lt => (lt, false),
            Compare::Le => (ge, true),
            Compare::Gt => (lt, true),
            Compare::Ge => (ge, false),
        };
        let left = self.scalar(lhs.1, lhs.0);
        let right = self.scalar(rhs.1, rhs.0);
        let target = self.temp();
        let operands = if swap {
            vec![Self::dest(target), right, left]
        }
        else {
            vec![Self::dest(target), left, right]
        };
        self.emit(opcode, 0, operands);
        Ok((target, Kind::Bool))
    }

    /// Compares two scalars for equality, using the integer opcodes for `u32`.
    fn compare_equality(&mut self, equal: bool, lhs: (u32, Kind), rhs: (u32, Kind)) -> Result<(u32, Kind), ProgramDxbcError> {
        let opcode = match (matches!(lhs.1, Kind::U32 | Kind::Bool), equal) {
            (true, true) => Opcode::IEq,
            (true, false) => Opcode::INe,
            (false, true) => Opcode::Eq,
            (false, false) => Opcode::Ne,
        };
        let left = self.scalar(lhs.1, lhs.0);
        let right = self.scalar(rhs.1, rhs.0);
        let target = self.temp();
        self.emit(opcode, 0, vec![Self::dest(target), left, right]);
        Ok((target, Kind::Bool))
    }

    fn negate(&mut self, value: (u32, Kind)) -> Result<u32, ProgramDxbcError> {
        let target = self.temp();
        match value.1 {
            Kind::Float | Kind::Vec3 => {
                self.emit(
                    Opcode::Mul,
                    0,
                    vec![Self::dest_of(target, value.1), self.scalar(value.1, value.0), Operand::immediate(&[(-1.0f32).to_bits()])],
                );
                Ok(target)
            }
            Kind::U32 => {
                self.emit(Opcode::INeg, 0, vec![Self::dest(target), self.scalar(value.1, value.0)]);
                Ok(target)
            }
            Kind::Bool => Err(ProgramDxbcError::UnsupportedExpression("negation of a bool".into())),
        }
    }

    fn lower_unary(&mut self, op: UnOp, arg: (u32, Kind)) -> Result<(u32, Kind), ProgramDxbcError> {
        let source = self.scalar(arg.1, arg.0);
        match op {
            UnOp::Neg => Ok((self.negate(arg)?, arg.1)),
            UnOp::Vec3Comp(component) if arg.1 == Kind::Vec3 => {
                let target = self.temp();
                self.emit(Opcode::Mov, 0, vec![Self::dest(target), Operand::temp(arg.0, Select::Scalar(component))]);
                Ok((target, Kind::Float))
            }
            UnOp::Length if arg.1 == Kind::Vec3 => {
                let squared = self.temp();
                self.emit(Opcode::Dp3, 0, vec![Self::dest(squared), source, self.scalar(arg.1, arg.0)]);
                let target = self.temp();
                self.emit(Opcode::Sqrt, 0, vec![Self::dest(target), Operand::temp(squared, Select::Scalar(0))]);
                Ok((target, Kind::Float))
            }
            UnOp::Normalize if arg.1 == Kind::Vec3 => {
                let squared = self.temp();
                self.emit(Opcode::Dp3, 0, vec![Self::dest(squared), source.clone(), self.scalar(arg.1, arg.0)]);
                let inverse = self.temp();
                self.emit(Opcode::Rsq, 0, vec![Self::dest(inverse), Operand::temp(squared, Select::Scalar(0))]);
                let target = self.temp();
                self.emit(
                    Opcode::Mul,
                    0,
                    vec![Self::dest_of(target, Kind::Vec3), source, Operand::temp(inverse, Select::Scalar(0))],
                );
                Ok((target, Kind::Vec3))
            }
            UnOp::Sqrt => {
                let target = self.temp();
                self.emit(Opcode::Sqrt, 0, vec![Self::dest(target), source]);
                Ok((target, Kind::Float))
            }
            UnOp::Sin | UnOp::Cos => {
                let pair = self.temp();
                self.emit(Opcode::Sincos, 0, vec![Operand::temp(pair, Select::Mask(0b0011)), source]);
                let component = if matches!(op, UnOp::Sin) { 0 } else { 1 };
                let target = self.temp();
                self.emit(Opcode::Mov, 0, vec![Self::dest(target), Operand::temp(pair, Select::Scalar(component))]);
                Ok((target, Kind::Float))
            }
            UnOp::Atan => Err(ProgramDxbcError::UnsupportedExpression("`atan`".into())),
            UnOp::Length | UnOp::Normalize => Err(ProgramDxbcError::UnsupportedExpression("vector length / normalize".into())),
            UnOp::Abs => {
                let negated = self.negate(arg)?;
                let target = self.temp();
                self.emit(
                    Opcode::Max,
                    0,
                    vec![Self::dest_of(target, arg.1), source, Operand::temp(negated, Select::Scalar(0))],
                );
                Ok((target, arg.1))
            }
            UnOp::Ln => {
                let logarithm = self.temp();
                self.emit(Opcode::Log, 0, vec![Self::dest(logarithm), source]);
                let target = self.temp();
                self.emit(
                    Opcode::Mul,
                    0,
                    vec![Self::dest(target), Operand::temp(logarithm, Select::Scalar(0)), Operand::immediate(&[std::f32::consts::LN_2.to_bits()])],
                );
                Ok((target, Kind::Float))
            }
            UnOp::IsFinite => {
                let ordered = self.temp();
                self.emit(Opcode::Eq, 0, vec![Self::dest(ordered), source, self.scalar(arg.1, arg.0)]);
                let negated = self.negate(arg)?;
                let absolute = self.temp();
                self.emit(Opcode::Max, 0, vec![Self::dest(absolute), self.scalar(arg.1, arg.0), Operand::temp(negated, Select::Scalar(0))]);
                let bounded = self.temp();
                self.emit(
                    Opcode::Lt,
                    0,
                    vec![Self::dest(bounded), Operand::temp(absolute, Select::Scalar(0)), Operand::immediate(&[f32::INFINITY.to_bits()])],
                );
                let target = self.temp();
                self.emit(
                    Opcode::And,
                    0,
                    vec![Self::dest(target), Operand::temp(ordered, Select::Scalar(0)), Operand::temp(bounded, Select::Scalar(0))],
                );
                Ok((target, Kind::Bool))
            }
            UnOp::Vec3Comp(_) => Err(ProgramDxbcError::UnsupportedType("vector component access".into())),
        }
    }
}

/// Whether a loop body uses `break` / `continue` for its own loop.
///
/// Nested loops own their own `break` / `continue`, so they are not counted.
fn loop_flow(statements: &[Stmt]) -> (bool, bool) {
    let mut breaks = false;
    let mut continues = false;
    for statement in statements {
        match statement {
            Stmt::Break => breaks = true,
            Stmt::Continue => continues = true,
            Stmt::If { then_body, else_body, .. } => {
                let (nested_breaks, nested_continues) = loop_flow(then_body);
                let (else_breaks, else_continues) = loop_flow(else_body);
                breaks |= nested_breaks || else_breaks;
                continues |= nested_continues || else_continues;
            }
            Stmt::For { .. } | Stmt::While { .. } | Stmt::Cfg { .. } => {}
            Stmt::Let { .. } | Stmt::StoreLocal { .. } | Stmt::StoreBuffer { .. } | Stmt::Return(_) | Stmt::Expr(_) => {}
        }
    }
    (breaks, continues)
}

/// Rewrites `continue` into stores to `flag`, guarding everything that follows.
///
/// Returns the rewritten statements and whether any `continue` survived for this
/// loop. Nested loops own their own `continue`, so they are left untouched.
fn rewrite_continues(statements: &[Stmt], flag: u32) -> (Vec<Stmt>, bool) {
    let mut out = Vec::new();
    let mut index = 0;
    while index < statements.len() {
        match &statements[index] {
            Stmt::Continue => {
                out.push(Stmt::StoreLocal { id: flag, value: Expr::ConstBool(true) });
                // Everything after an unconditional `continue` is unreachable.
                return (out, true);
            }
            Stmt::If { cond, then_body, else_body } => {
                let (then_body, then_continues) = rewrite_continues(then_body, flag);
                let (else_body, else_continues) = rewrite_continues(else_body, flag);
                let continues = then_continues || else_continues;
                out.push(Stmt::If { cond: cond.clone(), then_body, else_body });
                if continues {
                    let rest = &statements[index + 1..];
                    if !rest.is_empty() {
                        let (guarded, rest_continues) = rewrite_continues(rest, flag);
                        // `flag == false` is the "keep going" guard.
                        let keep_going = Expr::BinOp {
                            op: BinOp::Eq,
                            lhs: Box::new(Expr::LoadLocal(flag)),
                            rhs: Box::new(Expr::ConstBool(false)),
                        };
                        out.push(Stmt::If { cond: keep_going, then_body: guarded, else_body: Vec::new() });
                        return (out, continues || rest_continues);
                    }
                    return (out, continues);
                }
            }
            other => out.push(other.clone()),
        }
        index += 1;
    }
    (out, false)
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
            Expr::LoadBuffer { index, .. } => visit_expr(index, slots),
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
        Stmt::Let { init, .. } | Stmt::StoreLocal { value: init, .. } | Stmt::Return(init) | Stmt::Expr(init) => visit_expr(init, &mut slots),
        Stmt::If { cond, then_body, else_body } => {
            visit_expr(cond, &mut slots);
            for nested in then_body.iter().chain(else_body) {
                slots.extend(uniform_slots_stmt(nested));
            }
        }
        Stmt::For { body, .. } => {
            for nested in body {
                slots.extend(uniform_slots_stmt(nested));
            }
        }
        Stmt::While { cond, body, .. } => {
            visit_expr(cond, &mut slots);
            for nested in body {
                slots.extend(uniform_slots_stmt(nested));
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

fn collect_resources(stmt: &Stmt, reads: &mut BTreeSet<u32>, writes: &mut BTreeSet<u32>) {
    fn visit_expr(value: &Expr, reads: &mut BTreeSet<u32>, writes: &mut BTreeSet<u32>) {
        match value {
            Expr::LoadBuffer { resource, index } => {
                reads.insert(*resource);
                visit_expr(index, reads, writes);
            }
            Expr::BinOp { lhs, rhs, .. } => {
                visit_expr(lhs, reads, writes);
                visit_expr(rhs, reads, writes);
            }
            Expr::UnOp { arg, .. } => visit_expr(arg, reads, writes),
            Expr::Select { cond, then_, else_ } => {
                visit_expr(cond, reads, writes);
                visit_expr(then_, reads, writes);
                visit_expr(else_, reads, writes);
            }
            Expr::Vec3Compose { x, y, z } => {
                visit_expr(x, reads, writes);
                visit_expr(y, reads, writes);
                visit_expr(z, reads, writes);
            }
            Expr::Call { args, .. } => {
                for arg in args {
                    visit_expr(arg, reads, writes);
                }
            }
            _ => {}
        }
    }

    match stmt {
        Stmt::Let { init, .. } | Stmt::StoreLocal { value: init, .. } | Stmt::Return(init) | Stmt::Expr(init) => visit_expr(init, reads, writes),
        Stmt::StoreBuffer { resource, index, value } => {
            writes.insert(*resource);
            visit_expr(index, reads, writes);
            visit_expr(value, reads, writes);
        }
        Stmt::If { cond, then_body, else_body } => {
            visit_expr(cond, reads, writes);
            for nested in then_body.iter().chain(else_body) {
                collect_resources(nested, reads, writes);
            }
        }
        Stmt::For { body, .. } | Stmt::While { body, .. } => {
            for nested in body {
                collect_resources(nested, reads, writes);
            }
        }
        Stmt::Break | Stmt::Continue => {}
        Stmt::Cfg { blocks, .. } => {
            for block in blocks {
                for instruction in &block.instructions {
                    match instruction {
                        CfgInstruction::LoadBuffer { resource, .. } => {
                            reads.insert(*resource);
                        }
                        CfgInstruction::StoreBuffer { resource, .. } => {
                            writes.insert(*resource);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hardware_program::{FunctionBuilder, ProgramBuilder, StructuredFunctionBuilder};

    /// Opcodes of the `SHEX` chunk, skipping its two-word version header.
    fn opcodes(bytes: &[u8]) -> Vec<u32> {
        let count = u32::from_le_bytes(bytes[28..32].try_into().unwrap()) as usize;
        for index in 0..count {
            let field = 32 + index * 4;
            let offset = u32::from_le_bytes(bytes[field..field + 4].try_into().unwrap()) as usize;
            if &bytes[offset..offset + 4] == b"SHEX" {
                let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
                let payload = &bytes[offset + 8..offset + 8 + size];
                let words: Vec<u32> = payload.chunks_exact(4).map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])).collect();
                let mut out = Vec::new();
                let mut word = 2;
                while word < words.len() {
                    let token = words[word];
                    out.push(token & 0x7ff);
                    word += ((token >> 24) & 0x7f) as usize;
                }
                return out;
            }
        }
        panic!("module has no SHEX chunk");
    }

    fn program(body: Vec<Stmt>, ret: Ty) -> Program {
        Program { version: 1, functions: vec![UcfFunction { name: "main".into(), params: Vec::new(), locals: Vec::new(), body, ret }], entry: 0 }
    }

    #[test]
    fn emits_uniform_and_buffer_scalar_program() {
        let body = vec![Stmt::Return(Expr::BinOp {
            op: BinOp::Mul,
            lhs: Box::new(Expr::LoadUniform(0)),
            rhs: Box::new(Expr::LoadBuffer { resource: 0, index: Box::new(Expr::GlobalId) }),
        })];
        let bytes = emit_program_dxbc(&program(body, Ty::f32()), "ucf_program_main").expect("DXBC emission");
        assert_eq!(&bytes[0..4], b"DXBC");
        let opcodes = opcodes(&bytes);
        assert!(opcodes.contains(&(Opcode::DclConstantBuffer as u32)));
        assert!(opcodes.contains(&(Opcode::LdRaw as u32)));
        assert!(opcodes.contains(&(Opcode::StoreRaw as u32)));
        assert!(opcodes.contains(&(Opcode::Mul as u32)));
    }

    #[test]
    fn emits_integer_comparisons_with_unsigned_opcodes() {
        let select = |op: BinOp| {
            program(
                vec![Stmt::Return(Expr::Select {
                    cond: Box::new(Expr::BinOp { op, lhs: Box::new(Expr::GlobalId), rhs: Box::new(Expr::ConstU32(2)) }),
                    then_: Box::new(Expr::ConstF32(1.0)),
                    else_: Box::new(Expr::ConstF32(0.0)),
                })],
                Ty::f32(),
            )
        };
        let ordering = opcodes(&emit_program_dxbc(&select(BinOp::Ge), "compare").expect("DXBC emission"));
        assert!(ordering.contains(&(Opcode::Uge as u32)), "{ordering:?}");
        assert!(!ordering.contains(&(Opcode::Ge as u32)), "{ordering:?}");

        let equality = opcodes(&emit_program_dxbc(&select(BinOp::Eq), "compare").expect("DXBC emission"));
        assert!(equality.contains(&(Opcode::IEq as u32)), "{equality:?}");
        assert!(!equality.contains(&(Opcode::Eq as u32)), "{equality:?}");
    }

    #[test]
    fn emits_bounded_loop_with_break() {
        let entry = program(
            vec![
                Stmt::Let { id: 0, ty: Ty::f32(), init: Expr::ConstF32(0.0) },
                Stmt::For {
                    iter: 1,
                    limit: 4,
                    body: vec![
                        Stmt::If {
                            cond: Expr::BinOp { op: BinOp::Eq, lhs: Box::new(Expr::LoadLocal(1)), rhs: Box::new(Expr::ConstU32(2)) },
                            then_body: vec![Stmt::Break],
                            else_body: Vec::new(),
                        },
                        Stmt::StoreLocal {
                            id: 0,
                            value: Expr::BinOp { op: BinOp::Add, lhs: Box::new(Expr::LoadLocal(0)), rhs: Box::new(Expr::ConstF32(1.0)) },
                        },
                    ],
                },
                Stmt::Return(Expr::LoadLocal(0)),
            ],
            Ty::f32(),
        );
        let bytes = emit_program_dxbc(&entry, "loop_break").expect("DXBC emission");
        let opcodes = opcodes(&bytes);
        assert!(opcodes.contains(&(Opcode::Loop as u32)));
        assert!(opcodes.contains(&(Opcode::Break as u32)));
        assert!(opcodes.contains(&(Opcode::EndLoop as u32)));
    }

    #[test]
    fn emits_structured_if_and_bounded_loop() {
        let mut builder = StructuredFunctionBuilder::new("branchy");
        let condition = builder.binary(BinOp::Gt, builder.global_id(), Expr::ConstU32(4));
        builder.bounded_for(3, |builder, index| {
            let value = builder.load_buffer(0, Expr::GlobalId);
            builder.store_local(index, value);
        });
        let value = builder.load_buffer(0, Expr::GlobalId);
        let result = builder.select(condition, value, Expr::ConstF32(1.0));
        let function = builder.finish(Ty::f32(), result);
        let entry = Program { version: 1, functions: vec![function], entry: 0 };
        let bytes = emit_program_dxbc(&entry, "branchy").expect("DXBC emission");
        let opcodes = opcodes(&bytes);
        assert!(opcodes.contains(&(Opcode::IfNz as u32)));
        assert!(opcodes.contains(&(Opcode::EndIf as u32)));
        assert!(opcodes.contains(&(Opcode::Loop as u32)));
        assert!(opcodes.contains(&(Opcode::EndLoop as u32)));
    }

    #[test]
    fn rejects_constructs_outside_the_scalar_subset() {
        let f64_program = program(vec![Stmt::Return(Expr::ConstF64(1.0))], Ty::f64());
        assert!(matches!(emit_program_dxbc(&f64_program, "f64"), Err(ProgramDxbcError::UnsupportedType(_))));

        let vector =
            program(vec![Stmt::Return(Expr::UnOp { op: UnOp::Length, arg: Box::new(Expr::LoadUniform(0)) })], Ty::f32());
        assert!(matches!(emit_program_dxbc(&vector, "vector"), Err(ProgramDxbcError::UnsupportedExpression(_))));
    }

    #[test]
    fn store_buffer_writes_through_the_auxiliary_uav() {
        let store = program(
            vec![
                Stmt::StoreBuffer { resource: 0, index: Expr::GlobalId, value: Expr::LoadUniform(0) },
                Stmt::Return(Expr::ConstF32(0.0)),
            ],
            Ty::f32(),
        );
        let bindings = program_bindings(&store);
        assert!(bindings.reads.is_empty());
        assert_eq!(bindings.writes, vec![0]);
        let bytes = emit_program_dxbc(&store, "store").expect("DXBC emission");
        assert_eq!(&bytes[0..4], b"DXBC");
        assert!(opcodes(&bytes).contains(&(Opcode::StoreRaw as u32)));
    }

    #[test]
    fn emits_generic_program_through_the_unified_entry() {
        let mut builder = FunctionBuilder::new("cfg_main");
        let index = builder.emit_global_id().unwrap();
        let value = builder.emit_load_buffer(0, index, Ty::f32()).unwrap();
        let scale = builder.emit_load_uniform(0, Ty::f32()).unwrap();
        let result = builder.emit_binary(BinOp::Add, value, scale).unwrap();
        builder.return_value(result).unwrap();
        let entry = ProgramBuilder::new().entry(builder).unwrap().finish().unwrap();
        let module = crate::encode_program(&entry, "cfg_main", crate::TargetIsa::Dxbc, crate::CodegenOptions::default()).expect("DXBC program");
        assert_eq!(&module.bytes[0..4], b"DXBC");
    }
}
