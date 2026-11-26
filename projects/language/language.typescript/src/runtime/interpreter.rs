//! 树遍历解释器：差分基线，也是第一投影的语义来源。

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use language_typescript_ir::{statement::Statement, types::BinaryOp, Expression, Program};
use language_typescript_types::TsValue;

use super::{
    semantics,
    value::{Callable, Scope, Value},
};

/// 解释器结构化错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    /// 引用错误：名字未绑定。
    Reference(String),
    /// 类型错误：值不可调用 / 不可成员访问等。
    Type(String),
    /// 导出缺失：`call_exported` 找不到目标。
    ExportMissing(String),
    /// 字面量装载失败。
    Literal(String),
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reference(name) => write!(formatter, "ReferenceError: `{name}` 未定义"),
            Self::Type(detail) => write!(formatter, "TypeError: {detail}"),
            Self::ExportMissing(name) => write!(formatter, "导出 `{name}` 不存在或不是函数"),
            Self::Literal(detail) => write!(formatter, "字面量装载失败: {detail}"),
        }
    }
}

impl std::error::Error for RuntimeError {}

/// 语句执行的控制流信号。
enum Flow {
    Normal,
    Return(Value),
    Break,
    Continue,
}

/// TypeScript 子集解释器。
pub struct Interpreter {
    globals: Scope,
    scopes: Vec<Scope>,
    trace: Rc<RefCell<Vec<String>>>,
}

impl Interpreter {
    /// 装载并执行顶层语句，构造解释器实例。
    ///
    /// 顶层 `console.log` 写入可取的 trace（`Vec<String>`），保持调用顺序。
    pub fn new(program: &Program) -> Result<Self, RuntimeError> {
        let trace = Rc::new(RefCell::new(Vec::<String>::new()));
        let globals: Scope = Rc::new(RefCell::new(HashMap::new()));
        let mut interpreter = Self { globals: globals.clone(), scopes: vec![globals.clone()], trace: trace.clone() };
        // console 对象：log 内建按调用序写入 trace。
        let log_trace = trace.clone();
        let console = Value::object_with([(
            "log".to_string(),
            Value::native_function("log", move |args| {
                log_trace.borrow_mut().push(semantics::log_line(args));
                Ok(Value::Undefined)
            }),
        )]);
        globals.borrow_mut().insert("console".to_string(), console);
        for statement in &program.statements {
            match interpreter.exec_statement(statement)? {
                Flow::Return(value) => {
                    // 顶层 return 视为脚本完成值，忽略即可。
                    let _ = value;
                    break;
                }
                Flow::Break | Flow::Continue => return Err(RuntimeError::Type("顶层 break/continue".to_string())),
                Flow::Normal => {}
            }
        }
        Ok(interpreter)
    }

    /// 已记录的 `console.log` 行（调用序）。
    pub fn trace(&self) -> Vec<String> {
        self.trace.borrow().clone()
    }

    /// 调用导出函数。
    pub fn call_exported(&mut self, name: &str, args: &[Value]) -> Result<Value, RuntimeError> {
        let Some(value) = self.globals.borrow().get(name).cloned() else {
            return Err(RuntimeError::ExportMissing(name.to_string()));
        };
        let Value::Function(callable) = value else {
            return Err(RuntimeError::ExportMissing(format!("{name}（非函数）")));
        };
        self.invoke(&callable, args)
    }

    fn invoke(&mut self, callable: &Rc<Callable>, args: &[Value]) -> Result<Value, RuntimeError> {
        match callable.as_ref() {
            Callable::Native { call, .. } => call(args).map_err(|error| RuntimeError::Type(error.0)),
            Callable::User { params, body, closure, .. } => {
                let scope: Scope = Rc::new(RefCell::new(HashMap::new()));
                scope.borrow_mut().insert("this".to_string(), Value::Undefined);
                for (index, param) in params.iter().enumerate() {
                    let value = args.get(index).cloned().unwrap_or(Value::Undefined);
                    scope.borrow_mut().insert(param.clone(), value);
                }
                let _ = closure;
                self.scopes.push(scope.clone());
                let result = self.exec_statements(body);
                self.scopes.pop();
                match result? {
                    Flow::Return(value) => Ok(value),
                    _ => Ok(Value::Undefined),
                }
            }
        }
    }

