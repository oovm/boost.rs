//! TS × OVM 第一投影适配器：IR + 特化事实 → OVM LIR → `.odin` 字节。
//!
//! 残差表示：[`BytecodeEmitter`] 直接产 OVM LIR；残差值 = 局部槽
//! （`StoreLocal` / `LoadLocal`）。语义裁决全部发生在语言侧：静态可判定的
//! 在特化期结算进产物，动态不可判定的走 `odin.host` 模块下内核未收录的
//! `ts.*` provider 符号——provider 缺席即装载失败（失败闭合）。

use std::collections::{HashMap, HashSet};

use compiler_emitter::emitter_backend_vm::{sanitize_symbol, BytecodeEmitter, OvmProgramInput, assemble_ovm_module, encode_odin_function};
use compiler_specialize::{BranchResidual, SideEffectLedger, Stage};
use data_binary_odin::{
    OdinConstant, OdinExport, OdinExportKind, OdinFunction, OdinHeadCode, OdinImport, OdinLayout, OdinModuleData, Odin_VERSION, encode_module,
};
use language_typescript_ir::{statement::Statement, types::BinaryOp, types::PrimitiveType, Expression, Program, TypeAnnotation};

use crate::runtime::semantics;
use crate::runtime::Value;
use crate::specialize::{
    analyze, check_branch_residual, eval_known, inline_eval, is_pure, literal_to_value, narrow, FunctionInfo, ProgramFacts, SpecializeFailure,
    StaticType,
};

/// 语言 provider 必须注册的全部 `ts.*` 符号（内核未收录，缺席即装载失败）。
pub const TS_PROVIDER_SYMBOLS: &[&str] = &["ts.add", "ts.truthy", "ts.strictEqual", "ts.num_to_string", "ts.log"];

/// 第一投影产物。
#[derive(Debug, Clone)]
pub struct OvmArtifact {
    /// `.odin` 字节。
    pub bytes: Vec<u8>,
    /// 模块数据（取证入口）。
    pub module: OdinModuleData,
    /// 布局下标 → 冻结字段名（按名字排序）。宿主构造 guest 对象时使用。
    pub layout_fields: Vec<Vec<String>>,
}

/// 表达式降低结果：已知值（不产生代码）或栈顶残差。
#[derive(Debug, Clone)]
enum Residual {
    /// 特化期已结算。
    Known(Value, StaticType),
    /// 值已压在栈顶。
    OnStack(StaticType),
}

/// 单个函数降低期间的上下文。
struct FnCtx {
    /// 名字 → 局部槽（形参占 `0..arity`）。
    slots: HashMap<String, i32>,
    /// 下一个可用槽。
    next_slot: i32,
    /// 名字 → （已知值（`None` = 未知，必须残差），静态类型）。
    bindings: HashMap<String, (Option<Value>, StaticType)>,
    /// 循环标签栈：`(break 标签, continue 标签)`。
    loops: Vec<(u32, u32)>,
}

/// 程序 → `.odin` 的第一投影。
///
/// 已知事实（模块 `const`、全已知纯调用）在特化期结算；未知条件双分支残差、
/// 已知条件消去；副作用按源码序残差为 provider 调用。
pub fn specialize_to_odin(program: &Program, module_name: &str) -> Result<OvmArtifact, SpecializeFailure> {
    let facts = analyze(program)?;
    let mut lowerer = Lowerer {
        facts,
        imports: Vec::new(),
        constants: Vec::new(),
        code: Vec::new(),
        functions: Vec::new(),
        exports: Vec::new(),
        layouts: Vec::new(),
        layout_fields: Vec::new(),
        function_slot: HashMap::new(),
        effects: SideEffectLedger::new(),
        effect_order: 0,
    };
    // 预分配函数槽位（声明序）：`CallStatic` 只引用槽位，不在发射期嵌套降低。
    for (index, info) in lowerer.facts.functions.iter().enumerate() {
        lowerer.function_slot.insert(info.name.clone(), index as i32);
    }
    let all: Vec<FunctionInfo> = lowerer.facts.functions.clone();
    for info in &all {
        let slot = lowerer.function_slot[&info.name];
        lowerer.lower_function(info, slot)?;
        if info.exported {
            lowerer.exports.push(OdinExport { kind: OdinExportKind::Function, symbol_name: sanitize_symbol(&info.name), function_index: slot });
        }
    }
    let module = OdinModuleData {
        version: Odin_VERSION,
        name: module_name.to_string(),
        constants: lowerer.constants,
        functions: lowerer.functions,
        imports: lowerer.imports,
        exports: lowerer.exports,
        witness_entries: Vec::new(),
        code_bytes: lowerer.code,
        globals: Vec::new(),
        init_function_indices: Vec::new(),
        layouts: lowerer.layouts,
    };
    let (module, _abi) = assemble_ovm_module(&OvmProgramInput::new(module, None))
        .map_err(|error| SpecializeFailure::Unsupported { construct: "OVM 装配".to_string(), detail: error })?;
    Ok(OvmArtifact { bytes: encode_module(&module), module, layout_fields: lowerer.layout_fields })
}

struct Lowerer {
    facts: ProgramFacts,
    imports: Vec<OdinImport>,
    constants: Vec<OdinConstant>,
    code: Vec<u8>,
    functions: Vec<OdinFunction>,
    exports: Vec<OdinExport>,
    layouts: Vec<OdinLayout>,
    layout_fields: Vec<Vec<String>>,
    /// 函数名 → functions 表槽位（声明序预分配）。
    function_slot: HashMap<String, i32>,
    /// 副作用账本：残差效应必须按源码序登记。
    effects: SideEffectLedger,
    /// 副作用源码序计数器。
    effect_order: u64,
}

