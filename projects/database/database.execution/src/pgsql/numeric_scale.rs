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
    database_query::sql_execute::rescale_decimal_text(text, scale)
}

/// PostgreSQL's wire typmod for a declared `numeric(p,s)` type.
pub fn numeric_type_modifier(sql_type: &str) -> Option<i32> {
    database_query::sql_execute::numeric_type_modifier(sql_type)
}

/// Coerces one assigned value to the declared scale of its target column.
///
/// A literal is rewritten in place; any other expression is wrapped in a cast to
/// `numeric(p,s)` so the evaluator applies the scale after arithmetic.
fn coerce_expression(expression: &SqlCatalogBoundExpression, precision: u8, scale: u8) -> Result<SqlCatalogBoundExpression, StatementError> {
    match expression {
        SqlCatalogBoundExpression::Literal(literal) => Ok(SqlCatalogBoundExpression::Literal(rescale_literal(literal, precision, scale)?)),
        other => Ok(SqlCatalogBoundExpression::Cast {
            expr: Box::new(other.clone()),
            target: format!("numeric({precision},{scale})"),
        }),
    }
}

/// Rewrites the decimal literals a write plan carries to their declared scale.
///
/// `INSERT ... VALUES` and `UPDATE ... SET` assignments are covered, including
/// assignments computed by an expression (which become a `numeric(p,s)` cast).
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
                assignment.value = coerce_expression(&assignment.value, precision, scale)?;
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
