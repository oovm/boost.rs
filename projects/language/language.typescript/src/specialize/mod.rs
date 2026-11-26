//! 特化（Futamura 第一投影）：MIR → 目标无关残差 IR。
//!
//! 本模块是「解释器与特化从同一 MIR 分派」的另一半：已知／未知传播、
//! 分支残差形状、副作用账本全部在 MIR 节点上裁决，不触碰 AST。
//! 框架合同（分支残差形状、副作用账本）来自 `compiler.specialize`：
//!
//! - 已知条件消去对应分支；未知条件两侧必须完整残差；
//! - 残差效应按源码序登记进账本，禁止特化期偷跑；
//! - 未知输入参与的运算保留为残差操作（禁止样例常量预执行）；
//! - 纯函数且实参全已知时用解释器（同一 MIR）结算，分派从产物消失。

use std::collections::HashMap;

use compiler_specialize::{SideEffectLedger, SpecializeError, Stage};
use language_typescript_ir::types::{BinaryOp, UnaryOp};

use crate::hir::BindingRef;
use crate::mir::{literal_to_value, merge_type, EffectClass, MirExpr, MirExprKind, MirFunction, MirProgram, MirStmt, TypeFact};
use crate::residual::{LayoutId, ResidualConst, ResidualFunction, ResidualOp, ResidualProgram, SlotId};
use crate::runtime::{semantics, Interpreter, Value};

/// 重导出框架合同，消费者只需依赖本模块即可拿到约束接口。
pub use compiler_specialize::{branch_residual_shape, check_branch_residual};
/// 类型注解收窄（语言侧事实来源）。
pub use crate::mir::narrow;
/// 兼容别名：静态类型事实即 MIR 类型事实。
pub type StaticType = TypeFact;

/// 特化失败：结构化、带确切缺项，不做猜测回退。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecializeFailure {
    /// 顶层构造不在投影子集内。
    UnsupportedTopLevel {
        /// 构造描述。
        construct: String,
    },
    /// 表达式／语句构造不受支持。
    Unsupported {
        /// 构造类别。
        construct: String,
        /// 确切缺项。
        detail: String,
    },
    /// 框架约束违约。
    Framework(SpecializeError),
}

impl std::fmt::Display for SpecializeFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedTopLevel { construct } => write!(formatter, "顶层构造不在投影子集内: {construct}"),
            Self::Unsupported { construct, detail } => write!(formatter, "{construct} 不受支持: {detail}"),
            Self::Framework(error) => write!(formatter, "特化框架约束违约: {error}"),
        }
    }
}

impl std::error::Error for SpecializeFailure {}

impl From<SpecializeError> for SpecializeFailure {
    fn from(error: SpecializeError) -> Self {
        Self::Framework(error)
    }
}

/// MIR 程序 → 残差程序。
///
/// 顶层校验（模块绑定只接受 `const` 字面量）后逐函数特化；已知事实
/// （模块 `const`、全已知纯调用）在特化期结算，未知输入参与运算一律
/// 残差，绝不做样例常量预执行。
pub fn specialize(program: &MirProgram) -> Result<ResidualProgram, SpecializeFailure> {
    let mut specializer = Specializer { program, effects: SideEffectLedger::new(), effect_order: 0, layouts: Vec::new() };
    specializer.check_top()?;
    let known_globals = specializer.known_globals();
    let mut functions = Vec::with_capacity(program.functions.len());
    for function in &program.functions {
        functions.push(specializer.specialize_function(function, &known_globals)?);
    }
    Ok(ResidualProgram { functions, layouts: specializer.layouts })
}

struct Specializer<'a> {
    program: &'a MirProgram,
    /// 副作用账本：残差效应必须按源码序登记。
    effects: SideEffectLedger,
    /// 副作用源码序计数器。
    effect_order: u64,
    /// 布局注册序（字段名排序冻结）。
    layouts: Vec<Vec<String>>,
}

/// 表达式特化结果：已知值（不产生代码）或栈顶残差。
#[derive(Debug, Clone)]
enum Residual {
    /// 特化期已结算。
    Known(Value, TypeFact),
    /// 值已压在栈顶。
    OnStack(TypeFact),
}

/// 单个函数特化期间的上下文。
struct FnCtx {
    /// 局部槽 → （已知值（`None` = 未知，必须残差），类型事实）。
    locals: HashMap<SlotId, (Option<Value>, TypeFact)>,
    /// 全局绑定 → （已知值，类型事实）：模块 `const` 预置。
    globals: HashMap<SlotId, (Value, TypeFact)>,
    /// 下一个可用帧槽（临时槽从函数帧槽之后分配）。
    next_slot: SlotId,
    /// 下一个可用标签。
    next_label: u32,
    /// 循环标签栈：`(break 标签, continue 标签)`。
    loops: Vec<(u32, u32)>,
}

impl FnCtx {
    fn alloc_slot(&mut self) -> SlotId {
        let slot = self.next_slot;
        self.next_slot += 1;
        slot
    }

    fn alloc_label(&mut self) -> u32 {
        let label = self.next_label;
        self.next_label += 1;
        label
    }
}

impl<'a> Specializer<'a> {
    /// 顶层构造校验：只接受模块 `const` 字面量、函数声明与类型声明。
    fn check_top(&self) -> Result<(), SpecializeFailure> {
        for statement in &self.program.top {
            self.check_top_statement(statement)?;
        }
        Ok(())
    }

