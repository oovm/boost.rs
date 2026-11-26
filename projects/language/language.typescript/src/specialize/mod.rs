//! TS 侧特化分析：已知／未知传播、静态类型与「解释器作为投影输入」。
//!
//! 框架合同（分支残差形状、副作用账本）来自 `compiler.specialize`，本模块
//! 只做 TS 消费者落地：`const` 字面量初始化为已知；形参全为未知（guest
//! 输入）；类型注解给出静态类型；纯函数且实参全已知时用解释器结算。

use std::collections::{HashMap, HashSet};

use compiler_specialize::{SpecializeError, Stage};
use language_typescript_ir::{statement::Statement, types::PrimitiveType, Expression, Program, TypeAnnotation};
use language_typescript_types::TsValue;

use crate::runtime::{semantics, Interpreter, Value};

/// 重导出框架合同，消费者只需依赖本模块即可拿到约束接口。
pub use compiler_specialize::{branch_residual_shape, check_branch_residual};

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

/// 静态类型（语言无关框架只谈阶段，类型形状归 TS 侧）。
#[derive(Debug, Clone)]
pub enum StaticType {
    /// `number`。
    Number,
    /// `string`。
    String,
    /// `boolean`。
    Boolean,
    /// 对象：字段名与字段类型（声明序）。
    Object(Vec<(String, TypeAnnotation)>),
    /// 不可判定（联合、引用、any 等）。
    Unknown,
}

impl StaticType {
    /// 是否可与另一个静态类型共同裁决某个操作（双方都确定）。
    pub fn is_decided(&self) -> bool {
        !matches!(self, Self::Unknown)
    }
}

// `TypeAnnotation` 无 `PartialEq`：对象按字段名集合判等（槽位冻结只看名字）。
impl PartialEq for StaticType {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Number, Self::Number) | (Self::String, Self::String) | (Self::Boolean, Self::Boolean) | (Self::Unknown, Self::Unknown) => true,
            (Self::Object(a), Self::Object(b)) => a.len() == b.len() && a.iter().all(|(name, _)| b.iter().any(|(other, _)| other == name)),
            _ => false,
        }
    }
}

/// 把类型注解收窄为静态类型；联合／引用／any 一律不可判定。
pub fn narrow(ty: &TypeAnnotation) -> StaticType {
    match ty {
        TypeAnnotation::Primitive(PrimitiveType::Number) => StaticType::Number,
        TypeAnnotation::Primitive(PrimitiveType::String) => StaticType::String,
        TypeAnnotation::Primitive(PrimitiveType::Boolean) => StaticType::Boolean,
        TypeAnnotation::Object(fields) => StaticType::Object(fields.clone()),
        _ => StaticType::Unknown,
    }
}

/// 字面量的静态类型。
pub fn literal_static_type(value: &TsValue) -> StaticType {
    match value {
        TsValue::Number(_) => StaticType::Number,
        TsValue::String(_) => StaticType::String,
        TsValue::Boolean(_) => StaticType::Boolean,
        TsValue::Object(fields) => {
            StaticType::Object(fields.iter().map(|(key, value)| (key.clone(), widen_literal(value))).collect())
        }
        _ => StaticType::Unknown,
    }
}

fn widen_literal(value: &TsValue) -> TypeAnnotation {
    match value {
        TsValue::Number(_) => TypeAnnotation::Primitive(PrimitiveType::Number),
        TsValue::String(_) => TypeAnnotation::Primitive(PrimitiveType::String),
        TsValue::Boolean(_) => TypeAnnotation::Primitive(PrimitiveType::Boolean),
        _ => TypeAnnotation::Any,
    }
}

/// 一个已声明函数的冻结事实。
#[derive(Debug, Clone)]
pub struct FunctionInfo {
    /// 函数名。
    pub name: String,
    /// 形参名。
    pub params: Vec<String>,
    /// 形参类型注解（与 `params` 等长）。
    pub param_types: Vec<Option<TypeAnnotation>>,
    /// 返回类型注解。
    pub return_type: Option<TypeAnnotation>,
    /// 函数体。
    pub body: Vec<Statement>,
    /// 是否导出（导出即产物入口）。
    pub exported: bool,
}

