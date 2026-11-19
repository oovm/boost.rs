//! Catalog-directed scalar coercion shared by every write frontend.
//!
//! A table column declares a canonical type; a frontend only supplies a raw
//! literal (VOS literal or SQL literal). This module converts the frontend
//! value into the exact scalar the column stores, so VOS and SQL inserts and
//! updates type their values through one implementation.

use database_types::{Error, Result, Value};
use schema_input_vos::ResolvedContract;
use schema_types::ResolvedCanonicalType;

fn coerce_error(message: impl Into<String>) -> Error {
    Error::Schema { message: message.into() }
}

/// `CAST(expr AS <target>)` supporting the four standard SQL target types.
///
/// Returns an `Err` when the conversion is not representable (e.g. text that
/// does not parse as integer) so the caller can surface a diagnostic rather
/// than silently producing an incorrect value. Casting `NULL` preserves NULL.
pub fn eval_cast(value: &Value, target: &str) -> Result<Value> {
    // NULL stays NULL regardless of target type.
    if matches!(value, Value::Null) {
        return Ok(Value::Null);
    }
    let target = target.trim().to_ascii_uppercase();
    match target.as_str() {
        "INTEGER" | "INT" | "BIGINT" => cast_to_integer(value),
        "REAL" | "FLOAT" | "DOUBLE" => cast_to_real(value),
        "TEXT" | "VARCHAR" | "CHAR" | "STRING" => cast_to_text(value),
        "BLOB" | "BYTEA" | "BYTES" => cast_to_blob(value),
        other => Err(coerce_error(format!("unsupported CAST target type `{other}`"))),
    }
}

fn cast_to_integer(value: &Value) -> Result<Value> {
    let n = match value {
        Value::Bool(b) => {
            if *b {
                1
            }
            else {
                0
            }
        }
        Value::I64(n) => return Ok(Value::I64(*n)),
        Value::U64(n) => {
            let clamped = (*n).min(i64::MAX as u64) as i64;
            return Ok(Value::I64(clamped));
        }
        Value::F64(n) => {
            if n.is_nan() {
                return Err(coerce_error("CAST NaN to INTEGER is not representable"));
            }
            *n as i64
        }
        Value::Text(s) => s.trim().parse::<i64>().map_err(|_| coerce_error(format!("CAST text `{s}` to INTEGER failed")))?,
        _ => return Err(coerce_error(format!("CAST from `{:?}` to INTEGER is not supported", value))),
    };
    Ok(Value::I64(n))
}

fn cast_to_real(value: &Value) -> Result<Value> {
    let n = match value {
        Value::Bool(b) => {
            if *b {
                1.0
            }
            else {
                0.0
            }
        }
        Value::I64(n) => *n as f64,
        Value::U64(n) => *n as f64,
        Value::F64(n) => return Ok(Value::F64(*n)),
        Value::Text(s) => s.trim().parse::<f64>().map_err(|_| coerce_error(format!("CAST text `{s}` to REAL failed")))?,
        _ => return Err(coerce_error(format!("CAST from `{:?}` to REAL is not supported", value))),
    };
    Ok(Value::F64(n))
}

fn cast_to_text(value: &Value) -> Result<Value> {
    let text = match value {
        Value::Null => return Ok(Value::Null),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
        Value::I64(n) => n.to_string(),
        Value::U64(n) => n.to_string(),
        Value::F64(n) => {
            // Use the canonical float representation, trimmed of trailing zeros.
            // Format specifier `.15` matches SQLite-like display precision.
            let s = format!("{n:.15}");
            let trimmed = s.trim_end_matches('0').trim_end_matches('.').to_string();
            if trimmed.contains('.') { trimmed } else { format!("{trimmed}.0") }
        }
        Value::Text(s) => return Ok(Value::Text(s.clone())),
        Value::Uuid(u) => u.to_string(),
        _ => return Err(coerce_error(format!("CAST from `{:?}` to TEXT is not supported", value))),
    };
    Ok(Value::Text(text))
}

fn cast_to_blob(value: &Value) -> Result<Value> {
    let bytes = match value {
        Value::Null => return Ok(Value::Null),
        Value::Bytes(b) => return Ok(Value::Bytes(b.clone())),
        Value::Text(s) => s.as_bytes().to_vec(),
        _ => return Err(coerce_error(format!("CAST from `{:?}` to BLOB is not supported", value))),
    };
    Ok(Value::Bytes(bytes))
}

/// Deepest `Optional` / `Reference` / `List` nesting coercion follows.
const MAX_TYPE_DEPTH: u8 = 32;

/// Exact scalar target declared by a VOS canonical type.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ScalarTarget {
    Real,
    Bytes,
    Date,
    Time,
    Timestamp,
    Decimal { precision: u8, scale: u8 },
}