    fn check_top_statement(&self, statement: &MirStmt) -> Result<(), SpecializeFailure> {
        match statement {
            MirStmt::Declare { binding: BindingRef::Global(id), init, .. } => {
                let global = &self.program.globals[*id as usize];
                if !global.is_const {
                    return Err(SpecializeFailure::UnsupportedTopLevel { construct: "模块级 `let` / `var`".to_string() });
                }
                if global.literal.is_none() {
                    let construct = if init.is_none() {
                        format!("无初始化的模块级 `const {}`", global.name)
                    }
                    else {
                        format!("模块级 `const {}` 的初始化不是字面量", global.name)
                    };
                    return Err(SpecializeFailure::UnsupportedTopLevel { construct });
                }
                Ok(())
            }
            MirStmt::FunctionDecl { .. } | MirStmt::TypeDecl => Ok(()),
            MirStmt::Export { inner, is_default: false } => self.check_top_statement(inner),
            MirStmt::Export { is_default: true, .. } => {
                Err(SpecializeFailure::UnsupportedTopLevel { construct: "default export".to_string() })
            }
            MirStmt::OpaqueDecl(detail) => Err(SpecializeFailure::UnsupportedTopLevel { construct: detail.clone() }),
            _ => Err(SpecializeFailure::UnsupportedTopLevel { construct: "该顶层构造".to_string() }),
        }
    }

    /// 模块 `const` 字面量 → 特化期已知事实。
    fn known_globals(&self) -> HashMap<SlotId, (Value, TypeFact)> {
        let mut map = HashMap::new();
        for (id, global) in self.program.globals.iter().enumerate() {
            if let Some((literal, fact)) = &global.literal {
                map.insert(id as SlotId, (literal_to_value(literal), fact.clone()));
            }
        }
        map
    }

    fn specialize_function(&mut self, function: &MirFunction, known_globals: &HashMap<SlotId, (Value, TypeFact)>) -> Result<ResidualFunction, SpecializeFailure> {
        let mut ctx = FnCtx { locals: HashMap::new(), globals: known_globals.clone(), next_slot: function.slot_count, next_label: 0, loops: Vec::new() };
        for (index, _) in function.params.iter().enumerate() {
            let fact = function.param_types.get(index).cloned().unwrap_or(TypeFact::Unknown);
            // 形参全为未知（guest 输入）；声明的对象类型即布局合同，登记进 layouts。
            if let TypeFact::Object(fields) = &fact {
                self.register_layout(fields);
            }
            ctx.locals.insert(index as SlotId, (None, fact));
        }
        let mut body = Vec::new();
        self.lower_statements(&function.body, &mut ctx, &mut body)?;
        // 统一出口：无显式 return 的函数补空值。
        body.push(ResidualOp::Const(ResidualConst::Null));
        body.push(ResidualOp::Return);
        Ok(ResidualFunction {
            name: function.name.clone(),
            arity: function.params.len() as u32,
            slots: ctx.next_slot.max(function.params.len() as u32),
            body,
            exported: function.exported,
        })
    }

    fn lower_statements(&mut self, statements: &[MirStmt], ctx: &mut FnCtx, out: &mut Vec<ResidualOp>) -> Result<(), SpecializeFailure> {
        for statement in statements {
            self.lower_statement(statement, ctx, out)?;
        }
        Ok(())
    }

