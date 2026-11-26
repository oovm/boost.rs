//! HIR：作用域树、声明 → 绑定身份、函数表。
//!
//! 本层只做一件事：把 AST 的语法名字换成语义身份。每个声明获得一个绑定
//! 身份（模块直接声明进全局 arena，函数内 / 顶层块内声明进帧槽），每个
//! 名字引用解析为 [`BindingRef`]，函数表按声明序冻结。降低是**全函数**：
//! 不报错、不裁决语义；解析不到的名字保留 [`BindingRef::Unbound`]，由
//! MIR 消费者（解释器 / 特化）决定失败形态。

use std::collections::HashMap;

use language_typescript_ir::{statement::Statement, types::BinaryOp, types::UnaryOp, Expression, Program};
use language_typescript_types::TsValue;

/// 绑定身份：当前帧槽、模块 arena 下标或内建。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingRef {
    /// 当前帧槽位（函数形参占 `0..arity`；顶层块作用域使用模块帧槽）。
    Local(u32),
    /// 模块绑定 arena 下标。
    Global(u32),
    /// `this` 内建（仅函数体内有效）。
    This,
    /// `console` 内建宿主对象。
    HostConsole,
    /// 未绑定名字（保留诊断身份）。
    Unbound(String),
}

/// 一个函数的冻结事实（声明序进函数表）。
#[derive(Debug, Clone)]
pub struct HirFunction {
    /// 函数名。
    pub name: String,
    /// 形参名（占槽 `0..arity`）。
    pub params: Vec<String>,
    /// 形参类型注解（与 `params` 等长）。
    pub param_types: Vec<Option<language_typescript_ir::types::TypeAnnotation>>,
    /// 返回类型注解。
    pub return_type: Option<language_typescript_ir::types::TypeAnnotation>,
    /// 槽名表（形参在前，局部声明按分配序）。
    pub local_names: Vec<String>,
    /// 帧槽总数（形参 + 局部声明；不含特化临时槽）。
    pub slot_count: u32,
    /// 函数体。
    pub body: Vec<HirStmt>,
    /// 是否导出。
    pub exported: bool,
}

/// 模块绑定事实。
#[derive(Debug, Clone)]
pub struct HirGlobal {
    /// 名字。
    pub name: String,
    /// 是否 `const` 声明（函数声明绑定视为常量）。
    pub is_const: bool,
    /// 函数声明绑定 → 函数表下标。
    pub function: Option<usize>,
    /// 是否导出。
    pub exported: bool,
}

/// HIR 程序：作用域树结算后的模块。
#[derive(Debug, Clone)]
pub struct Hir {
    /// 函数表（顶层声明序在前，嵌套声明按发现序追加）。
    pub functions: Vec<HirFunction>,
    /// 模块绑定 arena。
    pub globals: Vec<HirGlobal>,
    /// 模块绑定名 → arena 下标（首次声明）。
    pub global_index: HashMap<String, usize>,
    /// 顶层模块帧槽数（顶层块作用域声明）。
    pub module_slot_count: u32,
    /// 顶层模块帧槽名表。
    pub module_local_names: Vec<String>,
    /// 顶层语句。
    pub top: Vec<HirStmt>,
}

