//! SQL values, row TLV encoding and expression evaluation shared by planning and execution.

use std::{cell::RefCell, sync::Arc};

use crate::{
    catalog_bind::{SqlCatalogBoundExpression, collect_column_ids},
    sql::{
        SqlBoundBinaryOperator, SqlBoundLiteral, SqlBoundOrderDirection, SqlBoundUnaryOperator, SqlDialect, SqlErrorKind, SqlFrontendError,
        SqlNullsOrder, SqlScalarFunction,
    },
    sql_planner::ScanPlan,
};

mod expression;

/// JSON 列的网关编解码：落库是结构化 `any` 值，文本只在驱动边界出现。
pub(crate) mod json;
mod scalar;
mod values;

pub(crate) use self::expression::eval_constant;
pub use self::{
    expression::{
        compare_values, compare_values_with_nulls, eval_binary, eval_cast, eval_cast_for_dialect, eval_expression, eval_in_values, eval_unary,
        normalize_integer_values, order_nulls_first, projected_rows_equal, retain_distinct_projected_rows, values_equal,
    },
    scalar::{apply_interval_to_datetime, eval_scalar_function, extract_value, format_number, parse_interval, scalar_text, IntervalOffset},
    values::{
        RowFieldValues, SqlResult, SqlValue, cmp_string, decode_row, decode_row_fields, encode_row, eq_string, expression_needed_field_ids,
        numeric_type_modifier, rescale_decimal_text, scan_needed_field_ids,
    },
};

/// Sequence state `nextval` / `currval` / `setval` reach during evaluation.
///
/// The evaluator is a pure function of the row, so the statement that runs it
/// installs the catalog for its own duration. `database.execution::pgsql`
/// supplies an implementation backed by the durable object catalog, which keeps
/// sequence state owned there instead of leaking a second copy into planning.
pub trait SqlSequenceCatalog: Send + Sync {
    /// Advances `name` and returns the value handed out (`2200H` at the bound).
    fn nextval(&self, name: &str) -> Result<i64, SqlFrontendError>;
    /// Last value handed out for `name` (`55000` when it was never called).
    fn currval(&self, name: &str) -> Result<i64, SqlFrontendError>;
    /// Repositions `name` and returns `value`.
    fn setval(&self, name: &str, value: i64, is_called: bool) -> Result<i64, SqlFrontendError>;
}

thread_local! {
    /// Nested statements (`EXPLAIN ANALYZE`) push another catalog; the last one
    /// wins and is popped when the statement finishes.
    static SEQUENCE_CATALOGS: RefCell<Vec<Arc<dyn SqlSequenceCatalog>>> = const { RefCell::new(Vec::new()) };

    /// Session GUC snapshot visible to `current_setting()`.
    ///
    /// The evaluator is a pure function of the row, so the statement that runs
    /// it installs the session's effective settings for its own duration. The
    /// key is the lower-case parameter name.
    static SESSION_SETTINGS: RefCell<Vec<Arc<std::collections::BTreeMap<String, String>>>> = const { RefCell::new(Vec::new()) };
}

/// Runs `body` with `catalog` visible to the sequence functions.
pub fn with_sequence_catalog<T>(catalog: Arc<dyn SqlSequenceCatalog>, body: impl FnOnce() -> T) -> T {
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            SEQUENCE_CATALOGS.with(|stack| {
                stack.borrow_mut().pop();
            });
        }
    }
    SEQUENCE_CATALOGS.with(|stack| stack.borrow_mut().push(catalog));
    let _pop = Pop;
    body()
}

/// The catalog the current statement installed, when sequence functions run.
pub(crate) fn sequence_catalog() -> Option<Arc<dyn SqlSequenceCatalog>> {
    SEQUENCE_CATALOGS.with(|stack| stack.borrow().last().cloned())
}

/// Runs `body` with `settings` visible to `current_setting()`.
pub fn with_session_settings<T>(settings: Arc<std::collections::BTreeMap<String, String>>, body: impl FnOnce() -> T) -> T {
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            SESSION_SETTINGS.with(|stack| {
                stack.borrow_mut().pop();
            });
        }
    }
    SESSION_SETTINGS.with(|stack| stack.borrow_mut().push(settings));
    let _pop = Pop;
    body()
}