    fn lower_statement(&mut self, statement: &MirStmt, ctx: &mut FnCtx, out: &mut Vec<ResidualOp>) -> Result<(), SpecializeFailure> {
        match statement {
            MirStmt::Expr(expression) => {
                let residual = self.emit_expr(expression, ctx, out)?;
                // 表达式语句丢弃结果，保持栈平衡。
                self.materialize(residual, out)?;
                out.push(ResidualOp::Pop);
                Ok(())
            }
            MirStmt::Declare { binding: BindingRef::Local(slot), declared, init, .. } => {
                let slot = *slot;
                match init {
                    Some(expression) => {
                        let residual = self.emit_expr(expression, ctx, out)?;
                        match residual {
                            Residual::Known(value, fact) => {
                                self.emit_known(&value, out)?;
                                out.push(ResidualOp::Store(slot));
                                ctx.locals.insert(slot, (Some(value), declared.clone().unwrap_or(fact)));
                            }
                            Residual::OnStack(fact) => {
                                out.push(ResidualOp::Store(slot));
                                ctx.locals.insert(slot, (None, declared.clone().unwrap_or(fact)));
                            }
                        }
                    }
                    None => {
                        out.push(ResidualOp::Const(ResidualConst::Null));
                        out.push(ResidualOp::Store(slot));
                        ctx.locals.insert(slot, (None, declared.clone().unwrap_or(TypeFact::Unknown)));
                    }
                }
                Ok(())
            }
            MirStmt::Declare { binding, .. } => {
                Err(SpecializeFailure::Unsupported { construct: "函数体语句".to_string(), detail: format!("非局部声明绑定 {binding:?}") })
            }
            MirStmt::Block(inner) => self.lower_statements(inner, ctx, out),
            MirStmt::If { test, then, else_ } => {
                let condition = self.eval_known(test, ctx).map(|value| semantics::truthy(&value));
                let shape = compiler_specialize::branch_residual_shape(&condition);
                match shape {
                    compiler_specialize::BranchResidual::ThenOnly => {
                        check_branch_residual(shape, true, false).map_err(SpecializeFailure::Framework)?;
                        self.lower_statements(then, ctx, out)?;
                    }
                    compiler_specialize::BranchResidual::ElseOnly => {
                        check_branch_residual(shape, false, true).map_err(SpecializeFailure::Framework)?;
                        self.lower_statements(else_, ctx, out)?;
                    }
                    compiler_specialize::BranchResidual::Both => {
                        check_branch_residual(shape, true, true).map_err(SpecializeFailure::Framework)?;
                        self.emit_condition(test, ctx, out)?;
                        let else_label = ctx.alloc_label();
                        let end_label = ctx.alloc_label();
                        out.push(ResidualOp::JumpIfFalse(else_label));
                        self.lower_statements(then, ctx, out)?;
                        out.push(ResidualOp::Jump(end_label));
                        out.push(ResidualOp::Label(else_label));
                        self.lower_statements(else_, ctx, out)?;
                        out.push(ResidualOp::Label(end_label));
                        // 分支内赋值使已知性保守失效（流不敏感）。
                        self.invalidate_assigned(then, ctx);
                        self.invalidate_assigned(else_, ctx);
                    }
                }
                Ok(())
            }
            MirStmt::While { test, body } => {
                let loop_label = ctx.alloc_label();
                let end_label = ctx.alloc_label();
                out.push(ResidualOp::Label(loop_label));
                self.emit_condition(test, ctx, out)?;
                out.push(ResidualOp::JumpIfFalse(end_label));
                ctx.loops.push((end_label, loop_label));
                self.lower_statements(body, ctx, out)?;
                ctx.loops.pop();
                out.push(ResidualOp::Jump(loop_label));
                out.push(ResidualOp::Label(end_label));
                self.invalidate_assigned(body, ctx);
                Ok(())
            }
            MirStmt::For { init, test, update, body } => {
                if let Some(init) = init {
                    self.lower_statement(init, ctx, out)?;
                }
                let loop_label = ctx.alloc_label();
                let end_label = ctx.alloc_label();
                out.push(ResidualOp::Label(loop_label));
                if let Some(test) = test {
                    self.emit_condition(test, ctx, out)?;
                    out.push(ResidualOp::JumpIfFalse(end_label));
                }
                let continue_label = if update.is_some() { ctx.alloc_label() } else { loop_label };
                ctx.loops.push((end_label, continue_label));
                self.lower_statements(body, ctx, out)?;
                ctx.loops.pop();
                if let Some(update) = update {
                    out.push(ResidualOp::Label(continue_label));
                    let residual = self.emit_expr(update, ctx, out)?;
                    self.materialize(residual, out)?;
                    out.push(ResidualOp::Pop);
                }
                out.push(ResidualOp::Jump(loop_label));
                out.push(ResidualOp::Label(end_label));
                self.invalidate_assigned(body, ctx);
                Ok(())
            }
            MirStmt::Return(expression) => {
                match expression {
                    Some(expression) => {
                        let residual = self.emit_expr(expression, ctx, out)?;
                        self.materialize(residual, out)?;
                    }
                    None => out.push(ResidualOp::Const(ResidualConst::Null)),
                }
                out.push(ResidualOp::Return);
                Ok(())
            }
            MirStmt::Break => match ctx.loops.last() {
                Some((end_label, _)) => {
                    out.push(ResidualOp::Jump(*end_label));
                    Ok(())
                }
                None => Err(SpecializeFailure::Unsupported { construct: "break".to_string(), detail: "不在循环内".to_string() }),
            },
            MirStmt::Continue => match ctx.loops.last() {
                Some((_, continue_label)) => {
                    out.push(ResidualOp::Jump(*continue_label));
                    Ok(())
                }
                None => Err(SpecializeFailure::Unsupported { construct: "continue".to_string(), detail: "不在循环内".to_string() }),
            },
            other => Err(SpecializeFailure::Unsupported { construct: "函数体语句".to_string(), detail: describe_statement(other) }),
        }
    }

    /// 发射条件并留下可被 `JumpIfFalse` 消费的布尔栈顶。
    fn emit_condition(&mut self, test: &MirExpr, ctx: &mut FnCtx, out: &mut Vec<ResidualOp>) -> Result<(), SpecializeFailure> {
        let fact = self.fact_of(test, ctx);
        let residual = self.emit_expr(test, ctx, out)?;
        match residual {
            Residual::Known(value, _) => self.emit_known(&Value::Bool(semantics::truthy(&value)), out)?,
            Residual::OnStack(_) => {
                if !matches!(fact, TypeFact::Boolean) {
                    // 动态非 bool：truthiness 残差。
                    out.push(ResidualOp::Truthy);
                }
            }
        }
        Ok(())
    }

    fn materialize(&mut self, residual: Residual, out: &mut Vec<ResidualOp>) -> Result<(), SpecializeFailure> {
        match residual {
            Residual::Known(value, _) => self.emit_known(&value, out),
            Residual::OnStack(_) => Ok(()),
        }
    }

    fn emit_known(&mut self, value: &Value, out: &mut Vec<ResidualOp>) -> Result<(), SpecializeFailure> {
        match value {
            Value::Number(number) => out.push(ResidualOp::Const(ResidualConst::Number(*number))),
            Value::String(text) => out.push(ResidualOp::Const(ResidualConst::String(text.to_string()))),
            Value::Bool(flag) => out.push(ResidualOp::Const(ResidualConst::Bool(*flag))),
            Value::Null => out.push(ResidualOp::Const(ResidualConst::Null)),
            Value::Undefined => out.push(ResidualOp::Const(ResidualConst::Undefined)),
            _ => {
                return Err(SpecializeFailure::Unsupported {
                    construct: "特化期常量".to_string(),
                    detail: "对象 / 数组 / 函数值无法直接进常量池".to_string(),
                })
            }
        }
        Ok(())
    }

