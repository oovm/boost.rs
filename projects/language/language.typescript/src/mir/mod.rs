//! MIR：带类型事实与语义类别的语言中间表示。
//!
//! 本层是 TS 前端的语义枢纽：解释器与特化（Futamura 第一投影）都从同一
//! MIR 程序分派。每个表达式节点携带：
//!
//! - [`TypeFact`]：静态类型事实（降低期的声明／注解估计；特化期以活绑定
//!   事实为准做裁决）；
//! - [`EffectClass`]：语义类别（纯、身份敏感、副作用、外部调用）。
//!
//! MIR 不含任何目标（OVM / Odin）形状，也不含 `ts.*` provider 符号名。

use std::{collections::HashMap, rc::Rc};

use language_typescript_ir::types::{BinaryOp, PrimitiveType, TypeAnnotation, UnaryOp};
use language_typescript_types::TsValue;

use crate::hir::{Hir, HirAssignTarget, HirCallTarget, HirExpr, HirFunction, HirStmt, BindingRef};
use crate::runtime::Value;

/// 静态类型事实。
#[derive(Debug, Clone)]
pub enum TypeFact {
    /// `number`。
    Number,
    /// `string`。
    String,
    /// `boolean`。
    Boolean,
    /// 对象：字段名与字段类型（声明序）。
    Object(Vec<(String, TypeFact)>),
    /// 不可判定（联合、引用、any 等）。
    Unknown,
}

impl TypeFact {
    /// 是否可与另一个类型事实共同裁决某个操作（双方都确定）。
    pub fn is_decided(&self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

// 对象按字段名集合判等（槽位冻结只看名字），与旧静态类型合同一致。
impl PartialEq for TypeFact {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Number, Self::Number) | (Self::String, Self::String) | (Self::Boolean, Self::Boolean) | (Self::Unknown, Self::Unknown) => true,
            (Self::Object(a), Self::Object(b)) => a.len() == b.len() && a.iter().all(|(name, _)| b.iter().any(|(other, _)| other == name)),
            _ => false,
        }
    }
}

/// 语义类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectClass {
    /// 纯：结果只由操作数值决定。
    Pure,
    /// 身份敏感：语义依赖引用身份（`===` / `!==` 的对象比较）。
    IdentitySensitive,
    /// 副作用：宿主可观察效应（日志、赋值）。
    SideEffect,
    /// 外部：不可静态结算的计算（动态调用、非纯函数调用）。
    External,
}

/// MIR 字面量（目标无关）。
#[derive(Debug, Clone, PartialEq)]
pub enum MirLiteral {
    /// `undefined`。
    Undefined,
    /// `null`。
    Null,
    /// 布尔。
    Bool(bool),
    /// 数字。
    Number(f64),
    /// 字符串。
    String(String),
    /// 对象字面量（声明序字段）。
    Object(Vec<(String, MirLiteral)>),
    /// 数组字面量。
    Array(Vec<MirLiteral>),
}

/// MIR 表达式节点：kind + 类型事实 + 语义类别。
#[derive(Debug, Clone)]
pub struct MirExpr {
    /// 形态。
    pub kind: MirExprKind,
    /// 降低期类型事实估计。
    pub ty: TypeFact,
    /// 语义类别。
    pub effect: EffectClass,
}

/// MIR 表达式形态。
#[derive(Debug, Clone)]
pub enum MirExprKind {
    /// 字面量。
    Literal(MirLiteral),
    /// 绑定引用。
    Binding(BindingRef),
    /// 二元表达式。
    Binary {
        /// 操作符。
        op: BinaryOp,
        /// 左操作数。
        left: Box<MirExpr>,
        /// 右操作数。
        right: Box<MirExpr>,
    },
    /// 一元表达式。
    Unary {
        /// 操作符。
        op: UnaryOp,
        /// 操作数。
        operand: Box<MirExpr>,
    },
    /// 三元条件。
    Conditional {
        /// 条件。
        test: Box<MirExpr>,
        /// 真分支。
        then: Box<MirExpr>,
        /// 假分支。
        else_: Box<MirExpr>,
    },
    /// 赋值表达式（完成值 = 右值）。
    Assignment {
        /// 目标。
        target: MirAssignTarget,
        /// 右值。
        value: Box<MirExpr>,
    },
    /// 成员访问。
    Member {
        /// 对象。
        object: Box<MirExpr>,
        /// 静态成员名。
        key: Option<String>,
    },
    /// 下标访问。
    Index {
        /// 对象。
        object: Box<MirExpr>,
        /// 下标。
        index: Box<MirExpr>,
    },
    /// 对象字面量。
    Object {
        /// 字段。
        fields: Vec<(String, MirExpr)>,
    },
    /// 数组字面量。
    Array {
        /// 元素。
        elements: Vec<MirExpr>,
    },
    /// 调用。
    Call {
        /// 目标。
        target: MirCallTarget,
        /// 实参。
        args: Vec<MirExpr>,
        /// 实参个数。
        arity: u32,
    },
    /// 解释子集之外的表达式形态。
    Unsupported(String),
}