/// 程序级特化事实。
#[derive(Debug, Default)]
pub struct ProgramFacts {
    /// 模块级 `const` 字面量：已知值 + 静态类型。
    pub module_consts: HashMap<String, (Value, StaticType)>,
    /// 函数事实（按声明序）。
    pub functions: Vec<FunctionInfo>,
    /// 名字 → 索引。
    pub function_index: HashMap<String, usize>,
}

/// 扫描顶层，收集已知常量与函数事实。
pub fn analyze(program: &Program) -> Result<ProgramFacts, SpecializeFailure> {
    let mut facts = ProgramFacts::default();
    for statement in &program.statements {
        let (statement, exported) = match statement {
            Statement::ExportDeclaration { declaration, is_default: false } => (declaration.as_ref(), true),
            Statement::ExportDeclaration { is_default: true, .. } => {
                return Err(SpecializeFailure::UnsupportedTopLevel { construct: "default export".to_string() })
            }
            other => (other, false),
        };
        match statement {
            Statement::VariableDeclaration { name, initializer, is_const: true, .. } => {
                let expression = initializer.as_ref().ok_or_else(|| SpecializeFailure::UnsupportedTopLevel {
                    construct: format!("无初始化的模块级 `const {name}`"),
                })?;
                let value = eval_known_literal(expression).ok_or_else(|| SpecializeFailure::UnsupportedTopLevel {
                    construct: format!("模块级 `const {name}` 的初始化不是字面量"),
                })?;
                let ty = literal_static_type_of(&value);
                facts.module_consts.insert(name.clone(), (value, ty));
            }
            Statement::FunctionDeclaration { name, params, param_types, return_type, body, .. } => {
                if param_types.len() != params.len() {
                    return Err(SpecializeFailure::Unsupported {
                        construct: format!("函数 `{name}`"),
                        detail: "形参注解长度与形参数不一致".to_string(),
                    });
                }
                let index = facts.functions.len();
                facts.function_index.insert(name.clone(), index);
                facts.functions.push(FunctionInfo {
                    name: name.clone(),
                    params: params.clone(),
                    param_types: param_types.clone(),
                    return_type: return_type.clone(),
                    body: body.clone(),
                    exported,
                });
            }
            Statement::InterfaceDeclaration { .. } | Statement::TypeAlias { .. } => {}
            other => {
                return Err(SpecializeFailure::UnsupportedTopLevel { construct: describe(other) });
            }
        }
    }
    Ok(facts)
}

fn literal_static_type_of(value: &Value) -> StaticType {
    match value {
        Value::Number(_) => StaticType::Number,
        Value::String(_) => StaticType::String,
        Value::Bool(_) => StaticType::Boolean,
        _ => StaticType::Unknown,
    }
}

fn describe(statement: &Statement) -> String {
    match statement {
        Statement::VariableDeclaration { is_const: false, .. } => "模块级 `let` / `var`".to_string(),
        Statement::ClassDeclaration { .. } => "class 声明".to_string(),
        Statement::ImportDeclaration { .. } => "import 声明".to_string(),
        Statement::EnumDeclaration { .. } => "enum 声明".to_string(),
        _ => "该顶层构造".to_string(),
    }
}

/// 特化期已知表达式求值：只接受字面量与已知常量的纯组合。
///
/// 这是「已知值在特化期走解释器的语义函数」的入口：任何一步都调用
/// `runtime::semantics`，与解释器、provider 同源。
pub fn eval_known(expression: &Expression, consts: &HashMap<String, (Value, StaticType)>) -> Stage<Value> {
    match expression {
        Expression::Literal(value) => match literal_to_value(value) {
            Some(value) => Stage::Known(value),
            None => Stage::Unknown,
        },
        Expression::Identifier(name) => consts.get(name).map(|(value, _)| Stage::Known(value.clone())).unwrap_or(Stage::Unknown),
        Expression::Binary { left, op, right } => {
            let left = match eval_known(left, consts) {
                Stage::Known(value) => value,
                Stage::Unknown => return Stage::Unknown,
            };
            let right = match eval_known(right, consts) {
                Stage::Known(value) => value,
                Stage::Unknown => return Stage::Unknown,
            };
            eval_known_binary(op, &left, &right)
        }
        Expression::Unary { op, expr } => {
            let value = match eval_known(expr, consts) {
                Stage::Known(value) => value,
                Stage::Unknown => return Stage::Unknown,
            };
            match op {
                language_typescript_ir::types::UnaryOp::Not => Stage::Known(Value::Bool(!semantics::truthy(&value))),
                language_typescript_ir::types::UnaryOp::Neg => Stage::Known(Value::Number(-semantics::to_number(&value))),
                _ => Stage::Unknown,
            }
        }
        Expression::Conditional { test, consequent, alternate } => {
            match eval_known(test, consts) {
                Stage::Known(value) => {
                    if semantics::truthy(&value) {
                        eval_known(consequent, consts)
                    }
                    else {
                        eval_known(alternate, consts)
                    }
                }
                Stage::Unknown => Stage::Unknown,
            }
        }
        _ => Stage::Unknown,
    }
}