    /// 发射表达式，返回已知值或栈顶残差。
    fn emit_expr(&mut self, expression: &MirExpr, ctx: &mut FnCtx, out: &mut Vec<ResidualOp>) -> Result<Residual, SpecializeFailure> {
        match &expression.kind {
            MirExprKind::Literal(literal) => {
                let value = literal_to_value(literal);
                Ok(Residual::Known(value.clone(), value_fact(&value)))
            }
            MirExprKind::Binding(binding) => match binding {
                BindingRef::Local(slot) => {
                    let (value, fact) = ctx
                        .locals
                        .get(slot)
                        .cloned()
                        .ok_or_else(|| SpecializeFailure::Unsupported { construct: "标识符".to_string(), detail: format!("槽 `{slot}` 未绑定") })?;
                    match value {
                        Some(value) => Ok(Residual::Known(value, fact)),
                        None => {
                            out.push(ResidualOp::Load(*slot));
                            Ok(Residual::OnStack(fact))
                        }
                    }
                }
                BindingRef::Global(id) => match ctx.globals.get(id) {
                    Some((value, fact)) => Ok(Residual::Known(value.clone(), fact.clone())),
                    None => Err(SpecializeFailure::Unsupported {
                        construct: "标识符".to_string(),
                        detail: format!("`{}` 未绑定", self.program.globals[*id as usize].name),
                    }),
                },
                BindingRef::This => Err(SpecializeFailure::Unsupported { construct: "标识符".to_string(), detail: "`this` 未绑定".to_string() }),
                BindingRef::HostConsole => {
                    Err(SpecializeFailure::Unsupported { construct: "标识符".to_string(), detail: "`console` 未绑定".to_string() })
                }
                BindingRef::Unbound(name) => {
                    Err(SpecializeFailure::Unsupported { construct: "标识符".to_string(), detail: format!("`{name}` 未绑定") })
                }
            },
            MirExprKind::Binary { op, left, right } => self.emit_binary(op.clone(), left, right, ctx, out),
            MirExprKind::Unary { op: UnaryOp::Neg, operand } => {
                let residual = self.emit_expr(operand, ctx, out)?;
                match residual {
                    Residual::Known(value, fact) => Ok(Residual::Known(Value::Number(-semantics::to_number(&value)), fact)),
                    Residual::OnStack(fact) => {
                        if !matches!(fact, TypeFact::Number) {
                            return Err(SpecializeFailure::Unsupported {
                                construct: "一元 `-`".to_string(),
                                detail: "操作数静态类型不是 number".to_string(),
                            });
                        }
                        out.push(ResidualOp::NumNeg);
                        Ok(Residual::OnStack(TypeFact::Number))
                    }
                }
            }
            MirExprKind::Unary { op: UnaryOp::Not, operand } => {
                let residual = self.emit_expr(operand, ctx, out)?;
                match residual {
                    Residual::Known(value, _) => Ok(Residual::Known(Value::Bool(!semantics::truthy(&value)), TypeFact::Boolean)),
                    Residual::OnStack(fact) => {
                        if !matches!(fact, TypeFact::Boolean) {
                            out.push(ResidualOp::Truthy);
                        }
                        out.push(ResidualOp::BoolNot);
                        Ok(Residual::OnStack(TypeFact::Boolean))
                    }
                }
            }
            MirExprKind::Unary { op, .. } => {
                Err(SpecializeFailure::Unsupported { construct: "一元操作".to_string(), detail: format!("{op:?}") })
            }
            MirExprKind::Conditional { test, then, else_ } => {
                let condition = self.eval_known(test, ctx).map(|value| semantics::truthy(&value));
                match condition {
                    Stage::Known(true) => self.emit_expr(then, ctx, out),
                    Stage::Known(false) => self.emit_expr(else_, ctx, out),
                    Stage::Unknown => {
                        let then_fact = self.fact_of(then, ctx);
                        let else_fact = self.fact_of(else_, ctx);
                        let result_fact = merge_type(&then_fact, &else_fact);
                        self.emit_condition(test, ctx, out)?;
                        let else_label = ctx.alloc_label();
                        let end_label = ctx.alloc_label();
                        let tmp = ctx.alloc_slot();
                        out.push(ResidualOp::JumpIfFalse(else_label));
                        let cons = self.emit_expr(then, ctx, out)?;
                        self.materialize(cons, out)?;
                        out.push(ResidualOp::Store(tmp));
                        out.push(ResidualOp::Jump(end_label));
                        out.push(ResidualOp::Label(else_label));
                        let alt = self.emit_expr(else_, ctx, out)?;
                        self.materialize(alt, out)?;
                        out.push(ResidualOp::Store(tmp));
                        out.push(ResidualOp::Label(end_label));
                        out.push(ResidualOp::Load(tmp));
                        Ok(Residual::OnStack(result_fact))
                    }
                }
            }
            MirExprKind::Assignment { target, value } => match target {
                crate::mir::MirAssignTarget::Binding(BindingRef::Local(slot)) => {
                    let residual = self.emit_expr(value, ctx, out)?;
                    let fact = match &residual {
                        Residual::Known(value, fact) => {
                            self.emit_known(value, out)?;
                            out.push(ResidualOp::Store(*slot));
                            ctx.locals.insert(*slot, (Some(value.clone()), fact.clone()));
                            fact.clone()
                        }
                        Residual::OnStack(fact) => {
                            out.push(ResidualOp::Store(*slot));
                            ctx.locals.insert(*slot, (None, fact.clone()));
                            fact.clone()
                        }
                    };
                    // 赋值表达式的值重新压回栈顶（JS 语义：`x = e` 的完成值是 e）。
                    let _ = fact;
                    out.push(ResidualOp::Load(*slot));
                    Ok(residual)
                }
                crate::mir::MirAssignTarget::Binding(BindingRef::Global(id)) => Err(SpecializeFailure::Unsupported {
                    construct: "赋值".to_string(),
                    detail: format!("模块级绑定 `{}` 的赋值不可残差", self.program.globals[*id as usize].name),
                }),
                crate::mir::MirAssignTarget::Binding(binding) => {
                    Err(SpecializeFailure::Unsupported { construct: "赋值左侧".to_string(), detail: format!("{binding:?}") })
                }
                crate::mir::MirAssignTarget::Member { object, key } => {
                    let key = key.clone().ok_or_else(|| SpecializeFailure::Unsupported {
                        construct: "成员名".to_string(),
                        detail: "动态成员名不在投影子集内".to_string(),
                    })?;
                    let object_residual = self.emit_expr(object, ctx, out)?;
                    let value_residual = self.emit_expr(value, ctx, out)?;
                    match (&object_residual, &value_residual) {
                        (Residual::Known(Value::Object(fields), _), Residual::Known(value, fact)) => {
                            fields.borrow_mut().insert(key, value.clone());
                            Ok(Residual::Known(value.clone(), fact.clone()))
                        }
                        (Residual::OnStack(TypeFact::Object(fields)), _) => {
                            let layout = self.register_layout(fields);
                            let slot = self.field_slot(fields, &key);
                            match &value_residual {
                                Residual::Known(value, _) => self.emit_known(value, out)?,
                                Residual::OnStack(_) => {}
                            }
                            out.push(ResidualOp::FieldSet(layout, slot));
                            Ok(Residual::OnStack(TypeFact::Unknown))
                        }
                        _ => Err(SpecializeFailure::Unsupported {
                            construct: "成员赋值".to_string(),
                            detail: "对象或字段类型不可静态判定".to_string(),
                        }),
                    }
                }
                crate::mir::MirAssignTarget::Index { .. } => {
                    Err(SpecializeFailure::Unsupported { construct: "赋值左侧".to_string(), detail: "下标写入".to_string() })
                }
            },
            MirExprKind::Member { object, key } => {
                let key = key.clone().ok_or_else(|| SpecializeFailure::Unsupported {
                    construct: "成员名".to_string(),
                    detail: "动态成员名不在投影子集内".to_string(),
                })?;
                let object_residual = self.emit_expr(object, ctx, out)?;
                match object_residual {
                    Residual::Known(Value::Object(fields), _) => match fields.borrow().get(&key) {
                        Some(value) => Ok(Residual::Known(value.clone(), value_fact(&value))),
                        None => Ok(Residual::Known(Value::Undefined, TypeFact::Unknown)),
                    },
                    Residual::Known(_, _) => {
                        Err(SpecializeFailure::Unsupported { construct: "成员访问".to_string(), detail: "已知值不是对象".to_string() })
                    }
                    Residual::OnStack(TypeFact::Object(fields)) => {
                        let slot = self.field_slot(&fields, &key);
                        let layout = self.register_layout(&fields);
                        let field_fact = fields.iter().find(|(name, _)| *name == key).map(|(_, fact)| fact.clone()).unwrap_or(TypeFact::Unknown);
                        out.push(ResidualOp::FieldGet(layout, slot));
                        Ok(Residual::OnStack(field_fact))
                    }
                    Residual::OnStack(fact) => Err(SpecializeFailure::Unsupported {
                        construct: format!("成员访问 `.{key}`"),
                        detail: format!("对象静态类型不可判定（{fact:?}）"),
                    }),
                }
            }
            MirExprKind::Index { .. } => {
                Err(SpecializeFailure::Unsupported { construct: "表达式".to_string(), detail: "下标访问不在投影子集内".to_string() })
            }
            MirExprKind::Array { .. } => {
                Err(SpecializeFailure::Unsupported { construct: "表达式".to_string(), detail: "数组字面量不在投影子集内".to_string() })
            }
            MirExprKind::Object { fields } => {
                let mut typed = Vec::with_capacity(fields.len());
                let mut lowered = Vec::with_capacity(fields.len());
                for (key, value) in fields {
                    let residual = self.emit_expr(value, ctx, out)?;
                    let fact = match &residual {
                        Residual::Known(value, _) => value_fact(value),
                        Residual::OnStack(fact) => fact.clone(),
                    };
                    typed.push((key.clone(), fact));
                    lowered.push((key.clone(), residual));
                }
                let layout = self.register_layout(&typed);
                out.push(ResidualOp::ObjectNew(layout));
                for (key, residual) in lowered {
                    let slot = self.field_slot(&typed, &key);
                    out.push(ResidualOp::Dup);
                    match residual {
                        Residual::Known(value, _) => self.emit_known(&value, out)?,
                        Residual::OnStack(_) => {}
                    }
                    out.push(ResidualOp::FieldSet(layout, slot));
                }
                Ok(Residual::OnStack(TypeFact::Object(typed)))
            }
            MirExprKind::Call { target, args, arity } => match target {
                crate::mir::MirCallTarget::HostLog => {
                    for arg in args {
                        let residual = self.emit_expr(arg, ctx, out)?;
                        self.materialize(residual, out)?;
                    }
                    // 副作用按源码序登记进账本（框架约束）。
                    self.effect_order += 1;
                    self.effects.record(self.effect_order, "console.log").map_err(SpecializeFailure::Framework)?;
                    out.push(ResidualOp::HostLog { arity: *arity });
                    Ok(Residual::OnStack(TypeFact::Unknown))
                }
                crate::mir::MirCallTarget::Named(index) => {
                    let known_args: Option<Vec<Value>> = args.iter().map(|arg| self.eval_known(arg, ctx).value().cloned()).collect();
                    let target_function = self.program.functions[*index].clone();
                    if let Some(known_args) = known_args {
                        if target_function.pure {
                            // 解释器分派在特化期结算（同一 MIR），从产物中消失。
                            if let Some(value) = self.inline_eval(*index, &known_args) {
                                let fact =
                                    if target_function.return_type.is_decided() { target_function.return_type.clone() } else { value_fact(&value) };
                                return Ok(Residual::Known(value, fact));
                            }
                        }
                    }
                    for arg in args {
                        let residual = self.emit_expr(arg, ctx, out)?;
                        self.materialize(residual, out)?;
                    }
                    out.push(ResidualOp::Call(*index));
                    Ok(Residual::OnStack(target_function.return_type.clone()))
                }
                crate::mir::MirCallTarget::Dynamic(_) => Err(SpecializeFailure::Unsupported {
                    construct: "调用目标".to_string(),
                    detail: "仅支持具名函数与 console.log".to_string(),
                }),
            },
            MirExprKind::Unsupported(detail) => {
                Err(SpecializeFailure::Unsupported { construct: "表达式".to_string(), detail: detail.clone() })
            }
        }
    }

