//! Expression evaluation mechanism of the SQLite dialect.
//!
//! The resolved syntax trees live in `database_query::sqlite`; this module owns
//! their evaluation: the shared `EvalContext`, the row budget, the CTE scope
//! chain, the subquery memo cache and the traits that attach `eval` to the
//! resolved shapes. Nested `SELECT` execution stays with the product facade
//! engine and re-enters through the [`SqliteSubqueryRun`] seam.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap, HashSet},
};

use database_query::sqlite::{
    SqlError,
    bind::{ResolvedExpr, ReturningPlan, Scope, ScopeShape, SortDirection, TriggerFrame},
    frontend::{BinaryOp, CompareOp, Statement, UnaryOp},
    value::{Cell, ProbeKey, probe_key},
};
use database_storage::sqlite::Collation;
use database_udf::{Budget as UdfBudget, UdfContext, UdfImplementation};

use super::{
    cells::*,
    host::{HostFunctionTable, cell_to_udf_value, udf_error_to_sql_error, udf_value_to_cell},
    scalar::eval_function,
};
/// Memoized result of one uncorrelated subquery, valid for a single statement.
pub enum SubqueryCache {
    /// Rows of a scalar subquery or `EXISTS`, in evaluation order.
    Rows(Vec<Vec<Cell>>),
    /// Value set of `IN (SELECT ...)`; `keys` is `None` when a value has no
    /// canonical signature, so membership falls back to the linear scan.
    InList {
        /// Result values in evaluation order.
        values: Vec<Cell>,
        /// Canonical signatures of `values`, when every value has one.
        keys: Option<HashSet<ProbeKey>>,
        /// Whether any result row is `NULL`, which decides three-valued logic.
        saw_null: bool,
    },
}

/// Evaluation of a `RETURNING` projection against one written row.
pub trait EvalReturningPlan {
    /// Evaluates the projection against one written row.
    fn eval(&self, cells: &[Cell], ctx: &EvalContext) -> Result<Vec<Cell>, SqlError>;
}

impl EvalReturningPlan for ReturningPlan {
    fn eval(&self, cells: &[Cell], ctx: &EvalContext) -> Result<Vec<Cell>, SqlError> {
        self.items.iter().map(|item| item.eval(cells, ctx)).collect()
    }
}

/// Upper bound on rows emitted by one statement, shared with every nested
/// subquery. Counting emissions rather than wall-clock keeps the result
/// deterministic across machines.
const EXECUTION_BUDGET: usize = 1_000_000;

/// Row budget for one statement.
pub struct Budget {
    remaining: std::cell::Cell<usize>,
}

impl Budget {
    /// Opens a fresh statement budget.
    pub fn new() -> Self {
        Self { remaining: std::cell::Cell::new(EXECUTION_BUDGET) }
    }

    /// Charges row emissions against the shared budget.
    pub fn charge(&self, rows: usize) -> Result<(), SqlError> {
        let remaining = self.remaining.get();
        if remaining < rows {
            return Err(SqlError::new(
                "execution-limit",
                format!("statement exceeded the execution budget of {EXECUTION_BUDGET} rows, most likely from a correlated subquery over a large product"),
            )
            .in_statement("select"));
        }
        self.remaining.set(remaining - rows);
        Ok(())
    }
}

/// One CTE or derived table already reduced to rows.
pub struct Materialized {
    /// Column labels of the materialized result.
    pub columns: Vec<String>,
    /// Materialized rows.
    pub rows: Vec<Vec<Cell>>,
}

/// `WITH` entries in scope, chained so a nested query keeps seeing the outer
/// ones. A CTE shadows a base table with the same name.
pub struct CteEnv<'a> {
    /// Enclosing WITH scope, if any.
    pub parent: Option<&'a CteEnv<'a>>,
    /// Materialized entries of this scope, keyed by lowercase name.
    pub tables: BTreeMap<String, Materialized>,
}

impl<'a> CteEnv<'a> {
    /// Resolves one CTE name, walking outward through enclosing scopes.
    pub fn lookup(&self, name: &str) -> Option<&Materialized> {
        self.tables.get(&name.to_ascii_lowercase()).or_else(|| self.parent.and_then(|parent| parent.lookup(name)))
    }
}

