//! MongoDB 值语义：路径取值、数值/标量相等、BSON 排序秩与聚合真值。
//!
//! 过滤、投影、排序与聚合表达式共享这一份实现，不再各写一套比较规则。

use std::cmp::Ordering;

use database_protocol_mongo::bson::{BsonDocument, BsonValue, encode_bson, integral_value, value_to_key};

/// Aggregation truthiness: `false`, `0`, `null`, and missing are false.
pub fn expression_truthy(value: &BsonValue) -> bool {
    match value {
        BsonValue::Bool(flag) => *flag,
        BsonValue::Null => false,
        BsonValue::Int32(number) => *number != 0,
        BsonValue::Int64(number) => *number != 0,
        BsonValue::Double(number) => *number != 0.0,
        _ => true,
    }
}

/// Renders a count as `int32` when it fits, otherwise `int64`.
pub fn count_value(count: i64) -> BsonValue {
    i32::try_from(count).map(BsonValue::Int32).unwrap_or(BsonValue::Int64(count))
}

/// Resolves a dotted field path, one level of nesting deep.
pub fn lookup_field<'a>(document: &'a BsonDocument, path: &str) -> Option<&'a BsonValue> {
    if let Some(value) = document.get(path) {
        return Some(value);
    }
    let (head, tail) = path.split_once('.')?;
    match document.get(head)? {
        BsonValue::Document(inner) => inner.get(tail),
        _ => None,
    }
}

/// Equality as the query engine defines it, including missing versus null.
pub fn values_equal(actual: Option<&BsonValue>, expected: &BsonValue) -> bool {
    match (actual, expected) {
        (None, BsonValue::Null) => true,
        (None, _) => false,
        (Some(left), right) => match (left, right) {
            (BsonValue::Document(left), BsonValue::Document(right)) => encode_bson(left) == encode_bson(right),
            (BsonValue::Array(left), BsonValue::Array(right)) => {
                left.len() == right.len() && left.iter().zip(right.iter()).all(|(left, right)| values_equal(Some(left), right))
            }
            (BsonValue::Null, BsonValue::Null) => true,
            _ => numeric_equal(left, right).unwrap_or_else(|| scalar_key(left) == scalar_key(right)),
        },
    }
}

/// MongoDB compares `int32`, `int64` and `double` by numeric value, so `1`,
/// `NumberLong(1)` and `1.0` are the same value. Integral doubles are compared
/// through their exact integer value, which keeps values above `2^53` exact
/// instead of collapsing them through `f64`.
pub(crate) fn numeric_equal(left: &BsonValue, right: &BsonValue) -> Option<bool> {
    if let (Some(left), Some(right)) = (integer_of(left), integer_of(right)) {
        return Some(left == right);
    }
    let (integer, double) = match (integer_of(left), double_of(right)) {
        (Some(integer), Some(double)) => (integer, double),
        _ => match (double_of(left), integer_of(right)) {
            (Some(double), Some(integer)) => (integer, double),
            _ => return None,
        },
    };
    Some(integral_value(double).map(|exact| i128::from(exact) == integer).unwrap_or(false))
}

pub(crate) fn scalar_key(value: &BsonValue) -> String {
    match value {
        BsonValue::String(text) => format!("s:{text}"),
        BsonValue::Bool(flag) => format!("b:{flag}"),
        BsonValue::ObjectId(bytes) => format!("o:{bytes:?}"),
        BsonValue::Binary(binary) => format!("y:{:02x}:{:?}", binary.subtype, binary.bytes),
        other => format!("x:{other:?}"),
    }
}

/// Exact integer view of an `int32` / `int64` value.
pub fn integer_of(value: &BsonValue) -> Option<i128> {
    match value {
        BsonValue::Int32(number) => Some(i128::from(*number)),
        BsonValue::Int64(number) => Some(i128::from(*number)),
        _ => None,
    }
}

/// Numeric view of a `double` value.
pub fn double_of(value: &BsonValue) -> Option<f64> {
    match value {
        BsonValue::Double(number) => Some(*number),
        _ => None,
    }
}

/// Numeric view of a value for arithmetic operands.
pub fn number_of(value: &BsonValue) -> Option<f64> {
    match value {
        BsonValue::Int32(number) => Some(*number as f64),
        BsonValue::Int64(number) => Some(*number as f64),
        BsonValue::Double(number) => Some(*number),
        _ => None,
    }
}

/// 取数值的精确整数视图：仅当值可整除且落在 `i64` 范围内。
/// `$mod` 与位算子只检查整数值。
pub fn integral_i64(value: &BsonValue) -> Option<i64> {
    match value {
        BsonValue::Int32(number) => Some(i64::from(*number)),
        BsonValue::Int64(number) => Some(*number),
        BsonValue::Double(number) if number.fract() == 0.0 && *number >= i64::MIN as f64 && *number <= i64::MAX as f64 => Some(*number as i64),
        _ => None,
    }
}