/// Value of one session GUC, when a caller installed a settings snapshot.
pub(crate) fn session_setting(name: &str) -> Option<String> {
    SESSION_SETTINGS.with(|stack| stack.borrow().last().and_then(|settings| settings.get(&name.trim().to_ascii_lowercase()).cloned()))
}

/// Payload byte cursor used by the row TLV decoder.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    fn read_u8(&mut self) -> Result<u8, SqlFrontendError> {
        if self.remaining() < 1 {
            return Err(internal_error("row TLV truncated reading u8"));
        }
        let value = self.data[self.pos];
        self.pos += 1;
        Ok(value)
    }

    fn read_u16(&mut self) -> Result<u16, SqlFrontendError> {
        if self.remaining() < 2 {
            return Err(internal_error("row TLV truncated reading u16"));
        }
        let value = u16::from_be_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(value)
    }

    fn read_u32(&mut self) -> Result<u32, SqlFrontendError> {
        if self.remaining() < 4 {
            return Err(internal_error("row TLV truncated reading u32"));
        }
        let bytes: [u8; 4] = self.data[self.pos..self.pos + 4].try_into().unwrap();
        let value = u32::from_be_bytes(bytes);
        self.pos += 4;
        Ok(value)
    }

    fn read_u64(&mut self) -> Result<u64, SqlFrontendError> {
        if self.remaining() < 8 {
            return Err(internal_error("row TLV truncated reading u64"));
        }
        let bytes: [u8; 8] = self.data[self.pos..self.pos + 8].try_into().unwrap();
        let value = u64::from_be_bytes(bytes);
        self.pos += 8;
        Ok(value)
    }

    fn read_bytes(&mut self, len: usize) -> Result<Vec<u8>, SqlFrontendError> {
        if self.remaining() < len {
            return Err(internal_error("row TLV truncated reading bytes"));
        }
        let out = self.data[self.pos..self.pos + len].to_vec();
        self.pos += len;
        Ok(out)
    }
}

pub(crate) fn internal_error(message: impl Into<String>) -> SqlFrontendError {
    SqlFrontendError::internal(message)
}

pub(crate) fn division_by_zero_error() -> SqlFrontendError {
    SqlFrontendError::new(SqlErrorKind::DivisionByZero, "division by zero")
}

/// 精确整数运算越界。两方言都要报 SQLSTATE `22003`，因此不降级成通用执行错误。
pub(crate) fn numeric_overflow_error(stage: &str) -> SqlFrontendError {
    SqlFrontendError::new(SqlErrorKind::NumericOverflow, format!("{stage} overflowed the integer range"))
}

/// SQL 三值逻辑里的真值：非 bool 值没有真值。
pub(crate) fn sql_truth(value: &SqlValue) -> Option<bool> {
    match value {
        SqlValue::Boolean(value) => Some(*value),
        SqlValue::Null => None,
        SqlValue::Integer(value) => Some(*value != 0),
        SqlValue::Number(value) => Some(*value != 0.0),
        SqlValue::String(value) => Some(!value.is_empty()),
        SqlValue::Bytes(value) => Some(!value.is_empty()),
        // 数组没有布尔真值语义（PostgreSQL 不允许数组作谓词）。
        SqlValue::Array(_) => None,
    }
}

pub(crate) fn sql_and(left: &SqlValue, right: &SqlValue) -> SqlValue {
    match (sql_truth(left), sql_truth(right)) {
        (Some(false), _) | (_, Some(false)) => SqlValue::Boolean(false),
        (Some(true), Some(true)) => SqlValue::Boolean(true),
        _ => SqlValue::Null,
    }
}

pub(crate) fn sql_or(left: &SqlValue, right: &SqlValue) -> SqlValue {
    match (sql_truth(left), sql_truth(right)) {
        (Some(true), _) | (_, Some(true)) => SqlValue::Boolean(true),
        (Some(false), Some(false)) => SqlValue::Boolean(false),
        _ => SqlValue::Null,
    }
}
