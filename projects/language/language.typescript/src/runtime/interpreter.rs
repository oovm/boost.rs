//! MIR 执行解释器：差分基线，也是第一投影的语义来源。
//!
//! 解释器与特化从**同一 MIR** 分派：本模块只执行 [`crate::mir`] 节点，
//! 不再遍历 AST。`Interpreter::new` 只是便利入口，内部先完成
//! AST → HIR → MIR 降低再执行。语义裁决唯一来源仍是 [`semantics`]。

use std::{cell::RefCell, rc::Rc};

use language_typescript_ir::types::BinaryOp;

use crate::hir::BindingRef;
use crate::mir::{self, literal_to_value, MirAssignTarget, MirCallTarget, MirExpr, MirExprKind, MirProgram, MirStmt};
use crate::runtime::value::{Callable, Value};

use super::semantics;

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

/// 调用帧：槽位按 [`BindingRef::Local`] 索引，`None` = 尚未声明。
struct Frame {
    slots: Vec<Option<Value>>,
    names: Vec<String>,
}

impl Frame {
    fn new(slots: u32, names: Vec<String>) -> Self {
        Self { slots: vec![None; slots as usize], names }
    }
}

/// TypeScript 子集解释器（MIR 执行）。
pub struct Interpreter {
    program: Rc<MirProgram>,
    /// 调用帧栈（栈顶为当前帧；栈底为模块帧）。
    frames: Vec<Frame>,
    /// 模块绑定帧（按 [`BindingRef::Global`] 索引）。
    globals: Vec<Option<Value>>,
    /// `console` 宿主对象（log 按调用序写入 trace）。
    console: Value,
    trace: Rc<RefCell<Vec<String>>>,
}

impl Interpreter {
    /// 装载并执行顶层语句，构造解释器实例（AST 便利入口）。
    ///
    /// 顶层 `console.log` 写入可取的 trace（`Vec<String>`），保持调用顺序。
    pub fn new(program: &language_typescript_ir::Program) -> Result<Self, RuntimeError> {
        let hir = crate::hir::lower(program);
        let mir = mir::lower(&hir);
        Self::from_mir(&mir)
    }