impl Lowerer {
    /// 当前语境下特化期已知的名字（模块常量 + 已知局部）。
    fn known_map(&self, ctx: &FnCtx) -> HashMap<String, (Value, StaticType)> {
        let mut map = self.facts.module_consts.clone();
        for (name, (value, ty)) in &ctx.bindings {
            if let Some(value) = value {
                map.insert(name.clone(), (value.clone(), ty.clone()));
            }
        }
        map
    }

    fn lower_function(&mut self, info: &FunctionInfo, slot: i32) -> Result<(), SpecializeFailure> {
        let constants_base = self.constants.len() as i32;
        let mut ctx = FnCtx { slots: HashMap::new(), next_slot: 0, bindings: HashMap::new(), loops: Vec::new() };
        for (index, param) in info.params.iter().enumerate() {
            ctx.slots.insert(param.clone(), index as i32);
            let ty = info.param_types.get(index).and_then(|ty| ty.clone()).map(|ty| narrow(&ty)).unwrap_or(StaticType::Unknown);
            // 形参全为未知（guest 输入）；声明的对象类型即布局合同，登记进 layouts。
            if let StaticType::Object(fields) = &ty {
                self.register_layout(fields);
            }
            ctx.bindings.insert(param.clone(), (None, ty));
        }
        ctx.next_slot = info.params.len() as i32;
        for (name, (value, ty)) in &self.facts.module_consts {
            ctx.bindings.insert(name.clone(), (Some(value.clone()), ty.clone()));
        }

        let body = info.body.clone();
        // `BytecodeEmitter` 借用导入表：降低期间把 imports 移出 `self`，结束后放回。
        let mut imports = std::mem::take(&mut self.imports);
        let outcome = {
            let mut emitter = BytecodeEmitter::new(constants_base, &mut imports);
            let result = self.lower_statements(&body, &mut ctx, &mut emitter);
            if result.is_ok() {
                // 统一出口：无显式 return 的函数补 `undefined`。
                emitter.emit_const_null();
                emitter.emit_plain(OdinHeadCode::Return);
            }
            result.and_then(|()| {
                encode_odin_function(&emitter.instructions)
                    .map(|code| (code, std::mem::take(&mut emitter.constants)))
                    .map_err(|error| SpecializeFailure::Unsupported { construct: "OVM LIR 编码".to_string(), detail: error })
            })
        };
        self.imports = imports;
        let (code, function_constants) = outcome?;
        debug_assert_eq!(self.functions.len(), slot as usize);
        let code_offset = self.code.len() as i32;
        self.code.extend_from_slice(&code);
        self.constants.extend(function_constants);
        self.functions.push(OdinFunction {
            name: sanitize_symbol(&info.name),
            arity: info.params.len() as i32,
            local_count: ctx.next_slot.max(info.params.len() as i32),
            code_offset,
            code_length: code.len() as i32,
        });
        Ok(())
    }

    fn lower_statements(&mut self, statements: &[Statement], ctx: &mut FnCtx, emitter: &mut BytecodeEmitter<'_>) -> Result<(), SpecializeFailure> {
        for statement in statements {
            self.lower_statement(statement, ctx, emitter)?;
        }
        Ok(())
    }

