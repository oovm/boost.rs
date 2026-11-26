//! 运行时值域：解释器与 TS std provider 共用的 JS 值模型。
//!
//! 与 `language.typescript.types::TsValue` 不同，本值域带引用语义
//! （对象 / 数组 / 函数按 `Rc` 指针身份比较），是解释执行与第一投影
//! 语义裁决的唯一值域；`TsValue` 只用于 AST 字面量装载。

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use language_typescript_ir::statement::Statement;

/// 一个可调用的宿主 / 用户函数。
pub enum Callable {
    /// 用户函数：源码参数、语句体与捕获作用域。
    User {
        /// 函数名（诊断用）。
        name: String,
        /// 形参名。
        params: Vec<String>,
        /// 函数体语句。
        body: Vec<Statement>,
        /// 定义处作用域（闭包）。
        closure: Scope,
    },
    /// 宿主内建函数（如 `console.log`）。
    Native {
        /// 内建名（诊断用）。
        name: String,
        /// 实现闭包：接收实参，返回结果值。
        call: Rc<dyn Fn(&[Value]) -> Result<Value, NativeError>>,
    },
}

/// 内建函数失败（由解释器包装为 [`super::RuntimeError`]）。
pub struct NativeError(pub String);

/// 变量作用域：名字 → 值。
pub type Scope = Rc<RefCell<HashMap<String, Value>>>;

/// TypeScript 运行时值。
#[derive(Clone)]
pub enum Value {
    /// `undefined`。
    Undefined,
    /// `null`。
    Null,
    /// 布尔。
    Bool(bool),
    /// 数字（IEEE-754 双精度）。
    Number(f64),
    /// 字符串。
    String(Rc<str>),
    /// 对象（引用语义）。
    Object(Rc<RefCell<HashMap<String, Value>>>),
    /// 数组（引用语义）。
    Array(Rc<RefCell<Vec<Value>>>),
    /// 函数（引用语义）。
    Function(Rc<Callable>),
}

impl Value {
    /// 构造字符串值。
    pub fn string(text: impl AsRef<str>) -> Self {
        Self::String(Rc::from(text.as_ref()))
    }

    /// 构造空对象值。
    pub fn object() -> Self {
        Self::Object(Rc::new(RefCell::new(HashMap::new())))
    }

    /// 构造对象字段值（插入序无关，按名字取）。
    pub fn object_with(fields: impl IntoIterator<Item = (String, Value)>) -> Self {
        Self::Object(Rc::new(RefCell::new(fields.into_iter().collect())))
    }

    /// 构造数组值。
    pub fn array(elements: Vec<Value>) -> Self {
        Self::Array(Rc::new(RefCell::new(elements)))
    }

    /// 构造用户函数值。
    pub fn user_function(name: impl Into<String>, params: Vec<String>, body: Vec<Statement>, closure: Scope) -> Self {
        Self::Function(Rc::new(Callable::User { name: name.into(), params, body, closure }))
    }

    /// 构造内建函数值。
    pub fn native_function(name: impl Into<String>, call: impl Fn(&[Value]) -> Result<Value, NativeError> + 'static) -> Self {
        Self::Function(Rc::new(Callable::Native { name: name.into(), call: Rc::new(call) }))
    }
}

impl std::fmt::Debug for Value {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Undefined => formatter.write_str("undefined"),
            Self::Null => formatter.write_str("null"),
            Self::Bool(value) => formatter.write_str(&value.to_string()),
            Self::Number(value) => formatter.write_str(&super::semantics::number_to_string(*value)),
            Self::String(value) => write!(formatter, "{value:?}"),
            Self::Object(_) => formatter.write_str("Object(..)"),
            Self::Array(_) => formatter.write_str("Array(..)"),
            Self::Function(_) => formatter.write_str("Function(..)"),
        }
    }
}