    /// 从 MIR 程序装载并执行顶层语句。
    pub fn from_mir(program: &MirProgram) -> Result<Self, RuntimeError> {
        let program = Rc::new(program.clone());
        let trace = Rc::new(RefCell::new(Vec::<String>::new()));
        // console 对象：log 内建按调用序写入 trace。
        let log_trace = trace.clone();
        let console = Value::object_with([(
            "log".to_string(),
            Value::native_function("log", move |args| {
                log_trace.borrow_mut().push(semantics::log_line(args));
                Ok(Value::Undefined)
            }),
        )]);
        let frames = vec![Frame::new(program.module_slot_count, program.module_local_names.clone())];
        let globals = vec![None; program.globals.len()];
        let mut interpreter = Self { program, frames, globals, console, trace };
        let top = interpreter.program.top.clone();
        for statement in &top {
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
        let Some(id) = self.program.global_index.get(name).copied() else {
            return Err(RuntimeError::ExportMissing(name.to_string()));
        };
        let Some(value) = self.globals.get(id).and_then(|slot| slot.clone()) else {
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
            Callable::User { function, .. } => {
                let function = function.clone();
                let mut frame = Frame::new(function.slot_count, function.local_names.clone());
                for index in 0..function.params.len() {
                    frame.slots[index] = Some(args.get(index).cloned().unwrap_or(Value::Undefined));
                }
                self.frames.push(frame);
                let result = self.exec_statements(&function.body);
                self.frames.pop();
                match result? {
                    Flow::Return(value) => Ok(value),
                    _ => Ok(Value::Undefined),
                }
            }
        }
    }

    fn exec_statements(&mut self, statements: &[MirStmt]) -> Result<Flow, RuntimeError> {
        for statement in statements {
            let flow = self.exec_statement(statement)?;
            if !matches!(flow, Flow::Normal) {
                return Ok(flow);
            }
        }
        Ok(Flow::Normal)
    }

    fn exec_statement(&mut self, statement: &MirStmt) -> Result<Flow, RuntimeError> {
        match statement {
            MirStmt::Expr(expression) => {
                self.eval(expression)?;
                Ok(Flow::Normal)
            }
            MirStmt::Declare { binding, init, .. } => {
                let value = match init {
                    Some(expression) => self.eval(expression)?,
                    None => Value::Undefined,
                };
                self.declare(binding, value);
                Ok(Flow::Normal)
            }
            MirStmt::Block(statements) => self.exec_statements(statements),
            MirStmt::If { test, then, else_ } => {
                if semantics::truthy(&self.eval(test)?) {
                    self.exec_statements(then)
                }
                else {
                    self.exec_statements(else_)
                }
            }
            MirStmt::While { test, body } => {
                while semantics::truthy(&self.eval(test)?) {
                    match self.exec_statements(body)? {
                        Flow::Break => break,
                        Flow::Continue | Flow::Normal => continue,
                        flow @ Flow::Return(_) => return Ok(flow),
                    }
                }
                Ok(Flow::Normal)
            }
            MirStmt::For { init, test, update, body } => {
                if let Some(init) = init {
                    self.exec_statement(init)?;
                }
                loop {
                    if let Some(test) = test {
                        if !semantics::truthy(&self.eval(test)?) {
                            break;
                        }
                    }
                    match self.exec_statements(body)? {
                        Flow::Break => break,
                        Flow::Continue | Flow::Normal => {}
                        flow @ Flow::Return(_) => return Ok(flow),
                    }
                    if let Some(update) = update {
                        self.eval(update)?;
                    }
                }
                Ok(Flow::Normal)
            }
            MirStmt::Return(expression) => {
                let value = match expression {
                    Some(expression) => self.eval(expression)?,
                    None => Value::Undefined,
                };
                Ok(Flow::Return(value))
            }
            MirStmt::Break => Ok(Flow::Break),
            MirStmt::Continue => Ok(Flow::Continue),
            MirStmt::FunctionDecl { binding, function } => {
                let function = self.program.functions[*function].clone();
                let name = function.name.clone();
                self.declare(binding, Value::Function(Rc::new(Callable::User { name, function })));
                Ok(Flow::Normal)
            }
            MirStmt::TypeDecl => Ok(Flow::Normal),
            MirStmt::OpaqueDecl(_) => Err(RuntimeError::Type("该声明形态不在解释子集内".to_string())),
            MirStmt::Export { inner, .. } => self.exec_statement(inner),
        }
    }

    /// 声明绑定：写入帧槽 / 全局槽。
    fn declare(&mut self, binding: &BindingRef, value: Value) {
        match binding {
            BindingRef::Local(slot) => {
                if let Some(frame) = self.frames.last_mut() {
                    if let Some(entry) = frame.slots.get_mut(*slot as usize) {
                        *entry = Some(value);
                    }
                }
            }
            BindingRef::Global(id) => {
                if let Some(entry) = self.globals.get_mut(*id as usize) {
                    *entry = Some(value);
                }
            }
            BindingRef::This | BindingRef::HostConsole | BindingRef::Unbound(_) => {}
        }
    }

    /// 读取绑定。
    fn lookup(&self, binding: &BindingRef) -> Result<Value, RuntimeError> {
        match binding {
            BindingRef::Local(slot) => {
                let frame = self.frames.last().expect("至少存在模块帧");
                match frame.slots.get(*slot as usize).and_then(|entry| entry.clone()) {
                    Some(value) => Ok(value),
                    None => Err(RuntimeError::Reference(self.local_name(*slot))),
                }
            }
            BindingRef::Global(id) => {
                let name = self.program.globals.get(*id as usize).map(|global| global.name.clone()).unwrap_or_default();
                match self.globals.get(*id as usize).and_then(|entry| entry.clone()) {
                    Some(value) => Ok(value),
                    None => Err(RuntimeError::Reference(name)),
                }
            }
            BindingRef::This => Ok(Value::Undefined),
            BindingRef::HostConsole => Ok(self.console.clone()),
            BindingRef::Unbound(name) => Err(RuntimeError::Reference(name.clone())),
        }
    }

    /// 赋值绑定。
    fn assign(&mut self, binding: &BindingRef, value: Value) -> Result<(), RuntimeError> {
        match binding {
            BindingRef::Local(slot) => {
                let name = self.local_name(*slot);
                let frame = self.frames.last_mut().expect("至少存在模块帧");
                match frame.slots.get_mut(*slot as usize) {
                    Some(entry @ Some(_)) => {
                        *entry = Some(value);
                        Ok(())
                    }
                    _ => Err(RuntimeError::Reference(name)),
                }
            }
            BindingRef::Global(id) => {
                let name = self.program.globals.get(*id as usize).map(|global| global.name.clone()).unwrap_or_default();
                match self.globals.get_mut(*id as usize) {
                    Some(entry @ Some(_)) => {
                        *entry = Some(value);
                        Ok(())
                    }
                    _ => Err(RuntimeError::Reference(name)),
                }
            }
            BindingRef::This => Err(RuntimeError::Type("不能给 `this` 赋值".to_string())),
            BindingRef::HostConsole => Err(RuntimeError::Type("不能给 `console` 赋值".to_string())),
            BindingRef::Unbound(name) => Err(RuntimeError::Reference(name.clone())),
        }
    }

    fn local_name(&self, slot: u32) -> String {
        self.frames
            .last()
            .and_then(|frame| frame.names.get(slot as usize).cloned())
            .unwrap_or_else(|| format!("slot{slot}"))
    }

    /// 求值 MIR 表达式。
    fn eval(&mut self, expression: &MirExpr) -> Result<Value, RuntimeError> {
        match &expression.kind {
            MirExprKind::Literal(literal) => Ok(literal_to_value(literal)),
            MirExprKind::Binding(binding) => self.lookup(binding),
            MirExprKind::Binary { op, left, right } => {
                let left = self.eval(left)?;
                match op {
                    // 短路：`&&` / `||` 返回操作数本身（JS 语义）。
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
                        eval_binary(op.clone(), &left, &right)
                    }
                }
            }
            MirExprKind::Unary { op, operand } => {
                use language_typescript_ir::types::UnaryOp;
                let value = self.eval(operand)?;
                match op {
                    UnaryOp::Not => Ok(Value::Bool(!semantics::truthy(&value))),
                    UnaryOp::Neg => Ok(Value::Number(-semantics::to_number(&value))),
                    _ => Err(RuntimeError::Type(format!("一元操作 {op:?} 不在子集内"))),
                }
            }
            MirExprKind::Conditional { test, then, else_ } => {
                if semantics::truthy(&self.eval(test)?) {
                    self.eval(then)
                }
                else {
                    self.eval(else_)
                }
            }
            MirExprKind::Assignment { target, value } => {
                let value = self.eval(value)?;
                match target {
                    MirAssignTarget::Binding(binding) => self.assign(binding, value.clone())?,
                    MirAssignTarget::Member { object, key } => {
                        let key = key.clone().ok_or_else(|| RuntimeError::Type("成员名形态不支持".to_string()))?;
                        let target = self.eval(object)?;
                        match &target {
                            Value::Object(fields) => fields.borrow_mut().insert(key, value.clone()),
                            other => return Err(RuntimeError::Type(format!("{} 不可写属性", type_name(other)))),
                        };
                    }
                    MirAssignTarget::Index { .. } => return Err(RuntimeError::Type("赋值左侧不可写".to_string())),
                }
                // 赋值表达式的值即右值（JS 语义）。
                Ok(value)
            }
            MirExprKind::Member { object, key } => {
                let key = key.clone().ok_or_else(|| RuntimeError::Type("成员名形态不支持".to_string()))?;
                let target = self.eval(object)?;
                match &target {
                    Value::Object(fields) => Ok(fields.borrow().get(&key).cloned().unwrap_or(Value::Undefined)),
                    Value::Array(elements) => {
                        let elements = elements.borrow();
                        if key == "length" {
                            Ok(Value::Number(elements.len() as f64))
                        }
                        else if let Ok(index) = key.parse::<usize>() {
                            Ok(elements.get(index).cloned().unwrap_or(Value::Undefined))
                        }
                        else {
                            Ok(Value::Undefined)
                        }
                    }
                    Value::String(text) => {
                        if key == "length" {
                            Ok(Value::Number(text.chars().count() as f64))
                        }
                        else {
                            Ok(Value::Undefined)
                        }
                    }
                    _ => Err(RuntimeError::Type(format!("{} 不可成员访问", type_name(&target)))),
                }
            }
            MirExprKind::Index { object, index } => {
                let target = self.eval(object)?;
                let key = self.eval(index)?;
                match &target {
                    Value::Object(fields) => Ok(fields.borrow().get(&semantics::to_string(&key)).cloned().unwrap_or(Value::Undefined)),
                    Value::Array(elements) => {
                        let number = semantics::to_number(&key);
                        if number.is_nan() || number < 0.0 {
                            return Ok(Value::Undefined);
                        }
                        Ok(elements.borrow().get(number as usize).cloned().unwrap_or(Value::Undefined))
                    }
                    Value::String(text) => {
                        let number = semantics::to_number(&key);
                        if number.is_nan() || number < 0.0 {
                            return Ok(Value::Undefined);
                        }
                        Ok(text.chars().nth(number as usize).map(|ch| Value::string(ch.to_string())).unwrap_or(Value::Undefined))
                    }
                    _ => Err(RuntimeError::Type(format!("{} 不可下标访问", type_name(&target)))),
                }
            }
            MirExprKind::Object { fields } => {
                let mut object = Vec::with_capacity(fields.len());
                for (key, value) in fields {
                    object.push((key.clone(), self.eval(value)?));
                }
                Ok(Value::object_with(object))
            }
            MirExprKind::Array { elements } => {
                let mut array = Vec::with_capacity(elements.len());
                for element in elements {
                    array.push(self.eval(element)?);
                }
                Ok(Value::array(array))
            }
            MirExprKind::Call { target, args, .. } => {
                let mut evaluated = Vec::with_capacity(args.len());
                for arg in args {
                    evaluated.push(self.eval(arg)?);
                }
                match target {
                    MirCallTarget::Named(index) => {
                        let function = self.program.functions[*index].clone();
                        let name = function.name.clone();
                        let callable = Rc::new(Callable::User { name, function });
                        self.invoke(&callable, &evaluated)
                    }
                    MirCallTarget::HostLog => {
                        self.trace.borrow_mut().push(semantics::log_line(&evaluated));
                        Ok(Value::Undefined)
                    }
                    MirCallTarget::Dynamic(callee) => {
                        let target = self.eval(callee)?;
                        match target {
                            Value::Function(callable) => self.invoke(&callable, &evaluated),
                            other => Err(RuntimeError::Type(format!("{} 不可调用", type_name(&other)))),
                        }
                    }
                }
            }
            MirExprKind::Unsupported(detail) => Err(RuntimeError::Type(format!("{detail}不在解释子集内"))),
        }
    }
}