    fn lower_statement(&mut self, statement: &Statement, ctx: &mut FnCtx, emitter: &mut BytecodeEmitter<'_>) -> Result<(), SpecializeFailure> {
        match statement {
            Statement::Expression(expression) => {
                let residual = self.emit_expr(expression, ctx, emitter)?;
                // 表达式语句丢弃结果，保持栈平衡。
                self.materialize_if_known(residual, emitter)?;
                emitter.emit_plain(OdinHeadCode::Pop);
                Ok(())
            }
            Statement::VariableDeclaration { name, ty, initializer, .. } => {
                let slot = self.slot(name, ctx);
                let declared = ty.as_ref().map(narrow);
                match initializer {
                    Some(expression) => {
                        let residual = self.emit_expr(expression, ctx, emitter)?;
                        match residual {
                            Residual::Known(value, static_ty) => {
                                self.emit_known(&value, emitter)?;
                                emitter.emit_imm1(OdinHeadCode::StoreLocal, slot);
                                let ty = declared.unwrap_or(static_ty);
                                ctx.bindings.insert(name.clone(), (Some(value), ty));
                            }
                            Residual::OnStack(static_ty) => {
                                emitter.emit_imm1(OdinHeadCode::StoreLocal, slot);
                                let ty = declared.unwrap_or(static_ty);
                                ctx.bindings.insert(name.clone(), (None, ty));
                            }
                        }
                    }
                    None => {
                        emitter.emit_const_null();
                        emitter.emit_imm1(OdinHeadCode::StoreLocal, slot);
                        ctx.bindings.insert(name.clone(), (None, declared.unwrap_or(StaticType::Unknown)));
                    }
                }
                Ok(())
            }
            Statement::Block(statements) => self.lower_statements(statements, ctx, emitter),
            Statement::If { test, consequent, alternate } => {
                let consts = self.known_map(ctx);
                let condition = eval_known(test, &consts).map(|value| semantics::truthy(&value));
                let shape = compiler_specialize::branch_residual_shape(&condition);
                let then_body = statement_body(consequent);
                let else_body: &[Statement] = match alternate.as_deref() {
                    Some(statement) => statement_body(statement),
                    None => &[],
                };
                match shape {
                    BranchResidual::ThenOnly => {
                        check_branch_residual(shape, true, false)?;
                        self.lower_statements(then_body, ctx, emitter)?;
                    }
                    BranchResidual::ElseOnly => {
                        check_branch_residual(shape, false, true)?;
                        self.lower_statements(else_body, ctx, emitter)?;
                    }
                    BranchResidual::Both => {
                        check_branch_residual(shape, true, true)?;
                        self.emit_condition(test, ctx, emitter)?;
                        let else_label = emitter.alloc_label();
                        let end_label = emitter.alloc_label();
                        emitter.emit_jump_to_label(OdinHeadCode::JumpIfFalse, else_label);
                        self.lower_statements(then_body, ctx, emitter)?;
                        emitter.emit_jump_to_label(OdinHeadCode::Jump, end_label);
                        emitter.emit_label(else_label);
                        self.lower_statements(else_body, ctx, emitter)?;
                        emitter.emit_label(end_label);
                        // 分支内赋值使已知性保守失效（流不敏感）。
                        self.invalidate_assigned(then_body, ctx);
                        self.invalidate_assigned(else_body, ctx);
                    }
                }
                Ok(())
            }
            Statement::While { test, body } => {
                let loop_label = emitter.alloc_label();
                let end_label = emitter.alloc_label();
                emitter.emit_label(loop_label);
                self.emit_condition(test, ctx, emitter)?;
                emitter.emit_jump_to_label(OdinHeadCode::JumpIfFalse, end_label);
                ctx.loops.push((end_label, loop_label));
                self.lower_statements(statement_body(body), ctx, emitter)?;
                ctx.loops.pop();
                emitter.emit_jump_to_label(OdinHeadCode::Jump, loop_label);
                emitter.emit_label(end_label);
                self.invalidate_assigned(statement_body(body), ctx);
                Ok(())
            }
            Statement::For { init, test, update, body } => {
                if let Some(init) = init {
                    self.lower_statement(init, ctx, emitter)?;
                }
                let loop_label = emitter.alloc_label();
                let end_label = emitter.alloc_label();
                emitter.emit_label(loop_label);
                if let Some(test) = test {
                    self.emit_condition(test, ctx, emitter)?;
                    emitter.emit_jump_to_label(OdinHeadCode::JumpIfFalse, end_label);
                }
                let continue_label = if update.is_some() { emitter.alloc_label() } else { loop_label };
                ctx.loops.push((end_label, continue_label));
                self.lower_statements(statement_body(body), ctx, emitter)?;
                ctx.loops.pop();
                if let Some(update) = update {
                    emitter.emit_label(continue_label);
                    let residual = self.emit_expr(update, ctx, emitter)?;
                    self.materialize_if_known(residual, emitter)?;
                    emitter.emit_plain(OdinHeadCode::Pop);
                }
                emitter.emit_jump_to_label(OdinHeadCode::Jump, loop_label);
                emitter.emit_label(end_label);
                self.invalidate_assigned(statement_body(body), ctx);
                Ok(())
            }
            Statement::Return(expression) => {
                match expression {
                    Some(expression) => {
                        let residual = self.emit_expr(expression, ctx, emitter)?;
                        self.materialize_if_known(residual, emitter)?;
                    }
                    None => emitter.emit_const_null(),
                }
                emitter.emit_plain(OdinHeadCode::Return);
                Ok(())
            }
            Statement::Break => match ctx.loops.last() {
                Some((end_label, _)) => {
                    emitter.emit_jump_to_label(OdinHeadCode::Jump, *end_label);
                    Ok(())
                }
                None => Err(SpecializeFailure::Unsupported { construct: "break".to_string(), detail: "不在循环内".to_string() }),
            },
            Statement::Continue => match ctx.loops.last() {
                Some((_, continue_label)) => {
                    emitter.emit_jump_to_label(OdinHeadCode::Jump, *continue_label);
                    Ok(())
                }
                None => Err(SpecializeFailure::Unsupported { construct: "continue".to_string(), detail: "不在循环内".to_string() }),
            },
            other => Err(SpecializeFailure::Unsupported { construct: "函数体语句".to_string(), detail: format!("{other:?}") }),
        }
    }

    /// 发射条件并留下可被 `JumpIfFalse` 消费的布尔栈顶。
    fn emit_condition(&mut self, test: &Expression, ctx: &mut FnCtx, emitter: &mut BytecodeEmitter<'_>) -> Result<(), SpecializeFailure> {
        let static_ty = self.static_type_of(test, ctx);
        let residual = self.emit_expr(test, ctx, emitter)?;
        match residual {
            Residual::Known(value, _) => self.emit_known(&Value::Bool(semantics::truthy(&value)), emitter)?,
            Residual::OnStack(_) => {
                if !matches!(static_ty, StaticType::Boolean) {
                    // 动态非 bool：truthiness 残差到 provider。
                    emitter.emit_call_import("ts.truthy", 1);
                }
            }
        }
        Ok(())
    }

    fn materialize_if_known(&mut self, residual: Residual, emitter: &mut BytecodeEmitter<'_>) -> Result<(), SpecializeFailure> {
        match residual {
            Residual::Known(value, _) => self.emit_known(&value, emitter),
            Residual::OnStack(_) => Ok(()),
        }
    }

    fn emit_known(&mut self, value: &Value, emitter: &mut BytecodeEmitter<'_>) -> Result<(), SpecializeFailure> {
        match value {
            Value::Number(number) => emitter.emit_const_f64(number.to_bits()),
            Value::String(text) => {
                let index = emitter.intern_string(text);
                emitter.emit_imm1(OdinHeadCode::Constant, index);
            }
            Value::Bool(flag) => emitter.emit_const_bool(*flag),
            Value::Null | Value::Undefined => emitter.emit_const_null(),
            _ => {
                return Err(SpecializeFailure::Unsupported {
                    construct: "特化期常量".to_string(),
                    detail: "对象 / 数组 / 函数值无法直接进常量池".to_string(),
                })
            }
        }
        Ok(())
    }

