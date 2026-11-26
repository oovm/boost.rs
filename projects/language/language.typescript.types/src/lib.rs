#![warn(missing_docs)]
#![doc = "TypeScript AST 字面量值域。\n\n本 crate 只保留有生产消费者的值模型：`language.typescript` 前端 / 解释器与\n`language.typescript.ir` 表达式求值使用的 `TsValue` 标量与结构变体。类型层\n计算值域（条件类型 / 映射类型 / 模板字面量 / 推断结果等）经审计零消费者，\n已作为死代码删除。"]

use std::{
    collections::HashMap,
    rc::Rc,
};

/// TypeScript 值类型枚举：AST 字面量的值域。
pub enum TsValue {
    /// 未定义
    Undefined,
    /// 空值
    Null,
    /// 布尔值
    Boolean(bool),
    /// 数字
    Number(f64),
    /// 字符串
    String(String),
    /// 对象
    Object(HashMap<String, TsValue>),
    /// 数组
    Array(Vec<TsValue>),
    /// 函数
    Function(Rc<dyn Fn(&[TsValue]) -> TsValue>),
}

impl PartialEq for TsValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (TsValue::Undefined, TsValue::Undefined) => true,
            (TsValue::Null, TsValue::Null) => true,
            (TsValue::Boolean(a), TsValue::Boolean(b)) => a == b,
            (TsValue::Number(a), TsValue::Number(b)) => a.to_bits() == b.to_bits(),
            (TsValue::String(a), TsValue::String(b)) => a == b,
            (TsValue::Object(a), TsValue::Object(b)) => a == b,
            (TsValue::Array(a), TsValue::Array(b)) => a == b,
            (TsValue::Function(_), TsValue::Function(_)) => false,
            _ => false,
        }
    }
}

impl Eq for TsValue {}

impl Clone for TsValue {
    fn clone(&self) -> Self {
        match self {
            TsValue::Undefined => TsValue::Undefined,
            TsValue::Null => TsValue::Null,
            TsValue::Boolean(b) => TsValue::Boolean(*b),
            TsValue::Number(n) => TsValue::Number(*n),
            TsValue::String(s) => TsValue::String(s.clone()),
            TsValue::Object(props) => TsValue::Object(props.clone()),
            TsValue::Array(arr) => TsValue::Array(arr.clone()),
            TsValue::Function(f) => TsValue::Function(Rc::clone(f)),
        }
    }
}

impl std::fmt::Debug for TsValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TsValue::Undefined => write!(f, "Undefined"),
            TsValue::Null => write!(f, "Null"),
            TsValue::Boolean(_) => write!(f, "Boolean"),
            TsValue::Number(_) => write!(f, "Number"),
            TsValue::String(_) => write!(f, "String"),
            TsValue::Object(_) => write!(f, "Object"),
            TsValue::Array(_) => write!(f, "Array"),
            TsValue::Function(_) => write!(f, "Function"),
        }
    }
}

impl TsValue {
    /// 检查是否为未定义
    pub fn is_undefined(&self) -> bool {
        matches!(self, TsValue::Undefined)
    }

    /// 检查是否为空值
    pub fn is_null(&self) -> bool {
        matches!(self, TsValue::Null)
    }

    /// 检查是否为布尔值
    pub fn is_boolean(&self) -> bool {
        matches!(self, TsValue::Boolean(_))
    }

    /// 检查是否为数字
    pub fn is_number(&self) -> bool {
        matches!(self, TsValue::Number(_))
    }

    /// 检查是否为字符串
    pub fn is_string(&self) -> bool {
        matches!(self, TsValue::String(_))
    }

    /// 检查是否为对象
    pub fn is_object(&self) -> bool {
        matches!(self, TsValue::Object(_))
    }

    /// 检查是否为数组
    pub fn is_array(&self) -> bool {
        matches!(self, TsValue::Array(_))
    }

    /// 检查是否为函数
    pub fn is_function(&self) -> bool {
        matches!(self, TsValue::Function(_))
    }

    /// 转换为布尔值
    pub fn to_boolean(&self) -> bool {
        match self {
            TsValue::Undefined => false,
            TsValue::Null => false,
            TsValue::Boolean(b) => *b,
            TsValue::Number(n) => *n != 0.0 && !n.is_nan(),
            TsValue::String(s) => !s.is_empty(),
            TsValue::Object(_) => true,
            TsValue::Array(_) => true,
            TsValue::Function(_) => true,
        }
    }

    /// 转换为数字
    pub fn to_number(&self) -> f64 {
        match self {
            TsValue::Undefined => f64::NAN,
            TsValue::Null => 0.0,
            TsValue::Boolean(b) => {
                if *b {
                    1.0
                }
                else {
                    0.0
                }
            }
            TsValue::Number(n) => *n,
            TsValue::String(s) => s.parse().unwrap_or(f64::NAN),
            TsValue::Object(_) => f64::NAN,
            TsValue::Array(arr) => {
                if arr.is_empty() {
                    0.0
                }
                else {
                    f64::NAN
                }
            }
            TsValue::Function(_) => f64::NAN,
        }
    }

    /// 转换为字符串
    pub fn to_string(&self) -> String {
        match self {
            TsValue::Undefined => "undefined".to_string(),
            TsValue::Null => "null".to_string(),
            TsValue::Boolean(b) => b.to_string(),
            TsValue::Number(n) => n.to_string(),
            TsValue::String(s) => s.clone(),
            TsValue::Object(_) => "[object Object]".to_string(),
            TsValue::Array(arr) => {
                let elements: Vec<String> = arr.iter().map(|v| v.to_string()).collect();
                format!("[{}]", elements.join(", "))
            }
            TsValue::Function(_) => "[Function]".to_string(),
        }
    }
}