    fn emit_binary(&mut self, op: BinaryOp, left: &MirExpr, right: &MirExpr, ctx: &mut FnCtx, out: &mut Vec<ResidualOp>) -> Result<Residual, SpecializeFailure> {
        let left_known = self.eval_known(left, ctx);
        let right_known = self.eval_known(right, ctx);
        if let (Stage::Known(a), Stage::Known(b)) = (&left_known, &right_known) {
            let value = fold_binary(&op, a, b).ok_or_else(|| SpecializeFailure::Unsupported {
                construct: "已知二元运算".to_string(),
                detail: format!("{op:?}"),
            })?;
            return Ok(Residual::Known(value, TypeFact::Unknown));
        }
        let left_ty = self.fact_of(left, ctx);
        let right_ty = self.fact_of(right, ctx);
        use BinaryOp::*;
        match op {
            Add => {
                let result_ty = match (&left_ty, &right_ty) {
                    (TypeFact::String, TypeFact::Number) | (TypeFact::Number, TypeFact::String) | (TypeFact::String, TypeFact::String) => TypeFact::String,
                    (TypeFact::Number, TypeFact::Number) => TypeFact::Number,
                    _ => TypeFact::Unknown,
                };
                match (&left_ty, &right_ty) {
                    (TypeFact::Number, TypeFact::Number) => {
                        self.emit_operand(left, &left_known, ctx, out)?;
                        self.emit_operand(right, &right_known, ctx, out)?;
                        out.push(ResidualOp::NumAdd);
                    }
                    (TypeFact::String, TypeFact::Number) => {
                        self.emit_operand(left, &left_known, ctx, out)?;
                        self.emit_operand(right, &right_known, ctx, out)?;
                        out.push(ResidualOp::NumToText);
                        out.push(ResidualOp::StrConcat);
                    }
                    (TypeFact::Number, TypeFact::String) => {
                        self.emit_operand(left, &left_known, ctx, out)?;
                        out.push(ResidualOp::NumToText);
                        self.emit_operand(right, &right_known, ctx, out)?;
                        out.push(ResidualOp::StrConcat);
                    }
                    (TypeFact::String, TypeFact::String) => {
                        self.emit_operand(left, &left_known, ctx, out)?;
                        self.emit_operand(right, &right_known, ctx, out)?;
                        out.push(ResidualOp::StrConcat);
                    }
                    _ => {
                        // 类型不可判定：JS `+` 残差。
                        self.emit_operand(left, &left_known, ctx, out)?;
                        self.emit_operand(right, &right_known, ctx, out)?;
                        out.push(ResidualOp::DynamicAdd);
                    }
                }
                Ok(Residual::OnStack(result_ty))
            }
            Sub | Mul | Div => {
                if !(matches!(left_ty, TypeFact::Number) && matches!(right_ty, TypeFact::Number)) {
                    return Err(SpecializeFailure::Unsupported {
                        construct: format!("二元 `{op:?}`"),
                        detail: "两侧静态类型必须都是 number".to_string(),
                    });
                }
                self.emit_operand(left, &left_known, ctx, out)?;
                self.emit_operand(right, &right_known, ctx, out)?;
                out.push(match op { Sub => ResidualOp::NumSub, Mul => ResidualOp::NumMul, _ => ResidualOp::NumDiv });
                Ok(Residual::OnStack(TypeFact::Number))
            }
            StrictEq | StrictNeq => {
                let strategy = match (&left_ty, &right_ty) {
                    (TypeFact::String, TypeFact::String) => ResidualOp::StrEq,
                    (TypeFact::Number, TypeFact::Number) => ResidualOp::NumEq,
                    _ => ResidualOp::DynamicStrictEq,
                };
                self.emit_operand(left, &left_known, ctx, out)?;
                self.emit_operand(right, &right_known, ctx, out)?;
                out.push(strategy);
                if matches!(op, StrictNeq) {
                    out.push(ResidualOp::BoolNot);
                }
                Ok(Residual::OnStack(TypeFact::Boolean))
            }
            Lt | Lte | Gt | Gte => {
                if !(matches!(left_ty, TypeFact::Number) && matches!(right_ty, TypeFact::Number)) {
                    return Err(SpecializeFailure::Unsupported {
                        construct: format!("关系 `{op:?}`"),
                        detail: "两侧静态类型必须都是 number".to_string(),
                    });
                }
                self.emit_operand(left, &left_known, ctx, out)?;
                self.emit_operand(right, &right_known, ctx, out)?;
                out.push(match op { Lt => ResidualOp::NumLt, Lte => ResidualOp::NumLe, Gt => ResidualOp::NumGt, _ => ResidualOp::NumGe });
                Ok(Residual::OnStack(TypeFact::Boolean))
            }
            And | Or => {
                // 短路：值语义（返回操作数本身），残差经临时槽。
                let left_fact = self.fact_of(left, ctx);
                let tmp = ctx.alloc_slot();
                let left_residual = self.emit_expr(left, ctx, out)?;
                self.materialize(left_residual, out)?;
                out.push(ResidualOp::Store(tmp));
                out.push(ResidualOp::Load(tmp));
                if !matches!(left_fact, TypeFact::Boolean) {
                    out.push(ResidualOp::Truthy);
                }
                let end_label = ctx.alloc_label();
                out.push(if matches!(op, And) { ResidualOp::JumpIfFalse(end_label) } else { ResidualOp::JumpIfTrue(end_label) });
                let right_residual = self.emit_expr(right, ctx, out)?;
                self.materialize(right_residual, out)?;
                out.push(ResidualOp::Store(tmp));
                out.push(ResidualOp::Label(end_label));
                out.push(ResidualOp::Load(tmp));
                Ok(Residual::OnStack(merge_type(&left_fact, &self.fact_of(right, ctx))))
            }
            other => Err(SpecializeFailure::Unsupported { construct: "二元运算".to_string(), detail: format!("{other:?}") }),
        }
    }