    fn slot(&mut self, name: &str, ctx: &mut FnCtx) -> i32 {
        if let Some(slot) = ctx.slots.get(name) {
            return *slot;
        }
        let slot = ctx.next_slot;
        ctx.next_slot += 1;
        ctx.slots.insert(name.to_string(), slot);
        slot
    }

    /// 发射表达式，返回已知值或栈顶残差。
    fn emit_expr(&mut self, expression: &Expression, ctx: &mut FnCtx, emitter: &mut BytecodeEmitter<'_>) -> Result<Residual, SpecializeFailure> {
        match expression {
            Expression::Literal(value) => match literal_to_value(value) {
                Some(value) => Ok(Residual::Known(value.clone(), narrow_literal_value(&value))),
                None => Err(SpecializeFailure::Unsupported { construct: "字面量".to_string(), detail: format!("{value:?}") }),
            },
            Expression::Identifier(name) => {
                let (value, ty) = ctx
                    .bindings
                    .get(name)
                    .cloned()
                    .ok_or_else(|| SpecializeFailure::Unsupported { construct: "标识符".to_string(), detail: format!("`{name}` 未绑定") })?;
                match value {
                    Some(value) => Ok(Residual::Known(value, ty)),
                    None => {
                        let slot = *ctx
                            .slots
                            .get(name)
                            .ok_or_else(|| SpecializeFailure::Unsupported { construct: "标识符".to_string(), detail: format!("`{name}` 无槽位") })?;
                        emitter.emit_imm1(OdinHeadCode::LoadLocal, slot);
                        Ok(Residual::OnStack(ty))
                    }
                }
            }
            Expression::Binary { left, op, right } => self.emit_binary(op.clone(), left, right, ctx, emitter),
            Expression::Unary { op, expr } => {
                use language_typescript_ir::types::UnaryOp;
                match op {
                    UnaryOp::Neg => {
                        let operand = self.emit_expr(expr, ctx, emitter)?;
                        match operand {
                            Residual::Known(value, ty) => Ok(Residual::Known(Value::Number(-semantics::to_number(&value)), ty)),
                            Residual::OnStack(ty) => {
                                if !matches!(ty, StaticType::Number) {
                                    return Err(SpecializeFailure::Unsupported {
                                        construct: "一元 `-`".to_string(),
                                        detail: "操作数静态类型不是 number".to_string(),
                                    });
                                }
                                emitter.emit_call_import("f64_neg", 1);
                                Ok(Residual::OnStack(StaticType::Number))
                            }
                        }
                    }
                    UnaryOp::Not => {
                        let operand = self.emit_expr(expr, ctx, emitter)?;
                        match operand {
                            Residual::Known(value, _) => Ok(Residual::Known(Value::Bool(!semantics::truthy(&value)), StaticType::Boolean)),
                            Residual::OnStack(ty) => {
                                if !matches!(ty, StaticType::Boolean) {
                                    emitter.emit_call_import("ts.truthy", 1);
                                }
                                emit_bool_flip(emitter);
                                Ok(Residual::OnStack(StaticType::Boolean))
                            }
                        }
                    }
                    other => Err(SpecializeFailure::Unsupported { construct: "一元操作".to_string(), detail: format!("{other:?}") }),
                }
            }
            Expression::Conditional { test, consequent, alternate } => {
                let consts = self.known_map(ctx);
                let condition = eval_known(test, &consts).map(|value| semantics::truthy(&value));
                match condition {
                    Stage::Known(true) => self.emit_expr(consequent, ctx, emitter),
                    Stage::Known(false) => self.emit_expr(alternate, ctx, emitter),
                    Stage::Unknown => {
                        let cons_ty = self.static_type_of(consequent, ctx);
                        let alt_ty = self.static_type_of(alternate, ctx);
                        let result_ty = merge_type(&cons_ty, &alt_ty);
                        self.emit_condition(test, ctx, emitter)?;
                        let else_label = emitter.alloc_label();
                        let end_label = emitter.alloc_label();
                        let tmp = ctx.next_slot;
                        ctx.next_slot += 1;
                        emitter.emit_jump_to_label(OdinHeadCode::JumpIfFalse, else_label);
                        let cons = self.emit_expr(consequent, ctx, emitter)?;
                        self.materialize(cons, emitter)?;
                        emitter.emit_imm1(OdinHeadCode::StoreLocal, tmp);
                        emitter.emit_jump_to_label(OdinHeadCode::Jump, end_label);
                        emitter.emit_label(else_label);
                        let alt = self.emit_expr(alternate, ctx, emitter)?;
                        self.materialize(alt, emitter)?;
                        emitter.emit_imm1(OdinHeadCode::StoreLocal, tmp);
                        emitter.emit_label(end_label);
                        emitter.emit_imm1(OdinHeadCode::LoadLocal, tmp);
                        Ok(Residual::OnStack(result_ty))
                    }
                }
            }
            Expression::Assignment { left, op, right } => {
                use language_typescript_ir::types::AssignmentOp;
                if !matches!(op, AssignmentOp::Assign) {
                    return Err(SpecializeFailure::Unsupported { construct: "赋值".to_string(), detail: format!("{op:?}") });
                }
                match left.as_ref() {
                    Expression::Identifier(name) => {
                        let residual = self.emit_expr(right, ctx, emitter)?;
                        let slot = self.slot(name, ctx);
                        match &residual {
                            Residual::Known(value, ty) => {
                                self.emit_known(value, emitter)?;
                                emitter.emit_imm1(OdinHeadCode::StoreLocal, slot);
                                ctx.bindings.insert(name.clone(), (Some(value.clone()), ty.clone()));
                            }
                            Residual::OnStack(ty) => {
                                emitter.emit_imm1(OdinHeadCode::StoreLocal, slot);
                                ctx.bindings.insert(name.clone(), (None, ty.clone()));
                            }
                        }
                        // 赋值表达式的值重新压回栈顶（JS 语义：`x = e` 的完成值是 e）。
                        emitter.emit_imm1(OdinHeadCode::LoadLocal, slot);
                        Ok(residual)
                    }
                    Expression::Member { object, property } => {
                        let key = member_name(property)?;
                        let object_residual = self.emit_expr(object, ctx, emitter)?;
                        let value_residual = self.emit_expr(right, ctx, emitter)?;
                        match (&object_residual, &value_residual) {
                            (Residual::Known(Value::Object(fields), _), Residual::Known(value, ty)) => {
                                fields.borrow_mut().insert(key, value.clone());
                                Ok(Residual::Known(value.clone(), ty.clone()))
                            }
                            (Residual::OnStack(StaticType::Object(fields)), _) => {
                                let slot = self.field_slot(fields, &key);
                                match value_residual {
                                    Residual::Known(value, _) => self.emit_known(&value, emitter)?,
                                    Residual::OnStack(_) => {}
                                }
                                emitter.emit_imm1(OdinHeadCode::FieldSet, slot);
                                Ok(Residual::OnStack(StaticType::Unknown))
                            }
                            _ => Err(SpecializeFailure::Unsupported {
                                construct: "成员赋值".to_string(),
                                detail: "对象或字段类型不可静态判定".to_string(),
                            }),
                        }
                    }
                    other => Err(SpecializeFailure::Unsupported { construct: "赋值左侧".to_string(), detail: format!("{other:?}") }),
                }
            }
            Expression::Member { object, property } => {
                let key = member_name(property)?;
                let object_residual = self.emit_expr(object, ctx, emitter)?;
                match object_residual {
                    Residual::Known(Value::Object(fields), _) => match fields.borrow().get(&key) {
                        Some(value) => Ok(Residual::Known(value.clone(), narrow_literal_value(value))),
                        None => Ok(Residual::Known(Value::Undefined, StaticType::Unknown)),
                    },
                    Residual::Known(_, _) => {
                        Err(SpecializeFailure::Unsupported { construct: "成员访问".to_string(), detail: "已知值不是对象".to_string() })
                    }
                    Residual::OnStack(StaticType::Object(fields)) => {
                        let slot = self.field_slot(&fields, &key);
                        let field_ty = fields.iter().find(|(name, _)| *name == key).map(|(_, ty)| narrow(ty)).unwrap_or(StaticType::Unknown);
                        emitter.emit_imm1(OdinHeadCode::FieldGet, slot);
                        Ok(Residual::OnStack(field_ty))
                    }
                    Residual::OnStack(ty) => Err(SpecializeFailure::Unsupported {
                        construct: format!("成员访问 `.{key}`"),
                        detail: format!("对象静态类型不可判定（{ty:?}）"),
                    }),
                }
            }
            Expression::Object(fields) => {
                let mut typed = Vec::with_capacity(fields.len());
                let mut lowered = Vec::with_capacity(fields.len());
                for (key, value) in fields {
                    let residual = self.emit_expr(value, ctx, emitter)?;
                    let ty = match &residual {
                        Residual::Known(value, _) => widen_runtime(value),
                        Residual::OnStack(ty) => widen_static(ty),
                    };
                    typed.push((key.clone(), ty));
                    lowered.push((key.clone(), residual));
                }
                let layout = self.register_layout(&typed);
                emitter.emit_imm1(OdinHeadCode::ObjectNew, layout);
                for (key, residual) in lowered {
                    let slot = self.field_slot(&typed, &key);
                    emitter.emit_plain(OdinHeadCode::Dup);
                    match residual {
                        Residual::Known(value, _) => self.emit_known(&value, emitter)?,
                        Residual::OnStack(_) => {}
                    }
                    emitter.emit_imm1(OdinHeadCode::FieldSet, slot);
                }
                Ok(Residual::OnStack(StaticType::Object(typed)))
            }
            Expression::Call { callee, args } => self.emit_call(callee, args, ctx, emitter),
            other => Err(SpecializeFailure::Unsupported { construct: "表达式".to_string(), detail: format!("{other:?}") }),
        }
    }