/// 赋值目标。
#[derive(Debug, Clone)]
pub enum MirAssignTarget {
    /// 绑定。
    Binding(BindingRef),
    /// 成员写入。
    Member {
        /// 对象。
        object: Box<MirExpr>,
        /// 静态成员名。
        key: Option<String>,
    },
    /// 下标写入。
    Index {
        /// 对象。
        object: Box<MirExpr>,
        /// 下标。
        index: Box<MirExpr>,
    },
}

/// 调用目标。
#[derive(Debug, Clone)]
pub enum MirCallTarget {
    /// 具名函数（函数表下标）。
    Named(usize),
    /// `console.log`。
    HostLog,
    /// 动态 callee。
    Dynamic(Box<MirExpr>),
}

/// MIR 语句。
#[derive(Debug, Clone)]
pub enum MirStmt {
    /// 表达式语句。
    Expr(MirExpr),
    /// 变量声明。
    Declare {
        /// 绑定身份。
        binding: BindingRef,
        /// 声明注解给出的类型事实。
        declared: Option<TypeFact>,
        /// 初始化。
        init: Option<MirExpr>,
        /// 是否 `const`。
        is_const: bool,
    },
    /// 块。
    Block(Vec<MirStmt>),
    /// 分支。
    If {
        /// 条件。
        test: MirExpr,
        /// then。
        then: Vec<MirStmt>,
        /// else。
        else_: Vec<MirStmt>,
    },
    /// `while`。
    While {
        /// 条件。
        test: MirExpr,
        /// 体。
        body: Vec<MirStmt>,
    },
    /// 三段 `for`。
    For {
        /// 初始化。
        init: Option<Box<MirStmt>>,
        /// 条件。
        test: Option<MirExpr>,
        /// 更新。
        update: Option<MirExpr>,
        /// 体。
        body: Vec<MirStmt>,
    },
    /// `return`。
    Return(Option<MirExpr>),
    /// `break`。
    Break,
    /// `continue`。
    Continue,
    /// 函数声明。
    FunctionDecl {
        /// 绑定身份。
        binding: BindingRef,
        /// 函数表下标。
        function: usize,
    },
    /// 类型声明（无操作）。
    TypeDecl,
    /// 子集外声明形态。
    OpaqueDecl(String),
    /// `export` 包装。
    Export {
        /// 内层语句。
        inner: Box<MirStmt>,
        /// 是否默认导出。
        is_default: bool,
    },
}

/// MIR 函数。
#[derive(Debug, Clone)]
pub struct MirFunction {
    /// 函数名。
    pub name: String,
    /// 形参名。
    pub params: Vec<String>,
    /// 形参类型事实。
    pub param_types: Vec<TypeFact>,
    /// 返回类型事实。
    pub return_type: TypeFact,
    /// 槽名表。
    pub local_names: Vec<String>,
    /// 帧槽数。
    pub slot_count: u32,
    /// 函数体。
    pub body: Vec<MirStmt>,
    /// 是否导出。
    pub exported: bool,
    /// 纯性（不动点：无宿主日志、无赋值、无动态／非纯调用、无递归环）。
    pub pure: bool,
}

/// 模块绑定事实。
#[derive(Debug, Clone)]
pub struct MirGlobal {
    /// 名字。
    pub name: String,
    /// 是否 `const`。
    pub is_const: bool,
    /// 字面量初始化事实（特化期已知）。
    pub literal: Option<(MirLiteral, TypeFact)>,
    /// 函数声明绑定 → 函数表下标。
    pub function: Option<usize>,
    /// 是否导出。
    pub exported: bool,
}