/// Evaluation context shared by every expression of one `SELECT` core.
pub struct EvalContext<'a> {
    /// Bound `?` parameters of the running statement.
    pub parameters: &'a [Cell],
    /// The `excluded` row used while resolving an upsert conflict.
    pub upsert_excluded: &'a [Cell],
    /// Frames of the trigger body currently executing.
    pub trigger_frames: &'a [TriggerFrame],
    /// Engine switch that routes `CURRENT OF` reads to the outer statement.
    pub outer_read: &'a std::cell::Cell<bool>,
    /// Memoized uncorrelated subquery results of the running statement.
    pub subquery_cache: &'a std::cell::RefCell<HashMap<usize, SubqueryCache>>,
    /// Host-registered scalar and table-valued functions of the session.
    pub host_functions: &'a HostFunctionTable,
    /// Seam that runs nested `SELECT`s through the host engine.
    pub subquery: &'a dyn SqliteSubqueryRun,
    /// Enclosing scopes, innermost first.
    pub outer: &'a [Scope],
    /// Column catalog of the core currently being evaluated.
    pub current: &'a ScopeShape,
    /// Whole-result aggregate values for the current row.
    pub aggregates: &'a [Cell],
    /// Already computed window values, one vector per window of this core.
    pub windows: &'a [Vec<Cell>],
    /// Row currently being projected, indexing into `windows`.
    pub window_row: usize,
    /// Row budget shared by the statement and its subqueries.
    pub budget: &'a Budget,
    /// CTEs visible to the current core, including enclosing `WITH` clauses.
    pub ctes: Option<&'a CteEnv<'a>>,
}

/// Key comparison honouring one resolved sort direction.
pub trait SortCompare {
    /// Compares two cells, honouring the direction and the `NULL` placement.
    fn compare(self, left: &Cell, right: &Cell) -> Ordering;
}

impl SortCompare for SortDirection {
    fn compare(self, left: &Cell, right: &Cell) -> Ordering {
        match (left, right) {
            (Cell::Null, Cell::Null) => Ordering::Equal,
            (Cell::Null, _) | (_, Cell::Null) => {
                let first = self.nulls_first.unwrap_or(!self.descending);
                if matches!(left, Cell::Null) == first { Ordering::Less } else { Ordering::Greater }
            }
            _ => {
                let ordering = compare_cells_with(left, right, self.collation);
                if self.descending { ordering.reverse() } else { ordering }
            }
        }
    }
}

/// Evaluation of one resolved expression against a row of the current scopes.
pub trait EvalExpr {
    /// Read-only evaluation: literals and bound columns borrow their cell
    /// directly without copying.
    fn eval_borrowed<'a>(&'a self, values: &'a [Cell]) -> Option<&'a Cell>;

    /// Evaluates this expression against one merged row.
    fn eval(&self, values: &[Cell], ctx: &EvalContext) -> Result<Cell, SqlError>;
}

impl EvalExpr for ResolvedExpr {
    /// 只读求值:字面量与已绑定列直接借出,不复制单元格。
    ///
    /// 只读算子(比较、`IS NULL`、`LIKE`)把操作数按引用交给下游比较函数,
    /// 因此逐行扫描不必为每个文本操作数克隆一份 `String`。其余形态返回
    /// `None`,由 [`ResolvedExpr::eval`] 走完整求值。
    fn eval_borrowed<'a>(&'a self, values: &'a [Cell]) -> Option<&'a Cell> {
        match self {
            Self::Cell(cell) => Some(cell),
            Self::Column { index, .. } => values.get(*index),
            _ => None,
        }
    }