/// HIR 语句。
#[derive(Debug, Clone)]
pub enum HirStmt {
    /// 表达式语句。
    Expr(HirExpr),
    /// 变量声明：绑定身份 + 注解 + 初始化。
    Declare {
        /// 绑定身份。
        binding: BindingRef,
        /// 声明类型注解。
        ty: Option<language_typescript_ir::types::TypeAnnotation>,
        /// 初始化表达式。
        init: Option<HirExpr>,
        /// 是否 `const`。
        is_const: bool,
    },
    /// 块。
    Block(Vec<HirStmt>),
    /// `if` / `else`。
    If {
        /// 条件。
        test: HirExpr,
        /// then 分支。
        then: Vec<HirStmt>,
        /// else 分支。
        else_: Vec<HirStmt>,
    },
    /// `while`。
    While {
        /// 条件。
        test: HirExpr,
        /// 循环体。
        body: Vec<HirStmt>,
    },
    /// 三段 `for`。
    For {
        /// 初始化语句。
        init: Option<Box<HirStmt>>,
        /// 条件。
        test: Option<HirExpr>,
        /// 更新表达式。
        update: Option<HirExpr>,
        /// 循环体。
        body: Vec<HirStmt>,
    },
    /// `return`。
    Return(Option<HirExpr>),
    /// `break`。
    Break,
    /// `continue`。
    Continue,
    /// 函数声明：绑定身份 + 函数表下标。
    FunctionDecl {
        /// 绑定身份。
        binding: BindingRef,
        /// 函数表下标。
        function: usize,
    },
    /// 类型层声明（`interface` / `type`）：运行时无操作。
    TypeDecl,
    /// 解释子集之外的声明形态（class / namespace / module / import / enum）。
    OpaqueDecl(String),
    /// `export` 包装。
    Export {
        /// 被导出语句。
        inner: Box<HirStmt>,
        /// 是否默认导出。
        is_default: bool,
    },
}

/// HIR 表达式。
#[derive(Debug, Clone)]
pub enum HirExpr {
    /// 字面量。
    Literal(TsValue),
    /// 绑定引用。
    Binding(BindingRef),
    /// 二元表达式。
    Binary {
        /// 操作符。
        op: BinaryOp,
        /// 左操作数。
        left: Box<HirExpr>,
        /// 右操作数。
        right: Box<HirExpr>,
    },
    /// 一元表达式。
    Unary {
        /// 操作符。
        op: UnaryOp,
        /// 操作数。
        operand: Box<HirExpr>,
    },
    /// 三元条件。
    Conditional {
        /// 条件。
        test: Box<HirExpr>,
        /// 真分支。
        then: Box<HirExpr>,
        /// 假分支。
        else_: Box<HirExpr>,
    },
    /// 赋值表达式（完成值 = 右值）。
    Assignment {
        /// 赋值目标。
        target: HirAssignTarget,
        /// 右值。
        value: Box<HirExpr>,
    },
    /// 成员访问（`key` 为静态成员名；动态成员名为 `None`）。
    Member {
        /// 对象。
        object: Box<HirExpr>,
        /// 静态成员名。
        key: Option<String>,
    },
    /// 下标访问。
    Index {
        /// 对象。
        object: Box<HirExpr>,
        /// 下标。
        index: Box<HirExpr>,
    },
    /// 对象字面量。
    Object {
        /// 字段（声明序）。
        fields: Vec<(String, HirExpr)>,
    },
    /// 数组字面量。
    Array {
        /// 元素。
        elements: Vec<HirExpr>,
    },
    /// 调用。
    Call {
        /// 调用目标。
        target: HirCallTarget,
        /// 实参。
        args: Vec<HirExpr>,
    },
    /// 解释子集之外的表达式形态（函数 / 箭头函数表达式）。
    Unsupported(String),
}

/// 赋值目标。
#[derive(Debug, Clone)]
pub enum HirAssignTarget {
    /// 绑定。
    Binding(BindingRef),
    /// 成员写入。
    Member {
        /// 对象。
        object: Box<HirExpr>,
        /// 静态成员名。
        key: Option<String>,
    },
    /// 下标写入（解释子集外，保留失败形态）。
    Index {
        /// 对象。
        object: Box<HirExpr>,
        /// 下标。
        index: Box<HirExpr>,
    },
}

/// 调用目标。
#[derive(Debug, Clone)]
pub enum HirCallTarget {
    /// 具名函数（模块函数绑定 → 函数表下标）。
    Named(usize),
    /// `console.log`。
    HostLog,
    /// 动态 callee。
    Dynamic(Box<HirExpr>),
}