    fn emit_operand(&mut self, expression: &MirExpr, known: &Stage<Value>, ctx: &mut FnCtx, out: &mut Vec<ResidualOp>) -> Result<(), SpecializeFailure> {
        match known {
            Stage::Known(value) => self.emit_known(value, out),
            Stage::Unknown => {
                let residual = self.emit_expr(expression, ctx, out)?;
                self.materialize(residual, out)
            }
        }
    }

    /// 特化期已知表达式求值：只接受字面量与已知绑定的纯组合。
    ///
    /// 任何一步都调用 `runtime::semantics`，与解释器、provider 同源。
    pub fn eval_known(&self, expression: &MirExpr, ctx: &FnCtx) -> Stage<Value> {
        if !matches!(expression.effect, EffectClass::Pure | EffectClass::IdentitySensitive) {
            return Stage::Unknown;
        }
        match &expression.kind {
            MirExprKind::Literal(literal) => Stage::Known(literal_to_value(literal)),
            MirExprKind::Binding(BindingRef::Local(slot)) => {
                match ctx.locals.get(slot).and_then(|(value, _)| value.clone()) {
                    Some(value) => Stage::Known(value),
                    None => Stage::Unknown,
                }
            }
            MirExprKind::Binding(BindingRef::Global(id)) => {
                match ctx.globals.get(id) {
                    Some((value, _)) => Stage::Known(value.clone()),
                    None => Stage::Unknown,
                }
            }
            MirExprKind::Binary { op, left, right } => {
                let left = match self.eval_known(left, ctx) {
                    Stage::Known(value) => value,
                    Stage::Unknown => return Stage::Unknown,
                };
                let right = match self.eval_known(right, ctx) {
                    Stage::Known(value) => value,
                    Stage::Unknown => return Stage::Unknown,
                };
                match fold_binary(op, &left, &right) {
                    Some(value) => Stage::Known(value),
                    None => Stage::Unknown,
                }
            }
            MirExprKind::Unary { op, operand } => {
                let value = match self.eval_known(operand, ctx) {
                    Stage::Known(value) => value,
                    Stage::Unknown => return Stage::Unknown,
                };
                match op {
                    UnaryOp::Not => Stage::Known(Value::Bool(!semantics::truthy(&value))),
                    UnaryOp::Neg => Stage::Known(Value::Number(-semantics::to_number(&value))),
                    _ => Stage::Unknown,
                }
            }
            MirExprKind::Conditional { test, then, else_ } => {
                match self.eval_known(test, ctx) {
                    Stage::Known(value) => {
                        if semantics::truthy(&value) {
                            self.eval_known(then, ctx)
                        }
                        else {
                            self.eval_known(else_, ctx)
                        }
                    }
                    Stage::Unknown => Stage::Unknown,
                }
            }
            _ => Stage::Unknown,
        }
    }