/// MIR 程序。
#[derive(Debug, Clone)]
pub struct MirProgram {
    /// 函数表。
    pub functions: Vec<Rc<MirFunction>>,
    /// 函数名 → 下标。
    pub function_index: HashMap<String, usize>,
    /// 模块绑定。
    pub globals: Vec<MirGlobal>,
    /// 模块绑定名 → 下标。
    pub global_index: HashMap<String, usize>,
    /// 顶层模块帧槽数。
    pub module_slot_count: u32,
    /// 顶层模块帧槽名表。
    pub module_local_names: Vec<String>,
    /// 顶层语句。
    pub top: Vec<MirStmt>,
}

impl MirProgram {
    /// 函数体是否纯（对已结算的 `pure` 标志做不动点迭代使用）。
    pub fn body_pure(&self, index: usize) -> bool {
        let function = &self.functions[index];
        statements_pure(&function.body, self, index)
    }
}

/// HIR → MIR 降低（全函数；类型事实与语义类别在此附着）。
pub fn lower(hir: &Hir) -> MirProgram {
    let mut function_index: HashMap<String, usize> = HashMap::new();
    let mut functions: Vec<Rc<MirFunction>> = Vec::new();
    for (index, function) in hir.functions.iter().enumerate() {
        function_index.entry(function.name.clone()).or_insert(index);
        functions.push(Rc::new(lower_function(function, hir)));
    }
    let mut globals: Vec<MirGlobal> = hir
        .globals
        .iter()
        .map(|global| MirGlobal { name: global.name.clone(), is_const: global.is_const, literal: None, function: global.function, exported: global.exported })
        .collect();
    let top = lower_statements(&hir.top, hir);
    // 模块直接 `const` 字面量 → 已知事实。
    for statement in &top {
        collect_literal_facts(statement, &mut globals);
    }
    let mut program = MirProgram { functions, function_index, globals, global_index: hir.global_index.clone(), module_slot_count: hir.module_slot_count, module_local_names: hir.module_local_names.clone(), top };
    // 纯性不动点：从全假向上结算，递归环永不纯。
    loop {
        let mut changed = false;
        for index in 0..program.functions.len() {
            let pure = program.body_pure(index);
            if program.functions[index].pure != pure {
                let mut function = (*program.functions[index]).clone();
                function.pure = pure;
                program.functions[index] = Rc::new(function);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    // 具名调用节点的语义类别按最终纯性回填。
    for index in 0..program.functions.len() {
        let mut function = (*program.functions[index]).clone();
        refresh_statements(&mut function.body, &program);
        program.functions[index] = Rc::new(function);
    }
    let mut top = std::mem::take(&mut program.top);
    refresh_statements(&mut top, &program);
    program.top = top;
    program
}

fn collect_literal_facts(statement: &MirStmt, globals: &mut [MirGlobal]) {
    match statement {
        MirStmt::Declare { binding: BindingRef::Global(id), init: Some(expression), .. } => {
            if let MirExprKind::Literal(literal) = &expression.kind {
                if let Some(global) = globals.get_mut(*id as usize) {
                    global.literal = Some((literal.clone(), expression.ty.clone()));
                }
            }
        }
        MirStmt::Export { inner, .. } => collect_literal_facts(inner, globals),
        _ => {}
    }
}

fn refresh_statements(statements: &mut [MirStmt], program: &MirProgram) {
    for statement in statements {
        match statement {
            MirStmt::Expr(expression) => refresh_expression(expression, program),
            MirStmt::Declare { init, .. } => {
                if let Some(expression) = init {
                    refresh_expression(expression, program);
                }
            }
            MirStmt::Block(inner) => refresh_statements(inner, program),
            MirStmt::If { test, then, else_, .. } => {
                refresh_expression(test, program);
                refresh_statements(then, program);
                refresh_statements(else_, program);
            }
            MirStmt::While { test, body } => {
                refresh_expression(test, program);
                refresh_statements(body, program);
            }
            MirStmt::For { init, test, update, body } => {
                if let Some(init) = init {
                    refresh_statements(std::slice::from_mut(&mut **init), program);
                }
                if let Some(test) = test {
                    refresh_expression(test, program);
                }
                if let Some(update) = update {
                    refresh_expression(update, program);
                }
                refresh_statements(body, program);
            }
            MirStmt::Return(Some(expression)) => refresh_expression(expression, program),
            MirStmt::Export { inner, .. } => refresh_statements(std::slice::from_mut(&mut **inner), program),
            _ => {}
        }
    }
}

fn refresh_expression(expression: &mut MirExpr, program: &MirProgram) {
    match &mut expression.kind {
        MirExprKind::Binary { left, right, .. } => {
            refresh_expression(left, program);
            refresh_expression(right, program);
        }
        MirExprKind::Unary { operand, .. } => refresh_expression(operand, program),
        MirExprKind::Conditional { test, then, else_, .. } => {
            refresh_expression(test, program);
            refresh_expression(then, program);
            refresh_expression(else_, program);
        }
        MirExprKind::Assignment { target, value } => {
            match target {
                MirAssignTarget::Member { object, .. } => refresh_expression(object, program),
                MirAssignTarget::Index { object, index } => {
                    refresh_expression(object, program);
                    refresh_expression(index, program);
                }
                MirAssignTarget::Binding(_) => {}
            }
            refresh_expression(value, program);
        }
        MirExprKind::Member { object, .. } => refresh_expression(object, program),
        MirExprKind::Index { object, index } => {
            refresh_expression(object, program);
            refresh_expression(index, program);
        }
        MirExprKind::Object { fields } => {
            for (_, value) in fields {
                refresh_expression(value, program);
            }
        }
        MirExprKind::Array { elements } => {
            for element in elements {
                refresh_expression(element, program);
            }
        }
        MirExprKind::Call { target, args, .. } => {
            for arg in args {
                refresh_expression(arg, program);
            }
            if let MirCallTarget::Named(index) = target {
                expression.effect = if program.functions[*index].pure { EffectClass::Pure } else { EffectClass::External };
            }
        }
        _ => {}
    }
}

fn lower_function(function: &HirFunction, hir: &Hir) -> MirFunction {
    MirFunction {
        name: function.name.clone(),
        params: function.params.clone(),
        param_types: function.param_types.iter().map(|ty| ty.as_ref().map(narrow).unwrap_or(TypeFact::Unknown)).collect(),
        return_type: function.return_type.as_ref().map(narrow).unwrap_or(TypeFact::Unknown),
        local_names: function.local_names.clone(),
        slot_count: function.slot_count,
        body: lower_statements(&function.body, hir),
        exported: function.exported,
        pure: false,
    }
}

fn lower_statements(statements: &[HirStmt], hir: &Hir) -> Vec<MirStmt> {
    statements.iter().map(|statement| lower_statement(statement, hir)).collect()
}

fn lower_statement(statement: &HirStmt, hir: &Hir) -> MirStmt {
    match statement {
        HirStmt::Expr(expression) => MirStmt::Expr(lower_expression(expression, hir)),
        HirStmt::Declare { binding, ty, init, is_const } => {
            MirStmt::Declare { binding: binding.clone(), declared: ty.as_ref().map(narrow), init: init.as_ref().map(|e| lower_expression(e, hir)), is_const: *is_const }
        }
        HirStmt::Block(inner) => MirStmt::Block(lower_statements(inner, hir)),
        HirStmt::If { test, then, else_ } => MirStmt::If { test: lower_expression(test, hir), then: lower_statements(then, hir), else_: lower_statements(else_, hir) },
        HirStmt::While { test, body } => MirStmt::While { test: lower_expression(test, hir), body: lower_statements(body, hir) },
        HirStmt::For { init, test, update, body } => MirStmt::For {
            init: init.as_ref().map(|statement| Box::new(lower_statement(statement, hir))),
            test: test.as_ref().map(|expression| lower_expression(expression, hir)),
            update: update.as_ref().map(|expression| lower_expression(expression, hir)),
            body: lower_statements(body, hir),
        },
        HirStmt::Return(expression) => MirStmt::Return(expression.as_ref().map(|expression| lower_expression(expression, hir))),
        HirStmt::Break => MirStmt::Break,
        HirStmt::Continue => MirStmt::Continue,
        HirStmt::FunctionDecl { binding, function } => MirStmt::FunctionDecl { binding: binding.clone(), function: *function },
        HirStmt::TypeDecl => MirStmt::TypeDecl,
        HirStmt::OpaqueDecl(detail) => MirStmt::OpaqueDecl(detail.clone()),
        HirStmt::Export { inner, is_default } => MirStmt::Export { inner: Box::new(lower_statement(inner, hir)), is_default: *is_default },
    }
}

fn lower_expression(expression: &HirExpr, hir: &Hir) -> MirExpr {
    let kind = match expression {
        HirExpr::Literal(value) => MirExprKind::Literal(literal_from_ts(value).unwrap_or(MirLiteral::Undefined)),
        HirExpr::Binding(binding) => MirExprKind::Binding(binding.clone()),
        HirExpr::Binary { op, left, right } => {
            MirExprKind::Binary { op: op.clone(), left: Box::new(lower_expression(left, hir)), right: Box::new(lower_expression(right, hir)) }
        }
        HirExpr::Unary { op, operand } => MirExprKind::Unary { op: op.clone(), operand: Box::new(lower_expression(operand, hir)) },
        HirExpr::Conditional { test, then, else_ } => MirExprKind::Conditional {
            test: Box::new(lower_expression(test, hir)),
            then: Box::new(lower_expression(then, hir)),
            else_: Box::new(lower_expression(else_, hir)),
        },
        HirExpr::Assignment { target, value } => {
            let target = match target {
                HirAssignTarget::Binding(binding) => MirAssignTarget::Binding(binding.clone()),
                HirAssignTarget::Member { object, key } => MirAssignTarget::Member { object: Box::new(lower_expression(object, hir)), key: key.clone() },
                HirAssignTarget::Index { object, index } => {
                    MirAssignTarget::Index { object: Box::new(lower_expression(object, hir)), index: Box::new(lower_expression(index, hir)) }
                }
            };
            MirExprKind::Assignment { target, value: Box::new(lower_expression(value, hir)) }
        }
        HirExpr::Member { object, key } => MirExprKind::Member { object: Box::new(lower_expression(object, hir)), key: key.clone() },
        HirExpr::Index { object, index } => MirExprKind::Index { object: Box::new(lower_expression(object, hir)), index: Box::new(lower_expression(index, hir)) },
        HirExpr::Object { fields } => MirExprKind::Object { fields: fields.iter().map(|(key, value)| (key.clone(), lower_expression(value, hir))).collect() },
        HirExpr::Array { elements } => MirExprKind::Array { elements: elements.iter().map(|element| lower_expression(element, hir)).collect() },
        HirExpr::Call { target, args } => {
            let target = match target {
                HirCallTarget::Named(index) => MirCallTarget::Named(*index),
                HirCallTarget::HostLog => MirCallTarget::HostLog,
                HirCallTarget::Dynamic(callee) => MirCallTarget::Dynamic(Box::new(lower_expression(callee, hir))),
            };
            let arity = args.len() as u32;
            MirExprKind::Call { target, args: args.iter().map(|arg| lower_expression(arg, hir)).collect(), arity }
        }
        HirExpr::Unsupported(detail) => MirExprKind::Unsupported(detail.clone()),
    };
    let ty = fact_of_node(&kind, hir);
    let effect = effect_of_node(&kind);
    MirExpr { kind, ty, effect }
}

/// 降低期类型事实估计（特化期以活绑定为准重新裁决）。
fn fact_of_node(kind: &MirExprKind, hir: &Hir) -> TypeFact {
    match kind {
        MirExprKind::Literal(literal) => literal_fact(literal),
        MirExprKind::Binding(_) => TypeFact::Unknown,
        MirExprKind::Binary { op, left, right } => {
            let left = left.ty.clone();
            let right = right.ty.clone();
            match op {
                BinaryOp::Add => match (&left, &right) {
                    (TypeFact::String, TypeFact::Number) | (TypeFact::Number, TypeFact::String) | (TypeFact::String, TypeFact::String) => TypeFact::String,
                    (TypeFact::Number, TypeFact::Number) => TypeFact::Number,
                    _ => TypeFact::Unknown,
                },
                BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
                    if matches!(left, TypeFact::Number) && matches!(right, TypeFact::Number) {
                        TypeFact::Number
                    }
                    else {
                        TypeFact::Unknown
                    }
                }
                BinaryOp::StrictEq | BinaryOp::StrictNeq | BinaryOp::Eq | BinaryOp::Neq | BinaryOp::Lt | BinaryOp::Lte | BinaryOp::Gt | BinaryOp::Gte => {
                    TypeFact::Boolean
                }
                _ => TypeFact::Unknown,
            }
        }
        MirExprKind::Unary { op, operand } => match op {
            UnaryOp::Not => TypeFact::Boolean,
            UnaryOp::Neg => {
                if matches!(operand.ty, TypeFact::Number) {
                    TypeFact::Number
                }
                else {
                    TypeFact::Unknown
                }
            }
            _ => TypeFact::Unknown,
        },
        MirExprKind::Conditional { then, else_, .. } => merge_type(&then.ty, &else_.ty),
        MirExprKind::Assignment { value, .. } => value.ty.clone(),
        MirExprKind::Member { object, key } => match &object.ty {
            TypeFact::Object(fields) => key.as_ref().and_then(|key| fields.iter().find(|(name, _)| name == key).map(|(_, ty)| ty.clone())).unwrap_or(TypeFact::Unknown),
            _ => TypeFact::Unknown,
        },
        MirExprKind::Index { .. } => TypeFact::Unknown,
        MirExprKind::Object { fields } => TypeFact::Object(fields.iter().map(|(key, value)| (key.clone(), value.ty.clone())).collect()),
        MirExprKind::Array { .. } => TypeFact::Unknown,
        MirExprKind::Call { target, .. } => match target {
            MirCallTarget::Named(index) => hir.functions.get(*index).and_then(|f| f.return_type.as_ref()).map(narrow).unwrap_or(TypeFact::Unknown),
            _ => TypeFact::Unknown,
        },
        MirExprKind::Unsupported(_) => TypeFact::Unknown,
    }
}

/// 语义类别。
fn effect_of_node(kind: &MirExprKind) -> EffectClass {
    match kind {
        MirExprKind::Binary { op: BinaryOp::StrictEq | BinaryOp::StrictNeq, .. } => EffectClass::IdentitySensitive,
        MirExprKind::Call { target: MirCallTarget::HostLog, .. } => EffectClass::SideEffect,
        MirExprKind::Call { target: MirCallTarget::Dynamic(_), .. } => EffectClass::External,
        MirExprKind::Assignment { .. } => EffectClass::SideEffect,
        MirExprKind::Unsupported(_) => EffectClass::External,
        _ => EffectClass::Pure,
    }
}

/// 类型注解收窄为类型事实；联合／引用／any 一律不可判定。
pub fn narrow(ty: &TypeAnnotation) -> TypeFact {
    match ty {
        TypeAnnotation::Primitive(PrimitiveType::Number) => TypeFact::Number,
        TypeAnnotation::Primitive(PrimitiveType::String) => TypeFact::String,
        TypeAnnotation::Primitive(PrimitiveType::Boolean) => TypeFact::Boolean,
        TypeAnnotation::Object(fields) => TypeFact::Object(fields.iter().map(|(name, ty)| (name.clone(), narrow(ty))).collect()),
        _ => TypeFact::Unknown,
    }
}

/// 字面量的类型事实。
pub fn literal_fact(literal: &MirLiteral) -> TypeFact {
    match literal {
        MirLiteral::Number(_) => TypeFact::Number,
        MirLiteral::String(_) => TypeFact::String,
        MirLiteral::Bool(_) => TypeFact::Boolean,
        MirLiteral::Object(fields) => TypeFact::Object(fields.iter().map(|(key, value)| (key.clone(), literal_fact(value))).collect()),
        _ => TypeFact::Unknown,
    }
}

/// AST 字面量 → MIR 字面量。
pub fn literal_from_ts(value: &TsValue) -> Option<MirLiteral> {
    Some(match value {
        TsValue::Undefined => MirLiteral::Undefined,
        TsValue::Null => MirLiteral::Null,
        TsValue::Boolean(flag) => MirLiteral::Bool(*flag),
        TsValue::Number(number) => MirLiteral::Number(*number),
        TsValue::String(text) => MirLiteral::String(text.clone()),
        TsValue::Object(fields) => {
            let mut out = Vec::with_capacity(fields.len());
            for (key, value) in fields {
                out.push((key.clone(), literal_from_ts(value)?));
            }
            MirLiteral::Object(out)
        }
        TsValue::Array(elements) => {
            let mut out = Vec::with_capacity(elements.len());
            for element in elements {
                out.push(literal_from_ts(element)?);
            }
            MirLiteral::Array(out)
        }
        _ => return None,
    })
}

/// MIR 字面量 → 运行时值。
pub fn literal_to_value(literal: &MirLiteral) -> Value {
    match literal {
        MirLiteral::Undefined => Value::Undefined,
        MirLiteral::Null => Value::Null,
        MirLiteral::Bool(flag) => Value::Bool(*flag),
        MirLiteral::Number(number) => Value::Number(*number),
        MirLiteral::String(text) => Value::string(text.as_str()),
        MirLiteral::Object(fields) => Value::object_with(fields.iter().map(|(key, value)| (key.clone(), literal_to_value(value)))),
        MirLiteral::Array(elements) => Value::array(elements.iter().map(literal_to_value).collect()),
    }
}

/// 两类型事实合并：相等保留，否则不可判定。
pub(crate) fn merge_type(left: &TypeFact, right: &TypeFact) -> TypeFact {
    if *left == *right {
        left.clone()
    }
    else {
        TypeFact::Unknown
    }
}

/// 语句序列是否纯（不动点迭代使用：具名调用按当前 `pure` 标志判定）。
fn statements_pure(statements: &[MirStmt], program: &MirProgram, self_index: usize) -> bool {
    statements.iter().all(|statement| match statement {
        MirStmt::Expr(expression) => expression_pure(expression, program, self_index),
        MirStmt::Declare { init, .. } => init.as_ref().map(|e| expression_pure(e, program, self_index)).unwrap_or(true),
        MirStmt::Return(Some(expression)) => expression_pure(expression, program, self_index),
        MirStmt::Return(None) | MirStmt::Break | MirStmt::Continue | MirStmt::TypeDecl => true,
        MirStmt::Block(inner) => statements_pure(inner, program, self_index),
        MirStmt::If { test, then, else_ } => {
            expression_pure(test, program, self_index) && statements_pure(then, program, self_index) && statements_pure(else_, program, self_index)
        }
        MirStmt::While { test, body } => expression_pure(test, program, self_index) && statements_pure(body, program, self_index),
        MirStmt::For { init, test, update, body } => {
            init.as_ref().map(|s| pure_statement(s, program, self_index)).unwrap_or(true)
                && test.as_ref().map(|e| expression_pure(e, program, self_index)).unwrap_or(true)
                && update.as_ref().map(|e| expression_pure(e, program, self_index)).unwrap_or(true)
                && statements_pure(body, program, self_index)
        }
        // 函数声明、子集外声明与 `export` 包装不在纯子集内。
        MirStmt::FunctionDecl { .. } | MirStmt::OpaqueDecl(_) | MirStmt::Export { .. } => false,
    })
}

fn pure_statement(statement: &MirStmt, program: &MirProgram, self_index: usize) -> bool {
    match statement {
        MirStmt::Expr(expression) => expression_pure(expression, program, self_index),
        MirStmt::Declare { init, .. } => init.as_ref().map(|e| expression_pure(e, program, self_index)).unwrap_or(true),
        other => statements_pure(std::slice::from_ref(other), program, self_index),
    }
}

fn expression_pure(expression: &MirExpr, program: &MirProgram, self_index: usize) -> bool {
    match &expression.kind {
        MirExprKind::Call { target, args, .. } => {
            if !args.iter().all(|arg| expression_pure(arg, program, self_index)) {
                return false;
            }
            match target {
                MirCallTarget::Named(index) => *index != self_index && program.functions[*index].pure,
                _ => false,
            }
        }
        MirExprKind::Binary { left, right, .. } => expression_pure(left, program, self_index) && expression_pure(right, program, self_index),
        MirExprKind::Unary { operand, .. } => expression_pure(operand, program, self_index),
        MirExprKind::Conditional { test, then, else_ } => {
            expression_pure(test, program, self_index) && expression_pure(then, program, self_index) && expression_pure(else_, program, self_index)
        }
        MirExprKind::Member { object, .. } => expression_pure(object, program, self_index),
        MirExprKind::Index { object, index } => expression_pure(object, program, self_index) && expression_pure(index, program, self_index),
        MirExprKind::Object { fields } => fields.iter().all(|(_, value)| expression_pure(value, program, self_index)),
        MirExprKind::Array { elements } => elements.iter().all(|element| expression_pure(element, program, self_index)),
        // 赋值、动态调用与子集外形态不纯；其余节点按语义类别裁决。
        MirExprKind::Assignment { .. } | MirExprKind::Unsupported(_) => false,
        _ => matches!(expression.effect, EffectClass::Pure | EffectClass::IdentitySensitive),
    }
}