/// BSON sort order between the value types this store can hold.
///
/// Mirrors the canonical total order encoded in [`BsonValue::compare`], which
/// itself was read from `mongod 8.0.9` `sort` results. Numeric types share one
/// bracket and `Symbol` shares the `String` bracket.
pub fn value_rank(value: &BsonValue) -> u8 {
    match value {
        BsonValue::MinKey => 0,
        BsonValue::Undefined => 1,
        BsonValue::Null => 2,
        BsonValue::Int32(_) | BsonValue::Int64(_) | BsonValue::Double(_) | BsonValue::Decimal128(_) => 3,
        BsonValue::String(_) | BsonValue::Symbol(_) => 4,
        BsonValue::Document(_) => 5,
        BsonValue::Array(_) => 6,
        BsonValue::Binary(_) => 7,
        BsonValue::ObjectId(_) => 8,
        BsonValue::Bool(_) => 9,
        BsonValue::DateTime(_) => 10,
        BsonValue::Timestamp { .. } => 11,
        BsonValue::Regex { .. } => 12,
        BsonValue::DbPointer { .. } => 13,
        BsonValue::Code(_) => 14,
        BsonValue::CodeWithScope { .. } => 15,
        BsonValue::MaxKey => 16,
    }
}

/// BSON total order between two values.
pub fn compare_values(left: &BsonValue, right: &BsonValue) -> Ordering {
    left.compare(right)
}

/// Parses `skip` / `limit` arguments, treating anything unusable as zero.
pub fn parse_count(value: Option<&BsonValue>) -> usize {
    value.and_then(int_value).filter(|count| *count > 0).map(|count| count as usize).unwrap_or(0)
}

/// Reads a numeric value as `i64` the way command arguments do.
pub fn int_value(value: &BsonValue) -> Option<i64> {
    match value {
        BsonValue::Int32(number) => Some(*number as i64),
        BsonValue::Int64(number) => Some(*number),
        BsonValue::Double(number) => Some(*number as i64),
        _ => None,
    }
}

/// Length-prefixed key so two adjacent tuples can never be confused.
pub fn index_tuple(values: &[BsonValue]) -> Vec<u8> {
    let mut tuple = Vec::new();
    for value in values {
        let key = value_to_key(value);
        tuple.extend_from_slice(&(key.len() as u32).to_be_bytes());
        tuple.extend_from_slice(&key);
    }
    tuple
}

/// Renders one value the way `mongod` prints it inside update error messages.
///
/// Every shape below was read back from a `mongod 8.0.9` duplicate-key `errmsg`;
/// dates print as `new Date(<milliseconds>)`, not as `ISODate("...")`.
pub fn render_shell_value(value: &BsonValue) -> String {
    match value {
        BsonValue::Document(document) => {
            if document.fields.is_empty() {
                "{}".to_string()
            }
            else {
                let fields: Vec<String> =
                    document.fields.iter().map(|(field, value)| format!("{field}: {}", render_shell_value(value))).collect();
                format!("{{ {} }}", fields.join(", "))
            }
        }
        BsonValue::Array(items) => {
            if items.is_empty() {
                "[]".to_string()
            }
            else {
                let items: Vec<String> = items.iter().map(render_shell_value).collect();
                format!("[ {} ]", items.join(", "))
            }
        }
        BsonValue::String(text) => format!("\"{text}\""),
        BsonValue::Int32(number) => number.to_string(),
        BsonValue::Int64(number) => number.to_string(),
        BsonValue::Double(number) => number.to_string(),
        BsonValue::Decimal128(number) => number.as_str(),
        BsonValue::Bool(flag) => if *flag { "true" } else { "false" }.to_string(),
        BsonValue::Null => "null".to_string(),
        BsonValue::Undefined => "undefined".to_string(),
        BsonValue::ObjectId(bytes) => format!("ObjectId('{}')", hex_text(bytes)),
        BsonValue::Binary(binary) => format!("BinData({}, {})", binary.subtype, hex_text(&binary.bytes)),
        BsonValue::DateTime(milliseconds) => format!("new Date({milliseconds})"),
        BsonValue::Timestamp { t, i } => format!("Timestamp({t}, {i})"),
        BsonValue::MinKey => "MinKey".to_string(),
        BsonValue::MaxKey => "MaxKey".to_string(),
        BsonValue::Symbol(text) => format!("\"{text}\""),
        BsonValue::Regex { pattern, options } => format!("/{pattern}/{options}"),
        BsonValue::Code(code) => code.clone(),
        BsonValue::CodeWithScope { code, scope } => {
            format!("CodeWScope( {code}, {} )", render_shell_value(&BsonValue::Document(scope.clone())))
        }
        // DBPointer is obsolete and `mongod 8.0.9` no longer emits it in error
        // messages, so this shape follows the classic shell form.
        BsonValue::DbPointer { namespace, id } => {
            format!("DBPointer( \"{namespace}\", ObjectId('{}') )", hex_text(id))
        }
    }
}

/// Lowercase hex of one byte slice.
pub fn hex_text(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