    fn exec_statements(&mut self, statements: &[Statement]) -> Result<Flow, RuntimeError> {
        for statement in statements {
            let flow = self.exec_statement(statement)?;
            if !matches!(flow, Flow::Normal) {
                return Ok(flow);
            }
        }
        Ok(Flow::Normal)
    }

    fn exec_statement(&mut self, statement: &Statement) -> Result<Flow, RuntimeError> {
        match statement {
            Statement::Expression(expression) => {
                self.eval(expression)?;
                Ok(Flow::Normal)
            }
            Statement::VariableDeclaration { name, initializer, .. } => {
                let value = match initializer {
                    Some(expression) => self.eval(expression)?,
                    None => Value::Undefined,
                };
                self.declare(name, value);
                Ok(Flow::Normal)
            }
            Statement::Block(statements) => {
                let scope: Scope = Rc::new(RefCell::new(HashMap::new()));
                self.scopes.push(scope);
                let flow = self.exec_statements(statements);
                self.scopes.pop();
                flow
            }
            Statement::If { test, consequent, alternate } => {
                if semantics::truthy(&self.eval(test)?) {
                    self.exec_statement(consequent)
                }
                else if let Some(alternate) = alternate {
                    self.exec_statement(alternate)
                }
                else {
                    Ok(Flow::Normal)
                }
            }
            Statement::While { test, body } => {
                while semantics::truthy(&self.eval(test)?) {
                    match self.exec_statement(body)? {
                        Flow::Break => break,
                        Flow::Continue | Flow::Normal => continue,
                        flow @ Flow::Return(_) => return Ok(flow),
                    }
                }
                Ok(Flow::Normal)
            }
            Statement::For { init, test, update, body } => {
                let scope: Scope = Rc::new(RefCell::new(HashMap::new()));
                self.scopes.push(scope);
                let result = (|| -> Result<Flow, RuntimeError> {
                    if let Some(init) = init {
                        self.exec_statement(init)?;
                    }
                    loop {
                        if let Some(test) = test {
                            if !semantics::truthy(&self.eval(test)?) {
                                break;
                            }
                        }
                        match self.exec_statement(body)? {
                            Flow::Break => break,
                            Flow::Continue | Flow::Normal => {}
                            flow @ Flow::Return(_) => return Ok(flow),
                        }
                        if let Some(update) = update {
                            self.eval(update)?;
                        }
                    }
                    Ok(Flow::Normal)
                })();
                self.scopes.pop();
                result
            }
            Statement::Return(expression) => {
                let value = match expression {
                    Some(expression) => self.eval(expression)?,
                    None => Value::Undefined,
                };
                Ok(Flow::Return(value))
            }
            Statement::Break => Ok(Flow::Break),
            Statement::Continue => Ok(Flow::Continue),
            Statement::FunctionDeclaration { name, params, body, .. } => {
                let closure = self.current_scope();
                let function = Value::user_function(name.clone(), params.clone(), body.clone(), closure);
                self.declare(name, function);
                Ok(Flow::Normal)
            }
            Statement::ExportDeclaration { declaration, .. } => self.exec_statement(declaration),
            // 类型层声明：运行时无操作。
            Statement::InterfaceDeclaration { .. } | Statement::TypeAlias { .. } => Ok(Flow::Normal),
            Statement::ClassDeclaration { .. }
            | Statement::NamespaceDeclaration { .. }
            | Statement::ModuleDeclaration { .. }
            | Statement::ImportDeclaration { .. }
            | Statement::EnumDeclaration { .. } => Err(RuntimeError::Type("该声明形态不在解释子集内".to_string())),
        }
    }

    fn current_scope(&self) -> Scope {
        self.scopes.last().expect("至少存在全局作用域").clone()
    }

    fn lookup(&self, name: &str) -> Option<Value> {
        for scope in self.scopes.iter().rev() {
            if let Some(value) = scope.borrow().get(name) {
                return Some(value.clone());
            }
        }
        None
    }

    fn declare(&mut self, name: &str, value: Value) {
        self.current_scope().borrow_mut().insert(name.to_string(), value);
    }

    fn assign(&mut self, name: &str, value: Value) -> Result<(), RuntimeError> {
        for scope in self.scopes.iter().rev() {
            if scope.borrow().contains_key(name) {
                scope.borrow_mut().insert(name.to_string(), value);
                return Ok(());
            }
        }
        Err(RuntimeError::Reference(name.to_string()))
    }