impl std::fmt::Display for ScalarTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Real => f.write_str("f64"),
            Self::Bytes => f.write_str("bytes"),
            Self::Date => f.write_str("date"),
            Self::Time => f.write_str("time"),
            Self::Timestamp => f.write_str("datetime"),
            Self::Decimal { precision, scale } => write!(f, "decimal({precision}, {scale})"),
        }
    }
}

pub(crate) fn scalar_target(ty: &ResolvedCanonicalType) -> Option<ScalarTarget> {
    use schema_types::{CanonicalScalar, ResolvedCanonicalTypeArgument};
    match ty {
        ResolvedCanonicalType::Scalar(CanonicalScalar::F64) => Some(ScalarTarget::Real),
        ResolvedCanonicalType::Scalar(CanonicalScalar::Bytes) => Some(ScalarTarget::Bytes),
        ResolvedCanonicalType::Scalar(CanonicalScalar::Date) => Some(ScalarTarget::Date),
        ResolvedCanonicalType::Scalar(CanonicalScalar::Time) => Some(ScalarTarget::Time),
        ResolvedCanonicalType::Scalar(CanonicalScalar::DateTime) => Some(ScalarTarget::Timestamp),
        ResolvedCanonicalType::Scalar(CanonicalScalar::Decimal) => {
            Some(ScalarTarget::Decimal { precision: database_types::scalar::DECIMAL_MAX_PRECISION, scale: 0 })
        }
        ResolvedCanonicalType::Generic { path, arguments } if path == &["Decimal"] => {
            let [ResolvedCanonicalTypeArgument::Literal(precision), ResolvedCanonicalTypeArgument::Literal(scale)] = arguments.as_slice()
            else {
                return None;
            };
            let precision = precision.parse::<u8>().ok()?;
            let scale = scale.parse::<u8>().ok()?;
            database_types::Decimal::new(0, precision, scale)
                .ok()
                .map(|decimal| ScalarTarget::Decimal { precision: decimal.precision(), scale: decimal.scale() })
        }
        _ => None,
    }
}

/// Coerce an already-evaluated frontend value into the column's declared type.
///
/// `T?`, `&T` and `[T]` are transparent wrappers:
///
/// - `T?` accepts null and otherwise coerces exactly like `T`,
/// - `&T` stores the referenced row's primary key, so it coerces as that key's
///   type (`&User` with a `utf8` key is `utf8`),
/// - `[T]` accepts a list and coerces every element as `T`.
///
/// Canonical scalars outside the local execution set are rejected instead of
/// being stored untyped, so a declared `utf16` or narrow integer never degrades
/// into text or a wider integer. A `uuid` column keeps the canonical lowercase
/// spelling, so a UUID value and a UUID text literal normalize to the same
/// value. A `vector<N>` column keeps exactly `N` finite components. Columns
/// without a scalar target (records and user types) accept the value unchanged.
pub fn value_to_scalar(value: Value, ty: &ResolvedCanonicalType, contract: &ResolvedContract) -> Result<Value> {
    coerce_value(value, ty, contract, 0)
}

fn coerce_value(value: Value, ty: &ResolvedCanonicalType, contract: &ResolvedContract, depth: u8) -> Result<Value> {
    if depth > MAX_TYPE_DEPTH {
        return Err(coerce_error("declared type nests deeper than the supported depth"));
    }
    match ty {
        ResolvedCanonicalType::Optional(inner) => match value {
            Value::Null => Ok(Value::Null),
            other => coerce_value(other, inner, contract, depth + 1),
        },
        // A reference stores the target's primary key, not the target row.
        ResolvedCanonicalType::Reference(inner) => match inner.as_ref() {
            ResolvedCanonicalType::User { type_id, .. } => {
                let key = primary_key_type(contract, *type_id)?;
                coerce_value(value, key, contract, depth + 1)
            }
            other => coerce_value(value, other, contract, depth + 1),
        },
        ResolvedCanonicalType::List(inner) => match value {
            Value::Null => Ok(Value::Null),
            Value::List(items) => {
                let mut coerced = Vec::with_capacity(items.len());
                for item in items {
                    coerced.push(coerce_value(item, inner, contract, depth + 1)?);
                }
                Ok(Value::List(coerced))
            }
            other => Err(coerce_error(format!("column type `[T]` requires a list value, got `{}`", other.kind()))),
        },
        _ => coerce_exact(value, ty),
    }
}

