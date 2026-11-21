//! SQL 网关的 JSON 列编解码。
//!
//! 底层没有 json/jsonb 标量：`json` / `jsonb` 只是网关的类型拼写，落库的是运行时
//! 结构化值 `any` —— [`Value::Row`] / [`Value::List`] 与 JSON 标量，直接复用行记录
//! 的类型标签编码（`database_formats::yy::row::encode_tagged`）。因此写入只在解析
//! 客户端文本时做一次结构化，读取与比较都走二进制值；文本只在 SQL 驱动边界由
//! [`text_of`] 渲染一次。

use std::collections::BTreeMap;
use std::fmt::Write as _;

use database_formats::yy::row::{decode_tagged, encode_tagged};
use database_types::Value;

use crate::sql::{SqlErrorKind, SqlFrontendError};

fn mismatch(message: impl Into<String>) -> SqlFrontendError {
    SqlFrontendError::new(SqlErrorKind::TypeMismatch, message)
}

/// 把客户端 JSON 文本解析成结构化 `any` 值并编码成存储载荷。
///
/// 文本参数与 `jsonb` 的二进制参数都可能出现；二进制参数按 PostgreSQL 约定是
/// `0x01` 版本字节 + JSON 文本，这里统一按 UTF-8 JSON 文本解析。
pub(crate) fn encode(text: &str) -> Result<Vec<u8>, SqlFrontendError> {
    let parsed: serde_json::Value = serde_json::from_str(text).map_err(|error| mismatch(format!("invalid JSON value: {error}")))?;
    let value = value_of(&parsed)?;
    encode_tagged(&value).map_err(|error| mismatch(format!("cannot store JSON value: {error}")))
}

/// 把 `jsonb` 二进制参数的负载还原为文本，再走同一解析路径。
pub(crate) fn encode_bytes(bytes: &[u8]) -> Result<Vec<u8>, SqlFrontendError> {
    let bytes = match bytes.first() {
        // PostgreSQL `jsonb` 二进制格式：1 字节版本号，后接 JSON 文本。
        Some(1) => &bytes[1..],
        _ => bytes,
    };
    let text = std::str::from_utf8(bytes).map_err(|_| mismatch("JSON parameter is not UTF-8"))?;
    encode(text)
}

/// 把存储载荷解码回结构化值。
pub(crate) fn decode(bytes: &[u8]) -> Result<Value, SqlFrontendError> {
    decode_tagged(bytes).map_err(|error| mismatch(format!("cannot decode stored JSON value: {error}")))
}

/// 表示层：把结构化值渲染成紧凑 JSON 文本。
pub(crate) fn text_of(value: &Value) -> Result<String, SqlFrontendError> {
    let mut out = String::new();
    render(value, &mut out)?;
    Ok(out)
}

/// 表示层：解码存储载荷并渲染为紧凑 JSON 文本。
pub(crate) fn text_from_bytes(bytes: &[u8]) -> Result<String, SqlFrontendError> {
    text_of(&decode(bytes)?)
}

fn value_of(json: &serde_json::Value) -> Result<Value, SqlFrontendError> {
    Ok(match json {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(flag) => Value::Bool(*flag),
        serde_json::Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                Value::I64(integer)
            } else if let Some(integer) = number.as_u64() {
                Value::U64(integer)
            } else {
                let real = number.as_f64().ok_or_else(|| mismatch("JSON number has no finite representation"))?;
                if !real.is_finite() {
                    return Err(mismatch("JSON number has no finite representation"));
                }
                Value::F64(real)
            }
        }
        serde_json::Value::String(text) => Value::Text(text.clone()),
        serde_json::Value::Array(items) => {
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                values.push(value_of(item)?);
            }
            Value::List(values)
        }
        serde_json::Value::Object(fields) => {
            let mut row = BTreeMap::new();
            for (key, item) in fields {
                row.insert(key.clone(), value_of(item)?);
            }
            Value::Row(row)
        }
    })
}

fn render(value: &Value, out: &mut String) -> Result<(), SqlFrontendError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(false) => out.push_str("false"),
        Value::Bool(true) => out.push_str("true"),
        Value::I64(integer) => {
            let _ = write!(out, "{integer}");
        }
        Value::U64(integer) => {
            let _ = write!(out, "{integer}");
        }
        Value::F64(real) => {
            if !real.is_finite() {
                return Err(mismatch("stored JSON number is not finite"));
            }
            let _ = write!(out, "{real}");
        }
        Value::Text(text) => escape_into(text, out),
        Value::List(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                render(item, out)?;
            }
            out.push(']');
        }
        Value::Row(fields) => {
            out.push('{');
            for (index, (key, item)) in fields.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                escape_into(key, out);
                out.push(':');
                render(item, out)?;
            }
            out.push('}');
        }
        other => return Err(mismatch(format!("stored JSON value contains a non-JSON `{}`", other.kind()))),
    }
    Ok(())
}

fn escape_into(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if (ch as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_structure_and_renders_at_the_boundary() {
        let bytes = encode(r#"{"b":[1,2.5,"x",null],"a":{"ok":true}}"#).unwrap();
        assert_eq!(text_from_bytes(&bytes).unwrap(), r#"{"a":{"ok":true},"b":[1,2.5,"x",null]}"#);
        // 落库载荷是结构化值，不是 JSON 文本：没有 `{` 开头。
        assert_ne!(bytes.first(), Some(&b'{'));
        assert_eq!(decode(&bytes).unwrap(), decode(&encode(&text_from_bytes(&bytes).unwrap()).unwrap()).unwrap());
    }

    #[test]
    fn rejects_invalid_json() {
        assert!(encode("{").is_err());
        assert!(encode("1e999").is_err());
    }

    #[test]
    fn decodes_postgres_binary_jsonb() {
        let mut bytes = vec![1_u8];
        bytes.extend_from_slice(br#"{"a":1}"#);
        assert_eq!(text_from_bytes(&encode_bytes(&bytes).unwrap()).unwrap(), r#"{"a":1}"#);
    }
}