/// AST → HIR 降低（全函数，不报错）。
pub fn lower(program: &Program) -> Hir {
    let mut functions: Vec<HirFunction> = Vec::new();
    let mut globals: Vec<HirGlobal> = Vec::new();
    let mut global_index: HashMap<String, usize> = HashMap::new();
    // 函数体降低工作表（AST 体 → 第三遍降低后替换）。
    let mut pending: Vec<(usize, Vec<Statement>)> = Vec::new();
    // 第一遍：模块直接声明登记（全局 arena + 顶层函数表，声明序）。
    for statement in &program.statements {
        let (inner, exported) = unwrap_export(statement);
        match inner {
            Statement::VariableDeclaration { name, is_const, .. } => {
                let id = globals.len() as u32;
                if !global_index.contains_key(name) {
                    global_index.insert(name.clone(), id as usize);
                    globals.push(HirGlobal { name: name.clone(), is_const: *is_const, function: None, exported });
                }
            }
            Statement::FunctionDeclaration { name, params, param_types, return_type, body, .. } => {
                let function = functions.len();
                functions.push(HirFunction {
                    name: name.clone(),
                    params: params.clone(),
                    param_types: param_types.clone(),
                    return_type: return_type.clone(),
                    local_names: params.clone(),
                    slot_count: params.len() as u32,
                    body: Vec::new(),
                    exported,
                });
                pending.push((function, body.clone()));
                if !global_index.contains_key(name) {
                    global_index.insert(name.clone(), globals.len());
                    globals.push(HirGlobal { name: name.clone(), is_const: true, function: Some(function), exported });
                }
            }
            _ => {}
        }
    }
    // 第二遍：顶层语句（模块作用域；顶层块内函数声明进工作表）。
    let mut lowerer = Lowerer {
        globals: &globals,
        global_index: &global_index,
        functions: &mut functions,
        scopes: Vec::new(),
        next_slot: 0,
        local_names: Vec::new(),
        in_function: false,
        nested: Vec::new(),
    };
    let top = lowerer.statements(&program.statements);
    let module_slot_count = lowerer.next_slot;
    let module_local_names = std::mem::take(&mut lowerer.local_names);
    let block_functions = std::mem::take(&mut lowerer.nested);
    drop(lowerer);
    // 第三遍：函数体（工作表；嵌套声明按发现序追加并继续降低）。
    pending.extend(block_functions);
    while let Some((index, body)) = pending.pop() {
        let params = functions[index].params.clone();
        let slot_count = functions[index].slot_count;
        let local_names = std::mem::take(&mut functions[index].local_names);
        let mut lowerer = Lowerer {
            globals: &globals,
            global_index: &global_index,
            functions: &mut functions,
            scopes: Vec::new(),
            next_slot: slot_count,
            local_names,
            in_function: true,
            nested: Vec::new(),
        };
        lowerer.enter_root(&params);
        lowerer.prescan(&body);
        let lowered = lowerer.statements(&body);
        let next_slot = lowerer.next_slot;
        let names = std::mem::take(&mut lowerer.local_names);
        let nested = std::mem::take(&mut lowerer.nested);
        drop(lowerer);
        functions[index].body = lowered;
        functions[index].slot_count = next_slot;
        functions[index].local_names = names;
        pending.extend(nested);
    }
    Hir { functions, globals, global_index, module_slot_count, module_local_names, top }
}

/// 降低器状态。
struct Lowerer<'a> {
    globals: &'a [HirGlobal],
    global_index: &'a HashMap<String, usize>,
    functions: &'a mut Vec<HirFunction>,
    /// 局部作用域栈（名字 → 绑定身份）。
    scopes: Vec<HashMap<String, BindingRef>>,
    /// 当前帧的下一个可用槽。
    next_slot: u32,
    /// 当前帧槽名表。
    local_names: Vec<String>,
    /// 是否在函数体内（决定 `this` 是否可见）。
    in_function: bool,
    /// 待降低的嵌套函数工作表。
    nested: Vec<(usize, Vec<Statement>)>,
}