fn eval_known_binary(op: &language_typescript_ir::types::BinaryOp, left: &Value, right: &Value) -> Stage<Value> {
    use language_typescript_ir::types::BinaryOp;
    Stage::Known(match op {
        BinaryOp::Add => semantics::add(left, right),
        BinaryOp::Sub => Value::Number(semantics::to_number(left) - semantics::to_number(right)),
        BinaryOp::Mul => Value::Number(semantics::to_number(left) * semantics::to_number(right)),
        BinaryOp::Div => Value::Number(semantics::to_number(left) / semantics::to_number(right)),
        BinaryOp::Mod => Value::Number(semantics::to_number(left) % semantics::to_number(right)),
        BinaryOp::StrictEq => Value::Bool(semantics::strict_eq(left, right)),
        BinaryOp::StrictNeq => Value::Bool(!semantics::strict_eq(left, right)),
        BinaryOp::Eq => Value::Bool(semantics::loose_eq(left, right)),
        BinaryOp::Neq => Value::Bool(!semantics::loose_eq(left, right)),
        BinaryOp::And if semantics::truthy(left) => right.clone(),
        BinaryOp::And => left.clone(),
        BinaryOp::Or if semantics::truthy(left) => left.clone(),
        BinaryOp::Or => right.clone(),
        BinaryOp::Lt | BinaryOp::Lte | BinaryOp::Gt | BinaryOp::Gte => {
            let (Some(a), Some(b)) = (number_order(left), number_order(right)) else { return Stage::Unknown };
            match op {
                BinaryOp::Lt => Value::Bool(a < b),
                BinaryOp::Lte => Value::Bool(a <= b),
                BinaryOp::Gt => Value::Bool(a > b),
                _ => Value::Bool(a >= b),
            }
        }
        _ => return Stage::Unknown,
    })
}

fn number_order(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) if !number.is_nan() => Some(*number),
        _ => None,
    }
}

/// 字面量表达式 → 运行时值（仅标量与纯对象/数组字面量）。
pub fn eval_known_literal(expression: &Expression) -> Option<Value> {
    match expression {
        Expression::Literal(value) => literal_to_value(value),
        _ => None,
    }
}

/// `TsValue` 字面量 → 运行时值。
pub fn literal_to_value(value: &TsValue) -> Option<Value> {
    Some(match value {
        TsValue::Undefined => Value::Undefined,
        TsValue::Null => Value::Null,
        TsValue::Boolean(flag) => Value::Bool(*flag),
        TsValue::Number(number) => Value::Number(*number),
        TsValue::String(text) => Value::string(text.as_str()),
        TsValue::Object(fields) => {
            let mut map = HashMap::new();
            for (key, field) in fields {
                map.insert(key.clone(), literal_to_value(field)?);
            }
            Value::object_with(map)
        }
        TsValue::Array(elements) => {
            let mut values = Vec::with_capacity(elements.len());
            for element in elements {
                values.push(literal_to_value(element)?);
            }
            Value::array(values)
        }
        _ => return None,
    })
}

/// 运行时值 → `TsValue` 字面量（标量；对象/数组/函数不可回装）。
pub fn value_to_literal(value: &Value) -> Option<TsValue> {
    Some(match value {
        Value::Undefined => TsValue::Undefined,
        Value::Null => TsValue::Null,
        Value::Bool(flag) => TsValue::Boolean(*flag),
        Value::Number(number) => TsValue::Number(*number),
        Value::String(text) => TsValue::String(text.to_string()),
        _ => return None,
    })
}