/// Primary-key type of one resolved table type.
pub(crate) fn primary_key_type(contract: &ResolvedContract, type_id: u64) -> Result<&ResolvedCanonicalType> {
    let target = contract
        .types
        .iter()
        .find(|entry| entry.type_id == type_id)
        .ok_or_else(|| coerce_error(format!("reference targets unknown type id `{type_id}`")))?;
    let field = target
        .fields
        .iter()
        .find(|field| field.attributes.iter().any(|attribute| attribute.name == "primary"))
        .ok_or_else(|| coerce_error(format!("reference target `{}` has no primary key field", target.canonical_path.join("::"))))?;
    Ok(&field.canonical_type)
}

/// Keep a value only when it already has the column's exact kind.
///
/// `i64`, `bool` and `utf8` are the three scalars the row codec stores as their
/// own kind, so a value of another kind would either be misread later or fail
/// at encode time. `null` stays accepted because a nullable column is declared
/// as `T?` and the write path may still see an explicit null.
fn coerce_kind(value: Value, kind: &'static str, matches_kind: impl Fn(&Value) -> bool) -> Result<Value> {
    if matches!(value, Value::Null) || matches_kind(&value) {
        return Ok(value);
    }
    Err(coerce_error(format!("column type `{kind}` requires a `{kind}` value, got `{}`", value.kind())))
}

/// Coerce a value whose declared type is already unwrapped.
fn coerce_exact(value: Value, ty: &ResolvedCanonicalType) -> Result<Value> {
    use schema_types::CanonicalScalar;
    // A REAL value belongs in an `f64` column. Every other declared column
    // keeps an exact value, so a REAL is rejected before the write is staged
    // rather than being rounded into an integer or degraded into text.
    if let Value::F64(number) = &value {
        return match scalar_target(ty) {
            Some(ScalarTarget::Real) => coerce_to(value, ScalarTarget::Real),
            _ => Err(coerce_error(format!(
                "REAL value `{number}` cannot be stored: the target column is not `f64` and 本地 never converts \
                 REAL into an integer, bool, text, uuid, date, time, timestamp or decimal column"
            ))),
        };
    }
    // A BLOB value belongs in a `bytes` column. Every other target keeps an
    // exact value, so bytes are rejected before staging instead of being
    // reinterpreted as text or a number. Whether a large payload stays inline
    // or becomes an object reference is a physical property of the execution
    // store, so coercion keeps the octets untouched and the execution side
    // places them through its storage port.
    if let Value::Bytes(_) = &value {
        return match scalar_target(ty) {
            Some(ScalarTarget::Bytes) => Ok(value),
            _ => Err(coerce_error(
                "BLOB value cannot be stored: the target column is not `bytes` and 本地 never converts BLOB into an \
                 integer, bool, text, uuid, date, time, timestamp or decimal column",
            )),
        };
    }
    // A VECTOR value belongs in a `vector<N>` column of the same dimension.
    // Every other target keeps an exact value, so a vector is rejected before
    // staging instead of being reinterpreted as text, bytes or a number.
    if matches!(&value, Value::Vector(_)) {
        return match ty {
            ResolvedCanonicalType::Vector { dimension, .. } => coerce_vector(value, *dimension),
            _ => Err(coerce_error(
                "VECTOR value cannot be stored: the target column is not `vector<N>` and 本地 never converts a vector \
                 into an integer, bool, text, bytes, uuid, date, time, timestamp or decimal column",
            )),
        };
    }
    match ty {
        ResolvedCanonicalType::Scalar(CanonicalScalar::Uuid) => coerce_uuid(value),
        ResolvedCanonicalType::Scalar(CanonicalScalar::I64) => coerce_kind(value, "i64", |value| matches!(value, Value::I64(_))),
        ResolvedCanonicalType::Scalar(CanonicalScalar::Bool) => coerce_kind(value, "bool", |value| matches!(value, Value::Bool(_))),
        ResolvedCanonicalType::Scalar(CanonicalScalar::Utf8) => coerce_kind(value, "utf8", |value| matches!(value, Value::Text(_))),
        ResolvedCanonicalType::Scalar(scalar) => {
            let target =
                scalar_target(ty).ok_or_else(|| coerce_error(format!("VOS scalar `{scalar}` is not in the local execution scalar set")))?;
            coerce_to(value, target)
        }
        ResolvedCanonicalType::Generic { path, .. } if path == &["Decimal"] => {
            let target = scalar_target(ty).ok_or_else(|| coerce_error("`Decimal` requires two literal arguments: precision and scale"))?;
            coerce_to(value, target)
        }
        ResolvedCanonicalType::Vector { dimension, .. } => coerce_vector(value, *dimension),
        ResolvedCanonicalType::File => coerce_file(value),
        _ => Ok(value),
    }
}