    fn materialize(&mut self, residual: Residual, emitter: &mut BytecodeEmitter<'_>) -> Result<(), SpecializeFailure> {
        match residual {
            Residual::Known(value, _) => self.emit_known(&value, emitter),
            Residual::OnStack(_) => Ok(()),
        }
    }

    fn emit_call(&mut self, callee: &Expression, args: &[Expression], ctx: &mut FnCtx, emitter: &mut BytecodeEmitter<'_>) -> Result<Residual, SpecializeFailure> {
        // `console.log(...)`：残差为固定 arity 的 `ts.log` provider 调用。
        if let Expression::Member { object, property } = callee {
            if matches!(object.as_ref(), Expression::Identifier(name) if name == "console") {
                let method = member_name(property)?;
                if method != "log" {
                    return Err(SpecializeFailure::Unsupported { construct: "console 方法".to_string(), detail: method });
                }
                for arg in args {
                    let residual = self.emit_expr(arg, ctx, emitter)?;
                    self.materialize(residual, emitter)?;
                }
                // 副作用按源码序登记进账本（框架约束）。
                self.effect_order += 1;
                self.effects.record(self.effect_order, "ts.log").map_err(SpecializeFailure::Framework)?;
                emitter.emit_call_import("ts.log", args.len() as i32);
                return Ok(Residual::OnStack(StaticType::Unknown));
            }
        }
        let Expression::Identifier(name) = callee else {
            return Err(SpecializeFailure::Unsupported { construct: "调用目标".to_string(), detail: "仅支持具名函数与 console.log".to_string() });
        };
        let Some(index) = self.facts.function_index.get(name).copied() else {
            return Err(SpecializeFailure::Unsupported { construct: "调用".to_string(), detail: format!("函数 `{name}` 未声明") });
        };
        let info = self.facts.functions[index].clone();
        let consts = self.known_map(ctx);
        let known_args: Option<Vec<Value>> = args.iter().map(|arg| eval_known(arg, &consts).value().cloned()).collect();
        if let Some(known_args) = known_args {
            let mut visiting = HashSet::new();
            if is_pure(&info, &self.facts, &mut visiting) {
                // 解释器分派在特化期结算，从产物中消失。
                if let Some(value) = inline_eval(&info, &known_args, &self.facts) {
                    let ty = info.return_type.as_ref().map(narrow).unwrap_or_else(|| narrow_literal_value(&value));
                    return Ok(Residual::Known(value, ty));
                }
            }
        }
        for arg in args {
            let residual = self.emit_expr(arg, ctx, emitter)?;
            self.materialize(residual, emitter)?;
        }
        let callee_index = self.function_slot[&info.name];
        // `CallStatic` 为 `Imm1` 形：实参个数由目标函数 `arity` 消费。
        emitter.emit_imm1(OdinHeadCode::CallStatic, callee_index);
        Ok(Residual::OnStack(info.return_type.as_ref().map(narrow).unwrap_or(StaticType::Unknown)))
    }

