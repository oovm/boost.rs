//! PostgreSQL `numeric` display scale.
//!
//! PostgreSQL `numeric(p,s)` coerces every stored value to `s` fractional
//! digits, so `12.5` written into a `numeric(12,2)` column reads back as
//! `12.50`. The gateway keeps decimal values as canonical text, so the scale
//! lives in the declared column type (`SchemaField::sql_type`) rather than in
//! the value. This module reads that declared type and rewrites the literal a
//! write carries before it reaches storage.

use database_query::{SqlBoundLiteral, SqlCatalogBoundExpression, SqlExecutionPlan};

use super::StatementError;

/// Declared precision and scale of a `NUMERIC` / `DECIMAL` column type.
///
/// Returns `None` for every other type and for a bare `NUMERIC`, which keeps
/// whatever scale the value was written with.
pub fn declared_scale(sql_type: &str) -> Option<(u8, u8)> {
    let sql_type = sql_type.trim();
    let open = sql_type.find('(')?;
    let close = sql_type.rfind(')')?;
    if close <= open {
        return None;
    }
    let base = sql_type[..open].trim();
    if !base.eq_ignore_ascii_case("NUMERIC") && !base.eq_ignore_ascii_case("DECIMAL") {
        return None;
    }
    let mut parameters = sql_type[open + 1..close].split(',').map(str::trim);
    let precision = parameters.next()?.parse::<u8>().ok()?;
    let scale = match parameters.next() {
        Some(scale) => scale.parse::<u8>().ok()?,
        // `NUMERIC(p)` keeps zero fractional digits.
        None => 0,
    };
    (scale <= precision).then_some((precision, scale))
}

/// Rewrites one literal to the declared scale of its target column.
///
/// Text, number and integer literals whose body is a decimal spelling are
/// rescaled; every other literal (boolean, NULL, bytes) passes through untouched
/// so the ordinary write validation still reports its own error.
fn rescale_literal(literal: &SqlBoundLiteral, precision: u8, scale: u8) -> Result<SqlBoundLiteral, StatementError> {
    let owned;
    let text = match literal {
        SqlBoundLiteral::Number(text) => text.as_str(),
        SqlBoundLiteral::String(text) => text.as_str(),
        // An integer written into a scaled column still carries the display
        // scale, so `12` becomes `12.00` rather than staying `12`.
        SqlBoundLiteral::Integer(number) if scale > 0 => {
            owned = number.to_string();
            owned.as_str()
        }
        _ => return Ok(literal.clone()),
    };
    let Some(scaled) = rescale_decimal_text(text, scale) else {
        return Ok(literal.clone());
    };
    // `precision` counts the digits of the value, not the display scale, so the
    // integer part's leading zeros are not significant.
    let integer = scaled.split('.').next().unwrap_or_default().trim_start_matches(['-', '+']).trim_start_matches('0');
    let total = integer.len() + usize::from(scale);
    if total > usize::from(precision) {
        return Err(StatementError::new(
            "22003",
            format!("numeric field overflow: value \"{text}\" does not fit numeric({precision},{scale})"),
        ));
    }
    // The scaled spelling is carried as text so the display scale survives
    // storage; a numeric literal would be re-rendered from its value, which drops
    // the trailing zeros the column declares.
    Ok(SqlBoundLiteral::String(scaled))
}