    /// Evaluates this expression against one merged row.
    ///
    /// Every position (projection, `WHERE`, `ORDER BY`, nested subquery) goes
    /// through this single function, so SQLite's NULL propagation, integer
    /// arithmetic and correlation rules cannot diverge between clauses.
    fn eval(&self, values: &[Cell], ctx: &EvalContext) -> Result<Cell, SqlError> {
        Ok(match self {
            Self::Cell(cell) => cell.clone(),
            Self::Parameter(index) => ctx.parameters.get(*index).cloned().unwrap_or(Cell::Null),
            Self::Column { index, .. } => values[*index].clone(),
            Self::TriggerColumn { old, index } => ctx
                .trigger_frames
                .last()
                .and_then(|frame| if *old { frame.old.as_ref() } else { frame.new.as_ref() })
                .and_then(|cells| cells.get(*index))
                .cloned()
                .unwrap_or(Cell::Null),
            Self::ExcludedColumn { index } => ctx.upsert_excluded.get(*index).cloned().unwrap_or(Cell::Null),
            Self::OuterColumn { depth, index } => {
                ctx.outer_read.set(true);
                ctx.outer[*depth].columns[*index].value.clone()
            }
            Self::AggregateRef(index) => ctx.aggregates[*index].clone(),
            Self::Window(index) => ctx.windows.get(*index).and_then(|column| column.get(ctx.window_row)).cloned().unwrap_or(Cell::Null),
            Self::Unary { op, value } => {
                let value = value.eval(values, ctx)?;
                match op {
                    UnaryOp::Negate => negate_cell(&value),
                    UnaryOp::Plus => value,
                    UnaryOp::BitNot => bit_not(&value),
                }
            }
            Self::Binary { left, op: BinaryOp::Concat, right } => concat(&left.eval(values, ctx)?, &right.eval(values, ctx)?),
            Self::Binary { left, op: BinaryOp::Arrow, right } => {
                super::json::arrow(&left.eval(values, ctx)?, &right.eval(values, ctx)?, false)?
            }
            Self::Binary { left, op: BinaryOp::ArrowText, right } => {
                super::json::arrow(&left.eval(values, ctx)?, &right.eval(values, ctx)?, true)?
            }
            Self::Binary { left, op, right } => arithmetic(*op, &left.eval(values, ctx)?, &right.eval(values, ctx)?),
            Self::Compare { left, op, right, collation } => {
                // 字面量与列操作数按引用比较:逐行扫描因此不克隆任何文本。
                let comparison = match (left.eval_borrowed(values), right.eval_borrowed(values)) {
                    (Some(left), Some(right)) => compare_values(left, right, *op, *collation),
                    _ => compare_values(&left.eval(values, ctx)?, &right.eval(values, ctx)?, *op, *collation),
                };
                match comparison {
                    Some(true) => Cell::Integer(1),
                    Some(false) => Cell::Integer(0),
                    None => Cell::Null,
                }
            }
            Self::IsNull { value, negated } => {
                let is_null = match value.eval_borrowed(values) {
                    Some(cell) => matches!(cell, Cell::Null),
                    None => matches!(value.eval(values, ctx)?, Cell::Null),
                };
                Cell::Integer(i64::from(is_null != *negated))
            }
            Self::IsTruth { value, truth, negated } => {
                // `IS TRUE` is the SQLite truthiness test, not a comparison with 1,
                // and it is never NULL: a NULL operand is simply not true (nor false).
                let is_truth = truthiness(&value.eval(values, ctx)?) == Some(*truth);
                Cell::Integer(i64::from(is_truth != *negated))
            }
            Self::Not(inner) => match truthiness(&inner.eval(values, ctx)?) {
                Some(value) => Cell::Integer(i64::from(!value)),
                None => Cell::Null,
            },
            // `AND` / `OR` 短路求值,与 SQLite 一致:左侧已能定案时不求值右侧,
            // 既省掉一个操作数的求值,也让 `a = 1 OR b = 2` 这类谓词在命中的行上
            // 少做一次比较。
            Self::And(left, right) => {
                let left = truthiness(&left.eval(values, ctx)?);
                if left == Some(false) {
                    return Ok(Cell::Integer(0));
                }
                match (left, truthiness(&right.eval(values, ctx)?)) {
                    (Some(false), _) | (_, Some(false)) => Cell::Integer(0),
                    (Some(true), Some(true)) => Cell::Integer(1),
                    _ => Cell::Null,
                }
            }
            Self::Or(left, right) => {
                let left = truthiness(&left.eval(values, ctx)?);
                if left == Some(true) {
                    return Ok(Cell::Integer(1));
                }
                match (left, truthiness(&right.eval(values, ctx)?)) {
                    (Some(true), _) | (_, Some(true)) => Cell::Integer(1),
                    (Some(false), Some(false)) => Cell::Integer(0),
                    _ => Cell::Null,
                }
            }
            Self::Case { operand, branches, otherwise, collation } => {
                match operand {
                    None => {
                        for (condition, result) in branches {
                            if truthiness(&condition.eval(values, ctx)?) == Some(true) {
                                return result.eval(values, ctx);
                            }
                        }
                    }
                    Some(operand) => {
                        let base = operand.eval(values, ctx)?;
                        for (candidate, result) in branches {
                            // 分支自带的显式 `COLLATE` 优先于 CASE 操作数的列声明。
                            let branch_collation = collation.or_else(|| match candidate {
                                Self::Collate { collation, .. } => Some(*collation),
                                _ => None,
                            });
                            if compare_values(&base, &candidate.eval(values, ctx)?, CompareOp::Eq, branch_collation.unwrap_or_default())
                                == Some(true)
                            {
                                return result.eval(values, ctx);
                            }
                        }
                    }
                }
                match otherwise {
                    Some(expr) => expr.eval(values, ctx)?,
                    None => Cell::Null,
                }
            }
            Self::In { value, list, negated, collation, constant } => {
                let value = value.eval(values, ctx)?;
                if matches!(value, Cell::Null) {
                    return Ok(Cell::Null);
                }
                if let Some(constant) = constant {
                    let found = probe_key(&value, Collation::Binary).is_some_and(|key| constant.keys.contains(&key));
                    return Ok(if found {
                        Cell::Integer(i64::from(!*negated))
                    }
                    else if constant.saw_null {
                        Cell::Null
                    }
                    else {
                        Cell::Integer(i64::from(*negated))
                    });
                }
                let mut saw_null = false;
                for item in list {
                    let item_value = item.eval(values, ctx)?;
                    if matches!(item_value, Cell::Null) {
                        saw_null = true;
                        continue;
                    }
                    // 左操作数的显式 `COLLATE` 优先；否则取列表项自带的显式规则。
                    let item_collation = match collation {
                        Some(collation) => *collation,
                        None => match item {
                            Self::Collate { collation, .. } => *collation,
                            _ => Collation::Binary,
                        },
                    };
                    if compare_values(&value, &item_value, CompareOp::Eq, item_collation) == Some(true) {
                        return Ok(Cell::Integer(i64::from(!*negated)));
                    }
                }
                if saw_null { Cell::Null } else { Cell::Integer(i64::from(*negated)) }
            }
            Self::Function { name, args } => {
                let values = args.iter().map(|arg| arg.eval(values, ctx)).collect::<Result<Vec<_>, _>>()?;
                if name.is_json() {
                    // SQLite 给 JSON 函数的结果打上 JSON 子类型，嵌套进
                    // `json_array` / `json_object` / `json_set` 时按 JSON 值而不是
                    // 普通文本处理。这里同样标记直接调用 JSON 函数的实参。
                    let json_arguments =
                        args.iter().map(|arg| matches!(arg, Self::Function { name, .. } if name.is_json())).collect::<Vec<_>>();
                    super::json::eval(*name, &values, &json_arguments)?
                }
                else {
                    eval_function(*name, &values)
                }
            }
            Self::HostFunction { name, args } => {
                let values = args.iter().map(|arg| arg.eval(values, ctx)).collect::<Result<Vec<_>, _>>()?;
                let implementation =
                    ctx.host_functions.get(name).ok_or_else(|| SqlError::new("unknown-function", format!("no such function: {name}")))?;
                let arguments = values.iter().map(|cell| cell_to_udf_value(name, cell)).collect::<Result<Vec<_>, _>>()?;
                let mut context = UdfContext::new(UdfBudget::new(ctx.budget.remaining.get() as u64));
                implementation.invoke(&mut context, &arguments).map_err(|error| udf_error_to_sql_error(name, error)).map(udf_value_to_cell)?
            }
            Self::Cast { value, target } => cast_cell(&value.eval(values, ctx)?, *target),
            Self::Collate { value, .. } => value.eval(values, ctx)?,
            Self::IsDistinct { left, right, negated, collation } => {
                let left = left.eval(values, ctx)?;
                let right = right.eval(values, ctx)?;
                // `NULL` is a value again: two `NULL`s are not distinct, while a
                // `NULL` against anything else is. Non-NULL operands use the same
                // storage-class ordering as `=`, so `1` and `1.0` are the same.
                let distinct = match (&left, &right) {
                    (Cell::Null, Cell::Null) => false,
                    (Cell::Null, _) | (_, Cell::Null) => true,
                    _ => compare_cells_with(&left, &right, *collation) != Ordering::Equal,
                };
                Cell::Integer(i64::from(distinct != *negated))
            }
            Self::Glob { value, pattern, negated } => {
                let value = value.eval(values, ctx)?;
                let pattern = pattern.eval(values, ctx)?;
                // Both operands must be text: `NULL` propagates, and a blob never
                // matches, unlike `LIKE` which unblobs its operand.
                if matches!(value, Cell::Null) || matches!(pattern, Cell::Null) {
                    return Ok(Cell::Null);
                }
                let matched = match (&value, &pattern) {
                    (Cell::Blob(_), _) | (_, Cell::Blob(_)) => false,
                    _ => {
                        let value: Vec<char> = text_form(&value).unwrap_or_default().chars().collect();
                        let pattern: Vec<char> = text_form(&pattern).unwrap_or_default().chars().collect();
                        glob_match(&value, &pattern)
                    }
                };
                Cell::Integer(i64::from(matched != *negated))
            }
            Self::Like { value, pattern, negated, escape } => {
                // 两个操作数按引用取出,避免逐行克隆文本(全表 `LIKE` 扫描的热点)。
                let owned_value;
                let value = match value.eval_borrowed(values) {
                    Some(cell) => cell,
                    None => {
                        owned_value = value.eval(values, ctx)?;
                        &owned_value
                    }
                };
                let owned_pattern;
                let pattern = match pattern.eval_borrowed(values) {
                    Some(cell) => cell,
                    None => {
                        owned_pattern = pattern.eval(values, ctx)?;
                        &owned_pattern
                    }
                };
                let escape = match escape {
                    Some(escape) => {
                        let text = text_form(&escape.eval(values, ctx)?);
                        match text.as_deref().map(|text| text.chars().collect::<Vec<_>>()) {
                            Some(chars) if chars.len() == 1 => Some(chars[0]),
                            Some(_) => {
                                return Err(
                                    SqlError::new("invalid-escape", "ESCAPE expression must be a single character").in_statement("select")
                                );
                            }
                            None => None,
                        }
                    }
                    None => None,
                };
                // `LIKE` with a NULL value, pattern or escape yields NULL.
                // Text operands match straight off the stored strings, which
                // keeps a full-table `LIKE` scan from allocating a `String`
                // copy and two `Vec<char>` per row. 纯前缀模式(`abc%`、`abc`)
                // 再走一次前缀比较,不必启动逐字符匹配器。
                let matched = match (value, pattern) {
                    (Cell::Text(value), Cell::Text(pattern)) => match like_plain_prefix(pattern, escape) {
                        Some((prefix, open)) => like_prefix_text(value, prefix, open),
                        None => like_match_text(value, pattern, escape),
                    },
                    _ => {
                        let (Some(value), Some(pattern)) = (text_form(value), text_form(pattern))
                        else {
                            return Ok(Cell::Null);
                        };
                        let characters: Vec<char> = value.chars().collect();
                        let pattern: Vec<char> = pattern.chars().collect();
                        like_match(&characters, &pattern, escape)
                    }
                };
                Cell::Integer(i64::from(matched != *negated))
            }
            Self::Subquery(query) => {
                let key = query.as_ref() as *const Statement as usize;
                {
                    let cache = ctx.subquery_cache.borrow();
                    if let Some(SubqueryCache::Rows(rows)) = cache.get(&key) {
                        return Ok(rows.first().map_or(Cell::Null, |row| row[0].clone()));
                    }
                }
                ctx.outer_read.set(false);
                let result = run_subquery(query, values, ctx)?;
                check_single_column(&result)?;
                if !ctx.outer_read.get() {
                    ctx.subquery_cache.borrow_mut().insert(key, SubqueryCache::Rows(result.rows.clone()));
                }
                result.rows.first().map_or(Cell::Null, |row| row[0].clone())
            }
            Self::Exists(query) => {
                // Existence is two-valued: no result value is consumed, so a
                // surrounding `NOT` can never turn this into `NULL`.
                let key = query.as_ref() as *const Statement as usize;
                {
                    let cache = ctx.subquery_cache.borrow();
                    if let Some(SubqueryCache::Rows(rows)) = cache.get(&key) {
                        return Ok(Cell::Integer(i64::from(!rows.is_empty())));
                    }
                }
                ctx.outer_read.set(false);
                if let Some(answer) = ctx.subquery.correlated_exists(query, values, ctx)? {
                    return Ok(Cell::Integer(i64::from(answer)));
                }
                let result = run_subquery(query, values, ctx)?;
                if !ctx.outer_read.get() {
                    ctx.subquery_cache.borrow_mut().insert(key, SubqueryCache::Rows(result.rows.clone()));
                }
                Cell::Integer(i64::from(!result.rows.is_empty()))
            }
            Self::InSubquery { value, query, negated } => {
                let value = value.eval(values, ctx)?;
                let key = query.as_ref() as *const Statement as usize;
                {
                    let cache = ctx.subquery_cache.borrow();
                    if let Some(SubqueryCache::InList { values: list, keys, saw_null }) = cache.get(&key) {
                        return Ok(in_result_cached(&value, list, keys.as_ref(), *saw_null, *negated));
                    }
                }
                ctx.outer_read.set(false);
                let result = run_subquery(query, values, ctx)?;
                check_single_column(&result)?;
                let correlated = ctx.outer_read.get();
                let mut list = Vec::with_capacity(result.rows.len());
                let mut saw_null = false;
                for row in &result.rows {
                    let cell = row[0].clone();
                    if matches!(cell, Cell::Null) {
                        saw_null = true;
                    }
                    list.push(cell);
                }
                let keys = in_list_keys(&list);
                let answer = in_result_cached(&value, &list, keys.as_ref(), saw_null, *negated);
                if !correlated {
                    ctx.subquery_cache.borrow_mut().insert(key, SubqueryCache::InList { values: list, keys, saw_null });
                }
                answer
            }
        })
    }
}

