//! SELECT 执行机器:SQLite 方言的语句级读路径。
//!
//! 本模块树拥有 `SELECT`(含集合运算、递归 CTE、视图展开、惰性游标与相关子查询)
//! 的全部执行机制。产品门面的 `Engine` 只保留写入、目录维护与语句分发,经
//! [`ExecContext`] 借用这里的实现;本模块树不得反向依赖任何产品门面。

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    rc::Rc,
    sync::Arc,
};

use database_formats::sqlite::{RecordLimits, validate_database};
use database_query::sqlite::{SqlError, bind::*, catalog::*, frontend::*, storage_failure, value::*};
use database_storage::sqlite::{
    Affinity, Collation, SchemaObjectKind, SqliteWriter, TableLimits, TableRow, WriteValue, affinity_of, apply_write_affinity, decode_varint,
};

use super::{
    cells::*,
    eval::{Budget, CteEnv, EvalContext, EvalExpr, Materialized, SortCompare, SubqueryCache, SubqueryOutcome},
    host::HostFunctionTable,
    probe::*,
    window::*,
};

thread_local! {
    /// Views can be nested, but a view that reaches itself would recurse
    /// forever. SQLite rejects the circular definition at prepare time; this
    /// counter turns the same case into one structured error instead.
    static VIEW_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Decrements the view depth when the nested evaluation returns, including on
/// an error path.
pub struct ViewDepthGuard;

impl ViewDepthGuard {
    /// Enters one view-expansion level, rejecting nesting deeper than SQLite's
    /// own limit.
    pub fn enter() -> Result<Self, SqlError> {
        VIEW_DEPTH.with(|depth| {
            depth.set(depth.get() + 1);
            if depth.get() > 8 {
                depth.set(depth.get() - 1);
                return Err(SqlError::new("view-depth", "view nesting depth exceeded"));
            }
            Ok(Self)
        })
    }
}

impl Drop for ViewDepthGuard {
    fn drop(&mut self) {
        VIEW_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Which schema a statement resolves its unqualified names in. The default
/// follows SQLite's search order (temp, then main, then attached databases).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TableScope {
    /// Default search order across temp, main and attached schemas.
    Default,
    /// Names resolve in `main` only.
    Main,
    /// Names resolve in `temp` only.
    Temp,
}

/// Reserved table names of one schema. `main.` never reaches the temp master
/// and `temp.` never reaches the main master, matching SQLite's isolation.
pub fn system_for_scope(scope: TableScope, name: &str) -> Option<SystemTable> {
    let system = SystemTable::from_name(name)?;
    match (scope, system) {
        (TableScope::Main, SystemTable::TempMaster) => None,
        (TableScope::Temp, SystemTable::Master | SystemTable::Sequence) => None,
        _ => Some(system),
    }
}

/// 门面 `Engine` 与 [`ExecContext`] 共享的语句级缓存状态。
///
/// 这些缓存跨语句存活、按写世代失效;所有权在门面,求值只经 [`ExecContext`]
/// 借用,不复制。
#[derive(Default)]
pub struct SqliteExecState {
    /// 单调递增的时钟,给索引内容版本取号用。
    ///
    /// 只有真正改动了某个索引内容的语句才推进它,逐语句推进会让写循环里
    /// 每次探针都重建整份索引键集。
    pub write_epoch: std::cell::Cell<u64>,
    /// 索引内容版本:小写索引名 → 该索引内容最后变化时的 epoch。
    pub index_versions: std::cell::RefCell<HashMap<String, u64>>,
    /// Live key sets of plain column indexes, keyed by lowercase index name.
    pub index_probes: std::cell::RefCell<HashMap<String, IndexProbe>>,
    /// Memoized uncorrelated subquery results for the running statement.
    pub subquery_cache: std::cell::RefCell<HashMap<usize, SubqueryCache>>,
    /// 相关 `EXISTS` 的索引探针计划,按语句内 AST 地址缓存。
    pub exists_probes: std::cell::RefCell<HashMap<usize, Option<ExistsProbePlan>>>,
    /// 单条语句内已解码的整表行,按小写表名索引,并记录该份行集覆盖的列掩码。
    pub statement_scans: std::cell::RefCell<HashMap<String, (Option<Vec<bool>>, std::rc::Weak<Vec<Vec<Cell>>>)>>,
    /// Set whenever an `OuterColumn` is read, so a subquery can tell whether it
    /// depended on the enclosing row and therefore must not be memoized.
    pub outer_read: std::cell::Cell<bool>,
}

/// 一条语句的执行上下文。
///
/// 门面 `Engine` 在分发语句时把自身的只读视图注入这里;SELECT 机器全部住在
/// 本模块树中,按引用共享目录、写入器与缓存,可廉价克隆下传嵌套求值。
#[derive(Clone)]
pub struct ExecContext<'a> {
    /// 存储写入器:页镜像与行数据的只读入口。
    pub writer: &'a SqliteWriter,
    /// 当前 schema 目录。
    pub catalog: &'a SqliteCatalog,
    /// 跨语句缓存状态,所有权在门面。
    pub state: &'a SqliteExecState,
    /// 会话宿主函数表。
    pub host_functions: &'a HostFunctionTable,
    /// 绑定到 `?` / `:name` 占位符的参数。
    pub parameters: &'a [Cell],
    /// `ON CONFLICT DO UPDATE` 的 `excluded` 行。
    pub upsert_excluded: &'a [Cell],
    /// 正在执行的触发器体帧,最外层在前。
    pub trigger_frames: &'a [TriggerFrame],
    /// 当前语句的表别名(`CURRENT OF` 判定)。
    pub alias: Option<&'a str>,
    /// `ATTACH DATABASE` 的每个附加库各自的执行上下文,按 attach 顺序。
    pub attached: Rc<[ExecContext<'a>]>,
    /// 小写附加库 schema 名到 `attached` 下标的映射。
    pub attached_lookup: &'a BTreeMap<String, usize>,
}

/// Maps a storage failure onto the engine error surface.
pub fn storage_error(error: impl std::fmt::Display) -> SqlError {
    SqlError::new("storage", error.to_string()).in_statement("select")
}

/// A malformed on-disk structure found by the lazy scan.
pub fn corrupt_error(message: &'static str) -> SqlError {
    SqlError::new("corrupt", message).in_statement("select")
}

/// Reserved tables SQLite keeps in the schema itself and that this engine
/// exposes as read-only sources.
#[derive(Clone, Copy)]
pub enum SystemTable {
    /// `sqlite_master`, also reachable as `sqlite_schema`.
    Master,
    /// `sqlite_temp_master`, always empty because temporary objects are not
    /// created by this surface.
    TempMaster,
    /// `sqlite_sequence`, present only after an AUTOINCREMENT table.
    Sequence,
}

impl SystemTable {
    /// Maps a reserved table name to its system table, case-insensitively.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "sqlite_master" | "sqlite_schema" => Some(Self::Master),
            "sqlite_temp_master" | "sqlite_temp_schema" => Some(Self::TempMaster),
            "sqlite_sequence" => Some(Self::Sequence),
            _ => None,
        }
    }
}

/// Output names of one view: the declared list when present, otherwise the
/// names the query produced. SQLite rejects a declared list whose width does
/// not match the query, and suffixes repeated names with `:1`, `:2`, ...
pub fn view_output_names(view: &ViewDef, actual: &[String]) -> Result<Vec<String>, SqlError> {
    let names = match &view.columns {
        Some(declared) => {
            if declared.len() != actual.len() {
                return Err(SqlError::new(
                    "view-arity",
                    format!("expected {} columns for '{}' but got {}", declared.len(), view.name, actual.len()),
                )
                .in_statement("select"));
            }
            declared.clone()
        }
        None => actual.to_vec(),
    };
    Ok(unique_output_names(&names))
}

/// Makes one result set's column names unique the way SQLite does for views,
/// derived tables and CTEs: a repeated name gets `:1`, `:2`, ... appended.
pub fn unique_output_names(names: &[String]) -> Vec<String> {
    let mut used = BTreeSet::new();
    let mut out = Vec::with_capacity(names.len());
    for name in names {
        let key = name.to_ascii_lowercase();
        if used.insert(key) {
            out.push(name.clone());
            continue;
        }
        let mut suffix = 1usize;
        loop {
            let candidate = format!("{name}:{suffix}");
            if used.insert(candidate.to_ascii_lowercase()) {
                out.push(candidate);
                break;
            }
            suffix += 1;
        }
    }
    out
}

/// Evaluates an optional view-DML predicate against one output row.
pub fn row_matches(
    predicate: &Option<ResolvedExpr>,
    row: &[Cell],
    exec: &ExecContext<'_>,
    shape: &ScopeShape,
    budget: &Budget,
) -> Result<bool, SqlError> {
    let Some(predicate) = predicate
    else {
        return Ok(true);
    };
    let ctx = EvalContext {
        exec: exec.clone(),
        parameters: exec.parameters,
        upsert_excluded: exec.upsert_excluded,
        trigger_frames: exec.trigger_frames,
        outer_read: &exec.state.outer_read,
        subquery_cache: &exec.state.subquery_cache,
        host_functions: exec.host_functions,
        outer: &[],
        current: shape,
        aggregates: &[],
        windows: &[],
        window_row: 0,
        budget,
        ctes: None,
    };
    Ok(truthiness(&predicate.eval(row, &ctx)?) == Some(true))
}

/// Compares two rows under the resolved sort keys of an ORDER BY / window order.
pub(crate) fn compare_key_rows(left: &[Cell], right: &[Cell], keys: &[(SortKey, SortDirection)]) -> Ordering {
    for (index, (_, direction)) in keys.iter().enumerate() {
        let ordering = direction.compare(&left[index], &right[index]);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// 一个 `SELECT` 核心的结果列标签。
///
/// 执行路径与 `describe` 共用本函数,保证预编译期推导的表头与
/// 执行后 `SubqueryOutcome.columns` 逐字节一致:裸列引用按目录拼写命名(所以
/// `SELECT A FROM t` 报 `a`),别名取书写名,表达式列由 `Expr::label` 重建
/// 文本,`*` 按 `FROM` 源的可见列展开(含 `USING` 合并列的隐藏规则)。
pub fn core_output_labels(relation: &Relation<'_>, items: &SelectItems) -> Vec<String> {
    match items {
        SelectItems::Named(exprs) => exprs
            .iter()
            .map(|expr| match expr {
                Expr::Column(reference) => relation.resolved_column_name(reference).unwrap_or_else(|| expr.label()),
                _ => expr.label(),
            })
            .collect(),
        SelectItems::CountAll => vec!["count(*)".to_string()],
        SelectItems::Star => relation.star_columns(),
    }
}

/// Resolves the trailing `ORDER BY` of a compound result.
///
/// SQLite only lets a compound query order by an output column, either by
/// 1-based ordinal or by its result label; source columns are out of scope
/// once the cores have been combined.
pub fn compound_order_keys(order: &[OrderKey], columns: &[String]) -> Result<Vec<(usize, SortDirection)>, SqlError> {
    order
        .iter()
        .map(|entry| {
            if let Expr::Literal(Literal::Integer(position)) = &entry.expr {
                let position = *position;
                if position < 1 || position as usize > columns.len() {
                    return Err(SqlError::new(
                        "order-term-out-of-range",
                        format!("ORDER BY term out of range - should be between 1 and {}", columns.len()),
                    )
                    .in_statement("select"));
                }
                return Ok((position as usize - 1, SortDirection::of(entry)));
            }
            if let Expr::Column(reference) = &entry.expr {
                if reference.qualifier.is_none() {
                    let matching = columns
                        .iter()
                        .enumerate()
                        .filter(|(_, column)| column.eq_ignore_ascii_case(&reference.name))
                        .map(|(index, _)| index)
                        .collect::<Vec<_>>();
                    return match matching.as_slice() {
                        [index] => Ok((*index, SortDirection::of(entry))),
                        [] => Err(SqlError::new(
                            "unknown-column",
                            format!("ORDER BY term `{}` does not match any column in the result set", reference.name),
                        )
                        .in_statement("select")),
                        _ => {
                            Err(SqlError::new("ambiguous-column", format!("ORDER BY term `{}` is ambiguous in the result set", reference.name))
                                .in_statement("select"))
                        }
                    };
                }
            }
            let mut error = SqlError::new("unsupported-clause", "a compound `SELECT` can only order by an output column name or ordinal")
                .in_statement("select");
            error.clause = Some("ORDER BY".to_string());
            Err(error)
        })
        .collect()
}

/// Two result rows are equal when every cell compares equal under the engine's
/// value rules; `NULL` equals `NULL`, as set operations require.
pub(crate) fn rows_equal(left: &[Cell], right: &[Cell]) -> bool {
    left.len() == right.len() && left.iter().zip(right).all(|(first, second)| compare_cells(first, second) == Ordering::Equal)
}

pub(crate) fn rows_contain(rows: &[Vec<Cell>], candidate: &[Cell]) -> bool {
    rows.iter().any(|row| rows_equal(row, candidate))
}

/// Removes duplicate rows while keeping the first occurrence of each value.
pub(crate) fn distinct_rows(rows: &[Vec<Cell>]) -> Vec<Vec<Cell>> {
    retain_first_distinct(rows.to_vec(), &[])
}

/// Applies one set operator with SQLite's multiplicity rules.
pub(crate) fn combine_rows(op: CompoundOp, left: Vec<Vec<Cell>>, right: Vec<Vec<Cell>>) -> Vec<Vec<Cell>> {
    match op {
        CompoundOp::UnionAll => left.into_iter().chain(right).collect(),
        CompoundOp::Union => {
            let mut rows = left;
            rows.extend(right);
            distinct_rows(&rows)
        }
        CompoundOp::Intersect => {
            let right_rows = distinct_rows(&right);
            distinct_rows(&left).into_iter().filter(|row| rows_contain(&right_rows, row)).collect()
        }
        CompoundOp::Except => {
            let right_rows = distinct_rows(&right);
            distinct_rows(&left).into_iter().filter(|row| !rows_contain(&right_rows, row)).collect()
        }
    }
}

/// The error SQLite raises when a recursive `WITH` entry reads itself in a
/// place the fixed-point evaluator cannot handle.
pub(crate) fn circular_reference(name: &str) -> SqlError {
    SqlError::new("SQLITE_ERROR", format!("circular reference: {name}")).in_statement("select")
}

/// Whether a projection turns each input row into at most one output row, so a
/// recursive CTE may be truncated at the enclosing `LIMIT`.
pub(crate) fn row_preserving_items(items: &SelectItems) -> bool {
    match items {
        SelectItems::Star => true,
        SelectItems::CountAll => false,
        SelectItems::Named(exprs) => exprs.iter().all(row_preserving_expr),
    }
}

pub(crate) fn row_preserving_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Column(_) | Expr::Literal(_) | Expr::Parameter { .. } => true,
        Expr::Unary { value, .. } => row_preserving_expr(value),
        Expr::Binary { left, right, .. } | Expr::Compare { left, right, .. } | Expr::And(left, right) | Expr::Or(left, right) => {
            row_preserving_expr(left) && row_preserving_expr(right)
        }
        Expr::IsDistinct { left, right, .. } => row_preserving_expr(left) && row_preserving_expr(right),
        Expr::Glob { value, pattern, .. } => row_preserving_expr(value) && row_preserving_expr(pattern),
        Expr::IsNull { value, .. } | Expr::IsTruth { value, .. } | Expr::Not(value) => row_preserving_expr(value),
        Expr::Case { operand, branches, otherwise } => {
            operand.as_ref().map_or(true, |expr| row_preserving_expr(expr))
                && branches.iter().all(|(condition, result)| row_preserving_expr(condition) && row_preserving_expr(result))
                && otherwise.as_ref().map_or(true, |expr| row_preserving_expr(expr))
        }
        Expr::In { value, list, .. } => row_preserving_expr(value) && list.iter().all(row_preserving_expr),
        Expr::Function { args, .. } | Expr::HostFunction { args, .. } => args.iter().all(row_preserving_expr),
        Expr::Alias { value, .. } | Expr::Cast { value, .. } | Expr::Collate { value, .. } => row_preserving_expr(value),
        Expr::Like { value, pattern, escape, .. } => {
            row_preserving_expr(value) && row_preserving_expr(pattern) && escape.as_ref().map_or(true, |expr| row_preserving_expr(expr))
        }
        _ => false,
    }
}

/// Drops the rows a limit pair hides.
pub(crate) fn truncate_rows(rows: &mut Vec<Vec<Cell>>, limit: Option<(usize, Option<usize>)>) {
    if let Some(limit) = limit {
        let (start, end) = limit_range(rows.len(), Some(limit));
        rows.truncate(end);
        rows.drain(..start);
    }
}

/// Keeps the first row for every distinct output value, preserving order.
///
/// Values are compared with `Cell`'s own equality, the same semantics grouping
/// uses, so integers and reals stay distinct exactly as SQLite keeps them.
pub(crate) fn retain_first_distinct(rows: Vec<Vec<Cell>>, collations: &[Collation]) -> Vec<Vec<Cell>> {
    // Single-column output needs one signature, not a `Vec<ProbeKey>` per row.
    if rows.first().is_some_and(|row| row.len() == 1) {
        let collation = collations.first().copied().unwrap_or_default();
        let mut buckets: HashSet<ProbeKey> = HashSet::with_capacity(rows.len());
        // 保留下标而不是整行副本:线性回退时直接与已保留的行比较,
        // 命中哈希的常见路径不再为每行复制一份 `Vec<Cell>`。
        let mut seen: Vec<usize> = Vec::with_capacity(rows.len());
        let mut out: Vec<Vec<Cell>> = Vec::with_capacity(rows.len());
        let mut linear = false;
        for row in rows {
            let signature = if linear {
                None
            }
            else {
                match probe_key(&row[0], collation) {
                    Some(part) if !matches!(part, ProbeKey::Wide(_)) => Some(part),
                    _ => None,
                }
            };
            let duplicate = match &signature {
                Some(signature) => buckets.contains(signature),
                None => {
                    linear = true;
                    seen.iter().any(|index| compare_cells_with(&out[*index][0], &row[0], collation) == Ordering::Equal)
                }
            };
            if !duplicate {
                if let Some(signature) = signature {
                    buckets.insert(signature);
                }
                seen.push(out.len());
                out.push(row);
            }
        }
        return out;
    }
    // Canonical signatures give a hash lookup for the common case; once a row
    // has no signature, the rest of the run falls back to the engine's linear
    // comparison so semantics never change.
    let mut buckets: HashSet<Vec<ProbeKey>> = HashSet::with_capacity(rows.len());
    // 保留下标而不是整行副本,避免哈希命中路径上每行多一次行克隆。
    let mut seen: Vec<usize> = Vec::with_capacity(rows.len());
    let mut out: Vec<Vec<Cell>> = Vec::with_capacity(rows.len());
    let mut linear = false;
    for row in rows {
        let signature = if linear { None } else { row_signature(&row, collations) };
        let duplicate = match &signature {
            Some(signature) => buckets.contains(signature),
            None => {
                linear = true;
                seen.iter().any(|index| rows_equal_with(&out[*index], &row, collations))
            }
        };
        if !duplicate {
            if let Some(signature) = signature {
                buckets.insert(signature);
            }
            seen.push(out.len());
            out.push(row);
        }
    }
    out
}

/// Computes one whole-result aggregate over the filtered rows.
///
/// Empty input follows SQLite: `count` is `0`, every other aggregate is `NULL`.
pub(crate) fn compute_aggregate(spec: &AggregateSpec, rows: &[Vec<Cell>], ctx: &EvalContext) -> Result<Cell, SqlError> {
    // `count(*)` 只数行,不需要为每行物化一个占位值。
    if !spec.distinct && spec.order.is_empty() && matches!(spec.function, AggregateFunction::CountStar) {
        let count = match &spec.filter {
            None => rows.len(),
            Some(filter) => {
                let mut count = 0usize;
                for row in rows {
                    if truthiness(&filter.eval(row, ctx)?) == Some(true) {
                        count += 1;
                    }
                }
                count
            }
        };
        return Ok(Cell::Integer(count as i64));
    }
    // `min` / `max` 逐行折叠即可,不必先把整列参数收进一个向量。
    if !spec.distinct
        && spec.order.is_empty()
        && spec.separator.is_none()
        && matches!(spec.function, AggregateFunction::Min | AggregateFunction::Max)
    {
        let Some(argument) = &spec.argument
        else {
            return Ok(Cell::Null);
        };
        let function = spec.function;
        let mut best: Option<Cell> = None;
        for row in rows {
            if let Some(filter) = &spec.filter {
                if truthiness(&filter.eval(row, ctx)?) != Some(true) {
                    continue;
                }
            }
            let value = argument.eval(row, ctx)?;
            if matches!(value, Cell::Null) {
                continue;
            }
            best = Some(match best {
                None => value,
                Some(current) => {
                    let ordering = compare_cells(&value, &current);
                    let replace = match function {
                        AggregateFunction::Min => ordering == Ordering::Less,
                        AggregateFunction::Max => ordering == Ordering::Greater,
                        _ => false,
                    };
                    if replace { value } else { current }
                }
            });
        }
        return Ok(best.unwrap_or(Cell::Null));
    }
    // 无 `DISTINCT`、无参数内 `ORDER BY` 时不需要物化 (value, separator, key)
    // 三元组,直接按行收集参数值,省掉每组一次分配与二次装载。
    if !spec.distinct && spec.order.is_empty() {
        let mut values = Vec::with_capacity(rows.len());
        let mut separators = if spec.separator.is_some() { Vec::with_capacity(rows.len()) } else { Vec::new() };
        for row in rows {
            if let Some(filter) = &spec.filter {
                if truthiness(&filter.eval(row, ctx)?) != Some(true) {
                    continue;
                }
            }
            let value = match &spec.argument {
                Some(argument) => argument.eval(row, ctx)?,
                None => Cell::Null,
            };
            if let Some(separator) = &spec.separator {
                separators.push(separator.eval(row, ctx)?);
            }
            values.push(value);
        }
        return Ok(aggregate_cells(spec.function, &values, if spec.separator.is_some() { Some(&separators) } else { None }));
    }
    let mut entries: Vec<(Cell, Cell, Vec<Cell>)> = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(filter) = &spec.filter {
            if truthiness(&filter.eval(row, ctx)?) != Some(true) {
                continue;
            }
        }
        let value = match &spec.argument {
            Some(argument) => argument.eval(row, ctx)?,
            None => Cell::Null,
        };
        let separator = match &spec.separator {
            Some(separator) => separator.eval(row, ctx)?,
            None => Cell::Null,
        };
        let keys = spec.order.iter().map(|(expr, _)| expr.eval(row, ctx)).collect::<Result<Vec<_>, SqlError>>()?;
        entries.push((value, separator, keys));
    }
    let (values, separators) = shape_aggregate_inputs(entries, spec.distinct, &spec.order, spec.collation);
    Ok(aggregate_cells(spec.function, &values, if spec.separator.is_some() { Some(&separators) } else { None }))
}