    /// 求值表达式。
    pub fn eval(&mut self, expression: &Expression) -> Result<Value, RuntimeError> {
        match expression {
            Expression::Literal(value) => literal_to_value(value),
            Expression::Identifier(name) => self.lookup(name).ok_or_else(|| RuntimeError::Reference(name.clone())),
            Expression::Binary { left, op, right } => {
                let left = self.eval(left)?;
                match op {
                    BinaryOp::And => {
                        if semantics::truthy(&left) {
                            self.eval(right)
                        }
                        else {
                            Ok(left)
                        }
                    }
                    BinaryOp::Or => {
                        if semantics::truthy(&left) {
                            Ok(left)
                        }
                        else {
                            self.eval(right)
                        }
                    }
                    _ => {
                        let right = self.eval(right)?;
                        self.eval_binary(op.clone(), &left, &right)
                    }
                }
            }
            Expression::Unary { op, expr } => {
                let value = self.eval(expr)?;
                Ok(match op {
                    language_typescript_ir::types::UnaryOp::Not => Value::Bool(!semantics::truthy(&value)),
                    language_typescript_ir::types::UnaryOp::Neg => Value::Number(-semantics::to_number(&value)),
                    other => return Err(RuntimeError::Type(format!("一元操作 {other:?} 不在子集内"))),
                })
            }
            Expression::Conditional { test, consequent, alternate } => {
                if semantics::truthy(&self.eval(test)?) {
                    self.eval(consequent)
                }
                else {
                    self.eval(alternate)
                }
            }
            Expression::Assignment { left, op, right } => {
                let value = self.eval(right)?;
                let value = if *op == language_typescript_ir::types::AssignmentOp::Assign {
                    value
                }
                else {
                    return Err(RuntimeError::Type("复合赋值不在子集内".to_string()));
                };
                match left.as_ref() {
                    Expression::Identifier(name) => self.assign(name, value.clone())?,
                    Expression::Member { object, property } => {
                        let target = self.eval(object)?;
                        let key = self.member_key(property)?;
                        match &target {
                            Value::Object(fields) => {
                                fields.borrow_mut().insert(key, value.clone());
                            }
                            other => return Err(RuntimeError::Type(format!("不可对 {} 写入属性", type_name(other)))),
                        }
                    }
                    other => return Err(RuntimeError::Type(format!("赋值左侧 {other:?} 不可写"))),
                }
                Ok(value)
            }
            Expression::Member { object, property } => {
                let target = self.eval(object)?;
                let key = self.member_key(property)?;
                Ok(match &target {
                    Value::Object(fields) => fields.borrow().get(&key).cloned().unwrap_or(Value::Undefined),
                    Value::Array(elements) => {
                        if key == "length" {
                            Value::Number(elements.borrow().len() as f64)
                        }
                        else {
                            key.parse::<usize>().ok().and_then(|index| elements.borrow().get(index).cloned()).unwrap_or(Value::Undefined)
                        }
                    }
                    Value::String(text) => {
                        if key == "length" {
                            Value::Number(text.chars().count() as f64)
                        }
                        else {
                            Value::Undefined
                        }
                    }
                    other => return Err(RuntimeError::Type(format!("不可读取 {} 的属性 `{key}`", type_name(other)))),
                })
            }
            Expression::Index { object, index } => {
                let target = self.eval(object)?;
                let key = self.eval(index)?;
                Ok(match (&target, &key) {
                    (Value::Object(fields), Value::String(name)) => fields.borrow().get(&name.to_string()).cloned().unwrap_or(Value::Undefined),
                    (Value::Array(elements), Value::Number(number)) => {
                        elements.borrow().get(*number as usize).cloned().unwrap_or(Value::Undefined)
                    }
                    (Value::String(text), Value::Number(number)) => text
                        .chars()
                        .nth(*number as usize)
                        .map(|ch| Value::string(ch.to_string()))
                        .unwrap_or(Value::Undefined),
                    _ => return Err(RuntimeError::Type("下标访问形态不受支持".to_string())),
                })
            }
            Expression::Object(fields) => {
                let mut map = HashMap::new();
                for (key, value) in fields {
                    let evaluated = self.eval(value)?;
                    map.insert(key.clone(), evaluated);
                }
                Ok(Value::Object(Rc::new(RefCell::new(map))))
            }
            Expression::Array(elements) => {
                let mut values = Vec::with_capacity(elements.len());
                for element in elements {
                    values.push(self.eval(element)?);
                }
                Ok(Value::Array(Rc::new(RefCell::new(values))))
            }
            Expression::Call { callee, args } => {
                // 方法调用：`console.log(...)` 等成员 callee。
                let mut evaluated = Vec::with_capacity(args.len());
                for arg in args {
                    evaluated.push(self.eval(arg)?);
                }
                let target = self.eval(callee)?;
                match target {
                    Value::Function(callable) => self.invoke(&callable, &evaluated),
                    other => Err(RuntimeError::Type(format!("{} 不可调用", type_name(&other)))),
                }
            }
            Expression::Function { .. } | Expression::ArrowFunction { .. } => Err(RuntimeError::Type("函数表达式不在子集内".to_string())),
        }
    }

