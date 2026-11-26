//! JS 语义的唯一裁决处：解释器、特化期折叠与 TS std provider 共用。
//!
//! 这是「解释器作为投影输入」的具体含义——已知值在特化期直接调用这里的
//! 函数结算进产物；动态值残差到 provider 符号，由 provider 调用同一批函数，
//! 语义绝不允许出现第二份实现。

use std::rc::Rc;

use super::value::Value;

/// JS truthiness：`NaN`、`±0`、`""`、`null`、`undefined` 为假；对象为真。
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Undefined | Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => *number != 0.0 && !number.is_nan(),
        Value::String(text) => !text.is_empty(),
        Value::Object(_) | Value::Array(_) | Value::Function(_) => true,
    }
}

/// `ToNumber`：本子集的对象按 `NaN`（无 `valueOf` 协议）。
pub fn to_number(value: &Value) -> f64 {
    match value {
        Value::Undefined => f64::NAN,
        Value::Null => 0.0,
        Value::Bool(flag) => if *flag { 1.0 } else { 0.0 },
        Value::Number(number) => *number,
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                0.0
            }
            else {
                trimmed.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
        Value::Object(_) | Value::Function(_) => f64::NAN,
        Value::Array(elements) => {
            let elements = elements.borrow();
            match elements.len() {
                0 => 0.0,
                1 => to_number(&elements[0]),
                _ => f64::NAN,
            }
        }
    }
}

/// 数字 → 文本：整数不带小数点；`NaN` / `±Infinity` 用 JS 拼写；`-0` 为 `"0"`。
pub fn number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value == f64::INFINITY {
        return "Infinity".to_string();
    }
    if value == f64::NEG_INFINITY {
        return "-Infinity".to_string();
    }
    if value == 0.0 {
        // 覆盖 `-0.0`：JS `String(-0)` 是 `"0"`。
        return "0".to_string();
    }
    if value.fract() == 0.0 && value.abs() < 1e21 {
        return format!("{}", value as i64);
    }
    format!("{value}")
}

/// `ToString`。
pub fn to_string(value: &Value) -> String {
    match value {
        Value::Undefined => "undefined".to_string(),
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number_to_string(*number),
        Value::String(text) => text.to_string(),
        Value::Object(_) => "[object Object]".to_string(),
        Value::Array(elements) => elements.borrow().iter().map(to_string).collect::<Vec<_>>().join(","),
        Value::Function(callable) => match callable.as_ref() {
            super::value::Callable::User { name, .. } | super::value::Callable::Native { name, .. } => format!("function {name}"),
        },
    }
}

/// `ToPrimitive`（本子集：对象/数组按 `ToString` 折叠）。
pub fn to_primitive(value: &Value) -> Value {
    match value {
        Value::Object(_) => Value::string(to_string(value)),
        Value::Array(_) => Value::string(to_string(value)),
        other => other.clone(),
    }
}

/// JS `+`：任一侧（原始化后）为字符串则字符串拼接，否则 `ToNumber` 相加。
pub fn add(left: &Value, right: &Value) -> Value {
    let left = to_primitive(left);
    let right = to_primitive(right);
    if matches!(left, Value::String(_)) || matches!(right, Value::String(_)) {
        Value::string(format!("{}{}", to_string(&left), to_string(&right)))
    }
    else {
        Value::Number(to_number(&left) + to_number(&right))
    }
}

/// JS `===`：类型不同即假；数字按 IEEE 相等（`NaN !== NaN`、`+0 === -0`）；
/// 对象 / 数组 / 函数按 `Rc` 指针身份。
pub fn strict_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Undefined, Value::Undefined) => true,
        (Value::Null, Value::Null) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Number(a), Value::Number(b)) => a == b,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Object(a), Value::Object(b)) => Rc::ptr_eq(a, b),
        (Value::Array(a), Value::Array(b)) => Rc::ptr_eq(a, b),
        (Value::Function(a), Value::Function(b)) => Rc::ptr_eq(a, b),
        _ => false,
    }
}

/// JS `==`（本子集：`null`/`undefined` 互等；其余同类型走 `===`，
/// 跨类型按 `ToNumber` 比较）。
pub fn loose_eq(left: &Value, right: &Value) -> bool {
    let nullish = |value: &Value| matches!(value, Value::Null | Value::Undefined);
    if nullish(left) && nullish(right) {
        return true;
    }
    if nullish(left) != nullish(right) {
        return false;
    }
    let left = to_primitive(left);
    let right = to_primitive(right);
    if matches!(left, Value::String(_)) && matches!(right, Value::String(_)) {
        return strict_eq(&left, &right);
    }
    if matches!(left, Value::Bool(_)) && matches!(right, Value::Bool(_)) {
        return strict_eq(&left, &right);
    }
    to_number(&left) == to_number(&right)
}

/// `console.log` 一行：参数按 `ToString` 后用空格连接。
pub fn log_line(args: &[Value]) -> String {
    args.iter().map(to_string).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nan_and_negative_zero_are_falsy_or_zero_string() {
        assert!(!truthy(&Value::Number(f64::NAN)));
        assert!(!truthy(&Value::Number(-0.0)));
        assert_eq!(to_string(&Value::Number(-0.0)), "0");
        assert_eq!(to_string(&Value::Number(f64::NAN)), "NaN");
        assert_eq!(to_string(&Value::Number(f64::INFINITY)), "Infinity");
    }

    #[test]
    fn plus_prefers_string_concat_then_numeric() {
        assert_eq!(to_string(&add(&Value::string("1"), &Value::Number(2.0))), "12");
        assert_eq!(to_string(&add(&Value::Number(1.0), &Value::string("2"))), "12");
        assert!(strict_eq(&add(&Value::Number(1.0), &Value::Number(2.0)), &Value::Number(3.0)));
    }

    #[test]
    fn object_identity_via_strict_eq() {
        let object = Value::object();
        let clone = object.clone();
        assert!(strict_eq(&object, &clone));
        assert!(!strict_eq(&object, &Value::object()));
    }

    #[test]
    fn integers_print_without_decimal_point() {
        assert_eq!(number_to_string(42.0), "42");
        assert_eq!(number_to_string(1.5), "1.5");
    }
}