impl<'a> Lowerer<'a> {
    /// 函数作用域根：形参占槽 `0..arity`。
    fn enter_root(&mut self, params: &[String]) {
        let mut root = HashMap::new();
        for (slot, param) in params.iter().enumerate() {
            root.insert(param.clone(), BindingRef::Local(slot as u32));
        }
        self.scopes.push(root);
    }

    fn prescan(&mut self, statements: &[Statement]) {
        for statement in statements {
            if let Statement::VariableDeclaration { name, .. } | Statement::FunctionDeclaration { name, .. } = statement {
                if !self.current_scope_contains(name) {
                    self.fresh_slot(name);
                }
            }
        }
    }

    fn current_scope_contains(&self, name: &str) -> bool {
        self.scopes.last().is_some_and(|scope| scope.contains_key(name))
    }

    fn fresh_slot(&mut self, name: &str) -> BindingRef {
        let slot = self.next_slot;
        self.next_slot += 1;
        self.local_names.push(name.to_string());
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), BindingRef::Local(slot));
        }
        BindingRef::Local(slot)
    }

    /// 声明绑定：模块直接声明的身份在登记遍已定（全局 arena）；函数作用域
    /// 内声明分配局部槽，函数声明同时登记进函数表。
    fn declare(&mut self, name: &str, is_function: bool, params: &[String], param_types: &[Option<language_typescript_ir::types::TypeAnnotation>], return_type: &Option<language_typescript_ir::types::TypeAnnotation>, body: &[Statement], exported: bool) -> (BindingRef, Option<usize>) {
        if self.scopes.is_empty() {
            let id = self.global_index[name] as u32;
            return (BindingRef::Global(id), self.globals[id as usize].function);
        }
        let function = if is_function { Some(self.register_function(name, params, param_types, return_type, body, exported)) } else { None };
        let binding = if self.current_scope_contains(name) {
            self.scopes.last().expect("刚检查过").get(name).expect("存在").clone()
        }
        else {
            self.fresh_slot(name)
        };
        (binding, function)
    }

    fn register_function(&mut self, name: &str, params: &[String], param_types: &[Option<language_typescript_ir::types::TypeAnnotation>], return_type: &Option<language_typescript_ir::types::TypeAnnotation>, body: &[Statement], exported: bool) -> usize {
        let index = self.functions.len();
        self.functions.push(HirFunction {
            name: name.to_string(),
            params: params.to_vec(),
            param_types: param_types.to_vec(),
            return_type: return_type.clone(),
            local_names: params.to_vec(),
            slot_count: params.len() as u32,
            body: Vec::new(),
            exported,
        });
        self.nested.push((index, body.to_vec()));
        index
    }

    fn resolve(&self, name: &str) -> BindingRef {
        for scope in self.scopes.iter().rev() {
            if let Some(binding) = scope.get(name) {
                return binding.clone();
            }
        }
        if let Some(id) = self.global_index.get(name) {
            return BindingRef::Global(*id as u32);
        }
        if self.in_function && name == "this" {
            return BindingRef::This;
        }
        if name == "console" {
            return BindingRef::HostConsole;
        }
        BindingRef::Unbound(name.to_string())
    }

    fn statements(&mut self, statements: &[Statement]) -> Vec<HirStmt> {
        statements.iter().map(|statement| self.statement(statement)).collect()
    }

    fn statement(&mut self, statement: &Statement) -> HirStmt {
        match statement {
            Statement::Expression(expression) => HirStmt::Expr(self.expression(expression)),
            Statement::VariableDeclaration { name, ty, initializer, is_const } => {
                let (binding, _) = self.declare(name, false, &[], &[], &None, &[], false);
                let init = initializer.as_ref().map(|expression| self.expression(expression));
                HirStmt::Declare { binding, ty: ty.clone(), init, is_const: *is_const }
            }
            Statement::Block(statements) => {
                self.scopes.push(HashMap::new());
                self.prescan(statements);
                let lowered = self.statements(statements);
                self.scopes.pop();
                HirStmt::Block(lowered)
            }
            Statement::If { test, consequent, alternate } => {
                let test = self.expression(test);
                let then = self.body_statements(consequent);
                let else_ = match alternate.as_deref() {
                    Some(statement) => self.body_statements(statement),
                    None => Vec::new(),
                };
                HirStmt::If { test, then, else_ }
            }
            Statement::While { test, body } => {
                let test = self.expression(test);
                let body = self.body_statements(body);
                HirStmt::While { test, body }
            }
            Statement::For { init, test, update, body } => {
                self.scopes.push(HashMap::new());
                if let Some(init) = init {
                    if let Statement::VariableDeclaration { name, .. } | Statement::FunctionDeclaration { name, .. } = init.as_ref() {
                        if !self.current_scope_contains(name) {
                            self.fresh_slot(name);
                        }
                    }
                }
                let lowered_init = init.as_ref().map(|statement| Box::new(self.statement(statement)));
                let lowered_test = test.as_ref().map(|expression| self.expression(expression));
                let lowered_update = update.as_ref().map(|expression| self.expression(expression));
                let lowered_body = self.body_statements(body);
                self.scopes.pop();
                HirStmt::For { init: lowered_init, test: lowered_test, update: lowered_update, body: lowered_body }
            }
            Statement::Return(expression) => HirStmt::Return(expression.as_ref().map(|expression| self.expression(expression))),
            Statement::Break => HirStmt::Break,
            Statement::Continue => HirStmt::Continue,
            Statement::FunctionDeclaration { name, params, param_types, return_type, body, .. } => {
                let (binding, function) = self.declare(name, true, params, param_types, return_type, body, false);
                HirStmt::FunctionDecl { binding, function: function.expect("函数声明必有下标") }
            }
            Statement::InterfaceDeclaration { .. } | Statement::TypeAlias { .. } => HirStmt::TypeDecl,
            Statement::ClassDeclaration { .. } => HirStmt::OpaqueDecl("class 声明".to_string()),
            Statement::NamespaceDeclaration { .. } => HirStmt::OpaqueDecl("namespace 声明".to_string()),
            Statement::ModuleDeclaration { .. } => HirStmt::OpaqueDecl("module 声明".to_string()),
            Statement::ImportDeclaration { .. } => HirStmt::OpaqueDecl("import 声明".to_string()),
            Statement::EnumDeclaration { .. } => HirStmt::OpaqueDecl("enum 声明".to_string()),
            Statement::ExportDeclaration { declaration, is_default } => {
                let inner = self.statement(declaration);
                HirStmt::Export { inner: Box::new(inner), is_default: *is_default }
            }
        }
    }

    /// 单语句体（`if` / `while` / `for` 的体）：`Block` 自带作用域，其余内联。
    fn body_statements(&mut self, statement: &Statement) -> Vec<HirStmt> {
        match statement {
            Statement::Block(statements) => {
                self.scopes.push(HashMap::new());
                self.prescan(statements);
                let lowered = self.statements(statements);
                self.scopes.pop();
                lowered
            }
            other => vec![self.statement(other)],
        }
    }

    fn expression(&mut self, expression: &Expression) -> HirExpr {
        match expression {
            Expression::Literal(value) => HirExpr::Literal(value.clone()),
            Expression::Identifier(name) => HirExpr::Binding(self.resolve(name)),
            Expression::Binary { left, op, right } => {
                let left = Box::new(self.expression(left));
                let right = Box::new(self.expression(right));
                HirExpr::Binary { op: op.clone(), left, right }
            }
            Expression::Unary { op, expr } => HirExpr::Unary { op: op.clone(), operand: Box::new(self.expression(expr)) },
            Expression::Conditional { test, consequent, alternate } => {
                let test = Box::new(self.expression(test));
                let then = Box::new(self.expression(consequent));
                let else_ = Box::new(self.expression(alternate));
                HirExpr::Conditional { test, then, else_ }
            }
            Expression::Assignment { left, op: _, right } => {
                let target = match left.as_ref() {
                    Expression::Identifier(name) => HirAssignTarget::Binding(self.resolve(name)),
                    Expression::Member { object, property } => {
                        let object = Box::new(self.expression(object));
                        HirAssignTarget::Member { object, key: member_key(property) }
                    }
                    Expression::Index { object, index } => {
                        let object = Box::new(self.expression(object));
                        let index = Box::new(self.expression(index));
                        HirAssignTarget::Index { object, index }
                    }
                    other => HirAssignTarget::Binding(BindingRef::Unbound(format!("{other:?}"))),
                };
                let value = Box::new(self.expression(right));
                HirExpr::Assignment { target, value }
            }
            Expression::Member { object, property } => {
                let object = Box::new(self.expression(object));
                HirExpr::Member { object, key: member_key(property) }
            }
            Expression::Index { object, index } => {
                let object = Box::new(self.expression(object));
                let index = Box::new(self.expression(index));
                HirExpr::Index { object, index }
            }
            Expression::Object(fields) => {
                let fields = fields.iter().map(|(key, value)| (key.clone(), self.expression(value))).collect();
                HirExpr::Object { fields }
            }
            Expression::Array(elements) => {
                let elements = elements.iter().map(|element| self.expression(element)).collect();
                HirExpr::Array { elements }
            }
            Expression::Call { callee, args } => {
                let target = match callee.as_ref() {
                    Expression::Identifier(name) => match self.resolve(name) {
                        BindingRef::Global(id) if self.globals[id as usize].function.is_some() => {
                            HirCallTarget::Named(self.globals[id as usize].function.expect("刚检查过"))
                        }
                        binding => HirCallTarget::Dynamic(Box::new(HirExpr::Binding(binding))),
                    },
                    Expression::Member { object, property } => {
                        let key = member_key(property);
                        if matches!(object.as_ref(), Expression::Identifier(name) if name == "console") && key.as_deref() == Some("log") {
                            // 只有未被遮蔽的 `console` 才是宿主日志。
                            if self.resolve("console") == BindingRef::HostConsole {
                                HirCallTarget::HostLog
                            }
                            else {
                                let object = Box::new(self.expression(object));
                                HirCallTarget::Dynamic(Box::new(HirExpr::Member { object, key }))
                            }
                        }
                        else {
                            let object = Box::new(self.expression(object));
                            HirCallTarget::Dynamic(Box::new(HirExpr::Member { object, key }))
                        }
                    }
                    other => HirCallTarget::Dynamic(Box::new(self.expression(other))),
                };
                let args = args.iter().map(|arg| self.expression(arg)).collect();
                HirExpr::Call { target, args }
            }
            Expression::Function { .. } => HirExpr::Unsupported("函数表达式".to_string()),
            Expression::ArrowFunction { .. } => HirExpr::Unsupported("箭头函数".to_string()),
        }
    }
}

/// `export` 包装展开：返回（内层语句，是否导出）。
fn unwrap_export(statement: &Statement) -> (&Statement, bool) {
    match statement {
        Statement::ExportDeclaration { declaration, .. } => (declaration.as_ref(), true),
        other => (other, false),
    }
}

/// 成员键：标识符与字符串字面量为静态名，其余动态。
fn member_key(property: &Expression) -> Option<String> {
    match property {
        Expression::Identifier(name) => Some(name.clone()),
        Expression::Literal(TsValue::String(text)) => Some(text.clone()),
        _ => None,
    }
}