/// Applies `DISTINCT` and the in-argument `ORDER BY` to one aggregate's input.
///
/// `DISTINCT` sorts the values first so equal ones sit together, drops the
/// duplicates, then the explicit `ORDER BY` (when present) re-sorts the
/// survivors. Without `DISTINCT` only the explicit order applies, and it is
/// stable so equal keys keep the row order SQLite would keep.
pub(crate) fn shape_aggregate_inputs(
    mut entries: Vec<(Cell, Cell, Vec<Cell>)>,
    distinct: bool,
    order: &[(ResolvedExpr, SortDirection)],
    collation: Collation,
) -> (Vec<Cell>, Vec<Cell>) {
    let directions: Vec<SortDirection> = order.iter().map(|(_, direction)| *direction).collect();
    if distinct {
        entries.sort_by(|left, right| compare_cells_with(&left.0, &right.0, collation));
        entries.dedup_by(|left, right| compare_cells_with(&left.0, &right.0, collation) == Ordering::Equal);
        if !order.is_empty() {
            entries.sort_by(|left, right| compare_ordered(&left.2, &right.2, &directions));
        }
    }
    else if !order.is_empty() {
        entries.sort_by(|left, right| compare_ordered(&left.2, &right.2, &directions));
    }
    let mut values = Vec::with_capacity(entries.len());
    let mut separators = Vec::with_capacity(entries.len());
    for (value, separator, _) in entries {
        values.push(value);
        separators.push(separator);
    }
    (values, separators)
}

/// Aggregates already evaluated argument values.
///
/// Shared by whole-result aggregates and window frames so both follow one rule
/// for empty input, integer overflow and text coercion.
pub(crate) fn aggregate_cells(function: AggregateFunction, values: &[Cell], separators: Option<&[Cell]>) -> Cell {
    match function {
        AggregateFunction::CountStar => Cell::Integer(values.len() as i64),
        AggregateFunction::Count => Cell::Integer(values.iter().filter(|value| !matches!(value, Cell::Null)).count() as i64),
        AggregateFunction::Avg => {
            let mut total = 0.0f64;
            let mut count = 0u64;
            for value in values {
                if let Some(number) = numeric(value) {
                    total += number_f64(number);
                    count += 1;
                }
            }
            if count == 0 { Cell::Null } else { Cell::Real(total / count as f64) }
        }
        AggregateFunction::Sum => {
            // Stay integral only while every input is an INTEGER *storage*
            // class; text and blobs that convert to whole numbers still make
            // the result a real, exactly like `sqlite3_value_type`.
            let mut integral: Option<i64> = Some(0);
            let mut real = 0.0f64;
            let mut seen = false;
            for cell in values {
                let Some(number) = numeric(cell)
                else {
                    continue;
                };
                seen = true;
                match (cell, number) {
                    (Cell::Integer(_), Number::Int(value)) => match integral {
                        Some(sum) => match sum.checked_add(value) {
                            Some(sum) => integral = Some(sum),
                            None => {
                                real = sum as f64 + value as f64;
                                integral = None;
                            }
                        },
                        None => real += value as f64,
                    },
                    (_, number) => match integral.take() {
                        Some(sum) => real = sum as f64 + number_f64(number),
                        None => real += number_f64(number),
                    },
                }
            }
            if !seen {
                Cell::Null
            }
            else {
                match integral {
                    Some(value) => Cell::Integer(value),
                    None => Cell::Real(real),
                }
            }
        }
        // `total()` is `sum()` in real mode, except that an empty input is 0.0.
        AggregateFunction::Total => {
            let mut real = 0.0f64;
            for value in values {
                if let Some(number) = numeric(value) {
                    real += number_f64(number);
                }
            }
            Cell::Real(real)
        }
        // `group_concat` joins the text forms in input order, skips `NULL`,
        // uses `,` when the separator argument is absent, and yields `NULL`
        // when nothing is left to join.
        AggregateFunction::GroupConcat => {
            let mut joined = String::new();
            let mut seen = false;
            for (index, value) in values.iter().enumerate() {
                let Some(value) = text_form(value)
                else {
                    continue;
                };
                if seen {
                    let separator = match separators {
                        // A present but NULL separator acts like an empty one.
                        Some(separators) => separators.get(index).and_then(text_form).unwrap_or_default(),
                        None => ",".to_string(),
                    };
                    joined.push_str(&separator);
                }
                joined.push_str(&value);
                seen = true;
            }
            if seen { Cell::Text(joined) } else { Cell::Null }
        }
        AggregateFunction::Min | AggregateFunction::Max => {
            let mut best: Option<Cell> = None;
            for value in values {
                if matches!(value, Cell::Null) {
                    continue;
                }
                best = Some(match best {
                    None => value.clone(),
                    Some(current) => {
                        let ordering = compare_cells(value, &current);
                        let replace = match function {
                            AggregateFunction::Min => ordering == Ordering::Less,
                            AggregateFunction::Max => ordering == Ordering::Greater,
                            _ => false,
                        };
                        if replace { value.clone() } else { current }
                    }
                });
            }
            best.unwrap_or(Cell::Null)
        }
    }
}

/// Partitions the filtered rows into `GROUP BY` buckets.
///
/// With no keys every row forms one bucket, and an empty input still yields one
/// empty bucket so that whole-result aggregates can report their empty-input
/// values. Group equality uses the same `NULL == NULL` rule as set operations.
pub(crate) fn build_groups(
    group_keys: &[GroupKey],
    projection: &Option<Vec<ResolvedExpr>>,
    rows: Vec<Vec<Cell>>,
    ctx: &EvalContext,
    budget: &Budget,
) -> Result<Vec<Vec<Vec<Cell>>>, SqlError> {
    if group_keys.is_empty() {
        return Ok(vec![rows]);
    }
    // 单键分组不需要每行一份 `Vec<ProbeKey>`:一个键签名就足以定位分组,
    // 桶里直接存首次出现的分组下标。分桶与线性回退的判定和多键路径一致。
    if group_keys.len() == 1 {
        let group_key = &group_keys[0];
        let collation = match group_key {
            GroupKey::Projected(_, collation) | GroupKey::Expression(_, collation) => *collation,
        };
        let mut groups: Vec<(Cell, Vec<Vec<Cell>>)> = Vec::new();
        let mut buckets: HashMap<ProbeKey, usize> = HashMap::new();
        let mut linear = false;
        for row in rows {
            let value = match group_key {
                GroupKey::Projected(index, _) => match projection {
                    Some(nodes) => match nodes.get(*index) {
                        Some(node) => node.eval(&row, ctx)?,
                        None => Cell::Null,
                    },
                    None => row.get(*index).cloned().unwrap_or(Cell::Null),
                },
                GroupKey::Expression(expr, _) => expr.eval(&row, ctx)?,
            };
            let signature = if linear {
                None
            }
            else {
                match probe_key(&value, collation) {
                    Some(part) if !matches!(part, ProbeKey::Wide(_)) => Some(part),
                    _ => None,
                }
            };
            let matched = match &signature {
                Some(signature) => buckets.get(signature).copied(),
                None => {
                    linear = true;
                    groups.iter().position(|(existing, _)| compare_cells_with(existing, &value, collation) == Ordering::Equal)
                }
            };
            match matched {
                Some(index) => {
                    budget.charge(1)?;
                    groups[index].1.push(row);
                }
                None => {
                    budget.charge(1)?;
                    let index = groups.len();
                    if let Some(signature) = signature {
                        buckets.insert(signature, index);
                    }
                    groups.push((value, vec![row]));
                }
            }
        }
        return Ok(groups.into_iter().map(|(_, rows)| rows).collect());
    }
    let mut groups: Vec<(Vec<Cell>, Vec<Vec<Cell>>)> = Vec::new();
    // Hash buckets over the canonical key signatures, so a row finds its
    // group in one probe instead of a linear scan over every group built so
    // far. Two keys land in one bucket exactly when they compare equal, and
    // within a bucket the first match in group-creation order is the same
    // first match the linear scan would have taken, so the groups and their
    // output order are unchanged.
    let mut buckets: HashMap<Vec<ProbeKey>, Vec<usize>> = HashMap::new();
    // A key the canonical form cannot hash (a NaN, which compares equal to
    // every numeric, or a wide integer) switches the rest of the build to the
    // linear scan, which reproduces the engine's comparison exactly.
    let mut linear = false;
    for row in rows {
        let key = group_keys
            .iter()
            .map(|key| match key {
                GroupKey::Projected(index, _) => match projection {
                    Some(nodes) => match nodes.get(*index) {
                        Some(node) => node.eval(&row, ctx),
                        None => Ok(Cell::Null),
                    },
                    None => Ok(row.get(*index).cloned().unwrap_or(Cell::Null)),
                },
                GroupKey::Expression(expr, _) => expr.eval(&row, ctx),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let signature = if linear { None } else { group_signature(&key, group_keys) };
        let matched = match &signature {
            Some(signature) => buckets
                .get(signature)
                .and_then(|candidates| candidates.iter().find(|index| group_keys_equal(&groups[**index].0, &key, group_keys)))
                .copied(),
            None => {
                linear = true;
                groups.iter().position(|(existing, _)| group_keys_equal(existing, &key, group_keys))
            }
        };
        match matched {
            Some(index) => {
                budget.charge(1)?;
                groups[index].1.push(row);
            }
            None => {
                budget.charge(1)?;
                let index = groups.len();
                if let Some(signature) = signature {
                    buckets.entry(signature).or_default().push(index);
                }
                groups.push((key, vec![row]));
            }
        }
    }
    Ok(groups.into_iter().map(|(_, rows)| rows).collect())
}

/// Canonical hash signature of one group key, or `None` when a component is
/// outside the canonical form and the row has to go through the linear scan.
pub(crate) fn group_signature(key: &[Cell], group_keys: &[GroupKey]) -> Option<Vec<ProbeKey>> {
    key.iter()
        .zip(group_keys)
        .map(|(cell, group_key)| {
            let collation = match group_key {
                GroupKey::Projected(_, collation) | GroupKey::Expression(_, collation) => *collation,
            };
            match probe_key(cell, collation) {
                Some(part) if !matches!(part, ProbeKey::Wide(_)) => Some(part),
                _ => None,
            }
        })
        .collect()
}

/// The engine's group equality: every position compares equal under its key's
/// collation, exactly as the linear scan decided.
pub(crate) fn group_keys_equal(left: &[Cell], right: &[Cell], group_keys: &[GroupKey]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).zip(group_keys).all(|((existing, key), group_key)| {
            let collation = match group_key {
                GroupKey::Projected(_, collation) | GroupKey::Expression(_, collation) => *collation,
            };
            compare_cells_with(existing, key, collation) == Ordering::Equal
        })
}

/// 统计普通源与内连接的匹配行数,不物化匹配行。
///
/// 与 [`join_rows`] 的通用路径逐行等价:按 `FROM` 顺序绑定,`ON` 在候选行上
/// 求值,`WHERE` 合取在完整行上求值。只有 `rowid` 等值探针借用右表的 rowid
/// 升序做二分,其余形态退回逐行扫描,因而不会改变计数结果。
pub(crate) fn count_inner_join_rows(
    relation: &Relation,
    ons: &[Option<ResolvedExpr>],
    tables: &[Rc<Vec<Vec<Cell>>>],
    conjuncts: &[ResolvedExpr],
    ctx: &EvalContext,
) -> Result<usize, SqlError> {
    // 探针形态与右表有序性每条语句只判定一次,避免在每行递归里重算。
    let probes: Vec<Option<JoinProbe<'_>>> = relation
        .sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            if let Some((right_slot, probe)) = rowid_join_probe(source, &ons[index], index) {
                let within = right_slot - source.offset;
                let ascending = tables[index].windows(2).all(|pair| match (&pair[0][within], &pair[1][within]) {
                    (Cell::Integer(left), Cell::Integer(right)) => left < right,
                    _ => false,
                });
                return Ok(Some(JoinProbe::Rowid(RowidProbe { within, probe, ascending })));
            }
            // 非 rowid 的等值连接:右表按该列建一次哈希桶,左行只做一次查找。
            // 任一存储键没有规范签名时整体退回逐行扫描,保证结果不变。
            if let Some((within, probe, collation)) = column_join_probe(source, &ons[index], index) {
                if let Some(buckets) = build_probe_buckets(&tables[index], within, collation) {
                    return Ok(Some(JoinProbe::Hash(HashProbe { within, probe, collation, buckets })));
                }
            }
            Ok(None)
        })
        .collect::<Result<Vec<_>, SqlError>>()?;
    let mut merged = vec![Cell::Null; relation.columns()];
    let mut count = 0usize;
    // `WHERE` 合取按最早可判定的源提前求值:外层行不满足单表谓词时,不必
    // 再为它做一次右表探针。
    let levels = conjunct_bind_levels(conjuncts, relation.sources.len());
    count_join_level(0, relation, ons, tables, &probes, conjuncts, &levels, ctx, &mut merged, &mut count)?;
    Ok(count)
}

/// 判定在第 `level` 步绑定完成后才能确定取值的 `WHERE` 合取。
///
/// `merged` 的 `0..=level` 源已经写入;返回 `false` 表示这一候选行不可能
/// 出现在结果里,调用方可以立刻剪枝。
pub(crate) fn join_level_conjuncts_hold(
    level: usize,
    conjuncts: &[ResolvedExpr],
    levels: &[Option<usize>],
    merged: &[Cell],
    ctx: &EvalContext,
) -> Result<bool, SqlError> {
    for (conjunct, bound) in conjuncts.iter().zip(levels) {
        if *bound != Some(level) {
            continue;
        }
        if truthiness(&conjunct.eval(merged, ctx)?) != Some(true) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 一条 `ON <右侧 rowid> = <前缀表达式>` 的等值探针,`ascending` 表示右表按该
/// rowid 列严格升序,可用二分定位。
pub(crate) struct RowidProbe<'a> {
    within: usize,
    probe: &'a ResolvedExpr,
    ascending: bool,
}

/// 一条 `ON <右侧列> = <前缀表达式>` 的等值探针:右表按该列按排序规则归一化后
/// 建一次哈希桶,左行求值后直接取桶,不再逐行扫描右表。
pub(crate) struct HashProbe<'a> {
    /// 右表行内该列的位置。
    within: usize,
    /// 左行上求值的探针表达式。
    probe: &'a ResolvedExpr,
    /// 等值比较生效的排序规则,决定文本键的归一化方式。
    collation: Collation,
    /// 归一化键到右表行下标的桶,下标按右表顺序追加。
    ///
    /// 桶只按 64 位规范化哈希分组,不保存键的副本:碰撞只会多取候选,而候选
    /// 行仍要重跑整条 `ON`,所以不会漏行也不会误配。
    buckets: HashMap<u64, Vec<usize>>,
}

/// 内连接右表的一条等值探针:rowid 二分或普通列哈希。
pub(crate) enum JoinProbe<'a> {
    Rowid(RowidProbe<'a>),
    Hash(HashProbe<'a>),
}

/// Detects an inner-join `ON` that is a single equality between one column of
/// the current right source and an expression over strictly earlier sources.
///
/// The probe is only a lookup accelerator: the caller still evaluates the
/// whole `ON` on every candidate, so a probe that over-fetches cannot change
/// the result. Anything the canonical key cannot reproduce (a text collation
/// the hash respects, a NaN, an integer outside the exact binary64 range) is
/// handled by the caller falling back to the nested scan.
pub(crate) fn column_join_probe<'a>(
    source: &Source,
    on: &'a Option<ResolvedExpr>,
    index: usize,
) -> Option<(usize, &'a ResolvedExpr, Collation)> {
    let on = on.as_ref()?;
    let (slot, probe, collation) = match on {
        ResolvedExpr::Compare { left, op: CompareOp::Eq, right, collation } => match (left.as_ref(), right.as_ref()) {
            (ResolvedExpr::Column { source: owner, index: slot }, other) if *owner == index => (*slot, other, *collation),
            (other, ResolvedExpr::Column { source: owner, index: slot }) if *owner == index => (*slot, other, *collation),
            _ => return None,
        },
        _ => return None,
    };
    if slot < source.offset || slot >= source.offset + source.width() {
        return None;
    }
    if probe.contains_subquery() {
        return None;
    }
    let mut refs = Vec::new();
    probe.collect_sources(&mut refs);
    if refs.iter().any(|owner| *owner >= index) {
        return None;
    }
    Some((slot - source.offset, probe, collation))
}

/// Builds the right-side hash buckets for one column probe, or `None` when any
/// stored key has no canonical signature and the nested scan must stay.
pub(crate) fn build_probe_buckets(rows: &[Vec<Cell>], within: usize, collation: Collation) -> Option<HashMap<u64, Vec<usize>>> {
    let mut buckets: HashMap<u64, Vec<usize>> = HashMap::new();
    for (position, row) in rows.iter().enumerate() {
        let key = probe_hash(&row[within], collation)?;
        buckets.entry(key).or_default().push(position);
    }
    Some(buckets)
}