    fn emit_binary(&mut self, op: BinaryOp, left: &Expression, right: &Expression, ctx: &mut FnCtx, emitter: &mut BytecodeEmitter<'_>) -> Result<Residual, SpecializeFailure> {
        let consts = self.known_map(ctx);
        let left_known = eval_known(left, &consts);
        let right_known = eval_known(right, &consts);
        if let (Stage::Known(a), Stage::Known(b)) = (&left_known, &right_known) {
            let value = fold_binary(&op, a, b).ok_or_else(|| SpecializeFailure::Unsupported {
                construct: "已知二元运算".to_string(),
                detail: format!("{op:?}"),
            })?;
            return Ok(Residual::Known(value, StaticType::Unknown));
        }
        let left_ty = self.static_type_of(left, ctx);
        let right_ty = self.static_type_of(right, ctx);
        use BinaryOp::*;
        match op {
            Add => {
                let result_ty = match (&left_ty, &right_ty) {
                    (StaticType::String, StaticType::Number) | (StaticType::Number, StaticType::String) | (StaticType::String, StaticType::String) => {
                        StaticType::String
                    }
                    (StaticType::Number, StaticType::Number) => StaticType::Number,
                    _ => StaticType::Unknown,
                };
                match (&left_ty, &right_ty) {
                    (StaticType::Number, StaticType::Number) => {
                        self.emit_operand(left, &left_known, ctx, emitter)?;
                        self.emit_operand(right, &right_known, ctx, emitter)?;
                        emitter.emit_call_import("f64_add", 2);
                    }
                    (StaticType::String, StaticType::Number) => {
                        self.emit_operand(left, &left_known, ctx, emitter)?;
                        self.emit_operand(right, &right_known, ctx, emitter)?;
                        emitter.emit_call_import("ts.num_to_string", 1);
                        emitter.emit_call_import("utf8.concat", 2);
                    }
                    (StaticType::Number, StaticType::String) => {
                        self.emit_operand(left, &left_known, ctx, emitter)?;
                        emitter.emit_call_import("ts.num_to_string", 1);
                        self.emit_operand(right, &right_known, ctx, emitter)?;
                        emitter.emit_call_import("utf8.concat", 2);
                    }
                    (StaticType::String, StaticType::String) => {
                        self.emit_operand(left, &left_known, ctx, emitter)?;
                        self.emit_operand(right, &right_known, ctx, emitter)?;
                        emitter.emit_call_import("utf8.concat", 2);
                    }
                    _ => {
                        // 类型不可判定：JS `+` 残差到 provider。
                        self.emit_operand(left, &left_known, ctx, emitter)?;
                        self.emit_operand(right, &right_known, ctx, emitter)?;
                        emitter.emit_call_import("ts.add", 2);
                    }
                }
                Ok(Residual::OnStack(result_ty))
            }
            Sub | Mul | Div => {
                if !(matches!(left_ty, StaticType::Number) && matches!(right_ty, StaticType::Number)) {
                    return Err(SpecializeFailure::Unsupported {
                        construct: format!("二元 `{op:?}`"),
                        detail: "两侧静态类型必须都是 number".to_string(),
                    });
                }
                self.emit_operand(left, &left_known, ctx, emitter)?;
                self.emit_operand(right, &right_known, ctx, emitter)?;
                emitter.emit_call_import(match op { Sub => "f64_sub", Mul => "f64_mul", _ => "f64_div" }, 2);
                Ok(Residual::OnStack(StaticType::Number))
            }
            StrictEq | StrictNeq => {
                let strategy = match (&left_ty, &right_ty) {
                    (StaticType::String, StaticType::String) => "utf8.eq",
                    (StaticType::Number, StaticType::Number) => "f64_eq",
                    _ => "ts.strictEqual",
                };
                self.emit_operand(left, &left_known, ctx, emitter)?;
                self.emit_operand(right, &right_known, ctx, emitter)?;
                emitter.emit_call_import(strategy, 2);
                if matches!(op, StrictNeq) {
                    emit_bool_flip(emitter);
                }
                Ok(Residual::OnStack(StaticType::Boolean))
            }
            Lt | Lte | Gt | Gte => {
                if !(matches!(left_ty, StaticType::Number) && matches!(right_ty, StaticType::Number)) {
                    return Err(SpecializeFailure::Unsupported {
                        construct: format!("关系 `{op:?}`"),
                        detail: "两侧静态类型必须都是 number".to_string(),
                    });
                }
                self.emit_operand(left, &left_known, ctx, emitter)?;
                self.emit_operand(right, &right_known, ctx, emitter)?;
                emitter.emit_call_import(match op { Lt => "f64_lt", Lte => "f64_le", Gt => "f64_gt", _ => "f64_ge" }, 2);
                Ok(Residual::OnStack(StaticType::Boolean))
            }
            And | Or => {
                // 短路：值语义（返回操作数本身），残差经临时槽。
                let left_ty_static = self.static_type_of(left, ctx);
                let tmp = ctx.next_slot;
                ctx.next_slot += 1;
                let left_residual = self.emit_expr(left, ctx, emitter)?;
                self.materialize(left_residual, emitter)?;
                emitter.emit_imm1(OdinHeadCode::StoreLocal, tmp);
                emitter.emit_imm1(OdinHeadCode::LoadLocal, tmp);
                if !matches!(left_ty_static, StaticType::Boolean) {
                    emitter.emit_call_import("ts.truthy", 1);
                }
                let end_label = emitter.alloc_label();
                emitter.emit_jump_to_label(if matches!(op, And) { OdinHeadCode::JumpIfFalse } else { OdinHeadCode::JumpIfTrue }, end_label);
                let right_residual = self.emit_expr(right, ctx, emitter)?;
                self.materialize(right_residual, emitter)?;
                emitter.emit_imm1(OdinHeadCode::StoreLocal, tmp);
                emitter.emit_label(end_label);
                emitter.emit_imm1(OdinHeadCode::LoadLocal, tmp);
                Ok(Residual::OnStack(merge_type(&left_ty_static, &self.static_type_of(right, ctx))))
            }
            other => Err(SpecializeFailure::Unsupported { construct: "二元运算".to_string(), detail: format!("{other:?}") }),
        }
    }