/// Check a `file` column value against the descriptor shape it must keep.
///
/// A file column stores a descriptor and a manifest reference, never inline
/// octets or a truncation of them, so every other value kind is rejected before
/// the write is staged.
fn coerce_file(value: Value) -> Result<Value> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::File(_) => Ok(value),
        other => Err(coerce_error(format!("column type `file` requires a file descriptor, got `{}`", other.kind()))),
    }
}

/// Check a vector value against the dimension the column declares.
///
/// The declared dimension is part of the column contract, so a vector with
/// another component count cannot be stored as if it were the declared one, and
/// a non-finite component is refused before the write is staged instead of
/// landing a value that no comparison or distance can interpret. Whether the
/// validated vector stays inline or becomes a CAS `VectorPayload` reference is
/// decided by the execution store, not here.
fn coerce_vector(value: Value, dimension: u32) -> Result<Value> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::Vector(vector) => {
            if vector.dim != dimension {
                return Err(coerce_error(format!("column type `vector<{dimension}>` requires {dimension} components, got {}", vector.dim)));
            }
            if let Some(index) = vector.data.iter().position(|component| !component.is_finite()) {
                return Err(coerce_error(format!(
                    "column type `vector<{dimension}>` requires finite components, component {index} is not finite"
                )));
            }
            Ok(Value::Vector(vector))
        }
        other => Err(coerce_error(format!("column type `vector<{dimension}>` requires a vector value, got `{}`", other.kind()))),
    }
}

/// Normalize a `uuid` column value to its canonical lowercase spelling.
///
/// A UUID value and a UUID text literal both end up as [`Value::Uuid`], so rows
/// written through the VOS and SQL frontends project, compare, and sort the
/// same way. Text that is not a UUID is rejected instead of being stored as if
/// it were one, and every other kind is rejected as well.
fn coerce_uuid(value: Value) -> Result<Value> {
    let parsed = match &value {
        Value::Null => return Ok(Value::Null),
        Value::Uuid(id) => return Ok(Value::Uuid(*id)),
        Value::Text(text) => uuid::Uuid::parse_str(text)
            .map_err(|error| coerce_error(format!("column type `uuid` requires a UUID literal: `{text}` is not one ({error})")))?,
        other => {
            return Err(coerce_error(format!("column type `uuid` requires a UUID literal, got `{}`", other.kind())));
        }
    };
    Ok(Value::Uuid(parsed))
}

fn coerce_to(value: Value, target: ScalarTarget) -> Result<Value> {
    match (value, target) {
        (Value::Null, _) => Ok(Value::Null),
        (Value::I64(number), ScalarTarget::Real) => Ok(Value::F64(number as f64)),
        (Value::U64(number), ScalarTarget::Real) => Ok(Value::F64(number as f64)),
        (Value::F64(number), ScalarTarget::Real) if number.is_finite() => Ok(Value::F64(number)),
        (Value::F64(number), ScalarTarget::Real) => {
            Err(coerce_error(format!("REAL value `{number}` is not finite and cannot be stored in an `f64` column")))
        }
        (Value::Text(_) | Value::Bool(_), ScalarTarget::Real) => {
            Err(coerce_error("column type `f64` requires an integer or REAL numeric literal"))
        }
        (Value::Bytes(bytes), ScalarTarget::Bytes) => Ok(Value::Bytes(bytes)),
        (Value::Object(object), ScalarTarget::Bytes) => Ok(Value::Object(object)),
        (Value::Text(_), ScalarTarget::Bytes) => Err(coerce_error(
            "column type `bytes` requires a BLOB value: 本地 never converts text into BLOB implicitly, use \
             `CAST(... AS BLOB)` for an explicit conversion",
        )),
        (Value::Text(text), ScalarTarget::Date) => Ok(Value::Date(database_types::Date::parse_iso(&text).map_err(Error::from)?)),
        (Value::Text(text), ScalarTarget::Time) => Ok(Value::Time(database_types::Time::parse_iso(&text).map_err(Error::from)?)),
        (Value::Text(text), ScalarTarget::Timestamp) => Ok(Value::Timestamp(database_types::Timestamp::parse_iso(&text).map_err(Error::from)?)),
        (Value::Text(text), ScalarTarget::Decimal { precision, scale }) => {
            Ok(Value::Decimal(database_types::Decimal::parse(&text, precision, scale).map_err(Error::from)?))
        }
        (Value::I64(number), ScalarTarget::Decimal { precision, scale }) => {
            Ok(Value::Decimal(database_types::Decimal::parse(&number.to_string(), precision, scale).map_err(Error::from)?))
        }
        (Value::Bool(_), target) => Err(coerce_error(format!("column type `{target}` requires an exact string or integer literal"))),
        (_, target) => Err(coerce_error(format!("unsupported literal for column type `{target}`"))),
    }
}
