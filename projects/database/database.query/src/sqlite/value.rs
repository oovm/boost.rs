//! SQLite value domain: the one-cell carrier, literal and record decoding, and
//! the canonical equality signature shared by lists, groups and probes.
//!
//! The comparison, arithmetic and affinity mechanisms stay with the facade
//! until the evaluation closure lands; this module owns only the value shapes
//! the catalog and the name resolver need.

use database_formats::sqlite::{RecordLimits, RecordValue, TextEncoding, WriteValue, decode_record};
use database_storage::sqlite::Collation;

use super::{error::SqlError, frontend::Literal, storage_failure};

/// One cell surfaced to the host.
#[derive(Clone, Debug, PartialEq)]
pub enum Cell {
    /// SQL `NULL`.
    Null,
    /// Signed integer.
    Integer(i64),
    /// Binary64 value.
    Real(f64),
    /// UTF-8 text.
    Text(String),
    /// Blob bytes.
    Blob(Vec<u8>),
}

/// Converts a parsed literal into its storage cell.
pub fn literal_cell(literal: &Literal) -> Cell {
    match literal {
        Literal::Null => Cell::Null,
        Literal::Integer(value) => Cell::Integer(*value),
        Literal::Real(value) => Cell::Real(*value),
        Literal::Text(value) => Cell::Text(value.clone()),
        Literal::Blob(value) => Cell::Blob(value.clone()),
    }
}

/// Canonical equality signature of one index or group key value.
///
/// Two cells share a signature exactly when `compare_cells_with` reports them
/// equal, so a hash bucket built from signatures never splits rows the linear
/// scan would have grouped, and never merges rows it would have kept apart.
/// Values whose equality the canonical form cannot reproduce (a binary64 NaN,
/// which the engine compares equal to every numeric, or an integer outside the
/// exact binary64 range) yield `None` and push the caller back to the linear
/// path.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum ProbeKey {
    /// SQL `NULL`.
    Null,
    /// A numeric value canonicalized to its binary64 bits, with every signed
    /// zero folded onto positive zero (the engine compares them equal).
    Number(u64),
    /// An integer binary64 cannot represent exactly.
    Wide(i64),
    /// Text folded for its collation, so collation equality is byte equality.
    Text(Box<[u8]>),
    /// Blob bytes.
    Blob(Box<[u8]>),
}

/// One key position's canonical signature, or `None` when the value is a NaN.
pub fn probe_key(cell: &Cell, collation: Collation) -> Option<ProbeKey> {
    match cell {
        Cell::Null => Some(ProbeKey::Null),
        Cell::Integer(value) => Some(integer_probe_key(*value)),
        Cell::Real(value) => real_probe_key(*value),
        Cell::Text(text) => Some(ProbeKey::Text(fold_text(text, collation).into_boxed_slice())),
        Cell::Blob(bytes) => Some(ProbeKey::Blob(bytes.clone().into_boxed_slice())),
    }
}

/// Signature of an integer: exact binary64 values fold onto the numeric bits,
/// everything else keeps the full `i64`.
pub fn integer_probe_key(value: i64) -> ProbeKey {
    let real = value as f64;
    if real.abs() < 9_223_372_036_854_775_808.0 && (real as i64) == value { ProbeKey::Number(real.to_bits()) } else { ProbeKey::Wide(value) }
}

/// Signature of a real; `None` for a NaN, which the engine compares equal to
/// every numeric value and therefore has no hashable class.
pub fn real_probe_key(value: f64) -> Option<ProbeKey> {
    if value.is_nan() {
        None
    }
    else if value == 0.0 {
        Some(ProbeKey::Number(0.0f64.to_bits()))
    }
    else {
        Some(ProbeKey::Number(value.to_bits()))
    }
}

/// Text folded for its collation: `NOCASE` lowercases ASCII bytes and `RTRIM`
/// drops trailing spaces, which is exactly what their comparisons ignore.
pub fn fold_text(text: &str, collation: Collation) -> Vec<u8> {
    match collation {
        Collation::Binary => text.as_bytes().to_vec(),
        Collation::Nocase => text.as_bytes().iter().map(u8::to_ascii_lowercase).collect(),
        Collation::Rtrim => text.trim_end_matches(' ').as_bytes().to_vec(),
    }
}

/// Converts a declared write value into its storage cell.
pub fn write_value_cell(value: &WriteValue) -> Cell {
    match value {
        WriteValue::Null => Cell::Null,
        WriteValue::Integer(value) => Cell::Integer(*value),
        WriteValue::Real(value) => Cell::Real(*value),
        WriteValue::Text(value) => Cell::Text(value.clone()),
        WriteValue::Blob(value) => Cell::Blob(value.clone()),
    }
}

/// Decodes a raw record into cells without expanding an implicit rowid alias.
pub fn decode_cells(payload: &[u8]) -> Result<Vec<Cell>, SqlError> {
    let limits = RecordLimits { max_columns: 1024, max_payload_bytes: 16 * 1024 * 1024 };
    let values = decode_record(payload, TextEncoding::Utf8, limits).map_err(|error| storage_failure(error).in_statement("select"))?;
    // 文本字段直接移交解码结果里的 `String`,不再多复制一次。
    Ok(values
        .into_iter()
        .map(|value| match value {
            RecordValue::Null => Cell::Null,
            RecordValue::Integer(value) => Cell::Integer(value),
            RecordValue::Real(value) => Cell::Real(value),
            RecordValue::Text(text) => Cell::Text(text),
            RecordValue::Blob(bytes) => Cell::Blob(bytes.to_vec()),
        })
        .collect())
}