    fn member_key(&mut self, property: &Expression) -> Result<String, RuntimeError> {
        match property {
            Expression::Identifier(name) => Ok(name.clone()),
            Expression::Literal(TsValue::String(text)) => Ok(text.clone()),
            other => Err(RuntimeError::Type(format!("成员名形态不受支持: {other:?}"))),
        }
    }

    fn eval_binary(&mut self, op: BinaryOp, left: &Value, right: &Value) -> Result<Value, RuntimeError> {
        Ok(match op {
            BinaryOp::Add => semantics::add(left, right),
            BinaryOp::Sub => Value::Number(semantics::to_number(left) - semantics::to_number(right)),
            BinaryOp::Mul => Value::Number(semantics::to_number(left) * semantics::to_number(right)),
            BinaryOp::Div => Value::Number(semantics::to_number(left) / semantics::to_number(right)),
            BinaryOp::Mod => Value::Number(semantics::to_number(left) % semantics::to_number(right)),
            BinaryOp::StrictEq => Value::Bool(semantics::strict_eq(left, right)),
            BinaryOp::StrictNeq => Value::Bool(!semantics::strict_eq(left, right)),
            BinaryOp::Eq => Value::Bool(semantics::loose_eq(left, right)),
            BinaryOp::Neq => Value::Bool(!semantics::loose_eq(left, right)),
            BinaryOp::Lt => Value::Bool(compare(left, right).is_some_and(|order| order == std::cmp::Ordering::Less)),
            BinaryOp::Lte => Value::Bool(compare(left, right).is_some_and(|order| order != std::cmp::Ordering::Greater)),
            BinaryOp::Gt => Value::Bool(compare(left, right).is_some_and(|order| order == std::cmp::Ordering::Greater)),
            BinaryOp::Gte => Value::Bool(compare(left, right).is_some_and(|order| order != std::cmp::Ordering::Less)),
            other => return Err(RuntimeError::Type(format!("二元操作 {other:?} 不在子集内"))),
        })
    }
}

/// JS 关系比较：双字符串按码点序，否则 `ToNumber`；`NaN` 参与恒假。
fn compare(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    if let (Value::String(a), Value::String(b)) = (left, right) {
        return Some(a.cmp(b));
    }
    let a = semantics::to_number(left);
    let b = semantics::to_number(right);
    if a.is_nan() || b.is_nan() {
        return None;
    }
    a.partial_cmp(&b)
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Undefined => "undefined",
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Object(_) => "object",
        Value::Array(_) => "array",
        Value::Function(_) => "function",
    }
}

/// AST 字面量装载：`TsValue` → 运行时 `Value`。
pub fn literal_to_value(value: &TsValue) -> Result<Value, RuntimeError> {
    Ok(match value {
        TsValue::Undefined => Value::Undefined,
        TsValue::Null => Value::Null,
        TsValue::Boolean(flag) => Value::Bool(*flag),
        TsValue::Number(number) => Value::Number(*number),
        TsValue::String(text) => Value::string(text.as_str()),
        TsValue::Array(elements) => {
            let mut values = Vec::with_capacity(elements.len());
            for element in elements {
                values.push(literal_to_value(element)?);
            }
            Value::Array(Rc::new(RefCell::new(values)))
        }
        TsValue::Object(fields) => {
            let mut map = HashMap::new();
            for (key, field) in fields {
                map.insert(key.clone(), literal_to_value(field)?);
            }
            Value::Object(Rc::new(RefCell::new(map)))
        }
        other => return Err(RuntimeError::Literal(format!("{other:?}"))),
    })
}