/// Runs one nested `SELECT` with the current row as the innermost enclosing
/// scope.
///
/// The subquery reuses the same `select` evaluator, so its `FROM`, predicates,
/// ordering, compounds and further nesting all behave exactly like a top-level
/// statement.
fn run_subquery(query: &Statement, values: &[Cell], ctx: &EvalContext) -> Result<SubqueryOutcome, SqlError> {
    ctx.budget.charge(1)?;
    let mut scopes = Vec::with_capacity(ctx.outer.len() + 1);
    scopes.push(ctx.current.instantiate(values));
    scopes.extend(ctx.outer.iter().cloned());
    // The subquery keeps the CTEs that are in scope where it appears, so
    // `(select ... from c)` inside a `WITH` statement resolves too.
    ctx.subquery.select_statement(query, &scopes, ctx.budget, ctx.ctes)
}

/// A scalar subquery and an `IN (SELECT ...)` must produce exactly one column,
/// as upstream SQLite enforces at prepare time.
fn check_single_column(result: &SubqueryOutcome) -> Result<(), SqlError> {
    if result.columns.len() != 1 {
        return Err(
            SqlError::new("arity-mismatch", format!("sub-select returns {} columns - expected 1", result.columns.len())).in_statement("select")
        );
    }
    Ok(())
}
/// Rows produced by one nested `SELECT` run through the host engine.
pub struct SubqueryOutcome {
    /// Column labels of the produced result.
    pub columns: Vec<String>,
    /// Produced rows.
    pub rows: Vec<Vec<Cell>>,
}

/// Seam through which the evaluation machinery runs nested `SELECT`s and the
/// correlated-`EXISTS` index probe. The statement machinery itself still lives
/// with the product facade engine, which implements this trait with a private
/// adapter that delegates to it.
pub trait SqliteSubqueryRun {
    /// Runs one nested statement with `scopes` as the enclosing scope chain.
    fn select_statement(
        &self,
        query: &Statement,
        scopes: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv<'_>>,
    ) -> Result<SubqueryOutcome, SqlError>;

    /// Index-probe fast path for a correlated `EXISTS`; `None` when the probe
    /// shape does not apply and the full subquery must run instead.
    fn correlated_exists(&self, query: &Statement, values: &[Cell], ctx: &EvalContext<'_>) -> Result<Option<bool>, SqlError>;
}