    fn emit_operand(&mut self, expression: &Expression, known: &Stage<Value>, ctx: &mut FnCtx, emitter: &mut BytecodeEmitter<'_>) -> Result<(), SpecializeFailure> {
        match known {
            Stage::Known(value) => self.emit_known(value, emitter),
            Stage::Unknown => {
                let residual = self.emit_expr(expression, ctx, emitter)?;
                self.materialize(residual, emitter)
            }
        }
    }

    /// 登记对象布局（字段按名字排序冻结），返回 layouts 下标。
    fn register_layout(&mut self, fields: &[(String, TypeAnnotation)]) -> i32 {
        let mut names: Vec<String> = fields.iter().map(|(name, _)| name.clone()).collect();
        names.sort();
        names.dedup();
        if let Some(index) = self.layout_fields.iter().position(|existing| *existing == names) {
            return index as i32;
        }
        self.layout_fields.push(names);
        self.layouts.push(OdinLayout { field_count: self.layout_fields.last().expect("刚推入").len() as i32 });
        (self.layouts.len() - 1) as i32
    }

    /// 字段名 → 冻结槽位（按名字排序）。
    fn field_slot(&mut self, fields: &[(String, TypeAnnotation)], key: &str) -> i32 {
        let layout = self.register_layout(fields);
        let names = &self.layout_fields[layout as usize];
        names.iter().position(|name| name == key).unwrap_or(0) as i32
    }

    /// 分支内赋值使已知性保守失效。
    fn invalidate_assigned(&mut self, statements: &[Statement], ctx: &mut FnCtx) {
        for statement in statements {
            match statement {
                Statement::VariableDeclaration { name, .. } => {
                    if let Some(entry) = ctx.bindings.get_mut(name) {
                        entry.0 = None;
                    }
                }
                Statement::Expression(expression) => {
                    if let Expression::Assignment { left, .. } = expression.as_ref() {
                        if let Expression::Identifier(name) = left.as_ref() {
                            if let Some(entry) = ctx.bindings.get_mut(name) {
                                entry.0 = None;
                            }
                        }
                    }
                }
                Statement::Block(inner) => self.invalidate_assigned(inner, ctx),
                Statement::If { consequent, alternate, .. } => {
                    self.invalidate_assigned(statement_body(consequent), ctx);
                    if let Some(alternate) = alternate {
                        self.invalidate_assigned(statement_body(alternate), ctx);
                    }
                }
                _ => {}
            }
        }
    }

