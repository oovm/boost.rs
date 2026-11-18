//! Rendering one text cell as a SQL literal for a declared target type.
//!
//! Source rows arrive as text, so the migration must decide per column whether
//! the value is a bare number, a boolean keyword, or a quoted string. Getting
//! this wrong would let the target coerce a number into text or the reverse,
//! which is why the decision is driven by the mapped target type and not by the
//! shape of the text.

use crate::ddl::quote_literal;

/// Renders `value` as a literal for `target_type`, or SQL `NULL` for `None`.
pub fn sql_literal(target_type: &str, value: Option<&str>) -> String {
    let Some(text) = value else {
        return "NULL".to_owned();
    };
    let normalized = target_type.trim().to_ascii_lowercase();
    if is_integer(&normalized) {
        return numeric_literal(text);
    }
    if is_number(&normalized) {
        return numeric_literal(text);
    }
    if normalized == "boolean" || normalized == "bool" {
        return match text.trim().to_ascii_lowercase().as_str() {
            "t" | "true" | "1" => "TRUE".to_owned(),
            "f" | "false" | "0" => "FALSE".to_owned(),
            _ => quote_literal(text),
        };
    }
    quote_literal(text)
}

/// Renders a text cell with no declared type: a numeric spelling stays bare and
/// everything else is quoted. Keyset cursors use this because they carry source
/// text for columns whose target type is irrelevant to the comparison.
pub fn raw_literal(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "''".to_owned();
    }
    if trimmed.parse::<f64>().is_ok() { trimmed.to_owned() } else { quote_literal(text) }
}

fn is_integer(target_type: &str) -> bool {
    matches!(target_type, "smallint" | "integer" | "bigint" | "int2" | "int4" | "int8" | "oid")
}

fn is_number(target_type: &str) -> bool {
    target_type == "real"
        || target_type == "double precision"
        || target_type == "float4"
        || target_type == "float8"
        || target_type == "numeric"
        || target_type.starts_with("numeric(")
        || target_type == "decimal"
}

fn numeric_literal(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "NULL".to_owned();
    }
    // A value that does not parse stays quoted so the target raises its own type
    // error; the migration never invents a number the source did not hold.
    if trimmed.parse::<f64>().is_ok() { trimmed.to_owned() } else { quote_literal(trimmed) }
}