    /// 表达式在当前语境下的类型事实（语义裁决的输入）。
    fn fact_of(&self, expression: &MirExpr, ctx: &FnCtx) -> TypeFact {
        match &expression.kind {
            MirExprKind::Literal(literal) => value_fact(&literal_to_value(literal)),
            MirExprKind::Binding(BindingRef::Local(slot)) => ctx.locals.get(slot).map(|(_, fact)| fact.clone()).unwrap_or(TypeFact::Unknown),
            MirExprKind::Binding(BindingRef::Global(id)) => ctx.globals.get(id).map(|(_, fact)| fact.clone()).unwrap_or(TypeFact::Unknown),
            MirExprKind::Binding(_) => TypeFact::Unknown,
            MirExprKind::Member { object, key } => match key.as_ref().and_then(|key| self.field_fact(object, key, ctx)) {
                Some(fact) => fact,
                None => TypeFact::Unknown,
            },
            MirExprKind::Binary { op, left, right } => {
                let left = self.fact_of(left, ctx);
                let right = self.fact_of(right, ctx);
                use BinaryOp::*;
                match op {
                    Add => match (&left, &right) {
                        (TypeFact::String, TypeFact::Number)
                        | (TypeFact::Number, TypeFact::String)
                        | (TypeFact::String, TypeFact::String) => TypeFact::String,
                        (TypeFact::Number, TypeFact::Number) => TypeFact::Number,
                        _ => TypeFact::Unknown,
                    },
                    Sub | Mul | Div | Mod => {
                        if matches!(left, TypeFact::Number) && matches!(right, TypeFact::Number) {
                            TypeFact::Number
                        }
                        else {
                            TypeFact::Unknown
                        }
                    }
                    StrictEq | StrictNeq | Eq | Neq | Lt | Lte | Gt | Gte => TypeFact::Boolean,
                    _ => TypeFact::Unknown,
                }
            }
            MirExprKind::Unary { op, operand } => match op {
                UnaryOp::Not => TypeFact::Boolean,
                UnaryOp::Neg => {
                    if matches!(self.fact_of(operand, ctx), TypeFact::Number) {
                        TypeFact::Number
                    }
                    else {
                        TypeFact::Unknown
                    }
                }
                _ => TypeFact::Unknown,
            },
            MirExprKind::Conditional { then, else_, .. } => {
                let then_fact = self.fact_of(then, ctx);
                let else_fact = self.fact_of(else_, ctx);
                merge_type(&then_fact, &else_fact)
            }
            MirExprKind::Assignment { value, .. } => self.fact_of(value, ctx),
            MirExprKind::Object { fields } => {
                TypeFact::Object(fields.iter().map(|(name, value)| (name.clone(), self.fact_of(value, ctx))).collect())
            }
            MirExprKind::Call { target, .. } => match target {
                crate::mir::MirCallTarget::Named(index) => self.program.functions[*index].return_type.clone(),
                _ => TypeFact::Unknown,
            },
            _ => TypeFact::Unknown,
        }
    }