/// Rounds a decimal spelling to `scale` fractional digits, half away from zero,
/// and pads it with zeros so the display scale is preserved.
///
/// Returns `None` when `text` is not a decimal spelling, which keeps the
/// existing literal validation in charge of the rejection.
pub fn rescale_decimal_text(text: &str, scale: u8) -> Option<String> {
    let trimmed = text.trim();
    let (negative, body) = match trimmed.strip_prefix('-') {
        Some(body) => (true, body),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    if body.is_empty()
        || !body.chars().all(|character| character.is_ascii_digit() || character == '.')
        || body.matches('.').count() > 1
        || body == "."
    {
        return None;
    }
    let (integer, fraction) = match body.split_once('.') {
        Some((integer, fraction)) => (integer, fraction),
        None => (body, ""),
    };
    let scale = usize::from(scale);
    let mut integer_digits: Vec<u8> = integer.bytes().map(|byte| byte - b'0').collect();
    let mut fraction_digits: Vec<u8> = fraction.bytes().map(|byte| byte - b'0').collect();
    if fraction_digits.len() > scale {
        let round_up = fraction_digits[scale] >= 5;
        fraction_digits.truncate(scale);
        if round_up {
            increment(&mut fraction_digits, &mut integer_digits);
        }
    }
    fraction_digits.resize(scale, 0);
    let integer_text = render_digits(&integer_digits);
    let zero = integer_digits.iter().chain(fraction_digits.iter()).all(|digit| *digit == 0);
    let sign = if negative && !zero { "-" } else { "" };
    // The fraction keeps every declared digit, including leading zeros.
    let fraction_text: String = fraction_digits.iter().map(|digit| char::from(b'0' + digit)).collect();
    Some(if scale == 0 { format!("{sign}{integer_text}") } else { format!("{sign}{integer_text}.{fraction_text}") })
}

/// Adds one to `fraction` as a fixed-point number, carrying into `integer`.
fn increment(fraction: &mut [u8], integer: &mut Vec<u8>) {
    for position in (0..fraction.len()).rev() {
        if fraction[position] == 9 {
            fraction[position] = 0;
            continue;
        }
        fraction[position] += 1;
        return;
    }
    for position in (0..integer.len()).rev() {
        if integer[position] == 9 {
            integer[position] = 0;
            continue;
        }
        integer[position] += 1;
        return;
    }
    integer.insert(0, 1);
}

fn render_digits(digits: &[u8]) -> String {
    let digits = match digits.iter().position(|digit| *digit != 0) {
        Some(start) => &digits[start..],
        // An all-zero integer part still renders as one `0`.
        None => return if digits.is_empty() { "0".to_string() } else { "0".to_string() },
    };
    digits.iter().map(|digit| char::from(b'0' + digit)).collect()
}

/// Rewrites the decimal literals a write plan carries to their declared scale.
///
/// Only literal writes are covered: `INSERT ... VALUES` and literal `UPDATE`
/// assignments. Values computed by an expression are evaluated by the execution
/// layer, which is outside this module.
///
/// `declared` answers one column map per table identity, which keeps a statement
/// that writes several fields of one table to a single catalog snapshot.
pub fn coerce_write_plan(
    plan: &mut SqlExecutionPlan,
    declared: impl Fn(u64) -> std::collections::BTreeMap<u64, String>,
) -> Result<(), StatementError> {
    let mut columns = DeclaredColumns::new(&declared);
    match plan {
        SqlExecutionPlan::Insert(write) => coerce_insert(write, &mut columns),
        SqlExecutionPlan::MultiInsert(writes) => {
            for write in writes {
                coerce_insert(write, &mut columns)?;
            }
            Ok(())
        }
        SqlExecutionPlan::Update(update) => {
            let Some(type_id) = namespace_type_id(&update.namespace)
            else {
                return Ok(());
            };
            for assignment in &mut update.fields {
                let Some((precision, scale)) = columns.declared_scale(type_id, assignment.field_id)
                else {
                    continue;
                };
                if let SqlCatalogBoundExpression::Literal(literal) = &assignment.value {
                    assignment.value = SqlCatalogBoundExpression::Literal(rescale_literal(literal, precision, scale)?);
                }
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Per-table declared column types, resolved at most once per statement.
struct DeclaredColumns<'a, F> {
    lookup: &'a F,
    cache: std::collections::BTreeMap<u64, std::collections::BTreeMap<u64, String>>,
}

impl<'a, F: Fn(u64) -> std::collections::BTreeMap<u64, String>> DeclaredColumns<'a, F> {
    fn new(lookup: &'a F) -> Self {
        Self { lookup, cache: std::collections::BTreeMap::new() }
    }

    fn declared_scale(&mut self, type_id: u64, field_id: u64) -> Option<(u8, u8)> {
        let columns = self.cache.entry(type_id).or_insert_with(|| (self.lookup)(type_id));
        columns.get(&field_id).and_then(|sql_type| declared_scale(sql_type))
    }
}

fn coerce_insert<F: Fn(u64) -> std::collections::BTreeMap<u64, String>>(
    write: &mut database_query::WritePlan,
    columns: &mut DeclaredColumns<'_, F>,
) -> Result<(), StatementError> {
    let Some(type_id) = namespace_type_id(&write.namespace)
    else {
        return Ok(());
    };
    for field in &mut write.fields {
        let Some((precision, scale)) = columns.declared_scale(type_id, field.field_id)
        else {
            continue;
        };
        field.value = rescale_literal(&field.value, precision, scale)?;
    }
    Ok(())
}

/// `table:{type_id}` namespace → type identity.
pub fn namespace_type_id(namespace: &str) -> Option<u64> {
    namespace.strip_prefix("table:")?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_scale_reads_only_parameterised_numeric() {
        assert_eq!(declared_scale("NUMERIC(12,2)"), Some((12, 2)));
        assert_eq!(declared_scale("DECIMAL(5, 0)"), Some((5, 0)));
        assert_eq!(declared_scale("NUMERIC(4)"), Some((4, 0)));
        assert_eq!(declared_scale("NUMERIC"), None);
        assert_eq!(declared_scale("INTEGER"), None);
        assert_eq!(declared_scale("NUMERIC(2,5)"), None);
    }

    #[test]
    fn rescale_pads_and_rounds_half_away_from_zero() {
        assert_eq!(rescale_decimal_text("12.5", 2).as_deref(), Some("12.50"));
        assert_eq!(rescale_decimal_text("12", 2).as_deref(), Some("12.00"));
        assert_eq!(rescale_decimal_text("12.345", 2).as_deref(), Some("12.35"));
        assert_eq!(rescale_decimal_text("12.344", 2).as_deref(), Some("12.34"));
        assert_eq!(rescale_decimal_text("-12.345", 2).as_deref(), Some("-12.35"));
        assert_eq!(rescale_decimal_text("9.999", 2).as_deref(), Some("10.00"));
        assert_eq!(rescale_decimal_text("0.5", 0).as_deref(), Some("1"));
        assert_eq!(rescale_decimal_text("abc", 2), None);
    }
}