    /// 表达式的静态类型（语义裁决的输入）。
    fn static_type_of(&mut self, expression: &Expression, ctx: &FnCtx) -> StaticType {
        match expression {
            Expression::Literal(value) => literal_to_value(value).map(|value| narrow_literal_value(&value)).unwrap_or(StaticType::Unknown),
            Expression::Identifier(name) => ctx.bindings.get(name).map(|(_, ty)| ty.clone()).unwrap_or(StaticType::Unknown),
            Expression::Member { object, property } => {
                let key = match property.as_ref() {
                    Expression::Identifier(name) => name.clone(),
                    _ => return StaticType::Unknown,
                };
                match self.static_type_of(object, ctx) {
                    StaticType::Object(fields) => fields
                        .iter()
                        .find(|(name, _)| *name == key)
                        .map(|(_, ty)| narrow(ty))
                        .unwrap_or(StaticType::Unknown),
                    _ => StaticType::Unknown,
                }
            }
            Expression::Binary { left, op, right } => {
                let left = self.static_type_of(left, ctx);
                let right = self.static_type_of(right, ctx);
                use BinaryOp::*;
                match op {
                    Add => match (&left, &right) {
                        (StaticType::String, StaticType::Number)
                        | (StaticType::Number, StaticType::String)
                        | (StaticType::String, StaticType::String) => StaticType::String,
                        (StaticType::Number, StaticType::Number) => StaticType::Number,
                        _ => StaticType::Unknown,
                    },
                    Sub | Mul | Div | Mod => {
                        if matches!(left, StaticType::Number) && matches!(right, StaticType::Number) {
                            StaticType::Number
                        }
                        else {
                            StaticType::Unknown
                        }
                    }
                    StrictEq | StrictNeq | Eq | Neq | Lt | Lte | Gt | Gte => StaticType::Boolean,
                    _ => StaticType::Unknown,
                }
            }
            Expression::Unary { op, expr } => match op {
                language_typescript_ir::types::UnaryOp::Not => StaticType::Boolean,
                language_typescript_ir::types::UnaryOp::Neg => {
                    if matches!(self.static_type_of(expr, ctx), StaticType::Number) {
                        StaticType::Number
                    }
                    else {
                        StaticType::Unknown
                    }
                }
                _ => StaticType::Unknown,
            },
            Expression::Conditional { consequent, alternate, .. } => {
                let cons = self.static_type_of(consequent, ctx);
                let alt = self.static_type_of(alternate, ctx);
                merge_type(&cons, &alt)
            }
            Expression::Assignment { right, .. } => self.static_type_of(right, ctx),
            Expression::Object(fields) => {
                StaticType::Object(fields.iter().map(|(name, value)| (name.clone(), widen_static(&self.static_type_of(value, ctx)))).collect())
            }
            Expression::Call { callee, .. } => {
                if let Expression::Identifier(name) = callee.as_ref() {
                    if let Some(index) = self.facts.function_index.get(name) {
                        return self.facts.functions[*index].return_type.as_ref().map(narrow).unwrap_or(StaticType::Unknown);
                    }
                }
                StaticType::Unknown
            }
            _ => StaticType::Unknown,
        }
    }
}

/// 栈顶布尔取反（无 `Not` 头码，用条件跳转构造）。
fn emit_bool_flip(emitter: &mut BytecodeEmitter<'_>) {
    let true_label = emitter.alloc_label();
    let end_label = emitter.alloc_label();
    emitter.emit_jump_to_label(OdinHeadCode::JumpIfTrue, true_label);
    emitter.emit_const_bool(true);
    emitter.emit_jump_to_label(OdinHeadCode::Jump, end_label);
    emitter.emit_label(true_label);
    emitter.emit_const_bool(false);
    emitter.emit_label(end_label);
}

fn fold_binary(op: &BinaryOp, left: &Value, right: &Value) -> Option<Value> {
    Some(match op {
        BinaryOp::Add => semantics::add(left, right),
        BinaryOp::Sub => Value::Number(semantics::to_number(left) - semantics::to_number(right)),
        BinaryOp::Mul => Value::Number(semantics::to_number(left) * semantics::to_number(right)),
        BinaryOp::Div => Value::Number(semantics::to_number(left) / semantics::to_number(right)),
        BinaryOp::Mod => Value::Number(semantics::to_number(left) % semantics::to_number(right)),
        BinaryOp::StrictEq => Value::Bool(semantics::strict_eq(left, right)),
        BinaryOp::StrictNeq => Value::Bool(!semantics::strict_eq(left, right)),
        BinaryOp::Eq => Value::Bool(semantics::loose_eq(left, right)),
        BinaryOp::Neq => Value::Bool(!semantics::loose_eq(left, right)),
        BinaryOp::And => {
            if semantics::truthy(left) { right.clone() } else { left.clone() }
        }
        BinaryOp::Or => {
            if semantics::truthy(left) { left.clone() } else { right.clone() }
        }
        BinaryOp::Lt | BinaryOp::Lte | BinaryOp::Gt | BinaryOp::Gte => {
            let a = number_order(left)?;
            let b = number_order(right)?;
            Value::Bool(match op {
                BinaryOp::Lt => a < b,
                BinaryOp::Lte => a <= b,
                BinaryOp::Gt => a > b,
                _ => a >= b,
            })
        }
        _ => return None,
    })
}

fn number_order(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) if !number.is_nan() => Some(*number),
        _ => None,
    }
}

fn narrow_literal_value(value: &Value) -> StaticType {
    match value {
        Value::Number(_) => StaticType::Number,
        Value::String(_) => StaticType::String,
        Value::Bool(_) => StaticType::Boolean,
        _ => StaticType::Unknown,
    }
}

fn widen_static(ty: &StaticType) -> TypeAnnotation {
    match ty {
        StaticType::Number => TypeAnnotation::Primitive(PrimitiveType::Number),
        StaticType::String => TypeAnnotation::Primitive(PrimitiveType::String),
        StaticType::Boolean => TypeAnnotation::Primitive(PrimitiveType::Boolean),
        StaticType::Object(fields) => TypeAnnotation::Object(fields.clone()),
        StaticType::Unknown => TypeAnnotation::Any,
    }
}

fn widen_runtime(value: &Value) -> TypeAnnotation {
    widen_static(&narrow_literal_value(value))
}

fn merge_type(left: &StaticType, right: &StaticType) -> StaticType {
    if *left == *right {
        left.clone()
    }
    else {
        StaticType::Unknown
    }
}

fn member_name(property: &Expression) -> Result<String, SpecializeFailure> {
    match property {
        Expression::Identifier(name) => Ok(name.clone()),
        Expression::Literal(language_typescript_types::TsValue::String(text)) => Ok(text.clone()),
        _ => Err(SpecializeFailure::Unsupported { construct: "成员名".to_string(), detail: format!("{property:?}") }),
    }
}

/// 单语句体（`if` / `while` 的 `Box<Statement>`）视作语句序列。
fn statement_body(statement: &Statement) -> &[Statement] {
    match statement {
        Statement::Block(inner) => inner,
        _ => std::slice::from_ref(statement),
    }
}