    fn field_fact(&self, object: &MirExpr, key: &str, ctx: &FnCtx) -> Option<TypeFact> {
        match self.fact_of(object, ctx) {
            TypeFact::Object(fields) => fields.iter().find(|(name, _)| name == key).map(|(_, fact)| fact.clone()),
            _ => None,
        }
    }

    /// 分支内赋值使已知性保守失效。
    fn invalidate_assigned(&self, statements: &[MirStmt], ctx: &mut FnCtx) {
        for statement in statements {
            match statement {
                MirStmt::Declare { binding: BindingRef::Local(slot), .. } => {
                    if let Some(entry) = ctx.locals.get_mut(slot) {
                        entry.0 = None;
                    }
                }
                MirStmt::Expr(expression) => {
                    if let MirExprKind::Assignment { target: crate::mir::MirAssignTarget::Binding(BindingRef::Local(slot)), .. } = &expression.kind {
                        if let Some(entry) = ctx.locals.get_mut(slot) {
                            entry.0 = None;
                        }
                    }
                }
                MirStmt::Block(inner) => self.invalidate_assigned(inner, ctx),
                MirStmt::If { then, else_, .. } => {
                    self.invalidate_assigned(then, ctx);
                    self.invalidate_assigned(else_, ctx);
                }
                _ => {}
            }
        }
    }

    /// 解释器分派结算：纯函数 + 全已知实参 → 特化期结果值。
    ///
    /// 这就是「解释器与特化从同一 MIR 分派」：内联求值直接复用 [`Interpreter`]
    /// 执行同一 MIR 程序，分派本身从产物中消失。
    fn inline_eval(&self, index: usize, args: &[Value]) -> Option<Value> {
        let mut interpreter = Interpreter::from_mir(self.program).ok()?;
        interpreter.call_exported(&self.program.functions[index].name, args).ok()
    }

    /// 登记对象布局（字段按名字排序冻结），返回布局下标。
    fn register_layout(&mut self, fields: &[(String, TypeFact)]) -> LayoutId {
        let mut names: Vec<String> = fields.iter().map(|(name, _)| name.clone()).collect();
        names.sort();
        names.dedup();
        if let Some(index) = self.layouts.iter().position(|existing| *existing == names) {
            return index as LayoutId;
        }
        self.layouts.push(names);
        (self.layouts.len() - 1) as LayoutId
    }

    /// 字段名 → 冻结槽位（按名字排序）。
    fn field_slot(&mut self, fields: &[(String, TypeFact)], key: &str) -> u32 {
        let layout = self.register_layout(fields);
        let names = &self.layouts[layout as usize];
        names.iter().position(|name| name == key).unwrap_or(0) as u32
    }
}

/// 运行时值 → 类型事实（标量；对象 / 数组 / 函数不可判定）。
fn value_fact(value: &Value) -> TypeFact {
    match value {
        Value::Number(_) => TypeFact::Number,
        Value::String(_) => TypeFact::String,
        Value::Bool(_) => TypeFact::Boolean,
        _ => TypeFact::Unknown,
    }
}

/// 已知二元折叠：任何一步都走 `runtime::semantics`。
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

/// 函数体语句的诊断描述。
fn describe_statement(statement: &MirStmt) -> String {
    match statement {
        MirStmt::FunctionDecl { .. } => "嵌套函数声明".to_string(),
        MirStmt::TypeDecl => "类型声明".to_string(),
        MirStmt::OpaqueDecl(detail) => detail.clone(),
        MirStmt::Export { .. } => "export 包装".to_string(),
        other => format!("{other:?}"),
    }
}