/// 二元求值（非短路），语义全部走 [`semantics`]。
fn eval_binary(op: BinaryOp, left: &Value, right: &Value) -> Result<Value, RuntimeError> {
    use BinaryOp::*;
    Ok(match op {
        Add => semantics::add(left, right),
        Sub => Value::Number(semantics::to_number(left) - semantics::to_number(right)),
        Mul => Value::Number(semantics::to_number(left) * semantics::to_number(right)),
        Div => Value::Number(semantics::to_number(left) / semantics::to_number(right)),
        Mod => Value::Number(semantics::to_number(left) % semantics::to_number(right)),
        StrictEq => Value::Bool(semantics::strict_eq(left, right)),
        StrictNeq => Value::Bool(!semantics::strict_eq(left, right)),
        Eq => Value::Bool(semantics::loose_eq(left, right)),
        Neq => Value::Bool(!semantics::loose_eq(left, right)),
        Lt => Value::Bool(compare(left, right)? < 0.0),
        Lte => Value::Bool(compare(left, right)? <= 0.0),
        Gt => Value::Bool(compare(left, right)? > 0.0),
        Gte => Value::Bool(compare(left, right)? >= 0.0),
        other => return Err(RuntimeError::Type(format!("二元操作 {other:?} 不在子集内"))),
    })
}

fn compare(left: &Value, right: &Value) -> Result<f64, RuntimeError> {
    let a = semantics::to_number(left);
    let b = semantics::to_number(right);
    if a.is_nan() || b.is_nan() {
        return Err(RuntimeError::Type("NaN 不参与关系比较".to_string()));
    }
    Ok(a - b)
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