/// 64 位规范化哈希:与 [`probe_key`] 的等价类一致,但只写哈希、不分配键。
///
/// 与 [`probe_key`] 一样,`NaN` 与超出 binary64 精确范围的整数返回 `None`,
/// 由调用方退回逐行扫描;其余值按 SQL 等值语义折叠(有符号零归一、整数与
/// 实数按同一数值签名、文本按排序规则折叠)。
pub(crate) fn probe_hash(cell: &Cell, collation: Collation) -> Option<u64> {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    fn add(hash: &mut u64, byte: u8) {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(PRIME);
    }
    fn add_bytes(hash: &mut u64, bytes: &[u8]) {
        for byte in bytes {
            add(hash, *byte);
        }
    }
    match cell {
        // Type tags keep the numeric, text and blob classes apart, exactly as
        // `ProbeKey`'s variants do.
        Cell::Null => {
            let mut hash = OFFSET;
            add(&mut hash, 0);
            Some(hash)
        }
        Cell::Integer(value) => match integer_probe_key(*value) {
            ProbeKey::Number(bits) => {
                let mut hash = OFFSET;
                add(&mut hash, 1);
                add_bytes(&mut hash, &bits.to_le_bytes());
                Some(hash)
            }
            _ => None,
        },
        Cell::Real(value) => match real_probe_key(*value) {
            Some(ProbeKey::Number(bits)) => {
                let mut hash = OFFSET;
                add(&mut hash, 1);
                add_bytes(&mut hash, &bits.to_le_bytes());
                Some(hash)
            }
            _ => None,
        },
        Cell::Text(text) => {
            let mut hash = OFFSET;
            add(&mut hash, 2);
            match collation {
                Collation::Binary => add_bytes(&mut hash, text.as_bytes()),
                Collation::Nocase => {
                    for byte in text.bytes() {
                        add(&mut hash, byte.to_ascii_lowercase());
                    }
                }
                Collation::Rtrim => add_bytes(&mut hash, text.trim_end_matches(' ').as_bytes()),
            }
            Some(hash)
        }
        Cell::Blob(bytes) => {
            let mut hash = OFFSET;
            add(&mut hash, 3);
            add_bytes(&mut hash, bytes);
            Some(hash)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn count_join_level(
    index: usize,
    relation: &Relation,
    ons: &[Option<ResolvedExpr>],
    tables: &[Rc<Vec<Vec<Cell>>>],
    probes: &[Option<JoinProbe<'_>>],
    conjuncts: &[ResolvedExpr],
    levels: &[Option<usize>],
    ctx: &EvalContext,
    merged: &mut [Cell],
    count: &mut usize,
) -> Result<(), SqlError> {
    if index == relation.sources.len() {
        for conjunct in conjuncts {
            if truthiness(&conjunct.eval(merged, ctx)?) != Some(true) {
                return Ok(());
            }
        }
        ctx.budget.charge(1)?;
        *count += 1;
        return Ok(());
    }
    let source = &relation.sources[index];
    let start = source.offset;
    let end = start + source.width();
    // 第一项与后续无 `ON` 的内连接都是无条件乘积。
    let Some(on) = ons[index].as_ref()
    else {
        for values in tables[index].iter() {
            merged[start..end].clone_from_slice(values);
            if !join_level_conjuncts_hold(index, conjuncts, levels, merged, ctx)? {
                continue;
            }
            count_join_level(index + 1, relation, ons, tables, probes, conjuncts, levels, ctx, merged, count)?;
        }
        return Ok(());
    };
    if let Some(probe) = &probes[index] {
        let right = tables[index].as_slice();
        match probe {
            JoinProbe::Rowid(rowid) => {
                if let Cell::Integer(key) = rowid.probe.eval(merged, ctx)? {
                    if rowid.ascending {
                        let within = rowid.within;
                        let position = right.partition_point(|row| matches!(&row[within], Cell::Integer(value) if *value < key));
                        if let Some(row) = right.get(position) {
                            if matches!(&row[within], Cell::Integer(value) if *value == key) {
                                // The probe is the whole `ON` equality: an integer
                                // rowid equal to the integer the probe produced makes
                                // it true without re-evaluating the comparison.
                                merged[start..end].clone_from_slice(row);
                                if join_level_conjuncts_hold(index, conjuncts, levels, merged, ctx)? {
                                    count_join_level(index + 1, relation, ons, tables, probes, conjuncts, levels, ctx, merged, count)?;
                                }
                            }
                        }
                        return Ok(());
                    }
                    let within = rowid.within;
                    for row in right {
                        if matches!(&row[within], Cell::Integer(value) if *value == key) {
                            merged[start..end].clone_from_slice(row);
                            if join_level_conjuncts_hold(index, conjuncts, levels, merged, ctx)? {
                                count_join_level(index + 1, relation, ons, tables, probes, conjuncts, levels, ctx, merged, count)?;
                            }
                        }
                    }
                    return Ok(());
                }
            }
            JoinProbe::Hash(hash) => {
                // 只有归一化键存在时才查找;键缺失的形态(超范围整数、NaN)
                // 落回下面的逐行扫描,结果与通用路径一致。
                if let Some(key) = probe_hash(&hash.probe.eval(merged, ctx)?, hash.collation) {
                    if let Some(positions) = hash.buckets.get(&key) {
                        for &position in positions {
                            merged[start..end].clone_from_slice(&right[position]);
                            if truthiness(&on.eval(merged, ctx)?) != Some(true) {
                                continue;
                            }
                            if join_level_conjuncts_hold(index, conjuncts, levels, merged, ctx)? {
                                count_join_level(index + 1, relation, ons, tables, probes, conjuncts, levels, ctx, merged, count)?;
                            }
                        }
                    }
                    return Ok(());
                }
            }
        }
    }
    for row in tables[index].iter() {
        merged[start..end].clone_from_slice(row);
        if truthiness(&on.eval(merged, ctx)?) != Some(true) {
            continue;
        }
        if !join_level_conjuncts_hold(index, conjuncts, levels, merged, ctx)? {
            continue;
        }
        count_join_level(index + 1, relation, ons, tables, probes, conjuncts, levels, ctx, merged, count)?;
    }
    Ok(())
}

/// Builds joined rows for a `FROM` clause that uses `JOIN`.
///
/// Sources are added left to right, and rows come out in left-major order, as
/// SQLite emits them. An inner join keeps only pairs whose predicate is SQL
/// `TRUE`; a left join keeps the left row with `NULL` on the right when no
/// pair matches; a right join drops unmatched left rows but appends unmatched
/// right rows padded with `NULL` on the left; a full join keeps both. `USING`
/// and `NATURAL` columns are matched by equality and written into a coalesced
/// slot, so the shared column exists once without losing either side's raw
/// value. Comma and `CROSS JOIN` items stay plain products.
pub(crate) fn join_rows(
    relation: &Relation,
    ons: &[Option<ResolvedExpr>],
    tables: &[Rc<Vec<Vec<Cell>>>],
    conjuncts: &[ResolvedExpr],
    take: usize,
    ctx: &EvalContext,
) -> Result<Vec<Vec<Cell>>, SqlError> {
    let mut rows: Vec<Vec<Cell>> = vec![vec![Cell::Null; relation.columns()]];
    // `WHERE` 合取尽早求值:一个合取只读已经绑定的源时,它在当前源合并完
    // 就可以判定,不必等右表全部展开。`join_indexed` / `join_left` 这类
    // 「外层行多、命中行少」的连接因此只对通过谓词的外层行做探针,而不是
    // 先为每一行生成连接结果再由末端的合取丢弃。
    let levels = conjunct_bind_levels(conjuncts, relation.sources.len());
    // 只有「内连接 / 左连接」构成的整个 `FROM` 才能提前裁剪左行:右/全连接
    // 会为未匹配的右行补一行左 `NULL`,提前丢弃左行会改变「哪些右行算未匹配」。
    // `USING` / `NATURAL` 的合并列在两侧都绑定后才成立,同样不做提前裁剪。
    let prunable = relation.using.iter().all(|pairs| pairs.is_empty())
        && relation.sources.iter().all(|source| matches!(source.join, None | Some(JoinKind::Inner) | Some(JoinKind::Left)));
    for (index, source) in relation.sources.iter().enumerate() {
        let width = source.width();
        let pairs = &relation.using[index];
        // No predicate means every pair matches, which SQLite treats as a
        // product regardless of the join keyword.
        let unconditional = ons[index].is_none() && pairs.is_empty();
        let kind = source.join.unwrap_or(JoinKind::Inner);
        let mut next = Vec::new();
        // Fast path for the common `INNER JOIN ... ON <right>.<rowid> = <expr
        // over the left>` shape: index the right side by rowid once and probe
        // per left row instead of rescanning every right row for every left
        // row. The full `ON` still decides each candidate and any non-integer
        // probe falls back to the nested scan, so the emitted rows and their
        // left-major order are identical to the generic path below.
        let fast = if unconditional || !pairs.is_empty() || !matches!(kind, JoinKind::Inner) {
            None
        }
        else {
            rowid_join_probe(source, &ons[index], index)
        };
        // 非 rowid 的等值连接同样不必逐行扫描右表:建一次哈希桶后按左行的
        // 探针取候选。候选仍需重跑 `ON`,所以多取不会改变结果;右表任一存储
        // 键没有规范签名时整条回退逐行扫描。`LEFT JOIN` 复用同一条探针,只是
        // 未命中的左行要按外连接语义补一行 NULL。
        let hashed = if fast.is_none() && !unconditional && pairs.is_empty() && matches!(kind, JoinKind::Inner | JoinKind::Left) {
            column_join_probe(source, &ons[index], index).and_then(|(within, probe, collation)| {
                build_probe_buckets(&tables[index], within, collation).map(|buckets| HashProbe { within, probe, collation, buckets })
            })
        }
        else {
            None
        };
        if unconditional {
            for left in &rows {
                for values in tables[index].iter() {
                    let mut merged = left.clone();
                    merged[source.offset..source.offset + width].clone_from_slice(values);
                    ctx.budget.charge(1)?;
                    next.push(merged);
                }
            }
        }
        else if let Some((right_slot, probe)) = fast {
            let within = right_slot - source.offset;
            let mut by_rowid: HashMap<i64, Vec<usize>> = HashMap::new();
            for (position, row) in tables[index].iter().enumerate() {
                if let Cell::Integer(key) = &row[within] {
                    by_rowid.entry(*key).or_default().push(position);
                }
            }
            let on = ons[index].as_ref().expect("the probe only returns for a present ON");
            for left in &rows {
                match probe.eval(left, ctx)? {
                    Cell::Integer(key) => {
                        if let Some(positions) = by_rowid.get(&key) {
                            for &position in positions {
                                let mut merged = left.clone();
                                merged[source.offset..source.offset + width].clone_from_slice(&tables[index][position]);
                                // The probe is the whole `ON` equality and the
                                // matched rowid is the same integer, so the
                                // comparison is already true.
                                ctx.budget.charge(1)?;
                                next.push(merged);
                            }
                        }
                    }
                    // A non-integer probe could still equal an integer rowid
                    // under SQLite's affinity rules, so keep the nested scan for
                    // this left row to guarantee an identical result.
                    _ => {
                        for values in tables[index].iter() {
                            let mut merged = left.clone();
                            merged[source.offset..source.offset + width].clone_from_slice(values);
                            if truthiness(&on.eval(&merged, ctx)?) == Some(true) {
                                ctx.budget.charge(1)?;
                                next.push(merged);
                            }
                        }
                    }
                }
            }
        }
        else if let Some(hash) = hashed {
            let on = ons[index].as_ref().expect("the probe only returns for a present ON");
            let outer = matches!(kind, JoinKind::Left);
            for left in &rows {
                let mut matched = false;
                if let Some(key) = probe_hash(&hash.probe.eval(left, ctx)?, hash.collation) {
                    if let Some(positions) = hash.buckets.get(&key) {
                        for &position in positions {
                            let mut merged = left.clone();
                            merged[source.offset..source.offset + width].clone_from_slice(&tables[index][position]);
                            if truthiness(&on.eval(&merged, ctx)?) == Some(true) {
                                ctx.budget.charge(1)?;
                                next.push(merged);
                                matched = true;
                            }
                        }
                    }
                }
                else {
                    // 探针值没有规范签名(超范围整数、NaN):这一行保持逐行扫描。
                    for values in tables[index].iter() {
                        let mut merged = left.clone();
                        merged[source.offset..source.offset + width].clone_from_slice(values);
                        if truthiness(&on.eval(&merged, ctx)?) == Some(true) {
                            ctx.budget.charge(1)?;
                            next.push(merged);
                            matched = true;
                        }
                    }
                }
                if outer && !matched {
                    // 左连接保留未命中的左行;这一来源的列在左行里已是 NULL。
                    ctx.budget.charge(1)?;
                    next.push(left.clone());
                }
            }
        }
        else {
            let mut matched_right = vec![false; tables[index].len()];
            for left in &rows {
                let mut matched = false;
                for (position, values) in tables[index].iter().enumerate() {
                    let mut merged = left.clone();
                    merged[source.offset..source.offset + width].clone_from_slice(values);
                    coalesce_using(&mut merged, pairs);
                    let keep = match &ons[index] {
                        None => pairs.iter().all(|pair| {
                            let collation = relation.column_collation(pair.left);
                            compare_values(&merged[pair.left], &merged[pair.right], CompareOp::Eq, collation) == Some(true)
                        }),
                        Some(on) => truthiness(&on.eval(&merged, ctx)?) == Some(true),
                    };
                    if keep {
                        ctx.budget.charge(1)?;
                        next.push(merged);
                        matched = true;
                        matched_right[position] = true;
                    }
                }
                if !matched && matches!(kind, JoinKind::Left | JoinKind::Full) {
                    // The left row already carries NULLs for this source's slice.
                    let mut padded = left.clone();
                    coalesce_using(&mut padded, pairs);
                    ctx.budget.charge(1)?;
                    next.push(padded);
                }
            }
            if matches!(kind, JoinKind::Right | JoinKind::Full) {
                // Unmatched right rows follow every pair, padded on the left.
                for (position, values) in tables[index].iter().enumerate() {
                    if !matched_right[position] {
                        let mut merged = vec![Cell::Null; relation.columns()];
                        merged[source.offset..source.offset + width].clone_from_slice(values);
                        coalesce_using(&mut merged, pairs);
                        ctx.budget.charge(1)?;
                        next.push(merged);
                    }
                }
            }
        }
        // `RIGHT` / `FULL` 连接会为未匹配的右行补一行左 `NULL`,提前丢弃左行
        // 会改变「哪些右行算未匹配」,因此这两类源之后不再提前裁剪。
        if prunable && levels.iter().any(|level| *level == Some(index)) {
            let mut kept = Vec::with_capacity(next.len());
            for row in next {
                let mut pass = true;
                for (conjunct, level) in conjuncts.iter().zip(&levels) {
                    if *level == Some(index) && truthiness(&conjunct.eval(&row, ctx)?) != Some(true) {
                        pass = false;
                        break;
                    }
                }
                if pass {
                    kept.push(row);
                }
            }
            next = kept;
        }
        rows = next;
    }
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut keep = true;
        for conjunct in conjuncts {
            if truthiness(&conjunct.eval(&row, ctx)?) != Some(true) {
                keep = false;
                break;
            }
        }
        if keep {
            ctx.budget.charge(1)?;
            out.push(row);
            if out.len() >= take {
                break;
            }
        }
    }
    Ok(out)
}

/// Fills one row's coalesced `USING` slots: only an outer join can make the
/// left and right values differ, because unmatched rows arrive `NULL`-padded
/// on the missing side.
pub(crate) fn coalesce_using(row: &mut [Cell], pairs: &[UsingJoin]) {
    for pair in pairs {
        let left = row[pair.left].clone();
        row[pair.slot] = if matches!(left, Cell::Null) { row[pair.right].clone() } else { left };
    }
}

/// Detects an inner-join `ON` that is a single equality between one base
/// rowid table's rowid (its rowid-alias column or the implicit `rowid`) and an
/// expression over strictly earlier sources. Returns the merged-row slot that
/// holds that rowid and the probe expression, or `None` for any shape the hash
/// fast path cannot prove equivalent to the nested scan.
pub(crate) fn rowid_join_probe<'a>(source: &Source, on: &'a Option<ResolvedExpr>, index: usize) -> Option<(usize, &'a ResolvedExpr)> {
    if !source.columns.hidden_rowid() {
        return None;
    }
    let def = match &source.columns {
        SourceColumns::Table(def) => *def,
        SourceColumns::Materialized { .. } => return None,
    };
    if def.without_rowid {
        return None;
    }
    let alias_slot = def.rowid_alias.map(|alias| source.offset + alias);
    let hidden_slot = source.offset + def.columns.len();
    let on = on.as_ref()?;
    let (slot, probe) = match on {
        ResolvedExpr::Compare { left, op: CompareOp::Eq, right, .. } => match (left.as_ref(), right.as_ref()) {
            (ResolvedExpr::Column { source: owner, index: slot }, other)
                if *owner == index && (*slot == hidden_slot || alias_slot == Some(*slot)) =>
            {
                (*slot, other)
            }
            (other, ResolvedExpr::Column { source: owner, index: slot })
                if *owner == index && (*slot == hidden_slot || alias_slot == Some(*slot)) =>
            {
                (*slot, other)
            }
            _ => return None,
        },
        _ => return None,
    };
    if probe.contains_subquery() {
        return None;
    }
    let mut refs = Vec::new();
    probe.collect_sources(&mut refs);
    if refs.iter().any(|owner| *owner >= index) {
        return None;
    }
    Some((slot, probe))
}

/// Materializes the Cartesian product of the `UPDATE ... FROM` sources.
/// Every source row already carries its hidden rowid slot, so concatenating
/// them in `FROM` order reproduces the merged row the relation resolves
/// against.
pub fn join_product(tables: &[Rc<Vec<Vec<Cell>>>]) -> Vec<Vec<Cell>> {
    let mut product: Vec<Vec<Cell>> = vec![Vec::new()];
    for rows in tables {
        let mut next = Vec::with_capacity(product.len() * rows.len());
        for prefix in &product {
            for row in rows.iter() {
                let mut merged = prefix.clone();
                merged.extend(row.iter().cloned());
                next.push(merged);
            }
        }
        product = next;
    }
    product
}

/// Whether a projection is exactly one `count(*)`, optionally aliased.
pub(crate) fn is_bare_count_star(items: &SelectItems) -> bool {
    let SelectItems::Named(exprs) = items
    else {
        return false;
    };
    let [expr] = exprs.as_slice()
    else {
        return false;
    };
    matches!(
        strip_alias(expr),
        Expr::Aggregate {
            function: AggregateFunction::CountStar,
            argument: None,
            separator: None,
            distinct: false,
            filter: None,
            order,
        } if order.is_empty()
    )
}

/// The expression an `AS name` alias wraps; aliases never change a value.
pub(crate) fn strip_alias(expr: &Expr) -> &Expr {
    match expr {
        Expr::Alias { value, .. } => strip_alias(value),
        other => other,
    }
}

/// 统计笛卡尔积中满足全部合取的行数,不物化结果行。
///
/// 绑定顺序与提前剪枝复用 [`cross_product`] 的同一套规则,因此计数与通用
/// 路径逐行一致,只是把最终结果行换成了计数。
pub(crate) fn count_cross_product_rows(
    tables: &[Rc<Vec<Vec<Cell>>>],
    relation: &Relation,
    conjuncts: &[ResolvedExpr],
    ctx: &EvalContext,
) -> Result<usize, SqlError> {
    let order = binding_order(relation, conjuncts);
    let mut position = vec![0usize; order.len()];
    for (slot, source) in order.iter().enumerate() {
        position[*source] = slot;
    }
    let mut refs = Vec::new();
    let depths: Vec<Option<usize>> = conjuncts
        .iter()
        .map(|conjunct| {
            if conjunct.contains_subquery() {
                return Some(order.len().saturating_sub(1));
            }
            refs.clear();
            conjunct.collect_sources(&mut refs);
            refs.iter().map(|source| position[*source]).max()
        })
        .collect();
    let mut merged = vec![Cell::Null; relation.columns()];
    for (conjunct, depth) in conjuncts.iter().zip(&depths) {
        if depth.is_none() && truthiness(&conjunct.eval(&merged, ctx)?) == Some(false) {
            return Ok(0);
        }
    }
    // A single source already occupies the whole merged row, so every conjunct
    // can read the stored row slice directly; skipping the per-row copy avoids
    // cloning text cells once per scanned row.
    if relation.sources.len() == 1 && tables.len() == 1 {
        let mut count = 0usize;
        for values in tables[0].iter() {
            let mut matched = true;
            for conjunct in conjuncts {
                if truthiness(&conjunct.eval(values, ctx)?) != Some(true) {
                    matched = false;
                    break;
                }
            }
            if matched {
                ctx.budget.charge(1)?;
                count += 1;
            }
        }
        return Ok(count);
    }
    let mut count = 0usize;
    count_cross_level(0, tables, relation, &order, conjuncts, &depths, ctx, &mut merged, &mut count)?;
    Ok(count)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn count_cross_level(
    slot: usize,
    tables: &[Rc<Vec<Vec<Cell>>>],
    relation: &Relation,
    order: &[usize],
    conjuncts: &[ResolvedExpr],
    depths: &[Option<usize>],
    ctx: &EvalContext,
    merged: &mut [Cell],
    count: &mut usize,
) -> Result<(), SqlError> {
    if slot == order.len() {
        for conjunct in conjuncts {
            if truthiness(&conjunct.eval(merged, ctx)?) != Some(true) {
                return Ok(());
            }
        }
        ctx.budget.charge(1)?;
        *count += 1;
        return Ok(());
    }
    let source = &relation.sources[order[slot]];
    for values in tables[order[slot]].iter() {
        merged[source.offset..source.offset + values.len()].clone_from_slice(values);
        let mut pruned = false;
        for (conjunct, depth) in conjuncts.iter().zip(depths) {
            if *depth == Some(slot) && truthiness(&conjunct.eval(merged, ctx)?) == Some(false) {
                pruned = true;
                break;
            }
        }
        if !pruned {
            count_cross_level(slot + 1, tables, relation, order, conjuncts, depths, ctx, merged, count)?;
        }
    }
    Ok(())
}

pub(crate) fn cross_product(
    tables: Vec<Rc<Vec<Vec<Cell>>>>,
    relation: &Relation,
    conjuncts: &[ResolvedExpr],
    take: usize,
    ctx: &EvalContext,
) -> Result<Vec<Vec<Cell>>, SqlError> {
    let order = binding_order(relation, conjuncts);
    let mut position = vec![0usize; order.len()];
    for (slot, source) in order.iter().enumerate() {
        position[*source] = slot;
    }
    // Depth of each conjunct in binding order; `None` means it is constant.
    let mut refs = Vec::new();
    let depths: Vec<Option<usize>> = conjuncts
        .iter()
        .map(|conjunct| {
            if conjunct.contains_subquery() {
                return Some(order.len().saturating_sub(1));
            }
            refs.clear();
            conjunct.collect_sources(&mut refs);
            refs.iter().map(|source| position[*source]).max()
        })
        .collect();
    let mut merged = vec![Cell::Null; relation.columns()];
    let mut rows = Vec::new();
    // Constant conjuncts decide the whole result once.
    for (conjunct, depth) in conjuncts.iter().zip(&depths) {
        if depth.is_none() && truthiness(&conjunct.eval(&merged, ctx)?) == Some(false) {
            return Ok(rows);
        }
    }
    // Same single-source elision as the count path: the stored row is already
    // the merged row, so predicates and the emitted row can use it directly.
    // 单源查询独占这份行集时直接按值消费,命中的行移动进结果而不是再复制一份;
    // 行集仍被其它源共享(自连接、同表子查询)时才退回复制。
    if relation.sources.len() == 1 && tables.len() == 1 {
        let stored = tables.into_iter().next().expect("length checked");
        let mut kept = Vec::with_capacity(stored.len().min(take));
        match Rc::try_unwrap(stored) {
            Ok(values) if conjuncts.is_empty() => {
                let count = values.len().min(take);
                for _ in 0..count {
                    ctx.budget.charge(1)?;
                }
                if count == values.len() {
                    kept = values;
                }
                else {
                    kept.extend(values.into_iter().take(count));
                }
            }
            Ok(values) => {
                for values in values {
                    if kept.len() >= take {
                        break;
                    }
                    let mut matched = true;
                    for conjunct in conjuncts {
                        if truthiness(&conjunct.eval(&values, ctx)?) != Some(true) {
                            matched = false;
                            break;
                        }
                    }
                    if matched {
                        ctx.budget.charge(1)?;
                        kept.push(values);
                    }
                }
            }
            Err(shared) => {
                for values in shared.iter() {
                    if kept.len() >= take {
                        break;
                    }
                    let mut matched = true;
                    for conjunct in conjuncts {
                        if truthiness(&conjunct.eval(values, ctx)?) != Some(true) {
                            matched = false;
                            break;
                        }
                    }
                    if matched {
                        ctx.budget.charge(1)?;
                        kept.push(values.clone());
                    }
                }
            }
        }
        return Ok(kept);
    }
    emit_rows(0, &tables, relation, &order, conjuncts, &depths, take, ctx, &mut merged, &mut rows)?;
    Ok(rows)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn emit_rows(
    slot: usize,
    tables: &[Rc<Vec<Vec<Cell>>>],
    relation: &Relation,
    order: &[usize],
    conjuncts: &[ResolvedExpr],
    depths: &[Option<usize>],
    take: usize,
    ctx: &EvalContext,
    merged: &mut [Cell],
    rows: &mut Vec<Vec<Cell>>,
) -> Result<(), SqlError> {
    if rows.len() >= take {
        return Ok(());
    }
    if slot == order.len() {
        // Only rows whose whole conjunction is SQL `TRUE` survive, so a `NULL`
        // comparison at any level still rejects the row.
        for conjunct in conjuncts {
            if truthiness(&conjunct.eval(merged, ctx)?) != Some(true) {
                return Ok(());
            }
        }
        ctx.budget.charge(1)?;
        rows.push(merged.to_vec());
        return Ok(());
    }
    let source = &relation.sources[order[slot]];
    for values in tables[order[slot]].iter() {
        merged[source.offset..source.offset + values.len()].clone_from_slice(values);
        let mut pruned = false;
        for (conjunct, bound) in conjuncts.iter().zip(depths) {
            if *bound == Some(slot) && truthiness(&conjunct.eval(merged, ctx)?) == Some(false) {
                pruned = true;
                break;
            }
        }
        if !pruned {
            emit_rows(slot + 1, tables, relation, order, conjuncts, depths, take, ctx, merged, rows)?;
            if rows.len() >= take {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Whether one parsed expression mentions `column`; `qualifiers` restricts the
/// match to `NEW` / `OLD` references when provided.
pub(crate) fn expression_mentions_column(expr: &Expr, column: &str, qualifiers: Option<&[&str]>) -> bool {
    match expr {
        Expr::Column(reference) => {
            if !reference.name.eq_ignore_ascii_case(column) {
                return false;
            }
            match (reference.qualifier.as_deref(), qualifiers) {
                (None, None) => true,
                (None, Some(_)) => false,
                (Some(qualifier), Some(allowed)) => allowed.iter().any(|allowed| qualifier.eq_ignore_ascii_case(allowed)),
                (Some(_), None) => true,
            }
        }
        Expr::Literal(_) | Expr::Parameter { .. } => false,
        Expr::Unary { value, .. } => expression_mentions_column(value, column, qualifiers),
        Expr::Binary { left, right, .. } | Expr::Compare { left, right, .. } | Expr::And(left, right) | Expr::Or(left, right) => {
            expression_mentions_column(left, column, qualifiers) || expression_mentions_column(right, column, qualifiers)
        }
        Expr::IsDistinct { left, right, .. } => {
            expression_mentions_column(left, column, qualifiers) || expression_mentions_column(right, column, qualifiers)
        }
        Expr::Glob { value, pattern, .. } => {
            expression_mentions_column(value, column, qualifiers) || expression_mentions_column(pattern, column, qualifiers)
        }
        Expr::IsNull { value, .. }
        | Expr::IsTruth { value, .. }
        | Expr::Not(value)
        | Expr::Alias { value, .. }
        | Expr::Cast { value, .. }
        | Expr::Collate { value, .. } => expression_mentions_column(value, column, qualifiers),
        Expr::Case { operand, branches, otherwise } => {
            operand.as_ref().is_some_and(|value| expression_mentions_column(value, column, qualifiers))
                || branches.iter().any(|(condition, value)| {
                    expression_mentions_column(condition, column, qualifiers) || expression_mentions_column(value, column, qualifiers)
                })
                || otherwise.as_ref().is_some_and(|value| expression_mentions_column(value, column, qualifiers))
        }
        Expr::In { value, list, .. } => {
            expression_mentions_column(value, column, qualifiers)
                || list.iter().any(|item| expression_mentions_column(item, column, qualifiers))
        }
        Expr::Function { args, .. } | Expr::HostFunction { args, .. } => {
            args.iter().any(|argument| expression_mentions_column(argument, column, qualifiers))
        }
        Expr::Subquery(statement) | Expr::Exists(statement) => statement_mentions_column(statement, column, qualifiers),
        Expr::InSubquery { value, query, .. } => {
            expression_mentions_column(value, column, qualifiers) || statement_mentions_column(query, column, qualifiers)
        }
        Expr::Aggregate { argument, separator, .. } => {
            argument.as_ref().is_some_and(|value| expression_mentions_column(value, column, qualifiers))
                || separator.as_ref().is_some_and(|value| expression_mentions_column(value, column, qualifiers))
        }
        Expr::Like { value, pattern, escape, .. } => {
            expression_mentions_column(value, column, qualifiers)
                || expression_mentions_column(pattern, column, qualifiers)
                || escape.as_ref().is_some_and(|value| expression_mentions_column(value, column, qualifiers))
        }
        Expr::Window { spec, .. } => window_mentions_column(spec, column, qualifiers),
    }
}

/// Whether one projection mentions the column.
pub(crate) fn select_items_mention_column(items: &SelectItems, column: &str, qualifiers: Option<&[&str]>) -> bool {
    match items {
        SelectItems::Named(items) => items.iter().any(|item| expression_mentions_column(item, column, qualifiers)),
        SelectItems::Star | SelectItems::CountAll => false,
    }
}

/// Whether one window specification mentions the column in its partitioning,
/// ordering or frame offsets.
pub(crate) fn window_mentions_column(spec: &WindowSpec, column: &str, qualifiers: Option<&[&str]>) -> bool {
    let bound = |edge: &FrameBound| edge.offset.as_ref().is_some_and(|value| expression_mentions_column(value, column, qualifiers));
    spec.partition.iter().any(|expr| expression_mentions_column(expr, column, qualifiers))
        || spec.order.iter().any(|key| expression_mentions_column(&key.expr, column, qualifiers))
        || spec.frame.as_ref().is_some_and(|frame| bound(&frame.start) || bound(&frame.end))
}

/// Whether one `SELECT` core mentions the column.
pub(crate) fn core_mentions_column(core: &SelectCore, column: &str, qualifiers: Option<&[&str]>) -> bool {
    select_items_mention_column(&core.items, column, qualifiers)
        || core.from.iter().any(|source| source.query.as_ref().is_some_and(|query| statement_mentions_column(query, column, qualifiers)))
        || core.filter.as_ref().is_some_and(|expr| expression_mentions_column(expr, column, qualifiers))
        || core.group.iter().any(|expr| expression_mentions_column(expr, column, qualifiers))
        || core.having.as_ref().is_some_and(|expr| expression_mentions_column(expr, column, qualifiers))
        || core.windows.iter().any(|window| window_mentions_column(&window.spec, column, qualifiers))
}

/// Whether one statement mentions the column; used to refuse dropping a column
/// still read by a view or a trigger body.
pub fn statement_mentions_column(statement: &Statement, column: &str, qualifiers: Option<&[&str]>) -> bool {
    match statement {
        Statement::Select { with, first, compounds, order, .. } => {
            with.iter().any(|entry| statement_mentions_column(&entry.query, column, qualifiers))
                || core_mentions_column(first, column, qualifiers)
                || compounds.iter().any(|term| core_mentions_column(&term.core, column, qualifiers))
                || order.iter().any(|key| expression_mentions_column(&key.expr, column, qualifiers))
        }
        Statement::Insert { returning, .. } => returning.as_ref().is_some_and(|items| select_items_mention_column(items, column, qualifiers)),
        Statement::Update { assignments, filter, returning, .. } => {
            assignments.iter().any(|assignment| expression_mentions_column(&assignment.value, column, qualifiers))
                || filter.as_ref().is_some_and(|expr| expression_mentions_column(expr, column, qualifiers))
                || returning.as_ref().is_some_and(|items| select_items_mention_column(items, column, qualifiers))
        }
        Statement::Delete { filter, returning, .. } => {
            filter.as_ref().is_some_and(|expr| expression_mentions_column(expr, column, qualifiers))
                || returning.as_ref().is_some_and(|items| select_items_mention_column(items, column, qualifiers))
        }
        _ => false,
    }
}

/// Flattens `a AND b AND c` into single conjuncts, each applied as soon as its
/// sources are bound.
pub(crate) fn split_conjuncts(predicate: ResolvedExpr, out: &mut Vec<ResolvedExpr>) {
    match predicate {
        ResolvedExpr::And(left, right) => {
            split_conjuncts(*left, out);
            split_conjuncts(*right, out);
        }
        other => out.push(other),
    }
}

/// 每个 `WHERE` 合取最早可以判定的源下标:它引用的最大源。
///
/// 该源与更早的源绑定后,合取的值就固定了,所以可以在连接的第 `level` 步
/// 求值并丢弃不满足的行。`None` 表示不能提前判定(含子查询,或不引用任何源):
/// 这类合取仍在整行绑定完之后求值。
pub(crate) fn conjunct_bind_levels(conjuncts: &[ResolvedExpr], source_count: usize) -> Vec<Option<usize>> {
    conjuncts
        .iter()
        .map(|conjunct| {
            if conjunct.contains_subquery() {
                return None;
            }
            let mut refs = Vec::new();
            conjunct.collect_sources(&mut refs);
            refs.into_iter().max().filter(|level| *level < source_count)
        })
        .collect()
}

/// Orders the sources so predicates can prune as early as possible.
///
/// Starting from the source with the most single-source conjuncts, each step
/// binds the source that completes the most predicates with the set already
/// bound. Without this an equality chain over twenty tables would enumerate the
/// whole product before any comparison applied.
pub(crate) fn binding_order(relation: &Relation, conjuncts: &[ResolvedExpr]) -> Vec<usize> {
    let refs: Vec<Vec<usize>> = conjuncts
        .iter()
        .map(|conjunct| {
            let mut sources = Vec::new();
            conjunct.collect_sources(&mut sources);
            sources.sort_unstable();
            sources.dedup();
            sources
        })
        .collect();
    let count = relation.sources.len();
    let mut order = Vec::with_capacity(count);
    let mut bound = vec![false; count];
    while order.len() < count {
        let mut best = None;
        for candidate in 0..count {
            if bound[candidate] {
                continue;
            }
            let applicable = refs
                .iter()
                .filter(|sources| !sources.is_empty() && sources.iter().all(|source| *source == candidate || bound[*source]))
                .count();
            if best.map_or(true, |(score, _)| applicable > score) {
                best = Some((applicable, candidate));
            }
        }
        let (_, source) = best.expect("an unbound source always remains");
        bound[source] = true;
        order.push(source);
    }
    order
}

/// 下推键的执行期取值：字面量，或绑定到该位置的参数。
///
/// 参数在语句执行前已经绑定，形态与字面量等价，所以把「仅字面量」放宽到「执行期
/// 已知的值」不改变结果：下推只收窄候选行，全量谓词照旧在下游执行。缺失的绑定
/// 退化为 `NULL`，既不会被当成可下推的整数键，也不会改变求值结果。
pub(crate) fn known_cell(expr: &Expr, parameters: &[Cell]) -> Option<Cell> {
    match expr {
        Expr::Literal(literal) => Some(literal_cell(literal)),
        Expr::Parameter { index, .. } => Some(parameters.get(*index).cloned().unwrap_or(Cell::Null)),
        _ => None,
    }
}

/// 把可精确表示整数的 REAL 折成 rowid 键，供等值下推使用。
///
/// 只接受有限、无小数部分且落在 `2^53` 内的值：这个范围内 f64 能精确表示每个
/// 整数，折算不会改变比较结果。超出范围或带小数时返回 `None`，调用方保持整表
/// 扫描，让通用比较逻辑按亲和规则处理。
pub(crate) fn integral_rowid(value: f64) -> Option<i64> {
    if !value.is_finite() || value.fract() != 0.0 || value.abs() > 9_007_199_254_740_992.0 {
        return None;
    }
    Some(value as i64)
}

/// Marks the columns one `WHERE` expression can read, mirroring the resolver's
/// name lookup for a single table source.
///
/// Returns `false` when a read cannot be attributed from the raw syntax (a
/// subquery, a window call, another table's qualifier, an unknown name), which
/// tells the caller to decode every column instead of guessing.
pub(crate) fn predicate_columns(expr: &Expr, label: &str, def: &TableDef, keep: &mut [bool]) -> bool {
    let mut refs = Vec::new();
    if !collect_unmasked_columns(expr, &mut refs) {
        return false;
    }
    for column in refs {
        if column.qualifier.as_deref().is_some_and(|qualifier| !qualifier.eq_ignore_ascii_case(label)) {
            return false;
        }
        // A declared column shadows the implicit rowid names, exactly as the
        // relation's own lookup does.
        match def.columns.iter().position(|entry| entry.name.eq_ignore_ascii_case(&column.name)) {
            Some(position) => {
                if let Some(flag) = keep.get_mut(position) {
                    *flag = true;
                }
            }
            None if is_rowid_alias(&column.name) => {}
            None => return false,
        }
    }
    true
}

/// Decodes one stored row into cells for every declared column.
pub fn decode_row(payload: &[u8], def: &TableDef, rowid: i64) -> Result<Vec<Cell>, SqlError> {
    decode_row_masked(payload, def, rowid, None)
}

/// Decodes one stored row, keeping only the declared columns `keep` flags.
///
/// A skipped position still walks its serial type and body bounds, so a corrupt
/// payload fails exactly as in the full decode, but its text or blob value is
/// never materialized. `None` keeps every column.
pub(crate) fn decode_row_masked(payload: &[u8], def: &TableDef, rowid: i64, keep: Option<&[bool]>) -> Result<Vec<Cell>, SqlError> {
    let mut cells = Vec::new();
    decode_row_into(payload, def, rowid, keep, &mut cells)?;
    Ok(cells)
}

/// Decodes one stored row into `cells`, reusing that buffer's allocation.
///
/// The result is exactly [`decode_row_masked`]'s: kept columns decoded, skipped
/// positions as [`Cell::Null`], short records padded with their declared
/// defaults and an `INTEGER PRIMARY KEY` alias filled from the rowid.
pub(crate) fn decode_row_into(
    payload: &[u8],
    def: &TableDef,
    rowid: i64,
    keep: Option<&[bool]>,
    cells: &mut Vec<Cell>,
) -> Result<(), SqlError> {
    decode_cells_kept_into(payload, keep, cells)?;
    // A record written before `ALTER TABLE ADD COLUMN` is short; SQLite reads
    // the missing trailing columns as their declared default.
    while cells.len() < def.columns.len() {
        let kept = keep.is_none_or(|mask| mask.get(cells.len()).copied().unwrap_or(false));
        let value = if kept {
            let column = &def.columns[cells.len()];
            let value = column.default_value.clone().unwrap_or(WriteValue::Null);
            write_value_cell(&apply_write_affinity(&column.declared_type, value))
        }
        else {
            Cell::Null
        };
        cells.push(value);
    }
    cells.truncate(def.columns.len());
    if let Some(index) = def.rowid_alias {
        if matches!(cells[index], Cell::Null) {
            cells[index] = Cell::Integer(rowid);
        }
    }
    Ok(())
}

/// Whether a decoded row set made with `broad` already carries every column a
/// `narrow` request needs. `None` is the full decode and covers any request.
pub(crate) fn mask_covers(broad: Option<&[bool]>, narrow: Option<&[bool]>) -> bool {
    match (broad, narrow) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(broad), Some(narrow)) => {
            narrow.iter().enumerate().all(|(position, needed)| !*needed || broad.get(position).copied().unwrap_or(false))
        }
    }
}

/// 一次惰性游标读取返回的行窗口。
pub struct LazyBatch {
    /// 本窗口的行,保持扫描顺序。
    pub rows: Vec<Vec<Cell>>,
    /// 扫描是否已经到达表末尾。
    pub done: bool,
}

/// 一个已经解析好的惰性单表扫描计划。
///
/// 计划里的表达式都已解析成 owned 形态,不借用引擎目录;执行期只需要逐行
/// 求值。整页快照与 b-tree 遍历状态在 [`LazyTableScan`] 里。
pub struct LazyCursor {
    plan: LazyPlan,
    scan: LazyTableScan,
    budget: Budget,
    /// 打开游标时绑定的参数快照。
    pub parameters: Vec<Cell>,
    /// 游标是否已经到达表末尾。
    pub finished: bool,
}

impl LazyCursor {
    /// 打开时推导出的结果列标签。
    pub fn columns(&self) -> &[String] {
        &self.plan.labels
    }
}

/// 一条惰性游标读取的定型计划:扫描所需的全部只读信息。
pub struct LazyPlan {
    /// 被扫描的表定义,用于解码存储记录。
    def: TableDef,
    /// 结果列标签。
    labels: Vec<String>,
    /// `SELECT *` 暴露的列数,不含隐藏 rowid。
    visible_width: usize,
    /// 命名投影;`None` 表示 `SELECT *`。
    projection: Option<Vec<ResolvedExpr>>,
    /// 逐行 `WHERE` 谓词。
    predicate: Option<ResolvedExpr>,
    /// 求值上下文里的列目录。
    shape_columns: Vec<(String, String)>,
    /// 解码时只需保留的列。
    mask: Option<Vec<bool>>,
}

/// 按表 b-tree 页顺序分批解码的扫描器。
///
/// `pages` 是打开游标时克隆的整页快照,后续写入只改引擎自己的页缓冲,因此
/// 扫描期间看到的内容始终是打开时刻的一致状态。内部页只用来定位叶子页,
/// 每片叶子页的载荷按行解码后进入队列,`next_rows` 每次只取要求的行数。
pub(crate) struct LazyTableScan {
    pages: Arc<Vec<u8>>,
    page_size: usize,
    usable: usize,
    page_count: usize,
    /// 尚未展开的页,栈顶是下一个要访问的子树。
    pending: Vec<u32>,
    /// 当前叶子页尚未返回的行。
    leaf: VecDeque<TableRow>,
    /// 页号→是否已访问,与存储层扫描同样的重复页检测。
    visited: Vec<bool>,
    visited_pages: usize,
    limits: TableLimits,
    total_payload: usize,
    total_rows: usize,
    last_rowid: Option<i64>,
    done: bool,
}

impl LazyTableScan {
    fn new(pages: Arc<Vec<u8>>, root_page: u32) -> Result<Self, SqlError> {
        if pages.is_empty() {
            return Err(corrupt_error("sqlite table root in empty database"));
        }
        let page_size = validate_database(&pages).map_err(storage_failure)?;
        let reserved = usize::from(*pages.get(20).ok_or_else(|| corrupt_error("sqlite header truncated"))?);
        let usable =
            page_size.checked_sub(reserved).filter(|usable| *usable >= 257).ok_or_else(|| corrupt_error("sqlite page size invalid"))?;
        let page_count = pages.len() / page_size;
        if page_count == 0 || root_page == 0 || root_page as usize > page_count {
            return Err(corrupt_error("sqlite table root page out of bounds"));
        }
        Ok(Self {
            pages,
            page_size,
            usable,
            page_count,
            pending: vec![root_page],
            leaf: VecDeque::new(),
            visited: vec![false; page_count + 1],
            visited_pages: 0,
            limits: TableLimits::query(),
            total_payload: 0,
            total_rows: 0,
            last_rowid: None,
            done: false,
        })
    }

    /// 取至多 `limit` 行,不足时继续展开后续叶子页。
    fn next_rows(&mut self, limit: usize) -> Result<Vec<TableRow>, SqlError> {
        let mut rows = Vec::new();
        while rows.len() < limit {
            match self.leaf.pop_front() {
                Some(row) => rows.push(row),
                None => {
                    if !self.load_leaf()? {
                        break;
                    }
                }
            }
        }
        Ok(rows)
    }

    /// 展开内部页直到拿到一片叶子页;整棵树读完时返回 `false`。
    fn load_leaf(&mut self) -> Result<bool, SqlError> {
        loop {
            let Some(number) = self.pending.pop()
            else {
                self.done = true;
                return Ok(false);
            };
            let pages = Arc::clone(&self.pages);
            let page = lazy_page(&pages, self.page_size, self.page_count, number)?;
            if self.visited_pages >= self.limits.max_pages {
                return Err(storage_error("sqlite table page limit exceeded"));
            }
            if self.visited[number as usize] {
                return Err(corrupt_error("sqlite repeated b-tree or overflow page"));
            }
            self.visited[number as usize] = true;
            self.visited_pages += 1;
            let header = if number == 1 { 100 } else { 0 };
            let kind = *page.get(header).ok_or_else(|| corrupt_error("sqlite b-tree header truncated"))?;
            let header_size = match kind {
                13 => 8,
                5 => 12,
                _ => return Err(corrupt_error("sqlite table b-tree page type invalid")),
            };
            let count = lazy_read_u16(page, header + 3)?;
            let pointers_end = header + header_size + count.saturating_mul(2);
            if pointers_end > page.len() || *page.get(header + 7).ok_or_else(|| corrupt_error("sqlite b-tree header truncated"))? > 60 {
                return Err(corrupt_error("sqlite b-tree header invalid"));
            }
            let raw_start = lazy_read_u16(page, header + 5)?;
            let content_start = if raw_start == 0 { 65536 } else { raw_start };
            if content_start < pointers_end || content_start > page.len() {
                return Err(corrupt_error("sqlite b-tree content boundary invalid"));
            }
            let mut children = Vec::new();
            let mut rows = Vec::new();
            for index in 0..count {
                let cell = lazy_read_u16(page, header + header_size + index.saturating_mul(2))?;
                if cell < content_start || cell >= page.len() {
                    return Err(corrupt_error("sqlite cell pointer out of bounds"));
                }
                if kind == 5 {
                    let child = lazy_read_u32(page, cell)?;
                    let tail = page.get(cell + 4..).ok_or_else(|| corrupt_error("sqlite interior cell truncated"))?;
                    decode_varint(tail).map_err(storage_failure)?;
                    children.push(child);
                    continue;
                }
                let (length, length_size) = decode_varint(&page[cell..]).map_err(storage_failure)?;
                let length = usize::try_from(length).map_err(|_| storage_error("sqlite payload size exceeds host"))?;
                if length > self.limits.max_payload_bytes {
                    return Err(storage_error("sqlite table payload limit exceeded"));
                }
                let total = self.total_payload.checked_add(length).ok_or_else(|| storage_error("sqlite table total payload overflow"))?;
                if total > self.limits.max_total_payload_bytes {
                    return Err(storage_error("sqlite table total payload limit exceeded"));
                }
                self.total_payload = total;
                if self.total_rows >= self.limits.max_rows {
                    return Err(storage_error("sqlite table row limit exceeded"));
                }
                let tail = page.get(cell + length_size..).ok_or_else(|| corrupt_error("sqlite table cell truncated"))?;
                let (rowid, rowid_size) = decode_varint(tail).map_err(storage_failure)?;
                let rowid = rowid as i64;
                if self.last_rowid.is_some_and(|last| rowid <= last) {
                    return Err(corrupt_error("sqlite table rows out of order"));
                }
                self.last_rowid = Some(rowid);
                self.total_rows += 1;
                let start = cell
                    .checked_add(length_size)
                    .and_then(|start| start.checked_add(rowid_size))
                    .ok_or_else(|| corrupt_error("sqlite cell length overflow"))?;
                let payload = lazy_cell_payload(
                    &pages,
                    self.page_size,
                    self.page_count,
                    self.usable,
                    page,
                    start,
                    length,
                    &mut self.visited,
                    &mut self.visited_pages,
                    &self.limits,
                )?;
                rows.push(TableRow { rowid, payload });
            }
            if kind == 5 {
                // 内部页最后还有一个指向最右子树的指针,它不属于任何 cell。
                children.push(lazy_read_u32(page, header + 8)?);
                for child in children.into_iter().rev() {
                    self.pending.push(child);
                }
            }
            else {
                self.leaf.extend(rows);
                if !self.leaf.is_empty() {
                    return Ok(true);
                }
            }
        }
    }
}

/// 判断 `ORDER BY` 是否与 b-tree 的 rowid 升序扫描完全一致。
///
/// 只接受 `*` 或裸列投影,避免投影别名改变 `ORDER BY` 的绑定目标;排序键
/// 必须是隐式 rowid 或 `INTEGER PRIMARY KEY` 别名,且不能 `DESC` 或覆盖
/// `NULLS` 顺序。
pub(crate) fn lazy_order_matches_scan(def: &TableDef, entry: &TableRef, items: &SelectItems, order: &[OrderKey]) -> bool {
    if order.is_empty() {
        return true;
    }
    if !matches!(items, SelectItems::Star)
        && !matches!(items, SelectItems::Named(exprs) if exprs.iter().all(|expr| matches!(expr, Expr::Column(_))))
    {
        return false;
    }
    let alias_name = def.rowid_alias.map(|index| def.columns[index].name.as_str());
    let shadowed = |name: &str| def.columns.iter().any(|column| column.name.eq_ignore_ascii_case(name));
    for key in order {
        if key.descending || key.nulls_first.is_some() {
            return false;
        }
        let Expr::Column(column) = &key.expr
        else {
            return false;
        };
        if column.qualifier.as_deref().is_some_and(|qualifier| !qualifier.eq_ignore_ascii_case(entry.label())) {
            return false;
        }
        let names_alias = alias_name.is_some_and(|name| column.name.eq_ignore_ascii_case(name));
        let names_hidden_rowid = is_rowid_alias(&column.name) && !shadowed(&column.name);
        if !names_alias && !names_hidden_rowid {
            return false;
        }
    }
    true
}

/// 取一页完整内容;页号越界按损坏处理。
pub(crate) fn lazy_page(pages: &[u8], page_size: usize, page_count: usize, number: u32) -> Result<&[u8], SqlError> {
    let index = number as usize;
    if index == 0 || index > page_count {
        return Err(corrupt_error("sqlite page reference out of bounds"));
    }
    let start = (index - 1) * page_size;
    pages.get(start..start + page_size).ok_or_else(|| corrupt_error("sqlite page truncated"))
}

/// 与上游 SQLite 的 U 值公式一致,返回载荷在本页存放的本地字节数。
pub(crate) fn lazy_local(usable: usize, length: usize) -> usize {
    let maximum = usable - 35;
    let minimum = ((usable - 12) * 32 / 255) - 23;
    if length <= maximum {
        return length;
    }
    let candidate = minimum + (length - minimum) % (usable - 4);
    if candidate <= maximum { candidate } else { minimum }
}

/// 组装一行载荷,溢出页按链读取,并记录访问过的溢出页。
#[allow(clippy::too_many_arguments)]
pub(crate) fn lazy_cell_payload(
    pages: &[u8],
    page_size: usize,
    page_count: usize,
    usable: usize,
    page: &[u8],
    offset: usize,
    length: usize,
    visited: &mut [bool],
    visited_pages: &mut usize,
    limits: &TableLimits,
) -> Result<Vec<u8>, SqlError> {
    let local = lazy_local(usable, length);
    let end = offset.checked_add(local).ok_or_else(|| corrupt_error("sqlite cell length overflow"))?;
    let initial = page.get(offset..end).ok_or_else(|| corrupt_error("sqlite cell payload truncated"))?;
    let mut payload = Vec::new();
    payload.try_reserve_exact(length).map_err(|_| storage_error("sqlite payload allocation failed"))?;
    payload.extend_from_slice(initial);
    if local == length {
        return Ok(payload);
    }
    let mut next = lazy_read_u32(page, end)?;
    while payload.len() < length {
        if *visited_pages >= limits.max_pages {
            return Err(storage_error("sqlite table page limit exceeded"));
        }
        let index = next as usize;
        if index == 0 || index > page_count {
            return Err(corrupt_error("sqlite page reference out of bounds"));
        }
        if visited[index] {
            return Err(corrupt_error("sqlite repeated b-tree or overflow page"));
        }
        visited[index] = true;
        *visited_pages += 1;
        let start = (index - 1) * page_size;
        let overflow = pages.get(start..start + usable).ok_or_else(|| corrupt_error("sqlite page truncated"))?;
        next = lazy_read_u32(overflow, 0)?;
        let count = (length - payload.len()).min(usable - 4);
        payload.extend_from_slice(overflow.get(4..4 + count).ok_or_else(|| corrupt_error("sqlite overflow payload truncated"))?);
    }
    if next != 0 {
        return Err(corrupt_error("sqlite overflow chain exceeds payload"));
    }
    Ok(payload)
}

pub(crate) fn lazy_read_u16(bytes: &[u8], offset: usize) -> Result<usize, SqlError> {
    let value = bytes.get(offset..offset + 2).ok_or_else(|| corrupt_error("sqlite b-tree field truncated"))?;
    Ok(usize::from(u16::from_be_bytes(value.try_into().expect("two bytes"))))
}

pub(crate) fn lazy_read_u32(bytes: &[u8], offset: usize) -> Result<u32, SqlError> {
    let value = bytes.get(offset..offset + 4).ok_or_else(|| corrupt_error("sqlite page field truncated"))?;
    Ok(u32::from_be_bytes(value.try_into().expect("four bytes")))
}

/// Decodes stored row payloads into merged rows, appending each hidden rowid.
pub(crate) fn decode_stored_rows(stored: &[TableRow], def: &TableDef, keep: Option<&[bool]>) -> Result<Vec<Vec<Cell>>, SqlError> {
    stored
        .iter()
        .map(|row| {
            let mut cells = decode_row_masked(&row.payload, def, row.rowid, keep)?;
            // Rowid tables expose the implicit integer primary key through
            // `rowid`, `_rowid_` and `oid`.
            cells.push(Cell::Integer(row.rowid));
            Ok(cells)
        })
        .collect()
}

/// Whether a bare column reference is qualified with exactly `label`.
pub(crate) fn column_qualified_as(column: &ColumnRef, label: &str) -> bool {
    column.qualifier.as_deref().is_some_and(|qualifier| qualifier.eq_ignore_ascii_case(label))
}

/// Collects the column references one expression can read, refusing any node
/// whose reads cannot be attributed from the raw AST.
///
/// Subqueries may read the enclosing row through a correlation, and window
/// functions carry their own partition and order expressions; both keep the
/// full decode instead of guessing.
pub(crate) fn collect_unmasked_columns<'a>(expr: &'a Expr, out: &mut Vec<&'a ColumnRef>) -> bool {
    match expr {
        Expr::Column(column) => {
            out.push(column);
            true
        }
        Expr::Literal(_) | Expr::Parameter { .. } => true,
        Expr::Unary { value, .. }
        | Expr::IsNull { value, .. }
        | Expr::IsTruth { value, .. }
        | Expr::Not(value)
        | Expr::Cast { value, .. }
        | Expr::Collate { value, .. }
        | Expr::Alias { value, .. } => collect_unmasked_columns(value, out),
        Expr::Binary { left, right, .. }
        | Expr::Compare { left, right, .. }
        | Expr::IsDistinct { left, right, .. }
        | Expr::Glob { value: left, pattern: right, .. } => collect_unmasked_columns(left, out) && collect_unmasked_columns(right, out),
        Expr::And(left, right) | Expr::Or(left, right) => collect_unmasked_columns(left, out) && collect_unmasked_columns(right, out),
        Expr::Case { operand, branches, otherwise } => {
            operand.as_ref().is_none_or(|operand| collect_unmasked_columns(operand, out))
                && branches.iter().all(|(condition, result)| collect_unmasked_columns(condition, out) && collect_unmasked_columns(result, out))
                && otherwise.as_ref().is_none_or(|otherwise| collect_unmasked_columns(otherwise, out))
        }
        Expr::In { value, list, .. } => collect_unmasked_columns(value, out) && list.iter().all(|item| collect_unmasked_columns(item, out)),
        Expr::Function { args, .. } | Expr::HostFunction { args, .. } => args.iter().all(|arg| collect_unmasked_columns(arg, out)),
        Expr::Aggregate { argument, separator, filter, order, .. } => {
            argument.as_ref().is_none_or(|argument| collect_unmasked_columns(argument, out))
                && separator.as_ref().is_none_or(|separator| collect_unmasked_columns(separator, out))
                && filter.as_ref().is_none_or(|filter| collect_unmasked_columns(filter, out))
                && order.iter().all(|key| collect_unmasked_columns(&key.expr, out))
        }
        Expr::Like { value, pattern, escape, .. } => {
            collect_unmasked_columns(value, out)
                && collect_unmasked_columns(pattern, out)
                && escape.as_ref().is_none_or(|escape| collect_unmasked_columns(escape, out))
        }
        Expr::Subquery(_) | Expr::Exists(_) | Expr::InSubquery { .. } | Expr::Window { .. } => false,
    }
}

/// Decodes one record payload into `cells`, materializing only the positions
/// `keep` flags and reusing that buffer's allocation.
///
/// Mirrors the storage record decoder for layout validation (header and body
/// bounds, reserved serial types, column limit, trailing bytes) while skipping
/// the text and blob conversion of columns the statement never reads.
pub(crate) fn decode_cells_kept_into(payload: &[u8], keep: Option<&[bool]>, cells: &mut Vec<Cell>) -> Result<(), SqlError> {
    cells.clear();
    let limits = RecordLimits { max_columns: 1024, max_payload_bytes: 16 * 1024 * 1024 };
    let corrupt = |message: &str| SqlError::new("corrupt", message).in_statement("select");
    let limit = |message: &str| SqlError::new("unsupported", message).in_statement("select");
    if payload.len() > limits.max_payload_bytes {
        return Err(limit("sqlite record payload limit exceeded"));
    }
    let (header_size, mut header_offset) = decode_varint(payload).map_err(|error| corrupt(&error.to_string()))?;
    let header_size = usize::try_from(header_size).map_err(|_| corrupt("sqlite record header size overflow"))?;
    if header_size < header_offset || header_size > payload.len() {
        return Err(corrupt("sqlite record header size invalid"));
    }
    let mut body_offset = header_size;
    while header_offset < header_size {
        if cells.len() >= limits.max_columns {
            return Err(limit("sqlite record column limit exceeded"));
        }
        let (serial, consumed) = decode_varint(&payload[header_offset..header_size]).map_err(|error| corrupt(&error.to_string()))?;
        header_offset += consumed;
        let length = match serial {
            0 | 8 | 9 => 0,
            1 => 1,
            2 => 2,
            3 => 3,
            4 => 4,
            5 => 6,
            6 | 7 => 8,
            10 | 11 => return Err(corrupt("sqlite reserved serial type")),
            serial => usize::try_from((serial - 12) / 2).map_err(|_| corrupt("sqlite serial length overflow"))?,
        };
        let end = body_offset.checked_add(length).filter(|end| *end <= payload.len()).ok_or_else(|| corrupt("truncated sqlite record body"))?;
        let bytes = &payload[body_offset..end];
        let position = cells.len();
        let kept = keep.is_none_or(|mask| mask.get(position).copied().unwrap_or(false));
        cells.push(if kept {
            match serial {
                0 => Cell::Null,
                1..=6 => Cell::Integer(signed_record_integer(bytes)),
                7 => Cell::Real(f64::from_bits(u64::from_be_bytes(bytes.try_into().expect("eight-byte real")))),
                8 => Cell::Integer(0),
                9 => Cell::Integer(1),
                serial if serial % 2 == 0 => Cell::Blob(bytes.to_vec()),
                _ => Cell::Text(record_text(bytes)?),
            }
        }
        else {
            Cell::Null
        });
        body_offset = end;
    }
    if body_offset != payload.len() {
        return Err(corrupt("sqlite record has trailing body bytes"));
    }
    Ok(())
}

/// Decodes one record text field exactly as the storage decoder does.
pub(crate) fn record_text(bytes: &[u8]) -> Result<String, SqlError> {
    std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| SqlError::new("corrupt", "sqlite non-unicode UTF-8 text").in_statement("select"))
}

/// Sign-extends a big-endian SQLite integer field of one to eight bytes.
pub(crate) fn signed_record_integer(bytes: &[u8]) -> i64 {
    let mut value = if bytes[0] & 0x80 != 0 { u64::MAX } else { 0 };
    for byte in bytes {
        value = (value << 8) | u64::from(*byte);
    }
    value as i64
}

impl<'e> ExecContext<'e> {
    /// Whether one table or view carries at least one trigger of any kind.
    pub fn table_has_triggers(&self, table: &str) -> bool {
        self.catalog.triggers.values().any(|trigger| trigger.table == table)
    }

    /// Materializes one reserved table with the labels and row order SQLite
    /// reports. `sqlite_master` is read straight from the writer's schema
    /// b-tree, so `type`, `sql`, `rootpage`, `rowid` and ordering are the
    /// on-disk values rather than a second copy that could drift.
    pub fn system_rows(&self, system: SystemTable, name: &str) -> Result<(Vec<String>, Vec<Vec<Cell>>, bool), SqlError> {
        let master_labels = || vec!["type".to_string(), "name".to_string(), "tbl_name".to_string(), "rootpage".to_string(), "sql".to_string()];
        match system {
            SystemTable::Master => {
                let objects = self.writer.schema_objects_with_rowids(TableLimits::query()).map_err(storage_error)?;
                let mut rows = Vec::with_capacity(objects.len());
                for (rowid, object) in objects {
                    let kind = match object.kind {
                        SchemaObjectKind::Table => "table",
                        SchemaObjectKind::Index => "index",
                        SchemaObjectKind::View => "view",
                        SchemaObjectKind::Trigger => "trigger",
                    };
                    rows.push(vec![
                        Cell::Text(kind.to_string()),
                        Cell::Text(object.name),
                        Cell::Text(object.table_name),
                        object.root_page.map_or(Cell::Null, |page| Cell::Integer(i64::from(page))),
                        object.sql.map_or(Cell::Null, Cell::Text),
                        Cell::Integer(rowid),
                    ]);
                }
                Ok((master_labels(), rows, true))
            }
            SystemTable::TempMaster => {
                let mut rows = Vec::with_capacity(self.catalog.temp_tables.len() + self.catalog.temp_views.len() + self.catalog.triggers.len());
                for table in &self.catalog.temp_tables {
                    rows.push(vec![
                        Cell::Text("table".to_string()),
                        Cell::Text(table.def.name.clone()),
                        Cell::Text(table.def.name.clone()),
                        Cell::Integer(i64::from(table.rootpage)),
                        Cell::Text(table.sql.clone()),
                        Cell::Integer(table.master_rowid),
                    ]);
                }
                for view in self.catalog.temp_views.values() {
                    rows.push(vec![
                        Cell::Text("view".to_string()),
                        Cell::Text(view.def.name.clone()),
                        Cell::Text(view.def.name.clone()),
                        Cell::Integer(0),
                        Cell::Text(view.sql.clone()),
                        Cell::Integer(view.master_rowid),
                    ]);
                }
                for trigger in self.catalog.triggers.values().filter(|trigger| trigger.temp) {
                    rows.push(vec![
                        Cell::Text("trigger".to_string()),
                        Cell::Text(trigger.name.clone()),
                        Cell::Text(trigger.table.clone()),
                        Cell::Integer(0),
                        Cell::Text(trigger.sql.clone()),
                        Cell::Integer(trigger.master_rowid),
                    ]);
                }
                rows.sort_by_key(|row| match row.get(5) {
                    Some(Cell::Integer(rowid)) => *rowid,
                    _ => 0,
                });
                Ok((master_labels(), rows, true))
            }
            SystemTable::Sequence => {
                let present = self
                    .writer
                    .schema_objects(TableLimits::query())
                    .map_err(storage_error)?
                    .iter()
                    .any(|object| object.kind == SchemaObjectKind::Table && object.name.eq_ignore_ascii_case(name));
                if !present {
                    return Err(SqlError::new("unknown-table", format!("no such table: {name}")).in_statement("select"));
                }
                let stored = self.writer.table_rows("sqlite_sequence", TableLimits::query()).map_err(storage_error)?;
                let mut rows = Vec::with_capacity(stored.len());
                for row in &stored {
                    let mut cells = decode_cells(&row.payload)?;
                    cells.resize(2, Cell::Null);
                    cells.push(Cell::Integer(row.rowid));
                    rows.push(cells);
                }
                Ok((vec!["name".to_string(), "seq".to_string()], rows, true))
            }
        }
    }

    /// Output columns and rows of one view, used as the source of
    /// `INSTEAD OF` triggers.
    pub fn view_output(&self, view: &ViewDef) -> Result<(Vec<String>, Vec<Vec<Cell>>), SqlError> {
        let budget = Budget::new();
        let result = self.select_statement(&view.query, &[], &budget, None)?;
        let names = view_output_names(view, &result.columns)?;
        Ok((names, result.rows))
    }

    /// Materializes one view exactly like a `FROM` reference does: the stored
    /// query runs again, then its output names are declared or inferred.
    pub fn expand_view(&self, view: &ViewDef, budget: &Budget) -> Result<(Vec<String>, Vec<Vec<Cell>>), SqlError> {
        let result = self.select_statement(&view.query, &[], budget, None)?;
        let names = view_output_names(view, &result.columns)?;
        Ok((names, result.rows))
    }

    /// Executes one `SELECT` over the comma-separated `FROM` list.
    ///
    /// The sources are flattened into one row shape in `FROM` order, then
    /// combined as a Cartesian product. Predicates are applied as soon as every
    /// source they read is bound, so an unselective `WHERE` never materializes
    /// the full product. Nested subqueries re-enter this same function with the
    /// current row pushed onto `outer`.
    pub(crate) fn select(
        &self,
        first: &SelectCore,
        compounds: &[CompoundTerm],
        order: Vec<OrderKey>,
        limit: Option<Limit>,
        outer: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv>,
    ) -> Result<SubqueryOutcome, SqlError> {
        let limit = self.eval_limit(limit.as_ref(), outer, budget, ctes)?;
        // `LIMIT 0` drops every row, so a single base-table `FROM` is never read.
        // The empty hint keeps the schema-derived column names intact, which is
        // all this statement can observe.
        let empty_limit = matches!(limit, Some((_, Some(0))));
        if !compounds.is_empty() {
            return self.compound_select(first, compounds, order, limit, outer, budget, ctes);
        }
        // `count(*)` over one plain rowid table is answered straight from the
        // b-tree payloads: each record is decoded into one reused cell buffer,
        // tested against the `WHERE` conjuncts and only counted. Rows are never
        // collected into a row vector, so a counting statement pays no
        // per-row `Vec<Cell>` allocation and no full-table decode. The helper
        // only accepts a narrow shape and returns `None` for everything else,
        // which keeps the generic path authoritative for the rest.
        if let Some(result) = self.count_only_scan(first, &order, limit, outer, budget, ctes)? {
            return Ok(result);
        }
        let SelectCore { distinct, items, from, filter, group, having, windows: named } = first;
        // Each `FROM` item becomes one source over already materialized rows:
        // a base table is decoded from storage, a CTE comes from the `WITH`
        // environment, and a derived table is evaluated once through this same
        // `select` (as a plain subquery, never correlated).
        // A single base rowid table with a `WHERE <rowid> = <integer literal>`
        // conjunct is bounded to its one candidate row before decoding; failing
        // that, a `WHERE` conjunction covering every key column of a plain
        // column index probes the index instead of decoding the whole table.
        // Both keep the full predicate running downstream, so the emitted row
        // set and order are unchanged.
        // 相关子查询的外层键探针比字面量区间更窄(通常只剩一行),所以先试它;
        // 只在只有一层外引用时生效,普通查询仍是零开销。
        let scan = self.runtime_index_pin(from, filter, outer).or_else(|| self.scan_hint(from, filter));
        // `ORDER BY <rowid order> LIMIT n` reads only the leading rows, because
        // the scan order already satisfies that sort.
        let scan = scan.or_else(|| self.rowid_head_bound(first, &order, limit).map(ScanHint::Head));
        // `LIMIT 0` is answered without touching the table. Only one base source
        // is narrowed: a derived table still runs (and may still raise), so the
        // statement keeps every error it would have reported.
        let scan = if empty_limit && from.len() == 1 && from[0].query.is_none() { Some(ScanHint::Empty) } else { scan };
        // Columns the statement never reads are skipped while decoding the base
        // table, so a `count(*)` join or an aggregate does not materialize every
        // text cell. The mask is derived from the raw expressions; any shape it
        // cannot attribute confidently falls back to decoding every column.
        let masks = self.read_column_masks(first, &order);
        // 多来源 `FROM` 里,只读单个来源的 `WHERE` 合取可以在读该来源时就
        // 收窄候选,不必先物化整表再在连接后过滤。单来源时整表过滤已由
        // 上面的 `scan` 覆盖。
        let local_hints: Vec<Option<ScanHint>> = if from.len() == 1 || scan.is_some() {
            Vec::new()
        }
        else {
            (0..from.len())
                .map(|index| {
                    let local = self.source_filter(index, from, filter)?;
                    self.scan_hint(std::slice::from_ref(&from[index]), &Some(local))
                })
                .collect()
        };
        let mut sources = Vec::with_capacity(from.len());
        let mut tables: Vec<Rc<Vec<Vec<Cell>>>> = Vec::with_capacity(from.len());
        let mut offset = 0;
        for (index, table) in from.iter().enumerate() {
            let keep = masks.as_ref().and_then(|masks| masks.get(index)).and_then(|mask| mask.as_deref());
            let local = local_hints.get(index).and_then(|hint| hint.as_ref());
            // 等值连接的内层能按索引取值时只读匹配行,不必先物化整张右表。
            let join_pin =
                if index > 0 && scan.is_none() && local.is_none() { self.join_index_pin(from, index, &tables, masks.as_deref()) } else { None };
            let hint = scan.as_ref().or(local).or(join_pin.as_ref());
            let (columns, rows) = self.select_source(table, budget, ctes, hint, keep)?;
            let width = columns.width();
            let source = Source {
                label: table.label().to_string(),
                columns,
                offset,
                join: table.join,
                star_hidden: vec![false; width],
                unqualified_hidden: vec![false; width],
                star_override: vec![None; width],
            };
            offset += source.width();
            sources.push(source);
            tables.push(rows);
        }
        let mut relation = Relation { sources, using: vec![Vec::new(); from.len()], using_slots: Vec::new(), trigger: None, excluded: false };
        relation.plan_joins(&from)?;
        let shape = relation.shape();
        let outer_shapes: Vec<ScopeShape> = outer.iter().map(Scope::shape).collect();
        // Join predicates are resolved once, against the whole `FROM`, and
        // rejected when they reach a source that is not yet in scope.
        let mut ons: Vec<Option<ResolvedExpr>> = Vec::with_capacity(relation.sources.len());
        for (index, table) in from.iter().enumerate() {
            let resolved = match &table.on {
                None => None,
                Some(on) => {
                    let mut scratch_aggregates = Vec::new();
                    let mut scratch_windows = Vec::new();
                    let resolved =
                        resolve_expr(on, &relation, &outer_shapes, &[], &mut scratch_windows, &mut scratch_aggregates, false, false)?;
                    let mut refs = Vec::new();
                    resolved.collect_sources(&mut refs);
                    if refs.iter().any(|source| *source > index) {
                        return Err(SqlError::new(
                            "unknown-column",
                            format!("the `ON` clause of source {} may not reference a later source", index + 1),
                        )
                        .in_statement("select"));
                    }
                    Some(resolved)
                }
            };
            ons.push(resolved);
        }
        // Resolve every expression before reading rows, so a bad name fails closed.
        let mut specs = Vec::new();
        let mut windows: Vec<ResolvedWindow> = Vec::new();
        let projection: Option<Vec<ResolvedExpr>> = match items {
            SelectItems::Named(exprs) => Some(
                exprs
                    .iter()
                    .map(|expr| resolve_expr(expr, &relation, &outer_shapes, named, &mut windows, &mut specs, true, true))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            SelectItems::CountAll | SelectItems::Star => None,
        };
        let labels = core_output_labels(&relation, items);
        let output_width = labels.len();
        // 输出列各自的排序规则：`DISTINCT`、`GROUP BY` 序数与 `ORDER BY` 序数
        // 都按它比较文本，与上游对结果列排序规则的继承一致。
        let output_collations = match items {
            SelectItems::Named(_) => projection
                .as_ref()
                .map(|nodes| nodes.iter().map(|node| resolved_collation(node, &relation)).collect::<Vec<_>>())
                .unwrap_or_default(),
            SelectItems::Star => relation.visible_collations(),
            SelectItems::CountAll => vec![Collation::Binary],
        };
        let output_collation = |position: usize| output_collations.get(position).copied().unwrap_or_default();
        // Aggregates are only defined over the whole result, so a predicate or
        // sort key that mixes one in is rejected instead of being evaluated per
        // row. Window functions are likewise limited to projection and `ORDER BY`.
        let predicate = match filter {
            Some(expr) => {
                let mut scratch_aggregates = Vec::new();
                let mut scratch_windows = Vec::new();
                Some(resolve_expr(expr, &relation, &outer_shapes, &[], &mut scratch_windows, &mut scratch_aggregates, false, false)?)
            }
            None => None,
        };
        // `GROUP BY` accepts a projection ordinal or any expression over the
        // current row.
        let mut group_keys = Vec::with_capacity(group.len());
        for expr in group {
            if let Expr::Literal(Literal::Integer(position)) = expr {
                let position = *position;
                if position < 1 || position as usize > output_width {
                    return Err(SqlError::new(
                        "order-term-out-of-range",
                        format!("GROUP BY term out of range - should be between 1 and {output_width}"),
                    )
                    .in_statement("select"));
                }
                group_keys.push(GroupKey::Projected(position as usize - 1, output_collation(position as usize - 1)));
                continue;
            }
            let mut scratch_aggregates = Vec::new();
            let mut scratch_windows = Vec::new();
            let resolved = resolve_expr(expr, &relation, &outer_shapes, &[], &mut scratch_windows, &mut scratch_aggregates, false, false)?;
            let collation = resolved_collation(&resolved, &relation);
            group_keys.push(GroupKey::Expression(resolved, collation));
        }
        // `HAVING` shares the projection's aggregate list, so it may add an
        // aggregate that the projection does not mention. Window functions are
        // not allowed there; SQLite reports that as a `misuse`.
        let having_expr = match having {
            Some(expr) => Some(resolve_expr(expr, &relation, &outer_shapes, named, &mut windows, &mut specs, true, false)?),
            None => None,
        };
        let keys: Vec<(SortKey, SortDirection)> = order
            .iter()
            .map(|entry| {
                // A bare integer is a 1-based output-column ordinal, as in SQLite.
                if let Expr::Literal(Literal::Integer(position)) = &entry.expr {
                    let position = *position;
                    if position < 1 || position as usize > output_width {
                        return Err(SqlError::new(
                            "order-term-out-of-range",
                            format!("ORDER BY term out of range - should be between 1 and {output_width}"),
                        )
                        .in_statement("select"));
                    }
                    return Ok((
                        SortKey::Projected(position as usize - 1),
                        SortDirection::of(entry).with_collation(output_collation(position as usize - 1)),
                    ));
                }
                // An unqualified name that matches a projection alias sorts by
                // the output column; SQLite prefers the alias over a same-named
                // base column.
                if let Expr::Column(column) = &entry.expr {
                    if column.qualifier.is_none() {
                        if let Some(position) = labels.iter().position(|label| label.eq_ignore_ascii_case(&column.name)) {
                            return Ok((SortKey::Projected(position), SortDirection::of(entry).with_collation(output_collation(position))));
                        }
                    }
                }
                let resolved = resolve_expr(&entry.expr, &relation, &outer_shapes, named, &mut windows, &mut specs, true, true)?;
                let collation = resolved_collation(&resolved, &relation);
                Ok((SortKey::Expression(resolved), SortDirection::of(entry).with_collation(collation)))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let grouped = !group_keys.is_empty() || !specs.is_empty();
        if having_expr.is_some() && !grouped {
            // SQLite rejects `HAVING` without a group or aggregate rather than
            // treating it as a second `WHERE`.
            let mut error = SqlError::new("misuse-of-aggregate", "HAVING clause on a non-aggregate query").in_statement("select");
            error.clause = Some("HAVING".to_string());
            return Err(error);
        }
        let joined = relation.sources.iter().any(|source| source.join.is_some());
        let windowed = !windows.is_empty();
        // `count(*)` over a plain product never has to materialize a row.
        if predicate.is_none() && keys.is_empty() && !joined && !grouped && having_expr.is_none() && !windowed {
            if let SelectItems::CountAll = items {
                let count = tables.iter().fold(1i64, |total, rows| total.saturating_mul(rows.len() as i64));
                let visible = match limit {
                    None => true,
                    Some((offset, count)) => offset == 0 && count.map_or(true, |count| count > 0),
                };
                return Ok(SubqueryOutcome { columns: labels, rows: if visible { vec![vec![Cell::Integer(count)]] } else { Vec::new() } });
            }
        }

        // A `LIMIT` may stop the product early only when no sort, join, aggregate
        // or window is pending. `DISTINCT` reads every row because collapsing
        // duplicates before `LIMIT` needs the whole set.
        let take = if keys.is_empty() && !joined && !grouped && !windowed && !*distinct {
            match limit {
                Some((offset, Some(count))) => offset.saturating_add(count),
                _ => usize::MAX,
            }
        }
        else {
            usize::MAX
        };
        let empty_aggregates: Vec<Cell> = Vec::new();
        let ctx = EvalContext {
            exec: self.clone(),
            parameters: self.parameters,
            upsert_excluded: self.upsert_excluded,
            trigger_frames: self.trigger_frames,
            outer_read: &self.state.outer_read,
            subquery_cache: &self.state.subquery_cache,
            host_functions: self.host_functions,
            outer,
            current: &shape,
            aggregates: &empty_aggregates,
            windows: &[],
            window_row: 0,
            budget,
            ctes,
        };
        let mut conjuncts = Vec::new();
        if let Some(predicate) = predicate {
            split_conjuncts(predicate, &mut conjuncts);
        }
        // 裸 `count(*)` 的内连接只需要计数,不必把每个匹配行合并成
        // `Vec<Cell>`。限定在全部为普通源或内连接的形态,其余连接走通用路径。
        let bare_count_star = grouped
            && keys.is_empty()
            && having_expr.is_none()
            && !windowed
            && !*distinct
            && matches!(projection.as_deref(), Some([ResolvedExpr::AggregateRef(0)]))
            && matches!(specs.as_slice(), [spec]
                if matches!(spec.function, AggregateFunction::CountStar)
                    && spec.argument.is_none()
                    && spec.filter.is_none()
                    && !spec.distinct
                    && spec.order.is_empty());
        if bare_count_star
            && joined
            && relation.sources.iter().all(|source| matches!(source.join, None | Some(JoinKind::Inner)))
            && relation.using.iter().all(|pairs| pairs.is_empty())
        {
            let count = count_inner_join_rows(&relation, &ons, &tables, &conjuncts, &ctx)?;
            let visible = match limit {
                None => true,
                Some((offset, limit)) => offset == 0 && limit.map_or(true, |limit| limit > 0),
            };
            let rows = if visible { vec![vec![Cell::Integer(count as i64)]] } else { Vec::new() };
            return Ok(SubqueryOutcome { columns: labels, rows });
        }
        if bare_count_star && !joined {
            let count = count_cross_product_rows(&tables, &relation, &conjuncts, &ctx)?;
            let visible = match limit {
                None => true,
                Some((offset, limit)) => offset == 0 && limit.map_or(true, |limit| limit > 0),
            };
            let rows = if visible { vec![vec![Cell::Integer(count as i64)]] } else { Vec::new() };
            return Ok(SubqueryOutcome { columns: labels, rows });
        }
        let rows = if joined {
            join_rows(&relation, &ons, &tables, &conjuncts, take, &ctx)?
        }
        else {
            cross_product(tables, &relation, &conjuncts, take, &ctx)?
        };
        if grouped {
            // One group per distinct key; without `GROUP BY` there is exactly
            // one group, even when it is empty, because aggregates must still
            // produce their empty-input values.
            let groups = build_groups(&group_keys, &projection, rows, &ctx, budget)?;
            let mut inputs: Vec<WindowRow> = Vec::with_capacity(groups.len());
            let mut sizes: Vec<i64> = Vec::with_capacity(groups.len());
            for group_rows in groups {
                let aggregates = specs.iter().map(|spec| compute_aggregate(spec, &group_rows, &ctx)).collect::<Result<Vec<_>, _>>()?;
                let representative = group_rows.first().cloned().unwrap_or_else(|| vec![Cell::Null; shape.columns.len()]);
                let group_ctx = EvalContext {
                    exec: self.clone(),
                    parameters: self.parameters,
                    upsert_excluded: self.upsert_excluded,
                    trigger_frames: self.trigger_frames,
                    outer_read: &self.state.outer_read,
                    subquery_cache: &self.state.subquery_cache,
                    host_functions: self.host_functions,
                    outer,
                    current: &shape,
                    aggregates: &aggregates,
                    windows: &[],
                    window_row: 0,
                    budget,
                    ctes,
                };
                if let Some(having) = &having_expr {
                    if truthiness(&having.eval(&representative, &group_ctx)?) != Some(true) {
                        continue;
                    }
                }
                sizes.push(group_rows.len() as i64);
                inputs.push(WindowRow { values: representative, aggregates });
            }
            let (window_values, window_keys, window_desc) = if windowed {
                self.evaluate_windows(&windows, &inputs, &shape, outer, budget, ctes)?
            }
            else {
                (Vec::new(), Vec::new(), Vec::new())
            };
            let mut decorated: Vec<(Vec<Cell>, Vec<Cell>, usize)> = Vec::with_capacity(inputs.len());
            for (index, row) in inputs.iter().enumerate() {
                let row_ctx = EvalContext {
                    exec: self.clone(),
                    parameters: self.parameters,
                    upsert_excluded: self.upsert_excluded,
                    trigger_frames: self.trigger_frames,
                    outer_read: &self.state.outer_read,
                    subquery_cache: &self.state.subquery_cache,
                    host_functions: self.host_functions,
                    outer,
                    current: &shape,
                    aggregates: &row.aggregates,
                    windows: &window_values,
                    window_row: index,
                    budget,
                    ctes,
                };
                let cells = match items {
                    SelectItems::Named(_) => projection
                        .as_ref()
                        .expect("named projection is resolved up front")
                        .iter()
                        .map(|node| node.eval(&row.values, &row_ctx))
                        .collect::<Result<Vec<_>, _>>()?,
                    SelectItems::CountAll => vec![Cell::Integer(sizes[index])],
                    SelectItems::Star => relation.visible_row(&row.values),
                };
                let mut sort_cells = Vec::with_capacity(keys.len());
                for (key, _) in &keys {
                    sort_cells.push(match key {
                        SortKey::Projected(index) => cells.get(*index).cloned().unwrap_or(Cell::Null),
                        SortKey::Expression(expr) => expr.eval(&row.values, &row_ctx)?,
                    });
                }
                decorated.push((sort_cells, cells, index));
            }
            if !keys.is_empty() {
                decorated.sort_by(|left, right| compare_key_rows(&left.0, &right.0, &keys));
            }
            else if windowed {
                decorated.sort_by(|left, right| compare_ordered(&window_keys[left.2], &window_keys[right.2], &window_desc));
            }
            let mut out: Vec<Vec<Cell>> = decorated.into_iter().map(|(_, cells, _)| cells).collect();
            if *distinct {
                out = retain_first_distinct(out, &output_collations);
            }
            truncate_rows(&mut out, limit);
            return Ok(SubqueryOutcome { columns: labels, rows: out });
        }
        let (window_values, window_keys, window_desc) = if windowed {
            let inputs: Vec<WindowRow> = rows.iter().map(|values| WindowRow { values: values.clone(), aggregates: Vec::new() }).collect();
            self.evaluate_windows(&windows, &inputs, &shape, outer, budget, ctes)?
        }
        else {
            (Vec::new(), Vec::new(), Vec::new())
        };
        let mut order: Vec<usize> = (0..rows.len()).collect();
        if !keys.is_empty() {
            // Sort keys may be projection ordinals or expressions over the
            // merged source row; evaluate both through the same evaluator.
            let mut sort_cells: Vec<Vec<Cell>> = Vec::with_capacity(rows.len());
            for (index, row) in rows.iter().enumerate() {
                let row_ctx = EvalContext {
                    exec: self.clone(),
                    parameters: self.parameters,
                    upsert_excluded: self.upsert_excluded,
                    trigger_frames: self.trigger_frames,
                    outer_read: &self.state.outer_read,
                    subquery_cache: &self.state.subquery_cache,
                    host_functions: self.host_functions,
                    outer,
                    current: &shape,
                    aggregates: &empty_aggregates,
                    windows: &window_values,
                    window_row: index,
                    budget,
                    ctes,
                };
                let mut cells = Vec::with_capacity(keys.len());
                for (key, _) in &keys {
                    cells.push(match key {
                        SortKey::Projected(index) => match &projection {
                            Some(nodes) => nodes[*index].eval(row, &row_ctx)?,
                            None => Cell::Null,
                        },
                        SortKey::Expression(expr) => expr.eval(row, &row_ctx)?,
                    });
                }
                sort_cells.push(cells);
            }
            order.sort_by(|left, right| compare_key_rows(&sort_cells[*left], &sort_cells[*right], &keys));
        }
        else if windowed {
            // Without a trailing `ORDER BY`, SQLite's row order is the first
            // window's sort order, with later windows breaking its ties.
            order.sort_by(|left, right| compare_ordered(&window_keys[*left], &window_keys[*right], &window_desc));
        }
        if *distinct && !matches!(items, SelectItems::CountAll) {
            // Every row's output has to be materialized before `LIMIT`, because
            // `DISTINCT` collapses duplicates across the whole set.
            let mut outputs: Vec<Vec<Cell>> = Vec::with_capacity(rows.len());
            for (index, row) in rows.iter().enumerate() {
                let cells = match items {
                    SelectItems::Named(_) => {
                        let nodes = projection.as_ref().expect("named projection is resolved up front");
                        let row_ctx = EvalContext {
                            exec: self.clone(),
                            parameters: self.parameters,
                            upsert_excluded: self.upsert_excluded,
                            trigger_frames: self.trigger_frames,
                            outer_read: &self.state.outer_read,
                            subquery_cache: &self.state.subquery_cache,
                            host_functions: self.host_functions,
                            outer,
                            current: &shape,
                            aggregates: &empty_aggregates,
                            windows: &window_values,
                            window_row: index,
                            budget,
                            ctes,
                        };
                        nodes.iter().map(|node| node.eval(row, &row_ctx)).collect::<Result<Vec<_>, _>>()?
                    }
                    SelectItems::Star => relation.visible_row(row),
                    SelectItems::CountAll => unreachable!("count(*) keeps a single row"),
                };
                outputs.push(cells);
            }
            let ordered: Vec<Vec<Cell>> = order.iter().map(|index| outputs[*index].clone()).collect();
            let unique = retain_first_distinct(ordered, &output_collations);
            let (start, end) = limit_range(unique.len(), limit);
            let out: Vec<Vec<Cell>> = unique.into_iter().skip(start).take(end - start).collect();
            return Ok(SubqueryOutcome { columns: labels, rows: out });
        }
        let total_rows = rows.len();
        let (start, end) = limit_range(total_rows, limit);
        // Without a sort key or a window order the surviving rows are already in
        // emission order, so they move out of the product instead of being
        // cloned cell by cell.
        let unordered = keys.is_empty() && !windowed;
        let mut final_rows: Vec<Vec<Cell>> = Vec::with_capacity(end.saturating_sub(start));
        let mut final_windows: Vec<Vec<Cell>> = vec![Vec::new(); window_values.len()];
        if unordered {
            final_rows.extend(rows.into_iter().skip(start).take(end - start));
        }
        else {
            for &index in &order[start..end] {
                final_rows.push(rows[index].clone());
                for (column, values) in final_windows.iter_mut().zip(window_values.iter()) {
                    column.push(values[index].clone());
                }
            }
        }
        match items {
            SelectItems::CountAll => {
                // The count is taken before `LIMIT`; the limit only decides how
                // many of the single-row results survive.
                let visible = match limit {
                    None => true,
                    Some((offset, count)) => offset == 0 && count.map_or(true, |count| count > 0),
                };
                let rows = if visible { vec![vec![Cell::Integer(total_rows as i64)]] } else { Vec::new() };
                Ok(SubqueryOutcome { columns: labels, rows })
            }
            SelectItems::Star if unordered => {
                // A single source without `USING` shuffling exposes exactly its
                // visible prefix, so the decoded row is the output row.
                let plain = relation.sources.len() == 1
                    && relation.sources[0].star_hidden.iter().all(|hidden| !*hidden)
                    && relation.sources[0].star_override.iter().all(Option::is_none);
                if plain {
                    let visible = relation.sources[0].visible_width();
                    for row in &mut final_rows {
                        row.truncate(visible);
                    }
                    return Ok(SubqueryOutcome { columns: labels, rows: final_rows });
                }
                Ok(SubqueryOutcome { columns: labels, rows: final_rows.iter().map(|row| relation.visible_row(row)).collect() })
            }
            SelectItems::Star => {
                Ok(SubqueryOutcome { columns: labels, rows: final_rows.iter().map(|row| relation.visible_row(row)).collect() })
            }
            SelectItems::Named(_) => {
                let nodes = projection.expect("named projection is resolved up front");
                let mut out = Vec::with_capacity(final_rows.len());
                for (index, values) in final_rows.iter().enumerate() {
                    let row_ctx = EvalContext {
                        exec: self.clone(),
                        parameters: self.parameters,
                        upsert_excluded: self.upsert_excluded,
                        trigger_frames: self.trigger_frames,
                        outer_read: &self.state.outer_read,
                        subquery_cache: &self.state.subquery_cache,
                        host_functions: self.host_functions,
                        outer,
                        current: &shape,
                        aggregates: &empty_aggregates,
                        windows: &final_windows,
                        window_row: index,
                        budget,
                        ctes,
                    };
                    out.push(nodes.iter().map(|node| node.eval(values, &row_ctx)).collect::<Result<Vec<_>, _>>()?);
                }
                Ok(SubqueryOutcome { columns: labels, rows: out })
            }
        }
    }

    /// Answers a bare `count(*)` over one plain main-schema rowid table without
    /// materializing a row vector.
    ///
    /// The accepted shape is deliberately narrow: no `DISTINCT`, `GROUP BY`,
    /// `HAVING`, window, `ORDER BY`, join, `USING`/`NATURAL`, derived table or
    /// rowid/index scan hint, and the projection must be exactly `count(*)`.
    /// Each stored record is decoded into one reused [`Vec<Cell>`], the `WHERE`
    /// conjuncts run against that buffer, and only the number of SQL-`TRUE`
    /// rows is kept, which is the same row set the generic path counts. Every
    /// other statement returns `None` and keeps the existing execution path.
    pub(crate) fn count_only_scan(
        &self,
        first: &SelectCore,
        order: &[OrderKey],
        limit: Option<(usize, Option<usize>)>,
        outer: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv>,
    ) -> Result<Option<SubqueryOutcome>, SqlError> {
        // `LIMIT 0` needs no count at all: the generic path answers it without
        // reading the table.
        if matches!(limit, Some((_, Some(0)))) {
            return Ok(None);
        }
        if first.distinct || !first.group.is_empty() || first.having.is_some() || !first.windows.is_empty() || !order.is_empty() {
            return Ok(None);
        }
        if first.from.len() != 1 || !is_bare_count_star(&first.items) {
            return Ok(None);
        }
        let entry = &first.from[0];
        if entry.query.is_some() || entry.join.is_some() || entry.on.is_some() || entry.using.is_some() || entry.natural {
            return Ok(None);
        }
        let Some(def) = self.prunable_def(entry)
        else {
            return Ok(None);
        };
        // A rowid or index hint narrows the scan to a handful of rows; a full
        // walk would answer the same count far more slowly, so keep it.
        if self.rowid_pin(&first.from, &first.filter).is_some()
            || self.rowid_in_pin(&first.from, &first.filter).is_some()
            || self.rowid_range_pin(&first.from, &first.filter).is_some()
            || self.rowid_bound_hint(&first.from, &first.filter).is_some()
            || self.index_pin(&first.from, &first.filter).is_some()
        {
            return Ok(None);
        }
        let width = def.columns.len();
        let source = Source {
            label: entry.label().to_string(),
            columns: SourceColumns::Table(def),
            offset: 0,
            join: None,
            star_hidden: vec![false; width],
            unqualified_hidden: vec![false; width],
            star_override: vec![None; width],
        };
        let mut relation = Relation { sources: vec![source], using: vec![Vec::new()], using_slots: Vec::new(), trigger: None, excluded: false };
        relation.plan_joins(&first.from)?;
        let shape = relation.shape();
        let outer_shapes: Vec<ScopeShape> = outer.iter().map(Scope::shape).collect();
        let mut keep = vec![false; width];
        let mut conjuncts = Vec::new();
        if let Some(filter) = &first.filter {
            let mut specs = Vec::new();
            let mut windows = Vec::new();
            let resolved = resolve_expr(filter, &relation, &outer_shapes, &[], &mut windows, &mut specs, false, false)?;
            if !specs.is_empty() || !windows.is_empty() {
                return Ok(None);
            }
            if !predicate_columns(filter, entry.label(), def, &mut keep) {
                // A predicate whose reads cannot be attributed from the raw
                // syntax (a subquery, a window call, a foreign qualifier) keeps
                // the generic path, which owns those semantics.
                return Ok(None);
            }
            split_conjuncts(resolved, &mut conjuncts);
        }
        let empty_aggregates: Vec<Cell> = Vec::new();
        let ctx = EvalContext {
            exec: self.clone(),
            parameters: self.parameters,
            upsert_excluded: self.upsert_excluded,
            trigger_frames: self.trigger_frames,
            outer_read: &self.state.outer_read,
            subquery_cache: &self.state.subquery_cache,
            host_functions: self.host_functions,
            outer,
            current: &shape,
            aggregates: &empty_aggregates,
            windows: &[],
            window_row: 0,
            budget,
            ctes,
        };
        let count = self.count_scan_rows(def, &keep, &conjuncts, &ctx)?;
        let labels = core_output_labels(&relation, &first.items);
        let visible = match limit {
            None => true,
            Some((offset, limit)) => offset == 0 && limit.map_or(true, |limit| limit > 0),
        };
        let rows = if visible { vec![vec![Cell::Integer(count as i64)]] } else { Vec::new() };
        Ok(Some(SubqueryOutcome { columns: labels, rows }))
    }

    /// Counts the rows of one table whose `WHERE` conjuncts are all SQL-`TRUE`.
    ///
    /// Records are decoded into a single reused buffer, so a counting scan
    /// allocates one row vector instead of one per stored row.
    pub(crate) fn count_scan_rows(
        &self,
        def: &TableDef,
        keep: &[bool],
        conjuncts: &[ResolvedExpr],
        ctx: &EvalContext,
    ) -> Result<usize, SqlError> {
        let stored =
            self.writer.table_rows(&def.name, TableLimits::query()).map_err(|error| storage_failure(error).in_statement("select"))?;
        let mut values: Vec<Cell> = Vec::with_capacity(def.columns.len() + 1);
        let mut count = 0usize;
        for row in &stored {
            values.clear();
            decode_row_into(&row.payload, def, row.rowid, Some(keep), &mut values)?;
            let mut matched = true;
            for conjunct in conjuncts {
                if truthiness(&conjunct.eval(&values, ctx)?) != Some(true) {
                    matched = false;
                    break;
                }
            }
            if matched {
                ctx.budget.charge(1)?;
                count += 1;
            }
        }
        Ok(count)
    }

    /// Evaluates the trailing `LIMIT` / `OFFSET` expressions once, before any
    /// row is read, and converts them with SQLite's integer rules.
    pub(crate) fn eval_limit(
        &self,
        limit: Option<&Limit>,
        outer: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv>,
    ) -> Result<Option<(usize, Option<usize>)>, SqlError> {
        let Some(limit) = limit
        else {
            return Ok(None);
        };
        let relation = Relation { sources: Vec::new(), using: Vec::new(), using_slots: Vec::new(), trigger: None, excluded: false };
        let shape = relation.shape();
        let outer_shapes: Vec<ScopeShape> = outer.iter().map(Scope::shape).collect();
        let mut aggregates = Vec::new();
        let mut windows = Vec::new();
        let empty: Vec<Cell> = Vec::new();
        let ctx = EvalContext {
            exec: self.clone(),
            parameters: self.parameters,
            upsert_excluded: self.upsert_excluded,
            trigger_frames: self.trigger_frames,
            outer_read: &self.state.outer_read,
            subquery_cache: &self.state.subquery_cache,
            host_functions: self.host_functions,
            outer,
            current: &shape,
            aggregates: &empty,
            windows: &[],
            window_row: 0,
            budget,
            ctes,
        };
        let mut offset = 0usize;
        if let Some(expr) = &limit.offset {
            let resolved = resolve_expr(expr, &relation, &outer_shapes, &[], &mut windows, &mut aggregates, false, false)?;
            let value = limit_integer(&resolved.eval(&[], &ctx)?, "OFFSET")?;
            offset = value.max(0) as usize;
        }
        let resolved = resolve_expr(&limit.count, &relation, &outer_shapes, &[], &mut windows, &mut aggregates, false, false)?;
        let count = limit_integer(&resolved.eval(&[], &ctx)?, "LIMIT")?;
        let count = if count < 0 { None } else { Some(count as usize) };
        Ok(Some((offset, count)))
    }

    /// Evaluates a compound `SELECT`: every core runs through the same single
    /// core evaluator, then the set operators combine the row sets, and only
    /// then does the trailing `ORDER BY` / `LIMIT` apply.
    pub(crate) fn compound_select(
        &self,
        first: &SelectCore,
        compounds: &[CompoundTerm],
        order: Vec<OrderKey>,
        limit: Option<(usize, Option<usize>)>,
        outer: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv>,
    ) -> Result<SubqueryOutcome, SqlError> {
        let head = self.select(first, &[], Vec::new(), None, outer, budget, ctes)?;
        let columns = head.columns;
        let width = columns.len();
        // SQLite folds compound terms strictly left to right, whatever the
        // operator, so the row set is combined as each core is reached.
        let mut rows = head.rows;
        for term in compounds {
            let right = self.select(&term.core, &[], Vec::new(), None, outer, budget, ctes)?;
            if right.columns.len() != width {
                let mut error = SqlError::new(
                    "arity-mismatch",
                    format!("SELECTs to the left and right of {} do not have the same number of result columns", term.op.text()),
                )
                .in_statement("select");
                error.clause = Some(term.op.clause().to_string());
                error.position = Some(term.position);
                return Err(error);
            }
            rows = combine_rows(term.op, rows, right.rows);
        }
        if !order.is_empty() {
            let keys = compound_order_keys(&order, &columns)?;
            rows.sort_by(|left, right| {
                keys.iter()
                    .find_map(|(index, direction)| {
                        let ordering = direction.compare(&left[*index], &right[*index]);
                        (ordering != Ordering::Equal).then_some(ordering)
                    })
                    .unwrap_or(Ordering::Equal)
            });
        }
        if let Some((offset, count)) = limit {
            let (start, end) = limit_range(rows.len(), Some((offset, count)));
            rows = rows.drain(start..end).collect();
        }
        Ok(SubqueryOutcome { columns, rows })
    }

    /// Evaluates a `SELECT` statement, materializing its `WITH` entries first.
    ///
    /// A CTE that reads its own name is evaluated as a fixed point: the terms
    /// before the first self-reference form the seed, and the remaining terms
    /// run against the previous iteration's rows until the working set is
    /// empty. The `RECURSIVE` keyword is optional, matching SQLite.
    #[allow(clippy::too_many_arguments)]
    pub fn run_select(
        &self,
        with: &[CommonTable],
        recursive: bool,
        first: &SelectCore,
        compounds: &[CompoundTerm],
        order: Vec<OrderKey>,
        limit: Option<Limit>,
        outer: &[Scope],
        budget: &Budget,
        parent: Option<&CteEnv>,
    ) -> Result<SubqueryOutcome, SqlError> {
        if with.is_empty() {
            return self.select(first, compounds, order, limit, outer, budget, parent);
        }
        // Only an actual self-reference makes a `WITH` entry recursive, so the
        // keyword itself carries no extra behavior.
        let _ = recursive;
        let pushdown = self.recursive_limit(first, compounds, &order, limit.as_ref(), outer, budget, parent)?;
        let mut env = CteEnv { parent, tables: BTreeMap::new() };
        for entry in with {
            let bound = pushdown.as_ref().filter(|(name, _)| name.eq_ignore_ascii_case(&entry.name)).map(|(_, bound)| *bound);
            let result = match self.recursive_start(entry)? {
                Some(start) => self.materialize_recursive(entry, start, &env, budget, bound)?,
                None => {
                    let result = self.select_statement(&entry.query, &[], budget, Some(&env))?;
                    if let Some(names) = &entry.columns {
                        if names.len() != result.columns.len() {
                            let mut error = SqlError::new(
                                "arity-mismatch",
                                format!("table {} has {} values for {} columns", entry.name, result.columns.len(), names.len()),
                            )
                            .in_statement("select");
                            error.clause = Some("WITH".to_string());
                            return Err(error);
                        }
                    }
                    let columns = entry.columns.clone().unwrap_or(result.columns);
                    Materialized { columns: unique_output_names(&columns), rows: result.rows }
                }
            };
            env.tables.insert(entry.name.to_ascii_lowercase(), result);
        }
        self.select(first, compounds, order, limit, outer, budget, Some(&env))
    }

    /// Where the recursive part of a CTE begins, or `None` when the entry only
    /// reads earlier tables.
    ///
    /// SQLite splits the compound body at the first term that reads the CTE
    /// name: everything before it is the seed, everything after it is a
    /// recursive term and must also read the name. Mixed operators and a
    /// seed that reads its own CTE are reported as `circular reference`.
    pub fn recursive_start(&self, entry: &CommonTable) -> Result<Option<usize>, SqlError> {
        let Statement::Select { first, compounds, .. } = entry.query.as_ref()
        else {
            return Ok(None);
        };
        let name = entry.name.as_str();
        if core_references(first, name) {
            return Err(circular_reference(name));
        }
        let Some(start) = compounds.iter().position(|term| core_references(&term.core, name))
        else {
            if statement_references(&entry.query, name) {
                return Err(circular_reference(name));
            }
            return Ok(None);
        };
        if compounds[start..].iter().any(|term| !core_references(&term.core, name)) {
            return Err(circular_reference(name));
        }
        let op = compounds[start].op;
        if !matches!(op, CompoundOp::Union | CompoundOp::UnionAll) || compounds.iter().any(|term| term.op != op) {
            return Err(circular_reference(name));
        }
        Ok(Some(start))
    }

    /// Evaluates one recursive CTE to its fixed point.
    ///
    /// `bound` is the largest row count the enclosing query can still consume;
    /// it is only supplied when the outer `LIMIT` is directly over this CTE's
    /// rows, so an infinite recursion can be cut off without changing the
    /// result of the statement.
    pub(crate) fn materialize_recursive(
        &self,
        entry: &CommonTable,
        start: usize,
        env: &CteEnv,
        budget: &Budget,
        bound: Option<usize>,
    ) -> Result<Materialized, SqlError> {
        let Statement::Select { with: inner_with, first, compounds, order, limit, .. } = entry.query.as_ref()
        else {
            return Err(SqlError::new("unsupported-statement", "a `WITH` body must be a `SELECT`").in_statement("select"));
        };
        // Terms before the first self-reference are the initial select; they
        // run once and may not read the CTE.
        let seed_statement = Statement::Select {
            with: inner_with.clone(),
            recursive: false,
            first: first.clone(),
            compounds: compounds[..start].to_vec(),
            order: Vec::new(),
            limit: None,
        };
        let seed = self.select_statement(&seed_statement, &[], budget, Some(env))?;
        if let Some(names) = &entry.columns {
            if names.len() != seed.columns.len() {
                let mut error = SqlError::new(
                    "arity-mismatch",
                    format!("table {} has {} values for {} columns", entry.name, seed.columns.len(), names.len()),
                )
                .in_statement("select");
                error.clause = Some("WITH".to_string());
                return Err(error);
            }
        }
        let columns = entry.columns.clone().unwrap_or(seed.columns);
        let width = columns.len();
        let operator = compounds[start].op;
        // The CTE's own `LIMIT` is also a stopping condition: SQLite cuts an
        // infinite recursion as soon as the body has produced that many rows.
        let own = if order.is_empty() {
            match self.eval_limit(limit.as_ref(), &[], budget, Some(env))? {
                Some((offset, Some(count))) => Some(offset.saturating_add(count)),
                _ => None,
            }
        }
        else {
            None
        };
        let bound = match (bound, own) {
            (Some(outer), Some(own)) => Some(outer.min(own)),
            (outer, own) => outer.or(own),
        };
        // `UNION` deduplicates the whole fixed point, including the seed.
        let mut rows = if matches!(operator, CompoundOp::Union) { distinct_rows(&seed.rows) } else { seed.rows };
        if let Some(bound) = bound {
            rows.truncate(bound);
        }
        let mut seen: Vec<Vec<Cell>> = if matches!(operator, CompoundOp::Union) { rows.clone() } else { Vec::new() };
        let mut working = rows.clone();
        while !working.is_empty() {
            if bound.is_some_and(|limit| rows.len() >= limit) {
                break;
            }
            let mut next: Vec<Vec<Cell>> = Vec::new();
            let mut done = false;
            for term in &compounds[start..] {
                let scope = CteEnv {
                    parent: Some(env),
                    tables: BTreeMap::from([(
                        entry.name.to_ascii_lowercase(),
                        Materialized { columns: columns.clone(), rows: working.clone() },
                    )]),
                };
                let output = self.select(&term.core, &[], Vec::new(), None, &[], budget, Some(&scope))?;
                if output.columns.len() != width {
                    let mut error = SqlError::new(
                        "arity-mismatch",
                        format!("SELECTs to the left and right of {} do not have the same number of result columns", term.op.text()),
                    )
                    .in_statement("select");
                    error.clause = Some(term.op.clause().to_string());
                    error.position = Some(term.position);
                    return Err(error);
                }
                for row in output.rows {
                    if bound.is_some_and(|limit| rows.len() >= limit) {
                        done = true;
                        break;
                    }
                    if matches!(term.op, CompoundOp::Union) {
                        if seen.iter().any(|existing| cells_equal(existing, &row)) {
                            continue;
                        }
                        seen.push(row.clone());
                    }
                    next.push(row.clone());
                    rows.push(row);
                }
                if done {
                    break;
                }
            }
            if done {
                break;
            }
            working = next;
        }
        // A trailing `ORDER BY` / `LIMIT` on the CTE body applies to its whole
        // output, like a compound select outside a `WITH`.
        if !order.is_empty() {
            let keys = compound_order_keys(order, &columns)?;
            rows.sort_by(|left, right| {
                keys.iter()
                    .find_map(|(index, direction)| {
                        let ordering = direction.compare(&left[*index], &right[*index]);
                        (ordering != Ordering::Equal).then_some(ordering)
                    })
                    .unwrap_or(Ordering::Equal)
            });
        }
        let evaluated = self.eval_limit(limit.as_ref(), &[], budget, Some(env))?;
        if evaluated.is_some() {
            let (start, end) = limit_range(rows.len(), evaluated);
            rows = rows.drain(start..end).collect();
        }
        Ok(Materialized { columns, rows })
    }

    /// Computes a safe row bound for a recursive CTE when the enclosing query
    /// is `SELECT <row-wise items> FROM <cte> LIMIT n [OFFSET m]`.
    ///
    /// Materializing a recursive CTE eagerly would never terminate for an
    /// unbounded recursion, while SQLite stops as soon as the `LIMIT` is
    /// satisfied. The bound is only used when every row of the CTE produces at
    /// most one output row and nothing else reads the CTE, so truncating the
    /// fixed point at `offset + count` rows cannot change the result.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn recursive_limit(
        &self,
        first: &SelectCore,
        compounds: &[CompoundTerm],
        order: &[OrderKey],
        limit: Option<&Limit>,
        outer: &[Scope],
        budget: &Budget,
        parent: Option<&CteEnv>,
    ) -> Result<Option<(String, usize)>, SqlError> {
        if !compounds.is_empty()
            || !order.is_empty()
            || limit.is_none()
            || !first.group.is_empty()
            || first.having.is_some()
            || first.filter.is_some()
            || !first.windows.is_empty()
            || !row_preserving_items(&first.items)
            || first.from.len() != 1
        {
            return Ok(None);
        }
        let source = &first.from[0];
        if source.query.is_some() || source.join.is_some() || source.using.is_some() || source.natural || source.name.is_empty() {
            return Ok(None);
        }
        let Some((offset, Some(count))) = self.eval_limit(limit, outer, budget, parent)?
        else {
            return Ok(None);
        };
        Ok(Some((source.name.to_ascii_lowercase(), offset.saturating_add(count))))
    }

    /// 相关 `EXISTS` 的索引探针快速路径。
    ///
    /// 返回 `Some(answer)` 表示已用一次索引探针回答存在性,`None` 表示形状
    /// 不适用或探针无法精确回答,调用方继续走通用子查询执行。计划按语句内
    /// AST 地址缓存,同一语句的每个外层行只付一次探针与候选行解码。
    pub(crate) fn correlated_exists(&self, query: &Statement, values: &[Cell], ctx: &EvalContext) -> Result<Option<bool>, SqlError> {
        let key = query as *const Statement as usize;
        if !self.state.exists_probes.borrow().contains_key(&key) {
            let plan = self.build_exists_probe(query, ctx);
            self.state.exists_probes.borrow_mut().insert(key, plan);
        }
        let probes = self.state.exists_probes.borrow();
        let Some(plan) = probes.get(&key).and_then(Option::as_ref)
        else {
            return Ok(None);
        };
        ctx.budget.charge(1)?;
        let Some(outer) = values.get(plan.outer)
        else {
            return Ok(None);
        };
        let def = &self.catalog.tables[plan.table_position];
        let affinity = affinity_of(&def.columns[plan.slot].declared_type);
        let cell = match (affinity, outer) {
            (Affinity::Integer, Cell::Real(real)) => match integral_rowid(*real) {
                Some(value) => Cell::Integer(value),
                None => return Ok(None),
            },
            _ => outer.clone(),
        };
        match (&affinity, &cell) {
            (Affinity::Text, Cell::Text(_)) | (Affinity::Real, Cell::Real(_)) | (Affinity::Integer, Cell::Integer(_)) => {}
            _ => return Ok(None),
        }
        let Some(probe) = probe_key(&cell, plan.probe_collation)
        else {
            return Ok(None);
        };
        if matches!(probe, ProbeKey::Wide(_)) {
            return Ok(None);
        }
        let Some(index) = self.catalog.indexes.get(&plan.index)
        else {
            return Ok(None);
        };
        let Some(rowids) = self.index_probe_rowids(index, &[probe])
        else {
            return Ok(None);
        };
        for rowid in rowids {
            let Some(payload) = self.writer.row_payload(&plan.table, rowid).map_err(|error| storage_failure(error).in_statement("select"))?
            else {
                continue;
            };
            let stored = [TableRow { rowid, payload }];
            let decoded = decode_stored_rows(&stored, def, None)?;
            let row = &decoded[0];
            // 探针只保证候选是超集（`NOCASE` / `RTRIM` 会合并等价类），
            // 等值本身仍按列排序规则复核一次。
            if compare_cells_with(&row[plan.slot], &cell, plan.compare_collation) != Ordering::Equal {
                continue;
            }
            let mut matched = true;
            if !plan.residual.is_empty() {
                let eval = EvalContext {
                    exec: self.clone(),
                    parameters: self.parameters,
                    upsert_excluded: self.upsert_excluded,
                    trigger_frames: self.trigger_frames,
                    outer_read: &self.state.outer_read,
                    subquery_cache: &self.state.subquery_cache,
                    host_functions: self.host_functions,
                    outer: &[],
                    current: &plan.shape,
                    aggregates: &[],
                    windows: &[],
                    window_row: 0,
                    budget: ctx.budget,
                    ctes: ctx.ctes,
                };
                for conjunct in &plan.residual {
                    if truthiness(&conjunct.eval(row, &eval)?) != Some(true) {
                        matched = false;
                        break;
                    }
                }
            }
            if matched {
                return Ok(Some(true));
            }
        }
        Ok(Some(false))
    }

    /// 构建 [`Self::correlated_exists`] 的计划；形状不适用时返回 `None`。
    pub(crate) fn build_exists_probe(&self, query: &Statement, ctx: &EvalContext) -> Option<ExistsProbePlan> {
        let Statement::Select { with, recursive: false, first, compounds, order, limit } = query
        else {
            return None;
        };
        if !with.is_empty() || !compounds.is_empty() || !order.is_empty() || limit.is_some() {
            return None;
        }
        if first.distinct || !first.group.is_empty() || first.having.is_some() || !first.windows.is_empty() {
            return None;
        }
        // `EXISTS (SELECT count(*) ...)` 恒为真，聚合与窗口必须排除；投影只
        // 接受不会在求值时报错的常量或裸列，存在性与投影内容无关。
        match &first.items {
            SelectItems::CountAll => return None,
            SelectItems::Named(exprs)
                if !exprs.iter().all(|expr| match expr {
                    Expr::Literal(_) | Expr::Column(_) => true,
                    Expr::Alias { value, .. } => matches!(value.as_ref(), Expr::Literal(_) | Expr::Column(_)),
                    _ => false,
                }) =>
            {
                return None;
            }
            _ => {}
        }
        if first.from.len() != 1 {
            return None;
        }
        let entry = &first.from[0];
        if entry.query.is_some() || entry.join.is_some() || entry.on.is_some() || entry.using.is_some() || entry.natural {
            return None;
        }
        let def = self.prunable_def(entry)?;
        let position = *self.catalog.table_lookup.get(&entry.name.to_ascii_lowercase())?;
        let filter = first.filter.as_ref()?;
        let width = def.columns.len();
        let source = Source {
            label: entry.label().to_string(),
            columns: SourceColumns::Table(def),
            offset: 0,
            join: None,
            star_hidden: vec![false; width],
            unqualified_hidden: vec![false; width],
            star_override: vec![None; width],
        };
        let relation = Relation { sources: vec![source], using: vec![Vec::new()], using_slots: Vec::new(), trigger: None, excluded: false };
        let shape = relation.shape();
        let mut outer_shapes: Vec<ScopeShape> = Vec::with_capacity(ctx.outer.len() + 1);
        outer_shapes.push(ctx.current.clone());
        outer_shapes.extend(ctx.outer.iter().map(Scope::shape));
        let mut windows = Vec::new();
        let mut specs = Vec::new();
        let resolved = match resolve_expr(filter, &relation, &outer_shapes, &[], &mut windows, &mut specs, false, false) {
            Ok(resolved) => resolved,
            Err(_) => return None,
        };
        if !windows.is_empty() || !specs.is_empty() {
            return None;
        }
        let mut conjuncts = Vec::new();
        split_conjuncts(resolved, &mut conjuncts);
        let mut equality: Option<(usize, usize, usize, Collation)> = None;
        for (at, conjunct) in conjuncts.iter().enumerate() {
            let ResolvedExpr::Compare { left, op: CompareOp::Eq, right, collation } = conjunct
            else {
                continue;
            };
            let pair = match (left.as_ref(), right.as_ref()) {
                (ResolvedExpr::Column { source: 0, index: slot }, ResolvedExpr::OuterColumn { depth: 0, index: outer }) => {
                    Some((*slot, *outer))
                }
                (ResolvedExpr::OuterColumn { depth: 0, index: outer }, ResolvedExpr::Column { source: 0, index: slot }) => {
                    Some((*slot, *outer))
                }
                _ => None,
            };
            if let Some((slot, outer)) = pair {
                equality = Some((at, slot, outer, *collation));
                break;
            }
        }
        let Some((at, slot, outer, compare_collation)) = equality
        else {
            return None;
        };
        let table_key = def.name.to_ascii_lowercase();
        let Some(index) =
            self.catalog.indexes.values().find(|index| !index.managed && index.table == table_key && index.columns.as_slice() == [slot])
        else {
            return None;
        };
        let index_name = index.name.to_ascii_lowercase();
        let probe_collation = index.collations.first().copied().unwrap_or_default();
        let mut residual = Vec::with_capacity(conjuncts.len() - 1);
        for (position, conjunct) in conjuncts.into_iter().enumerate() {
            if position == at {
                continue;
            }
            // 残差必须只读内层列:读取外层或再跑子查询都会把快速路径重新拖回
            // 通用执行,也破坏“按 AST 缓存一次”的前提。
            if !conjunct.is_inner_only() {
                return None;
            }
            residual.push(conjunct);
        }
        Some(ExistsProbePlan {
            table: def.name.clone(),
            table_position: position,
            index: index_name,
            slot,
            outer,
            probe_collation,
            compare_collation,
            residual,
            shape,
        })
    }

    /// Detects `ORDER BY <rowid order> LIMIT n` over a single base rowid table
    /// with nothing else that could reorder or drop rows: no `WHERE`, no
    /// `DISTINCT`, no aggregate, group or window, no `count(*)`, no `DESC` and
    /// no `NULLS` override. Returns the number of leading rows the scan has to
    /// read (`offset + count`).
    ///
    /// A table scan already emits rowid order, which is exactly what an
    /// ascending rowid sort produces, so reading only the prefix and letting
    /// the generic sort run over it keeps the rows and their order identical.
    /// Projection items must stay bare column references (or `*`): an aliased
    /// projection could make an `ORDER BY` name resolve to a different
    /// expression than the rowid column the scan order is based on.
    pub(crate) fn rowid_head_bound(&self, first: &SelectCore, order: &[OrderKey], limit: Option<(usize, Option<usize>)>) -> Option<usize> {
        if order.is_empty()
            || first.filter.is_some()
            || first.distinct
            || !first.group.is_empty()
            || first.having.is_some()
            || !first.windows.is_empty()
            || matches!(first.items, SelectItems::CountAll)
        {
            return None;
        }
        if !matches!(first.items, SelectItems::Star)
            && !matches!(&first.items, SelectItems::Named(exprs) if exprs.iter().all(|expr| matches!(expr, Expr::Column(_))))
        {
            return None;
        }
        let Some((offset, Some(count))) = limit
        else {
            return None;
        };
        let (def, table) = self.bounded_scan_target(&first.from)?;
        let alias_name = def.rowid_alias.map(|index| def.columns[index].name.clone());
        for entry in order {
            if entry.descending || entry.nulls_first.is_some() {
                return None;
            }
            let Expr::Column(column) = &entry.expr
            else {
                return None;
            };
            if column.qualifier.as_deref().is_some_and(|qualifier| !qualifier.eq_ignore_ascii_case(table.label())) {
                return None;
            }
            let rowid_named = is_rowid_alias(&column.name);
            let alias_named = alias_name.as_deref().is_some_and(|name| name.eq_ignore_ascii_case(&column.name));
            if !rowid_named && !alias_named {
                return None;
            }
        }
        Some(offset.saturating_add(count))
    }

    /// 尝试为一个简单单表扫描建立惰性游标。
    ///
    /// 只有同时满足下列条件的 `SELECT` 才返回 `Some`:
    ///
    /// - 单表、无 `JOIN`、无派生表/子查询、无 CTE 与复合查询;
    /// - 无 `DISTINCT`、`GROUP BY`、`HAVING`、窗口与聚合;
    /// - 无 `LIMIT`/`OFFSET`;
    /// - 无 `ORDER BY`,或排序键就是主键/rowid 升序,与 b-tree 扫描顺序一致;
    /// - 投影是 `*` 或普通列引用/常量表达式,`WHERE` 可逐行求值;
    /// - `WHERE` 未命中索引等值/范围下推(命中时物化路径按索引候选行读取)。
    ///
    /// 打开时把当前整页镜像克隆成独立快照,后续写入只改引擎自己的页缓冲,
    /// 游标因此始终读到打开时刻的一致状态。返回 `None` 表示语句不满足条件,
    /// 调用方走既有的物化路径。
    pub fn open_lazy_scan(&self, plan: LazyPlan, parameters: &[Cell]) -> Result<Option<LazyCursor>, SqlError> {
        let objects = match self.writer.schema_objects_with_rowids(TableLimits::query()) {
            Ok(objects) => objects,
            Err(_) => return Ok(None),
        };
        let root_page = objects
            .iter()
            .find(|(_, object)| object.kind == SchemaObjectKind::Table && object.name.eq_ignore_ascii_case(&plan.def.name))
            .and_then(|(_, object)| object.root_page)
            .filter(|root| *root != 0 && *root != 1);
        let Some(root_page) = root_page
        else {
            return Ok(None);
        };
        let snapshot = Arc::new(self.writer.pages().to_vec());
        let scan = LazyTableScan::new(snapshot, root_page)?;
        Ok(Some(LazyCursor { plan, scan, budget: Budget::new(), parameters: parameters.to_vec(), finished: false }))
    }

    /// 从惰性游标读取至多 `limit` 行,保持 b-tree 扫描顺序。
    pub fn lazy_cursor_next_inner(&self, cursor: &mut LazyCursor, limit: usize) -> Result<LazyBatch, SqlError> {
        let shape = ScopeShape { columns: cursor.plan.shape_columns.clone() };
        let empty_aggregates: Vec<Cell> = Vec::new();
        let mut rows = Vec::new();
        while rows.len() < limit {
            let stored = cursor.scan.next_rows(limit - rows.len())?;
            if stored.is_empty() {
                cursor.finished = true;
                break;
            }
            let decoded = decode_stored_rows(&stored, &cursor.plan.def, cursor.plan.mask.as_deref())?;
            for values in decoded {
                let ctx = EvalContext {
                    exec: self.clone(),
                    parameters: self.parameters,
                    upsert_excluded: self.upsert_excluded,
                    trigger_frames: self.trigger_frames,
                    outer_read: &self.state.outer_read,
                    subquery_cache: &self.state.subquery_cache,
                    host_functions: self.host_functions,
                    outer: &[],
                    current: &shape,
                    aggregates: &empty_aggregates,
                    windows: &[],
                    window_row: 0,
                    budget: &cursor.budget,
                    ctes: None,
                };
                if let Some(predicate) = &cursor.plan.predicate
                    && truthiness(&predicate.eval(&values, &ctx)?) != Some(true)
                {
                    continue;
                }
                cursor.budget.charge(1)?;
                let out = match &cursor.plan.projection {
                    Some(nodes) => nodes.iter().map(|node| node.eval(&values, &ctx)).collect::<Result<Vec<_>, _>>()?,
                    None => values[..cursor.plan.visible_width].to_vec(),
                };
                rows.push(out);
                if rows.len() >= limit {
                    break;
                }
            }
        }
        Ok(LazyBatch { rows, done: cursor.finished })
    }

    /// 判定语句能否走惰性扫描,能把结果表达式解析成 owned 的执行计划。
    pub fn plan_lazy_scan(&self, statement: &Statement) -> Result<Option<LazyPlan>, SqlError> {
        let Statement::Select { with, first, compounds, order, limit, .. } = statement
        else {
            return Ok(None);
        };
        if !with.is_empty() || !compounds.is_empty() || limit.is_some() || first.from.len() != 1 {
            return Ok(None);
        }
        let entry = &first.from[0];
        if entry.query.is_some() || entry.join.is_some() || entry.on.is_some() || entry.using.is_some() || entry.natural {
            return Ok(None);
        }
        if first.distinct || !first.group.is_empty() || first.having.is_some() || !first.windows.is_empty() {
            return Ok(None);
        }
        if matches!(first.items, SelectItems::CountAll) {
            return Ok(None);
        }
        let Some(def) = self.prunable_def(entry)
        else {
            return Ok(None);
        };
        // 子查询与窗口会读到本行之外的上下文,不能按批惰性求值。
        let mut refs = Vec::new();
        if let SelectItems::Named(exprs) = &first.items {
            for expr in exprs {
                if !collect_unmasked_columns(expr, &mut refs) {
                    return Ok(None);
                }
            }
        }
        if let Some(filter) = &first.filter
            && !collect_unmasked_columns(filter, &mut refs)
        {
            return Ok(None);
        }
        // 索引等值 / 范围下推会把整表扫描收窄成索引候选行,而惰性游标只会
        // 按 b-tree 页顺序扫全表。两者不能同时成立:命中下推时让位给物化
        // 路径,EXPLAIN QUERY PLAN 报告的 SEARCH 才是执行层真实走的路。
        if self.index_pin(&first.from, &first.filter).is_some() {
            return Ok(None);
        }
        if !lazy_order_matches_scan(def, entry, &first.items, order) {
            return Ok(None);
        }
        let source = Source {
            label: entry.label().to_string(),
            columns: SourceColumns::Table(def),
            offset: 0,
            join: None,
            star_hidden: vec![false; def.columns.len()],
            unqualified_hidden: vec![false; def.columns.len()],
            star_override: vec![None; def.columns.len()],
        };
        let mut relation = Relation { sources: vec![source], using: vec![Vec::new()], using_slots: Vec::new(), trigger: None, excluded: false };
        relation.plan_joins(&first.from)?;
        let shape = relation.shape();
        let outer_shapes: Vec<ScopeShape> = Vec::new();
        let mut specs = Vec::new();
        let mut windows = Vec::new();
        let projection = match &first.items {
            SelectItems::Named(exprs) => Some(
                exprs
                    .iter()
                    .map(|expr| resolve_expr(expr, &relation, &outer_shapes, &first.windows, &mut windows, &mut specs, true, true))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            SelectItems::Star => None,
            SelectItems::CountAll => unreachable!("count(*) is rejected above"),
        };
        // 聚合与窗口要么读到整组行,要么依赖排序后的位置,只能走物化路径。
        if !specs.is_empty() || !windows.is_empty() {
            return Ok(None);
        }
        let predicate = match &first.filter {
            Some(expr) => {
                let mut scratch_specs = Vec::new();
                let mut scratch_windows = Vec::new();
                Some(resolve_expr(expr, &relation, &outer_shapes, &[], &mut scratch_windows, &mut scratch_specs, false, false)?)
            }
            None => None,
        };
        let labels = core_output_labels(&relation, &first.items);
        let mask = self.read_column_masks(first, order).and_then(|masks| masks.into_iter().next().flatten());
        Ok(Some(LazyPlan {
            def: def.clone(),
            labels,
            visible_width: def.columns.len(),
            projection,
            predicate,
            shape_columns: shape.columns,
            mask,
        }))
    }

    /// Per-`FROM` column masks for the projected read, or `None` when the shape
    /// is not safe to prune.
    ///
    /// A mask marks the declared columns the statement can observe. Positions
    /// outside the mask decode as [`Cell::Null`] and are never read, which keeps
    /// text and blob cells out of memory for `count(*)`, single-column
    /// projections and aggregates. Only main-schema rowid tables qualify; CTEs,
    /// views, derived tables, WITHOUT ROWID tables and attached schemas keep
    /// their full decode, and `USING` / `NATURAL` joins read coalesced slots
    /// that the raw expressions never mention.
    pub fn read_column_masks(&self, first: &SelectCore, order: &[OrderKey]) -> Option<Vec<Option<Vec<bool>>>> {
        if first.from.is_empty()
            || matches!(first.items, SelectItems::Star)
            || !first.windows.is_empty()
            || first.from.iter().any(|entry| entry.using.is_some() || entry.natural)
        {
            return None;
        }
        let defs: Vec<Option<&TableDef>> = first.from.iter().map(|entry| self.prunable_def(entry)).collect();
        if defs.iter().all(Option::is_none) {
            return None;
        }
        let mut masks: Vec<Option<Vec<bool>>> = defs.iter().map(|def| def.map(|def| vec![false; def.columns.len()])).collect();
        let mut refs: Vec<&ColumnRef> = Vec::new();
        let collected = match &first.items {
            SelectItems::Named(items) => items.iter().all(|expr| collect_unmasked_columns(expr, &mut refs)),
            _ => true,
        } && first.filter.as_ref().is_none_or(|expr| collect_unmasked_columns(expr, &mut refs))
            && first.group.iter().all(|expr| collect_unmasked_columns(expr, &mut refs))
            && first.having.as_ref().is_none_or(|expr| collect_unmasked_columns(expr, &mut refs))
            && order.iter().all(|key| collect_unmasked_columns(&key.expr, &mut refs))
            && first.from.iter().all(|entry| entry.on.as_ref().is_none_or(|expr| collect_unmasked_columns(expr, &mut refs)));
        if !collected {
            return None;
        }
        for column in refs {
            let matched: Vec<usize> = match column.qualifier.as_deref() {
                Some(qualifier) => (0..first.from.len()).filter(|index| first.from[*index].label().eq_ignore_ascii_case(qualifier)).collect(),
                None => {
                    // An unqualified name over several sources cannot be
                    // attributed without the resolver, so keep every column.
                    if first.from.len() > 1 {
                        return None;
                    }
                    vec![0]
                }
            };
            for index in matched {
                let Some(mask) = masks[index].as_mut()
                else {
                    continue;
                };
                let def = defs[index].expect("a mask only exists for a prunable table");
                match def.columns.iter().position(|column_def| column_def.name.eq_ignore_ascii_case(&column.name)) {
                    Some(position) => mask[position] = true,
                    // The implicit rowid slots stay available whatever the mask.
                    None if is_rowid_alias(&column.name) => {}
                    None => masks[index] = None,
                }
            }
        }
        Some(masks)
    }

    /// The definition of a plain main-schema rowid table, or `None` when the
    /// `FROM` entry can resolve to a view, CTE, temp table or WITHOUT ROWID
    /// table whose rows are materialized by another path.
    pub(crate) fn prunable_def(&self, entry: &TableRef) -> Option<&TableDef> {
        if entry.query.is_some() {
            return None;
        }
        let main = match entry.schema.as_deref() {
            None => true,
            Some(qualifier) => {
                qualifier.eq_ignore_ascii_case("main") || self.alias.as_deref().is_some_and(|alias| alias.eq_ignore_ascii_case(qualifier))
            }
        };
        if !main {
            return None;
        }
        let key = entry.name.to_ascii_lowercase();
        // Views and temp tables shadow the main table in the resolver, so any
        // lookup they could win keeps the full decode.
        if self.catalog.views.contains_key(&key) || self.catalog.temp_views.contains_key(&key) || self.catalog.temp_lookup.contains_key(&key) {
            return None;
        }
        let position = self.catalog.table_lookup.get(&key).copied()?;
        let def = &self.catalog.tables[position];
        if def.without_rowid { None } else { Some(def) }
    }

    /// 按候选 rowid 读取行,保持升序。
    ///
    /// 候选稠密时用一次 `[first, last]` 范围读后按集合过滤,避免对每个 rowid
    /// 都重走一次表元数据与 b-tree 定位。候选含重复或跨度过大时退回逐点读,
    /// 两条路径都按 rowid 升序返回,与全表扫描顺序一致。
    pub fn rows_for_rowids(&self, table: &str, rowids: &[i64]) -> Result<Vec<TableRow>, SqlError> {
        let ascending = rowids.windows(2).all(|pair| pair[0] < pair[1]);
        let dense = rowids.len() >= 2
            && ascending
            && rowids
                .last()
                .and_then(|last| last.checked_sub(rowids[0]))
                .is_some_and(|span| span <= (rowids.len() as i64).saturating_mul(4).saturating_add(16));
        if dense {
            let stored = self
                .writer
                .table_range(table, rowids[0], rowids[rowids.len() - 1], TableLimits::query())
                .map_err(|error| storage_failure(error).in_statement("select"))?;
            let wanted: HashSet<i64> = rowids.iter().copied().collect();
            return Ok(stored.into_iter().filter(|row| wanted.contains(&row.rowid)).collect());
        }
        let mut stored = Vec::with_capacity(rowids.len());
        for rowid in rowids {
            // 单行候选走写入器的 rowid 下钻，避免为一行重建整个区间读取上下文。
            if let Some(payload) = self.writer.row_payload(table, *rowid).map_err(|error| storage_failure(error).in_statement("select"))? {
                stored.push(TableRow { rowid: *rowid, payload });
            }
        }
        Ok(stored)
    }

    /// Runs a nested `SELECT` statement through the same entry point as a
    /// top-level one, so its own `WITH` entries work as well.
    /// Resolves one `FROM` entry against the schema it names, if any. An
    /// unqualified name follows SQLite's order: temp, main, then attached
    /// databases; `main.` and `temp.` pin the search to one schema.
    pub fn select_source(
        &self,
        table: &TableRef,
        budget: &Budget,
        ctes: Option<&CteEnv>,
        scan: Option<&ScanHint>,
        keep: Option<&[bool]>,
    ) -> Result<(SourceColumns<'e>, Rc<Vec<Vec<Cell>>>), SqlError> {
        if let Some(query) = &table.query {
            let result = self.select_statement(query, &[], budget, ctes)?;
            let names = unique_output_names(&result.columns);
            return Ok((SourceColumns::Materialized { names, rowid: false }, Rc::new(result.rows)));
        }
        let scope = match table.schema.as_deref() {
            None => Some(TableScope::Default),
            Some(qualifier) => {
                let lower = qualifier.to_ascii_lowercase();
                if lower == "main" {
                    Some(TableScope::Main)
                }
                else if lower == "temp" {
                    Some(TableScope::Temp)
                }
                else if self.alias.as_deref().is_some_and(|alias| alias.eq_ignore_ascii_case(qualifier)) {
                    Some(TableScope::Default)
                }
                else {
                    None
                }
            }
        };
        let Some(scope) = scope
        else {
            let qualifier = table.schema.as_deref().unwrap_or_default();
            let index = match self.attached_lookup.get(&qualifier.to_ascii_lowercase()).copied() {
                Some(index) => index,
                None => {
                    return Err(SqlError::new("unknown-table", format!("no such table: {qualifier}.{}", table.name)).in_statement("select"));
                }
            };
            return self.attached_source(index, qualifier, table, budget);
        };
        if table.schema.is_none() {
            if let Some(materialized) = ctes.and_then(|env| env.lookup(&table.name)) {
                return Ok((
                    SourceColumns::Materialized { names: materialized.columns.clone(), rowid: false },
                    Rc::new(materialized.rows.clone()),
                ));
            }
        }
        if let Some(system) = system_for_scope(scope, &table.name) {
            let (columns, rows, rowid) = self.system_rows(system, &table.name)?;
            return Ok((SourceColumns::Materialized { names: columns, rowid }, Rc::new(rows)));
        }
        let key = table.name.to_ascii_lowercase();
        if scope != TableScope::Main {
            if let Some(view) = self.catalog.temp_views.get(&key) {
                let _depth = ViewDepthGuard::enter()?;
                let (names, rows) = self.expand_view(&view.def, budget)?;
                return Ok((SourceColumns::Materialized { names, rowid: false }, Rc::new(rows)));
            }
        }
        if scope != TableScope::Temp {
            if let Some(view) = self.catalog.views.get(&key) {
                // A view re-runs its stored query against the current contents
                // of every base table, exactly like SQLite. Views are not
                // correlated, so they see no outer row or CTE from this query.
                let _depth = ViewDepthGuard::enter()?;
                let (names, rows) = self.expand_view(view, budget)?;
                return Ok((SourceColumns::Materialized { names, rowid: false }, Rc::new(rows)));
            }
        }
        if scope != TableScope::Main {
            if let Some(position) = self.catalog.temp_lookup.get(&key).copied() {
                let def = &self.catalog.temp_tables[position].def;
                let decoded = self.catalog.temp_tables[position]
                    .rows
                    .iter()
                    .map(|(rowid, cells)| {
                        let mut cells = cells.clone();
                        cells.push(Cell::Integer(*rowid));
                        cells
                    })
                    .collect::<Vec<Vec<Cell>>>();
                return Ok((SourceColumns::Table(def), Rc::new(decoded)));
            }
        }
        let position = if scope == TableScope::Temp { None } else { self.catalog.table_lookup.get(&key).copied() };
        if let Some(position) = position {
            let def = &self.catalog.tables[position];
            // `LIMIT 0` over this table: no row can reach the output, so the
            // b-tree is never walked. The column names still come from `def`.
            if matches!(scan, Some(ScanHint::Empty)) {
                return Ok((SourceColumns::Table(def), Rc::new(Vec::new())));
            }
            if def.without_rowid {
                let stored = self
                    .writer
                    .without_rowid_rows(&def.name, TableLimits::query())
                    .map_err(|error| storage_failure(error).in_statement("select"))?;
                let decoded = stored.iter().map(|payload| def.decode_without_rowid(payload)).collect::<Result<Vec<Vec<Cell>>, SqlError>>()?;
                return Ok((SourceColumns::Table(def), Rc::new(decoded)));
            }
            // 无扫描提示的普通全表扫描在本语句内只解码一次。自连接和内外层
            // 同表子查询因此共享同一份行集,结果与逐次解码完全一致。缓存按表名
            // 保存该份行集覆盖的列掩码:新请求的列全被覆盖时直接复用,不重复
            // 解码;新请求更宽时用更宽的那份替换(掩码无关列在行内是 NULL 占位)。
            if scan.is_none() {
                let key = def.name.to_ascii_lowercase();
                {
                    let scans = self.state.statement_scans.borrow();
                    // 缓存持弱引用:同一行集若已没有使用方存活,就重新解码;仍有
                    // 使用方(自连接、内外层同表子查询)时正常复用。
                    if let Some((cached_mask, cached)) = scans.get(&key)
                        && let Some(cached) = cached.upgrade()
                        && mask_covers(cached_mask.as_deref(), keep)
                    {
                        return Ok((SourceColumns::Table(def), cached));
                    }
                }
                let stored =
                    self.writer.table_rows(&def.name, TableLimits::query()).map_err(|error| storage_failure(error).in_statement("select"))?;
                let decoded = Rc::new(decode_stored_rows(&stored, def, keep)?);
                let wider = {
                    let scans = self.state.statement_scans.borrow();
                    match scans.get(&key) {
                        None => true,
                        Some((cached_mask, _)) => cached_mask.as_deref() != keep && mask_covers(keep, cached_mask.as_deref()),
                    }
                };
                if wider {
                    self.state.statement_scans.borrow_mut().insert(key, (keep.map(<[bool]>::to_vec), Rc::downgrade(&decoded)));
                }
                return Ok((SourceColumns::Table(def), decoded));
            }
            let stored = match scan {
                // A `WHERE <rowid alias> = <integer literal>` on this very base
                // rowid table: bound the scan to the one candidate row instead
                // of decoding the whole table. The full predicate still runs in
                // `cross_product`, so the emitted row set and order are exactly
                // the unpushed path's; a miss yields no rows. The writer's own
                // rowid descent reads one cell instead of walking the reader's
                // whole checked-range path.
                Some(ScanHint::Pin(rowid)) => self
                    .writer
                    .row_payload(&def.name, *rowid)
                    .map_err(|error| storage_failure(error).in_statement("select"))?
                    .map(|payload| TableRow { rowid: *rowid, payload })
                    .into_iter()
                    .collect(),
                // A `WHERE <indexed column> = <literal>` covering every key
                // column of a plain index: fetch just the probe's rowids. They
                // arrive ascending, which is both the index order under one
                // full-key equality and the order of the full scan, so the
                // emitted rows keep their position.
                Some(ScanHint::Rowids(rowids)) => self.rows_for_rowids(&def.name, rowids)?,
                // `ORDER BY <rowid order> LIMIT n`: read only the leading rows;
                // the scan order already satisfies the sort, and the generic
                // sort downstream just confirms it over this small prefix.
                Some(ScanHint::Head(max_rows)) => self
                    .writer
                    .table_head(&def.name, *max_rows, TableLimits::query())
                    .map_err(|error| storage_failure(error).in_statement("select"))?,
                // `WHERE <rowid> >= a` / `<= b`: read the interval that the
                // predicate can still keep; the missing side stays unbounded.
                Some(ScanHint::Range { lower, upper }) => self
                    .writer
                    .table_range(&def.name, lower.unwrap_or(i64::MIN), upper.unwrap_or(i64::MAX), TableLimits::query())
                    .map_err(|error| storage_failure(error).in_statement("select"))?,
                // Handled before the branch: the b-tree is never read.
                Some(ScanHint::Empty) => Vec::new(),
                None => unreachable!("plain table scans are cached above"),
            };
            return Ok((SourceColumns::Table(def), Rc::new(decode_stored_rows(&stored, def, keep)?)));
        }
        if table.schema.is_none() {
            // The attached databases come after temp and main in the search
            // order, and the first one that defines the name wins.
            for index in 0..self.attached.len() {
                if let Ok(source) = self.attached_source(index, "", table, budget) {
                    return Ok(source);
                }
            }
        }
        let message = match table.schema.as_deref() {
            None => format!("unknown table `{}`", table.name),
            Some(qualifier) => format!("no such table: {qualifier}.{}", table.name),
        };
        Err(SqlError::new("unknown-table", message).in_statement("select"))
    }

    /// Reads one table, view or system table of an attached database.
    pub(crate) fn attached_source(
        &self,
        index: usize,
        qualifier: &str,
        table: &TableRef,
        budget: &Budget,
    ) -> Result<(SourceColumns<'e>, Rc<Vec<Vec<Cell>>>), SqlError> {
        let engine = &self.attached[index];
        if let Some(system) = SystemTable::from_name(&table.name) {
            let (columns, rows, rowid) = engine.system_rows(system, &table.name)?;
            return Ok((SourceColumns::Materialized { names: columns, rowid }, Rc::new(rows)));
        }
        let key = table.name.to_ascii_lowercase();
        if let Some(view) = engine.catalog.temp_views.get(&key) {
            let _depth = ViewDepthGuard::enter()?;
            let (names, rows) = engine.expand_view(&view.def, budget)?;
            return Ok((SourceColumns::Materialized { names, rowid: false }, Rc::new(rows)));
        }
        if let Some(view) = engine.catalog.views.get(&key) {
            let _depth = ViewDepthGuard::enter()?;
            let (names, rows) = engine.expand_view(view, budget)?;
            return Ok((SourceColumns::Materialized { names, rowid: false }, Rc::new(rows)));
        }
        if let Some(position) = engine.catalog.table_lookup.get(&key).copied() {
            let def = &engine.catalog.tables[position];
            let stored =
                engine.writer.table_rows(&def.name, TableLimits::query()).map_err(|error| storage_failure(error).in_statement("select"))?;
            return Ok((SourceColumns::Table(def), Rc::new(decode_stored_rows(&stored, def, None)?)));
        }
        Err(SqlError::new("unknown-table", format!("no such table: {qualifier}.{}", table.name)).in_statement("select"))
    }

    /// Runs one nested statement with `outer` as the enclosing scope chain.
    pub fn select_statement(
        &self,
        statement: &Statement,
        outer: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv>,
    ) -> Result<SubqueryOutcome, SqlError> {
        match statement {
            Statement::Select { with, recursive, first, compounds, order, limit } => {
                self.run_select(with, *recursive, first, compounds, order.clone(), limit.clone(), outer, budget, ctes)
            }
            other => Err(SqlError::new("unsupported-statement", format!("a subquery must be a `SELECT` but found `{}`", other.name()))
                .in_statement("select")),
        }
    }
}