/// 函数体是否纯（无宿主效应、无外部调用、无递归环）。
pub fn is_pure(info: &FunctionInfo, facts: &ProgramFacts, visiting: &mut HashSet<String>) -> bool {
    if !visiting.insert(info.name.clone()) {
        return false;
    }
    let result = statements_pure(&info.body, facts, visiting);
    visiting.remove(&info.name);
    result
}

fn statements_pure(statements: &[Statement], facts: &ProgramFacts, visiting: &mut HashSet<String>) -> bool {
    statements.iter().all(|statement| match statement {
        Statement::Expression(expression) => expression_pure(expression, facts, visiting),
        Statement::Return(Some(expression)) => expression_pure(expression, facts, visiting),
        Statement::Return(None) | Statement::Break | Statement::Continue => true,
        Statement::VariableDeclaration { initializer: Some(expression), .. } => expression_pure(expression, facts, visiting),
        Statement::VariableDeclaration { .. } => true,
        Statement::Block(inner) => statements_pure(inner, facts, visiting),
        Statement::If { test, consequent, alternate } => {
            expression_pure(test, facts, visiting)
                && statements_pure(statement_body(consequent), facts, visiting)
                && alternate.as_ref().map(|s| statements_pure(statement_body(s), facts, visiting)).unwrap_or(true)
        }
        Statement::While { test, body } => expression_pure(test, facts, visiting) && statements_pure(statement_body(body), facts, visiting),
        Statement::For { init, test, update, body } => {
            init.as_ref().map(|s| statements_pure(statement_body(s), facts, visiting)).unwrap_or(true)
                && test.as_ref().map(|e| expression_pure(e, facts, visiting)).unwrap_or(true)
                && update.as_ref().map(|e| expression_pure(e, facts, visiting)).unwrap_or(true)
                && statements_pure(statement_body(body), facts, visiting)
        }
        _ => false,
    })
}

/// 单语句体（`if`/`while` 的 `Box<Statement>`）视作语句序列。
fn statement_body(statement: &Statement) -> &[Statement] {
    match statement {
        Statement::Block(inner) => inner,
        _ => std::slice::from_ref(statement),
    }
}

fn expression_pure(expression: &Expression, facts: &ProgramFacts, visiting: &mut HashSet<String>) -> bool {
    match expression {
        Expression::Call { callee, args } => {
            let Expression::Identifier(name) = callee.as_ref() else { return false };
            match facts.function_index.get(name) {
                Some(index) => args.iter().all(|arg| expression_pure(arg, facts, visiting)) && is_pure(&facts.functions[*index], facts, visiting),
                None => false,
            }
        }
        Expression::Binary { left, right, .. } => expression_pure(left, facts, visiting) && expression_pure(right, facts, visiting),
        Expression::Unary { expr, .. } => expression_pure(expr, facts, visiting),
        Expression::Conditional { test, consequent, alternate } => {
            expression_pure(test, facts, visiting) && expression_pure(consequent, facts, visiting) && expression_pure(alternate, facts, visiting)
        }
        Expression::Assignment { .. } => false,
        _ => true,
    }
}

/// 解释器分派结算：纯函数 + 全已知实参 → 特化期结果值。
///
/// 这就是「解释器作为投影输入」：内联求值直接复用 [`Interpreter`]，
/// 分派本身从产物中消失。
pub fn inline_eval(info: &FunctionInfo, args: &[Value], facts: &ProgramFacts) -> Option<Value> {
    let mut statements = Vec::new();
    for (name, (value, _)) in &facts.module_consts {
        let literal = value_to_literal(value)?;
        statements.push(Statement::VariableDeclaration {
            name: name.clone(),
            ty: None,
            initializer: Some(Box::new(Expression::Literal(literal))),
            is_const: true,
        });
    }
    statements.push(Statement::FunctionDeclaration {
        name: info.name.clone(),
        type_params: Vec::new(),
        params: info.params.clone(),
        param_types: info.param_types.clone(),
        return_type: info.return_type.clone(),
        body: info.body.clone(),
        decorators: Vec::new(),
    });
    let mut interpreter = Interpreter::new(&Program { statements }).ok()?;
    interpreter.call_exported(&info.name, args).ok()
}
