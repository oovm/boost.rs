//! In-memory execution layer: SQL subset over the pure-Rust format-3 writer.
//!
//! The engine keeps the whole database image in `SqliteWriter::pages()`. It
//! never imports an upstream SQLite engine; [`Engine::from_writer`] wraps an
//! image read by `SqliteWriter::open`, and [`Engine::save`] flushes the current
//! image back to a file.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use database_udf::{Budget as UdfBudget, HostMicroDefinition, HostRuntimeAdapter, UdfContext, UdfImplementation};

use crate::{
    Affinity, Collation, ColumnSpec, DropIndexOutcome, RecordLimits, SchemaObjectKind, SqliteWriter, TableLimits, TableRow, TextEncoding,
    UpdateValue, affinity_of, decode_library_version, decode_record, decode_varint, read_index_table_with, read_table, validate_database,
};

use super::{
    AggregateFunction, AlterAction, Assignment, BinaryOp, CastType, ColumnRef, CommonTable, CompareOp, CompoundOp, CompoundTerm,
    ConflictAction, ConflictClause, DropColumnConflict, Expr, ForeignKeyAction, FrameBound, FrameEdge, FrameExclude, FrameUnits, IndexColumn,
    InsertValue, JoinKind, Limit, Literal, OrderKey, ScalarFunction, SelectCore, SelectItems, SqlError, Statement, TableRef, TransactionAction,
    TriggerEvent, TriggerTiming, UnaryOp, WindowSpec, core_references,
    datetime::{self, DateTimeFunction},
    host::{HostFunctionTable, cell_to_udf_value, udf_error_to_sql_error, udf_value_to_cell},
    json_string, parse, parse_with_host_functions, rewrite_create_index_column, rewrite_create_index_table, rewrite_create_table_column,
    rewrite_create_table_drop_column, rewrite_create_table_name, rewrite_view_column, rewrite_view_table, statement_references,
    storage_failure, strip_schema_qualifiers,
};

use super::{bind::*, catalog::*, value::*};

/// Result of one executed statement.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecResult {
    /// Lowercase statement name (`create-table`, `create-index`, `insert`, `select`).
    pub statement: &'static str,
    /// Column labels in result order.
    pub columns: Vec<String>,
    /// Result rows; empty for DDL and inserts.
    pub rows: Vec<Vec<Cell>>,
    /// Rows created by an insert, or 1 for DDL.
    pub affected: usize,
}

impl ExecResult {
    /// Serializes one result as the documented JSON envelope.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"statement\":");
        out.push_str(&json_string(self.statement));
        out.push_str(",\"affected\":");
        out.push_str(&self.affected.to_string());
        out.push_str(",\"columns\":[");
        for (index, column) in self.columns.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&json_string(column));
        }
        out.push_str("],\"rows\":[");
        for (row_index, row) in self.rows.iter().enumerate() {
            if row_index > 0 {
                out.push(',');
            }
            out.push('[');
            for (cell_index, cell) in row.iter().enumerate() {
                if cell_index > 0 {
                    out.push(',');
                }
                out.push_str(&cell_json(cell));
            }
            out.push(']');
        }
        out.push_str("]}");
        out
    }
}

/// How one base-table scan is bounded before any row is decoded.
///
/// Every variant keeps the emitted row set and row order identical to the
/// unbounded read: the full predicate still runs downstream, and a bounded
/// read only narrows the candidate rows the scan materializes.
enum ScanHint {
    /// `WHERE <rowid alias> = <integer literal>`: one candidate rowid.
    Pin(i64),
    /// `WHERE <indexed column> = <literal>` covering every key column of a
    /// plain column index: the rowids the index probe found, ascending.
    Rowids(Vec<i64>),
    /// `ORDER BY <rowid order> LIMIT n` with no other row-reducing clause:
    /// only the first `n` rows in rowid order can reach the output.
    Head(usize),
    /// `<rowid> >= a AND/OR <rowid> <= b` 的单边或双边区间:按 rowid 区间读取,
    /// 只解码区间内的行。缺失的一侧不设界。
    Range { lower: Option<i64>, upper: Option<i64> },
    /// `LIMIT 0`: no row can reach the output, so the base table is not read at
    /// all. Column names still come from the schema, exactly as the materialized
    /// path would produce them.
    Empty,
}

/// One inclusive/exclusive range bound on an indexed column.
struct IndexBound {
    /// Literal the stored key is compared against.
    cell: Cell,
    /// Whether a key equal to `cell` is inside the range.
    inclusive: bool,
}

/// Cached live key set of one plain column index, valid while `epoch` matches
/// the engine's write epoch.
struct IndexProbe {
    /// Write epoch the key set was read under.
    epoch: u64,
    /// Rowids sharing a canonical key, ascending.
    map: HashMap<Vec<ProbeKey>, Vec<i64>>,
    /// Decoded key values in index order, used by range probes that need the
    /// engine's own comparison instead of a hash lookup.
    entries: Vec<(Vec<Cell>, i64)>,
    /// `entries` 按首列排序规则非递减,范围探针因此可以二分定位。
    sorted: bool,
    /// 已从表中删除、但还没从 `entries` 里剔除的 rowid。
    ///
    /// 删除语句按行打补丁时只把 rowid 记到这里,避免每条删除都扫一遍整份
    /// 索引项;条目累积到一半时再一次性压缩。
    deleted: HashSet<i64>,
    /// A stored key the canonical form cannot reproduce; the probe is then
    /// never used and every query falls back to the full scan.
    poisoned: bool,
}

/// Memoized result of one uncorrelated subquery, valid for a single statement.
enum SubqueryCache {
    /// Rows of a scalar subquery or `EXISTS`, in evaluation order.
    Rows(Vec<Vec<Cell>>),
    /// Value set of `IN (SELECT ...)`; `keys` is `None` when a value has no
    /// canonical signature, so membership falls back to the linear scan.
    InList { values: Vec<Cell>, keys: Option<HashSet<ProbeKey>>, saw_null: bool },
}

/// 一个相关 `EXISTS` 的索引探针计划。
///
/// 只接受「内层单表 + 一个内层列等于外层列的等值 + 其余只读内层列的合取」
/// 这一形状。计划按语句内 AST 地址缓存,逐行求值只剩一次索引探针和候选行
/// 解码,不再重跑内层 `SELECT` 的整套解析与物化流程。
struct ExistsProbePlan {
    /// 内层表名,读取时按目录重新定位。
    table: String,
    /// 内层表在 `tables` 中的位置。
    table_position: usize,
    /// 参与探针的普通索引名(小写)。
    index: String,
    /// 参与等值的内层列位置。
    slot: usize,
    /// 参与等值的外层(当前行)单元格位置。
    outer: usize,
    /// 索引探针使用的排序规则。
    probe_collation: Collation,
    /// 等值比较本身的排序规则;探针只保证候选超集,结果以它复核。
    compare_collation: Collation,
    /// 等值之外的其余合取,只读内层列。
    residual: Vec<ResolvedExpr>,
    /// 内层行形状,供残差求值。
    shape: ScopeShape,
}

/// One row-level trigger event queued by a DML statement. Queueing instead of
/// firing inline keeps the statement's own writes and its trigger programs in
/// SQLite's row order while leaving the write path unchanged.
struct PendingTriggerEvent {
    /// Lowercase table the event belongs to.
    table: String,
    timing: TriggerTiming,
    event: TriggerEvent,
    columns: Vec<String>,
    new: Option<Vec<Cell>>,
    old: Option<Vec<Cell>>,
}

/// Depth limit SQLite applies before raising `too many levels of trigger
/// recursion` when `PRAGMA recursive_triggers` is on.
const MAX_TRIGGER_DEPTH: usize = 100;

/// 写语句候选 rowid 合并成区间读的最大缝隙。
///
/// 候选按 rowid 升序,缝隙不超过这个值的相邻候选交给同一次 `table_range`,用少量
/// 多余行换掉一次 b-tree 下降。
const CANDIDATE_GAP: i64 = 8;

/// 行 rowid 区间下推的候选上限。
///
/// `WHERE <rowid> >= a AND <rowid> <= b` 直接按区间读 b-tree,但只有区间宽度在
/// 这个上限内才展开成候选 rowid;更宽的区间仍然是整表扫描,避免为一条语句
/// 造出与表同量级的候选向量。
const ROWID_RANGE_CANDIDATES: i64 = 4_096;

/// One database attached with `ATTACH DATABASE`.
struct AttachedDb {
    /// Schema name as written.
    name: String,
    /// File the database lives in; `None` for an in-memory or temporary one.
    file: Option<PathBuf>,
    /// Text `PRAGMA database_list` reports in the `file` column.
    display: String,
    /// The attached database runs through the same engine as `main`, so every
    /// statement shape keeps working inside it without a second parser.
    engine: Box<Engine>,
    /// The in-memory image differs from the file on disk.
    dirty: bool,
}

/// Which schema a statement resolves its unqualified names in. The default
/// follows SQLite's search order (temp, then main, then attached databases).
#[derive(Clone, Copy, PartialEq, Eq)]
enum TableScope {
    Default,
    Main,
    Temp,
}

/// Object family an unqualified `DROP` or `ALTER` statement may target in an
/// attached database.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Table,
    Index,
    View,
    Trigger,
}

/// Reserved table names of one schema. `main.` never reaches the temp master
/// and `temp.` never reaches the main master, matching SQLite's isolation.
fn system_for_scope(scope: TableScope, name: &str) -> Option<SystemTable> {
    let system = SystemTable::from_name(name)?;
    match (scope, system) {
        (TableScope::Main, SystemTable::TempMaster) => None,
        (TableScope::Temp, SystemTable::Master | SystemTable::Sequence) => None,
        _ => Some(system),
    }
}

/// The schema qualifier that decides where a statement runs.
fn leading_schema(statement: &Statement) -> Option<&str> {
    let primary = match statement {
        Statement::CreateTable { schema, .. }
        | Statement::CreateView { schema, .. }
        | Statement::CreateIndex { schema, .. }
        | Statement::DropTable { schema, .. }
        | Statement::DropIndex { schema, .. }
        | Statement::DropView { schema, .. }
        | Statement::DropTrigger { schema, .. }
        | Statement::AlterTable { schema, .. }
        | Statement::Insert { schema, .. }
        | Statement::Update { schema, .. }
        | Statement::Delete { schema, .. }
        | Statement::Pragma { schema, .. }
        | Statement::Vacuum { schema, .. }
        | Statement::CreateTrigger { schema, .. } => schema.as_deref(),
        _ => None,
    };
    if primary.is_some() {
        return primary;
    }
    match statement {
        Statement::CreateTrigger { table_schema, .. } => table_schema.as_deref(),
        _ => None,
    }
}

/// Drops the qualifier that matches `schema` from a statement so it can run
/// inside that schema.
fn clear_statement_schema(statement: &mut Statement, schema: &str) {
    fn clear(slot: &mut Option<String>, schema: &str) {
        if slot.as_deref().is_some_and(|value| value.eq_ignore_ascii_case(schema)) {
            *slot = None;
        }
    }
    match statement {
        Statement::CreateTable { schema: slot, .. }
        | Statement::CreateView { schema: slot, .. }
        | Statement::CreateIndex { schema: slot, .. }
        | Statement::DropTable { schema: slot, .. }
        | Statement::DropIndex { schema: slot, .. }
        | Statement::DropView { schema: slot, .. }
        | Statement::DropTrigger { schema: slot, .. }
        | Statement::AlterTable { schema: slot, .. }
        | Statement::Insert { schema: slot, .. }
        | Statement::Update { schema: slot, .. }
        | Statement::Delete { schema: slot, .. }
        | Statement::Pragma { schema: slot, .. }
        | Statement::Vacuum { schema: slot, .. } => clear(slot, schema),
        Statement::CreateTrigger { schema: slot, table_schema, .. } => {
            clear(slot, schema);
            clear(table_schema, schema);
        }
        _ => {}
    }
}

/// SQLite stores an attached database's objects without their schema
/// qualifier; the same rewrite keeps the stored text reloadable.
fn strip_statement_sql(statement: &mut Statement, schema: &str) {
    match statement {
        Statement::CreateTable { sql, .. }
        | Statement::CreateView { sql, .. }
        | Statement::CreateTrigger { sql, .. }
        | Statement::CreateIndex { sql, .. } => {
            *sql = strip_schema_qualifiers(sql, schema);
        }
        _ => {}
    }
}

/// Error for a schema name that is neither `main`, `temp`, nor attached.
fn unknown_schema_error(statement: &Statement, schema: &str) -> SqlError {
    match statement {
        Statement::Insert { table, .. } | Statement::Update { table, .. } | Statement::Delete { table, .. } => {
            SqlError::new("unknown-table", format!("no such table: {schema}.{table}")).in_statement(statement.name())
        }
        _ => SqlError::new("unknown-database", format!("unknown database {schema}")).in_statement(statement.name()),
    }
}

thread_local! {
    /// Views can be nested, but a view that reaches itself would recurse
    /// forever. SQLite rejects the circular definition at prepare time; this
    /// counter turns the same case into one structured error instead.
    static VIEW_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Decrements the view depth when the nested evaluation returns, including on
/// an error path.
struct ViewDepthGuard;

impl ViewDepthGuard {
    fn enter() -> Result<Self, SqlError> {
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

/// Statement label a trigger event reports in diagnostics.
fn event_statement(event: &TriggerEvent) -> &'static str {
    match event {
        TriggerEvent::Insert => "insert",
        TriggerEvent::Update(_) => "update",
        TriggerEvent::Delete => "delete",
    }
}

/// Column names a trigger frame exposes: the declared columns plus `rowid`.
fn trigger_frame_columns(def: &TableDef) -> Vec<String> {
    let mut columns: Vec<String> = def.columns.iter().map(|column| column.name.clone()).collect();
    columns.push("rowid".to_string());
    columns
}

/// Appends the rowid slot a trigger frame expects to one declared row.
fn frame_cells(cells: &[Cell], width: usize, rowid: i64) -> Vec<Cell> {
    let mut out = cells.to_vec();
    out.resize(width, Cell::Null);
    out.push(Cell::Integer(rowid));
    out
}

/// Output names of one view: the declared list when present, otherwise the
/// names the query produced. SQLite rejects a declared list whose width does
/// not match the query, and suffixes repeated names with `:1`, `:2`, ...
fn view_output_names(view: &ViewDef, actual: &[String]) -> Result<Vec<String>, SqlError> {
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
fn unique_output_names(names: &[String]) -> Vec<String> {
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

/// SQLite's standard type name for one declared type: the affinity class the
/// type text resolves to, or an empty string for the BLOB affinity.
fn standard_affinity_name(declared: &str) -> String {
    let upper = declared.to_ascii_uppercase();
    let name = if upper.contains("INT") {
        "INT"
    }
    else if upper.contains("CHAR") || upper.contains("CLOB") || upper.contains("TEXT") {
        "TEXT"
    }
    else if declared.is_empty() || upper.contains("BLOB") {
        ""
    }
    else if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB") {
        "REAL"
    }
    else {
        "NUM"
    };
    name.to_string()
}

/// Resolves a `column` reference of a CTAS output item to the unique source
/// column that defines it.
fn ctas_lookup_column<'a>(reference: &ColumnRef, sources: &'a [(String, Vec<(String, String)>)]) -> Option<&'a (String, String)> {
    let qualifier = reference.qualifier.as_deref();
    let mut found = None;
    for (label, columns) in sources {
        if let Some(qualifier) = qualifier {
            if !label.eq_ignore_ascii_case(qualifier) {
                continue;
            }
        }
        for column in columns {
            if column.0.eq_ignore_ascii_case(&reference.name) {
                if found.is_some() {
                    return None;
                }
                found = Some(column);
            }
        }
    }
    found
}

/// Output label of one CTAS item: the source column's own spelling for a bare
/// reference, otherwise the expression's label.
fn ctas_expression_label(expression: &Expr, sources: &[(String, Vec<(String, String)>)]) -> String {
    if let Expr::Column(reference) = expression {
        if let Some(column) = ctas_lookup_column(reference, sources) {
            return column.0.clone();
        }
    }
    expression.label()
}

/// Declared type of one CTAS output item: a bare column reference copies its
/// source affinity, every other expression gets no declared type.
fn ctas_expression_type(expression: &Expr, sources: &[(String, Vec<(String, String)>)]) -> String {
    match expression {
        Expr::Alias { value, .. } => ctas_expression_type(value, sources),
        Expr::Column(reference) => ctas_lookup_column(reference, sources).map(|column| standard_affinity_name(&column.1)).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Quotes a schema identifier when the generated `CREATE TABLE` text needs it.
fn ctas_quote_name(name: &str) -> String {
    let plain = !name.is_empty()
        && name.chars().next().is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && name.chars().all(|character| character.is_ascii_alphanumeric() || character == '_');
    if plain { name.to_string() } else { format!("\"{}\"", name.replace('"', "\"\"")) }
}

/// `sqlite_master.sql` text of a `CREATE TABLE ... AS SELECT` result, built the
/// same way SQLite generates it. SQLite switches to a multi-line layout for
/// long definitions; the compact form is emitted here because every reader
/// (including this parser) accepts it.
fn ctas_schema_sql(name: &str, columns: &[ColumnSpec]) -> String {
    let mut sql = format!("CREATE TABLE {}(", ctas_quote_name(name));
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            sql.push(',');
        }
        sql.push_str(&ctas_quote_name(&column.name));
        if !column.declared_type.is_empty() {
            sql.push(' ');
            sql.push_str(&column.declared_type);
        }
    }
    sql.push(')');
    sql
}

/// `INSERT ... SELECT` slot mapping: SQLite reports a different arity message
/// depending on whether an explicit column list was written.
fn select_arity(
    def: &TableDef,
    table: &str,
    columns: Option<&[String]>,
    values: &[crate::WriteValue],
) -> Result<Vec<Option<crate::WriteValue>>, SqlError> {
    match columns {
        Some(names) => {
            if values.len() != names.len() {
                return Err(
                    SqlError::new("arity-mismatch", format!("{} values for {} columns", values.len(), names.len())).in_statement("insert")
                );
            }
            self_contained_arity(def, table, Some(names), values)
        }
        None => {
            if values.len() != def.columns.len() {
                return Err(SqlError::new(
                    "arity-mismatch",
                    format!("table {table} has {} columns but {} values were supplied", def.columns.len(), values.len()),
                )
                .in_statement("insert"));
            }
            self_contained_arity(def, table, None, values)
        }
    }
}

/// Converts one select result cell into an `INSERT` value when the target is a
/// view handled by `INSTEAD OF` triggers.
fn cell_insert_value(cell: &Cell) -> InsertValue {
    InsertValue::Literal(match cell {
        Cell::Null => super::Literal::Null,
        Cell::Integer(value) => super::Literal::Integer(*value),
        Cell::Real(value) => super::Literal::Real(*value),
        Cell::Text(value) => super::Literal::Text(value.clone()),
        Cell::Blob(value) => super::Literal::Blob(value.clone()),
    })
}

/// A `FROM`-shape over one view's output, so view DML can resolve its filter
/// and assignments with the regular expression path.
fn view_relation<'a>(label: &str, names: &[String], trigger: Option<&'a TriggerFrame>) -> Relation<'a> {
    let width = names.len();
    Relation {
        sources: vec![Source {
            label: label.to_string(),
            columns: SourceColumns::Materialized { names: names.to_vec(), rowid: false },
            offset: 0,
            join: None,
            star_hidden: vec![false; width],
            unqualified_hidden: vec![false; width],
            star_override: vec![None; width],
        }],
        using: vec![Vec::new()],
        using_slots: Vec::new(),
        trigger,
        excluded: false,
    }
}

/// Evaluates an optional view-DML predicate against one output row.
fn row_matches(predicate: &Option<ResolvedExpr>, row: &[Cell], engine: &Engine, shape: &ScopeShape, budget: &Budget) -> Result<bool, SqlError> {
    let Some(predicate) = predicate
    else {
        return Ok(true);
    };
    let ctx = EvalContext { engine, outer: &[], current: shape, aggregates: &[], windows: &[], window_row: 0, budget, ctes: None };
    Ok(truthiness(&predicate.eval(row, &ctx)?) == Some(true))
}

/// Whether a definition's event covers an incoming statement event.
fn trigger_event_matches(defined: &TriggerEvent, incoming: &TriggerEvent) -> bool {
    match (defined, incoming) {
        (TriggerEvent::Insert, TriggerEvent::Insert) | (TriggerEvent::Delete, TriggerEvent::Delete) => true,
        (TriggerEvent::Update(defined), TriggerEvent::Update(assigned)) => {
            defined.is_empty() || defined.iter().any(|column| assigned.iter().any(|assigned| assigned.eq_ignore_ascii_case(column)))
        }
        _ => false,
    }
}

/// One uniqueness rule an `UPDATE` or an `ON CONFLICT` target re-checks
/// against the live table.
#[derive(Clone)]
enum UniqueKey {
    /// The hidden rowid, reported under `t.rowid` or `t.<alias>`.
    Rowid(String),
    /// One or more column positions, plus the `t.a, t.b` message fragment and
    /// the collation each key position compares with.
    Columns(Vec<usize>, String, Vec<Collation>),
}

impl UniqueKey {
    /// The key values of one row; `cells` holds the declared columns only.
    fn values(&self, cells: &[Cell], rowid: i64) -> Vec<Cell> {
        match self {
            Self::Rowid(_) => vec![Cell::Integer(rowid)],
            Self::Columns(positions, _, _) => positions.iter().map(|position| cells[*position].clone()).collect(),
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::Rowid(label) | Self::Columns(_, label, _) => label,
        }
    }
}

/// Everything an `UPDATE` will write, computed before storage is touched so a
/// constraint failure can never leave a half-updated table.
struct UpdatePlan {
    /// Current rowid plus one slot per column, in ascending rowid order.
    writes: Vec<(i64, Vec<UpdateValue>)>,
    /// `RETURNING` rows, evaluated against the new row values.
    returning: Vec<Vec<Cell>>,
    affected: usize,
    /// 规划期读到的行集快照。处理阶段直接复用它，不再重复整表解码。
    live: Vec<(i64, Vec<Cell>)>,
}

/// Everything a `DELETE` will remove.
struct DeletePlan {
    removed: Vec<i64>,
    /// `RETURNING` rows, evaluated against the row as it was.
    returning: Vec<Vec<Cell>>,
    /// 规划期读到的行集快照（以 `removed` 为子集），供触发器与外键动作复用。
    live: Vec<(i64, Vec<Cell>)>,
}

/// One engine-managed unique index an `ON CONFLICT` target matched.
#[derive(Clone)]
struct ManagedTarget {
    /// The partial or expression index the target selected.
    index: IndexDef,
    /// `t.a, t.b` fragment for uniqueness failures on the index.
    label: String,
}

/// The conflict target and rewrite of one `INSERT ... ON CONFLICT` clause.
struct UpsertPlan {
    /// Uniqueness rules the clause reacts to.
    keys: Vec<UniqueKey>,
    /// Managed indexes the clause reacts to.
    managed: Vec<ManagedTarget>,
    /// Other uniqueness rules that still abort the statement when the supplied
    /// row violates them.
    others: Vec<UniqueKey>,
    /// Managed indexes outside the target that still abort the statement.
    other_managed: Vec<ManagedTarget>,
    /// `DO NOTHING` or `DO UPDATE`.
    action: UpsertAction,
    /// `DO UPDATE SET` targets with their resolved right-hand sides.
    assignments: Vec<(usize, ResolvedExpr)>,
    /// Optional `DO UPDATE ... WHERE` predicate over the stored and supplied
    /// rows.
    filter: Option<ResolvedExpr>,
}

/// What an `ON CONFLICT` clause does with a conflicting row.
enum UpsertAction {
    /// `DO NOTHING`: the supplied row is discarded.
    Nothing,
    /// `DO UPDATE`: the conflicting stored row is rewritten.
    Update,
}

/// Result of applying one supplied row of an upsert.
enum UpsertOutcome {
    /// `DO NOTHING` skipped the row.
    Skipped,
    /// The row was written with this rowid.
    Inserted(i64),
    /// The conflicting row was rewritten.
    Updated {
        /// Rowid of the rewritten row.
        rowid: i64,
        /// `RETURNING` rows evaluated against the new values.
        rows: Vec<Vec<Cell>>,
    },
}

/// Transitional home for `RETURNING` projection evaluation until the
/// evaluation closure moves the mechanism itself.
trait EvalReturningPlan {
    /// Evaluates the projection against one written row.
    fn eval(&self, cells: &[Cell], ctx: &EvalContext) -> Result<Vec<Cell>, SqlError>;
}

impl EvalReturningPlan for ReturningPlan {
    fn eval(&self, cells: &[Cell], ctx: &EvalContext) -> Result<Vec<Cell>, SqlError> {
        self.items.iter().map(|item| item.eval(cells, ctx)).collect()
    }
}

/// Reserved tables SQLite keeps in the schema itself and that this engine
/// exposes as read-only sources.
#[derive(Clone, Copy)]
enum SystemTable {
    /// `sqlite_master`, also reachable as `sqlite_schema`.
    Master,
    /// `sqlite_temp_master`, always empty because temporary objects are not
    /// created by this surface.
    TempMaster,
    /// `sqlite_sequence`, present only after an AUTOINCREMENT table.
    Sequence,
}

impl SystemTable {
    fn from_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "sqlite_master" | "sqlite_schema" => Some(Self::Master),
            "sqlite_temp_master" | "sqlite_temp_schema" => Some(Self::TempMaster),
            "sqlite_sequence" => Some(Self::Sequence),
            _ => None,
        }
    }
}

/// `sqlite_master` and its aliases.
fn is_schema_table(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "sqlite_master" | "sqlite_schema" | "sqlite_temp_master" | "sqlite_temp_schema")
}

/// SQLite reserves every `sqlite_` name for internal objects.
fn check_reserved_object_name(name: &str, statement: &'static str) -> Result<(), SqlError> {
    if name.to_ascii_lowercase().starts_with("sqlite_") {
        return Err(SqlError::new("reserved-name", format!("object name reserved for internal use: {name}")).in_statement(statement));
    }
    Ok(())
}

/// Maps a storage failure onto the engine error surface.
fn storage_error(error: impl std::fmt::Display) -> SqlError {
    SqlError::new("storage", error.to_string()).in_statement("select")
}

/// A malformed on-disk structure found by the lazy scan.
fn corrupt_error(message: &'static str) -> SqlError {
    SqlError::new("corrupt", message).in_statement("select")
}

/// The schema tables are read-only for user writes.
fn check_readonly_table(name: &str, statement: &'static str) -> Result<(), SqlError> {
    if is_schema_table(name) || name.eq_ignore_ascii_case("sqlite_sequence") {
        return Err(SqlError::new("readonly-system-table", format!("table {name} may not be modified")).in_statement(statement));
    }
    Ok(())
}

/// In-memory SQLite format-3 database with a minimal SQL surface.
pub struct Engine {
    writer: SqliteWriter,
    /// The catalog: schema objects statements resolve names against, plus
    /// the temp-schema allocator state.
    catalog: SqliteCatalog,
    /// `NEW` / `OLD` frames of the trigger bodies currently executing,
    /// outermost first.
    trigger_frames: Vec<TriggerFrame>,
    /// Row-level trigger events queued by the statement being executed.
    pending_events: Vec<PendingTriggerEvent>,
    /// Databases attached with `ATTACH DATABASE`, in attach order.
    attached: Vec<AttachedDb>,
    /// Lowercase attached schema name to its position in [`Engine::attached`].
    attached_lookup: BTreeMap<String, usize>,
    /// Schema name this database was attached under. A reference to that name
    /// inside the database itself resolves like an unqualified one.
    alias: Option<String>,
    /// `main.` / `temp.` qualifier of the statement being executed.
    scope: TableScope,
    /// Lowercase names of the triggers whose body is executing right now.
    /// With SQLite's default `recursive_triggers = OFF` a trigger program is
    /// not re-invoked by changes it caused itself, directly or indirectly,
    /// while the other triggers still fire.
    active_triggers: Vec<String>,
    /// Values bound to `?` / `:name` placeholders for the statement currently
    /// executing, in declaration order.
    parameters: Vec<Cell>,
    /// Supplied row of the `ON CONFLICT DO UPDATE` clause being evaluated.
    /// `excluded.<column>` reads this row instead of the table's stored row.
    upsert_excluded: Vec<Cell>,
    /// `PRAGMA foreign_keys`, enabled at session start like better-sqlite3.
    foreign_keys: bool,
    /// `PRAGMA recursive_triggers`, off at session start like SQLite.
    recursive_triggers: bool,
    /// Rowid of the most recent row a statement wrote successfully, like
    /// `sqlite3_last_insert_rowid`. It starts at 0 for every connection and
    /// only changes when an insert writes at least one row.
    last_insert_rowid: i64,
    /// Open explicit transaction, or `None` in autocommit. Savepoint names are
    /// stored outermost first.
    transaction: Option<TransactionState>,
    /// 单调递增的时钟,给索引内容版本取号用。
    ///
    /// 只有真正改动了某个索引内容的语句才推进它,逐语句推进会让写循环里
    /// 每次探针都重建整份索引键集。
    write_epoch: std::cell::Cell<u64>,
    /// 索引内容版本:小写索引名 → 该索引内容最后变化时的 epoch。
    ///
    /// 探针缓存的 `epoch` 与这里一致时才可复用。`INSERT` / 改动索引列的
    /// `UPDATE` / `DELETE` 之外的语句(DDL、`VACUUM` 等)一律全量失效。
    index_versions: std::cell::RefCell<HashMap<String, u64>>,
    /// Live key sets of plain column indexes, keyed by lowercase index name,
    /// valid only while the stored epoch matches the index's content version.
    index_probes: std::cell::RefCell<HashMap<String, IndexProbe>>,
    /// Memoized uncorrelated subquery results for the running statement, keyed
    /// by the resolved subquery node address. Cleared before every statement.
    subquery_cache: std::cell::RefCell<HashMap<usize, SubqueryCache>>,
    /// 相关 `EXISTS` 的索引探针计划,按语句内 AST 地址缓存;`None` 表示该
    /// 形状不适用,同一语句内不再重复尝试。每条语句开始时清空。
    exists_probes: std::cell::RefCell<HashMap<usize, Option<ExistsProbePlan>>>,
    /// 单条语句内已解码的整表行,按小写表名索引,并记录该份行集覆盖的列掩码。
    /// 只缓存主库普通 rowid 表的无提示全表扫描:新请求所需列若已被缓存掩码
    /// 覆盖(全量解码覆盖任何裁剪),就复用同一份行集,让同表内外层子查询与
    /// 自连接不必各自解码一遍。语句开始与结束都清空,不跨语句保留。
    statement_scans: std::cell::RefCell<HashMap<String, (Option<Vec<bool>>, std::rc::Weak<Vec<Vec<Cell>>>)>>,
    /// Set whenever an `OuterColumn` is read, so a subquery can tell whether it
    /// depended on the enclosing row and therefore must not be memoized.
    outer_read: std::cell::Cell<bool>,
    /// 会话宿主函数表,按折叠小写名索引;解析期与求值期共用同一份。
    host_functions: HostFunctionTable,
    /// 显式事务内被推迟的索引 b-tree 重建。
    ///
    /// 索引页是整树重建出来的,单条语句重建一次会让事务内的第 n 条写入花掉
    /// O(n) 的重建成本。事务内只置位,提交或读镜像前统一重建一次。
    pending_indexes: bool,
    /// 同 [`Engine::pending_indexes`],针对 WITHOUT ROWID 表的聚簇树。
    pending_without_rowid: bool,
}

/// Explicit transaction bookkeeping, mirroring the writer's snapshot levels.
struct TransactionState {
    /// `BEGIN` opened the transaction rather than a bare `SAVEPOINT`.
    from_begin: bool,
    /// Open savepoint names, outermost first.
    savepoints: Vec<String>,
}

impl Engine {
    /// Creates a blank in-memory database image.
    pub fn new(page_size: Option<u32>) -> Result<Self, SqlError> {
        let page_size = page_size.unwrap_or(4096) as usize;
        let writer = SqliteWriter::create(page_size).map_err(|error| storage_failure(error))?;
        Self::from_writer(writer)
    }

    /// Wraps an already opened writer and rebuilds the catalog from the
    /// `sqlite_master` rows of its image, so `SELECT` works on a file opened
    /// with [`SqliteWriter::open`].
    pub fn from_writer(writer: SqliteWriter) -> Result<Self, SqlError> {
        let mut engine = Self {
            writer,
            catalog: SqliteCatalog::default(),
            trigger_frames: Vec::new(),
            pending_events: Vec::new(),
            attached: Vec::new(),
            attached_lookup: BTreeMap::new(),
            alias: None,
            scope: TableScope::Default,
            active_triggers: Vec::new(),
            parameters: Vec::new(),
            upsert_excluded: Vec::new(),
            foreign_keys: true,
            recursive_triggers: false,
            last_insert_rowid: 0,
            transaction: None,
            write_epoch: std::cell::Cell::new(0),
            index_versions: std::cell::RefCell::new(HashMap::new()),
            index_probes: std::cell::RefCell::new(HashMap::new()),
            subquery_cache: std::cell::RefCell::new(HashMap::new()),
            exists_probes: std::cell::RefCell::new(HashMap::new()),
            statement_scans: std::cell::RefCell::new(HashMap::new()),
            outer_read: std::cell::Cell::new(false),
            host_functions: HostFunctionTable::default(),
            pending_indexes: false,
            pending_without_rowid: false,
        };
        engine.load_catalog()?;
        Ok(engine)
    }

    /// Applies one explicit transaction control statement. Every scope keeps a
    /// writer snapshot level, so statements stay atomic inside a wider
    /// transaction and a rollback only undoes the scope it names.
    fn transaction(&mut self, action: TransactionAction) -> Result<ExecResult, SqlError> {
        let statement = action_name(&action);
        match action {
            TransactionAction::Begin => {
                if self.transaction.is_some() {
                    return Err(SqlError::new("transaction-active", "cannot start a transaction within a transaction").in_statement("begin"));
                }
                self.writer.begin().map_err(|error| storage_failure(error).in_statement("begin"))?;
                for attached in &mut self.attached {
                    attached.engine.writer.begin().map_err(|error| storage_failure(error).in_statement("begin"))?;
                }
                self.transaction = Some(TransactionState { from_begin: true, savepoints: Vec::new() });
            }
            TransactionAction::Commit => {
                if self.transaction.is_none() {
                    return Err(SqlError::new("transaction-missing", "cannot commit - no transaction is active").in_statement("commit"));
                }
                self.check_deferred_foreign_keys()?;
                for attached in &self.attached {
                    attached.engine.check_deferred_foreign_keys()?;
                }
                // 推迟的索引重建留到读取镜像的入口再落下:提交本身不读镜像,
                // 在这里重建会让每个写事务都付一次整树重建成本。
                self.transaction.take();
                self.writer.commit_all().map_err(|error| storage_failure(error).in_statement("commit"))?;
                for attached in &mut self.attached {
                    attached.engine.writer.commit_all().map_err(|error| storage_failure(error).in_statement("commit"))?;
                }
                self.persist_attached_databases()?;
            }
            TransactionAction::Rollback => {
                if self.transaction.take().is_none() {
                    return Err(SqlError::new("transaction-missing", "cannot rollback - no transaction is active").in_statement("rollback"));
                }
                self.writer.rollback_all().map_err(|error| storage_failure(error).in_statement("rollback"))?;
                // 回滚把内存键集退回事务前,但推迟标记可能来自事务开始之前
                // 的 autocommit 写入,这里只保留标记,不因回滚丢弃。
                self.reload_catalog()?;
                for index in 0..self.attached.len() {
                    self.attached[index].engine.writer.rollback_all().map_err(|error| storage_failure(error).in_statement("rollback"))?;
                    self.attached[index].dirty = false;
                    self.attached[index].engine.reload_catalog()?;
                }
            }
            TransactionAction::Savepoint(name) => {
                if self.transaction.is_none() {
                    // A savepoint outside `BEGIN` opens its own transaction.
                    self.writer.begin().map_err(|error| storage_failure(error).in_statement("savepoint"))?;
                    for attached in &mut self.attached {
                        attached.engine.writer.begin().map_err(|error| storage_failure(error).in_statement("savepoint"))?;
                    }
                    self.transaction = Some(TransactionState { from_begin: false, savepoints: Vec::new() });
                }
                self.writer.savepoint(&name).map_err(|error| storage_failure(error).in_statement("savepoint"))?;
                for attached in &mut self.attached {
                    attached.engine.writer.savepoint(&name).map_err(|error| storage_failure(error).in_statement("savepoint"))?;
                }
                self.transaction.as_mut().expect("savepoint opened a transaction").savepoints.push(name);
            }
            TransactionAction::Release(name) => {
                let index = self.savepoint_position(&name, "release")?;
                let closing = {
                    let state = self.transaction.as_ref().expect("an open savepoint implies a transaction");
                    index == 0 && !state.from_begin
                };
                if closing {
                    // Releasing the last savepoint of a transaction that no
                    // `BEGIN` opened commits it, so the commit gate runs
                    // first: deferred foreign keys must pass before any
                    // savepoint state is destroyed, and a failed check leaves
                    // the transaction open for a repair or a rollback.
                    self.check_deferred_foreign_keys()?;
                    for attached in &self.attached {
                        attached.engine.check_deferred_foreign_keys()?;
                    }
                }
                self.writer.release(&name).map_err(|error| storage_failure(error).in_statement("release"))?;
                for attached in &mut self.attached {
                    attached.engine.writer.release(&name).map_err(|error| storage_failure(error).in_statement("release"))?;
                }
                let closing = {
                    let state = self.transaction.as_mut().expect("an open savepoint implies a transaction");
                    state.savepoints.truncate(index);
                    state.savepoints.is_empty() && !state.from_begin
                };
                if closing {
                    // Releasing the last savepoint of a transaction that no
                    // `BEGIN` opened commits it, as in SQLite. 索引重建同样
                    // 推迟到读取镜像的入口。
                    self.writer.commit_all().map_err(|error| storage_failure(error).in_statement("release"))?;
                    self.transaction = None;
                    self.persist_attached_databases()?;
                }
            }
            TransactionAction::RollbackTo(name) => {
                let index = self.savepoint_position(&name, "rollback-to")?;
                self.writer.rollback_to(&name).map_err(|error| storage_failure(error).in_statement("rollback-to"))?;
                self.reload_catalog()?;
                for index in 0..self.attached.len() {
                    self.attached[index]
                        .engine
                        .writer
                        .rollback_to(&name)
                        .map_err(|error| storage_failure(error).in_statement("rollback-to"))?;
                    self.attached[index].engine.reload_catalog()?;
                }
                self.transaction.as_mut().expect("an open savepoint implies a transaction").savepoints.truncate(index + 1);
            }
        }
        Ok(ExecResult { statement, columns: Vec::new(), rows: Vec::new(), affected: 0 })
    }

    /// Finds an open savepoint by name and reports SQLite's missing-savepoint
    /// error when it is absent. Names are case insensitive.
    fn savepoint_position(&self, name: &str, statement: &'static str) -> Result<usize, SqlError> {
        let missing = || SqlError::new("savepoint-missing", format!("no such savepoint: {name}")).in_statement(statement);
        let state = self.transaction.as_ref().ok_or_else(missing)?;
        state.savepoints.iter().rposition(|saved| saved.eq_ignore_ascii_case(name)).ok_or_else(missing)
    }

    /// Rebuilds the in-memory catalog after storage rolled back, so schema
    /// changes undone by the rollback also disappear from later statements.
    fn reload_catalog(&mut self) -> Result<(), SqlError> {
        self.catalog.tables.clear();
        self.catalog.table_lookup.clear();
        self.catalog.indexes.clear();
        self.catalog.views.clear();
        self.catalog.triggers.clear();
        self.load_catalog()
    }

    /// `PRAGMA integrity_check`: walks the header, the schema, every table and
    /// index b-tree, and the freelist chain over the current image. A healthy
    /// image reports one `ok` row; a structural problem is reported as a row of
    /// text, matching SQLite's non-failing result shape.
    ///
    /// The check reads pages, it does not trust the in-memory catalog or the
    /// writer's bookkeeping, so a decoder failure surfaces here instead of
    /// being answered with a hard-coded `ok`. Page-reachability accounting
    /// (detecting a page shared by two b-trees) is outside this walk because
    /// the reader API validates each tree in isolation.
    fn pragma_integrity_check(&mut self, argument: &str) -> ExecResult {
        // 检查走的是镜像页,先把推迟的索引重建落下,否则会把旧索引页判成问题。
        if let Err(error) = self.flush_pending_materialization("integrity-check") {
            return ExecResult {
                statement: "pragma",
                columns: vec!["integrity_check".to_string()],
                rows: vec![vec![Cell::Text(error.to_string())]],
                affected: 0,
            };
        }
        let mut problems = Vec::new();
        let pages = self.writer.pages();
        let limits = TableLimits::default();
        match validate_database(pages) {
            Ok(_) => {
                let objects = match self.writer.schema_objects(limits) {
                    Ok(objects) => objects,
                    Err(error) => {
                        problems.push(format!("schema: {error}"));
                        Vec::new()
                    }
                };
                for object in &objects {
                    let Some(root) = object.root_page.filter(|root| *root != 0)
                    else {
                        continue;
                    };
                    let outcome = match object.kind {
                        SchemaObjectKind::Index => {
                            let collations = self
                                .catalog
                                .indexes
                                .get(&object.name.to_ascii_lowercase())
                                .map(|index| index.collations.clone())
                                .unwrap_or_default();
                            read_index_table_with(pages, root, limits, &collations).map(|_| ())
                        }
                        SchemaObjectKind::Table if self.table_is_without_rowid(&object.name) => {
                            let collations = match self.catalog.tables.iter().find(|def| def.name.eq_ignore_ascii_case(&object.name)) {
                                Some(def) => {
                                    let layout = def.without_rowid_layout();
                                    layout[..def.primary_key_positions().map(|keys| keys.len()).unwrap_or(0)]
                                        .iter()
                                        .map(|position| def.columns[*position].collate.unwrap_or_default())
                                        .collect()
                                }
                                None => Vec::new(),
                            };
                            read_index_table_with(pages, root, limits, &collations).map(|_| ())
                        }
                        SchemaObjectKind::Table => read_table(pages, root, limits).map(|_| ()),
                        SchemaObjectKind::View | SchemaObjectKind::Trigger => Ok(()),
                    };
                    if let Err(error) = outcome {
                        let kind = match object.kind {
                            SchemaObjectKind::Table => "table",
                            SchemaObjectKind::Index => "index",
                            SchemaObjectKind::View => "view",
                            SchemaObjectKind::Trigger => "trigger",
                        };
                        problems.push(format!("{kind} `{}`: {error}", object.name));
                    }
                }
                self.collect_freelist_problems(pages, &mut problems);
            }
            Err(error) => problems.push(error.to_string()),
        }
        let limit = argument.trim().parse::<usize>().ok().filter(|limit| *limit > 0).unwrap_or(100);
        let rows = if problems.is_empty() {
            vec![vec![Cell::Text("ok".to_string())]]
        }
        else {
            problems.iter().take(limit).map(|problem| vec![Cell::Text(problem.clone())]).collect()
        };
        ExecResult { statement: "pragma", columns: vec!["integrity_check".to_string()], rows, affected: 0 }
    }

    /// Matches a schema table name against the catalog to know whether its
    /// root is a table b-tree or a primary-key index b-tree.
    fn table_is_without_rowid(&self, name: &str) -> bool {
        self.catalog.tables.iter().any(|def| def.name.eq_ignore_ascii_case(name) && def.without_rowid)
    }

    /// Replays the freelist chain and checks that the trunk pages and leaves
    /// account for exactly the count declared in the file header.
    fn collect_freelist_problems(&self, pages: &[u8], problems: &mut Vec<String>) {
        let page_size = self.writer.page_size() as usize;
        if page_size == 0 || pages.len() < page_size {
            return;
        }
        let page_count = pages.len() / page_size;
        let declared = database_formats::sqlite::read_freelist_count(pages) as usize;
        let mut trunk = database_formats::sqlite::read_freelist_trunk(pages);
        let mut seen = BTreeSet::new();
        let mut total = 0usize;
        while trunk != 0 {
            let index = trunk as usize;
            if index == 0 || index > page_count {
                problems.push(format!("freelist trunk page {trunk} is out of range"));
                return;
            }
            if !seen.insert(trunk) {
                problems.push(format!("freelist trunk page {trunk} is repeated"));
                return;
            }
            total += 1;
            let start = (index - 1) * page_size;
            let page = &pages[start..start + page_size];
            let next = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
            let leaves = u32::from_be_bytes([page[4], page[5], page[6], page[7]]) as usize;
            let capacity = page_size / 4 - 2;
            if leaves > capacity {
                problems.push(format!("freelist trunk page {trunk} declares {leaves} leaves beyond capacity {capacity}"));
                return;
            }
            for leaf_index in 0..leaves {
                let offset = 8 + leaf_index * 4;
                let leaf = u32::from_be_bytes([page[offset], page[offset + 1], page[offset + 2], page[offset + 3]]) as usize;
                if leaf == 0 || leaf > page_count {
                    problems.push(format!("freelist leaf page {leaf} on trunk {trunk} is out of range"));
                    continue;
                }
                if !seen.insert(leaf as u32) {
                    problems.push(format!("freelist leaf page {leaf} is repeated"));
                    continue;
                }
                total += 1;
            }
            trunk = next;
        }
        if total != declared {
            problems.push(format!("freelist declares {declared} pages but the chain holds {total}"));
        }
    }

    /// `EXPLAIN [QUERY PLAN]` reports this engine's own execution shape.
    ///
    /// The column headers follow SQLite, but the rows describe this tree-walking
    /// interpreter, not a SQLite VDBE program. Opcode names are this engine's
    /// steps, so the rows are not comparable with the real engine's disassembly.
    /// `EXPLAIN QUERY PLAN` reports the accesses that will really run, which is
    /// a scan whenever this engine has no index-driven lookup path.
    fn explain_statement(&mut self, query_plan: bool, statement: &Statement) -> Result<ExecResult, SqlError> {
        self.explain_check_tables(statement, &BTreeSet::new())?;
        let (columns, rows) = if query_plan {
            (vec!["id".to_string(), "parent".to_string(), "notused".to_string(), "detail".to_string()], self.explain_query_plan(statement))
        }
        else {
            (
                vec![
                    "addr".to_string(),
                    "opcode".to_string(),
                    "p1".to_string(),
                    "p2".to_string(),
                    "p3".to_string(),
                    "p4".to_string(),
                    "p5".to_string(),
                    "comment".to_string(),
                ],
                self.explain_program(statement),
            )
        };
        Ok(ExecResult { statement: "explain", columns, rows, affected: 0 })
    }

    /// `EXPLAIN` still resolves a missing source or target table, matching the
    /// prepare-time check the real engine performs. Column resolution is left
    /// to the statement itself because the program is not executed.
    fn explain_check_tables(&self, statement: &Statement, ctes: &BTreeSet<String>) -> Result<(), SqlError> {
        match statement {
            Statement::Select { with, first, compounds, .. } => {
                let mut scope = ctes.clone();
                for entry in with {
                    scope.insert(entry.name.to_ascii_lowercase());
                }
                for table in first.from.iter().chain(compounds.iter().flat_map(|term| term.core.from.iter())) {
                    self.explain_check_source(table, &scope)?;
                }
                Ok(())
            }
            Statement::Insert { table, query, .. } => {
                if !self.explain_table_known(table) {
                    return Err(SqlError::new("unknown-table", format!("no such table: {table}")));
                }
                match query {
                    Some(query) => self.explain_check_tables(query, ctes),
                    None => Ok(()),
                }
            }
            Statement::Update { table, from, .. } => {
                if !self.explain_table_known(table) {
                    return Err(SqlError::new("unknown-table", format!("no such table: {table}")));
                }
                for source in from {
                    self.explain_check_source(source, ctes)?;
                }
                Ok(())
            }
            Statement::Delete { table, .. } => {
                if self.explain_table_known(table) {
                    Ok(())
                }
                else {
                    Err(SqlError::new("unknown-table", format!("no such table: {table}")))
                }
            }
            Statement::Explain { statement, .. } => self.explain_check_tables(statement, ctes),
            _ => Ok(()),
        }
    }

    /// One `FROM` source: a derived table recurses into its own query, a named
    /// table must exist in a schema that the statement can see.
    fn explain_check_source(&self, table: &TableRef, scope: &BTreeSet<String>) -> Result<(), SqlError> {
        if let Some(query) = &table.query {
            return self.explain_check_tables(query, scope);
        }
        if let Some(schema) = &table.schema {
            let schema = schema.to_ascii_lowercase();
            if schema != "main" && schema != "temp" && self.attached_lookup.contains_key(&schema) {
                return Ok(());
            }
        }
        if scope.contains(&table.name.to_ascii_lowercase()) || self.explain_table_known(&table.name) {
            Ok(())
        }
        else {
            Err(SqlError::new("unknown-table", format!("no such table: {}", table.name)))
        }
    }

    /// Whether a name resolves to a table, view or system catalog table.
    fn explain_table_known(&self, name: &str) -> bool {
        let key = name.to_ascii_lowercase();
        self.catalog.table_lookup.contains_key(&key)
            || self.catalog.temp_lookup.contains_key(&key)
            || self.catalog.views.contains_key(&key)
            || self.catalog.temp_views.contains_key(&key)
            || is_schema_table(name)
            || SystemTable::from_name(name).is_some()
            || key == "sqlite_sequence"
    }

    /// Planner rows in SQLite's four-column shape. Ids are allocated here in
    /// visit order and `notused` stays zero because the real value is an
    /// internal bitmap, not an address this engine can produce.
    fn explain_query_plan(&self, statement: &Statement) -> Vec<Vec<Cell>> {
        let mut rows = Vec::new();
        let mut next_id = 1_i64;
        self.explain_plan_rows(statement, &mut rows, &mut next_id);
        rows
    }

    fn explain_plan_rows(&self, statement: &Statement, rows: &mut Vec<Vec<Cell>>, next_id: &mut i64) {
        match statement {
            Statement::Select { first, compounds, order, .. } => {
                self.explain_plan_core(first, rows, next_id);
                for term in compounds {
                    self.explain_plan_core(&term.core, rows, next_id);
                }
                if !first.group.is_empty() {
                    push_plan_row(rows, next_id, "USE TEMP B-TREE FOR GROUP BY");
                }
                if first.distinct {
                    push_plan_row(rows, next_id, "USE TEMP B-TREE FOR DISTINCT");
                }
                if !order.is_empty() {
                    push_plan_row(rows, next_id, "USE TEMP B-TREE FOR ORDER BY");
                }
            }
            Statement::Insert { query: Some(query), .. } => self.explain_plan_rows(query, rows, next_id),
            Statement::Update { table, from, .. } => {
                push_plan_row(rows, next_id, &format!("SCAN {table}"));
                for source in from {
                    push_plan_row(rows, next_id, &format!("SCAN {}", table_ref_label(source)));
                }
            }
            Statement::Delete { table, .. } => {
                push_plan_row(rows, next_id, &format!("SCAN {table}"));
            }
            Statement::Explain { statement, .. } => self.explain_plan_rows(statement, rows, next_id),
            _ => {}
        }
    }

    fn explain_plan_core(&self, core: &SelectCore, rows: &mut Vec<Vec<Cell>>, next_id: &mut i64) {
        if core.from.is_empty() {
            push_plan_row(rows, next_id, "SCAN CONSTANT ROW");
        }
        for table in &core.from {
            push_plan_row(rows, next_id, &format!("SCAN {}", table_ref_label(table)));
        }
    }

    /// Stepping program in SQLite's eight-column shape. This engine evaluates a
    /// statement tree, so the rows are the coarse steps it takes: open the
    /// sources, loop, produce a row, and close.
    fn explain_program(&self, statement: &Statement) -> Vec<Vec<Cell>> {
        let mut steps: Vec<ProgramStep> = Vec::new();
        match statement {
            Statement::Select { first, compounds, order, .. } => {
                let mut cursors = 0_i64;
                let mut sources: Vec<&TableRef> = first.from.iter().collect();
                for term in compounds {
                    sources.extend(term.core.from.iter());
                }
                for source in sources {
                    steps.push(ProgramStep::read(cursors, table_ref_label(source)));
                    cursors += 1;
                }
                if cursors > 0 {
                    steps.push(ProgramStep::plain("Rewind", 0));
                    steps.push(ProgramStep::plain("ResultRow", 0));
                    steps.push(ProgramStep::next(0));
                    for cursor in 0..cursors {
                        steps.push(ProgramStep::plain("Close", cursor));
                    }
                }
                else {
                    steps.push(ProgramStep::plain("ResultRow", 0));
                }
                if !order.is_empty() {
                    steps.push(ProgramStep::plain("Sort", 0));
                }
            }
            Statement::Insert { table, query, .. } => {
                if let Some(query) = query {
                    let _ = query;
                    steps.push(ProgramStep::plain("OpenRead", 0));
                    steps.push(ProgramStep::plain("Rewind", 0));
                    steps.push(ProgramStep::plain("Next", 0));
                }
                steps.push(ProgramStep::write(table));
                steps.push(ProgramStep::plain("Insert", 0));
                steps.push(ProgramStep::plain("Close", 0));
            }
            Statement::Update { table, .. } | Statement::Delete { table, .. } => {
                steps.push(ProgramStep::write(table));
                steps.push(ProgramStep::plain("Rewind", 0));
                steps.push(ProgramStep::plain("Next", 0));
                steps.push(ProgramStep::plain("Close", 0));
            }
            Statement::Transaction { .. } => steps.push(ProgramStep::plain("Transaction", 0)),
            Statement::Pragma { name, .. } => steps.push(ProgramStep::named("Pragma", name)),
            Statement::Attach { name, .. } => steps.push(ProgramStep::named("Attach", name)),
            Statement::Detach { name } => steps.push(ProgramStep::named("Detach", name)),
            Statement::Explain { statement, .. } => steps.push(ProgramStep::named("Explain", statement.name())),
            _ => steps.push(ProgramStep::named("ParseSchema", statement.name())),
        }
        let halt = steps.len() as i64 + 1;
        let mut rows = Vec::with_capacity(steps.len() + 2);
        rows.push(program_row(0, "Init", 0, halt, 0, None, 0));
        for (index, step) in steps.iter().enumerate() {
            rows.push(program_row(index as i64 + 1, step.opcode, step.p1, step.p2, 0, step.p4.as_deref(), step.p5));
        }
        rows.push(program_row(halt, "Halt", 0, 0, 0, None, 0));
        rows
    }

    /// Replays the stored `CREATE TABLE` / `CREATE INDEX` text of every
    /// schema row into the in-memory catalog without touching storage.
    fn load_catalog(&mut self) -> Result<(), SqlError> {
        let objects = self.writer.schema_objects(TableLimits::default()).map_err(storage_error)?;
        self.catalog.load_schema(&objects)
    }

    /// Executes exactly one statement.
    pub fn execute_one(&mut self, statement: &str) -> Result<ExecResult, SqlError> {
        let statements = self.parse(statement)?;
        match statements.len() {
            1 => {
                let only = statements.into_iter().next().expect("length checked");
                self.run_triggered(only)
            }
            0 => Err(SqlError::new("empty-statement", "expected exactly one statement but found 0")),
            count => Err(SqlError::new("multiple-statements", format!("expected exactly one statement but found {count}"))),
        }
    }

    /// Executes a list of statements in order and stops at the first rejection.
    pub fn execute_batch(&mut self, statements: &[String]) -> Result<Vec<ExecResult>, SqlError> {
        let mut results = Vec::with_capacity(statements.len());
        for statement in statements {
            results.push(self.execute_one(statement)?);
        }
        Ok(results)
    }

    /// Executes a semicolon-separated script and returns the results in order.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<ExecResult>, SqlError> {
        let statements = self.parse(sql)?;
        let mut results = Vec::with_capacity(statements.len());
        for statement in statements {
            results.push(self.run_triggered(statement)?);
        }
        Ok(results)
    }

    /// Executes a semicolon-separated script with positional parameter values
    /// bound to `?`, `?NNN` and `:name` placeholders.
    pub fn execute_with_parameters(&mut self, sql: &str, parameters: &[Cell]) -> Result<Vec<ExecResult>, SqlError> {
        self.parameters = parameters.to_vec();
        let result = self.execute(sql);
        self.parameters.clear();
        result
    }

    /// 执行一条已经解析好的语句,并绑定零基位置参数。
    ///
    /// 已解析的 AST 就是本引擎的"预编译"形态:宿主解析一次、反复执行同一
    /// 语句时不再重新词法或重新解析 SQL 文本。触发器体照常经过
    /// [`Engine::run_triggered`],与文本入口的执行路径一致。
    pub fn execute_statement(&mut self, statement: &Statement, parameters: &[Cell]) -> Result<ExecResult, SqlError> {
        // 复用参数缓冲:预编译语句高频执行时不再每次重新分配一份参数向量。
        self.parameters.clear();
        self.parameters.extend_from_slice(parameters);
        let result = self.run_triggered(statement.clone());
        self.parameters.clear();
        result
    }

    /// 按本引擎的宿主函数表解析一段分号分隔的脚本。
    ///
    /// 与 [`crate::sql::parse`] 的唯一区别:折叠名命中宿主注册表且元数相符
    /// 的函数调用成为 [`Expr::HostFunction`] 节点,同名同元数的内建派发被
    /// 替换。宿主在 prepare 之前注册即可让语句通过解析;注册发生在某条
    /// 已解析语句之后时,该语句里的宿主调用在执行期按当时的注册表求值,
    /// 未注册的名字以 `unknown-function` 如实拒绝。
    pub fn parse(&self, sql: &str) -> Result<Vec<Statement>, SqlError> {
        parse_with_host_functions(sql, Some(&self.host_functions))
    }

    /// 注册一个宿主自定义标量函数,形态取自 `database-udf` 合同。
    ///
    /// `definition` 是合同的 [`HostMicroDefinition`] 元数据(身份名、签名、
    /// 策略、不透明句柄、指纹),`adapter` 是宿主运行时中真实函数注册表的
    /// 适配器;注册前按合同校验元数据,不合合同的定义以
    /// `udf-invalid-definition` 拒绝。派发名按 ASCII 小写折叠,与内建函数
    /// 同一口径;折叠同名再次注册替换旧实现(JS API 的注册语义),折叠名
    /// 与内建函数同名同元数时宿主实现优先,与 SQLite
    /// `sqlite3_create_function` 替换内建的真实行为一致。
    ///
    /// 注册同时传播到已附加库的引擎,并对之后 `ATTACH` 的库继续生效,
    /// 使宿主函数在跨库语句与附加库视图展开里同样可求值。
    ///
    /// 合同是 Phase-1 标量形态,没有 step / final 聚合语义:宿主聚合与
    /// 宿主窗口函数不接入,`OVER` / `FILTER` / `DISTINCT` 形态在解析期以
    /// 明确错误拒绝。
    pub fn register_host_function(&mut self, definition: HostMicroDefinition, adapter: Arc<dyn HostRuntimeAdapter>) -> Result<(), SqlError> {
        let implementation = HostFunctionTable::build(definition, adapter)?;
        self.host_functions.install(implementation.clone());
        for attached in &mut self.attached {
            attached.engine.host_functions.install(implementation.clone());
        }
        Ok(())
    }

    /// 当前会话注册的宿主函数名(折叠小写,字典序)。
    pub fn host_function_names(&self) -> Vec<String> {
        self.host_functions.names()
    }

    /// `PRAGMA synchronous` 的透传:`false` 时落盘跳过持久化刷新。
    pub fn set_synchronous(&mut self, on: bool) {
        self.writer.set_synchronous(on);
    }

    /// Current database image, suitable for writing to a `.sqlite3` file.
    ///
    /// Index b-trees are rebuilt before the image is handed out, so the exported
    /// bytes stay consistent with their `sqlite_master` entries without calling
    /// [`SqliteWriter::save`]. Inside an explicit transaction the rebuild of the
    /// writes made so far happens here, which is why the receiver is mutable and
    /// the rebuild can fail.
    pub fn snapshot(&mut self) -> Result<&[u8], SqlError> {
        self.flush_pending_materialization("snapshot")?;
        Ok(self.writer.pages())
    }

    /// 写完一条语句后标记索引 b-tree 需要重建。
    ///
    /// 索引 b-tree 只能整树重建,单条语句重建一次会让事务内第 n 条写入付掉
    /// O(n) 的重建成本,所以任何写语句都只置位。重建推迟到真正读取镜像的
    /// 入口(提交不再重建,`snapshot` / `save` / `VACUUM` /
    /// `PRAGMA integrity_check` / 惰性游标打开),这些入口都会先调用
    /// [`Engine::flush_pending_materialization`],因此读到的镜像仍然与内存
    /// 键集一致。
    fn materialize_indexes_after_write(&mut self, _statement: &'static str) -> Result<(), SqlError> {
        self.pending_indexes = true;
        Ok(())
    }

    /// 同 [`Engine::materialize_indexes_after_write`],针对 WITHOUT ROWID 表的聚簇树。
    ///
    /// 与索引键集不同,`WITHOUT ROWID` 表的行只存在于这份聚簇树里,普通
    /// `SELECT` 会直接读镜像页。因此这里立即重建,不留延迟标记,否则同一条
    /// 连接里写完再读会看到空表。
    fn materialize_without_rowid_after_write(&mut self, statement: &'static str) -> Result<(), SqlError> {
        self.pending_without_rowid = true;
        self.flush_pending_materialization(statement)
    }

    /// 把推迟的索引 / 聚簇树重建落进当前镜像。
    ///
    /// 只在事务提交、写盘或读取镜像时调用;重建完成后清位,失败则保留标记让
    /// 调用方还能重试或回滚。
    fn flush_pending_materialization(&mut self, statement: &'static str) -> Result<(), SqlError> {
        if self.pending_indexes {
            self.writer.materialize_indexes().map_err(|error| storage_failure(error).in_statement(statement))?;
            self.pending_indexes = false;
        }
        if self.pending_without_rowid {
            self.writer.materialize_without_rowid().map_err(|error| storage_failure(error).in_statement(statement))?;
            self.pending_without_rowid = false;
        }
        Ok(())
    }

    /// Rowid of the last row this connection inserted successfully, mirroring
    /// `sqlite3_last_insert_rowid`.
    ///
    /// A statement that inserts several rows reports the last row it wrote. If
    /// a later row fails and the statement rolls back, the value stays at the
    /// last row that did pass, because SQLite records it row by row. Statements
    /// that insert no row leave it untouched.
    pub fn last_insert_rowid(&self) -> i64 {
        self.last_insert_rowid
    }

    /// Whether an explicit transaction or savepoint scope is open.
    pub fn in_transaction(&self) -> bool {
        self.transaction.is_some()
    }

    /// Flushes the current database image to `path`.
    ///
    /// Reads of an existing file go through [`Engine::from_writer`]; this is
    /// the matching write-back so a file-backed session survives reopening.
    pub fn save(&mut self, path: impl AsRef<Path>) -> Result<(), SqlError> {
        self.flush_pending_materialization("save")?;
        self.writer.save(path).map_err(|error| storage_failure(error))
    }

    /// Flushes the current database image to `path` without ever exposing a
    /// half-written file, by renaming a complete sibling temporary file over
    /// it.
    pub fn save_atomic(&mut self, path: impl AsRef<Path>) -> Result<(), SqlError> {
        self.flush_pending_materialization("save")?;
        self.writer.save_atomic(path).map_err(|error| storage_failure(error))
    }

    /// Configured page size.
    pub fn page_size(&self) -> u32 {
        self.writer.page_size() as u32
    }

    /// Format-3 library version stamped in the image header.
    pub fn sqlite_version(&self) -> Result<String, SqlError> {
        decode_library_version(self.writer.pages()).map_err(|error| storage_failure(error))
    }

    /// Materializes one reserved table with the labels and row order SQLite
    /// reports. `sqlite_master` is read straight from the writer's schema
    /// b-tree, so `type`, `sql`, `rootpage`, `rowid` and ordering are the
    /// on-disk values rather than a second copy that could drift.
    fn system_rows(&self, system: SystemTable, name: &str) -> Result<(Vec<String>, Vec<Vec<Cell>>, bool), SqlError> {
        let master_labels = || vec!["type".to_string(), "name".to_string(), "tbl_name".to_string(), "rootpage".to_string(), "sql".to_string()];
        match system {
            SystemTable::Master => {
                let objects = self.writer.schema_objects_with_rowids(TableLimits::default()).map_err(storage_error)?;
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
                    .schema_objects(TableLimits::default())
                    .map_err(storage_error)?
                    .iter()
                    .any(|object| object.kind == SchemaObjectKind::Table && object.name.eq_ignore_ascii_case(name));
                if !present {
                    return Err(SqlError::new("unknown-table", format!("no such table: {name}")).in_statement("select"));
                }
                let stored = self.writer.table_rows("sqlite_sequence", TableLimits::default()).map_err(storage_error)?;
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

    /// Runs one statement and fires the row triggers attached to its target.
    ///
    /// A DML statement queues one event per affected row; the events are
    /// drained after the statement's writes are in place, in SQLite's
    /// `BEFORE` / `AFTER` order. Trigger bodies run through this same entry
    /// point, so a body statement still fires the other triggers. With
    /// `recursive_triggers` off, a trigger already on the stack is skipped.
    fn run_triggered(&mut self, statement: Statement) -> Result<ExecResult, SqlError> {
        // 全库没有任何触发器时不可能排入事件,语句不需要为「触发器失败回滚」
        // 额外开一层写事务;直接走常规语句路径。
        if self.catalog.triggers.is_empty() {
            return self.run_statement(statement);
        }
        let fired = match &statement {
            Statement::Insert { schema, table, .. } => Some((schema.as_deref(), table.clone())),
            Statement::Update { schema, table, .. } => Some((schema.as_deref(), table.clone())),
            Statement::Delete { schema, table, .. } => Some((schema.as_deref(), table.clone())),
            _ => None,
        };
        let Some((schema, table)) = fired
        else {
            return self.run_statement(statement);
        };
        if !self.target_is_local(schema, &table) {
            return self.run_statement(statement);
        }
        let saved_events = std::mem::take(&mut self.pending_events);
        // A trigger that fails has to undo the statement that fired it, like
        // SQLite's per-statement rollback, so the whole statement runs inside
        // one writer level owned here.
        self.writer.begin().map_err(|error| storage_failure(error).in_statement("trigger"))?;
        let outcome = match self.run_statement(statement) {
            Ok(result) => {
                let events = std::mem::take(&mut self.pending_events);
                let mut failure = None;
                for event in events {
                    if let Err(error) = self.fire_event(event) {
                        failure = Some(error);
                        break;
                    }
                }
                match failure {
                    Some(error) => {
                        let _ = self.writer.rollback();
                        Err(error)
                    }
                    None => {
                        self.writer.commit().map_err(|error| storage_failure(error).in_statement("trigger"))?;
                        Ok(result)
                    }
                }
            }
            Err(error) => {
                let _ = self.writer.rollback();
                Err(error)
            }
        };
        self.pending_events = saved_events;
        outcome
    }

    /// Whether an unqualified or schema-qualified target table lives in one of
    /// the schemas this engine owns; an attached target runs in its own engine
    /// and must not fire the triggers of `main`.
    fn target_is_local(&self, schema: Option<&str>, table: &str) -> bool {
        let key = table.to_ascii_lowercase();
        match schema {
            Some(qualifier) => {
                let lower = qualifier.to_ascii_lowercase();
                lower == "main" || lower == "temp" || self.alias.as_deref().is_some_and(|alias| alias.eq_ignore_ascii_case(qualifier))
            }
            None => {
                self.catalog.temp_lookup.contains_key(&key)
                    || self.catalog.table_lookup.contains_key(&key)
                    || self.catalog.views.contains_key(&key)
                    || self.attached.is_empty()
            }
        }
    }

    /// Fires the triggers one queued row event matches.
    fn fire_event(&mut self, event: PendingTriggerEvent) -> Result<(), SqlError> {
        let statement = event_statement(&event.event);
        if self.recursive_triggers && self.active_triggers.len() >= MAX_TRIGGER_DEPTH {
            return Err(SqlError::new("trigger-depth", "too many levels of trigger recursion").in_statement(statement));
        }
        let frame = TriggerFrame { columns: event.columns, new: event.new, old: event.old };
        self.trigger_frames.push(frame);
        let outcome = self.fire_matching(&event.table, &event.event, event.timing);
        self.trigger_frames.pop();
        outcome
    }

    /// Runs every trigger of `table` whose event and timing match, newest
    /// first, which is the order SQLite uses for triggers of the same event.
    fn fire_matching(&mut self, table: &str, event: &TriggerEvent, timing: TriggerTiming) -> Result<(), SqlError> {
        let mut matched: Vec<(String, TriggerDef)> = self
            .catalog
            .triggers
            .iter()
            .filter(|(_, trigger)| trigger.table == table && trigger.timing == timing && trigger_event_matches(&trigger.event, event))
            .map(|(key, trigger)| (key.clone(), trigger.clone()))
            .collect();
        matched.sort_by(|left, right| right.1.order.cmp(&left.1.order));
        let statement = event_statement(event);
        for (key, trigger) in matched {
            if !self.recursive_triggers && self.active_triggers.contains(&key) {
                continue;
            }
            if self.recursive_triggers && self.active_triggers.len() >= MAX_TRIGGER_DEPTH {
                return Err(SqlError::new("trigger-depth", "too many levels of trigger recursion").in_statement(statement));
            }
            if let Some(when) = &trigger.when {
                match self.trigger_when_holds(when) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => return Err(error),
                }
            }
            self.active_triggers.push(key);
            let mut outcome = Ok(());
            for statement in trigger.body.clone() {
                outcome = self.run_triggered(statement).map(|_| ());
                if outcome.is_err() {
                    break;
                }
            }
            self.active_triggers.pop();
            outcome?;
        }
        Ok(())
    }

    /// Evaluates one trigger `WHEN` predicate against the current frame.
    fn trigger_when_holds(&self, when: &Expr) -> Result<bool, SqlError> {
        let relation =
            Relation { sources: Vec::new(), using: Vec::new(), using_slots: Vec::new(), trigger: self.trigger_frames.last(), excluded: false };
        let shape = relation.shape();
        let mut aggregates = Vec::new();
        let mut windows = Vec::new();
        let resolved = resolve_expr(when, &relation, &[], &[], &mut windows, &mut aggregates, false, false)?;
        let budget = Budget::new();
        let ctx = EvalContext {
            engine: self,
            outer: &[],
            current: &shape,
            aggregates: &[],
            windows: &[],
            window_row: 0,
            budget: &budget,
            ctes: None,
        };
        Ok(truthiness(&resolved.eval(&[], &ctx)?) == Some(true))
    }

    /// Evaluates one `VALUES` expression of a trigger body against the active
    /// `NEW` / `OLD` frame.
    fn eval_trigger_expression(&self, expression: &Expr, statement: &'static str) -> Result<Cell, SqlError> {
        let relation =
            Relation { sources: Vec::new(), using: Vec::new(), using_slots: Vec::new(), trigger: self.trigger_frames.last(), excluded: false };
        let shape = relation.shape();
        let mut aggregates = Vec::new();
        let mut windows = Vec::new();
        let resolved = resolve_expr(expression, &relation, &[], &[], &mut windows, &mut aggregates, false, false)
            .map_err(|error| error.in_statement(statement))?;
        let budget = Budget::new();
        let ctx = EvalContext {
            engine: self,
            outer: &[],
            current: &shape,
            aggregates: &[],
            windows: &[],
            window_row: 0,
            budget: &budget,
            ctes: None,
        };
        resolved.eval(&[], &ctx)
    }

    /// Whether one table or view carries at least one trigger of any kind.
    fn table_has_triggers(&self, table: &str) -> bool {
        self.catalog.triggers.values().any(|trigger| trigger.table == table)
    }

    /// Queues one `BEFORE` update event of a row.
    fn queue_update_before(
        &mut self,
        table: &str,
        def: &TableDef,
        columns: &[String],
        assigned: &[String],
        rowid: i64,
        old: &[Cell],
        new: &[Cell],
    ) {
        self.queue_update_event(table, def, columns, assigned, rowid, old, new, TriggerTiming::Before);
    }

    /// Queues one `AFTER` update event of a row.
    fn queue_update_after(
        &mut self,
        table: &str,
        def: &TableDef,
        columns: &[String],
        assigned: &[String],
        rowid: i64,
        old: &[Cell],
        new: &[Cell],
    ) {
        self.queue_update_event(table, def, columns, assigned, rowid, old, new, TriggerTiming::After);
    }

    /// Queues the `BEFORE` and `AFTER` events of one updated row.
    fn queue_update_events(
        &mut self,
        table: &str,
        def: &TableDef,
        columns: &[String],
        assigned: &[String],
        rowid: i64,
        old: &[Cell],
        new: &[Cell],
    ) {
        self.queue_update_before(table, def, columns, assigned, rowid, old, new);
        self.queue_update_after(table, def, columns, assigned, rowid, old, new);
    }

    fn queue_update_event(
        &mut self,
        table: &str,
        def: &TableDef,
        columns: &[String],
        assigned: &[String],
        rowid: i64,
        old: &[Cell],
        new: &[Cell],
        timing: TriggerTiming,
    ) {
        let new_rowid = def
            .rowid_alias
            .and_then(|alias| new.get(alias))
            .and_then(|cell| match cell {
                Cell::Integer(value) => Some(*value),
                _ => None,
            })
            .unwrap_or(rowid);
        let old = frame_cells(old, def.columns.len(), rowid);
        let new = frame_cells(new, def.columns.len(), new_rowid);
        self.pending_events.push(PendingTriggerEvent {
            table: table.to_string(),
            timing,
            event: TriggerEvent::Update(assigned.to_vec()),
            columns: columns.to_vec(),
            new: Some(new),
            old: Some(old),
        });
    }

    /// Queues one `BEFORE` delete event of a row.
    fn queue_delete_before(&mut self, table: &str, columns: &[String], rowid: i64, old: &[Cell]) {
        self.queue_delete_event(table, columns, rowid, old, TriggerTiming::Before);
    }

    /// Queues one `AFTER` delete event of a row.
    fn queue_delete_after(&mut self, table: &str, columns: &[String], rowid: i64, old: &[Cell]) {
        self.queue_delete_event(table, columns, rowid, old, TriggerTiming::After);
    }

    /// Queues the `BEFORE` and `AFTER` events of one deleted row.
    fn queue_delete_events(&mut self, table: &str, columns: &[String], rowid: i64, old: &[Cell]) {
        self.queue_delete_before(table, columns, rowid, old);
        self.queue_delete_after(table, columns, rowid, old);
    }

    fn queue_delete_event(&mut self, table: &str, columns: &[String], rowid: i64, old: &[Cell], timing: TriggerTiming) {
        let old = frame_cells(old, columns.len() - 1, rowid);
        self.pending_events.push(PendingTriggerEvent {
            table: table.to_string(),
            timing,
            event: TriggerEvent::Delete,
            columns: columns.to_vec(),
            new: None,
            old: Some(old),
        });
    }

    /// Output columns and rows of one view, used as the source of
    /// `INSTEAD OF` triggers.
    fn view_output(&self, view: &ViewDef) -> Result<(Vec<String>, Vec<Vec<Cell>>), SqlError> {
        let budget = Budget::new();
        let result = self.select_statement(&view.query, &[], &budget, None)?;
        let names = view_output_names(view, &result.columns)?;
        Ok((names, result.rows))
    }

    /// Materializes one view exactly like a `FROM` reference does: the stored
    /// query runs again, then its output names are declared or inferred.
    fn expand_view(&self, view: &ViewDef, budget: &Budget) -> Result<(Vec<String>, Vec<Vec<Cell>>), SqlError> {
        let result = self.select_statement(&view.query, &[], budget, None)?;
        let names = view_output_names(view, &result.columns)?;
        Ok((names, result.rows))
    }

    /// Runs the `INSTEAD OF` triggers of one view when it has any; a view
    /// without one keeps SQLite's non-modifiable diagnostic.
    fn view_trigger_gate(&self, table: &str, key: &str, timing: TriggerTiming, event: &TriggerEvent) -> Result<(), SqlError> {
        if self
            .catalog
            .triggers
            .values()
            .any(|trigger| trigger.table == key && trigger.timing == timing && trigger_event_matches(&trigger.event, event))
        {
            return Ok(());
        }
        Err(SqlError::new("readonly-view", format!("cannot modify {table} because it is a view")).in_statement(event_statement(event)))
    }

    /// `INSERT INTO <view>` dispatched to its `INSTEAD OF INSERT` triggers.
    fn run_view_insert(
        &mut self,
        table: &str,
        key: &str,
        columns: Option<&[String]>,
        rows: &[Vec<InsertValue>],
        default_values: bool,
    ) -> Result<ExecResult, SqlError> {
        let event = TriggerEvent::Insert;
        self.view_trigger_gate(table, key, TriggerTiming::InsteadOf, &event)?;
        let view = self.catalog.views.get(key).expect("view resolved").clone();
        let (names, _) = self.view_output(&view)?;
        let mut frames: Vec<Vec<Cell>> = Vec::with_capacity(rows.len().max(1));
        if default_values {
            frames.push(vec![Cell::Null; names.len()]);
        }
        else {
            for values in rows {
                let mut cells = vec![Cell::Null; names.len()];
                if let Some(list) = columns {
                    for (slot, name) in list.iter().enumerate() {
                        let position = names.iter().position(|column| column.eq_ignore_ascii_case(name)).ok_or_else(|| {
                            SqlError::new("unknown-column", format!("table {table} has no column named {name}")).in_statement("insert")
                        })?;
                        if let Some(value) = values.get(slot) {
                            cells[position] = self.insert_value_cell(value, "insert")?;
                        }
                    }
                }
                else {
                    for (slot, value) in values.iter().enumerate() {
                        if let Some(cell) = cells.get_mut(slot) {
                            *cell = self.insert_value_cell(value, "insert")?;
                        }
                    }
                }
                frames.push(cells);
            }
        }
        let saved = self.last_insert_rowid;
        for cells in frames {
            let frame = TriggerFrame { columns: names.clone(), new: Some(cells), old: None };
            self.trigger_frames.push(frame);
            let outcome = self.fire_matching(key, &event, TriggerTiming::InsteadOf);
            self.trigger_frames.pop();
            outcome?;
        }
        self.last_insert_rowid = saved;
        Ok(ExecResult { statement: "insert", columns: Vec::new(), rows: Vec::new(), affected: 0 })
    }

    /// Evaluates one `INSERT` value, including the trigger-body expressions.
    fn insert_value_cell(&self, value: &InsertValue, statement: &'static str) -> Result<Cell, SqlError> {
        match value {
            InsertValue::Literal(literal) => Ok(literal_cell(literal)),
            InsertValue::Parameter { index, .. } => Ok(self.parameters.get(*index).cloned().unwrap_or(Cell::Null)),
            InsertValue::Expression(expression) => self.eval_trigger_expression(expression, statement),
        }
    }

    /// `UPDATE <view>` dispatched to its `INSTEAD OF UPDATE` triggers.
    fn run_view_update(
        &mut self,
        table: &str,
        key: &str,
        alias: Option<&str>,
        assignments: &[Assignment],
        filter: Option<&Expr>,
        returning: Option<&SelectItems>,
    ) -> Result<ExecResult, SqlError> {
        let assigned: Vec<String> = assignments.iter().map(|assignment| assignment.column.clone()).collect();
        let event = TriggerEvent::Update(assigned);
        self.view_trigger_gate(table, key, TriggerTiming::InsteadOf, &event)?;
        if returning.is_some() {
            return Err(
                SqlError::new("unsupported-clause", "RETURNING on an INSTEAD OF view update is not supported yet").in_statement("update")
            );
        }
        let view = self.catalog.views.get(key).expect("view resolved").clone();
        let (names, rows) = self.view_output(&view)?;
        // A placeholder frame lets `old.` / `new.` references resolve; the
        // values are replaced row by row below.
        let placeholder = vec![Cell::Null; names.len()];
        self.trigger_frames.push(TriggerFrame { columns: names.clone(), new: Some(placeholder.clone()), old: Some(placeholder) });
        let resolved = (|| -> Result<(Option<ResolvedExpr>, Vec<(usize, ResolvedExpr)>, ScopeShape), SqlError> {
            let relation = view_relation(alias.unwrap_or(table), &names, self.trigger_frames.last());
            let shape = relation.shape();
            let mut aggregates = Vec::new();
            let mut windows = Vec::new();
            let predicate = match filter {
                Some(filter) => Some(resolve_expr(filter, &relation, &[], &[], &mut windows, &mut aggregates, false, false)?),
                None => None,
            };
            let mut targets = Vec::with_capacity(assignments.len());
            for assignment in assignments {
                let index = names.iter().position(|column| column.eq_ignore_ascii_case(&assignment.column)).ok_or_else(|| {
                    SqlError::new("unknown-column", format!("table {table} has no column named {}", assignment.column)).in_statement("update")
                })?;
                let expression = resolve_expr(&assignment.value, &relation, &[], &[], &mut windows, &mut aggregates, false, false)?;
                targets.push((index, expression));
            }
            Ok((predicate, targets, shape))
        })();
        let (predicate, targets, shape) = match resolved {
            Ok(resolved) => resolved,
            Err(error) => {
                self.trigger_frames.pop();
                return Err(error);
            }
        };
        let budget = Budget::new();
        let saved = self.last_insert_rowid;
        let mut outcome = Ok(());
        for row in rows {
            let row = row.get(..names.len()).map(|cells| cells.to_vec()).unwrap_or_else(|| vec![Cell::Null; names.len()]);
            match row_matches(&predicate, &row, self, &shape, &budget) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
            let ctx = EvalContext {
                engine: self,
                outer: &[],
                current: &shape,
                aggregates: &[],
                windows: &[],
                window_row: 0,
                budget: &budget,
                ctes: None,
            };
            let mut next = row.clone();
            for (index, expression) in &targets {
                match expression.eval(&row, &ctx) {
                    Ok(value) => next[*index] = value,
                    Err(error) => {
                        outcome = Err(error.in_statement("update"));
                        break;
                    }
                }
            }
            if outcome.is_err() {
                break;
            }
            if let Some(frame) = self.trigger_frames.last_mut() {
                frame.old = Some(row);
                frame.new = Some(next);
            }
            outcome = self.fire_matching(key, &event, TriggerTiming::InsteadOf);
            if outcome.is_err() {
                break;
            }
        }
        self.trigger_frames.pop();
        self.last_insert_rowid = saved;
        outcome?;
        Ok(ExecResult { statement: "update", columns: Vec::new(), rows: Vec::new(), affected: 0 })
    }

    /// `DELETE FROM <view>` dispatched to its `INSTEAD OF DELETE` triggers.
    fn run_view_delete(
        &mut self,
        table: &str,
        key: &str,
        alias: Option<&str>,
        filter: Option<&Expr>,
        returning: Option<&SelectItems>,
    ) -> Result<ExecResult, SqlError> {
        let event = TriggerEvent::Delete;
        self.view_trigger_gate(table, key, TriggerTiming::InsteadOf, &event)?;
        if returning.is_some() {
            return Err(
                SqlError::new("unsupported-clause", "RETURNING on an INSTEAD OF view delete is not supported yet").in_statement("delete")
            );
        }
        let view = self.catalog.views.get(key).expect("view resolved").clone();
        let (names, rows) = self.view_output(&view)?;
        let placeholder = vec![Cell::Null; names.len()];
        self.trigger_frames.push(TriggerFrame { columns: names.clone(), new: None, old: Some(placeholder) });
        let resolved = (|| -> Result<(Option<ResolvedExpr>, ScopeShape), SqlError> {
            let relation = view_relation(alias.unwrap_or(table), &names, self.trigger_frames.last());
            let shape = relation.shape();
            let mut aggregates = Vec::new();
            let mut windows = Vec::new();
            let predicate = match filter {
                Some(filter) => Some(resolve_expr(filter, &relation, &[], &[], &mut windows, &mut aggregates, false, false)?),
                None => None,
            };
            Ok((predicate, shape))
        })();
        let (predicate, shape) = match resolved {
            Ok(resolved) => resolved,
            Err(error) => {
                self.trigger_frames.pop();
                return Err(error);
            }
        };
        let budget = Budget::new();
        let saved = self.last_insert_rowid;
        let mut outcome = Ok(());
        for row in rows {
            let row = row.get(..names.len()).map(|cells| cells.to_vec()).unwrap_or_else(|| vec![Cell::Null; names.len()]);
            match row_matches(&predicate, &row, self, &shape, &budget) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
            if let Some(frame) = self.trigger_frames.last_mut() {
                frame.old = Some(row);
            }
            outcome = self.fire_matching(key, &event, TriggerTiming::InsteadOf);
            if outcome.is_err() {
                break;
            }
        }
        self.trigger_frames.pop();
        self.last_insert_rowid = saved;
        outcome?;
        Ok(ExecResult { statement: "delete", columns: Vec::new(), rows: Vec::new(), affected: 0 })
    }

    fn run_statement(&mut self, statement: Statement) -> Result<ExecResult, SqlError> {
        // 子查询备忘与已解码行集都只在一条语句的 AST 存活期内有意义。
        self.subquery_cache.borrow_mut().clear();
        self.exists_probes.borrow_mut().clear();
        self.statement_scans.borrow_mut().clear();
        self.outer_read.set(false);
        let result = self.run_statement_dispatch(statement);
        // 释放本条语句解码的整表行,避免大表在下一条语句前仍驻留内存。
        self.statement_scans.borrow_mut().clear();
        result
    }

    fn run_statement_dispatch(&mut self, statement: Statement) -> Result<ExecResult, SqlError> {
        // 读语句不动索引;`UPDATE` / `DELETE` 在自己的执行分支里按实际改动
        // 的索引精确失效(见 `invalidate_indexes_after_update` 与
        // `patch_index_probes_after_delete`),其余会改行或改 schema 的语句
        // 在这里一刀切,让相关索引的缓存整体作废。
        if !matches!(statement, Statement::Select { .. } | Statement::Explain { .. }) {
            match &statement {
                Statement::Update { .. } | Statement::Delete { .. } => {}
                // `BEGIN` / `COMMIT` 本身不改行:已缓存的索引内容仍然准确,
                // 一刀切只会让包住写入的事务丢掉增量补丁。回滚类动作相反,
                // 会撤销事务内已并入探针的改动,必须走全量失效。
                Statement::Transaction { action: TransactionAction::Begin | TransactionAction::Commit, .. } => {}
                // 只追加行的普通 `INSERT` 不改变既有索引内容:语句提交后会把新行
                // 增量并入探针,这里先全量失效只会让补丁失去对齐基准。
                Statement::Insert { schema: None, table, conflict: None, replace: false, .. } if self.insert_keeps_index_content(table) => {}
                _ => match self.statement_table_key(&statement) {
                    Some(key) => self.invalidate_table_indexes(&key),
                    None => self.invalidate_all_indexes(),
                },
            }
        }
        let Some(schema) = leading_schema(&statement).map(str::to_owned)
        else {
            if let Some(index) = self.attached_target(&statement) {
                return self.run_attached(index, statement);
            }
            return self.run_statement_inner(statement);
        };
        let lower = schema.to_ascii_lowercase();
        if self.alias.as_deref().is_some_and(|alias| alias.eq_ignore_ascii_case(&schema)) {
            let mut statement = statement;
            clear_statement_schema(&mut statement, &schema);
            return self.run_statement_inner(statement);
        }
        if lower == "main" || lower == "temp" {
            let mut statement = statement;
            clear_statement_schema(&mut statement, &schema);
            let previous = std::mem::replace(&mut self.scope, if lower == "main" { TableScope::Main } else { TableScope::Temp });
            let result = self.run_statement_inner(statement);
            self.scope = previous;
            return result;
        }
        match self.attached_lookup.get(&lower).copied() {
            Some(index) => {
                let mut statement = statement;
                clear_statement_schema(&mut statement, &schema);
                strip_statement_sql(&mut statement, &schema);
                self.run_attached(index, statement)
            }
            None => Err(unknown_schema_error(&statement, &schema)),
        }
    }

    /// The position of the attached database an unqualified statement resolves
    /// to, following SQLite's search order after temp and main miss.
    fn attached_target(&self, statement: &Statement) -> Option<usize> {
        // A trigger body resolves unqualified names in the schema of the
        // trigger, never in an attached database.
        if !self.active_triggers.is_empty() {
            return None;
        }
        let (name, kind) = match statement {
            Statement::Insert { schema: None, table, .. }
            | Statement::Update { schema: None, table, .. }
            | Statement::Delete { schema: None, table, .. }
            | Statement::DropTable { schema: None, name: table, .. }
            | Statement::AlterTable { schema: None, table, .. } => (table, TargetKind::Table),
            Statement::DropIndex { schema: None, name, .. } => (name, TargetKind::Index),
            Statement::DropView { schema: None, name, .. } => (name, TargetKind::View),
            Statement::DropTrigger { schema: None, name, .. } => (name, TargetKind::Trigger),
            _ => return None,
        };
        let key = name.to_ascii_lowercase();
        if kind == TargetKind::Table && (self.catalog.temp_lookup.contains_key(&key) || self.catalog.table_lookup.contains_key(&key)) {
            return None;
        }
        self.attached.iter().position(|attached| match kind {
            TargetKind::Table => attached.engine.catalog.table_lookup.contains_key(&key),
            TargetKind::Index => attached.engine.catalog.indexes.contains_key(&key),
            TargetKind::View => attached.engine.catalog.views.contains_key(&key),
            TargetKind::Trigger => attached.engine.catalog.triggers.contains_key(&key),
        })
    }

    /// Runs one statement inside an attached database with the parameters and
    /// the connection-wide last rowid carried over.
    fn run_attached(&mut self, index: usize, statement: Statement) -> Result<ExecResult, SqlError> {
        let parameters = self.parameters.clone();
        let attached = &mut self.attached[index];
        attached.engine.parameters = parameters;
        let result = attached.engine.run_triggered(statement);
        let rowid = attached.engine.last_insert_rowid;
        attached.engine.parameters.clear();
        let result = result?;
        self.last_insert_rowid = rowid;
        self.attached[index].dirty = true;
        if self.transaction.is_none() {
            self.persist_attached(index)?;
        }
        Ok(result)
    }

    /// Flushes one attached database image back to its file.
    fn persist_attached(&mut self, index: usize) -> Result<(), SqlError> {
        if !self.attached.get(index).is_some_and(|attached| attached.dirty) {
            return Ok(());
        }
        if let Some(path) = self.attached[index].file.clone() {
            self.attached[index].engine.save(&path)?;
        }
        self.attached[index].dirty = false;
        Ok(())
    }

    /// Flushes every attached database that changed inside a transaction.
    fn persist_attached_databases(&mut self) -> Result<(), SqlError> {
        for index in 0..self.attached.len() {
            self.persist_attached(index)?;
        }
        Ok(())
    }

    /// Table name as reported by diagnostics under the active schema scope.
    fn scoped_name(&self, table: &str) -> String {
        match self.scope {
            TableScope::Default => table.to_string(),
            TableScope::Main => format!("main.{table}"),
            TableScope::Temp => format!("temp.{table}"),
        }
    }

    /// Error for a table that the active `main.` / `temp.` qualifier cannot
    /// find; an unqualified miss keeps the existing diagnostic.
    fn table_missing(&self, table: &str, statement: &'static str) -> SqlError {
        let message = match self.scope {
            TableScope::Default => format!("unknown table `{table}`"),
            _ => format!("no such table: {}", self.scoped_name(table)),
        };
        SqlError::new("unknown-table", message).in_statement(statement)
    }

    /// `ATTACH [DATABASE] file AS name`.
    fn attach_database(&mut self, file: &str, name: &str) -> Result<ExecResult, SqlError> {
        let key = name.to_ascii_lowercase();
        if key == "main" || key == "temp" || self.attached_lookup.contains_key(&key) {
            return Err(SqlError::new("database-in-use", format!("database {name} is already in use")).in_statement("attach"));
        }
        if self.attached.len() >= 10 {
            return Err(SqlError::new("too-many-databases", "too many attached databases - max 10").in_statement("attach"));
        }
        let memory = file.is_empty() || file.eq_ignore_ascii_case(":memory:");
        let (engine, path, display) = if memory {
            (Engine::new(Some(self.page_size()))?, None, String::new())
        }
        else {
            let requested = PathBuf::from(file);
            let path = if requested.is_absolute() {
                requested.clone()
            }
            else {
                std::env::current_dir().map(|cwd| cwd.join(&requested)).unwrap_or_else(|_| requested.clone())
            };
            let writer = if requested.is_dir() {
                return Err(SqlError::new("unable-to-open", format!("unable to open database: {file}")).in_statement("attach"));
            }
            else if requested.exists() && std::fs::metadata(&requested).map(|meta| meta.len() > 0).unwrap_or(false) {
                SqliteWriter::open(&requested).map_err(|_| SqlError::new("not-a-database", "file is not a database").in_statement("attach"))?
            }
            else {
                // SQLite creates the file at ATTACH time but leaves it empty
                // until the first change.
                if let Some(parent) = requested.parent() {
                    if !parent.as_os_str().is_empty() && !parent.exists() {
                        return Err(SqlError::new("unable-to-open", format!("unable to open database: {file}")).in_statement("attach"));
                    }
                }
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&requested)
                    .map_err(|_| SqlError::new("unable-to-open", format!("unable to open database: {file}")))?;
                SqliteWriter::create(self.page_size() as usize).map_err(|error| storage_failure(error))?
            };
            let engine = Engine::from_writer(writer)?;
            (engine, Some(path.clone()), path.to_string_lossy().into_owned())
        };
        let mut engine = Box::new(engine);
        engine.alias = Some(name.to_string());
        // 宿主函数在本会话注册一次后对之后 `ATTACH` 的库同样生效;表内只存放
        // `Arc` 句柄,克隆不会复制宿主实现。
        engine.host_functions = self.host_functions.clone();
        let index = self.attached.len();
        self.attached.push(AttachedDb { name: name.to_string(), file: path, display, engine, dirty: false });
        self.attached_lookup.insert(key, index);
        Ok(ExecResult { statement: "attach", columns: Vec::new(), rows: Vec::new(), affected: 0 })
    }

    /// `DETACH [DATABASE] name`.
    fn detach_database(&mut self, name: &str) -> Result<ExecResult, SqlError> {
        let key = name.to_ascii_lowercase();
        if key == "main" || key == "temp" {
            return Err(SqlError::new("cannot-detach", format!("cannot detach database {name}")).in_statement("detach"));
        }
        let Some(index) = self.attached_lookup.get(&key).copied()
        else {
            return Err(SqlError::new("unknown-database", format!("no such database: {name}")).in_statement("detach"));
        };
        self.persist_attached(index)?;
        self.attached.remove(index);
        self.attached_lookup.clear();
        for (position, attached) in self.attached.iter().enumerate() {
            self.attached_lookup.insert(attached.name.to_ascii_lowercase(), position);
        }
        Ok(ExecResult { statement: "detach", columns: Vec::new(), rows: Vec::new(), affected: 0 })
    }

    /// `VACUUM [schema] [INTO target]`.
    ///
    /// 重建与上游 `VACUUM` 同形:按 `sqlite_master` 的顺序重放表结构,复制行,
    /// 再重建索引、视图与触发器。新镜像没有 freelist 页,并保留页大小、文本
    /// 编码、`user_version` 与 `application_id`。不带 `INTO` 时新镜像替换
    /// 当前页集合;带 `INTO` 时写到目标文件,当前库保持不变。
    ///
    /// 与上游一致的边界:`VACUUM`/`VACUUM INTO` 在事务内拒绝,`VACUUM` 也不能
    /// 出现在触发器体里(解析期按上游报 `near "vacuum": syntax error`);
    /// `temp` 库的 `VACUUM` 是空操作,`VACUUM temp INTO` 写出空库;目标文件
    /// 已存在时报 `output file already exists`,目标父目录缺失或目标是目录时
    /// 报 `unable to open database`,目标不是文本时报 `non-text filename`。
    fn vacuum_statement(&mut self, into: Option<&Expr>) -> Result<ExecResult, SqlError> {
        if self.transaction.is_some() {
            return Err(SqlError::new("SQLITE_ERROR", "cannot VACUUM from within a transaction").in_statement("vacuum"));
        }
        if self.scope == TableScope::Temp {
            if let Some(expression) = into {
                let target = self.vacuum_target(expression)?;
                let empty = SqliteWriter::create(self.writer.page_size()).map_err(|error| storage_failure(error).in_statement("vacuum"))?;
                self.write_vacuum_image(&target, empty.pages())?;
            }
            return Ok(ExecResult { statement: "vacuum", columns: Vec::new(), rows: Vec::new(), affected: 0 });
        }
        let image = self.vacuum_image()?;
        if let Some(expression) = into {
            let target = self.vacuum_target(expression)?;
            self.write_vacuum_image(&target, &image)?;
        }
        else {
            let synchronous = self.writer.synchronous();
            let mut writer = SqliteWriter::from_pages(image).map_err(|error| storage_failure(error).in_statement("vacuum"))?;
            writer.set_synchronous(synchronous);
            self.writer = writer;
            self.reload_catalog()?;
            self.index_probes.borrow_mut().clear();
            self.subquery_cache.borrow_mut().clear();
            self.exists_probes.borrow_mut().clear();
            self.write_epoch.set(self.write_epoch.get() + 1);
        }
        Ok(ExecResult { statement: "vacuum", columns: Vec::new(), rows: Vec::new(), affected: 0 })
    }

    /// 求出 `VACUUM INTO` 的文件名;SQLite 要求表达式的结果是文本。
    fn vacuum_target(&self, expression: &Expr) -> Result<String, SqlError> {
        match self.eval_trigger_expression(expression, "vacuum")? {
            Cell::Text(target) => Ok(target),
            _ => Err(SqlError::new("SQLITE_ERROR", "non-text filename").in_statement("vacuum")),
        }
    }

    /// 把重建镜像写到 `target`,`VACUUM INTO` 的真实落盘步骤。
    ///
    /// 空文件名与 `:memory:` 与上游一样不产生文件,命令仍然成功。目标已存在
    /// 时不覆盖;父目录缺失、目标是目录或写入失败都报 `unable to open database`。
    fn write_vacuum_image(&self, target: &str, image: &[u8]) -> Result<(), SqlError> {
        if target.is_empty() || target.eq_ignore_ascii_case(":memory:") {
            return Ok(());
        }
        let unable = || SqlError::new("SQLITE_CANTOPEN", format!("unable to open database: {target}")).in_statement("vacuum");
        let path = PathBuf::from(target);
        if path.is_dir() || path.parent().is_some_and(|parent| !parent.as_os_str().is_empty() && !parent.exists()) {
            return Err(unable());
        }
        if path.exists() {
            return Err(SqlError::new("SQLITE_ERROR", "output file already exists").in_statement("vacuum"));
        }
        std::fs::write(&path, image).map_err(|_| unable())
    }

    /// 生成 `VACUUM` 的重建镜像。
    ///
    /// 顺序与上游一致:先建表,再复制行(触发器尚未建立,复制不会改写数据),
    /// 最后建索引、视图与触发器。`sqlite_sequence` 的高水位先于行复制写入,
    /// 之后的显式 rowid 只会上抬、不会覆盖。`rowid` 表按 rowid 升序复制,
    /// 没有 `INTEGER PRIMARY KEY` 的表因此像上游一样重新编号。
    fn vacuum_image(&mut self) -> Result<Vec<u8>, SqlError> {
        self.flush_pending_materialization("vacuum")?;
        let page_size = self.writer.page_size();
        let encoding = match u32::from_be_bytes(
            self.writer
                .pages()
                .get(56..60)
                .and_then(|marker| marker.try_into().ok())
                .ok_or_else(|| SqlError::new("storage", "sqlite header too short").in_statement("vacuum"))?,
        ) {
            2 => TextEncoding::Utf16Le,
            3 => TextEncoding::Utf16Be,
            _ => TextEncoding::Utf8,
        };
        let user_version = self.writer.user_version();
        let application_id = self.writer.application_id();
        let limits = TableLimits::default();
        let objects = self.writer.schema_objects(limits).map_err(|error| storage_failure(error).in_statement("vacuum"))?;
        let writer = SqliteWriter::create_with_encoding(page_size, encoding).map_err(|error| storage_failure(error).in_statement("vacuum"))?;
        let mut fresh = Engine::from_writer(writer)?;
        for object in &objects {
            if object.kind != SchemaObjectKind::Table || object.name.eq_ignore_ascii_case("sqlite_sequence") {
                continue;
            }
            if let Some(sql) = &object.sql {
                fresh.execute_one(sql)?;
            }
        }
        // `sqlite_sequence` 是保留表,不在主目录里,因此直接读存储层。它在
        // 源库里存在就照样出现在重建镜像里,高水位在建表后先写入。
        let sequence_present =
            objects.iter().any(|object| object.kind == SchemaObjectKind::Table && object.name.eq_ignore_ascii_case("sqlite_sequence"));
        if sequence_present {
            let fresh_present = fresh
                .writer
                .schema_objects(limits)
                .map_err(|error| storage_failure(error).in_statement("vacuum"))?
                .iter()
                .any(|object| object.kind == SchemaObjectKind::Table && object.name.eq_ignore_ascii_case("sqlite_sequence"));
            if !fresh_present {
                // 源库可能只留下一个空的 `sqlite_sequence`(最后一个 AUTOINCREMENT
                // 表被删后行没了、表还在),重建镜像要照样保留它。
                let columns: Vec<ColumnSpec> = ["name", "seq"]
                    .into_iter()
                    .map(|name| ColumnSpec {
                        name: name.to_string(),
                        declared_type: String::new(),
                        not_null: false,
                        default_value: None,
                        default_text: None,
                        primary_key: false,
                        unique: false,
                        autoincrement: false,
                        collate: None,
                    })
                    .collect();
                fresh
                    .writer
                    .create_table_with_primary_key_sql("sqlite_sequence", &columns, &[], Some("CREATE TABLE sqlite_sequence(name,seq)"))
                    .map_err(|error| storage_failure(error).in_statement("vacuum"))?;
            }
            let limits = TableLimits::default();
            for row in self.writer.table_rows("sqlite_sequence", limits).map_err(|error| storage_failure(error).in_statement("vacuum"))? {
                let values = decode_record(&row.payload, encoding, RecordLimits { max_columns: 1024, max_payload_bytes: 16 * 1024 * 1024 })
                    .map_err(|error| storage_failure(error).in_statement("vacuum"))?;
                let cells: Vec<Cell> = values
                    .iter()
                    .map(|value| match value {
                        crate::RecordValue::Null => Cell::Null,
                        crate::RecordValue::Integer(value) => Cell::Integer(*value),
                        crate::RecordValue::Real(value) => Cell::Real(*value),
                        crate::RecordValue::Text(value) => Cell::Text(value.clone()),
                        crate::RecordValue::Blob(value) => Cell::Blob(value.to_vec()),
                    })
                    .collect();
                let slots: Vec<Option<crate::WriteValue>> = cells.iter().map(|cell| Some(cell_write_value(cell))).collect();
                fresh.writer.insert("sqlite_sequence", &slots).map_err(|error| storage_failure(error).in_statement("vacuum"))?;
            }
        }
        for def in self.catalog.tables.clone() {
            if def.temp || def.name.to_ascii_lowercase().starts_with("sqlite_") {
                continue;
            }
            for (_, cells) in self.live_rows(&def, "vacuum")? {
                let slots: Vec<Option<crate::WriteValue>> = cells.iter().map(|cell| Some(cell_write_value(cell))).collect();
                fresh.writer.insert(&def.name, &slots).map_err(|error| storage_failure(error).in_statement("vacuum"))?;
            }
        }
        fresh.writer.materialize_without_rowid().map_err(|error| storage_failure(error).in_statement("vacuum"))?;
        for kind in [SchemaObjectKind::Index, SchemaObjectKind::View, SchemaObjectKind::Trigger] {
            for object in &objects {
                if object.kind != kind {
                    continue;
                }
                if let Some(sql) = &object.sql {
                    fresh.execute_one(sql)?;
                }
            }
        }
        fresh.writer.materialize_indexes().map_err(|error| storage_failure(error).in_statement("vacuum"))?;
        fresh.writer.materialize_without_rowid().map_err(|error| storage_failure(error).in_statement("vacuum"))?;
        fresh.writer.set_user_version(user_version);
        fresh.writer.set_application_id(application_id);
        Ok(fresh.writer.pages().to_vec())
    }

    /// Executes `CREATE TABLE name AS SELECT ...`.
    ///
    /// The declared columns are derived from the query output: a bare column
    /// reference copies its source column's affinity as SQLite's standard
    /// type name (`INT`, `TEXT`, `REAL`, `NUM`), and every other expression
    /// gets no declared type. Duplicate output labels are numbered `:1`, `:2`.
    fn create_table_as(&mut self, name: String, if_not_exists: bool, temp: bool, query: &Statement) -> Result<ExecResult, SqlError> {
        let key = name.to_ascii_lowercase();
        check_reserved_object_name(&name, "create-table")?;
        let temp_target = temp || self.scope == TableScope::Temp;
        if temp_target {
            if self.catalog.temp_lookup.contains_key(&key) || self.catalog.temp_views.contains_key(&key) {
                if if_not_exists {
                    return Ok(ExecResult { statement: "create-table", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                }
                return Err(SqlError::new("table-exists", format!("table {name} already exists")).in_statement("create-table"));
            }
        }
        else {
            if self.catalog.indexes.contains_key(&key) {
                return Err(SqlError::new("index-exists", format!("there is already an index named {name}")).in_statement("create-table"));
            }
            if self.catalog.views.contains_key(&key) {
                return Err(SqlError::new("view-exists", format!("there is already a view named {name}")).in_statement("create-table"));
            }
            if self.catalog.table_lookup.contains_key(&key) {
                if if_not_exists {
                    return Ok(ExecResult { statement: "create-table", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                }
                return Err(SqlError::new("table-exists", format!("table {name} already exists")).in_statement("create-table"));
            }
        }
        let budget = Budget::new();
        let selected = self.select_statement(query, &[], &budget, None)?;
        let labels = unique_output_names(&selected.columns);
        let mut types: Vec<String> = self.ctas_statement_columns(query, &BTreeMap::new())?.into_iter().map(|(_, declared)| declared).collect();
        types.resize(labels.len(), String::new());
        types.truncate(labels.len());
        let columns: Vec<ColumnSpec> = labels
            .iter()
            .zip(types)
            .map(|(name, declared_type)| ColumnSpec {
                name: name.clone(),
                declared_type,
                not_null: false,
                default_value: None,
                default_text: None,
                primary_key: false,
                unique: false,
                autoincrement: false,
                collate: None,
            })
            .collect();
        let sql = ctas_schema_sql(&name, &columns);
        let mut def = SqliteCatalog::table_entry(name.clone(), columns.clone(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), false, false)?;
        if temp_target {
            def.temp = true;
            let rootpage = match self.catalog.temp_free_pages.pop_first() {
                Some(page) => page,
                None => {
                    let page = self.catalog.temp_next_page;
                    self.catalog.temp_next_page += 1;
                    page
                }
            };
            let master_rowid = self.catalog.temp_next_rowid;
            self.catalog.temp_next_rowid += 1;
            let mut rows = BTreeMap::new();
            for row in &selected.rows {
                let slots: Vec<Option<crate::WriteValue>> = row.iter().map(|cell| Some(cell_write_value(cell))).collect();
                temp_insert_row(&def, &mut rows, &slots, &[])?;
            }
            let position = self.catalog.temp_tables.len();
            self.catalog.temp_lookup.insert(key, position);
            self.catalog.temp_tables.push(TempTable { def, sql, rootpage, master_rowid, rows });
            self.catalog.temp_created = true;
            return Ok(ExecResult { statement: "create-table", columns: Vec::new(), rows: Vec::new(), affected: 0 });
        }
        self.writer
            .create_table_with_primary_key_sql(&name, &columns, &[], Some(&sql))
            .map_err(|error| storage_failure(error).in_statement("create-table"))?;
        if !selected.rows.is_empty() {
            self.writer.begin().map_err(|error| storage_failure(error).in_statement("create-table"))?;
            for row in &selected.rows {
                let slots: Vec<Option<crate::WriteValue>> = row.iter().map(|cell| Some(cell_write_value(cell))).collect();
                if let Err(error) = self.writer.insert(&name, &slots) {
                    let _ = self.writer.rollback();
                    return Err(storage_failure(error).in_statement("create-table"));
                }
            }
            self.writer.commit().map_err(|error| storage_failure(error).in_statement("create-table"))?;
        }
        let position = self.catalog.tables.len();
        self.catalog.table_lookup.insert(key, position);
        self.catalog.tables.push(def);
        Ok(ExecResult { statement: "create-table", columns: Vec::new(), rows: Vec::new(), affected: 0 })
    }

    /// Output column labels and copied declared types of one query, used to
    /// build the schema of `CREATE TABLE ... AS SELECT`.
    fn ctas_statement_columns(
        &self,
        statement: &Statement,
        inherited: &BTreeMap<String, Vec<(String, String)>>,
    ) -> Result<Vec<(String, String)>, SqlError> {
        let Statement::Select { with, first, .. } = statement
        else {
            return Ok(Vec::new());
        };
        let mut ctes = inherited.clone();
        for entry in with {
            let mut columns = self.ctas_statement_columns(&entry.query, &ctes)?;
            if let Some(names) = &entry.columns {
                if names.len() == columns.len() {
                    for (column, name) in columns.iter_mut().zip(names) {
                        column.0 = name.clone();
                    }
                }
            }
            ctes.insert(entry.name.to_ascii_lowercase(), columns);
        }
        let mut sources: Vec<(String, Vec<(String, String)>)> = Vec::with_capacity(first.from.len());
        for table in &first.from {
            let label = table.alias.clone().unwrap_or_else(|| table.name.clone());
            sources.push((label, self.ctas_table_columns(table, &ctes)?));
        }
        let mut output = Vec::new();
        match &first.items {
            SelectItems::Named(expressions) => {
                for expression in expressions {
                    let label = ctas_expression_label(expression, &sources);
                    let declared = ctas_expression_type(expression, &sources);
                    output.push((label, declared));
                }
            }
            SelectItems::CountAll => output.push(("count(*)".to_string(), String::new())),
            SelectItems::Star => {
                for (_, columns) in &sources {
                    output.extend(columns.iter().cloned());
                }
            }
        }
        Ok(output)
    }

    /// Columns one `FROM` item exposes to the output of a CTAS query.
    fn ctas_table_columns(&self, table: &TableRef, ctes: &BTreeMap<String, Vec<(String, String)>>) -> Result<Vec<(String, String)>, SqlError> {
        if let Some(query) = &table.query {
            return self.ctas_statement_columns(query, ctes);
        }
        let name = table.name.to_ascii_lowercase();
        if let Some(columns) = ctes.get(&name) {
            return Ok(columns.clone());
        }
        if let Some(def) = self.catalog.tables.iter().find(|def| def.name.eq_ignore_ascii_case(&table.name)) {
            return Ok(def.columns.iter().map(|column| (column.name.clone(), standard_affinity_name(&column.declared_type))).collect());
        }
        if let Some(temp) = self.catalog.temp_tables.iter().find(|temp| temp.def.name.eq_ignore_ascii_case(&table.name)) {
            return Ok(temp.def.columns.iter().map(|column| (column.name.clone(), standard_affinity_name(&column.declared_type))).collect());
        }
        if let Some(view) = self.catalog.views.get(&name) {
            let mut columns = self.ctas_statement_columns(&view.query, ctes)?;
            if let Some(names) = &view.columns {
                if names.len() == columns.len() {
                    for (column, name) in columns.iter_mut().zip(names) {
                        column.0 = name.clone();
                    }
                }
            }
            return Ok(columns);
        }
        Ok(Vec::new())
    }

    fn run_statement_inner(&mut self, statement: Statement) -> Result<ExecResult, SqlError> {
        match statement {
            Statement::Explain { query_plan, statement } => self.explain_statement(query_plan, &statement),
            Statement::Transaction { action } => self.transaction(action),
            Statement::Attach { file, name } => self.attach_database(&file, &name),
            Statement::Detach { name } => self.detach_database(&name),
            Statement::Vacuum { into, .. } => self.vacuum_statement(into.as_ref()),
            Statement::CreateTable {
                schema: _,
                name,
                if_not_exists,
                columns,
                primary_key,
                table_constraints,
                checks,
                foreign_keys,
                strict,
                without_rowid,
                temp,
                query,
                sql,
            } => {
                if let Some(query) = query {
                    return self.create_table_as(name, if_not_exists, temp, &query);
                }
                let key = name.to_ascii_lowercase();
                // A WITHOUT ROWID table's writer call needs the key columns
                // whether they were declared inline or as a table constraint.
                let without_rowid_keys: Vec<String> = if primary_key.is_empty() {
                    columns.iter().filter(|column| column.primary_key).map(|column| column.name.clone()).collect()
                }
                else {
                    primary_key.clone()
                };
                check_reserved_object_name(&name, "create-table")?;
                if temp {
                    // The temp schema owns a separate namespace: a main object
                    // with the same name can coexist and temp wins in name
                    // resolution.
                    if self.catalog.temp_lookup.contains_key(&key) {
                        if if_not_exists {
                            return Ok(ExecResult { statement: "create-table", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                        }
                        return Err(SqlError::new("table-exists", format!("table {name} already exists")).in_statement("create-table"));
                    }
                    let mut def =
                        SqliteCatalog::table_entry(name, columns, primary_key, table_constraints, checks, foreign_keys, strict, without_rowid)?;
                    def.temp = true;
                    // SQLite reuses a freed temp page when one is available
                    // and otherwise extends the temp database by one page.
                    let rootpage = match self.catalog.temp_free_pages.pop_first() {
                        Some(page) => page,
                        None => {
                            let page = self.catalog.temp_next_page;
                            self.catalog.temp_next_page += 1;
                            page
                        }
                    };
                    let master_rowid = self.catalog.temp_next_rowid;
                    self.catalog.temp_next_rowid += 1;
                    let position = self.catalog.temp_tables.len();
                    self.catalog.temp_lookup.insert(key, position);
                    self.catalog.temp_tables.push(TempTable { def, sql, rootpage, master_rowid, rows: BTreeMap::new() });
                    self.catalog.temp_created = true;
                    return Ok(ExecResult { statement: "create-table", columns: Vec::new(), rows: Vec::new(), affected: 1 });
                }
                if self.catalog.indexes.contains_key(&key) {
                    return Err(SqlError::new("index-exists", format!("there is already an index named {name}")).in_statement("create-table"));
                }
                if self.catalog.views.contains_key(&key) {
                    return Err(SqlError::new("view-exists", format!("there is already a view named {name}")).in_statement("create-table"));
                }
                if self.catalog.table_lookup.contains_key(&key) {
                    if if_not_exists {
                        return Ok(ExecResult { statement: "create-table", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                    }
                    return Err(SqlError::new("table-exists", format!("table {name} already exists")).in_statement("create-table"));
                }
                let def = SqliteCatalog::table_entry(
                    name.clone(),
                    columns.clone(),
                    primary_key.clone(),
                    table_constraints.clone(),
                    checks.clone(),
                    foreign_keys.clone(),
                    strict,
                    without_rowid,
                )?;
                if without_rowid {
                    self.writer
                        .create_table_without_rowid_sql(&name, &columns, &without_rowid_keys, Some(&sql))
                        .map_err(|error| storage_failure(error).in_statement("create-table"))?;
                }
                else {
                    self.writer
                        .create_table_with_primary_key_sql(&name, &columns, &primary_key, Some(&sql))
                        .map_err(|error| storage_failure(error).in_statement("create-table"))?;
                }
                // Appending keeps the catalog in creation order, which is the
                // order `sqlite_master` reports.
                let position = self.catalog.tables.len();
                self.catalog.table_lookup.insert(key, position);
                self.catalog.tables.push(def);
                Ok(ExecResult { statement: "create-table", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            Statement::CreateView { schema, name, if_not_exists, columns, query, temp, sql } => {
                let key = name.to_ascii_lowercase();
                check_reserved_object_name(&name, "create-view")?;
                // `CREATE TEMP VIEW` and `CREATE VIEW temp.v` both target the
                // connection-owned schema, whose objects shadow main ones.
                let temp_target =
                    temp || self.scope == TableScope::Temp || schema.as_deref().is_some_and(|qualifier| qualifier.eq_ignore_ascii_case("temp"));
                if temp_target {
                    if self.catalog.temp_lookup.contains_key(&key) || self.catalog.temp_views.contains_key(&key) {
                        if if_not_exists {
                            return Ok(ExecResult { statement: "create-view", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                        }
                        return Err(SqlError::new("table-exists", format!("table {name} already exists")).in_statement("create-view"));
                    }
                    // `sqlite_temp_master` stores the statement without `TEMP`
                    // and without a `temp.` qualifier.
                    let stored = schema.as_deref().map(|qualifier| strip_schema_qualifiers(&sql, qualifier)).unwrap_or(sql);
                    let master_rowid = self.catalog.temp_next_rowid;
                    self.catalog.temp_next_rowid += 1;
                    self.catalog.temp_views.insert(key, TempView { def: ViewDef { name, columns, query: *query }, sql: stored, master_rowid });
                    self.catalog.temp_created = true;
                    return Ok(ExecResult { statement: "create-view", columns: Vec::new(), rows: Vec::new(), affected: 1 });
                }
                if self.catalog.indexes.contains_key(&key) {
                    return Err(SqlError::new("index-exists", format!("there is already an index named {name}")).in_statement("create-view"));
                }
                if self.catalog.views.contains_key(&key) {
                    if if_not_exists {
                        return Ok(ExecResult { statement: "create-view", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                    }
                    return Err(SqlError::new("view-exists", format!("view {name} already exists")).in_statement("create-view"));
                }
                if self.catalog.tables.iter().any(|def| def.name.eq_ignore_ascii_case(&name)) {
                    if if_not_exists {
                        return Ok(ExecResult { statement: "create-view", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                    }
                    return Err(SqlError::new("table-exists", format!("table {name} already exists")).in_statement("create-view"));
                }
                self.writer.create_view(&name, &sql).map_err(|error| storage_failure(error).in_statement("create-view"))?;
                self.catalog.views.insert(key, ViewDef { name, columns, query: *query });
                Ok(ExecResult { statement: "create-view", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            Statement::DropView { schema, name, if_exists } => {
                let key = name.to_ascii_lowercase();
                let temp_target =
                    self.scope == TableScope::Temp || schema.as_deref().is_some_and(|qualifier| qualifier.eq_ignore_ascii_case("temp"));
                // Unqualified `DROP VIEW` searches temp before main, so a temp
                // view disappears while its main namesake survives.
                if schema.is_none() || temp_target {
                    if self.catalog.temp_views.remove(&key).is_some() {
                        self.catalog.triggers.retain(|_, trigger| !(trigger.temp && trigger.table == key));
                        return Ok(ExecResult { statement: "drop-view", columns: Vec::new(), rows: Vec::new(), affected: 1 });
                    }
                    if temp_target {
                        if if_exists {
                            return Ok(ExecResult { statement: "drop-view", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                        }
                        return Err(SqlError::new("unknown-view", format!("no such view: {name}")).in_statement("drop-view"));
                    }
                }
                if self.catalog.tables.iter().any(|def| def.name.eq_ignore_ascii_case(&name)) {
                    return Err(SqlError::new("wrong-object", format!("use DROP TABLE to delete table {name}")).in_statement("drop-view"));
                }
                // An index is not a view, so SQLite reports it as missing
                // rather than as the wrong object kind.
                if self.catalog.indexes.contains_key(&key) {
                    if if_exists {
                        return Ok(ExecResult { statement: "drop-view", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                    }
                    return Err(SqlError::new("unknown-view", format!("no such view: {name}")).in_statement("drop-view"));
                }
                if self.catalog.views.remove(&key).is_none() {
                    if if_exists {
                        return Ok(ExecResult { statement: "drop-view", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                    }
                    return Err(SqlError::new("unknown-view", format!("no such view: {name}")).in_statement("drop-view"));
                }
                self.writer.drop_view(&name).map_err(|error| storage_failure(error).in_statement("drop-view"))?;
                self.catalog.triggers.retain(|_, trigger| trigger.table != key);
                Ok(ExecResult { statement: "drop-view", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            Statement::CreateTrigger { schema: _, name, if_not_exists, timing, event, table_schema: _, table, when, body, sql } => {
                let key = name.to_ascii_lowercase();
                check_reserved_object_name(&name, "create-trigger")?;
                if self.catalog.triggers.contains_key(&key) {
                    if if_not_exists {
                        return Ok(ExecResult { statement: "create-trigger", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                    }
                    return Err(SqlError::new("trigger-exists", format!("trigger {name} already exists")).in_statement("create-trigger"));
                }
                let table_key = table.to_ascii_lowercase();
                let temp_target = if self.scope == TableScope::Main { None } else { self.catalog.temp_lookup.get(&table_key).copied() };
                let main_target = if temp_target.is_some() { None } else { self.catalog.table_lookup.get(&table_key).copied() };
                let view_target =
                    if temp_target.is_some() || main_target.is_some() { false } else { self.catalog.views.contains_key(&table_key) };
                if temp_target.is_none() && main_target.is_none() && !view_target {
                    let qualifier = match self.scope {
                        TableScope::Temp => "temp",
                        _ => "main",
                    };
                    return Err(SqlError::new("unknown-table", format!("no such table: {qualifier}.{table}")).in_statement("create-trigger"));
                }
                let timing_text = match timing {
                    TriggerTiming::Before => "BEFORE",
                    TriggerTiming::After => "AFTER",
                    TriggerTiming::InsteadOf => "INSTEAD OF",
                };
                if view_target && !matches!(timing, TriggerTiming::InsteadOf) {
                    return Err(SqlError::new("trigger-target", format!("cannot create {timing_text} trigger on view: {table}"))
                        .in_statement("create-trigger"));
                }
                if !view_target && matches!(timing, TriggerTiming::InsteadOf) {
                    return Err(SqlError::new("trigger-target", format!("cannot create INSTEAD OF trigger on table: {table}"))
                        .in_statement("create-trigger"));
                }
                let temp = self.scope == TableScope::Temp || temp_target.is_some();
                let order = self.catalog.trigger_sequence;
                self.catalog.trigger_sequence += 1;
                let master_rowid = if temp {
                    let rowid = self.catalog.temp_next_rowid;
                    self.catalog.temp_next_rowid += 1;
                    self.catalog.temp_created = true;
                    rowid
                }
                else {
                    self.writer.create_trigger(&name, &table, &sql).map_err(|error| storage_failure(error).in_statement("create-trigger"))?;
                    0
                };
                self.catalog
                    .triggers
                    .insert(key, TriggerDef { name, table: table_key, timing, event, when, body, order, temp, sql, master_rowid });
                Ok(ExecResult { statement: "create-trigger", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            Statement::DropTrigger { schema: _, name, if_exists } => {
                let key = name.to_ascii_lowercase();
                let Some(trigger) = self.catalog.triggers.remove(&key)
                else {
                    if if_exists {
                        return Ok(ExecResult { statement: "drop-trigger", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                    }
                    return Err(SqlError::new("unknown-trigger", format!("no such trigger: {name}")).in_statement("drop-trigger"));
                };
                if !trigger.temp {
                    self.writer.drop_trigger(&trigger.name).map_err(|error| storage_failure(error).in_statement("drop-trigger"))?;
                }
                Ok(ExecResult { statement: "drop-trigger", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            Statement::Insert { schema: _, table, columns, rows, default_values, query, replace, ignore, conflict, returning } => {
                let key = table.to_ascii_lowercase();
                check_readonly_table(&table, "insert")?;
                // A `SELECT` source is evaluated before any row is written, so
                // `INSERT INTO t SELECT ... FROM t` reads the table's original
                // contents, exactly like SQLite.
                let selected_rows: Option<Vec<Vec<Cell>>> = match &query {
                    Some(query) => {
                        let budget = Budget::new();
                        Some(self.select_statement(query, &[], &budget, None)?.rows)
                    }
                    None => None,
                };
                if self.catalog.views.contains_key(&key) {
                    let literal_rows: Vec<Vec<InsertValue>> = match &selected_rows {
                        Some(rows) => rows.iter().map(|row| row.iter().map(cell_insert_value).collect()).collect(),
                        None => rows.clone(),
                    };
                    return self.run_view_insert(&table, &key, columns.as_deref(), &literal_rows, default_values);
                }
                let temp_position = if self.scope == TableScope::Main { None } else { self.catalog.temp_lookup.get(&key).copied() };
                let main_position = if self.scope == TableScope::Temp {
                    None
                }
                else {
                    match temp_position {
                        Some(_) => None,
                        None => Some(*self.catalog.table_lookup.get(&key).ok_or_else(|| self.table_missing(&table, "insert"))?),
                    }
                };
                // Temp tables are mutated in memory, so their definition is
                // cloned: the loop below needs the catalog while it borrows
                // the temp row map mutably.
                let def_storage = match temp_position {
                    Some(position) => self.catalog.temp_tables[position].def.clone(),
                    None => self.catalog.tables[main_position.expect("main table resolved")].clone(),
                };
                let def = &def_storage;
                let returning_shape = TargetShape::new(def, &table, None);
                let returning = match &returning {
                    Some(items) => Some(returning_shape.plan(def, items, "insert")?),
                    None => None,
                };
                let upsert = match &conflict {
                    Some(clause) => {
                        if replace {
                            return Err(SqlError::new("syntax", "ON CONFLICT clause is not allowed with REPLACE").in_statement("insert"));
                        }
                        Some(self.prepare_upsert(def, &key, &table, clause)?)
                    }
                    None => None,
                };
                // 只追加行的语句可以把既有索引探针按新行增量补齐,不必等下一条
                // 查询整份重建。带 `ON CONFLICT` / `REPLACE` / 触发器的语句可能
                // 改写或删除既有行,仍走常规失效。
                let watch_probes =
                    conflict.is_none() && !replace && temp_position.is_none() && !self.table_has_triggers(&key) && self.table_has_probes(&key);
                let mut probe_rows: Vec<(i64, Vec<Cell>)> = Vec::new();
                let fast_upsert = upsert.as_ref().is_some_and(|plan| self.upsert_rowid_fast_path(def, &key, plan, temp_position));
                let mut upsert_live = if upsert.is_some() && !fast_upsert { self.live_rows(def, "insert")? } else { Vec::new() };
                let mut upsert_live_loaded = upsert.is_some() && !fast_upsert;
                let mut slots_by_row = Vec::with_capacity(rows.len().max(1));
                if default_values {
                    // `DEFAULT VALUES` leaves every slot unset so the writer fills
                    // it from the column default (and reports NOT NULL violations).
                    slots_by_row.push(vec![None; def.columns.len()]);
                }
                for values in &rows {
                    let mut resolved: Vec<crate::WriteValue> = Vec::with_capacity(values.len());
                    for value in values {
                        let cell = match value {
                            InsertValue::Literal(literal) => literal_cell(literal),
                            InsertValue::Parameter { index, .. } => self.parameters.get(*index).cloned().unwrap_or(Cell::Null),
                            InsertValue::Expression(expression) => self.eval_trigger_expression(expression, "insert")?,
                        };
                        resolved.push(cell_write_value(&cell));
                    }
                    slots_by_row.push(self_contained_arity(def, &table, columns.as_deref(), &resolved)?);
                }
                if let Some(rows) = &selected_rows {
                    for row in rows {
                        let resolved: Vec<crate::WriteValue> = row.iter().map(cell_write_value).collect();
                        slots_by_row.push(select_arity(def, &table, columns.as_deref(), &resolved)?);
                    }
                }
                // SQLite applies column affinity to every supplied value before
                // the row reaches conflict detection: a `REAL` bound to an
                // `INTEGER`-affinity column with no fractional part becomes an
                // integer. Normalize the slots here so the upsert rowid probe and
                // the scanning conflict path compare the same storage class the
                // writer will store, instead of missing the conflict and surfacing
                // a spurious `UNIQUE` error.
                for slots in &mut slots_by_row {
                    for (position, slot) in slots.iter_mut().enumerate() {
                        if let Some(value) = slot.take() {
                            *slot = Some(crate::apply_write_affinity(&def.columns[position].declared_type, value));
                        }
                    }
                }
                // A `STRICT` table rejects a value whose post-affinity storage
                // class differs from the declared type, before any row is
                // written. `INSERT OR IGNORE` skips the offending row instead.
                if ignore {
                    slots_by_row.retain(|slots| {
                        slots.iter().enumerate().all(|(position, slot)| match slot {
                            Some(value) => strict_value_violation(def, position, value, "insert").is_none(),
                            None => true,
                        })
                    });
                }
                else {
                    for slots in &slots_by_row {
                        for (position, slot) in slots.iter().enumerate() {
                            if let Some(value) = slot {
                                if let Some(error) = strict_value_violation(def, position, value, "insert") {
                                    return Err(error);
                                }
                            }
                        }
                    }
                }
                // One row at a time inside a transaction: a failing `CHECK`
                // must undo every row of a multi-row statement, as SQLite does.
                // Temp tables snapshot their row map instead of opening one.
                let temp_snapshot = temp_position.map(|position| self.catalog.temp_tables[position].rows.clone());
                if temp_position.is_none() {
                    self.writer.begin().map_err(|error| storage_failure(error).in_statement("insert"))?;
                }
                let temp_keys = if temp_position.is_some() { self.unique_keys(def, &key) } else { Vec::new() };
                let check_shape = ScopeShape { columns: Vec::new() };
                let check_budget = Budget::new();
                let queue_triggers = self.table_has_triggers(&key);
                let trigger_columns = trigger_frame_columns(def);
                let recursive_triggers = self.recursive_triggers;
                let mut replace_deleted: Vec<(i64, Vec<Cell>)> = Vec::new();
                let mut replace_index_needed = false;
                let mut replace_without_needed = false;
                let mut rowids = Vec::with_capacity(slots_by_row.len());
                let mut last_written: Option<i64> = None;
                let mut affected = 0usize;
                let mut returned: Vec<Vec<Cell>> = Vec::new();
                let mut failure = None;
                for slots in &slots_by_row {
                    // A `BEFORE INSERT` trigger sees the row before it is
                    // written: defaults are already applied and an unassigned
                    // rowid is `-1`, exactly like SQLite.
                    if queue_triggers && !replace {
                        let before_rowid = def
                            .rowid_alias
                            .and_then(|alias| slots.get(alias).and_then(|slot| slot.as_ref()))
                            .and_then(|value| match value {
                                crate::WriteValue::Integer(value) => Some(*value),
                                _ => None,
                            })
                            .unwrap_or(-1);
                        let cells = check_row(def, slots, before_rowid);
                        self.pending_events.push(PendingTriggerEvent {
                            table: key.clone(),
                            timing: TriggerTiming::Before,
                            event: TriggerEvent::Insert,
                            columns: trigger_columns.clone(),
                            new: Some(cells),
                            old: None,
                        });
                    }
                    let outcome = if replace {
                        match self.apply_replace(
                            def,
                            &key,
                            &table,
                            slots,
                            temp_position,
                            recursive_triggers,
                            queue_triggers,
                            &trigger_columns,
                            &mut replace_deleted,
                        ) {
                            Ok(rowid) => UpsertOutcome::Inserted(rowid),
                            Err(error) => {
                                failure = Some(error);
                                break;
                            }
                        }
                    }
                    else {
                        match &upsert {
                            Some(plan) => {
                                let fast = if fast_upsert {
                                    Some(self.apply_upsert_rowid(
                                        def,
                                        &key,
                                        &table,
                                        plan,
                                        slots,
                                        returning.as_ref(),
                                        &returning_shape.scope,
                                        &check_budget,
                                    ))
                                }
                                else {
                                    None
                                };
                                let outcome = match fast {
                                    Some(Ok(Some(outcome))) => Ok(outcome),
                                    Some(Err(error)) => Err(error),
                                    Some(Ok(None)) | None => {
                                        if !upsert_live_loaded {
                                            upsert_live = match self.live_rows(def, "insert") {
                                                Ok(rows) => rows,
                                                Err(error) => {
                                                    failure = Some(error);
                                                    break;
                                                }
                                            };
                                            upsert_live_loaded = true;
                                        }
                                        self.apply_upsert(
                                            def,
                                            &key,
                                            &table,
                                            plan,
                                            slots,
                                            temp_position,
                                            &mut upsert_live,
                                            returning.as_ref(),
                                            &returning_shape.scope,
                                            &check_budget,
                                        )
                                    }
                                };
                                match outcome {
                                    Ok(outcome) => outcome,
                                    Err(error) => {
                                        failure = Some(error);
                                        break;
                                    }
                                }
                            }
                            None => {
                                let inserted = if let Some(position) = temp_position {
                                    let table = &mut self.catalog.temp_tables[position];
                                    temp_insert_row(def, &mut table.rows, slots, &temp_keys)
                                }
                                else {
                                    self.writer.insert(&table, slots).map_err(|error| storage_failure(error).in_statement("insert"))
                                };
                                match inserted {
                                    Ok(rowid) => UpsertOutcome::Inserted(rowid),
                                    Err(error) if ignore && ignoreable_insert_error(&error) => continue,
                                    Err(error) => {
                                        failure = Some(error);
                                        break;
                                    }
                                }
                            }
                        }
                    };
                    let rowid = match outcome {
                        UpsertOutcome::Skipped => continue,
                        UpsertOutcome::Inserted(rowid) => {
                            affected += 1;
                            rowid
                        }
                        UpsertOutcome::Updated { rowid, rows } => {
                            // `plan_update` already validated the rewritten row
                            // and evaluated `RETURNING` against it.
                            affected += 1;
                            last_written = Some(rowid);
                            returned.extend(rows);
                            continue;
                        }
                    };
                    // 行像只在 CHECK / RETURNING / 触发器 / 索引探针缓存需要时
                    // 才拼出来:普通插入路径不再为每行复制一份单元格向量。
                    let needs_cells = !def.checks.is_empty() || returning.is_some() || queue_triggers || watch_probes;
                    let cells = if needs_cells { check_row(def, slots, rowid) } else { Vec::new() };
                    let ctx = EvalContext {
                        engine: self,
                        outer: &[],
                        current: &check_shape,
                        aggregates: &[],
                        windows: &[],
                        window_row: 0,
                        budget: &check_budget,
                        ctes: None,
                    };
                    for check in &def.checks {
                        let value = match check.expr.eval(&cells, &ctx) {
                            Ok(value) => value,
                            Err(error) => {
                                failure = Some(error);
                                break;
                            }
                        };
                        // SQLite only fails a check whose result is false; a
                        // NULL result passes.
                        if truthiness(&value) == Some(false) {
                            let mut error = SqlError::new(
                                "check-constraint",
                                format!("CHECK constraint failed: {}", check.name.clone().unwrap_or_else(|| check.text.clone())),
                            )
                            .in_statement("insert");
                            error.clause = Some("CHECK".to_string());
                            failure = Some(error);
                            break;
                        }
                    }
                    if failure.is_some() {
                        break;
                    }
                    // The row passed the writer's NOT NULL / UNIQUE / PRIMARY
                    // KEY checks and this statement's CHECK constraints, so
                    // SQLite would have recorded its rowid already. A later
                    // failure rolls the row back but does not undo the record.
                    last_written = Some(rowid);
                    if let Some(plan) = &returning {
                        match plan.eval(&cells, &ctx) {
                            Ok(row) => returned.push(row),
                            Err(error) => {
                                failure = Some(error);
                                break;
                            }
                        }
                    }
                    rowids.push(rowid);
                    if watch_probes {
                        probe_rows.push((rowid, cells.clone()));
                    }
                    if queue_triggers {
                        self.pending_events.push(PendingTriggerEvent {
                            table: key.clone(),
                            timing: TriggerTiming::After,
                            event: TriggerEvent::Insert,
                            columns: trigger_columns.clone(),
                            new: Some(cells.clone()),
                            old: None,
                        });
                    }
                }
                // Referential checks run after every row is written and before
                // the transaction commits: a failing statement leaves no row
                // behind, and a self-reference may point at a later row of the
                // same statement.
                // 只有本表或被别的表引用时才需要外键检查:没有外键关系的表上
                // `REPLACE` 不再为每条语句做一次全表快照。
                let replace_needs_foreign_keys = self.foreign_keys
                    && temp_position.is_none()
                    && (!def.foreign_keys.is_empty() || !self.referencing_foreign_keys(&def.name).is_empty());
                if failure.is_none() && replace_needs_foreign_keys && !replace_deleted.is_empty() {
                    let changes: Vec<(Vec<Cell>, Option<Vec<Cell>>)> = replace_deleted.iter().map(|(_, cells)| (cells.clone(), None)).collect();
                    match self.apply_foreign_key_actions(def, &changes, "insert", 0) {
                        Ok((index_needed, without_needed)) => {
                            replace_index_needed |= index_needed;
                            replace_without_needed |= without_needed;
                        }
                        Err(error) => failure = Some(error),
                    }
                    if failure.is_none() {
                        let removed: Vec<i64> = replace_deleted.iter().map(|(rowid, _)| *rowid).collect();
                        // 只有开启外键时才需要整表快照：普通 `REPLACE` 不再为每条
                        // 语句扫一遍全表。
                        let replace_old_rows = self.live_rows(def, "insert")?;
                        if let Err(error) = self.check_delete_foreign_keys(def, &replace_old_rows, &removed, "insert") {
                            failure = Some(error);
                        }
                    }
                }
                if failure.is_none() && self.foreign_keys {
                    if let Err(error) = self.check_insert_foreign_keys(def, &rowids, "insert") {
                        failure = Some(error);
                    }
                }
                if let Some(rowid) = last_written {
                    if !def.without_rowid {
                        self.last_insert_rowid = rowid;
                    }
                }
                if let Some(error) = failure {
                    match (temp_position, temp_snapshot) {
                        (Some(position), Some(snapshot)) => self.catalog.temp_tables[position].rows = snapshot,
                        _ => {
                            let _ = self.writer.rollback();
                        }
                    }
                    return Err(error);
                }
                if temp_position.is_none() {
                    if let Err(error) = self.refresh_managed_indexes(&key, def, "insert") {
                        let _ = self.writer.rollback();
                        return Err(error);
                    }
                    self.writer.commit().map_err(|error| storage_failure(error).in_statement("insert"))?;
                    if watch_probes {
                        self.patch_index_probes_after_insert(&key, &probe_rows);
                    }
                }
                // Explicit indexes and the automatic ones built for any
                // `PRIMARY KEY` / `UNIQUE` constraint live only as key sets
                // until they are materialized into b-tree pages, so a write to
                // an indexed table has to rebuild them before the image is read.
                if temp_position.is_none() && (self.has_indexes(def, &key) || replace_index_needed) {
                    self.materialize_indexes_after_write("insert")?;
                }
                if temp_position.is_none() && (def.without_rowid || replace_without_needed) {
                    self.materialize_without_rowid_after_write("insert")?;
                }
                if let Some(plan) = &returning {
                    return Ok(ExecResult { statement: "insert", columns: plan.columns.clone(), rows: returned, affected });
                }
                if default_values {
                    // `DEFAULT VALUES` reports neither columns nor rows, like
                    // SQLite; the stored row is written the same way.
                    return Ok(ExecResult { statement: "insert", columns: Vec::new(), rows: Vec::new(), affected });
                }
                if def.without_rowid {
                    // A WITHOUT ROWID insert has no rowid to report.
                    return Ok(ExecResult { statement: "insert", columns: Vec::new(), rows: Vec::new(), affected });
                }
                let rows = rowids.into_iter().map(|rowid| vec![Cell::Integer(rowid)]).collect();
                Ok(ExecResult { statement: "insert", columns: vec!["rowid".to_string()], rows, affected })
            }
            Statement::CreateIndex { schema: _, name, table, if_not_exists, unique, columns, predicate, sql } => {
                let table_key = table.to_ascii_lowercase();
                if is_schema_table(&table) {
                    return Err(
                        SqlError::new("readonly-system-table", format!("table {table} may not be indexed")).in_statement("create-index")
                    );
                }
                if self.catalog.views.contains_key(&table_key) {
                    return Err(SqlError::new("readonly-view", "views may not be indexed").in_statement("create-index"));
                }
                check_reserved_object_name(&name, "create-index")?;
                let position = *self
                    .catalog
                    .table_lookup
                    .get(&table_key)
                    .ok_or_else(|| SqlError::new("unknown-table", format!("unknown table `{table}`")).in_statement("create-index"))?;
                let def_storage = self.catalog.tables[position].clone();
                let def = &def_storage;
                let key = name.to_ascii_lowercase();
                if self.catalog.table_lookup.contains_key(&key) {
                    return Err(SqlError::new("table-exists", format!("there is already a table named `{name}`")).in_statement("create-index"));
                }
                if self.catalog.indexes.contains_key(&key) {
                    if if_not_exists {
                        return Ok(ExecResult { statement: "create-index", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                    }
                    return Err(SqlError::new("index-exists", format!("index `{name}` already exists")).in_statement("create-index"));
                }
                if self.catalog.views.contains_key(&key) {
                    return Err(
                        SqlError::new("index-exists", format!("there is already an object named `{name}`")).in_statement("create-index")
                    );
                }
                // Expression keys and a `WHERE` predicate make the key values
                // row-dependent, so the engine computes them and hands the exact
                // entries to the writer.
                let managed = predicate.is_some() || columns.iter().any(|column| column.expr.is_some() || column.collate.is_some());
                if managed {
                    let shape = TargetShape::new(def, &table, None);
                    let mut terms = Vec::with_capacity(columns.len());
                    let mut collations = Vec::with_capacity(columns.len());
                    for column in &columns {
                        let resolved = match &column.expr {
                            Some(expr) => shape.resolve(expr, "create-index")?,
                            None => {
                                let position =
                                    def.columns.iter().position(|existing| existing.name.eq_ignore_ascii_case(&column.name)).ok_or_else(
                                        || {
                                            SqlError::new("unknown-column", format!("unknown column `{}` in table `{table}`", column.name))
                                                .in_statement("create-index")
                                        },
                                    )?;
                                ResolvedExpr::Column { source: 0, index: position }
                            }
                        };
                        // 键项显式 `COLLATE` 优先，裸列继承列声明，表达式按 `BINARY`。
                        collations.push(match &column.collate {
                            Some(name) => Collation::parse(name).ok_or_else(|| unknown_collation(name).in_statement("create-index"))?,
                            None => match &column.expr {
                                Some(_) => Collation::Binary,
                                None => def
                                    .columns
                                    .iter()
                                    .find(|existing| existing.name.eq_ignore_ascii_case(&column.name))
                                    .and_then(|existing| existing.collate)
                                    .unwrap_or_default(),
                            },
                        });
                        terms.push(resolved);
                    }
                    let predicate = shape.resolve_optional(predicate.as_ref(), "create-index")?;
                    let rows = self.live_rows(def, "create-index")?;
                    let entries = self.managed_index_entries(def, &terms, predicate.as_ref(), &rows)?;
                    if unique {
                        let names = columns.iter().map(|column| format!("{table}.{}", column.name)).collect::<Vec<_>>().join(", ");
                        if managed_unique_violation(&entries, &collations) {
                            return Err(
                                SqlError::new("unique-constraint", format!("UNIQUE constraint failed: {names}")).in_statement("create-index")
                            );
                        }
                    }
                    self.writer
                        .create_managed_index(&name, &table, unique, &sql, &entries)
                        .map_err(|error| storage_failure(error).in_statement("create-index"))?;
                    let order = self.catalog.index_sequence;
                    self.catalog.index_sequence += 1;
                    self.catalog.indexes.insert(
                        key,
                        IndexDef {
                            name: name.clone(),
                            table: table_key,
                            columns: Vec::new(),
                            unique,
                            order,
                            managed: true,
                            terms,
                            predicate,
                            collations,
                        },
                    );
                    return Ok(ExecResult { statement: "create-index", columns: Vec::new(), rows: Vec::new(), affected: 1 });
                }
                let mut positions = Vec::with_capacity(columns.len());
                let mut collations = Vec::with_capacity(columns.len());
                for column in &columns {
                    let position =
                        def.columns.iter().position(|existing| existing.name.eq_ignore_ascii_case(&column.name)).ok_or_else(|| {
                            SqlError::new("unknown-column", format!("unknown column `{}` in table `{table}`", column.name))
                                .in_statement("create-index")
                        })?;
                    positions.push(position);
                    collations.push(match &column.collate {
                        Some(name) => Collation::parse(name).ok_or_else(|| unknown_collation(name).in_statement("create-index"))?,
                        None => def.columns[position].collate.unwrap_or_default(),
                    });
                }
                if unique {
                    self.ensure_unique_keys(&table, def, &columns)?;
                }
                let names = columns.iter().map(|column| column.name.clone()).collect::<Vec<_>>();
                self.writer
                    .create_index_with_sql(&name, &table, &names, unique, Some(&sql))
                    .map_err(|error| storage_failure(error).in_statement("create-index"))?;
                // 空表上新键的索引:探针从空集开始,后续 `INSERT` / `DELETE`
                // 逐行增量维护,首次区间删除或等值查询不必再整份解码索引。
                let empty_index = self.writer.index_is_empty(&name).unwrap_or(false);
                let order = self.catalog.index_sequence;
                self.catalog.index_sequence += 1;
                self.catalog.indexes.insert(
                    key,
                    IndexDef {
                        name: name.clone(),
                        table: table_key,
                        columns: positions,
                        unique,
                        order,
                        managed: false,
                        terms: Vec::new(),
                        predicate: None,
                        collations,
                    },
                );
                if empty_index {
                    self.install_empty_index_probe(&name);
                }
                Ok(ExecResult { statement: "create-index", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            Statement::Update { schema: _, table, alias, assignments, from, filter, returning } => {
                check_readonly_table(&table, "update")?;
                let key = table.to_ascii_lowercase();
                if self.catalog.views.contains_key(&key) {
                    if !from.is_empty() {
                        let mut error =
                            SqlError::new("unsupported-clause", "`UPDATE ... FROM` is not supported on a view; name a base table instead");
                        error.clause = Some("FROM".to_string());
                        return Err(error.in_statement("update"));
                    }
                    return self.run_view_update(&table, &key, alias.as_deref(), &assignments, filter.as_ref(), returning.as_ref());
                }
                let temp_position = if self.scope == TableScope::Main { None } else { self.catalog.temp_lookup.get(&key).copied() };
                let main_position = if self.scope == TableScope::Temp {
                    None
                }
                else {
                    match temp_position {
                        Some(_) => None,
                        None => Some(*self.catalog.table_lookup.get(&key).ok_or_else(|| self.table_missing(&table, "update"))?),
                    }
                };
                let def_storage = match temp_position {
                    Some(position) => self.catalog.temp_tables[position].def.clone(),
                    None => self.catalog.tables[main_position.expect("main table resolved")].clone(),
                };
                let def = &def_storage;
                let budget = Budget::new();
                // An `UPDATE ... FROM` resolves assignments and the predicate
                // against the target row plus every `FROM` source, so a bare
                // column may come from either side.
                let (shape, from_tables) = if from.is_empty() {
                    (TargetShape::new(def, alias.as_deref().unwrap_or(&table), self.trigger_frames.last()), Vec::new())
                }
                else {
                    let width = def.columns.len();
                    let mut sources = vec![Source {
                        label: alias.as_deref().unwrap_or(&table).to_string(),
                        columns: SourceColumns::Table(def),
                        offset: 0,
                        join: None,
                        star_hidden: vec![false; width],
                        unqualified_hidden: vec![false; width],
                        star_override: vec![None; width],
                    }];
                    let mut offset = width + usize::from(!def.without_rowid);
                    let mut from_tables: Vec<Rc<Vec<Vec<Cell>>>> = Vec::with_capacity(from.len());
                    for entry in &from {
                        let (columns, rows) = self.select_source(entry, &budget, None, None, None)?;
                        let visible = columns.width();
                        let width = visible + usize::from(columns.hidden_rowid());
                        sources.push(Source {
                            label: entry.label().to_string(),
                            columns,
                            offset,
                            join: None,
                            star_hidden: vec![false; visible],
                            unqualified_hidden: vec![false; visible],
                            star_override: vec![None; visible],
                        });
                        offset += width;
                        from_tables.push(rows);
                    }
                    let source_count = sources.len();
                    let relation = Relation {
                        sources,
                        using: vec![Vec::new(); source_count],
                        using_slots: Vec::new(),
                        trigger: self.trigger_frames.last(),
                        excluded: false,
                    };
                    let scope = relation.shape();
                    (TargetShape { relation, scope }, from_tables)
                };
                let mut targets: Vec<(usize, ResolvedExpr)> = Vec::with_capacity(assignments.len());
                for assignment in &assignments {
                    let index =
                        def.columns.iter().position(|column| column.name.eq_ignore_ascii_case(&assignment.column)).ok_or_else(|| {
                            SqlError::new("unknown-column", format!("unknown column `{}` in table `{table}`", assignment.column))
                                .in_statement("update")
                        })?;
                    targets.push((index, shape.resolve(&assignment.value, "update")?));
                }
                let predicate = shape.resolve_optional(filter.as_ref(), "update")?;
                // SQLite resolves `RETURNING` against the real table name; the
                // `AS` alias of the update target is not visible there.
                let returning_shape = TargetShape::new(def, &table, None);
                let returning = match &returning {
                    Some(items) => Some(returning_shape.plan(def, items, "update")?),
                    None => None,
                };
                // A target row joined to several `FROM` rows is updated once,
                // using the first matching join row, like SQLite.
                let joined = if from.is_empty() {
                    None
                }
                else {
                    let product = join_product(&from_tables);
                    let mut joined: BTreeMap<i64, Vec<Cell>> = BTreeMap::new();
                    if !product.is_empty() {
                        let live = self.live_rows(def, "update")?;
                        let ctx = EvalContext {
                            engine: self,
                            outer: &[],
                            current: &shape.scope,
                            aggregates: &[],
                            windows: &[],
                            window_row: 0,
                            budget: &budget,
                            ctes: None,
                        };
                        for (rowid, cells) in &live {
                            let mut base = cells.clone();
                            if !def.without_rowid {
                                base.push(Cell::Integer(*rowid));
                            }
                            for extra in &product {
                                let mut merged = base.clone();
                                merged.extend(extra.iter().cloned());
                                let keep = match &predicate {
                                    Some(predicate) => truthiness(&predicate.eval(&merged, &ctx)?) == Some(true),
                                    None => true,
                                };
                                if keep {
                                    joined.insert(*rowid, merged);
                                    break;
                                }
                            }
                        }
                    }
                    Some(joined)
                };
                let mut needs_index = self.has_indexes(def, &key);
                // `WHERE <rowid> = <key>` 或普通列索引谓词命中时，扫描不必整表解码。
                let candidates =
                    if from.is_empty() { self.update_candidates(def, &key, &table, alias.as_deref(), filter.as_ref(), &targets) } else { None };
                let plan = self.plan_update(
                    def,
                    &key,
                    &targets,
                    predicate.as_ref(),
                    joined.as_ref(),
                    returning.as_ref(),
                    &shape.scope,
                    &budget,
                    None,
                    candidates.as_deref(),
                )?;
                // Old row values are needed to tell which referenced keys this
                // statement rewrites, so they come from the plan's pre-statement
                // snapshot instead of decoding the table a second time.
                let old_rows = &plan.live;
                let assigned: Vec<bool> =
                    def.columns.iter().enumerate().map(|(index, _)| targets.iter().any(|(target, _)| *target == index)).collect();
                for (_, values) in &plan.writes {
                    for (position, value) in values.iter().enumerate() {
                        if let crate::UpdateValue::Set(value) = value {
                            if let Some(error) = strict_value_violation(def, position, value, "update") {
                                return Err(error);
                            }
                        }
                    }
                }
                let queue_triggers = self.table_has_triggers(&key);
                let trigger_columns = trigger_frame_columns(def);
                let assigned_names: Vec<String> = assignments.iter().map(|assignment| assignment.column.clone()).collect();
                let old_lookup: BTreeMap<i64, Vec<Cell>> =
                    if queue_triggers || def.without_rowid || self.foreign_keys { old_rows.iter().cloned().collect() } else { BTreeMap::new() };
                // The `AFTER` events wait until referential actions and checks
                // have run, matching SQLite's `BEFORE`, cascade, `AFTER` order.
                let mut after_events: Vec<(i64, Vec<Cell>, Vec<Cell>)> = Vec::new();
                if let Some(position) = temp_position {
                    let mut events: Vec<(i64, Vec<Cell>, Vec<Cell>)> = Vec::new();
                    {
                        let table = &mut self.catalog.temp_tables[position];
                        for (rowid, values) in &plan.writes {
                            let cells = table
                                .rows
                                .get_mut(rowid)
                                .ok_or_else(|| SqlError::new("storage", "temp row disappeared").in_statement("update"))?;
                            let old_cells = cells.clone();
                            for (slot, value) in values.iter().enumerate() {
                                if let crate::UpdateValue::Set(value) = value {
                                    cells[slot] = write_value_cell(value);
                                }
                            }
                            if queue_triggers {
                                events.push((*rowid, old_cells, cells.clone()));
                            }
                        }
                    }
                    for (rowid, old_cells, new_cells) in events {
                        self.queue_update_events(&key, def, &trigger_columns, &assigned_names, rowid, &old_cells, &new_cells);
                    }
                }
                else {
                    self.writer.begin().map_err(|error| storage_failure(error).in_statement("update"))?;
                    // 规划阶段已经解出每一行的原值:写侧直接复用这份行像,不再从
                    // b-tree 读一次 payload 并重新解码。
                    let mut live_cursor = 0usize;
                    for (rowid, values) in &plan.writes {
                        while live_cursor < plan.live.len() && plan.live[live_cursor].0 < *rowid {
                            live_cursor += 1;
                        }
                        let previous = plan.live.get(live_cursor).filter(|(id, _)| id == rowid).map(|(_, cells)| cells.as_slice());
                        // 行像只有触发器和 WITHOUT ROWID 表需要;普通 rowid 表把
                        // 赋值交给写侧,不为每一行拼一份完整的单元格向量。
                        let needs_cells = queue_triggers || def.without_rowid;
                        let mut new_cells = if needs_cells { old_lookup.get(rowid).cloned().unwrap_or_default() } else { Vec::new() };
                        if needs_cells {
                            if new_cells.len() < def.columns.len() {
                                new_cells.resize(def.columns.len(), Cell::Null);
                            }
                            for (slot, value) in values.iter().enumerate() {
                                if let crate::UpdateValue::Set(value) = value {
                                    new_cells[slot] = write_value_cell(value);
                                }
                            }
                        }
                        if queue_triggers {
                            let old_cells = old_lookup.get(rowid).cloned().unwrap_or_default();
                            self.queue_update_before(&key, def, &trigger_columns, &assigned_names, *rowid, &old_cells, &new_cells);
                            after_events.push((*rowid, old_cells, new_cells.clone()));
                        }
                        let written = if def.without_rowid {
                            let old_values: Vec<crate::WriteValue> =
                                old_lookup.get(rowid).cloned().unwrap_or_default().iter().map(cell_write_value).collect();
                            let new_values: Vec<crate::WriteValue> = new_cells.iter().map(cell_write_value).collect();
                            self.writer.update_without_rowid(&table, &old_values, &new_values)
                        }
                        else if let Some(previous) = previous {
                            let before = cells_write_values(previous);
                            self.writer.update_with_values(&table, *rowid, values, before)
                        }
                        else {
                            self.writer.update(&table, *rowid, values)
                        };
                        if let Err(error) = written {
                            let _ = self.writer.rollback();
                            return Err(write_failure(error, "update", def));
                        }
                    }
                }
                let mut needs_without_rowid = temp_position.is_none() && def.without_rowid;
                if temp_position.is_none() && self.foreign_keys {
                    let changes: Vec<(Vec<Cell>, Option<Vec<Cell>>)> = plan
                        .writes
                        .iter()
                        .map(|(rowid, values)| {
                            let old = old_lookup.get(rowid).cloned().unwrap_or_default();
                            let mut next = old.clone();
                            if next.len() < def.columns.len() {
                                next.resize(def.columns.len(), Cell::Null);
                            }
                            for (slot, value) in values.iter().enumerate() {
                                if let crate::UpdateValue::Set(value) = value {
                                    next[slot] = write_value_cell(value);
                                }
                            }
                            (old, Some(next))
                        })
                        .collect();
                    match self.apply_foreign_key_actions(def, &changes, "update", 0) {
                        Ok((index_needed, without_needed)) => {
                            needs_index |= index_needed;
                            needs_without_rowid |= without_needed;
                        }
                        Err(error) => {
                            let _ = self.writer.rollback();
                            return Err(error);
                        }
                    }
                }
                if self.foreign_keys {
                    if let Err(error) = self.check_update_foreign_keys(def, &plan, &old_rows, &assigned, "update") {
                        if temp_position.is_none() {
                            let _ = self.writer.rollback();
                        }
                        return Err(error);
                    }
                }
                if temp_position.is_none() {
                    for (rowid, old_cells, new_cells) in &after_events {
                        self.queue_update_after(&key, def, &trigger_columns, &assigned_names, *rowid, old_cells, new_cells);
                    }
                }
                if temp_position.is_none() {
                    if let Err(error) = self.refresh_managed_indexes(&key, def, "update") {
                        let _ = self.writer.rollback();
                        return Err(error);
                    }
                    self.writer.commit().map_err(|error| storage_failure(error).in_statement("update"))?;
                }
                if temp_position.is_none() && needs_index {
                    self.materialize_indexes_after_write("update")?;
                }
                if needs_without_rowid {
                    self.materialize_without_rowid_after_write("update")?;
                }
                // 外键动作会直接改写子表行,索引内容不再只由本表语句决定;
                // 其余情况按被赋值的列精确失效,只改非索引列时缓存原样保留。
                let foreign_actions =
                    self.foreign_keys && (!def.foreign_keys.is_empty() || !self.referencing_foreign_keys(&def.name).is_empty());
                if foreign_actions {
                    self.invalidate_all_indexes();
                }
                else {
                    self.invalidate_indexes_after_update(def, &key, &assigned);
                }
                Ok(ExecResult {
                    statement: "update",
                    columns: returning.as_ref().map(|plan| plan.columns.clone()).unwrap_or_default(),
                    rows: plan.returning,
                    affected: plan.affected,
                })
            }
            Statement::Delete { schema: _, table, alias, filter, returning } => {
                check_readonly_table(&table, "delete")?;
                let key = table.to_ascii_lowercase();
                if self.catalog.views.contains_key(&key) {
                    return self.run_view_delete(&table, &key, alias.as_deref(), filter.as_ref(), returning.as_ref());
                }
                let temp_position = if self.scope == TableScope::Main { None } else { self.catalog.temp_lookup.get(&key).copied() };
                let main_position = if self.scope == TableScope::Temp {
                    None
                }
                else {
                    match temp_position {
                        Some(_) => None,
                        None => Some(*self.catalog.table_lookup.get(&key).ok_or_else(|| self.table_missing(&table, "delete"))?),
                    }
                };
                // Temp rows are removed from the in-memory map, so their
                // definition is cloned to keep the catalog readable while the
                // map is borrowed mutably.
                let def_storage = match temp_position {
                    Some(position) => self.catalog.temp_tables[position].def.clone(),
                    None => self.catalog.tables[main_position.expect("main table resolved")].clone(),
                };
                let def = &def_storage;
                let shape = TargetShape::new(def, alias.as_deref().unwrap_or(&table), self.trigger_frames.last());
                let predicate = shape.resolve_optional(filter.as_ref(), "delete")?;
                let returning_shape = TargetShape::new(def, &table, None);
                let returning = match &returning {
                    Some(items) => Some(returning_shape.plan(def, items, "delete")?),
                    None => None,
                };
                let budget = Budget::new();
                let mut needs_index = self.has_indexes(def, &key);
                // `WHERE <rowid> = <key>` 或普通列索引谓词命中时，删除不必整表解码。
                let candidates = self.delete_candidates(def, &key, &table, alias.as_deref(), filter.as_ref());
                let plan = self.plan_delete(def, predicate.as_ref(), returning.as_ref(), &shape.scope, &budget, candidates.as_deref())?;
                // A temp table snapshots its row map instead of opening a
                // writer transaction, so a failing referential check undoes the
                // removal exactly like a rollback would.
                let temp_snapshot = temp_position.map(|position| self.catalog.temp_tables[position].rows.clone());
                let queue_triggers = self.table_has_triggers(&key);
                let trigger_columns = trigger_frame_columns(def);
                // The deleted rows' referenced keys are only visible before the
                // removal, so the snapshot comes from the plan; 只有触发器和外键
                // 动作需要按 rowid 查旧行，其余路径不建这张表。
                let old_lookup: BTreeMap<i64, Vec<Cell>> =
                    if queue_triggers || self.foreign_keys { plan.live.iter().cloned().collect() } else { BTreeMap::new() };
                let mut after_events: Vec<(i64, Vec<Cell>)> = Vec::new();
                if let Some(position) = temp_position {
                    {
                        let table = &mut self.catalog.temp_tables[position];
                        for rowid in &plan.removed {
                            table.rows.remove(rowid);
                        }
                    }
                    if queue_triggers {
                        for rowid in &plan.removed {
                            let old_cells = old_lookup.get(rowid).cloned().unwrap_or_default();
                            self.queue_delete_events(&key, &trigger_columns, *rowid, &old_cells);
                        }
                    }
                }
                else {
                    self.writer.begin().map_err(|error| storage_failure(error).in_statement("delete"))?;
                    // 删除语句在规划阶段已经解出被删行:有索引键要维护时把这些值
                    // 直接交给写侧,没有索引时连行像都不必读,只删 b-tree 项。
                    let maintain_indexes = needs_index && !def.without_rowid;
                    let mut live_cursor = 0usize;
                    for rowid in &plan.removed {
                        if queue_triggers {
                            let old_cells = old_lookup.get(rowid).cloned().unwrap_or_default();
                            self.queue_delete_before(&key, &trigger_columns, *rowid, &old_cells);
                            after_events.push((*rowid, old_cells));
                        }
                        let removed = if def.without_rowid {
                            let old_values: Vec<crate::WriteValue> =
                                old_lookup.get(rowid).cloned().unwrap_or_default().iter().map(cell_write_value).collect();
                            self.writer.delete_without_rowid(&table, &old_values)
                        }
                        else if maintain_indexes {
                            while live_cursor < plan.live.len() && plan.live[live_cursor].0 < *rowid {
                                live_cursor += 1;
                            }
                            match plan.live.get(live_cursor).filter(|(id, _)| id == rowid) {
                                Some((_, cells)) => {
                                    let values = cells_write_values(cells);
                                    self.writer.delete_with_values(&table, *rowid, &values)
                                }
                                None => self.writer.delete(&table, *rowid),
                            }
                        }
                        else {
                            self.writer.delete_rowid(&table, *rowid)
                        };
                        if let Err(error) = removed {
                            let _ = self.writer.rollback();
                            return Err(write_failure(error, "delete", def));
                        }
                    }
                }
                let mut needs_without_rowid = temp_position.is_none() && def.without_rowid;
                if temp_position.is_none() && self.foreign_keys {
                    let changes: Vec<(Vec<Cell>, Option<Vec<Cell>>)> =
                        plan.removed.iter().map(|rowid| (old_lookup.get(rowid).cloned().unwrap_or_default(), None)).collect();
                    match self.apply_foreign_key_actions(def, &changes, "delete", 0) {
                        Ok((index_needed, without_needed)) => {
                            needs_index |= index_needed;
                            needs_without_rowid |= without_needed;
                        }
                        Err(error) => {
                            let _ = self.writer.rollback();
                            return Err(error);
                        }
                    }
                }
                if self.foreign_keys {
                    if let Err(error) = self.check_delete_foreign_keys(def, &plan.live, &plan.removed, "delete") {
                        match (temp_position, temp_snapshot) {
                            (Some(position), Some(snapshot)) => self.catalog.temp_tables[position].rows = snapshot,
                            _ => {
                                let _ = self.writer.rollback();
                            }
                        }
                        return Err(error);
                    }
                }
                if temp_position.is_none() {
                    for (rowid, old_cells) in &after_events {
                        self.queue_delete_after(&key, &trigger_columns, *rowid, old_cells);
                    }
                }
                if temp_position.is_none() {
                    if let Err(error) = self.refresh_managed_indexes(&key, def, "delete") {
                        let _ = self.writer.rollback();
                        return Err(error);
                    }
                    self.writer.commit().map_err(|error| storage_failure(error).in_statement("delete"))?;
                }
                if temp_position.is_none() && needs_index {
                    self.materialize_indexes_after_write("delete")?;
                }
                if needs_without_rowid {
                    self.materialize_without_rowid_after_write("delete")?;
                }
                // 同 `UPDATE`:外键级联删除会改动子表,只能全量失效;否则
                // 把被删行从整份索引快照里精确剔除。
                let foreign_actions =
                    self.foreign_keys && (!def.foreign_keys.is_empty() || !self.referencing_foreign_keys(&def.name).is_empty());
                if foreign_actions {
                    self.invalidate_all_indexes();
                }
                else {
                    self.patch_index_probes_after_delete(&key, &plan);
                }
                Ok(ExecResult {
                    statement: "delete",
                    columns: returning.as_ref().map(|plan| plan.columns.clone()).unwrap_or_default(),
                    rows: plan.returning,
                    affected: plan.removed.len(),
                })
            }
            Statement::Pragma { schema, name, value } => {
                if name.eq_ignore_ascii_case("database_list") {
                    let mut rows = vec![vec![Cell::Integer(0), Cell::Text("main".to_string()), Cell::Text(String::new())]];
                    if self.catalog.temp_created {
                        rows.push(vec![Cell::Integer(1), Cell::Text("temp".to_string()), Cell::Text(String::new())]);
                    }
                    for (index, attached) in self.attached.iter().enumerate() {
                        rows.push(vec![
                            Cell::Integer(2 + index as i64),
                            Cell::Text(attached.name.clone()),
                            Cell::Text(attached.display.clone()),
                        ]);
                    }
                    return Ok(ExecResult {
                        statement: "pragma",
                        columns: vec!["seq".to_string(), "name".to_string(), "file".to_string()],
                        rows,
                        affected: 0,
                    });
                }
                let argument = value.unwrap_or_default();
                match name.to_ascii_lowercase().as_str() {
                    "table_info" => return pragma_table_info(self, schema.as_deref(), &argument, false),
                    "table_xinfo" => return pragma_table_info(self, schema.as_deref(), &argument, true),
                    "index_list" => return Ok(pragma_index_list(self, &argument)),
                    "index_info" => return Ok(pragma_index_info(self, &argument)),
                    "foreign_key_list" => return Ok(pragma_foreign_key_list(self, &argument)),
                    "user_version" => {
                        return Ok(match argument.is_empty() {
                            false => {
                                self.writer.set_user_version(argument.trim().parse::<i64>().unwrap_or(0));
                                ExecResult { statement: "pragma", columns: Vec::new(), rows: Vec::new(), affected: 0 }
                            }
                            true => ExecResult {
                                statement: "pragma",
                                columns: vec!["user_version".to_string()],
                                rows: vec![vec![Cell::Integer(self.writer.user_version())]],
                                affected: 0,
                            },
                        });
                    }
                    "application_id" => {
                        return Ok(match argument.is_empty() {
                            false => {
                                self.writer.set_application_id(argument.trim().parse::<i64>().unwrap_or(0));
                                ExecResult { statement: "pragma", columns: Vec::new(), rows: Vec::new(), affected: 0 }
                            }
                            true => ExecResult {
                                statement: "pragma",
                                columns: vec!["application_id".to_string()],
                                rows: vec![vec![Cell::Integer(self.writer.application_id())]],
                                affected: 0,
                            },
                        });
                    }
                    "integrity_check" => return Ok(self.pragma_integrity_check(&argument)),
                    _ => {}
                }
                let boolean = |text: &str| -> bool {
                    match text.to_ascii_lowercase().as_str() {
                        "on" | "true" | "yes" => true,
                        "off" | "false" | "no" => false,
                        other => other.parse::<i64>().map(|value| value > 0).unwrap_or(false),
                    }
                };
                let recursive = name.eq_ignore_ascii_case("recursive_triggers");
                if !name.eq_ignore_ascii_case("foreign_keys") && !recursive {
                    return Err(SqlError::new(
                        "unsupported-pragma",
                        format!(
                            "unsupported pragma `{name}`; this engine supports `foreign_keys`, `recursive_triggers`, `table_info`, `table_xinfo`, `index_list`, `index_info`, `foreign_key_list`, `database_list`, `user_version`, `application_id` and `integrity_check`"
                        ),
                    )
                    .in_clause("PRAGMA")
                    .in_statement("pragma"));
                }
                let flag = if recursive { &mut self.recursive_triggers } else { &mut self.foreign_keys };
                if argument.is_empty() {
                    Ok(ExecResult {
                        statement: "pragma",
                        columns: vec![name.clone()],
                        rows: vec![vec![Cell::Integer(i64::from(*flag))]],
                        affected: 0,
                    })
                }
                else {
                    // SQLite reads the value as a boolean: `on` / `true`
                    // / `yes` and any positive integer enable it, `off` /
                    // `false` / `no` / zero and non-numeric text disable
                    // it.
                    *flag = boolean(&argument);
                    Ok(ExecResult { statement: "pragma", columns: Vec::new(), rows: Vec::new(), affected: 0 })
                }
            }
            Statement::DropTable { schema: _, name, if_exists } => {
                if is_schema_table(&name) {
                    return Err(SqlError::new("readonly-system-table", format!("table {name} may not be dropped")).in_statement("drop-table"));
                }
                let key = name.to_ascii_lowercase();
                // The temp schema is searched before main, so `DROP TABLE`
                // removes the temp object when both schemas share the name.
                let temp_position = if self.scope == TableScope::Main { None } else { self.catalog.temp_lookup.get(&key).copied() };
                if temp_position.is_none() && self.catalog.temp_views.contains_key(&key) {
                    return Err(SqlError::new("wrong-object", format!("use DROP VIEW to delete view {name}")).in_statement("drop-table"));
                }
                if let Some(position) = temp_position {
                    let dropped = self.catalog.temp_tables.remove(position);
                    // The freed page goes back to the temp allocator, so the
                    // next temp table reuses it like SQLite does.
                    self.catalog.temp_free_pages.insert(dropped.rootpage);
                    self.catalog.temp_lookup.remove(&key);
                    self.reindex_temp_tables();
                    self.catalog.triggers.retain(|_, trigger| !(trigger.temp && trigger.table == key));
                    return Ok(ExecResult { statement: "drop-table", columns: Vec::new(), rows: Vec::new(), affected: 1 });
                }
                let objects = self.schema_objects("drop-table")?;
                let present = objects.iter().any(|object| object.kind == SchemaObjectKind::Table && object.name.eq_ignore_ascii_case(&name));
                if !present {
                    if objects.iter().any(|object| object.kind == SchemaObjectKind::View && object.name.eq_ignore_ascii_case(&name)) {
                        return Err(SqlError::new("wrong-object", format!("use DROP VIEW to delete view {name}")).in_statement("drop-table"));
                    }
                    if if_exists {
                        return Ok(ExecResult { statement: "drop-table", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                    }
                    return Err(
                        SqlError::new("unknown-table", format!("no such table: {}", self.scoped_name(&name))).in_statement("drop-table")
                    );
                }
                if name.eq_ignore_ascii_case("sqlite_sequence") {
                    return Err(SqlError::new("readonly-system-table", format!("table {name} may not be dropped")).in_statement("drop-table"));
                }
                let key = name.to_ascii_lowercase();
                let position = *self
                    .catalog
                    .table_lookup
                    .get(&key)
                    .ok_or_else(|| SqlError::new("unknown-table", format!("no such table: {name}")).in_statement("drop-table"))?;
                self.writer.begin().map_err(|error| storage_failure(error).in_statement("drop-table"))?;
                match self.writer.drop_table(&name) {
                    Ok(true) => {}
                    Ok(false) => {
                        let _ = self.writer.rollback();
                        return Err(SqlError::new("unknown-table", format!("no such table: {name}")).in_statement("drop-table"));
                    }
                    Err(error) => {
                        let _ = self.writer.rollback();
                        return Err(storage_failure(error).in_statement("drop-table"));
                    }
                }
                self.writer.commit().map_err(|error| storage_failure(error).in_statement("drop-table"))?;
                self.catalog.tables.remove(position);
                self.reindex_tables();
                self.catalog.indexes.retain(|_, index| index.table != key);
                self.catalog.triggers.retain(|_, trigger| trigger.table != key);
                Ok(ExecResult { statement: "drop-table", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            Statement::DropIndex { schema: _, name, if_exists } => {
                self.writer.begin().map_err(|error| storage_failure(error).in_statement("drop-index"))?;
                let outcome = match self.writer.drop_index(&name) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        let _ = self.writer.rollback();
                        return Err(storage_failure(error).in_statement("drop-index"));
                    }
                };
                match outcome {
                    DropIndexOutcome::Dropped => {}
                    DropIndexOutcome::Missing => {
                        let _ = self.writer.rollback();
                        if if_exists {
                            return Ok(ExecResult { statement: "drop-index", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                        }
                        return Err(SqlError::new("unknown-index", format!("no such index: {name}")).in_statement("drop-index"));
                    }
                    DropIndexOutcome::Automatic => {
                        let _ = self.writer.rollback();
                        return Err(SqlError::new(
                            "autoindex-immutable",
                            "index associated with UNIQUE or PRIMARY KEY constraint cannot be dropped",
                        )
                        .in_statement("drop-index"));
                    }
                }
                self.writer.commit().map_err(|error| storage_failure(error).in_statement("drop-index"))?;
                self.catalog.indexes.remove(&name.to_ascii_lowercase());
                Ok(ExecResult { statement: "drop-index", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            Statement::AlterTable { schema: _, table, action } => self.run_alter_table(table, action),
            Statement::Select { with, recursive, first, compounds, order, limit } => {
                // One budget per statement, shared by every nested subquery, so
                // deep correlation reports a structured error instead of
                // running until the host timeout.
                let budget = Budget::new();
                self.run_select(&with, recursive, &first, &compounds, order, limit, &[], &budget, None)
            }
        }
    }

    /// Reads the live schema rows with a statement-tagged storage error.
    fn schema_objects(&self, statement: &'static str) -> Result<Vec<crate::SchemaObject>, SqlError> {
        self.writer.schema_objects(TableLimits::default()).map_err(|error| storage_failure(error).in_statement(statement))
    }

    /// Rebuilds the lowercase name to position map after a table is removed.
    fn reindex_tables(&mut self) {
        self.catalog.table_lookup.clear();
        for (position, table) in self.catalog.tables.iter().enumerate() {
            self.catalog.table_lookup.insert(table.name.to_ascii_lowercase(), position);
        }
    }

    /// Rebuilds the temp schema name map after a temp table is removed.
    fn reindex_temp_tables(&mut self) {
        self.catalog.temp_lookup.clear();
        for (position, table) in self.catalog.temp_tables.iter().enumerate() {
            self.catalog.temp_lookup.insert(table.def.name.to_ascii_lowercase(), position);
        }
    }

    /// Executes the `ALTER TABLE` forms: `RENAME TO`, `RENAME COLUMN`, `ADD
    /// COLUMN` and `DROP COLUMN`. A drop rewrites every stored record and the
    /// stored `CREATE TABLE` text, refusing columns still read by an index,
    /// a view, a trigger or a table-level constraint.
    fn run_alter_table(&mut self, table: String, action: AlterAction) -> Result<ExecResult, SqlError> {
        if is_schema_table(&table) {
            return Err(SqlError::new("readonly-system-table", format!("table {table} may not be altered")).in_statement("alter-table"));
        }
        let objects = self.schema_objects("alter-table")?;
        if table.eq_ignore_ascii_case("sqlite_sequence") {
            let present = objects.iter().any(|object| object.kind == SchemaObjectKind::Table && object.name.eq_ignore_ascii_case(&table));
            if present {
                return Err(SqlError::new("readonly-system-table", format!("table {table} may not be altered")).in_statement("alter-table"));
            }
            return Err(SqlError::new("unknown-table", format!("no such table: {table}")).in_statement("alter-table"));
        }
        let key = table.to_ascii_lowercase();
        let position = *self
            .catalog
            .table_lookup
            .get(&key)
            .ok_or_else(|| SqlError::new("unknown-table", format!("no such table: {table}")).in_statement("alter-table"))?;
        let stored = objects
            .iter()
            .find(|object| object.kind == SchemaObjectKind::Table && object.name.eq_ignore_ascii_case(&self.catalog.tables[position].name))
            .and_then(|object| object.sql.clone())
            .ok_or_else(|| SqlError::new("storage", "sqlite table has no stored text").in_statement("alter-table"))?;
        match action {
            AlterAction::RenameTable { new_name } => {
                check_reserved_object_name(&new_name, "alter-table")?;
                let new_key = new_name.to_ascii_lowercase();
                if self.catalog.table_lookup.contains_key(&new_key)
                    || self.catalog.indexes.contains_key(&new_key)
                    || self.catalog.views.contains_key(&new_key)
                {
                    return Err(SqlError::new("table-exists", format!("there is already another table or index with this name: {new_name}"))
                        .in_statement("alter-table"));
                }
                let old_name = self.catalog.tables[position].name.clone();
                let new_sql = rewrite_create_table_name(&stored, &new_name)
                    .ok_or_else(|| SqlError::new("storage", "sqlite stored table text unrecognized").in_statement("alter-table"))?;
                let mut pending = Vec::new();
                for object in &objects {
                    if object.kind != SchemaObjectKind::Index || !object.table_name.eq_ignore_ascii_case(&old_name) {
                        continue;
                    }
                    if let Some(text) = &object.sql {
                        let rewritten = rewrite_create_index_table(text, &new_name)
                            .ok_or_else(|| SqlError::new("storage", "sqlite stored index text unrecognized").in_statement("alter-table"))?;
                        pending.push((object.name.clone(), rewritten));
                    }
                }
                // Every view that names the old table follows the rename, and
                // SQLite rewrites its stored text so a later reopen resolves.
                let mut pending_views = Vec::new();
                for object in &objects {
                    if object.kind != SchemaObjectKind::View {
                        continue;
                    }
                    let Some(view) = self.catalog.views.get(&object.name.to_ascii_lowercase())
                    else {
                        continue;
                    };
                    let Some(text) = &object.sql
                    else {
                        continue;
                    };
                    if !statement_references(&view.query, &old_name) {
                        continue;
                    }
                    if let Some(rewritten) = rewrite_view_table(text, &old_name, &new_name) {
                        pending_views.push((view.name.clone(), rewritten));
                    }
                }
                self.writer.begin().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
                if let Err(error) = self.writer.rename_table_schema(&old_name, &new_name, &new_sql, &pending) {
                    let _ = self.writer.rollback();
                    return Err(storage_failure(error).in_statement("alter-table"));
                }
                self.writer.commit().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
                if !pending_views.is_empty() {
                    self.writer.begin().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
                    for (name, text) in &pending_views {
                        if let Err(error) = self.writer.rewrite_view_sql(name, text) {
                            let _ = self.writer.rollback();
                            return Err(storage_failure(error).in_statement("alter-table"));
                        }
                    }
                    self.writer.commit().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
                }
                self.catalog.tables[position].name = new_name;
                self.reindex_tables();
                for index in self.catalog.indexes.values_mut() {
                    if index.table == key {
                        index.table = new_key.clone();
                    }
                }
                for trigger in self.catalog.triggers.values_mut() {
                    if trigger.table == key {
                        trigger.table = new_key.clone();
                    }
                }
                for (name, text) in pending_views {
                    if let Some(Statement::CreateView { query, .. }) = parse(&text).ok().and_then(|statements| statements.into_iter().next()) {
                        if let Some(view) = self.catalog.views.get_mut(&name.to_ascii_lowercase()) {
                            view.query = *query;
                        }
                    }
                }
                Ok(ExecResult { statement: "alter-table", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            AlterAction::RenameColumn { old, new } => {
                let def = &self.catalog.tables[position];
                let column = def
                    .columns
                    .iter()
                    .position(|column| column.name.eq_ignore_ascii_case(&old))
                    .ok_or_else(|| SqlError::new("unknown-column", format!("no such column: \"{old}\"")).in_statement("alter-table"))?;
                if def.columns.iter().enumerate().any(|(index, existing)| index != column && existing.name.eq_ignore_ascii_case(&new)) {
                    return Err(SqlError::new("duplicate-column", format!("duplicate column name: {new}")).in_statement("alter-table"));
                }
                if old.eq_ignore_ascii_case(&new) {
                    return Ok(ExecResult { statement: "alter-table", columns: Vec::new(), rows: Vec::new(), affected: 1 });
                }
                let name = def.name.clone();
                let old_column = old;
                let new_sql = rewrite_create_table_column(&stored, &old_column, &new)
                    .ok_or_else(|| SqlError::new("storage", "sqlite stored table text unrecognized").in_statement("alter-table"))?;
                let mut pending = Vec::new();
                for object in &objects {
                    if object.kind != SchemaObjectKind::Index || !object.table_name.eq_ignore_ascii_case(&name) {
                        continue;
                    }
                    if let Some(text) = &object.sql {
                        let rewritten = rewrite_create_index_column(text, &old_column, &new)
                            .ok_or_else(|| SqlError::new("storage", "sqlite stored index text unrecognized").in_statement("alter-table"))?;
                        pending.push((object.name.clone(), rewritten));
                    }
                }
                // Views keep the renamed column reference spelled correctly in
                // their stored text, exactly like SQLite rewrites them.
                let mut pending_views = Vec::new();
                for object in &objects {
                    if object.kind != SchemaObjectKind::View {
                        continue;
                    }
                    let Some(view) = self.catalog.views.get(&object.name.to_ascii_lowercase())
                    else {
                        continue;
                    };
                    let Some(text) = &object.sql
                    else {
                        continue;
                    };
                    if !statement_mentions_column(&view.query, &old_column, None) {
                        continue;
                    }
                    if let Some(rewritten) = rewrite_view_column(text, &old_column, &new) {
                        pending_views.push((view.name.clone(), rewritten));
                    }
                }
                self.writer.begin().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
                if let Err(error) = self.writer.rename_column_schema(&name, &new_sql, &pending) {
                    let _ = self.writer.rollback();
                    return Err(storage_failure(error).in_statement("alter-table"));
                }
                self.writer.commit().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
                if !pending_views.is_empty() {
                    self.writer.begin().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
                    for (view, text) in &pending_views {
                        if let Err(error) = self.writer.rewrite_view_sql(view, text) {
                            let _ = self.writer.rollback();
                            return Err(storage_failure(error).in_statement("alter-table"));
                        }
                    }
                    self.writer.commit().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
                }
                for (view, text) in pending_views {
                    if let Some(Statement::CreateView { query, .. }) = parse(&text).ok().and_then(|statements| statements.into_iter().next()) {
                        if let Some(view) = self.catalog.views.get_mut(&view.to_ascii_lowercase()) {
                            view.query = *query;
                        }
                    }
                }
                self.catalog.tables[position].columns[column].name = new;
                Ok(ExecResult { statement: "alter-table", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            AlterAction::AddColumn { column, checks, foreign_keys, sql } => {
                let def = &self.catalog.tables[position];
                if def.columns.iter().any(|existing| existing.name.eq_ignore_ascii_case(&column.name)) {
                    return Err(
                        SqlError::new("duplicate-column", format!("duplicate column name: {}", column.name)).in_statement("alter-table")
                    );
                }
                if column.primary_key {
                    return Err(SqlError::new("invalid-alter", "Cannot add a PRIMARY KEY column").in_statement("alter-table"));
                }
                if column.unique {
                    return Err(SqlError::new("invalid-alter", "Cannot add a UNIQUE column").in_statement("alter-table"));
                }
                if !foreign_keys.is_empty() && matches!(&column.default_value, Some(value) if !matches!(value, crate::WriteValue::Null)) {
                    return Err(SqlError::new("invalid-alter", "Cannot add a REFERENCES column with non-NULL default value")
                        .in_statement("alter-table"));
                }
                // SQLite only refuses when an existing row would read back
                // NULL; on an empty table the same definition is accepted.
                let has_rows = !self
                    .writer
                    .table_rows(&def.name, TableLimits::default())
                    .map_err(|error| storage_failure(error).in_statement("alter-table"))?
                    .is_empty();
                if column.not_null && matches!(column.default_value, None | Some(crate::WriteValue::Null)) && has_rows {
                    return Err(
                        SqlError::new("invalid-alter", "Cannot add a NOT NULL column with default value NULL").in_statement("alter-table")
                    );
                }
                let mut columns = def.columns.clone();
                columns.push(column.clone());
                let resolved = resolve_checks(&def.name, &columns, &checks, "alter-table", def.without_rowid)?;
                let resolved_foreign_keys = resolve_foreign_keys(&columns, &foreign_keys)?;
                let open = stored
                    .rfind(')')
                    .ok_or_else(|| SqlError::new("storage", "sqlite stored table text has no column list").in_statement("alter-table"))?;
                let new_sql = format!("{}, {}{}", &stored[..open], sql, &stored[open..]);
                self.writer.begin().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
                if let Err(error) = self.writer.rewrite_table_sql(&def.name, &new_sql) {
                    let _ = self.writer.rollback();
                    return Err(storage_failure(error).in_statement("alter-table"));
                }
                self.writer.commit().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
                self.catalog.tables[position].columns.push(column);
                self.catalog.tables[position].checks.extend(resolved);
                self.catalog.tables[position].foreign_keys.extend(resolved_foreign_keys);
                Ok(ExecResult { statement: "alter-table", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            AlterAction::DropColumn { column } => {
                let def = &self.catalog.tables[position];
                let column_position = def
                    .columns
                    .iter()
                    .position(|existing| existing.name.eq_ignore_ascii_case(&column))
                    .ok_or_else(|| SqlError::new("unknown-column", format!("no such column: \"{column}\"")).in_statement("alter-table"))?;
                let primary = def.rowid_alias == Some(column_position)
                    || def.primary_key.contains(&column_position)
                    || def.columns[column_position].primary_key;
                if primary {
                    return Err(
                        SqlError::new("invalid-alter", format!("cannot drop PRIMARY KEY column: \"{column}\"")).in_statement("alter-table")
                    );
                }
                if def.columns[column_position].unique {
                    return Err(SqlError::new("invalid-alter", format!("cannot drop UNIQUE column: \"{column}\"")).in_statement("alter-table"));
                }
                if def.columns.len() == 1 {
                    return Err(SqlError::new("invalid-alter", format!("cannot drop column \"{column}\": no other columns exist"))
                        .in_statement("alter-table"));
                }
                if let Some((name, _)) =
                    self.catalog.indexes.iter().find(|(_, index)| index.table == key && index.columns.contains(&column_position))
                {
                    return Err(SqlError::new("invalid-alter", format!("error in index {name} after drop column: no such column: {column}"))
                        .in_statement("alter-table"));
                }
                let new_sql = match rewrite_create_table_drop_column(&stored, &def.columns[column_position].name) {
                    Ok(Some(text)) => text,
                    Ok(None) => {
                        return Err(SqlError::new("storage", "sqlite stored table text unrecognized").in_statement("alter-table"));
                    }
                    Err(DropColumnConflict::TableCheck) => {
                        return Err(SqlError::new(
                            "invalid-alter",
                            format!("error in table {} after drop column: no such column: {column}", def.name),
                        )
                        .in_statement("alter-table"));
                    }
                    Err(DropColumnConflict::TableForeignKey) => {
                        return Err(SqlError::new(
                            "invalid-alter",
                            format!("error in table {} after drop column: unknown column \"{column}\" in foreign key definition", def.name),
                        )
                        .in_statement("alter-table"));
                    }
                };
                if let Some((name, _)) = self.catalog.views.iter().find(|(_, view)| statement_mentions_column(&view.query, &column, None)) {
                    return Err(SqlError::new("invalid-alter", format!("error in view {name} after drop column: no such column: {column}"))
                        .in_statement("alter-table"));
                }
                if let Some((name, _)) = self.catalog.triggers.iter().find(|(_, trigger)| {
                    trigger.table == key
                        && trigger.body.iter().any(|statement| statement_mentions_column(statement, &column, Some(&["new", "old"])))
                }) {
                    return Err(SqlError::new("invalid-alter", format!("error in trigger {name} after drop column: no such column: {column}"))
                        .in_statement("alter-table"));
                }
                // The catalog is re-derived from the rewritten DDL so checks,
                // foreign keys and the rowid alias all follow the new column
                // positions instead of being patched by hand.
                let mut statements = parse(&new_sql)
                    .map_err(|_| SqlError::new("storage", "sqlite stored table text unrecognized").in_statement("alter-table"))?;
                let Some(Statement::CreateTable {
                    name,
                    columns,
                    primary_key,
                    table_constraints,
                    checks,
                    foreign_keys,
                    strict,
                    without_rowid,
                    ..
                }) = statements.drain(..).next()
                else {
                    return Err(SqlError::new("storage", "sqlite stored table text unrecognized").in_statement("alter-table"));
                };
                let derived =
                    SqliteCatalog::table_entry(name, columns, primary_key, table_constraints, checks, foreign_keys, strict, without_rowid)?;
                self.writer.begin().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
                if let Err(error) = self.writer.drop_column(&def.name, column_position) {
                    let _ = self.writer.rollback();
                    return Err(storage_failure(error).in_statement("alter-table"));
                }
                if let Err(error) = self.writer.rewrite_table_sql(&def.name, &new_sql) {
                    let _ = self.writer.rollback();
                    return Err(storage_failure(error).in_statement("alter-table"));
                }
                self.writer.commit().map_err(|error| storage_failure(error).in_statement("alter-table"))?;
                for index in self.catalog.indexes.values_mut() {
                    if index.table != key {
                        continue;
                    }
                    for position in &mut index.columns {
                        if *position > column_position {
                            *position -= 1;
                        }
                    }
                }
                self.catalog.tables[position] = derived;
                Ok(ExecResult { statement: "alter-table", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
        }
    }

    /// Rejects a `UNIQUE` index when two existing rows carry the same key.
    ///
    /// SQLite treats a `NULL` key column as distinct, so rows holding `NULL`
    /// never collide. Key comparison follows the engine's storage-class rules,
    /// which makes `1` and `1.0` collide just like upstream.
    fn ensure_unique_keys(&self, table: &str, def: &TableDef, columns: &[IndexColumn]) -> Result<(), SqlError> {
        let mut positions = Vec::with_capacity(columns.len());
        let mut collations = Vec::with_capacity(columns.len());
        for column in columns {
            let position = def.columns.iter().position(|existing| existing.name.eq_ignore_ascii_case(&column.name)).ok_or_else(|| {
                SqlError::new("unknown-column", format!("unknown column `{}` in table `{table}`", column.name)).in_statement("create-index")
            })?;
            positions.push(position);
            // 键项显式 `COLLATE` 优先，否则继承列的声明排序规则。
            collations.push(match &column.collate {
                Some(name) => Collation::parse(name).ok_or_else(|| unknown_collation(name).in_statement("create-index"))?,
                None => def.columns[position].collate.unwrap_or_default(),
            });
        }
        let mut rows = Vec::new();
        for row in self.writer.table_rows(table, TableLimits::default()).map_err(|error| storage_failure(error).in_statement("create-index"))? {
            rows.push(decode_row(&row.payload, def, row.rowid)?);
        }
        for right in 1..rows.len() {
            for left in 0..right {
                let collides = positions.iter().enumerate().all(|(position, column)| {
                    let (first, second) = (&rows[left][*column], &rows[right][*column]);
                    !matches!(first, Cell::Null)
                        && !matches!(second, Cell::Null)
                        && compare_cells_with(first, second, collations[position]) == Ordering::Equal
                });
                if collides {
                    let names =
                        positions.iter().map(|position| format!("{table}.{}", def.columns[*position].name)).collect::<Vec<_>>().join(", ");
                    return Err(SqlError::new("unique-constraint", format!("UNIQUE constraint failed: {names}")).in_statement("create-index"));
                }
            }
        }
        Ok(())
    }

    /// Reads every row of `def`, decoded and sorted by rowid.
    fn live_rows(&self, def: &TableDef, statement: &'static str) -> Result<Vec<(i64, Vec<Cell>)>, SqlError> {
        if def.temp {
            let position = *self
                .catalog
                .temp_lookup
                .get(&def.name.to_ascii_lowercase())
                .ok_or_else(|| SqlError::new("storage", format!("temp table `{}` disappeared", def.name)).in_statement(statement))?;
            return Ok(self.catalog.temp_tables[position].rows.iter().map(|(rowid, cells)| (*rowid, cells.clone())).collect());
        }
        if def.without_rowid {
            // WITHOUT ROWID rows carry no rowid; a scan position is used as
            // the identity the update / delete planners sort by.
            let stored = self
                .writer
                .without_rowid_rows(&def.name, TableLimits::default())
                .map_err(|error| storage_failure(error).in_statement(statement))?;
            let mut rows = Vec::with_capacity(stored.len());
            for (position, payload) in stored.iter().enumerate() {
                rows.push((position as i64, def.decode_without_rowid(payload)?));
            }
            return Ok(rows);
        }
        let stored =
            self.writer.table_rows(&def.name, TableLimits::default()).map_err(|error| storage_failure(error).in_statement(statement))?;
        let mut rows = Vec::with_capacity(stored.len());
        for row in &stored {
            rows.push((row.rowid, decode_row(&row.payload, def, row.rowid)?));
        }
        rows.sort_by_key(|(rowid, _)| *rowid);
        Ok(rows)
    }

    /// 按候选 rowid 读行,候选升序取回。
    ///
    /// 相邻间隔不超过 [`CANDIDATE_GAP`] 的候选合并成一次区间读,避免逐行下降。
    /// 区间读会带回缝隙里的少量非候选行,谓词与唯一性判定照旧处理,因此只是多读
    /// 几行,不会改变语句结果。
    fn live_rows_at(&self, def: &TableDef, statement: &'static str, rowids: &[i64]) -> Result<Vec<(i64, Vec<Cell>)>, SqlError> {
        if def.temp {
            return Ok(self.live_rows(def, statement)?.into_iter().filter(|(rowid, _)| rowids.contains(rowid)).collect());
        }
        let mut sorted = rowids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let mut rows = Vec::with_capacity(sorted.len());
        let mut position = 0;
        while position < sorted.len() {
            let start = sorted[position];
            let mut end = start;
            let mut next = position + 1;
            while next < sorted.len() && sorted[next] - end <= CANDIDATE_GAP {
                end = sorted[next];
                next += 1;
            }
            let stored = if start == end {
                // 单行候选直接走 writer 的 rowid 下钻：点查/按唯一键改写是
                // DML 的常见形态，不必为一行重建区间读取上下文。
                match self.writer.row_payload(&def.name, start).map_err(|error| storage_failure(error).in_statement(statement))? {
                    Some(payload) => vec![TableRow { rowid: start, payload }],
                    None => Vec::new(),
                }
            }
            else {
                self.writer
                    .table_range(&def.name, start, end, TableLimits::default())
                    .map_err(|error| storage_failure(error).in_statement(statement))?
            };
            for row in &stored {
                rows.push((row.rowid, decode_row(&row.payload, def, row.rowid)?));
            }
            position = next;
        }
        rows.sort_by_key(|(rowid, _)| *rowid);
        Ok(rows)
    }

    /// `DELETE` 的候选 rowid:裸 `<rowid> = <键>` 等值,或普通列索引的探针结果。
    ///
    /// 与 SELECT 的 `rowid_pin` / `index_pin` 同一判定:只认裸 `AND` 链上的等值与
    /// 首列范围比较,键可以是字面量或执行期已绑定的参数,索引必须是未被托管改造的
    /// 普通列索引。谓词仍会在候选行上重跑一遍,所以候选只是超集,不会改变删除集合。
    fn delete_candidates(&self, def: &TableDef, table_key: &str, table: &str, alias: Option<&str>, filter: Option<&Expr>) -> Option<Vec<i64>> {
        if def.without_rowid || def.temp {
            return None;
        }
        self.write_candidates(def, table_key, alias.unwrap_or(table), filter)
    }

    /// `UPDATE` 的候选 rowid,约束与 [`Engine::delete_candidates`] 相同。
    ///
    /// `UPDATE` 还要用整表做唯一性冲突判定,因此只在「唯一键只有 rowid,且没有任何
    /// 赋值改动 rowid」时才收窄。这两条成立时,收窄后的行集与整表在冲突判定上等价;
    /// 外键检查只看被写入的行,因此不需要整表。`WITHOUT ROWID` 表一律走整表读取。
    fn update_candidates(
        &self,
        def: &TableDef,
        table_key: &str,
        table: &str,
        alias: Option<&str>,
        filter: Option<&Expr>,
        targets: &[(usize, ResolvedExpr)],
    ) -> Option<Vec<i64>> {
        if def.without_rowid || def.temp {
            return None;
        }
        if targets.iter().any(|(index, _)| Some(*index) == def.rowid_alias) {
            return None;
        }
        if self.unique_keys(def, table_key).iter().any(|key| !matches!(key, UniqueKey::Rowid(_))) {
            return None;
        }
        self.write_candidates(def, table_key, alias.unwrap_or(table), filter)
    }

    /// 按谓词取写语句的候选 rowid:先认裸 `<rowid> = <键>`,再逐个普通列索引试
    /// 全键等值与首列范围。返回的 rowid 升序,可能含谓词不成立的行。
    fn write_candidates(&self, def: &TableDef, table_key: &str, label: &str, filter: Option<&Expr>) -> Option<Vec<i64>> {
        let filter = filter?;
        let alias_name = def.rowid_alias.map(|index| def.columns[index].name.clone());
        if let Some(rowid) = Self::rowid_eq_conjunct(filter, alias_name.as_deref(), label, &self.parameters) {
            return Some(vec![rowid]);
        }
        // `IN (<rowids>)` 与 `>= a AND <= b` 同样是 rowid 上的精确候选集,
        // 不必为一条写语句解码整表再逐行判断。
        if let Some(rowids) = Self::rowid_in_conjunct(filter, alias_name.as_deref(), label, &self.parameters) {
            return Some(rowids);
        }
        if let Some(rowids) = Self::rowid_range_candidates(filter, alias_name.as_deref(), label, &self.parameters) {
            return Some(rowids);
        }
        for index in self.catalog.indexes.values() {
            if index.managed || index.table != table_key || index.columns.is_empty() {
                continue;
            }
            if let Some(rowids) = self.index_probe_rows(index, def, filter, label) {
                return Some(rowids);
            }
            if let Some(rowids) = self.index_range_rows(index, def, filter, label) {
                return Some(rowids);
            }
        }
        None
    }

    /// Resolves one `ON CONFLICT` clause against the insert target: the
    /// uniqueness rules its target names, plus the `DO UPDATE` assignments
    /// evaluated against the stored row and the supplied (`excluded`) row.
    fn prepare_upsert(&self, def: &TableDef, table_key: &str, label: &str, clause: &ConflictClause) -> Result<UpsertPlan, SqlError> {
        let mismatch = || {
            SqlError::new("conflict-target", "ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint").in_statement("insert")
        };
        // The target predicate is resolved against the insert target alone:
        // `excluded` is not in scope there, exactly like SQLite.
        let target_predicate = match (&clause.target, &clause.predicate) {
            (Some(_), Some(expression)) => {
                let relation = target_relation(def, label, None);
                Some(Self::resolve_upsert_expr(&relation, expression)?)
            }
            (Some(_), None) => None,
            (None, Some(_)) => return Err(mismatch()),
            (None, None) => None,
        };
        let positions = match &clause.target {
            None => None,
            Some(columns) => {
                let mut positions = Vec::with_capacity(columns.len());
                for column in columns {
                    let position = def
                        .columns
                        .iter()
                        .position(|existing| existing.name.eq_ignore_ascii_case(column))
                        .ok_or_else(|| SqlError::new("unknown-column", format!("no such column: {column}")).in_statement("insert"))?;
                    positions.push(position);
                }
                Some(positions)
            }
        };
        let all_keys = self.unique_keys(def, table_key);
        let (keys, others) = match &positions {
            None => {
                let mut keys = all_keys;
                if def.without_rowid {
                    keys.retain(|key| !matches!(key, UniqueKey::Rowid(_)));
                }
                (keys, Vec::new())
            }
            Some(positions) if target_predicate.is_some() => {
                // A target `WHERE` can only name a partial index, so no plain
                // unique constraint or full index is the conflict resolver.
                (Vec::new(), all_keys)
            }
            Some(positions) => {
                let keys = self.upsert_target_keys(def, table_key, positions);
                let others = all_keys
                    .into_iter()
                    .filter(|key| match key {
                        UniqueKey::Rowid(_) => !(positions.len() == 1 && def.rowid_alias == Some(positions[0])),
                        UniqueKey::Columns(existing, _, _) => !same_positions(existing, positions),
                    })
                    .collect();
                (keys, others)
            }
        };
        let all_managed = self.upsert_managed_indexes(def, table_key);
        let (managed, other_managed) = match &positions {
            None => (all_managed, Vec::new()),
            Some(positions) => {
                let mut managed = Vec::new();
                let mut other_managed = Vec::new();
                for target in all_managed {
                    if managed_target_matches(&target, positions, target_predicate.as_ref()) {
                        managed.push(target);
                    }
                    else {
                        other_managed.push(target);
                    }
                }
                (managed, other_managed)
            }
        };
        if positions.is_some() && keys.is_empty() && managed.is_empty() {
            return Err(mismatch());
        }
        let (action, assignments, filter) = match &clause.action {
            ConflictAction::Nothing => (UpsertAction::Nothing, Vec::new(), None),
            ConflictAction::Update { assignments, predicate } => {
                let relation = upsert_relation(def, label);
                let mut targets = Vec::with_capacity(assignments.len());
                for assignment in assignments {
                    let index =
                        def.columns.iter().position(|column| column.name.eq_ignore_ascii_case(&assignment.column)).ok_or_else(|| {
                            SqlError::new("unknown-column", format!("no such column: {}", assignment.column)).in_statement("insert")
                        })?;
                    targets.push((index, Self::resolve_upsert_expr(&relation, &assignment.value)?));
                }
                let filter = match predicate {
                    Some(expression) => Some(Self::resolve_upsert_expr(&relation, expression)?),
                    None => None,
                };
                (UpsertAction::Update, targets, filter)
            }
        };
        Ok(UpsertPlan { keys, managed, others, other_managed, action, assignments, filter })
    }

    /// Resolves one expression of a `DO UPDATE` clause against the upsert
    /// relation, which exposes both the stored row and `excluded.<column>`.
    fn resolve_upsert_expr(relation: &Relation, expression: &Expr) -> Result<ResolvedExpr, SqlError> {
        let mut windows = Vec::new();
        let mut aggregates = Vec::new();
        resolve_expr(expression, relation, &[], &[], &mut windows, &mut aggregates, false, false).map_err(|error| error.in_statement("insert"))
    }

    /// Every unique engine-managed index of `table`, which includes partial and
    /// expression indexes.
    fn upsert_managed_indexes(&self, def: &TableDef, table_key: &str) -> Vec<ManagedTarget> {
        if def.temp {
            return Vec::new();
        }
        self.catalog
            .indexes
            .values()
            .filter(|index| index.managed && index.unique && index.table == table_key)
            .map(|index| {
                let label = managed_index_label(def, index);
                ManagedTarget { index: index.clone(), label }
            })
            .collect()
    }

    /// The uniqueness rules one `ON CONFLICT (columns)` target matches.
    fn upsert_target_keys(&self, def: &TableDef, table_key: &str, positions: &[usize]) -> Vec<UniqueKey> {
        self.unique_keys(def, table_key)
            .into_iter()
            .filter(|key| match key {
                UniqueKey::Rowid(_) => !def.without_rowid && positions.len() == 1 && def.rowid_alias == Some(positions[0]),
                UniqueKey::Columns(existing, _, _) => same_positions(existing, positions),
            })
            .collect()
    }

    /// The key values one managed index produces for a row, or `None` when the
    /// row is outside the index's partial predicate.
    fn managed_target_key(&self, def: &TableDef, target: &ManagedTarget, cells: &[Cell]) -> Result<Option<Vec<Cell>>, SqlError> {
        let shape = TargetShape::new(def, &def.name, None);
        let budget = Budget::new();
        let aggregates: Vec<Cell> = Vec::new();
        let windows: Vec<Vec<Cell>> = Vec::new();
        let context = EvalContext {
            engine: self,
            outer: &[],
            current: &shape.scope,
            aggregates: &aggregates,
            windows: &windows,
            window_row: 0,
            budget: &budget,
            ctes: None,
        };
        if let Some(predicate) = &target.index.predicate {
            if truthiness(&predicate.eval(cells, &context)?) != Some(true) {
                return Ok(None);
            }
        }
        let mut key = Vec::with_capacity(target.index.terms.len());
        for term in &target.index.terms {
            key.push(term.eval(cells, &context)?);
        }
        Ok(Some(key))
    }

    /// The first uniqueness rule outside the conflict target the supplied row
    /// still violates; SQLite aborts the statement instead of resolving it.
    fn upsert_other_conflict(
        &self,
        def: &TableDef,
        plan: &UpsertPlan,
        candidate: &[Cell],
        candidate_rowid: i64,
        live: &[(i64, Vec<Cell>)],
        skip: i64,
    ) -> Result<Option<String>, SqlError> {
        for key in &plan.others {
            let wanted = key.values(candidate, candidate_rowid);
            for (rowid, cells) in live {
                if *rowid == skip {
                    continue;
                }
                let other = key.values(cells, *rowid);
                let collides = wanted.iter().zip(&other).all(|(first, second)| {
                    !matches!(first, Cell::Null) && !matches!(second, Cell::Null) && compare_cells(first, second) == Ordering::Equal
                });
                if collides {
                    return Ok(Some(key.label().to_string()));
                }
            }
        }
        for target in &plan.other_managed {
            let Some(wanted) = self.managed_target_key(def, target, candidate)?
            else {
                continue;
            };
            for (rowid, cells) in live {
                if *rowid == skip {
                    continue;
                }
                if let Some(other) = self.managed_target_key(def, target, cells)? {
                    if managed_keys_conflict(&wanted, &other, &target.index.collations) {
                        return Ok(Some(target.label.clone()));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Whether an `ON CONFLICT (rowid-alias)` target can be resolved by
    /// probing the primary key instead of materializing the whole table.
    ///
    /// The fast path only applies when nothing else can observe the table:
    /// no temporary shadow table, no other uniqueness rule, no managed unique
    /// index, no trigger, no foreign key, and no assignment that moves the
    /// rowid. Everything else keeps the full-scan [`Self::apply_upsert`].
    fn upsert_rowid_fast_path(&self, def: &TableDef, table_key: &str, plan: &UpsertPlan, temp_position: Option<usize>) -> bool {
        temp_position.is_none()
            && def.rowid_alias.is_some()
            && matches!(plan.keys.as_slice(), [UniqueKey::Rowid(_)])
            && plan.managed.is_empty()
            && plan.others.is_empty()
            && plan.other_managed.is_empty()
            && plan.assignments.iter().all(|(position, _)| Some(*position) != def.rowid_alias)
            && (!self.foreign_keys || (def.foreign_keys.is_empty() && self.referencing_foreign_keys(&def.name).is_empty()))
            && !self.table_has_triggers(table_key)
    }

    /// Applies one supplied row of a rowid-keyed upsert by probing only the
    /// conflicting primary-key row.
    ///
    /// `None` means the row has no explicit rowid value, so the caller must
    /// fall back to the scanning path. Rows written earlier in the same
    /// statement are already in storage, so a later supplied row still sees
    /// them exactly as a full scan would.
    #[allow(clippy::too_many_arguments)]
    fn apply_upsert_rowid(
        &mut self,
        def: &TableDef,
        table_key: &str,
        table: &str,
        plan: &UpsertPlan,
        slots: &[Option<crate::WriteValue>],
        returning: Option<&ReturningPlan>,
        shape: &ScopeShape,
        budget: &Budget,
    ) -> Result<Option<UpsertOutcome>, SqlError> {
        let width = def.columns.len();
        let Some(alias) = def.rowid_alias
        else {
            return Ok(None);
        };
        let rowid = match slots.get(alias).and_then(|slot| slot.as_ref()) {
            Some(crate::WriteValue::Integer(rowid)) => *rowid,
            _ => return Ok(None),
        };
        let payload = self.writer.row_payload(table, rowid).map_err(|error| storage_failure(error).in_statement("insert"))?;
        let Some(payload) = payload
        else {
            let inserted = self.writer.insert(table, slots).map_err(|error| storage_failure(error).in_statement("insert"))?;
            return Ok(Some(UpsertOutcome::Inserted(inserted)));
        };
        if matches!(plan.action, UpsertAction::Nothing) {
            return Ok(Some(UpsertOutcome::Skipped));
        }
        // Lean rewrite: every assignment is a direct `excluded.<column>` with no
        // `DO UPDATE WHERE`, no `RETURNING` and no `CHECK` constraints. The stored
        // row keeps its unassigned columns and the assigned ones take the
        // supplied (already affinity-normalized) values, so the expression-eval
        // machinery, the joined-row map and `plan_update` are skipped entirely.
        let lean = returning.is_none()
            && plan.filter.is_none()
            && def.checks.is_empty()
            && plan.assignments.iter().all(|(_, expr)| matches!(expr, ResolvedExpr::ExcludedColumn { .. }));
        if lean {
            let candidate = check_row(def, slots, rowid);
            let mut values: Vec<UpdateValue> = vec![UpdateValue::Keep; width];
            for (index, expr) in &plan.assignments {
                let ResolvedExpr::ExcludedColumn { index: supplied } = expr
                else {
                    unreachable!("lean upsert assignments are all excluded-column references")
                };
                let value = cell_write_value(&candidate[*supplied]);
                if let Some(error) = strict_value_violation(def, *index, &value, "insert") {
                    return Err(error);
                }
                values[*index] = UpdateValue::Set(value);
            }
            if let Err(error) = self.writer.update(table, rowid, &values) {
                return Err(write_failure(error, "insert", def));
            }
            return Ok(Some(UpsertOutcome::Updated { rowid, rows: Vec::new() }));
        }
        let stored = decode_row(&payload, def, rowid)?;
        let candidate = check_row(def, slots, rowid);
        let mut eval_cells = stored.clone();
        eval_cells.push(Cell::Integer(rowid));
        self.upsert_excluded = candidate[..width].to_vec();
        if let Some(filter) = &plan.filter {
            let ctx =
                EvalContext { engine: self, outer: &[], current: shape, aggregates: &[], windows: &[], window_row: 0, budget, ctes: None };
            let keep = truthiness(&filter.eval(&eval_cells, &ctx)?) == Some(true);
            if !keep {
                self.upsert_excluded.clear();
                return Ok(Some(UpsertOutcome::Skipped));
            }
        }
        let mut joined = BTreeMap::new();
        joined.insert(rowid, eval_cells);
        let state = vec![(rowid, stored)];
        let planned = self.plan_update(def, table_key, &plan.assignments, None, Some(&joined), returning, shape, budget, Some(&state), None);
        self.upsert_excluded.clear();
        let planned = planned?;
        if planned.writes.is_empty() {
            return Ok(Some(UpsertOutcome::Skipped));
        }
        for (_, values) in &planned.writes {
            for (position, value) in values.iter().enumerate() {
                if let crate::UpdateValue::Set(value) = value {
                    if let Some(error) = strict_value_violation(def, position, value, "insert") {
                        return Err(error);
                    }
                }
            }
        }
        for (write_rowid, values) in &planned.writes {
            if let Err(error) = self.writer.update(table, *write_rowid, values) {
                return Err(write_failure(error, "insert", def));
            }
        }
        Ok(Some(UpsertOutcome::Updated { rowid, rows: planned.returning }))
    }

    /// Applies one supplied row of an upsert: insert it, skip it, or rewrite the
    /// stored row it conflicts with. `live` is the statement's own view of the
    /// table so a later supplied row sees rows written earlier in the same
    /// statement, exactly like SQLite.
    #[allow(clippy::too_many_arguments)]
    fn apply_upsert(
        &mut self,
        def: &TableDef,
        table_key: &str,
        table: &str,
        plan: &UpsertPlan,
        slots: &[Option<crate::WriteValue>],
        temp_position: Option<usize>,
        live: &mut Vec<(i64, Vec<Cell>)>,
        returning: Option<&ReturningPlan>,
        shape: &ScopeShape,
        budget: &Budget,
    ) -> Result<UpsertOutcome, SqlError> {
        let width = def.columns.len();
        // The rowid the supplied row will occupy. An explicit `INTEGER PRIMARY
        // KEY` value is used as written, otherwise SQLite's `max(rowid) + 1`.
        let explicit_rowid = def.rowid_alias.and_then(|alias| slots.get(alias).and_then(|slot| slot.as_ref())).and_then(|value| match value {
            crate::WriteValue::Integer(value) => Some(*value),
            _ => None,
        });
        let candidate_rowid = match explicit_rowid {
            Some(rowid) => rowid,
            None => live.iter().map(|(rowid, _)| *rowid).max().unwrap_or(0) + 1,
        };
        let candidate = check_row(def, slots, candidate_rowid);
        let mut conflict_rowid = None;
        for key in &plan.keys {
            for (rowid, cells) in live.iter() {
                if upsert_key_conflicts(key, &candidate, candidate_rowid, *rowid, cells) {
                    conflict_rowid = Some(*rowid);
                    break;
                }
            }
            if conflict_rowid.is_some() {
                break;
            }
        }
        if conflict_rowid.is_none() {
            for target in &plan.managed {
                let Some(wanted) = self.managed_target_key(def, target, &candidate)?
                else {
                    continue;
                };
                for (rowid, cells) in live.iter() {
                    if let Some(other) = self.managed_target_key(def, target, cells)? {
                        if managed_keys_conflict(&wanted, &other, &target.index.collations) {
                            conflict_rowid = Some(*rowid);
                            break;
                        }
                    }
                }
                if conflict_rowid.is_some() {
                    break;
                }
            }
        }
        let unique_failed =
            |label: String| SqlError::new("unique-constraint", format!("UNIQUE constraint failed: {label}")).in_statement("insert");
        let Some(rowid) = conflict_rowid
        else {
            if let Some(label) = self.upsert_other_conflict(def, plan, &candidate, candidate_rowid, live, -1)? {
                return Err(unique_failed(label));
            }
            let inserted = if let Some(position) = temp_position {
                let keys = self.unique_keys(def, table_key);
                let table_state = &mut self.catalog.temp_tables[position];
                temp_insert_row(def, &mut table_state.rows, slots, &keys)?
            }
            else {
                self.writer.insert(table, slots).map_err(|error| storage_failure(error).in_statement("insert"))?
            };
            live.push((inserted, candidate[..width].to_vec()));
            return Ok(UpsertOutcome::Inserted(inserted));
        };
        if matches!(plan.action, UpsertAction::Nothing) {
            if let Some(label) = self.upsert_other_conflict(def, plan, &candidate, candidate_rowid, live, -1)? {
                return Err(unique_failed(label));
            }
            return Ok(UpsertOutcome::Skipped);
        }
        if let Some(label) = self.upsert_other_conflict(def, plan, &candidate, candidate_rowid, live, rowid)? {
            return Err(unique_failed(label));
        }
        // `DO UPDATE` rewrites the conflicting row the query planner would have
        // matched: the stored row plus this rowid, narrowed by the clause's own
        // `WHERE`.
        let mut predicate = ResolvedExpr::Compare {
            left: Box::new(ResolvedExpr::Column { source: 0, index: width }),
            op: CompareOp::Eq,
            right: Box::new(ResolvedExpr::Cell(Cell::Integer(rowid))),
            collation: Collation::Binary,
        };
        if let Some(filter) = &plan.filter {
            predicate = ResolvedExpr::And(Box::new(predicate), Box::new(filter.clone()));
        }
        self.upsert_excluded = candidate[..width].to_vec();
        let planned = self.plan_update(def, table_key, &plan.assignments, Some(&predicate), None, returning, shape, budget, None, None);
        self.upsert_excluded.clear();
        let planned = planned?;
        if planned.writes.is_empty() {
            // `DO UPDATE ... WHERE` was false: SQLite leaves the row alone.
            return Ok(UpsertOutcome::Skipped);
        }
        for (_, values) in &planned.writes {
            for (position, value) in values.iter().enumerate() {
                if let crate::UpdateValue::Set(value) = value {
                    if let Some(error) = strict_value_violation(def, position, value, "insert") {
                        return Err(error);
                    }
                }
            }
        }
        let find_old = |rowid: i64| live.iter().find(|(candidate, _)| *candidate == rowid).map(|(_, cells)| cells.clone()).unwrap_or_default();
        // A `DO UPDATE` is an ordinary UPDATE for trigger and foreign-key
        // purposes: SQLite fires the update triggers with the rewritten row and
        // re-checks references the statement moves.
        let queue_triggers = self.table_has_triggers(table_key);
        let trigger_columns = trigger_frame_columns(def);
        let assigned_names: Vec<String> = plan.assignments.iter().map(|(position, _)| def.columns[*position].name.clone()).collect();
        let assigned: Vec<bool> = (0..width).map(|position| plan.assignments.iter().any(|(target, _)| *target == position)).collect();
        let mut events: Vec<(i64, Vec<Cell>, Vec<Cell>)> = Vec::new();
        if let Some(position) = temp_position {
            let table_state = &mut self.catalog.temp_tables[position];
            for (write_rowid, values) in &planned.writes {
                let cells = table_state
                    .rows
                    .get_mut(write_rowid)
                    .ok_or_else(|| SqlError::new("storage", "temp row disappeared").in_statement("insert"))?;
                let old_cells = cells.clone();
                for (slot, value) in values.iter().enumerate() {
                    if let crate::UpdateValue::Set(value) = value {
                        cells[slot] = write_value_cell(value);
                    }
                }
                if queue_triggers {
                    events.push((*write_rowid, old_cells, cells.clone()));
                }
            }
        }
        else {
            for (write_rowid, values) in &planned.writes {
                let old_cells = find_old(*write_rowid);
                let mut new_cells = old_cells.clone();
                if new_cells.len() < width {
                    new_cells.resize(width, Cell::Null);
                }
                for (slot, value) in values.iter().enumerate() {
                    if let crate::UpdateValue::Set(value) = value {
                        new_cells[slot] = write_value_cell(value);
                    }
                }
                if queue_triggers {
                    self.queue_update_before(table_key, def, &trigger_columns, &assigned_names, *write_rowid, &old_cells, &new_cells);
                    events.push((*write_rowid, old_cells.clone(), new_cells.clone()));
                }
                let written = if def.without_rowid {
                    let old_values: Vec<crate::WriteValue> = old_cells.iter().map(cell_write_value).collect();
                    let new_values: Vec<crate::WriteValue> = new_cells.iter().map(cell_write_value).collect();
                    self.writer.update_without_rowid(table, &old_values, &new_values)
                }
                else {
                    self.writer.update(table, *write_rowid, values)
                };
                if let Err(error) = written {
                    return Err(write_failure(error, "insert", def));
                }
            }
        }
        if temp_position.is_some() {
            for (updated_rowid, old_cells, new_cells) in &events {
                self.queue_update_events(table_key, def, &trigger_columns, &assigned_names, *updated_rowid, old_cells, new_cells);
            }
        }
        if temp_position.is_none() && self.foreign_keys {
            let changes: Vec<(Vec<Cell>, Option<Vec<Cell>>)> = planned
                .writes
                .iter()
                .map(|(write_rowid, values)| {
                    let old = find_old(*write_rowid);
                    let mut next = old.clone();
                    if next.len() < width {
                        next.resize(width, Cell::Null);
                    }
                    for (slot, value) in values.iter().enumerate() {
                        if let crate::UpdateValue::Set(value) = value {
                            next[slot] = write_value_cell(value);
                        }
                    }
                    (old, Some(next))
                })
                .collect();
            let (index_needed, without_needed) = self.apply_foreign_key_actions(def, &changes, "insert", 0)?;
            if index_needed {
                self.materialize_indexes_after_write("insert")?;
            }
            if without_needed {
                self.materialize_without_rowid_after_write("insert")?;
            }
        }
        if self.foreign_keys {
            self.check_update_foreign_keys(def, &planned, live, &assigned, "insert")?;
        }
        if temp_position.is_none() {
            for (updated_rowid, old_cells, new_cells) in &events {
                self.queue_update_after(table_key, def, &trigger_columns, &assigned_names, *updated_rowid, old_cells, new_cells);
            }
        }
        if temp_position.is_none() && !self.table_has_triggers(table_key) {
            // Without triggers nothing else can rewrite the table, so the
            // statement's own writes are enough to keep the in-memory rows
            // current; reloading every row per upsert would make a batch of
            // single-row upserts quadratic.
            for (write_rowid, values) in &planned.writes {
                if let Some((_, cells)) = live.iter_mut().find(|(candidate, _)| *candidate == *write_rowid) {
                    for (slot, value) in values.iter().enumerate() {
                        if let (crate::UpdateValue::Set(value), Some(cell)) = (value, cells.get_mut(slot)) {
                            *cell = write_value_cell(value);
                        }
                    }
                }
            }
        }
        else {
            *live = self.live_rows(def, "insert")?;
        }
        let rowid = planned.writes.first().map(|(rowid, _)| *rowid).unwrap_or(rowid);
        Ok(UpsertOutcome::Updated { rowid, rows: planned.returning })
    }

    /// Applies one `REPLACE` row: delete every stored row that conflicts on a
    /// uniqueness rule, then insert the supplied row.
    ///
    /// The delete is an ordinary row removal for foreign-key purposes, but a
    /// `DELETE` trigger only fires when `recursive_triggers` is on, matching
    /// SQLite's documented `REPLACE` behavior.
    #[allow(clippy::too_many_arguments)]
    fn apply_replace(
        &mut self,
        def: &TableDef,
        table_key: &str,
        table: &str,
        slots: &[Option<crate::WriteValue>],
        temp_position: Option<usize>,
        recursive_triggers: bool,
        queue_triggers: bool,
        trigger_columns: &[String],
        deleted: &mut Vec<(i64, Vec<Cell>)>,
    ) -> Result<i64, SqlError> {
        let explicit_rowid = def.rowid_alias.and_then(|alias| slots.get(alias).and_then(|slot| slot.as_ref())).and_then(|value| match value {
            crate::WriteValue::Integer(value) => Some(*value),
            _ => None,
        });
        // 已给出 rowid、且表上没有其它唯一键/表达式索引时，`REPLACE` 只可能
        // 与同一 rowid 的旧行冲突：按 rowid 点读即可，不必为每条语句扫全表。
        let extra_unique = self.unique_keys(def, table_key).into_iter().any(|key| !matches!(key, UniqueKey::Rowid(_)));
        let managed_targets = self.upsert_managed_indexes(def, table_key);
        if let Some(rowid) = explicit_rowid.filter(|_| !def.without_rowid && !extra_unique && managed_targets.is_empty()) {
            let old = match temp_position {
                Some(position) => self.catalog.temp_tables[position].rows.get(&rowid).cloned(),
                None => self.live_rows_at(def, "insert", &[rowid])?.into_iter().next().map(|(_, cells)| cells),
            };
            if let Some(old) = old {
                if queue_triggers && recursive_triggers {
                    self.queue_delete_before(table_key, trigger_columns, rowid, &old);
                }
                if let Some(position) = temp_position {
                    self.catalog.temp_tables[position].rows.remove(&rowid);
                }
                else {
                    self.writer.delete(table, rowid).map_err(|error| write_failure(error, "insert", def))?;
                }
                deleted.push((rowid, old.clone()));
                if queue_triggers && recursive_triggers {
                    self.queue_delete_after(table_key, trigger_columns, rowid, &old);
                }
            }
            if queue_triggers {
                self.pending_events.push(PendingTriggerEvent {
                    table: table_key.to_string(),
                    timing: TriggerTiming::Before,
                    event: TriggerEvent::Insert,
                    columns: trigger_columns.to_vec(),
                    new: Some(check_row(def, slots, rowid)),
                    old: None,
                });
            }
            return match temp_position {
                Some(position) => {
                    let keys = self.unique_keys(def, table_key);
                    let table_state = &mut self.catalog.temp_tables[position];
                    temp_insert_row(def, &mut table_state.rows, slots, &keys)
                }
                None => self.writer.insert(table, slots).map_err(|error| storage_failure(error).in_statement("insert")),
            };
        }
        let live = self.live_rows(def, "insert")?;
        let candidate_rowid = match explicit_rowid {
            Some(rowid) => rowid,
            None => live.iter().map(|(rowid, _)| *rowid).max().unwrap_or(0) + 1,
        };
        let candidate = check_row(def, slots, candidate_rowid);
        let mut conflicts: BTreeSet<i64> = BTreeSet::new();
        for unique in self.unique_keys(def, table_key) {
            for (rowid, cells) in &live {
                if upsert_key_conflicts(&unique, &candidate, candidate_rowid, *rowid, cells) {
                    conflicts.insert(*rowid);
                }
            }
        }
        for target in self.upsert_managed_indexes(def, table_key) {
            let Some(wanted) = self.managed_target_key(def, &target, &candidate)?
            else {
                continue;
            };
            for (rowid, cells) in &live {
                if let Some(other) = self.managed_target_key(def, &target, cells)? {
                    if managed_keys_conflict(&wanted, &other, &target.index.collations) {
                        conflicts.insert(*rowid);
                    }
                }
            }
        }
        for rowid in conflicts {
            let old = live.iter().find(|(candidate, _)| *candidate == rowid).map(|(_, cells)| cells.clone()).unwrap_or_default();
            if queue_triggers && recursive_triggers {
                self.queue_delete_before(table_key, trigger_columns, rowid, &old);
            }
            if let Some(position) = temp_position {
                self.catalog.temp_tables[position].rows.remove(&rowid);
            }
            else {
                let removed = if def.without_rowid {
                    let old_values: Vec<crate::WriteValue> = old.iter().map(cell_write_value).collect();
                    self.writer.delete_without_rowid(table, &old_values)
                }
                else {
                    self.writer.delete(table, rowid)
                };
                if let Err(error) = removed {
                    return Err(write_failure(error, "insert", def));
                }
            }
            deleted.push((rowid, old.clone()));
            if queue_triggers && recursive_triggers {
                // `AFTER DELETE` is queued as soon as the row is gone, so the
                // replacement's delete trigger runs before its insert trigger.
                self.queue_delete_after(table_key, trigger_columns, rowid, &old);
            }
        }
        if queue_triggers {
            // The conflict-resolution deletes have already run, so a
            // `BEFORE INSERT` trigger observes the state after them.
            let before_rowid = def
                .rowid_alias
                .and_then(|alias| slots.get(alias).and_then(|slot| slot.as_ref()))
                .and_then(|value| match value {
                    crate::WriteValue::Integer(value) => Some(*value),
                    _ => None,
                })
                .unwrap_or(-1);
            self.pending_events.push(PendingTriggerEvent {
                table: table_key.to_string(),
                timing: TriggerTiming::Before,
                event: TriggerEvent::Insert,
                columns: trigger_columns.to_vec(),
                new: Some(check_row(def, slots, before_rowid)),
                old: None,
            });
        }
        if let Some(position) = temp_position {
            let keys = self.unique_keys(def, table_key);
            let table_state = &mut self.catalog.temp_tables[position];
            temp_insert_row(def, &mut table_state.rows, slots, &keys)
        }
        else {
            self.writer.insert(table, slots).map_err(|error| storage_failure(error).in_statement("insert"))
        }
    }

    /// Computes the `(key values, rowid)` entries of one managed index by
    /// evaluating its terms and predicate against every live row.
    fn managed_index_entries(
        &self,
        def: &TableDef,
        terms: &[ResolvedExpr],
        predicate: Option<&ResolvedExpr>,
        rows: &[(i64, Vec<Cell>)],
    ) -> Result<Vec<(Vec<crate::WriteValue>, i64)>, SqlError> {
        let shape = TargetShape::new(def, &def.name, None);
        let budget = Budget::new();
        let aggregates: Vec<Cell> = Vec::new();
        let windows: Vec<Vec<Cell>> = Vec::new();
        let mut entries = Vec::with_capacity(rows.len());
        for (rowid, cells) in rows {
            let context = EvalContext {
                engine: self,
                outer: &[],
                current: &shape.scope,
                aggregates: &aggregates,
                windows: &windows,
                window_row: 0,
                budget: &budget,
                ctes: None,
            };
            if let Some(predicate) = predicate {
                if truthiness(&predicate.eval(cells, &context)?) != Some(true) {
                    continue;
                }
            }
            let mut key = Vec::with_capacity(terms.len());
            for term in terms {
                key.push(cell_write_value(&term.eval(cells, &context)?));
            }
            entries.push((key, *rowid));
        }
        Ok(entries)
    }

    /// Rebuilds the key sets of every managed index on `table` from the live
    /// rows. Called inside the statement's transaction, before it commits.
    fn refresh_managed_indexes(&mut self, table: &str, def: &TableDef, statement: &'static str) -> Result<(), SqlError> {
        let managed: Vec<IndexDef> =
            self.catalog.indexes.values().filter(|index| index.table.eq_ignore_ascii_case(table) && index.managed).cloned().collect();
        if managed.is_empty() {
            return Ok(());
        }
        let rows = self.live_rows(def, statement)?;
        for index in managed {
            let entries = self.managed_index_entries(def, &index.terms, index.predicate.as_ref(), &rows)?;
            self.writer.replace_managed_index_entries(&index.name, &entries).map_err(|error| storage_failure(error).in_statement(statement))?;
        }
        Ok(())
    }

    /// Resolves one foreign key against the live catalog: the parent table
    /// position and the parent key column positions.
    ///
    /// A missing parent table and a parent key that is not unique both report
    /// the codes SQLite uses (`SQLITE_ERROR`), because the same statements
    /// fail identically in the reference engine.
    fn foreign_key_parent(&self, child: &TableDef, key: &ForeignKeyDef, statement: &'static str) -> Result<(usize, Vec<usize>), SqlError> {
        let parent_position = *self
            .catalog
            .table_lookup
            .get(&key.parent)
            .ok_or_else(|| SqlError::new("SQLITE_ERROR", format!("no such table: main.{}", key.parent_display)).in_statement(statement))?;
        let parent = &self.catalog.tables[parent_position];
        let positions = if key.parent_columns.is_empty() {
            parent.primary_key_positions().ok_or_else(|| foreign_key_mismatch(child, key, statement))?
        }
        else {
            let mut positions = Vec::with_capacity(key.parent_columns.len());
            for name in &key.parent_columns {
                let position = parent
                    .columns
                    .iter()
                    .position(|column| column.name.eq_ignore_ascii_case(name))
                    .ok_or_else(|| foreign_key_mismatch(child, key, statement))?;
                positions.push(position);
            }
            positions
        };
        if positions.len() != key.columns.len() || !self.parent_key_is_unique(parent, &positions) {
            return Err(foreign_key_mismatch(child, key, statement));
        }
        Ok((parent_position, positions))
    }

    /// Whether `positions` identifies at most one parent row: the primary key,
    /// a column-level `UNIQUE` / `PRIMARY KEY`, or an explicit unique index.
    fn parent_key_is_unique(&self, parent: &TableDef, positions: &[usize]) -> bool {
        if let Some(alias) = parent.rowid_alias {
            if positions == [alias] {
                return true;
            }
        }
        if !parent.primary_key.is_empty() && same_positions(&parent.primary_key, positions) {
            return true;
        }
        let declared: Vec<usize> =
            parent.columns.iter().enumerate().filter(|(_, column)| column.primary_key).map(|(position, _)| position).collect();
        if !declared.is_empty() && same_positions(&declared, positions) {
            return true;
        }
        if positions.len() == 1 && parent.columns[positions[0]].unique {
            return true;
        }
        if positions.iter().all(|position| parent.columns[*position].unique) {
            return true;
        }
        let parent_key = parent.name.to_ascii_lowercase();
        self.catalog.indexes.values().any(|index| index.table == parent_key && index.unique && same_positions(&index.columns, positions))
    }

    /// Every foreign key in the catalog that references `parent`, as
    /// `(child table position, key)` pairs; the child may be `parent` itself.
    fn referencing_foreign_keys(&self, parent: &str) -> Vec<(usize, ForeignKeyDef)> {
        let mut found = Vec::new();
        for (position, table) in self.catalog.tables.iter().enumerate() {
            for key in &table.foreign_keys {
                if key.parent.eq_ignore_ascii_case(parent) {
                    found.push((position, key.clone()));
                }
            }
        }
        found
    }

    /// Verifies the rows an `INSERT` just wrote against every foreign key of
    /// their table. The check runs after the writes so a self-reference may
    /// point at a later row of the same statement.
    fn check_insert_foreign_keys(&self, def: &TableDef, rowids: &[i64], statement: &'static str) -> Result<(), SqlError> {
        if def.foreign_keys.is_empty() {
            return Ok(());
        }
        let live = self.live_rows(def, statement)?;
        for key in &def.foreign_keys {
            if key.deferred && self.in_transaction() {
                continue;
            }
            let (parent_position, parent_columns) = self.foreign_key_parent(def, key, statement)?;
            let parent = &self.catalog.tables[parent_position];
            let parent_rows = self.live_rows(parent, statement)?;
            for (rowid, cells) in &live {
                if !rowids.contains(rowid) {
                    continue;
                }
                let values = pick(cells, &key.columns);
                // A NULL in any child key column satisfies the constraint.
                if values.iter().any(|cell| matches!(cell, Cell::Null)) {
                    continue;
                }
                if !parent_rows.iter().any(|(_, parent_cells)| keys_equal(&values, parent_cells, &parent_columns)) {
                    return Err(foreign_key_violation(statement));
                }
            }
        }
        Ok(())
    }

    /// Verifies an `UPDATE` against both sides of every foreign key it touches.
    ///
    /// Child keys are re-checked only when the statement assigns one of their
    /// columns. Referenced parent keys are re-checked only when the statement
    /// assigns one of their columns, and then only child rows still holding a
    /// value the update removed are examined.
    fn check_update_foreign_keys(
        &self,
        def: &TableDef,
        plan: &UpdatePlan,
        old_rows: &[(i64, Vec<Cell>)],
        assigned: &[bool],
        statement: &'static str,
    ) -> Result<(), SqlError> {
        for key in &def.foreign_keys {
            if key.deferred && self.in_transaction() {
                continue;
            }
            if !key.columns.iter().any(|position| assigned[*position]) {
                continue;
            }
            let (parent_position, parent_columns) = self.foreign_key_parent(def, key, statement)?;
            let parent_rows = self.live_rows(&self.catalog.tables[parent_position], statement)?;
            for (rowid, values) in &plan.writes {
                let old = row_of(old_rows, *rowid);
                let next = apply_assignment(old, values);
                let child_values = pick(&next, &key.columns);
                if child_values.iter().any(|cell| matches!(cell, Cell::Null)) {
                    continue;
                }
                if !parent_rows.iter().any(|(_, parent_cells)| keys_equal(&child_values, parent_cells, &parent_columns)) {
                    return Err(foreign_key_violation(statement));
                }
            }
        }
        for (child_position, key) in self.referencing_foreign_keys(&def.name) {
            if key.deferred && self.in_transaction() && key.on_update_action != ForeignKeyAction::Restrict {
                continue;
            }
            let child = &self.catalog.tables[child_position];
            let (parent_position, parent_columns) = self.foreign_key_parent(child, &key, statement)?;
            if !parent_columns.iter().any(|position| assigned.get(*position).copied().unwrap_or(false)) {
                continue;
            }
            let mut changed: Vec<Vec<Cell>> = Vec::new();
            for (rowid, values) in &plan.writes {
                let old = row_of(old_rows, *rowid);
                let old_values = pick(old, &parent_columns);
                if old_values.iter().any(|cell| matches!(cell, Cell::Null)) {
                    continue;
                }
                let next = apply_assignment(old, values);
                let new_values = pick(&next, &parent_columns);
                if !cells_equal(&old_values, &new_values) {
                    changed.push(old_values);
                }
            }
            if changed.is_empty() {
                continue;
            }
            let parent_rows = self.live_rows(&self.catalog.tables[parent_position], statement)?;
            let child_rows = self.live_rows(child, statement)?;
            for (_, child_cells) in &child_rows {
                let child_values = pick(child_cells, &key.columns);
                if child_values.iter().any(|cell| matches!(cell, Cell::Null)) {
                    continue;
                }
                if changed.iter().any(|value| cells_equal(value, &child_values))
                    && !parent_rows.iter().any(|(_, parent_cells)| keys_equal(&child_values, parent_cells, &parent_columns))
                {
                    return Err(foreign_key_violation(statement));
                }
            }
        }
        Ok(())
    }

    /// Verifies a `DELETE`: its own foreign keys are still resolved (a malformed
    /// or missing parent fails the statement like SQLite), and every child row
    /// that referenced a removed parent key is looked for after the removal.
    fn check_delete_foreign_keys(
        &self,
        def: &TableDef,
        old_rows: &[(i64, Vec<Cell>)],
        removed: &[i64],
        statement: &'static str,
    ) -> Result<(), SqlError> {
        for key in &def.foreign_keys {
            self.foreign_key_parent(def, key, statement)?;
        }
        for (child_position, key) in self.referencing_foreign_keys(&def.name) {
            if key.deferred && self.in_transaction() && key.on_delete_action != ForeignKeyAction::Restrict {
                continue;
            }
            let child = &self.catalog.tables[child_position];
            let (_, parent_columns) = self.foreign_key_parent(child, &key, statement)?;
            let mut gone: Vec<Vec<Cell>> = Vec::new();
            for (rowid, cells) in old_rows {
                if !removed.contains(rowid) {
                    continue;
                }
                let values = pick(cells, &parent_columns);
                if values.iter().any(|cell| matches!(cell, Cell::Null)) {
                    continue;
                }
                gone.push(values);
            }
            if gone.is_empty() {
                continue;
            }
            let child_rows = self.live_rows(child, statement)?;
            for (_, child_cells) in &child_rows {
                let child_values = pick(child_cells, &key.columns);
                if child_values.iter().any(|cell| matches!(cell, Cell::Null)) {
                    continue;
                }
                if gone.iter().any(|value| cells_equal(value, &child_values)) {
                    return Err(foreign_key_violation(statement));
                }
            }
        }
        Ok(())
    }

    /// Re-checks every deferred foreign key against the state `COMMIT` would
    /// publish. Immediate keys were already enforced per statement, so only
    /// deferred keys are walked. A violation leaves the transaction open, as
    /// SQLite does, so the caller can repair the rows and commit again.
    fn check_deferred_foreign_keys(&self) -> Result<(), SqlError> {
        if !self.foreign_keys {
            return Ok(());
        }
        for child in &self.catalog.tables {
            for key in child.foreign_keys.iter().filter(|key| key.deferred) {
                let (parent_position, parent_columns) = self.foreign_key_parent(child, key, "commit")?;
                let parent = &self.catalog.tables[parent_position];
                let parent_rows = self.live_rows(parent, "commit")?;
                let child_rows = self.live_rows(child, "commit")?;
                for (_, cells) in &child_rows {
                    let values = pick(cells, &key.columns);
                    if values.iter().any(|cell| matches!(cell, Cell::Null)) {
                        continue;
                    }
                    if !parent_rows.iter().any(|(_, parent_cells)| keys_equal(&values, parent_cells, &parent_columns)) {
                        return Err(foreign_key_violation("commit"));
                    }
                }
            }
        }
        Ok(())
    }

    /// Applies the `ON DELETE` / `ON UPDATE` actions of every foreign key that
    /// references `parent`, recursing into the tables those actions touch.
    ///
    /// `changes` carries the cells every changed parent row had before the
    /// statement and, for a rewrite, the cells it has now; `None` means the row
    /// was removed. `NO ACTION` and `RESTRICT` rows are left for the
    /// end-of-statement checks. Returns whether a touched child owns
    /// materialized indexes or a stored WITHOUT ROWID b-tree.
    fn apply_foreign_key_actions(
        &mut self,
        parent: &TableDef,
        changes: &[(Vec<Cell>, Option<Vec<Cell>>)],
        statement: &'static str,
        depth: usize,
    ) -> Result<(bool, bool), SqlError> {
        if depth > 100 {
            return Err(SqlError::new("constraint-failed", "foreign key cascade exceeded the depth limit").in_statement(statement));
        }
        let deleting = changes.iter().all(|(_, next)| next.is_none());
        let mut needs_index = false;
        let mut needs_without_rowid = false;
        for (child_position, key) in self.referencing_foreign_keys(&parent.name) {
            let action = if deleting { key.on_delete_action } else { key.on_update_action };
            if matches!(action, ForeignKeyAction::NoAction | ForeignKeyAction::Restrict) {
                continue;
            }
            let child = self.catalog.tables[child_position].clone();
            if child.temp {
                continue;
            }
            let (_, parent_columns) = self.foreign_key_parent(&child, &key, statement)?;
            let mut pairs: Vec<(Vec<Cell>, Option<Vec<Cell>>)> = Vec::new();
            for (old, next) in changes {
                let old_values = pick(old, &parent_columns);
                if old_values.iter().any(|cell| matches!(cell, Cell::Null)) {
                    continue;
                }
                let new_values = next.as_ref().map(|cells| pick(cells, &parent_columns));
                if let Some(values) = &new_values {
                    if cells_equal(&old_values, values) {
                        continue;
                    }
                }
                pairs.push((old_values, new_values));
            }
            if pairs.is_empty() {
                continue;
            }
            let child_rows = self.live_rows(&child, statement)?;
            let child_key = child.name.to_ascii_lowercase();
            let frame_columns = trigger_frame_columns(&child);
            let fires = self.table_has_triggers(&child_key);
            let assigned: Vec<String> = key.columns.iter().map(|position| child.columns[*position].name.clone()).collect();
            let mut deleted: Vec<(i64, Vec<Cell>)> = Vec::new();
            let mut updated: Vec<(i64, Vec<Cell>, Vec<Cell>)> = Vec::new();
            for (rowid, cells) in &child_rows {
                let values = pick(cells, &key.columns);
                if values.iter().any(|cell| matches!(cell, Cell::Null)) {
                    continue;
                }
                let Some((_, next)) = pairs.iter().find(|(old, _)| cells_equal(old, &values))
                else {
                    continue;
                };
                match action {
                    ForeignKeyAction::Cascade => match next {
                        None => deleted.push((*rowid, cells.clone())),
                        Some(next) => {
                            let mut rewritten = cells.clone();
                            for (position, value) in key.columns.iter().zip(next.iter()) {
                                rewritten[*position] = value.clone();
                            }
                            updated.push((*rowid, cells.clone(), rewritten));
                        }
                    },
                    ForeignKeyAction::SetNull | ForeignKeyAction::SetDefault => {
                        let mut rewritten = cells.clone();
                        for position in &key.columns {
                            let column = &child.columns[*position];
                            let value = match action {
                                ForeignKeyAction::SetNull => Cell::Null,
                                _ => {
                                    let default = column.default_value.clone().unwrap_or(crate::WriteValue::Null);
                                    write_value_cell(&crate::apply_write_affinity(&column.declared_type, default))
                                }
                            };
                            if matches!(value, Cell::Null) && column.not_null {
                                return Err(SqlError::new(
                                    "not-null-constraint",
                                    format!("NOT NULL constraint failed: {}.{}", child.name, column.name),
                                )
                                .in_statement(statement));
                            }
                            rewritten[*position] = value;
                        }
                        updated.push((*rowid, cells.clone(), rewritten));
                    }
                    _ => {}
                }
            }
            if deleted.is_empty() && updated.is_empty() {
                continue;
            }
            let table_key = child.name.clone();
            for (rowid, old) in &deleted {
                if fires {
                    self.queue_delete_before(&table_key, &frame_columns, *rowid, old);
                }
                let written = if child.without_rowid {
                    let values: Vec<crate::WriteValue> = old.iter().map(cell_write_value).collect();
                    self.writer.delete_without_rowid(&table_key, &values)
                }
                else {
                    self.writer.delete(&table_key, *rowid)
                };
                written.map_err(|error| write_failure(error, statement, &child))?;
                if fires {
                    self.queue_delete_after(&table_key, &frame_columns, *rowid, old);
                }
            }
            for (rowid, old, next) in &updated {
                if fires {
                    self.queue_update_before(&table_key, &child, &frame_columns, &assigned, *rowid, old, next);
                }
                let written = if child.without_rowid {
                    let old_values: Vec<crate::WriteValue> = old.iter().map(cell_write_value).collect();
                    let new_values: Vec<crate::WriteValue> = next.iter().map(cell_write_value).collect();
                    self.writer.update_without_rowid(&table_key, &old_values, &new_values)
                }
                else {
                    let values: Vec<UpdateValue> = next.iter().map(|cell| UpdateValue::Set(cell_write_value(cell))).collect();
                    self.writer.update(&table_key, *rowid, &values)
                };
                written.map_err(|error| write_failure(error, statement, &child))?;
                if fires {
                    self.queue_update_after(&table_key, &child, &frame_columns, &assigned, *rowid, old, next);
                }
            }
            // Rewritten child rows still have to resolve every foreign key,
            // which catches `SET DEFAULT` values that name no parent.
            let validated: Vec<i64> = updated
                .iter()
                .map(|(rowid, _, next)| {
                    child
                        .rowid_alias
                        .and_then(|alias| match next.get(alias) {
                            Some(Cell::Integer(value)) => Some(*value),
                            _ => None,
                        })
                        .unwrap_or(*rowid)
                })
                .collect();
            if !validated.is_empty() {
                self.check_insert_foreign_keys(&child, &validated, statement)?;
            }
            self.refresh_managed_indexes(&child_key, &child, statement)?;
            if self.has_indexes(&child, &child_key) {
                needs_index = true;
            }
            if child.without_rowid {
                needs_without_rowid = true;
            }
            let child_changes: Vec<(Vec<Cell>, Option<Vec<Cell>>)> = if deleting {
                deleted.iter().map(|(_, old)| (old.clone(), None)).collect()
            }
            else {
                updated.iter().map(|(_, old, next)| (old.clone(), Some(next.clone()))).collect()
            };
            let (index_needed, without_needed) = self.apply_foreign_key_actions(&child, &child_changes, statement, depth + 1)?;
            needs_index |= index_needed;
            needs_without_rowid |= without_needed;
        }
        Ok((needs_index, needs_without_rowid))
    }

    /// Every uniqueness rule an `UPDATE` has to re-check, in the order SQLite
    /// reports them: the rowid first, then the primary key, then column-level
    /// `UNIQUE`, then explicit unique indexes.
    fn unique_keys(&self, def: &TableDef, table_key: &str) -> Vec<UniqueKey> {
        let label = |positions: &[usize]| {
            positions.iter().map(|position| format!("{}.{}", def.name, def.columns[*position].name)).collect::<Vec<_>>().join(", ")
        };
        let rowid_label = match def.rowid_alias {
            Some(alias) => format!("{}.{}", def.name, def.columns[alias].name),
            None => format!("{}.rowid", def.name),
        };
        let mut keys = Vec::new();
        if !def.without_rowid {
            keys.push(UniqueKey::Rowid(rowid_label));
        }
        // A WITHOUT ROWID table has no rowid and its primary key is the
        // clustered key, so that key is the first uniqueness rule.
        if def.without_rowid && !def.primary_key.is_empty() {
            keys.push(UniqueKey::Columns(def.primary_key.clone(), label(&def.primary_key), self.key_collations(def, &def.primary_key)));
        }
        else if def.primary_key_index && !def.primary_key.is_empty() {
            keys.push(UniqueKey::Columns(def.primary_key.clone(), label(&def.primary_key), self.key_collations(def, &def.primary_key)));
        }
        for (position, column) in def.columns.iter().enumerate() {
            if Some(position) == def.rowid_alias {
                continue;
            }
            if column.primary_key || column.unique {
                keys.push(UniqueKey::Columns(vec![position], label(&[position]), self.key_collations(def, &[position])));
            }
        }
        for constraint in &def.table_constraints {
            if !constraint.primary {
                keys.push(UniqueKey::Columns(
                    constraint.columns.clone(),
                    label(&constraint.columns),
                    self.key_collations(def, &constraint.columns),
                ));
            }
        }
        for index in self.catalog.indexes.values() {
            // Managed indexes are expression or partial, so their uniqueness is
            // enforced while their keys are rebuilt from the live rows.
            if !def.temp && !index.managed && index.table == table_key && index.unique {
                let collations = if index.collations.len() == index.columns.len() {
                    index.collations.clone()
                }
                else {
                    self.key_collations(def, &index.columns)
                };
                keys.push(UniqueKey::Columns(index.columns.clone(), label(&index.columns), collations));
            }
        }
        keys
    }

    /// 各键位按列声明取得的排序规则，供唯一性与冲突检测使用。
    fn key_collations(&self, def: &TableDef, positions: &[usize]) -> Vec<Collation> {
        positions.iter().map(|position| def.columns[*position].collate.unwrap_or_default()).collect()
    }

    /// True when the table owns any index whose b-tree has to be rewritten
    /// after a write.
    fn has_indexes(&self, def: &TableDef, table_key: &str) -> bool {
        def.primary_key_index
            || def
                .columns
                .iter()
                .enumerate()
                .any(|(position, column)| (column.primary_key || column.unique) && Some(position) != def.rowid_alias)
            || def.table_constraints.iter().any(|constraint| !constraint.primary)
            || (!def.temp && self.catalog.indexes.values().any(|index| index.table == table_key))
    }

    /// Plans one `UPDATE` without touching storage: every right-hand side is
    /// evaluated against the pre-statement row, assignments apply left to right
    /// (last one wins), and each row is checked against the *current* table, so
    /// a rowid shift behaves exactly like SQLite's ascending-order pass.
    ///
    /// `joined` carries the row shape of an `UPDATE ... FROM` statement: the
    /// target columns, its rowid, and every `FROM` source appended, for the
    /// target rows whose join produced a match. A row outside the map is not
    /// updated, and the predicate has already been applied to its merged row.
    fn plan_update(
        &self,
        def: &TableDef,
        table_key: &str,
        targets: &[(usize, ResolvedExpr)],
        predicate: Option<&ResolvedExpr>,
        joined: Option<&BTreeMap<i64, Vec<Cell>>>,
        returning: Option<&ReturningPlan>,
        shape: &ScopeShape,
        budget: &Budget,
        state_override: Option<&[(i64, Vec<Cell>)]>,
        candidates: Option<&[i64]>,
    ) -> Result<UpdatePlan, SqlError> {
        let width = def.columns.len();
        let keys = self.unique_keys(def, table_key);
        let owned_live = match state_override {
            Some(_) => None,
            None => Some(match candidates {
                Some(rowids) => self.live_rows_at(def, "update", rowids)?,
                None => self.live_rows(def, "update")?,
            }),
        };
        let live: &[(i64, Vec<Cell>)] = match state_override {
            Some(rows) => rows,
            None => owned_live.as_deref().unwrap_or_default(),
        };
        // 冲突判定的状态只在存在非 rowid 唯一键时才需要整份拷贝。rowid 唯一性
        // 用活动 rowid 集合判定,于是整表 `UPDATE` 每行不再扫一遍整张表。
        let column_keys: Vec<&UniqueKey> = keys.iter().filter(|key| matches!(key, UniqueKey::Columns(..))).collect();
        let rowid_label: Option<&str> = keys.iter().find_map(|key| match key {
            UniqueKey::Rowid(label) => Some(label.as_str()),
            UniqueKey::Columns(..) => None,
        });
        let mut state = if column_keys.is_empty() { Vec::new() } else { live.to_vec() };
        // 只有赋值可能改动 rowid 时才需要活动 rowid 集合:整表更新通常一个
        // `INTEGER PRIMARY KEY` 都没碰,构建与逐行维护这份集合纯属浪费。
        let rowid_move_possible = targets.iter().any(|(index, _)| Some(*index) == def.rowid_alias);
        let mut live_rowids: HashSet<i64> = if rowid_move_possible { live.iter().map(|(rowid, _)| *rowid).collect() } else { HashSet::new() };
        let mut writes = Vec::new();
        let mut returned = Vec::new();
        let mut affected = 0usize;
        // 逐行复用的求值与赋值缓冲:整表 `UPDATE` 因此不再为每一行重新分配行向量。
        let mut eval_cells: Vec<Cell> = Vec::with_capacity(width + 1);
        let mut next: Vec<Cell> = Vec::with_capacity(width);
        let mut assigned = vec![false; width];
        for (position, (rowid, cells)) in live.iter().enumerate() {
            eval_cells.clear();
            match joined {
                Some(joined) => match joined.get(rowid) {
                    Some(row) => eval_cells.extend_from_slice(row),
                    None => continue,
                },
                None => {
                    eval_cells.extend_from_slice(cells);
                    eval_cells.push(Cell::Integer(*rowid));
                }
            }
            let ctx =
                EvalContext { engine: self, outer: &[], current: shape, aggregates: &[], windows: &[], window_row: 0, budget, ctes: None };
            if joined.is_none() {
                if let Some(predicate) = predicate {
                    if truthiness(&predicate.eval(&eval_cells, &ctx)?) != Some(true) {
                        continue;
                    }
                }
            }
            next.clear();
            next.extend_from_slice(cells);
            assigned.fill(false);
            let mut next_rowid = *rowid;
            for (index, target) in targets {
                let value = target.eval(&eval_cells, &ctx)?;
                let column = &def.columns[*index];
                let cell = write_value_cell(&crate::apply_write_affinity(&column.declared_type, cell_write_value(&value)));
                if Some(*index) == def.rowid_alias {
                    match cell {
                        Cell::Null => {
                            return Err(if column.not_null {
                                SqlError::new("not-null-constraint", format!("NOT NULL constraint failed: {}.{}", def.name, column.name))
                            }
                            else {
                                SqlError::new("datatype-mismatch", "datatype mismatch")
                            }
                            .in_statement("update"));
                        }
                        Cell::Integer(value) => {
                            next_rowid = value;
                            next[*index] = Cell::Integer(value);
                        }
                        _ => return Err(SqlError::new("datatype-mismatch", "datatype mismatch").in_statement("update")),
                    }
                }
                else {
                    if matches!(cell, Cell::Null) && column.not_null {
                        return Err(SqlError::new("not-null-constraint", format!("NOT NULL constraint failed: {}.{}", def.name, column.name))
                            .in_statement("update"));
                    }
                    next[*index] = cell;
                }
                assigned[*index] = true;
            }
            if !def.checks.is_empty() {
                let mut check_cells = next.clone();
                check_cells.push(Cell::Integer(next_rowid));
                for check in &def.checks {
                    if truthiness(&check.expr.eval(&check_cells, &ctx)?) == Some(false) {
                        let name = check.name.clone().unwrap_or_else(|| check.text.clone());
                        return Err(SqlError::new("check-constraint", format!("CHECK constraint failed: {name}"))
                            .in_clause("CHECK")
                            .in_statement("update"));
                    }
                }
            }
            if let Some(label) = rowid_label {
                // `UNIQUE(rowid)` 只在换成另一个仍被占用的 rowid 时冲突。
                if next_rowid != *rowid && live_rowids.contains(&next_rowid) {
                    return Err(SqlError::new("unique-constraint", format!("UNIQUE constraint failed: {label}")).in_statement("update"));
                }
            }
            for key in &column_keys {
                if let Some(label) = unique_conflict(std::slice::from_ref(*key), &next, next_rowid, &state, *rowid) {
                    return Err(SqlError::new("unique-constraint", format!("UNIQUE constraint failed: {label}")).in_statement("update"));
                }
            }
            if !state.is_empty() && position < state.len() {
                state[position].0 = next_rowid;
                state[position].1 = next.clone();
            }
            if rowid_move_possible {
                live_rowids.remove(rowid);
                live_rowids.insert(next_rowid);
            }
            if let Some(returning) = returning {
                let mut new_cells = next.clone();
                new_cells.push(Cell::Integer(next_rowid));
                returned.push(returning.eval(&new_cells, &ctx)?);
            }
            let values = def
                .columns
                .iter()
                .enumerate()
                .map(|(index, _)| if assigned[index] { UpdateValue::Set(cell_write_value(&next[index])) } else { UpdateValue::Keep })
                .collect();
            writes.push((*rowid, values));
            affected += 1;
        }
        // 复用的快照优先直接交出所有权，避免再复制一份整表。
        let live_snapshot = match owned_live {
            Some(rows) => rows,
            None => live.to_vec(),
        };
        Ok(UpdatePlan { writes, returning: returned, affected, live: live_snapshot })
    }

    /// Plans one `DELETE`: the matching rowids in ascending order, plus any
    /// `RETURNING` rows, which see the row as it was before removal.
    ///
    /// `candidates` narrows the scan to the rowids a `WHERE <rowid> = <key>`
    /// equality or an index probe can reach. The predicate still runs over every
    /// candidate, so a stale or mismatched probe only costs the pushdown and
    /// never changes the removed set.
    fn plan_delete(
        &self,
        def: &TableDef,
        predicate: Option<&ResolvedExpr>,
        returning: Option<&ReturningPlan>,
        shape: &ScopeShape,
        budget: &Budget,
        candidates: Option<&[i64]>,
    ) -> Result<DeletePlan, SqlError> {
        let live = match candidates {
            Some(rowids) => self.live_rows_at(def, "delete", rowids)?,
            None => self.live_rows(def, "delete")?,
        };
        let mut removed = Vec::new();
        let mut returned = Vec::new();
        // 逐行复用的求值缓冲:批量删除不再为每一行复制一份行向量。
        let mut eval_cells: Vec<Cell> = Vec::with_capacity(def.columns.len() + 1);
        for (rowid, cells) in &live {
            eval_cells.clear();
            eval_cells.extend_from_slice(cells);
            eval_cells.push(Cell::Integer(*rowid));
            let ctx =
                EvalContext { engine: self, outer: &[], current: shape, aggregates: &[], windows: &[], window_row: 0, budget, ctes: None };
            if let Some(predicate) = predicate {
                if truthiness(&predicate.eval(&eval_cells, &ctx)?) != Some(true) {
                    continue;
                }
            }
            if let Some(returning) = returning {
                returned.push(returning.eval(&eval_cells, &ctx)?);
            }
            removed.push(*rowid);
        }
        Ok(DeletePlan { removed, returning: returned, live })
    }

    /// Executes one `SELECT` over the comma-separated `FROM` list.
    ///
    /// The sources are flattened into one row shape in `FROM` order, then
    /// combined as a Cartesian product. Predicates are applied as soon as every
    /// source they read is bound, so an unselective `WHERE` never materializes
    /// the full product. Nested subqueries re-enter this same function with the
    /// current row pushed onto `outer`.
    fn select(
        &self,
        first: &SelectCore,
        compounds: &[CompoundTerm],
        order: Vec<OrderKey>,
        limit: Option<Limit>,
        outer: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv>,
    ) -> Result<ExecResult, SqlError> {
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
                return Ok(ExecResult {
                    statement: "select",
                    columns: labels,
                    rows: if visible { vec![vec![Cell::Integer(count)]] } else { Vec::new() },
                    affected: 0,
                });
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
        let ctx =
            EvalContext { engine: self, outer, current: &shape, aggregates: &empty_aggregates, windows: &[], window_row: 0, budget, ctes };
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
            return Ok(ExecResult { statement: "select", columns: labels, rows, affected: 0 });
        }
        if bare_count_star && !joined {
            let count = count_cross_product_rows(&tables, &relation, &conjuncts, &ctx)?;
            let visible = match limit {
                None => true,
                Some((offset, limit)) => offset == 0 && limit.map_or(true, |limit| limit > 0),
            };
            let rows = if visible { vec![vec![Cell::Integer(count as i64)]] } else { Vec::new() };
            return Ok(ExecResult { statement: "select", columns: labels, rows, affected: 0 });
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
                let group_ctx =
                    EvalContext { engine: self, outer, current: &shape, aggregates: &aggregates, windows: &[], window_row: 0, budget, ctes };
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
                    engine: self,
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
            return Ok(ExecResult { statement: "select", columns: labels, rows: out, affected: 0 });
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
                    engine: self,
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
                            engine: self,
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
            return Ok(ExecResult { statement: "select", columns: labels, rows: out, affected: 0 });
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
                Ok(ExecResult { statement: "select", columns: labels, rows, affected: 0 })
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
                    return Ok(ExecResult { statement: "select", columns: labels, rows: final_rows, affected: 0 });
                }
                Ok(ExecResult {
                    statement: "select",
                    columns: labels,
                    rows: final_rows.iter().map(|row| relation.visible_row(row)).collect(),
                    affected: 0,
                })
            }
            SelectItems::Star => Ok(ExecResult {
                statement: "select",
                columns: labels,
                rows: final_rows.iter().map(|row| relation.visible_row(row)).collect(),
                affected: 0,
            }),
            SelectItems::Named(_) => {
                let nodes = projection.expect("named projection is resolved up front");
                let mut out = Vec::with_capacity(final_rows.len());
                for (index, values) in final_rows.iter().enumerate() {
                    let row_ctx = EvalContext {
                        engine: self,
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
                Ok(ExecResult { statement: "select", columns: labels, rows: out, affected: 0 })
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
    fn count_only_scan(
        &self,
        first: &SelectCore,
        order: &[OrderKey],
        limit: Option<(usize, Option<usize>)>,
        outer: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv>,
    ) -> Result<Option<ExecResult>, SqlError> {
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
        let ctx =
            EvalContext { engine: self, outer, current: &shape, aggregates: &empty_aggregates, windows: &[], window_row: 0, budget, ctes };
        let count = self.count_scan_rows(def, &keep, &conjuncts, &ctx)?;
        let labels = core_output_labels(&relation, &first.items);
        let visible = match limit {
            None => true,
            Some((offset, limit)) => offset == 0 && limit.map_or(true, |limit| limit > 0),
        };
        let rows = if visible { vec![vec![Cell::Integer(count as i64)]] } else { Vec::new() };
        Ok(Some(ExecResult { statement: "select", columns: labels, rows, affected: 0 }))
    }

    /// Counts the rows of one table whose `WHERE` conjuncts are all SQL-`TRUE`.
    ///
    /// Records are decoded into a single reused buffer, so a counting scan
    /// allocates one row vector instead of one per stored row.
    fn count_scan_rows(&self, def: &TableDef, keep: &[bool], conjuncts: &[ResolvedExpr], ctx: &EvalContext) -> Result<usize, SqlError> {
        let stored =
            self.writer.table_rows(&def.name, TableLimits::default()).map_err(|error| storage_failure(error).in_statement("select"))?;
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
    fn eval_limit(
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
        let ctx = EvalContext { engine: self, outer, current: &shape, aggregates: &empty, windows: &[], window_row: 0, budget, ctes };
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
    fn compound_select(
        &self,
        first: &SelectCore,
        compounds: &[CompoundTerm],
        order: Vec<OrderKey>,
        limit: Option<(usize, Option<usize>)>,
        outer: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv>,
    ) -> Result<ExecResult, SqlError> {
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
        Ok(ExecResult { statement: "select", columns, rows, affected: 0 })
    }

    /// Computes every window of one `SELECT` core over its input rows.
    ///
    /// Returns one value column per window (indexed by input row), the
    /// composite sort key of every input row, and the descending flag of each
    /// key column. The composite key concatenates, for each window in
    /// declaration order, its partition keys then its order keys, so the first
    /// window is the primary sort and later windows only break its ties — the
    /// order SQLite emits when no trailing `ORDER BY` is written.
    fn evaluate_windows(
        &self,
        windows: &[ResolvedWindow],
        inputs: &[WindowRow],
        shape: &ScopeShape,
        outer: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv>,
    ) -> Result<(Vec<Vec<Cell>>, Vec<Vec<Cell>>, Vec<SortDirection>), SqlError> {
        let mut values: Vec<Vec<Cell>> = vec![Vec::new(); windows.len()];
        let mut keys: Vec<Vec<Cell>> = vec![Vec::new(); inputs.len()];
        let mut descending: Vec<SortDirection> = Vec::new();
        // Key expressions are evaluated once per window; the sorts then run
        // from the last window to the first. Each later window is a tie-breaker
        // of the previous one, which reproduces SQLite's sorter chain: the
        // first window is the primary order of the output, and within its peer
        // groups the later windows decide which row a ranking function sees
        // first.
        let mut partitions: Vec<Vec<Vec<Cell>>> = Vec::with_capacity(windows.len());
        let mut orders: Vec<Vec<Vec<Cell>>> = Vec::with_capacity(windows.len());
        for window in windows {
            let mut window_partitions: Vec<Vec<Cell>> = Vec::with_capacity(inputs.len());
            let mut window_orders: Vec<Vec<Cell>> = Vec::with_capacity(inputs.len());
            for row in inputs {
                let ctx =
                    EvalContext { engine: self, outer, current: shape, aggregates: &row.aggregates, windows: &[], window_row: 0, budget, ctes };
                let mut partition = Vec::with_capacity(window.partition.len());
                for expr in &window.partition {
                    partition.push(expr.eval(&row.values, &ctx)?);
                }
                let mut order = Vec::with_capacity(window.order.len());
                for (expr, _) in &window.order {
                    order.push(expr.eval(&row.values, &ctx)?);
                }
                window_partitions.push(partition);
                window_orders.push(order);
            }
            partitions.push(window_partitions);
            orders.push(window_orders);
            descending.resize(descending.len() + window.partition.len(), SortDirection::default());
            descending.extend(window.order.iter().map(|(_, direction)| *direction));
        }
        for (index, key_row) in keys.iter_mut().enumerate() {
            for window in 0..windows.len() {
                key_row.extend(partitions[window][index].iter().cloned());
                key_row.extend(orders[window][index].iter().cloned());
            }
        }
        let mut order: Vec<usize> = (0..inputs.len()).collect();
        for index in (0..windows.len()).rev() {
            let window = &windows[index];
            let window_partitions = &partitions[index];
            let window_orders = &orders[index];
            order.sort_by(|left, right| {
                compare_window_rows(
                    &window_partitions[*left],
                    &window_orders[*left],
                    &window_partitions[*right],
                    &window_orders[*right],
                    &window.order,
                )
            });
            let mut column = vec![Cell::Null; inputs.len()];
            let mut start = 0;
            while start < order.len() {
                let mut end = start + 1;
                while end < order.len() && cells_equal(&window_partitions[order[end]], &window_partitions[order[start]]) {
                    end += 1;
                }
                self.evaluate_window(window, &order[start..end], window_orders, inputs, &mut column, shape, outer, budget, ctes)?;
                start = end;
            }
            values[index] = column;
        }
        Ok((values, keys, descending))
    }

    /// Computes one window over one partition.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_window(
        &self,
        window: &ResolvedWindow,
        indices: &[usize],
        orders: &[Vec<Cell>],
        inputs: &[WindowRow],
        column: &mut [Cell],
        shape: &ScopeShape,
        outer: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv>,
    ) -> Result<(), SqlError> {
        let len = indices.len();
        // Peer groups: rows whose every ORDER BY value compares equal. With no
        // ORDER BY the whole partition is one peer group, which is what makes
        // ranking functions report ties across a whole partition.
        let mut peer_start = vec![0usize; len];
        let mut peer_end = vec![len - 1; len];
        let mut dense = vec![0i64; len];
        let mut group = 0i64;
        let mut at = 0;
        while at < len {
            let mut end = at + 1;
            while end < len && cells_equal(&orders[indices[at]], &orders[indices[end]]) {
                end += 1;
            }
            for slot in at..end {
                peer_start[slot] = at;
                peer_end[slot] = end - 1;
                dense[slot] = group;
            }
            group += 1;
            at = end;
        }
        match &window.function {
            ResolvedWindowFunction::RowNumber => {
                for slot in 0..len {
                    column[indices[slot]] = Cell::Integer(slot as i64 + 1);
                }
            }
            ResolvedWindowFunction::Rank => {
                for slot in 0..len {
                    column[indices[slot]] = Cell::Integer(peer_start[slot] as i64 + 1);
                }
            }
            ResolvedWindowFunction::DenseRank => {
                for slot in 0..len {
                    column[indices[slot]] = Cell::Integer(dense[slot] + 1);
                }
            }
            ResolvedWindowFunction::PercentRank => {
                for slot in 0..len {
                    column[indices[slot]] = if len == 1 { Cell::Real(0.0) } else { Cell::Real(peer_start[slot] as f64 / (len - 1) as f64) };
                }
            }
            ResolvedWindowFunction::CumeDist => {
                for slot in 0..len {
                    column[indices[slot]] = Cell::Real((peer_end[slot] + 1) as f64 / len as f64);
                }
            }
            ResolvedWindowFunction::Ntile(buckets) => {
                let ctx = self.window_context(inputs, indices[0], shape, outer, budget, ctes);
                let count = ntile_buckets(&buckets.eval(&inputs[indices[0]].values, &ctx)?)?;
                // Bigger buckets first: the first `len % count` buckets hold one
                // extra row.
                let base = len / count;
                let extra = len % count;
                let mut slot = 0;
                for bucket in 0..count {
                    let size = if bucket < extra { base + 1 } else { base };
                    for _ in 0..size {
                        if slot >= len {
                            break;
                        }
                        column[indices[slot]] = Cell::Integer(bucket as i64 + 1);
                        slot += 1;
                    }
                }
            }
            ResolvedWindowFunction::Lag { value, offset, default } => {
                self.evaluate_offset_function(value, offset, default, true, indices, inputs, column, shape, outer, budget, ctes)?;
            }
            ResolvedWindowFunction::Lead { value, offset, default } => {
                self.evaluate_offset_function(value, offset, default, false, indices, inputs, column, shape, outer, budget, ctes)?;
            }
            ResolvedWindowFunction::FirstValue(value) => {
                self.evaluate_frame_value(
                    ValuePick::First,
                    value,
                    None,
                    window,
                    indices,
                    orders,
                    inputs,
                    column,
                    &peer_start,
                    &peer_end,
                    &dense,
                    shape,
                    outer,
                    budget,
                    ctes,
                )?;
            }
            ResolvedWindowFunction::LastValue(value) => {
                self.evaluate_frame_value(
                    ValuePick::Last,
                    value,
                    None,
                    window,
                    indices,
                    orders,
                    inputs,
                    column,
                    &peer_start,
                    &peer_end,
                    &dense,
                    shape,
                    outer,
                    budget,
                    ctes,
                )?;
            }
            ResolvedWindowFunction::NthValue { value, position } => {
                self.evaluate_frame_value(
                    ValuePick::Nth,
                    value,
                    Some(position),
                    window,
                    indices,
                    orders,
                    inputs,
                    column,
                    &peer_start,
                    &peer_end,
                    &dense,
                    shape,
                    outer,
                    budget,
                    ctes,
                )?;
            }
            ResolvedWindowFunction::Aggregate { function, argument, separator, distinct, filter, order, collation } => {
                self.evaluate_frame_aggregate(
                    *function,
                    argument.as_ref(),
                    separator.as_ref(),
                    *distinct,
                    *collation,
                    filter.as_ref(),
                    order,
                    window,
                    indices,
                    orders,
                    inputs,
                    column,
                    &peer_start,
                    &peer_end,
                    &dense,
                    shape,
                    outer,
                    budget,
                    ctes,
                )?;
            }
        }
        Ok(())
    }

    /// Context of one input row, carrying that row's whole-result aggregates.
    fn window_context<'a>(
        &'a self,
        inputs: &'a [WindowRow],
        index: usize,
        shape: &'a ScopeShape,
        outer: &'a [Scope],
        budget: &'a Budget,
        ctes: Option<&'a CteEnv>,
    ) -> EvalContext<'a> {
        EvalContext { engine: self, outer, current: shape, aggregates: &inputs[index].aggregates, windows: &[], window_row: 0, budget, ctes }
    }

    /// `lag`/`lead`: the value of a row `offset` positions away.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_offset_function(
        &self,
        value: &ResolvedExpr,
        offset: &Option<ResolvedExpr>,
        default: &Option<ResolvedExpr>,
        backward: bool,
        indices: &[usize],
        inputs: &[WindowRow],
        column: &mut [Cell],
        shape: &ScopeShape,
        outer: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv>,
    ) -> Result<(), SqlError> {
        let len = indices.len();
        for slot in 0..len {
            let ctx = self.window_context(inputs, indices[slot], shape, outer, budget, ctes);
            let step = match offset {
                None => 1,
                Some(expr) => match lag_offset(&expr.eval(&inputs[indices[slot]].values, &ctx)?) {
                    Some(step) => step,
                    // A non-integer offset makes SQLite produce NULL, it does
                    // not raise, so the row contributes nothing.
                    None => {
                        column[indices[slot]] = Cell::Null;
                        continue;
                    }
                },
            };
            let target = if backward { (slot as i64).checked_sub(step) } else { (slot as i64).checked_add(step) };
            let target = match target {
                Some(target) if target >= 0 && (target as usize) < len => target as usize,
                _ => {
                    column[indices[slot]] = match default {
                        Some(default) => default.eval(&inputs[indices[slot]].values, &ctx)?,
                        None => Cell::Null,
                    };
                    continue;
                }
            };
            let target_ctx = self.window_context(inputs, indices[target], shape, outer, budget, ctes);
            column[indices[slot]] = value.eval(&inputs[indices[target]].values, &target_ctx)?;
        }
        Ok(())
    }

    /// `first_value`/`last_value`/`nth_value`: a value read from the frame.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_frame_value(
        &self,
        pick: ValuePick,
        value: &ResolvedExpr,
        position: Option<&ResolvedExpr>,
        window: &ResolvedWindow,
        indices: &[usize],
        orders: &[Vec<Cell>],
        inputs: &[WindowRow],
        column: &mut [Cell],
        peer_start: &[usize],
        peer_end: &[usize],
        group_of: &[i64],
        shape: &ScopeShape,
        outer: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv>,
    ) -> Result<(), SqlError> {
        let len = indices.len();
        for slot in 0..len {
            let ctx = self.window_context(inputs, indices[slot], shape, outer, budget, ctes);
            let (start, end) = self.frame_bounds(window, slot, len, indices, orders, peer_start, peer_end, group_of, &ctx)?;
            let exclude = window.frame.as_ref().map_or(FrameExclude::NoOthers, |frame| frame.exclude);
            // Every position still in the frame after EXCLUDE, in frame order.
            let mut positions = (start.max(0)..=end.min(len as i64 - 1))
                .filter(|position| !frame_row_excluded(exclude, *position as usize, slot, peer_start, peer_end))
                .map(|position| position as usize);
            let wanted = match pick {
                ValuePick::First => positions.next(),
                ValuePick::Last => positions.last(),
                ValuePick::Nth => {
                    let position = position.expect("nth_value carries a position");
                    match nth_position(&position.eval(&inputs[indices[slot]].values, &ctx)?)? {
                        0 => unreachable!("nth_position rejects zero"),
                        nth => positions.nth(nth as usize - 1),
                    }
                }
            };
            let Some(wanted) = wanted
            else {
                column[indices[slot]] = Cell::Null;
                continue;
            };
            let target_ctx = self.window_context(inputs, indices[wanted], shape, outer, budget, ctes);
            column[indices[slot]] = value.eval(&inputs[indices[wanted]].values, &target_ctx)?;
        }
        Ok(())
    }

    /// Aggregate over the frame of each row.
    #[allow(clippy::too_many_arguments)]
    fn evaluate_frame_aggregate(
        &self,
        function: AggregateFunction,
        argument: Option<&ResolvedExpr>,
        separator: Option<&ResolvedExpr>,
        distinct: bool,
        collation: Collation,
        filter: Option<&ResolvedExpr>,
        order: &[(ResolvedExpr, SortDirection)],
        window: &ResolvedWindow,
        indices: &[usize],
        orders: &[Vec<Cell>],
        inputs: &[WindowRow],
        column: &mut [Cell],
        peer_start: &[usize],
        peer_end: &[usize],
        group_of: &[i64],
        shape: &ScopeShape,
        outer: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv>,
    ) -> Result<(), SqlError> {
        let len = indices.len();
        let exclude = window.frame.as_ref().map_or(FrameExclude::NoOthers, |frame| frame.exclude);
        for slot in 0..len {
            let ctx = self.window_context(inputs, indices[slot], shape, outer, budget, ctes);
            let (start, end) = self.frame_bounds(window, slot, len, indices, orders, peer_start, peer_end, group_of, &ctx)?;
            let mut entries: Vec<(Cell, Cell, Vec<Cell>)> = Vec::new();
            for position in start.max(0)..=end.min(len as i64 - 1) {
                if frame_row_excluded(exclude, position as usize, slot, peer_start, peer_end) {
                    continue;
                }
                let row = &inputs[indices[position as usize]];
                let row_ctx = self.window_context(inputs, indices[position as usize], shape, outer, budget, ctes);
                if let Some(filter) = filter {
                    if truthiness(&filter.eval(&row.values, &row_ctx)?) != Some(true) {
                        continue;
                    }
                }
                let value = match argument {
                    Some(argument) => argument.eval(&row.values, &row_ctx)?,
                    None => Cell::Null,
                };
                let separator = match separator {
                    Some(separator) => separator.eval(&row.values, &row_ctx)?,
                    None => Cell::Null,
                };
                let keys = order.iter().map(|(expr, _)| expr.eval(&row.values, &row_ctx)).collect::<Result<Vec<_>, SqlError>>()?;
                entries.push((value, separator, keys));
            }
            let (values, separators) = shape_aggregate_inputs(entries, distinct, order, collation);
            column[indices[slot]] = aggregate_cells(function, &values, if separator.is_some() { Some(&separators) } else { None });
        }
        Ok(())
    }

    /// Start and end positions (inclusive) of a row's frame inside its partition.
    #[allow(clippy::too_many_arguments)]
    fn frame_bounds(
        &self,
        window: &ResolvedWindow,
        slot: usize,
        len: usize,
        indices: &[usize],
        orders: &[Vec<Cell>],
        peer_start: &[usize],
        peer_end: &[usize],
        group_of: &[i64],
        _ctx: &EvalContext,
    ) -> Result<(i64, i64), SqlError> {
        let Some(frame) = &window.frame
        else {
            // No ORDER BY means the whole partition; otherwise the default is
            // `RANGE UNBOUNDED PRECEDING AND CURRENT ROW`, whose CURRENT ROW
            // edge is the last peer, not the current position.
            return Ok(if window.order.is_empty() { (0, len as i64 - 1) } else { (0, peer_end[slot] as i64) });
        };
        match frame.units {
            FrameUnits::Rows => {
                let start = rows_edge(&frame.start, "starting", slot, len)?;
                let end = rows_edge(&frame.end, "ending", slot, len)?;
                Ok((start, end))
            }
            FrameUnits::Groups => Ok((
                groups_edge(&frame.start, "starting", slot, len, peer_start, peer_end, group_of)?,
                groups_edge(&frame.end, "ending", slot, len, peer_start, peer_end, group_of)?,
            )),
            FrameUnits::Range => self.range_bounds(window, frame, slot, len, indices, orders, peer_start, peer_end),
        }
    }

    /// Value-based `RANGE` frame: rows whose ORDER BY value lies in the interval
    /// the edges describe, with `NULL`/text rows only ever matching peers.
    #[allow(clippy::too_many_arguments)]
    fn range_bounds(
        &self,
        window: &ResolvedWindow,
        frame: &ResolvedFrame,
        slot: usize,
        len: usize,
        indices: &[usize],
        orders: &[Vec<Cell>],
        peer_start: &[usize],
        peer_end: &[usize],
    ) -> Result<(i64, i64), SqlError> {
        let descending = window.order.first().is_some_and(|(_, direction)| direction.descending);
        let current = if window.order.len() == 1 { orders[indices[slot]].first().cloned() } else { None };
        // RANGE offsets are numeric only: text and blob ordering keys compare by
        // collation, so SQLite narrows such a frame to the peer group instead of
        // coercing the key with `numeric()`.
        let coerced = match current.as_ref() {
            Some(Cell::Integer(value)) => Some(Number::Int(*value)),
            Some(Cell::Real(value)) => Some(Number::Real(*value)),
            _ => None,
        };
        let offset_start = range_offset(&frame.start, "starting")?;
        let offset_end = range_offset(&frame.end, "ending")?;
        if (offset_start.is_some() || offset_end.is_some()) && window.order.len() != 1 {
            let mut error =
                SqlError::new("invalid-frame", "RANGE with offset PRECEDING/FOLLOWING requires one ORDER BY expression").in_statement("select");
            error.clause = Some("RANGE".to_string());
            return Err(error);
        }
        // A non-numeric current value can only match its own peers; unbounded
        // edges still reach the edges of the partition.
        let Some(current) = coerced
        else {
            let start = match frame.start.edge {
                FrameEdge::UnboundedPreceding => 0,
                FrameEdge::UnboundedFollowing => len as i64,
                _ => peer_start[slot] as i64,
            };
            let end = match frame.end.edge {
                FrameEdge::UnboundedFollowing => len as i64 - 1,
                FrameEdge::UnboundedPreceding => -1,
                _ => peer_end[slot] as i64,
            };
            return Ok((start, end));
        };
        let current = number_f64(current);
        // Each edge names a value on the ORDER BY axis. Descending order flips
        // PRECEDING towards larger values, so the frame is the interval between
        // the two edges, and is empty when they are in the wrong order.
        let start_edge = range_edge(frame.start.edge, offset_start, current, descending);
        let end_edge = range_edge(frame.end.edge, offset_end, current, descending);
        let ordered = if descending { start_edge >= end_edge } else { start_edge <= end_edge };
        if !ordered {
            return Ok((1, 0));
        }
        let low = start_edge.min(end_edge);
        let high = start_edge.max(end_edge);
        // An unbounded edge spans that whole side of the partition, so rows
        // whose ORDER BY key has no numeric form are not dropped from it.
        let start_unbounded = matches!(frame.start.edge, FrameEdge::UnboundedPreceding);
        let end_unbounded = matches!(frame.end.edge, FrameEdge::UnboundedFollowing);
        let mut first = start_unbounded.then_some(0);
        let mut last = end_unbounded.then_some(len - 1);
        for position in 0..len {
            let Some(value) = orders[indices[position]].first().and_then(numeric)
            else {
                continue;
            };
            let value = number_f64(value);
            if value >= low && value <= high {
                first.get_or_insert(position);
                if !end_unbounded {
                    last = Some(position);
                }
            }
        }
        // NULL sorts before every numeric key in ascending order, so the
        // leading run of rows without a numeric key stays inside a frame that
        // begins at the partition start even when its other edge is bounded.
        if start_unbounded && !descending && first.is_some() {
            let nulls = (0..len).take_while(|position| orders[indices[*position]].first().and_then(numeric).is_none()).count();
            if nulls > 0 {
                last = Some(last.map_or(nulls - 1, |last| last.max(nulls - 1)));
            }
        }
        Ok(match (first, last) {
            (Some(first), Some(last)) => (first as i64, last as i64),
            _ => (1, 0),
        })
    }

    /// Evaluates a `SELECT` statement, materializing its `WITH` entries first.
    ///
    /// A CTE that reads its own name is evaluated as a fixed point: the terms
    /// before the first self-reference form the seed, and the remaining terms
    /// run against the previous iteration's rows until the working set is
    /// empty. The `RECURSIVE` keyword is optional, matching SQLite.
    #[allow(clippy::too_many_arguments)]
    fn run_select(
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
    ) -> Result<ExecResult, SqlError> {
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
    fn recursive_start(&self, entry: &CommonTable) -> Result<Option<usize>, SqlError> {
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
    fn materialize_recursive(
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
    fn recursive_limit(
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

    /// Detects a `WHERE <rowid> = <integer literal>` conjunct that bounds a
    /// single base rowid table to one candidate row.
    ///
    /// Only the plain main-schema rowid-table branch of [`Engine::select_source`]
    /// honors the pin; every other source shape (join, derived table, view, temp
    /// table, `WITHOUT ROWID`, attached schema, non-integer literal, or an
    /// equality sitting under `OR` / `NOT`) yields `None` so the scan stays a
    /// full read. The predicate is still evaluated in full downstream, so a pin
    /// never changes the emitted row set or order — it only narrows the
    /// candidate rows the scan decodes.
    fn rowid_pin(&self, from: &[TableRef], filter: &Option<Expr>) -> Option<i64> {
        if from.len() != 1 {
            return None;
        }
        let table = &from[0];
        if table.query.is_some() {
            return None;
        }
        // An unqualified name reaches the bounded branch only when no temp
        // object shadows it; `temp.` never does. A view re-evaluates its query
        // and is not bounded here.
        let key = table.name.to_ascii_lowercase();
        let reaches_main = match table.schema.as_deref() {
            None => !self.catalog.temp_lookup.contains_key(&key) && !self.catalog.temp_views.contains_key(&key),
            Some(qualifier) => {
                let lower = qualifier.to_ascii_lowercase();
                lower == "main" || self.alias.as_deref().is_some_and(|alias| alias.eq_ignore_ascii_case(&lower))
            }
        };
        if !reaches_main || self.catalog.views.contains_key(&key) {
            return None;
        }
        let position = *self.catalog.table_lookup.get(&key)?;
        let def = &self.catalog.tables[position];
        if def.without_rowid {
            return None;
        }
        let alias_name = def.rowid_alias.map(|index| def.columns[index].name.clone());
        Self::rowid_eq_conjunct(filter.as_ref()?, alias_name.as_deref(), table.label(), &self.parameters)
    }

    /// `WHERE <rowid> IN (<integers>)` on one bounded rowid table: the listed
    /// rowids, ascending and de-duplicated.
    ///
    /// Only an all-integer `IN` list on the rowid-alias (or `rowid` /
    /// `_rowid_` / `oid`) name qualifies; anything else keeps the full read.
    /// The predicate still runs downstream, so the candidate set only narrows
    /// what the scan decodes and never changes the emitted rows or their order.
    fn rowid_in_pin(&self, from: &[TableRef], filter: &Option<Expr>) -> Option<Vec<i64>> {
        let (def, table) = self.bounded_scan_target(from)?;
        let alias_name = def.rowid_alias.map(|index| def.columns[index].name.clone());
        Self::rowid_in_conjunct(filter.as_ref()?, alias_name.as_deref(), table.label(), &self.parameters)
    }

    /// `WHERE <rowid> >= <a> AND <rowid> <= <b>`: the rowids inside the stated
    /// interval, ascending. Bounds are widened to inclusive and the interval is
    /// capped, so an unhelpful range keeps the full read instead of building a
    /// huge candidate vector.
    fn rowid_range_pin(&self, from: &[TableRef], filter: &Option<Expr>) -> Option<Vec<i64>> {
        let (def, table) = self.bounded_scan_target(from)?;
        let alias_name = def.rowid_alias.map(|index| def.columns[index].name.clone());
        Self::rowid_range_candidates(filter.as_ref()?, alias_name.as_deref(), table.label(), &self.parameters)
    }

    /// Rowid candidates of a bounded `WHERE <rowid> >= <a> AND <rowid> <= <b>`
    /// conjunction. Only intervals that state both bounds qualify.
    fn rowid_range_candidates(expr: &Expr, alias_name: Option<&str>, label: &str, parameters: &[Cell]) -> Option<Vec<i64>> {
        let mut lower: Option<i64> = None;
        let mut upper: Option<i64> = None;
        Self::collect_rowid_range(expr, alias_name, label, parameters, &mut lower, &mut upper);
        let (start, end) = (lower?, upper?);
        // 宽度用 checked 差值判定:极端边界(i64::MIN..i64::MAX)不展开候选。
        let span = end.checked_sub(start)?;
        if span < 0 || span >= ROWID_RANGE_CANDIDATES {
            return None;
        }
        Some((start..=end).collect())
    }

    /// Folds the tightest rowid bounds of one predicate into `lower` / `upper`.
    /// Exclusive comparisons are widened to inclusive on purpose: the
    /// candidate set is only a superset and the predicate runs again later.
    fn collect_rowid_range(
        expr: &Expr,
        alias_name: Option<&str>,
        label: &str,
        parameters: &[Cell],
        lower: &mut Option<i64>,
        upper: &mut Option<i64>,
    ) {
        match expr {
            Expr::And(left, right) => {
                Self::collect_rowid_range(left, alias_name, label, parameters, lower, upper);
                Self::collect_rowid_range(right, alias_name, label, parameters, lower, upper);
            }
            Expr::Compare { left, op, right } if matches!(op, CompareOp::Gt | CompareOp::GtEq | CompareOp::Lt | CompareOp::LtEq) => {
                let (column, value, flipped) = match (left.as_ref(), right.as_ref()) {
                    (Expr::Column(column), value) => (column, known_cell(value, parameters), false),
                    (value, Expr::Column(column)) => (column, known_cell(value, parameters), true),
                    _ => return,
                };
                let Some(cell) = value
                else {
                    return;
                };
                let value = match cell {
                    Cell::Integer(value) => value,
                    Cell::Real(real) => match integral_rowid(real) {
                        Some(value) => value,
                        None => return,
                    },
                    _ => return,
                };
                if column.qualifier.as_deref().is_some_and(|qualifier| !qualifier.eq_ignore_ascii_case(label)) {
                    return;
                }
                let rowid_named = is_rowid_alias(&column.name);
                let alias_named = alias_name.is_some_and(|name| name.eq_ignore_ascii_case(&column.name));
                if !(rowid_named || alias_named) {
                    return;
                }
                let op = if flipped { flip_range_op(*op) } else { *op };
                match op {
                    CompareOp::Gt | CompareOp::GtEq => *lower = Some(lower.map_or(value, |bound| bound.max(value))),
                    CompareOp::Lt | CompareOp::LtEq => *upper = Some(upper.map_or(value, |bound| bound.min(value))),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Walks the top-level `AND` chain of a predicate for a `rowid = <int>` (or
    /// `<int> = rowid`) comparison on the rowid-alias column or an implicit
    /// `rowid` / `_rowid_` / `oid` name. Anything outside a plain `AND` of
    /// `=` comparisons yields `None`.
    ///
    /// The right-hand side may be an integer literal or a bound parameter whose
    /// value is already known at execution time; a non-integer binding keeps the
    /// full scan, exactly like a non-integer literal.
    fn rowid_eq_conjunct(expr: &Expr, alias_name: Option<&str>, label: &str, parameters: &[Cell]) -> Option<i64> {
        match expr {
            Expr::And(left, right) => Self::rowid_eq_conjunct(left, alias_name, label, parameters)
                .or_else(|| Self::rowid_eq_conjunct(right, alias_name, label, parameters)),
            Expr::Compare { left, op, right } if *op == CompareOp::Eq => {
                let (column, value) = match (left.as_ref(), right.as_ref()) {
                    (Expr::Column(column), value) => (column, known_cell(value, parameters)?),
                    (value, Expr::Column(column)) => (column, known_cell(value, parameters)?),
                    _ => return None,
                };
                // JS 绑定的数字默认是 REAL 存储类，而 rowid 列是 INTEGER 亲和。
                // SQLite 比较前会做亲和转换，`id = 3.0` 与 `id = 3` 命中同一行；
                // 这里把无小数部分的 REAL 折成 INTEGER 才不丢掉这条下推。
                let value = match value {
                    Cell::Integer(value) => value,
                    Cell::Real(real) => integral_rowid(real)?,
                    _ => return None,
                };
                if column.qualifier.as_deref().is_some_and(|qualifier| !qualifier.eq_ignore_ascii_case(label)) {
                    return None;
                }
                let rowid_named = is_rowid_alias(&column.name);
                let alias_named = alias_name.is_some_and(|name| name.eq_ignore_ascii_case(&column.name));
                if rowid_named || alias_named { Some(value) } else { None }
            }
            _ => None,
        }
    }

    /// Walks the top-level `AND` chain for a `rowid IN (<integers>)` list and
    /// returns the listed rowids ascending.
    ///
    /// The list must be a plain (not `NOT`) `IN` over the rowid-alias or
    /// implicit rowid name, with every item a known integer. Bound values are
    /// accepted once they are bound, exactly like the equality pin; anything
    /// else keeps the full scan.
    fn rowid_in_conjunct(expr: &Expr, alias_name: Option<&str>, label: &str, parameters: &[Cell]) -> Option<Vec<i64>> {
        match expr {
            Expr::And(left, right) => Self::rowid_in_conjunct(left, alias_name, label, parameters)
                .or_else(|| Self::rowid_in_conjunct(right, alias_name, label, parameters)),
            Expr::In { value, list, negated: false } => {
                let Expr::Column(column) = value.as_ref()
                else {
                    return None;
                };
                if column.qualifier.as_deref().is_some_and(|qualifier| !qualifier.eq_ignore_ascii_case(label)) {
                    return None;
                }
                let rowid_named = is_rowid_alias(&column.name);
                let alias_named = alias_name.is_some_and(|name| name.eq_ignore_ascii_case(&column.name));
                if !(rowid_named || alias_named) || list.is_empty() {
                    return None;
                }
                let mut rowids = Vec::with_capacity(list.len());
                for item in list {
                    let value = match known_cell(item, parameters)? {
                        Cell::Integer(value) => value,
                        Cell::Real(real) => integral_rowid(real)?,
                        _ => return None,
                    };
                    rowids.push(value);
                }
                rowids.sort_unstable();
                rowids.dedup();
                Some(rowids)
            }
            _ => None,
        }
    }

    /// Single base rowid table of the main schema that a bounded scan may
    /// target: one `FROM` item, no derived table, no temp object or view
    /// shadowing the name, not `WITHOUT ROWID`. This is the shape gate shared
    /// by the point-pin and index-pin paths; anything else keeps the full read.
    fn bounded_scan_target<'a>(&'a self, from: &'a [TableRef]) -> Option<(&'a TableDef, &'a TableRef)> {
        if from.len() != 1 {
            return None;
        }
        let table = &from[0];
        if table.query.is_some() {
            return None;
        }
        let key = table.name.to_ascii_lowercase();
        let reaches_main = match table.schema.as_deref() {
            None => !self.catalog.temp_lookup.contains_key(&key) && !self.catalog.temp_views.contains_key(&key),
            Some(qualifier) => {
                let lower = qualifier.to_ascii_lowercase();
                lower == "main" || self.alias.as_deref().is_some_and(|alias| alias.eq_ignore_ascii_case(&lower))
            }
        };
        if !reaches_main || self.catalog.views.contains_key(&key) {
            return None;
        }
        let position = *self.catalog.table_lookup.get(&key)?;
        let def = &self.catalog.tables[position];
        if def.without_rowid {
            return None;
        }
        Some((def, table))
    }

    /// The pin/head hint for one `FROM` list: a point pin first, then the
    /// candidate rowids of `IN`, a rowid interval or an index probe. Every pin
    /// only narrows what the scan decodes; the predicate still runs in full, so
    /// the emitted rows and their order never change.
    fn scan_hint(&self, from: &[TableRef], filter: &Option<Expr>) -> Option<ScanHint> {
        match self.rowid_pin(from, filter) {
            Some(rowid) => Some(ScanHint::Pin(rowid)),
            None => self
                .rowid_in_pin(from, filter)
                .or_else(|| self.rowid_range_pin(from, filter))
                .map(ScanHint::Rowids)
                .or_else(|| self.rowid_bound_hint(from, filter))
                .or_else(|| self.index_pin(from, filter).map(ScanHint::Rowids)),
        }
    }

    /// 相关子查询里的等值下推:`where <indexed col> = <外层列>`。
    ///
    /// 与外层字面量下推不同,这里的键只在执行期由外层行决定。形状被刻意限窄:
    /// 单张普通 rowid 表、比较的一侧是本表的一列且该列是某个普通索引的首列、
    /// 另一侧直接就是一层外引用。命中时按外层值查一次索引探针,只解码候选行;
    /// 谓词仍整体重跑,所以候选超集不影响结果。
    fn runtime_index_pin(&self, from: &[TableRef], filter: &Option<Expr>, outer: &[Scope]) -> Option<ScanHint> {
        if from.len() != 1 || outer.len() != 1 {
            return None;
        }
        let filter = filter.as_ref()?;
        let def = self.prunable_def(&from[0])?;
        let width = def.columns.len();
        let source = Source {
            label: from[0].label().to_string(),
            columns: SourceColumns::Table(def),
            offset: 0,
            join: None,
            star_hidden: vec![false; width],
            unqualified_hidden: vec![false; width],
            star_override: vec![None; width],
        };
        let relation = Relation { sources: vec![source], using: vec![Vec::new()], using_slots: Vec::new(), trigger: None, excluded: false };
        let shape = relation.shape();
        let outer_shapes: Vec<ScopeShape> = outer.iter().map(Scope::shape).collect();
        let mut windows = Vec::new();
        let mut specs = Vec::new();
        let resolved = resolve_expr(filter, &relation, &outer_shapes, &[], &mut windows, &mut specs, false, false).ok()?;
        if !windows.is_empty() || !specs.is_empty() {
            return None;
        }
        let mut conjuncts = Vec::new();
        split_conjuncts(resolved, &mut conjuncts);
        for conjunct in &conjuncts {
            let ResolvedExpr::Compare { left, op: CompareOp::Eq, right, .. } = conjunct
            else {
                continue;
            };
            let (slot, probe) = match (left.as_ref(), right.as_ref()) {
                (ResolvedExpr::Column { source: 0, index: slot }, probe) => (*slot, probe),
                (probe, ResolvedExpr::Column { source: 0, index: slot }) => (*slot, probe),
                _ => continue,
            };
            let ResolvedExpr::OuterColumn { depth: 0, index } = probe
            else {
                continue;
            };
            let Some(value) = outer[0].columns.get(*index).map(|column| column.value.clone())
            else {
                continue;
            };
            if let Some(rowids) = self.index_probe_by_column(def, slot, &value) {
                return Some(ScanHint::Rowids(rowids));
            }
        }
        None
    }

    /// 相关 `EXISTS` 的索引探针快速路径。
    ///
    /// 返回 `Some(answer)` 表示已用一次索引探针回答存在性,`None` 表示形状
    /// 不适用或探针无法精确回答,调用方继续走通用子查询执行。计划按语句内
    /// AST 地址缓存,同一语句的每个外层行只付一次探针与候选行解码。
    fn correlated_exists(&self, query: &Statement, values: &[Cell], ctx: &EvalContext) -> Result<Option<bool>, SqlError> {
        let key = query as *const Statement as usize;
        if !self.exists_probes.borrow().contains_key(&key) {
            let plan = self.build_exists_probe(query, ctx);
            self.exists_probes.borrow_mut().insert(key, plan);
        }
        let probes = self.exists_probes.borrow();
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
                    engine: self,
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
    fn build_exists_probe(&self, query: &Statement, ctx: &EvalContext) -> Option<ExistsProbePlan> {
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

    /// 等值连接的内层按索引取值:只把匹配外层键的右行交给连接。
    ///
    /// 形状刻意限得很窄:内连 / 左连的 `ON` 恰好是 `<本源裸列> = <更早源的裸列>`,
    /// 两列声明的排序规则相同,且本源码列是某个普通单列索引的首列。命中时用
    /// 已物化的外层列值逐值查索引,把候选 rowid 当作内层的扫描提示;`ON` 仍在
    /// 候选上重跑,所以候选只是超集,行集与左主序都不变。右 / 全连接会用到未
    /// 匹配的右行,因此不做这层过滤。
    fn join_index_pin(
        &self,
        from: &[TableRef],
        index: usize,
        tables: &[Rc<Vec<Vec<Cell>>>],
        masks: Option<&[Option<Vec<bool>>]>,
    ) -> Option<ScanHint> {
        let table = from.get(index)?;
        if !matches!(table.join, Some(JoinKind::Inner) | Some(JoinKind::Left)) {
            return None;
        }
        let Expr::Compare { left, op: CompareOp::Eq, right } = table.on.as_ref()?
        else {
            return None;
        };
        let (Expr::Column(inner), Expr::Column(probe)) = (left.as_ref(), right.as_ref())
        else {
            return None;
        };
        let (inner, probe) = if column_qualified_as(inner, table.label()) {
            (inner, probe)
        }
        else if column_qualified_as(probe, table.label()) {
            (probe, inner)
        }
        else {
            return None;
        };
        let def = self.prunable_def(table)?;
        let position = def.columns.iter().position(|column| column.name.eq_ignore_ascii_case(&inner.name))?;
        // rowid 别名列没有存储值,按它取键会读到 NULL;这类连接保持整表扫描。
        if Some(position) == def.rowid_alias {
            return None;
        }
        let qualifier = probe.qualifier.as_deref()?;
        let owner = (0..index).find(|candidate| from[*candidate].label().eq_ignore_ascii_case(qualifier))?;
        let owner_def = self.prunable_def(&from[owner])?;
        let owner_position = owner_def.columns.iter().position(|column| column.name.eq_ignore_ascii_case(&probe.name))?;
        // 投影掩码可能跳过探针列,行内只剩 NULL 占位,此时不能取键。
        if masks
            .and_then(|masks| masks.get(owner))
            .and_then(|mask| mask.as_deref())
            .is_some_and(|mask| !mask.get(owner_position).copied().unwrap_or(true))
        {
            return None;
        }
        let collation = def.columns[position].collate.unwrap_or_default();
        if owner_def.columns[owner_position].collate.unwrap_or_default() != collation {
            return None;
        }
        let table_key = def.name.to_ascii_lowercase();
        let usable = self.catalog.indexes.values().any(|candidate| {
            !candidate.managed
                && candidate.table == table_key
                && candidate.columns.as_slice() == [position]
                && candidate.collations.first().copied().unwrap_or_default() == collation
        });
        if !usable {
            return None;
        }
        let rows = tables.get(owner)?;
        let mut rowids = std::collections::BTreeSet::new();
        for row in rows.iter() {
            let Some(cell) = row.get(owner_position)
            else {
                return None;
            };
            // `NULL = anything` 永远不成立,不贡献候选,也不该让整条连接退回整表扫描。
            if matches!(cell, Cell::Null) {
                continue;
            }
            rowids.extend(self.index_probe_by_column(def, position, cell)?);
        }
        Some(ScanHint::Rowids(rowids.into_iter().collect()))
    }

    /// 按一列的运行期取值查普通索引,返回候选 rowid。
    fn index_probe_by_column(&self, def: &TableDef, position: usize, cell: &Cell) -> Option<Vec<i64>> {
        let table_key = def.name.to_ascii_lowercase();
        for index in self.catalog.indexes.values() {
            if index.managed || index.table != table_key || index.columns.as_slice() != [position] {
                continue;
            }
            let affinity = affinity_of(&def.columns[position].declared_type);
            let cell = match (affinity, cell) {
                (Affinity::Integer, Cell::Real(real)) => Cell::Integer(integral_rowid(*real)?),
                _ => cell.clone(),
            };
            match (&affinity, &cell) {
                (Affinity::Text, Cell::Text(_)) | (Affinity::Real, Cell::Real(_)) | (Affinity::Integer, Cell::Integer(_)) => {}
                _ => continue,
            }
            let collation = index.collations.first().copied().unwrap_or_default();
            let key = probe_key(&cell, collation)?;
            if matches!(key, ProbeKey::Wide(_)) {
                return None;
            }
            return self.index_probe_rowids(index, &[key]);
        }
        None
    }

    /// `<rowid> >= a` / `<rowid> <= b` 的单边或双边区间提示。
    /// 与 [`Engine::rowid_range_pin`] 不同,这里不要求两侧都有界,也不展开候选
    /// rowid:读取侧直接用同一个区间走 b-tree 范围读,缺失的一侧不设界。谓词仍
    /// 在解码后的行上重跑,所以区间只是候选超集。
    fn rowid_bound_hint(&self, from: &[TableRef], filter: &Option<Expr>) -> Option<ScanHint> {
        let (def, table) = self.bounded_scan_target(from)?;
        let alias_name = def.rowid_alias.map(|index| def.columns[index].name.clone());
        let filter = filter.as_ref()?;
        let mut lower = None;
        let mut upper = None;
        Self::collect_rowid_range(filter, alias_name.as_deref(), table.label(), &self.parameters, &mut lower, &mut upper);
        if lower.is_none() && upper.is_none() {
            return None;
        }
        Some(ScanHint::Range { lower, upper })
    }

    /// `WHERE` conjuncts that read only one source of a multi-source `FROM`,
    /// re-combined into that source's own filter.
    ///
    /// A conjunct is kept only when every column it reads is qualified with the
    /// source's own label, or is an unqualified declared column no other source
    /// can shadow. The whole predicate still runs after the join, so narrowing
    /// the source read with this filter cannot change the result set.
    fn source_filter(&self, index: usize, from: &[TableRef], filter: &Option<Expr>) -> Option<Expr> {
        let filter = filter.as_ref()?;
        let table = &from[index];
        let def = self.prunable_def(table)?;
        let label = table.label();
        let mut conjuncts = Vec::new();
        self.collect_source_conjuncts(filter, def, label, from, index, &mut conjuncts);
        let mut combined = conjuncts.into_iter();
        let mut filter = combined.next()?;
        for conjunct in combined {
            filter = Expr::And(Box::new(filter), Box::new(conjunct));
        }
        Some(filter)
    }

    /// Splits a raw `AND` chain into the conjuncts that belong to one source.
    fn collect_source_conjuncts(&self, expr: &Expr, def: &TableDef, label: &str, from: &[TableRef], index: usize, out: &mut Vec<Expr>) {
        match expr {
            Expr::And(left, right) => {
                self.collect_source_conjuncts(left, def, label, from, index, out);
                self.collect_source_conjuncts(right, def, label, from, index, out);
            }
            single => {
                if self.conjunct_is_source_local(single, def, label, from, index) {
                    out.push(single.clone());
                }
            }
        }
    }

    /// Whether every column a conjunct reads provably resolves to one source.
    fn conjunct_is_source_local(&self, expr: &Expr, def: &TableDef, label: &str, from: &[TableRef], index: usize) -> bool {
        let mut refs = Vec::new();
        if !collect_unmasked_columns(expr, &mut refs) {
            return false;
        }
        for column in refs {
            if let Some(qualifier) = &column.qualifier {
                if !qualifier.eq_ignore_ascii_case(label) {
                    return false;
                }
                continue;
            }
            // 裸列名可能被任意一个来源解析,只有其它来源都不提供同名列时
            // 才能确定归属;隐式 rowid 名字在多表下同样有歧义。
            if is_rowid_alias(&column.name) {
                return false;
            }
            if !def.columns.iter().any(|entry| entry.name.eq_ignore_ascii_case(&column.name)) {
                return false;
            }
            for (other, table) in from.iter().enumerate() {
                if other == index {
                    continue;
                }
                match self.prunable_def(table) {
                    Some(other_def) => {
                        if other_def.columns.iter().any(|entry| entry.name.eq_ignore_ascii_case(&column.name)) {
                            return false;
                        }
                    }
                    None => return false,
                }
            }
        }
        true
    }

    /// Detects a `WHERE` conjunction that pins every key column of one plain
    /// column index to a literal with `=`, and returns the matching rowids in
    /// ascending order.
    ///
    /// The gate is deliberately narrow so the probe can never miss a row the
    /// full predicate would keep. Only non-managed indexes over declared
    /// columns qualify, each key column's literal must match the column's
    /// affinity class exactly (a `TEXT` column with a text literal, an
    /// `INTEGER` column with an integer literal, a `REAL` column with a real
    /// literal), and a stored key the canonical signature cannot reproduce
    /// poisons the whole probe. The full predicate still runs downstream, so
    /// over-fetching is harmless. Within one full-key equality the index
    /// entries are ordered by rowid, which is also the order of the full
    /// table scan, so the emitted rows keep their position.
    fn index_pin(&self, from: &[TableRef], filter: &Option<Expr>) -> Option<Vec<i64>> {
        let (def, table) = self.bounded_scan_target(from)?;
        let filter = filter.as_ref()?;
        let table_key = def.name.to_ascii_lowercase();
        for index in self.catalog.indexes.values() {
            if index.managed || index.table != table_key || index.columns.is_empty() {
                continue;
            }
            if let Some(rowids) = self.index_probe_rows(index, def, filter, table.label()) {
                return Some(rowids);
            }
            if let Some(rowids) = self.index_range_rows(index, def, filter, table.label()) {
                return Some(rowids);
            }
        }
        None
    }

    /// Builds the probe key from the equality literals and looks it up in the
    /// index's cached key set. `None` means the shape is not pushdown-safe and
    /// the caller must fall back to the full scan.
    fn index_probe_rows(&self, index: &IndexDef, def: &TableDef, filter: &Expr, label: &str) -> Option<Vec<i64>> {
        let mut equalities: Vec<(usize, Cell)> = Vec::new();
        collect_index_equalities(filter, def, label, &self.parameters, &mut equalities);
        if equalities.is_empty() {
            return None;
        }
        let mut key = Vec::with_capacity(index.columns.len());
        for (slot, position) in index.columns.iter().enumerate() {
            let (_, cell) = equalities.iter().find(|(column, _)| column == position)?;
            let affinity = affinity_of(&def.columns[*position].declared_type);
            // INTEGER 亲和列上的绑定数字是 REAL 存储类；无小数部分时折成
            // INTEGER，索引键的规范签名才会与存储键一致。
            let cell = match (affinity, cell) {
                (Affinity::Integer, Cell::Real(real)) => Cell::Integer(integral_rowid(*real)?),
                _ => cell.clone(),
            };
            match (affinity, &cell) {
                (Affinity::Text, Cell::Text(_)) | (Affinity::Real, Cell::Real(_)) | (Affinity::Integer, Cell::Integer(_)) => {}
                _ => return None,
            }
            let collation = index.collations.get(slot).copied().unwrap_or_default();
            key.push(probe_key(&cell, collation)?);
        }
        if key.iter().any(|part| matches!(part, ProbeKey::Wide(_))) {
            return None;
        }
        self.index_probe_rowids(index, &key)
    }

    /// Returns the rowids stored under one probe key, rebuilding the cached
    /// key set first when the write epoch moved on.
    fn index_probe_rowids(&self, index: &IndexDef, key: &[ProbeKey]) -> Option<Vec<i64>> {
        let version = self.index_version(&index.name);
        let name = index.name.to_ascii_lowercase();
        {
            let cache = self.index_probes.borrow();
            if let Some(probe) = cache.get(&name) {
                if probe.epoch == version {
                    return if probe.poisoned { None } else { probe.map.get(key).cloned() };
                }
            }
        }
        let probe = self.build_index_probe(index, version);
        let rowids = if probe.poisoned { None } else { probe.map.get(key).cloned() };
        self.index_probes.borrow_mut().insert(name, probe);
        rowids
    }

    /// Detects a range predicate on the index's leading column and returns the
    /// matching rowids in ascending order.
    ///
    /// The gate is as narrow as the equality probe: only non-managed indexes
    /// over declared columns qualify, every bound literal must match the
    /// column's affinity class, and a poisoned key set keeps the full scan. The
    /// full predicate still runs downstream, so a wider candidate set is safe,
    /// and ascending rowids preserve the full-scan order.
    fn index_range_rows(&self, index: &IndexDef, def: &TableDef, filter: &Expr, label: &str) -> Option<Vec<i64>> {
        let position = *index.columns.first()?;
        let mut lower: Option<IndexBound> = None;
        let mut upper: Option<IndexBound> = None;
        collect_range_bounds(filter, def, label, position, &self.parameters, &mut lower, &mut upper);
        if lower.is_none() && upper.is_none() {
            return None;
        }
        let affinity = affinity_of(&def.columns[position].declared_type);
        for bound in [lower.as_mut(), upper.as_mut()].into_iter().flatten() {
            // 绑定数字是 REAL 存储类，INTEGER 亲和列的等值 / 范围比较会先做亲和
            // 转换；无小数部分的 REAL 折成 INTEGER 才能继续用索引探针。
            if let (Affinity::Integer, Cell::Real(real)) = (&affinity, &bound.cell) {
                match integral_rowid(*real) {
                    Some(value) => bound.cell = Cell::Integer(value),
                    None => return None,
                }
            }
            match (&affinity, &bound.cell) {
                (Affinity::Text, Cell::Text(_)) | (Affinity::Real, Cell::Real(_)) | (Affinity::Integer, Cell::Integer(_)) => {}
                _ => return None,
            }
        }
        let collation = index.collations.first().copied().unwrap_or_default();
        self.index_range_rowids(index, &lower, &upper, collation)
    }

    /// Filters the cached index entries against the range bounds, rebuilding
    /// the cache first when the write epoch moved on.
    fn index_range_rowids(
        &self,
        index: &IndexDef,
        lower: &Option<IndexBound>,
        upper: &Option<IndexBound>,
        collation: Collation,
    ) -> Option<Vec<i64>> {
        let version = self.index_version(&index.name);
        let name = index.name.to_ascii_lowercase();
        let select = |probe: &IndexProbe| -> Option<Vec<i64>> {
            if probe.poisoned {
                return None;
            }
            // 索引项按首列有序时,用两次二分取出候选区间,只遍历命中项。
            if probe.sorted {
                let collation = collation;
                let lower_start = match lower {
                    None => 0,
                    Some(bound) => probe.entries.partition_point(|(key, _)| match key.first() {
                        Some(cell) => {
                            let ordering = compare_cells_with(cell, &bound.cell, collation);
                            ordering == Ordering::Less || (!bound.inclusive && ordering == Ordering::Equal)
                        }
                        None => true,
                    }),
                };
                let upper_end = match upper {
                    None => probe.entries.len(),
                    Some(bound) => probe.entries.partition_point(|(key, _)| match key.first() {
                        Some(cell) => {
                            let ordering = compare_cells_with(cell, &bound.cell, collation);
                            ordering == Ordering::Less || (bound.inclusive && ordering == Ordering::Equal)
                        }
                        None => false,
                    }),
                };
                if lower_start >= upper_end {
                    return Some(Vec::new());
                }
                let mut rowids: Vec<i64> = probe.entries[lower_start..upper_end]
                    .iter()
                    .map(|(_, rowid)| *rowid)
                    .filter(|rowid| !probe.deleted.contains(rowid))
                    .collect();
                rowids.sort_unstable();
                // 重新插入同一个 rowid 会留下一条旧键墓碑条目,候选按 rowid
                // 去重后与整表扫描的行集一致。
                rowids.dedup();
                return Some(rowids);
            }
            let mut rowids = Vec::new();
            for (key, rowid) in &probe.entries {
                if probe.deleted.contains(rowid) {
                    continue;
                }
                let Some(cell) = key.first()
                else {
                    continue;
                };
                if let Some(bound) = lower {
                    let ordering = compare_cells_with(cell, &bound.cell, collation);
                    if ordering == Ordering::Less || (!bound.inclusive && ordering == Ordering::Equal) {
                        continue;
                    }
                }
                if let Some(bound) = upper {
                    let ordering = compare_cells_with(cell, &bound.cell, collation);
                    if ordering == Ordering::Greater || (!bound.inclusive && ordering == Ordering::Equal) {
                        continue;
                    }
                }
                rowids.push(*rowid);
            }
            rowids.sort_unstable();
            rowids.dedup();
            Some(rowids)
        };
        {
            let cache = self.index_probes.borrow();
            if let Some(probe) = cache.get(&name) {
                if probe.epoch == version {
                    return select(probe);
                }
            }
        }
        let probe = self.build_index_probe(index, version);
        let rowids = select(&probe);
        self.index_probes.borrow_mut().insert(name, probe);
        rowids
    }

    /// Decodes one index's live key set into canonical signatures. A stored
    /// key outside the canonical form (a NaN or a wide integer) poisons the
    /// probe so every later query keeps the full scan.
    fn build_index_probe(&self, index: &IndexDef, epoch: u64) -> IndexProbe {
        let mut probe = IndexProbe { epoch, map: HashMap::new(), entries: Vec::new(), sorted: false, deleted: HashSet::new(), poisoned: false };
        let entries = match self.writer.index_entries_decoded(&index.name) {
            Ok(entries) => entries,
            Err(_) => {
                probe.poisoned = true;
                return probe;
            }
        };
        probe.entries.reserve(entries.len());
        for (values, rowid) in entries {
            if values.len() != index.columns.len() {
                probe.poisoned = true;
                break;
            }
            let cells: Vec<Cell> = values.iter().map(write_value_cell).collect();
            let mut key = Vec::with_capacity(values.len());
            let mut canonical = true;
            for (cell, collation) in cells.iter().zip(&index.collations) {
                match probe_key(cell, *collation) {
                    Some(part) if !matches!(part, ProbeKey::Wide(_)) => key.push(part),
                    _ => {
                        canonical = false;
                        break;
                    }
                }
            }
            if !canonical {
                probe.poisoned = true;
                break;
            }
            probe.map.entry(key).or_default().push(rowid);
            probe.entries.push((cells, rowid));
        }
        if !probe.poisoned {
            for rowids in probe.map.values_mut() {
                rowids.sort_unstable();
            }
            // 索引项通常按首列有序存储,确认一次后范围探针即可二分。
            let collation = index.collations.first().copied().unwrap_or_default();
            probe.sorted = probe.entries.windows(2).all(|pair| match (pair[0].0.first(), pair[1].0.first()) {
                (Some(left), Some(right)) => compare_cells_with(left, right, collation) != Ordering::Greater,
                _ => false,
            });
        }
        probe
    }

    /// 一条索引的内容版本:从未登记过的索引视为 `0`。
    fn index_version(&self, name: &str) -> u64 {
        self.index_versions.borrow().get(&name.to_ascii_lowercase()).copied().unwrap_or(0)
    }

    /// 在空索引上登记一份空探针,让后续写语句从第一行起就增量维护键集。
    ///
    /// 只在 `CREATE INDEX` 建在空表上时调用:此时键集为空,登记成本为常数,
    /// 而首次查询 / 区间删除不必再整份解码索引项。
    fn install_empty_index_probe(&self, name: &str) {
        let epoch = self.index_version(name);
        let probe = IndexProbe { epoch, map: HashMap::new(), entries: Vec::new(), sorted: true, deleted: HashSet::new(), poisoned: false };
        self.index_probes.borrow_mut().insert(name.to_ascii_lowercase(), probe);
    }

    /// 推进时钟并返回新 epoch。
    fn next_epoch(&self) -> u64 {
        let next = self.write_epoch.get().wrapping_add(1);
        self.write_epoch.set(next);
        next
    }

    /// 取出一条写语句的目标表名(小写)。
    ///
    /// 只有裸表名或 `main` / 本库别名限定的 DML 才能精确到单表;其余形态
    /// (其它 attached schema、DDL 等)返回 `None`,调用方按全量失效处理。
    fn statement_table_key(&self, statement: &Statement) -> Option<String> {
        let (schema, table) = match statement {
            Statement::Insert { schema, table, .. } | Statement::Update { schema, table, .. } | Statement::Delete { schema, table, .. } => {
                (schema.as_deref(), table)
            }
            _ => return None,
        };
        match schema {
            None => Some(table.to_ascii_lowercase()),
            Some(qualifier) => {
                let lower = qualifier.to_ascii_lowercase();
                if lower == "main" || self.alias.as_deref().is_some_and(|alias| alias.eq_ignore_ascii_case(&lower)) {
                    Some(table.to_ascii_lowercase())
                }
                else {
                    None
                }
            }
        }
    }

    /// 让一张表上的全部索引缓存失效。
    fn invalidate_table_indexes(&self, table_key: &str) {
        let epoch = self.next_epoch();
        let mut versions = self.index_versions.borrow_mut();
        for index in self.catalog.indexes.values() {
            if index.table == table_key {
                versions.insert(index.name.to_ascii_lowercase(), epoch);
            }
        }
    }

    /// 让全部索引缓存失效。
    fn invalidate_all_indexes(&self) {
        let epoch = self.next_epoch();
        let mut versions = self.index_versions.borrow_mut();
        for index in self.catalog.indexes.values() {
            versions.insert(index.name.to_ascii_lowercase(), epoch);
        }
    }

    /// `INSERT` 执行后把新行增量补进既有索引探针。
    ///
    /// 与删除侧的 [`Engine::patch_index_probes_after_delete`] 对称:只要语句只
    /// 追加行、不改写既有行,缓存内容加上这批新行就等于当前索引,不必让下一次
    /// 查询整份重建。重新插入的 rowid 会先清掉墓碑,否则探针会把它当作已删除。
    ///
    /// 追加后只要首列仍非递减,`entries` 就继续按首列有序,范围探针仍可二分;
    /// 键序被打乱时才退回线性扫描,由下一次会改写行的语句按常规失效并重建。
    fn patch_index_probes_after_insert(&self, table_key: &str, rows: &[(i64, Vec<Cell>)]) {
        if rows.is_empty() {
            return;
        }
        let epoch = self.next_epoch();
        let mut patched: Vec<String> = Vec::new();
        {
            let mut probes = self.index_probes.borrow_mut();
            for index in self.catalog.indexes.values() {
                if index.table != table_key || index.managed || index.columns.is_empty() {
                    continue;
                }
                let name = index.name.to_ascii_lowercase();
                let version = self.index_version(&index.name);
                let Some(probe) = probes.get_mut(&name)
                else {
                    continue;
                };
                // 只有与当前索引内容同步的探针才能追加:被其它语句改写过的
                // 旧快照必须留给下一条查询整份重建。
                if probe.poisoned || probe.epoch != version {
                    continue;
                }
                let mut canonical = true;
                for (rowid, cells) in rows {
                    let mut key = Vec::with_capacity(index.columns.len());
                    let mut key_cells = Vec::with_capacity(index.columns.len());
                    for (slot, position) in index.columns.iter().enumerate() {
                        let collation = index.collations.get(slot).copied().unwrap_or_default();
                        let Some(cell) = cells.get(*position)
                        else {
                            canonical = false;
                            break;
                        };
                        match probe_key(cell, collation) {
                            Some(part) if !matches!(part, ProbeKey::Wide(_)) => key.push(part),
                            _ => {
                                canonical = false;
                                break;
                            }
                        }
                        key_cells.push(cell.clone());
                    }
                    if !canonical {
                        break;
                    }
                    probe.deleted.remove(rowid);
                    let rowids = probe.map.entry(key).or_default();
                    let at = rowids.partition_point(|existing| existing < rowid);
                    if rowids.get(at) != Some(rowid) {
                        rowids.insert(at, *rowid);
                    }
                    // 只有新键落在首列序的前面时才破坏有序性;按递增键插入的
                    // 常见形态保持 `sorted`,范围探针继续走二分。
                    if probe.sorted {
                        let collation = index.collations.first().copied().unwrap_or_default();
                        if let (Some(last), Some(next)) = (probe.entries.last().and_then(|(cells, _)| cells.first()), key_cells.first())
                            && compare_cells_with(last, next, collation) == Ordering::Greater
                        {
                            probe.sorted = false;
                        }
                    }
                    probe.entries.push((key_cells, *rowid));
                }
                if !canonical {
                    probe.poisoned = true;
                    continue;
                }
                patched.push(name);
            }
        }
        if patched.is_empty() {
            return;
        }
        {
            let mut versions = self.index_versions.borrow_mut();
            for name in &patched {
                versions.insert(name.clone(), epoch);
            }
        }
        // 补丁后的探针内容仍准确,把版本对齐到新 epoch 继续复用。
        let mut probes = self.index_probes.borrow_mut();
        for name in &patched {
            if let Some(probe) = probes.get_mut(name) {
                probe.epoch = epoch;
            }
        }
    }

    /// 表上是否存在可增量维护的索引探针缓存。
    fn table_has_probes(&self, table_key: &str) -> bool {
        let probes = self.index_probes.borrow();
        self.catalog.indexes.values().any(|index| {
            index.table == table_key && !index.managed && !index.columns.is_empty() && probes.contains_key(&index.name.to_ascii_lowercase())
        })
    }

    /// 普通 `INSERT` 是否只往表尾追加行、因而保持既有索引内容不变。
    ///
    /// 条件与插入路径里 `watch_probes` 的判定一致:主库的普通 rowid 表、无
    /// 触发器、已有可增量维护的探针,且语句没有 `ON CONFLICT` / `REPLACE`。
    /// 成立时语句执行前不必让索引缓存整体失效,提交后的增量补丁即可对齐。
    fn insert_keeps_index_content(&self, table: &str) -> bool {
        let key = table.to_ascii_lowercase();
        if self.catalog.temp_lookup.contains_key(&key) || self.table_has_triggers(&key) {
            return false;
        }
        let Some(position) = self.catalog.table_lookup.get(&key).copied()
        else {
            return false;
        };
        if self.catalog.tables[position].without_rowid {
            return false;
        }
        self.table_has_probes(&key)
    }

    /// `UPDATE` 执行后只让真正被改动的索引失效。
    ///
    /// 索引项只由被索引列的值和行 rowid 决定:赋值没有碰到任何索引列、也没改
    /// rowid 时,索引内容与语句执行前逐项一致,缓存可以继续复用。托管索引
    /// (表达式、`COLLATE`、部分索引)的键可能依赖任意列,一律按失效处理。
    fn invalidate_indexes_after_update(&self, def: &TableDef, table_key: &str, assigned: &[bool]) {
        let rowid_moved = def.rowid_alias.is_some_and(|position| assigned.get(position).copied().unwrap_or(false));
        let mut dirty: Vec<String> = Vec::new();
        for index in self.catalog.indexes.values() {
            if index.table != table_key {
                continue;
            }
            let affected = rowid_moved
                || index.managed
                || index.columns.is_empty()
                || index.columns.iter().any(|position| assigned.get(*position).copied().unwrap_or(true));
            if affected {
                dirty.push(index.name.to_ascii_lowercase());
            }
        }
        if dirty.is_empty() {
            return;
        }
        let epoch = self.next_epoch();
        let mut versions = self.index_versions.borrow_mut();
        for name in dirty {
            versions.insert(name, epoch);
        }
    }

    /// `DELETE` 执行后把被删行从已缓存的索引探针里摘掉。
    ///
    /// 缓存是整份索引的快照,按 rowid 精确剔除与整树重建等价,因此不必让
    /// 每次删除都重付一遍全索引解码。托管索引与已过期的缓存退化为常规失效,
    /// 由下一次探针重建。
    fn patch_index_probes_after_delete(&self, table_key: &str, plan: &DeletePlan) {
        let removed: HashSet<i64> = plan.removed.iter().copied().collect();
        let mut stale: Vec<String> = Vec::new();
        let mut patched: Vec<String> = Vec::new();
        {
            let mut probes = self.index_probes.borrow_mut();
            for index in self.catalog.indexes.values() {
                if index.table != table_key {
                    continue;
                }
                let name = index.name.to_ascii_lowercase();
                let version = self.index_version(&index.name);
                let Some(probe) = probes.get_mut(&name)
                else {
                    continue;
                };
                if index.managed || index.columns.is_empty() || probe.poisoned || probe.epoch != version {
                    stale.push(name);
                    continue;
                }
                for (rowid, cells) in &plan.live {
                    if !removed.contains(rowid) {
                        continue;
                    }
                    let mut key = Vec::with_capacity(index.columns.len());
                    let mut canonical = true;
                    for (slot, position) in index.columns.iter().enumerate() {
                        let collation = index.collations.get(slot).copied().unwrap_or_default();
                        match cells.get(*position).and_then(|cell| probe_key(cell, collation)) {
                            Some(part) if !matches!(part, ProbeKey::Wide(_)) => key.push(part),
                            _ => {
                                canonical = false;
                                break;
                            }
                        }
                    }
                    if !canonical {
                        probe.poisoned = true;
                        break;
                    }
                    let emptied = match probe.map.get_mut(&key) {
                        Some(rowids) => {
                            rowids.retain(|id| id != rowid);
                            rowids.is_empty()
                        }
                        None => false,
                    };
                    if emptied {
                        probe.map.remove(&key);
                    }
                }
                if probe.poisoned {
                    stale.push(name);
                    continue;
                }
                if !removed.is_empty() {
                    // 只记墓碑;条目累积到一半时再一次性压缩,单条删除不再
                    // 扫整份索引项。
                    probe.deleted.extend(removed.iter().copied());
                    if probe.deleted.len() * 2 > probe.entries.len() {
                        let deleted = std::mem::take(&mut probe.deleted);
                        probe.entries.retain(|(_, rowid)| !deleted.contains(rowid));
                    }
                }
                patched.push(name);
            }
        }
        if stale.is_empty() && patched.is_empty() {
            return;
        }
        let epoch = self.next_epoch();
        {
            let mut versions = self.index_versions.borrow_mut();
            for name in stale.iter().chain(patched.iter()) {
                versions.insert(name.clone(), epoch);
            }
        }
        // 打过补丁的探针内容仍然准确,只把版本对齐到新 epoch 继续复用。
        if !patched.is_empty() {
            let mut probes = self.index_probes.borrow_mut();
            for name in &patched {
                if let Some(probe) = probes.get_mut(name) {
                    probe.epoch = epoch;
                }
            }
        }
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
    fn rowid_head_bound(&self, first: &SelectCore, order: &[OrderKey], limit: Option<(usize, Option<usize>)>) -> Option<usize> {
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
    /// - 投影是 `*` 或普通列引用/常量表达式,`WHERE` 可逐行求值。
    ///
    /// 打开时把当前整页镜像克隆成独立快照,后续写入只改引擎自己的页缓冲,
    /// 游标因此始终读到打开时刻的一致状态。返回 `None` 表示语句不满足条件,
    /// 调用方走既有的物化路径。
    pub(crate) fn open_lazy_cursor(&mut self, statement: &Statement, parameters: &[Cell]) -> Result<Option<LazyCursor>, SqlError> {
        let Some(plan) = self.plan_lazy_scan(statement)?
        else {
            return Ok(None);
        };
        // 游标快照直接克隆镜像页,推迟的索引重建必须先落下。
        self.flush_pending_materialization("cursor")?;
        let objects = match self.writer.schema_objects_with_rowids(TableLimits::default()) {
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
    pub(crate) fn lazy_cursor_next(&mut self, cursor: &mut LazyCursor, limit: usize) -> Result<LazyBatch, SqlError> {
        if cursor.finished || limit == 0 {
            return Ok(LazyBatch { rows: Vec::new(), done: cursor.finished });
        }
        let saved = std::mem::take(&mut self.parameters);
        self.parameters = cursor.parameters.clone();
        let result = self.lazy_cursor_next_inner(cursor, limit);
        self.parameters = saved;
        result
    }

    fn lazy_cursor_next_inner(&self, cursor: &mut LazyCursor, limit: usize) -> Result<LazyBatch, SqlError> {
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
                    engine: self,
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
    fn plan_lazy_scan(&self, statement: &Statement) -> Result<Option<LazyPlan>, SqlError> {
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
    fn read_column_masks(&self, first: &SelectCore, order: &[OrderKey]) -> Option<Vec<Option<Vec<bool>>>> {
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
    fn prunable_def(&self, entry: &TableRef) -> Option<&TableDef> {
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
    fn rows_for_rowids(&self, table: &str, rowids: &[i64]) -> Result<Vec<TableRow>, SqlError> {
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
                .table_range(table, rowids[0], rowids[rowids.len() - 1], TableLimits::default())
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
    fn select_source<'b>(
        &'b self,
        table: &TableRef,
        budget: &Budget,
        ctes: Option<&CteEnv>,
        scan: Option<&ScanHint>,
        keep: Option<&[bool]>,
    ) -> Result<(SourceColumns<'b>, Rc<Vec<Vec<Cell>>>), SqlError> {
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
                    .without_rowid_rows(&def.name, TableLimits::default())
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
                    let scans = self.statement_scans.borrow();
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
                    self.writer.table_rows(&def.name, TableLimits::default()).map_err(|error| storage_failure(error).in_statement("select"))?;
                let decoded = Rc::new(decode_stored_rows(&stored, def, keep)?);
                let wider = {
                    let scans = self.statement_scans.borrow();
                    match scans.get(&key) {
                        None => true,
                        Some((cached_mask, _)) => cached_mask.as_deref() != keep && mask_covers(keep, cached_mask.as_deref()),
                    }
                };
                if wider {
                    self.statement_scans.borrow_mut().insert(key, (keep.map(<[bool]>::to_vec), Rc::downgrade(&decoded)));
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
                    .table_head(&def.name, *max_rows, TableLimits::default())
                    .map_err(|error| storage_failure(error).in_statement("select"))?,
                // `WHERE <rowid> >= a` / `<= b`: read the interval that the
                // predicate can still keep; the missing side stays unbounded.
                Some(ScanHint::Range { lower, upper }) => self
                    .writer
                    .table_range(&def.name, lower.unwrap_or(i64::MIN), upper.unwrap_or(i64::MAX), TableLimits::default())
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
                if let Ok(source) = self.attached_source(index, &self.attached[index].name, table, budget) {
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
    fn attached_source<'b>(
        &'b self,
        index: usize,
        qualifier: &str,
        table: &TableRef,
        budget: &Budget,
    ) -> Result<(SourceColumns<'b>, Rc<Vec<Vec<Cell>>>), SqlError> {
        let engine = &self.attached[index].engine;
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
                engine.writer.table_rows(&def.name, TableLimits::default()).map_err(|error| storage_failure(error).in_statement("select"))?;
            return Ok((SourceColumns::Table(def), Rc::new(decode_stored_rows(&stored, def, None)?)));
        }
        Err(SqlError::new("unknown-table", format!("no such table: {qualifier}.{}", table.name)).in_statement("select"))
    }

    fn select_statement(&self, statement: &Statement, outer: &[Scope], budget: &Budget, ctes: Option<&CteEnv>) -> Result<ExecResult, SqlError> {
        match statement {
            Statement::Select { with, recursive, first, compounds, order, limit } => {
                self.run_select(with, *recursive, first, compounds, order.clone(), limit.clone(), outer, budget, ctes)
            }
            other => Err(SqlError::new("unsupported-statement", format!("a subquery must be a `SELECT` but found `{}`", other.name()))
                .in_statement("select")),
        }
    }

    /// 预编译期语句元数据:从目录推导结果列标签,绝不执行语句。
    ///
    /// `Ok(Some(标签))` 表示目录足以确定行集表头,标签与成功执行后的
    /// `ExecResult.columns` 逐字节一致(两边共用 [`core_output_labels`] 与
    /// 同一套 `Relation` 标签方法)。`Ok(None)` 表示语句没有行集结果:
    /// 无 `RETURNING` 的 DML、DDL、事务控制、`ATTACH`/`DETACH` 与写形
    /// `PRAGMA`。`Err` 表示目录解析失败(缺表、缺列、歧义列、视图递归超限
    /// 等),与执行期同一阶段的名字检查一起失败;连接层把它折叠成 `None`,
    /// 错误留待真正执行时按既有语义抛出,prepare 不提前拒绝。
    /// schema 限定名(`main.`、`temp.`、附加库、连接别名)按执行路径
    /// [`Engine::run_statement`] 的同一顺序分派,但只走目录,不写镜像。
    pub fn describe(&self, statement: &Statement) -> Result<Option<Vec<String>>, SqlError> {
        let mut statement = statement.clone();
        let scope = match leading_schema(&statement).map(str::to_owned) {
            None => {
                if let Some(index) = self.attached_target(&statement) {
                    return self.attached[index].engine.describe(&statement);
                }
                TableScope::Default
            }
            Some(schema) => {
                let lower = schema.to_ascii_lowercase();
                if self.alias.as_deref().is_some_and(|alias| alias.eq_ignore_ascii_case(&schema)) {
                    clear_statement_schema(&mut statement, &schema);
                    TableScope::Default
                }
                else if lower == "main" {
                    clear_statement_schema(&mut statement, &schema);
                    TableScope::Main
                }
                else if lower == "temp" {
                    clear_statement_schema(&mut statement, &schema);
                    TableScope::Temp
                }
                else if let Some(index) = self.attached_lookup.get(&lower).copied() {
                    clear_statement_schema(&mut statement, &schema);
                    strip_statement_sql(&mut statement, &schema);
                    return self.attached[index].engine.describe(&statement);
                }
                else {
                    return Err(unknown_schema_error(&statement, &schema));
                }
            }
        };
        self.describe_in(&statement, scope)
    }

    /// 在已解析出的 schema 作用域内推导一条语句的结果列标签。
    fn describe_in(&self, statement: &Statement, scope: TableScope) -> Result<Option<Vec<String>>, SqlError> {
        match statement {
            Statement::Select { .. } => Ok(Some(self.describe_select_statement(statement, None)?)),
            Statement::Explain { query_plan, statement } => {
                self.explain_check_tables(statement, &BTreeSet::new())?;
                Ok(Some(if *query_plan {
                    vec!["id".to_string(), "parent".to_string(), "notused".to_string(), "detail".to_string()]
                }
                else {
                    vec![
                        "addr".to_string(),
                        "opcode".to_string(),
                        "p1".to_string(),
                        "p2".to_string(),
                        "p3".to_string(),
                        "p4".to_string(),
                        "p5".to_string(),
                        "comment".to_string(),
                    ]
                }))
            }
            Statement::Pragma { schema, name, value } => self.describe_pragma(schema.as_deref(), name, value.as_deref()),
            Statement::Insert { schema: _, table, query, returning, .. } => {
                check_readonly_table(table, "insert")?;
                let key = table.to_ascii_lowercase();
                if let Some(query) = query {
                    self.describe_select_statement(query, None)?;
                }
                if self.catalog.views.contains_key(&key) {
                    return Ok(None);
                }
                let def = self.describe_target_def(table, &key, scope, "insert")?;
                match returning {
                    Some(items) => Ok(Some(TargetShape::new(&def, table, None).plan(&def, items, "insert")?.columns)),
                    None => Ok(None),
                }
            }
            Statement::Update { schema: _, table, alias, assignments, from, filter, returning } => {
                check_readonly_table(table, "update")?;
                let key = table.to_ascii_lowercase();
                if self.catalog.views.contains_key(&key) {
                    if !from.is_empty() {
                        let mut error =
                            SqlError::new("unsupported-clause", "`UPDATE ... FROM` is not supported on a view; name a base table instead");
                        error.clause = Some("FROM".to_string());
                        return Err(error.in_statement("update"));
                    }
                    if returning.is_some() {
                        return Err(SqlError::new("unsupported-clause", "RETURNING on an INSTEAD OF view update is not supported yet")
                            .in_statement("update"));
                    }
                    return Ok(None);
                }
                let def = self.describe_target_def(table, &key, scope, "update")?;
                let shape = self.describe_update_shape(&def, table, alias.as_deref(), from)?;
                for assignment in assignments {
                    if !def.columns.iter().any(|column| column.name.eq_ignore_ascii_case(&assignment.column)) {
                        return Err(SqlError::new("unknown-column", format!("unknown column `{}` in table `{table}`", assignment.column))
                            .in_statement("update"));
                    }
                    shape.resolve(&assignment.value, "update")?;
                }
                shape.resolve_optional(filter.as_ref(), "update")?;
                match returning {
                    Some(items) => Ok(Some(TargetShape::new(&def, table, None).plan(&def, items, "update")?.columns)),
                    None => Ok(None),
                }
            }
            Statement::Delete { schema: _, table, alias, filter, returning } => {
                check_readonly_table(table, "delete")?;
                let key = table.to_ascii_lowercase();
                if self.catalog.views.contains_key(&key) {
                    if returning.is_some() {
                        return Err(SqlError::new("unsupported-clause", "RETURNING on an INSTEAD OF view delete is not supported yet")
                            .in_statement("delete"));
                    }
                    return Ok(None);
                }
                let def = self.describe_target_def(table, &key, scope, "delete")?;
                let shape = TargetShape::new(&def, alias.as_deref().unwrap_or(table), None);
                shape.resolve_optional(filter.as_ref(), "delete")?;
                match returning {
                    Some(items) => Ok(Some(TargetShape::new(&def, table, None).plan(&def, items, "delete")?.columns)),
                    None => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }

    /// 定位一条 DML 的目标表定义,镜像执行路径的查找顺序:temp 先于 main,
    /// `main.`/`temp.` 作用域只查各自一侧。返回克隆的定义,让调用方在只读
    /// 借用目录的同时安全持有它。
    fn describe_target_def(&self, table: &str, key: &str, scope: TableScope, statement: &'static str) -> Result<TableDef, SqlError> {
        let temp_position = if scope == TableScope::Main { None } else { self.catalog.temp_lookup.get(key).copied() };
        let main_position = if scope == TableScope::Temp {
            None
        }
        else {
            match temp_position {
                Some(_) => None,
                None => Some(
                    *self
                        .catalog
                        .table_lookup
                        .get(key)
                        .ok_or_else(|| SqlError::new("unknown-table", format!("unknown table `{table}`")).in_statement(statement))?,
                ),
            }
        };
        Ok(match temp_position {
            Some(position) => self.catalog.temp_tables[position].def.clone(),
            None => self.catalog.tables[main_position.expect("main table resolved")].clone(),
        })
    }

    /// 构造 `UPDATE ... FROM` 解析赋值与谓词所用的 `FROM` 形状,镜像执行期
    /// 的关系;目标表以目录列名进入,派生 `FROM` 源只推导名字。
    fn describe_update_shape<'a>(
        &self,
        def: &'a TableDef,
        table: &str,
        alias: Option<&str>,
        from: &[TableRef],
    ) -> Result<TargetShape<'a>, SqlError> {
        if from.is_empty() {
            return Ok(TargetShape::new(def, alias.unwrap_or(table), None));
        }
        let width = def.columns.len();
        let mut sources = vec![Source {
            label: alias.unwrap_or(table).to_string(),
            columns: SourceColumns::Materialized {
                names: def.columns.iter().map(|column| column.name.clone()).collect(),
                rowid: !def.without_rowid,
            },
            offset: 0,
            join: None,
            star_hidden: vec![false; width],
            unqualified_hidden: vec![false; width],
            star_override: vec![None; width],
        }];
        let mut offset = width + usize::from(!def.without_rowid);
        for entry in from {
            let (names, rowid) = self.describe_source(entry, None)?;
            let visible = names.len();
            sources.push(Source {
                label: entry.label().to_string(),
                columns: SourceColumns::Materialized { names, rowid },
                offset,
                join: entry.join,
                star_hidden: vec![false; visible],
                unqualified_hidden: vec![false; visible],
                star_override: vec![None; visible],
            });
            offset += visible + usize::from(rowid);
        }
        let source_count = sources.len();
        let relation = Relation { sources, using: vec![Vec::new(); source_count], using_slots: Vec::new(), trigger: None, excluded: false };
        let scope = relation.shape();
        Ok(TargetShape { relation, scope })
    }

    /// 推导一条读形 `PRAGMA` 的静态表头;写形 `PRAGMA` 没有行集结果,如实
    /// 返回 `None`。`table_info`/`table_xinfo` 指向视图时镜像执行期的
    /// `view_output` 校验:视图体推导失败(缺列、宽度不符)即整体失败。
    fn describe_pragma(&self, schema: Option<&str>, name: &str, value: Option<&str>) -> Result<Option<Vec<String>>, SqlError> {
        if name.eq_ignore_ascii_case("database_list") {
            return Ok(Some(vec!["seq".to_string(), "name".to_string(), "file".to_string()]));
        }
        let argument = value.unwrap_or_default();
        match name.to_ascii_lowercase().as_str() {
            "table_info" | "table_xinfo" => {
                let extended = name.eq_ignore_ascii_case("table_xinfo");
                if pragma_table_def(self, schema, argument).is_none() {
                    if let Some(view) = pragma_view_def(self, schema, argument) {
                        let _depth = ViewDepthGuard::enter()?;
                        let actual = self.describe_select_statement(&view.query, None)?;
                        view_output_names(view, &actual)?;
                    }
                }
                let mut columns =
                    vec!["cid".to_string(), "name".to_string(), "type".to_string(), "notnull".to_string(), "dflt_value".to_string(), "pk".to_string()];
                if extended {
                    columns.push("hidden".to_string());
                }
                Ok(Some(columns))
            }
            "index_list" => Ok(Some(vec!["seq".to_string(), "name".to_string(), "unique".to_string(), "origin".to_string(), "partial".to_string()])),
            "index_info" => Ok(Some(vec!["seqno".to_string(), "cid".to_string(), "name".to_string()])),
            "foreign_key_list" => Ok(Some(vec![
                "id".to_string(),
                "seq".to_string(),
                "table".to_string(),
                "from".to_string(),
                "to".to_string(),
                "on_update".to_string(),
                "on_delete".to_string(),
                "match".to_string(),
            ])),
            "integrity_check" => Ok(Some(vec!["integrity_check".to_string()])),
            "user_version" | "application_id" | "foreign_keys" | "recursive_triggers" => {
                Ok(if argument.is_empty() { Some(vec![name.to_string()]) } else { None })
            }
            _ => Err(SqlError::new(
                "unsupported-pragma",
                format!(
                    "unsupported pragma `{name}`; this engine supports `foreign_keys`, `recursive_triggers`, `table_info`, `table_xinfo`, `index_list`, `index_info`, `foreign_key_list`, `database_list`, `user_version`, `application_id` and `integrity_check`"
                ),
            )
            .in_clause("PRAGMA")
            .in_statement("pragma")),
        }
    }

    /// 推导一条已解析 `SELECT`(含其 `WITH` 前言)的结果列标签。
    /// `env` 是外层已物化的 CTE 名字表,与执行期的 `CteEnv` 平行:派生表
    /// 能看见外层 CTE,视图体看不见(镜像执行路径的传参)。
    fn describe_select_statement(&self, statement: &Statement, env: Option<&DescribeEnv>) -> Result<Vec<String>, SqlError> {
        let Statement::Select { with, first, compounds, order, limit, .. } = statement
        else {
            return Err(SqlError::new("unsupported-statement", format!("a subquery must be a `SELECT` but found `{}`", statement.name()))
                .in_statement("select"));
        };
        if with.is_empty() {
            return self.describe_select(first, compounds, order, limit.as_ref(), env);
        }
        let mut owned = DescribeEnv { parent: env, tables: BTreeMap::new() };
        for entry in with {
            let columns = match self.recursive_start(entry)? {
                // 递归项:镜像 `materialize_recursive` 的种子推导——种子列
                // 即 CTE 列,且不做去重改名(执行路径同样没有)。
                Some(start) => {
                    let Statement::Select { with: inner_with, first, compounds, .. } = entry.query.as_ref()
                    else {
                        return Err(SqlError::new("unsupported-statement", "a `WITH` body must be a `SELECT`").in_statement("select"));
                    };
                    let seed = Statement::Select {
                        with: inner_with.clone(),
                        recursive: false,
                        first: first.clone(),
                        compounds: compounds[..start].to_vec(),
                        order: Vec::new(),
                        limit: None,
                    };
                    let seed_columns = self.describe_select_statement(&seed, Some(&owned))?;
                    if let Some(names) = &entry.columns {
                        if names.len() != seed_columns.len() {
                            let mut error = SqlError::new(
                                "arity-mismatch",
                                format!("table {} has {} values for {} columns", entry.name, seed_columns.len(), names.len()),
                            )
                            .in_statement("select");
                            error.clause = Some("WITH".to_string());
                            return Err(error);
                        }
                    }
                    entry.columns.clone().unwrap_or(seed_columns)
                }
                None => {
                    let result_columns = self.describe_select_statement(&entry.query, Some(&owned))?;
                    if let Some(names) = &entry.columns {
                        if names.len() != result_columns.len() {
                            let mut error = SqlError::new(
                                "arity-mismatch",
                                format!("table {} has {} values for {} columns", entry.name, result_columns.len(), names.len()),
                            )
                            .in_statement("select");
                            error.clause = Some("WITH".to_string());
                            return Err(error);
                        }
                    }
                    unique_output_names(&entry.columns.clone().unwrap_or(result_columns))
                }
            };
            owned.tables.insert(entry.name.to_ascii_lowercase(), columns);
        }
        self.describe_select(first, compounds, order, limit.as_ref(), Some(&owned))
    }

    /// 推导一个 `SELECT` 语句体(可能复合)的结果列标签。`LIMIT` 表达式按
    /// 执行路径 [`Engine::eval_limit`] 的同一位置、同一关系求名字:只解析
    /// 不求值,字面量才做整数转换检查,避免预编译期执行任何子查询。
    fn describe_select(
        &self,
        first: &SelectCore,
        compounds: &[CompoundTerm],
        order: &[OrderKey],
        limit: Option<&Limit>,
        env: Option<&DescribeEnv>,
    ) -> Result<Vec<String>, SqlError> {
        if let Some(limit) = limit {
            let relation = Relation { sources: Vec::new(), using: Vec::new(), using_slots: Vec::new(), trigger: None, excluded: false };
            let mut aggregates = Vec::new();
            let mut windows = Vec::new();
            if let Some(offset) = &limit.offset {
                let resolved = resolve_expr(offset, &relation, &[], &[], &mut windows, &mut aggregates, false, false)?;
                if let ResolvedExpr::Cell(cell) = &resolved {
                    limit_integer(cell, "OFFSET")?;
                }
            }
            let resolved = resolve_expr(&limit.count, &relation, &[], &[], &mut windows, &mut aggregates, false, false)?;
            if let ResolvedExpr::Cell(cell) = &resolved {
                limit_integer(cell, "LIMIT")?;
            }
        }
        if compounds.is_empty() {
            return self.describe_core(first, order, env);
        }
        let columns = self.describe_core(first, &[], env)?;
        for term in compounds {
            let right = self.describe_core(&term.core, &[], env)?;
            if right.len() != columns.len() {
                let mut error = SqlError::new(
                    "arity-mismatch",
                    format!("SELECTs to the left and right of {} do not have the same number of result columns", term.op.text()),
                )
                .in_statement("select");
                error.clause = Some(term.op.clause().to_string());
                error.position = Some(term.position);
                return Err(error);
            }
        }
        if !order.is_empty() {
            compound_order_keys(order, &columns)?;
        }
        Ok(columns)
    }

    /// 推导一个 `SELECT` 核心的结果列标签。
    ///
    /// 与执行路径 [`Engine::select`] 同一顺序:先按目录构造每个 `FROM` 源
    /// 的名字表,再规划 `USING`/`NATURAL` 合并,然后解析 `ON`、投影、
    /// `WHERE`、`GROUP BY`、`HAVING`、`ORDER BY`——任何一处名字解析失败即
    /// 整体失败(执行路径同样在读行之前全部解析,坏名字 fail-closed)。
    /// 标签由 [`core_output_labels`] 产出,与执行期共用。派生表、视图与
    /// CTE 只递归推导名字,不物化任何行。
    fn describe_core(&self, core: &SelectCore, order: &[OrderKey], env: Option<&DescribeEnv>) -> Result<Vec<String>, SqlError> {
        let SelectCore { distinct: _, items, from, filter, group, having, windows: named } = core;
        let mut sources = Vec::with_capacity(from.len());
        let mut offset = 0;
        for table in from {
            let (names, rowid) = self.describe_source(table, env)?;
            let width = names.len();
            sources.push(Source {
                label: table.label().to_string(),
                columns: SourceColumns::Materialized { names, rowid },
                offset,
                join: table.join,
                star_hidden: vec![false; width],
                unqualified_hidden: vec![false; width],
                star_override: vec![None; width],
            });
            offset += width + usize::from(rowid);
        }
        let mut relation = Relation { sources, using: vec![Vec::new(); from.len()], using_slots: Vec::new(), trigger: None, excluded: false };
        relation.plan_joins(from)?;
        for (index, table) in from.iter().enumerate() {
            if let Some(on) = &table.on {
                let mut scratch_windows = Vec::new();
                let mut scratch_aggregates = Vec::new();
                let resolved = resolve_expr(on, &relation, &[], &[], &mut scratch_windows, &mut scratch_aggregates, false, false)?;
                let mut refs = Vec::new();
                resolved.collect_sources(&mut refs);
                if refs.iter().any(|source| *source > index) {
                    return Err(SqlError::new(
                        "unknown-column",
                        format!("the `ON` clause of source {} may not reference a later source", index + 1),
                    )
                    .in_statement("select"));
                }
            }
        }
        let mut specs = Vec::new();
        let mut windows: Vec<ResolvedWindow> = Vec::new();
        if let SelectItems::Named(exprs) = items {
            for expr in exprs {
                resolve_expr(expr, &relation, &[], named, &mut windows, &mut specs, true, true)?;
            }
        }
        let labels = core_output_labels(&relation, items);
        if let Some(predicate) = filter {
            let mut scratch_windows = Vec::new();
            let mut scratch_aggregates = Vec::new();
            resolve_expr(predicate, &relation, &[], &[], &mut scratch_windows, &mut scratch_aggregates, false, false)?;
        }
        for expr in group {
            if let Expr::Literal(Literal::Integer(position)) = expr {
                let position = *position;
                if position < 1 || position as usize > labels.len() {
                    return Err(SqlError::new(
                        "order-term-out-of-range",
                        format!("GROUP BY term out of range - should be between 1 and {}", labels.len()),
                    )
                    .in_statement("select"));
                }
                continue;
            }
            let mut scratch_windows = Vec::new();
            let mut scratch_aggregates = Vec::new();
            resolve_expr(expr, &relation, &[], &[], &mut scratch_windows, &mut scratch_aggregates, false, false)?;
        }
        if let Some(having_expr) = having {
            resolve_expr(having_expr, &relation, &[], named, &mut windows, &mut specs, true, false)?;
        }
        if having.is_some() && group.is_empty() && specs.is_empty() {
            let mut error = SqlError::new("misuse-of-aggregate", "HAVING clause on a non-aggregate query").in_statement("select");
            error.clause = Some("HAVING".to_string());
            return Err(error);
        }
        for entry in order {
            if let Expr::Literal(Literal::Integer(position)) = &entry.expr {
                let position = *position;
                if position < 1 || position as usize > labels.len() {
                    return Err(SqlError::new(
                        "order-term-out-of-range",
                        format!("ORDER BY term out of range - should be between 1 and {}", labels.len()),
                    )
                    .in_statement("select"));
                }
                continue;
            }
            if let Expr::Column(column) = &entry.expr {
                if column.qualifier.is_none() && labels.iter().any(|label| label.eq_ignore_ascii_case(&column.name)) {
                    continue;
                }
            }
            resolve_expr(&entry.expr, &relation, &[], named, &mut windows, &mut specs, true, true)?;
        }
        Ok(labels)
    }

    /// 推导一个 `FROM` 源暴露的列名与隐藏 rowid 标志,镜像执行路径
    /// [`Engine::select_source`] 的解析顺序:派生表 → schema 限定 → CTE →
    /// 系统表 → temp 视图 → 视图 → temp 表 → main 表 → 附加库回退。
    /// 每一步只取名字:派生表与视图递归 [`Self::describe_select_statement`],
    /// 基础表直接读目录定义,系统表用静态表头。
    fn describe_source(&self, table: &TableRef, env: Option<&DescribeEnv>) -> Result<(Vec<String>, bool), SqlError> {
        if let Some(query) = &table.query {
            let columns = self.describe_select_statement(query, env)?;
            return Ok((unique_output_names(&columns), false));
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
            return self.describe_attached_source(index, table);
        };
        if table.schema.is_none() {
            if let Some(names) = env.and_then(|env| env.lookup(&table.name)) {
                return Ok((names.clone(), false));
            }
        }
        if let Some(system) = system_for_scope(scope, &table.name) {
            return self.describe_system_rows(system, &table.name);
        }
        let key = table.name.to_ascii_lowercase();
        if scope != TableScope::Main {
            if let Some(view) = self.catalog.temp_views.get(&key) {
                let _depth = ViewDepthGuard::enter()?;
                let actual = self.describe_select_statement(&view.def.query, None)?;
                return Ok((view_output_names(&view.def, &actual)?, false));
            }
        }
        if scope != TableScope::Temp {
            if let Some(view) = self.catalog.views.get(&key) {
                let _depth = ViewDepthGuard::enter()?;
                let actual = self.describe_select_statement(&view.query, None)?;
                return Ok((view_output_names(view, &actual)?, false));
            }
        }
        if scope != TableScope::Main {
            if let Some(position) = self.catalog.temp_lookup.get(&key).copied() {
                let def = &self.catalog.temp_tables[position].def;
                return Ok((def.columns.iter().map(|column| column.name.clone()).collect(), !def.without_rowid));
            }
        }
        if scope != TableScope::Temp {
            if let Some(position) = self.catalog.table_lookup.get(&key).copied() {
                let def = &self.catalog.tables[position];
                return Ok((def.columns.iter().map(|column| column.name.clone()).collect(), !def.without_rowid));
            }
        }
        if table.schema.is_none() {
            for index in 0..self.attached.len() {
                if let Ok(source) = self.describe_attached_source(index, table) {
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

    /// 推导附加数据库中一个 `FROM` 源的名字表,镜像 [`Engine::attached_source`]。
    fn describe_attached_source(&self, index: usize, table: &TableRef) -> Result<(Vec<String>, bool), SqlError> {
        let engine = &self.attached[index].engine;
        if let Some(system) = SystemTable::from_name(&table.name) {
            return engine.describe_system_rows(system, &table.name);
        }
        let key = table.name.to_ascii_lowercase();
        if let Some(view) = engine.catalog.temp_views.get(&key) {
            let _depth = ViewDepthGuard::enter()?;
            let actual = engine.describe_select_statement(&view.def.query, None)?;
            return Ok((view_output_names(&view.def, &actual)?, false));
        }
        if let Some(view) = engine.catalog.views.get(&key) {
            let _depth = ViewDepthGuard::enter()?;
            let actual = engine.describe_select_statement(&view.query, None)?;
            return Ok((view_output_names(view, &actual)?, false));
        }
        if let Some(position) = engine.catalog.table_lookup.get(&key).copied() {
            let def = &engine.catalog.tables[position];
            return Ok((def.columns.iter().map(|column| column.name.clone()).collect(), !def.without_rowid));
        }
        let qualifier = table.schema.as_deref().unwrap_or_default();
        Err(SqlError::new("unknown-table", format!("no such table: {qualifier}.{}", table.name)).in_statement("select"))
    }

    /// 系统表的静态列名与隐藏 rowid 标志,镜像 [`Engine::system_rows`] 的
    /// 名字部分:`sqlite_sequence` 只在目录里真的存在时可用,与执行期一致。
    fn describe_system_rows(&self, system: SystemTable, name: &str) -> Result<(Vec<String>, bool), SqlError> {
        match system {
            SystemTable::Master | SystemTable::TempMaster => {
                Ok((vec!["type".to_string(), "name".to_string(), "tbl_name".to_string(), "rootpage".to_string(), "sql".to_string()], true))
            }
            SystemTable::Sequence => {
                let present = self
                    .writer
                    .schema_objects(TableLimits::default())
                    .map_err(storage_error)?
                    .iter()
                    .any(|object| object.kind == SchemaObjectKind::Table && object.name.eq_ignore_ascii_case(name));
                if !present {
                    return Err(SqlError::new("unknown-table", format!("no such table: {name}")).in_statement("select"));
                }
                Ok((vec!["name".to_string(), "seq".to_string()], true))
            }
        }
    }
}

/// 一个 `WITH` 条目在 describe 路径上的名字表,与执行期的 `CteEnv` 平行但
/// 只携带列名,不携带行。链式父作用域镜像 CTE 作用域的嵌套可见性。
struct DescribeEnv<'a> {
    parent: Option<&'a DescribeEnv<'a>>,
    tables: BTreeMap<String, Vec<String>>,
}

impl<'a> DescribeEnv<'a> {
    fn lookup(&self, name: &str) -> Option<&Vec<String>> {
        self.tables.get(&name.to_ascii_lowercase()).or_else(|| self.parent.and_then(|parent| parent.lookup(name)))
    }
}

/// 一个 `SELECT` 核心的结果列标签。
///
/// 执行路径与 [`Engine::describe`] 共用本函数,保证预编译期推导的表头与
/// 执行后 `ExecResult.columns` 逐字节一致:裸列引用按目录拼写命名(所以
/// `SELECT A FROM t` 报 `a`),别名取书写名,表达式列由 `Expr::label` 重建
/// 文本,`*` 按 `FROM` 源的可见列展开(含 `USING` 合并列的隐藏规则)。
fn core_output_labels(relation: &Relation<'_>, items: &SelectItems) -> Vec<String> {
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
fn compound_order_keys(order: &[OrderKey], columns: &[String]) -> Result<Vec<(usize, SortDirection)>, SqlError> {
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

/// Upper bound on rows emitted by one statement, shared with every nested
/// subquery. Counting emissions rather than wall-clock keeps the result
/// deterministic across machines.
const EXECUTION_BUDGET: usize = 1_000_000;

/// Row budget for one statement.
struct Budget {
    remaining: std::cell::Cell<usize>,
}

impl Budget {
    fn new() -> Self {
        Self { remaining: std::cell::Cell::new(EXECUTION_BUDGET) }
    }

    fn charge(&self, rows: usize) -> Result<(), SqlError> {
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
struct Materialized {
    columns: Vec<String>,
    rows: Vec<Vec<Cell>>,
}

/// `WITH` entries in scope, chained so a nested query keeps seeing the outer
/// ones. A CTE shadows a base table with the same name.
struct CteEnv<'a> {
    parent: Option<&'a CteEnv<'a>>,
    tables: BTreeMap<String, Materialized>,
}

impl<'a> CteEnv<'a> {
    fn lookup(&self, name: &str) -> Option<&Materialized> {
        self.tables.get(&name.to_ascii_lowercase()).or_else(|| self.parent.and_then(|parent| parent.lookup(name)))
    }
}

/// Evaluation context shared by every expression of one `SELECT` core.
struct EvalContext<'a> {
    engine: &'a Engine,
    /// Enclosing scopes, innermost first.
    outer: &'a [Scope],
    /// Column catalog of the core currently being evaluated.
    current: &'a ScopeShape,
    /// Whole-result aggregate values for the current row.
    aggregates: &'a [Cell],
    /// Already computed window values, one vector per window of this core.
    windows: &'a [Vec<Cell>],
    /// Row currently being projected, indexing into `windows`.
    window_row: usize,
    budget: &'a Budget,
    /// CTEs visible to the current core, including enclosing `WITH` clauses.
    ctes: Option<&'a CteEnv<'a>>,
}

/// Two result rows are equal when every cell compares equal under the engine's
/// value rules; `NULL` equals `NULL`, as set operations require.
fn rows_equal(left: &[Cell], right: &[Cell]) -> bool {
    left.len() == right.len() && left.iter().zip(right).all(|(first, second)| compare_cells(first, second) == Ordering::Equal)
}

fn rows_contain(rows: &[Vec<Cell>], candidate: &[Cell]) -> bool {
    rows.iter().any(|row| rows_equal(row, candidate))
}

/// Removes duplicate rows while keeping the first occurrence of each value.
fn distinct_rows(rows: &[Vec<Cell>]) -> Vec<Vec<Cell>> {
    retain_first_distinct(rows.to_vec(), &[])
}

/// Applies one set operator with SQLite's multiplicity rules.
fn combine_rows(op: CompoundOp, left: Vec<Vec<Cell>>, right: Vec<Vec<Cell>>) -> Vec<Vec<Cell>> {
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
fn circular_reference(name: &str) -> SqlError {
    SqlError::new("SQLITE_ERROR", format!("circular reference: {name}")).in_statement("select")
}

/// Whether a projection turns each input row into at most one output row, so a
/// recursive CTE may be truncated at the enclosing `LIMIT`.
fn row_preserving_items(items: &SelectItems) -> bool {
    match items {
        SelectItems::Star => true,
        SelectItems::CountAll => false,
        SelectItems::Named(exprs) => exprs.iter().all(row_preserving_expr),
    }
}

fn row_preserving_expr(expr: &Expr) -> bool {
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

/// One row of the input a window partition is computed over: either a source
/// row or the representative row of a group, plus that row's aggregate values.
#[derive(Clone)]
struct WindowRow {
    /// Row cells.
    values: Vec<Cell>,
    /// Whole-result aggregate values, empty in an ungrouped query.
    aggregates: Vec<Cell>,
}

/// Transitional home for key comparison: the direction shape lives in
/// `database_query`, the cell comparison mechanism stays with the facade.
trait SortCompare {
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

/// Transitional home for expression evaluation: the resolved tree lives in
/// `database_query`, the evaluation mechanism stays with the facade until
/// the evaluation closure lands.
trait EvalExpr {
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
            Self::Parameter(index) => ctx.engine.parameters.get(*index).cloned().unwrap_or(Cell::Null),
            Self::Column { index, .. } => values[*index].clone(),
            Self::TriggerColumn { old, index } => ctx
                .engine
                .trigger_frames
                .last()
                .and_then(|frame| if *old { frame.old.as_ref() } else { frame.new.as_ref() })
                .and_then(|cells| cells.get(*index))
                .cloned()
                .unwrap_or(Cell::Null),
            Self::ExcludedColumn { index } => ctx.engine.upsert_excluded.get(*index).cloned().unwrap_or(Cell::Null),
            Self::OuterColumn { depth, index } => {
                ctx.engine.outer_read.set(true);
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
                let implementation = ctx
                    .engine
                    .host_functions
                    .get(name)
                    .ok_or_else(|| SqlError::new("unknown-function", format!("no such function: {name}")))?;
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
                    let cache = ctx.engine.subquery_cache.borrow();
                    if let Some(SubqueryCache::Rows(rows)) = cache.get(&key) {
                        return Ok(rows.first().map_or(Cell::Null, |row| row[0].clone()));
                    }
                }
                ctx.engine.outer_read.set(false);
                let result = run_subquery(query, values, ctx)?;
                check_single_column(&result)?;
                if !ctx.engine.outer_read.get() {
                    ctx.engine.subquery_cache.borrow_mut().insert(key, SubqueryCache::Rows(result.rows.clone()));
                }
                result.rows.first().map_or(Cell::Null, |row| row[0].clone())
            }
            Self::Exists(query) => {
                // Existence is two-valued: no result value is consumed, so a
                // surrounding `NOT` can never turn this into `NULL`.
                let key = query.as_ref() as *const Statement as usize;
                {
                    let cache = ctx.engine.subquery_cache.borrow();
                    if let Some(SubqueryCache::Rows(rows)) = cache.get(&key) {
                        return Ok(Cell::Integer(i64::from(!rows.is_empty())));
                    }
                }
                ctx.engine.outer_read.set(false);
                if let Some(answer) = ctx.engine.correlated_exists(query, values, ctx)? {
                    return Ok(Cell::Integer(i64::from(answer)));
                }
                let result = run_subquery(query, values, ctx)?;
                if !ctx.engine.outer_read.get() {
                    ctx.engine.subquery_cache.borrow_mut().insert(key, SubqueryCache::Rows(result.rows.clone()));
                }
                Cell::Integer(i64::from(!result.rows.is_empty()))
            }
            Self::InSubquery { value, query, negated } => {
                let value = value.eval(values, ctx)?;
                let key = query.as_ref() as *const Statement as usize;
                {
                    let cache = ctx.engine.subquery_cache.borrow();
                    if let Some(SubqueryCache::InList { values: list, keys, saw_null }) = cache.get(&key) {
                        return Ok(in_result_cached(&value, list, keys.as_ref(), *saw_null, *negated));
                    }
                }
                ctx.engine.outer_read.set(false);
                let result = run_subquery(query, values, ctx)?;
                check_single_column(&result)?;
                let correlated = ctx.engine.outer_read.get();
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
                    ctx.engine.subquery_cache.borrow_mut().insert(key, SubqueryCache::InList { values: list, keys, saw_null });
                }
                answer
            }
        })
    }
}

/// One resolved `ORDER BY` key.
enum SortKey {
    /// Zero-based index into the projection output.
    Projected(usize),
    /// Expression over the merged source row.
    Expression(ResolvedExpr),
}

/// One `GROUP BY` term.
enum GroupKey {
    /// A one-based projection ordinal.
    Projected(usize, Collation),
    /// Expression over the merged source row.
    Expression(ResolvedExpr, Collation),
}

/// Runs one nested `SELECT` with the current row as the innermost enclosing
/// scope.
///
/// The subquery reuses the same `select` evaluator, so its `FROM`, predicates,
/// ordering, compounds and further nesting all behave exactly like a top-level
/// statement.
fn run_subquery(query: &Statement, values: &[Cell], ctx: &EvalContext) -> Result<ExecResult, SqlError> {
    ctx.budget.charge(1)?;
    let mut scopes = Vec::with_capacity(ctx.outer.len() + 1);
    scopes.push(ctx.current.instantiate(values));
    scopes.extend(ctx.outer.iter().cloned());
    // The subquery keeps the CTEs that are in scope where it appears, so
    // `(select ... from c)` inside a `WITH` statement resolves too.
    ctx.engine.select_statement(query, &scopes, ctx.budget, ctx.ctes)
}

/// A scalar subquery and an `IN (SELECT ...)` must produce exactly one column,
/// as upstream SQLite enforces at prepare time.
fn check_single_column(result: &ExecResult) -> Result<(), SqlError> {
    if result.columns.len() != 1 {
        return Err(
            SqlError::new("arity-mismatch", format!("sub-select returns {} columns - expected 1", result.columns.len())).in_statement("select")
        );
    }
    Ok(())
}

/// Which position of a frame `first_value`/`last_value`/`nth_value` reads.
#[derive(Clone, Copy)]
enum ValuePick {
    /// First row of the frame.
    First,
    /// Last row of the frame.
    Last,
    /// `nth` row of the frame.
    Nth,
}

/// Compares two input rows by partition keys, then by ORDER BY keys.
fn compare_window_rows(
    left_partition: &[Cell],
    left_order: &[Cell],
    right_partition: &[Cell],
    right_order: &[Cell],
    order: &[(ResolvedExpr, SortDirection)],
) -> Ordering {
    for (left, right) in left_partition.iter().zip(right_partition) {
        let ordering = compare_cells(left, right);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    for (index, (_, direction)) in order.iter().enumerate() {
        let ordering = direction.compare(&left_order[index], &right_order[index]);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// Row-wise equality using SQLite's cross-type numeric rule (`1` equals `1.0`).
fn cells_equal(left: &[Cell], right: &[Cell]) -> bool {
    left.len() == right.len() && left.iter().zip(right).all(|(left, right)| compare_cells(left, right) == Ordering::Equal)
}

/// Integer offset a frame edge carries; `None` for `UNBOUNDED` and `CURRENT ROW`.
fn range_offset(bound: &ResolvedFrameBound, which: &str) -> Result<Option<i64>, SqlError> {
    match &bound.offset {
        Some(ResolvedExpr::Cell(Cell::Integer(value))) if *value >= 0 => Ok(Some(*value)),
        Some(_) => Err(SqlError::new("invalid-frame", format!("frame {which} offset must be a non-negative integer")).in_statement("select")),
        None => Ok(None),
    }
}

/// Value an `RANGE` edge names on the ORDER BY axis, given the sort direction.
fn range_edge(edge: FrameEdge, offset: Option<i64>, current: f64, descending: bool) -> f64 {
    let offset = offset.unwrap_or(0) as f64;
    match edge {
        FrameEdge::UnboundedPreceding => {
            if descending {
                f64::INFINITY
            }
            else {
                f64::NEG_INFINITY
            }
        }
        FrameEdge::UnboundedFollowing => {
            if descending {
                f64::NEG_INFINITY
            }
            else {
                f64::INFINITY
            }
        }
        FrameEdge::CurrentRow => current,
        FrameEdge::Preceding => {
            if descending {
                current + offset
            }
            else {
                current - offset
            }
        }
        FrameEdge::Following => {
            if descending {
                current - offset
            }
            else {
                current + offset
            }
        }
    }
}

/// Resolves one `ROWS` frame edge to a position in the partition.
fn rows_edge(bound: &ResolvedFrameBound, which: &str, slot: usize, len: usize) -> Result<i64, SqlError> {
    let position = slot as i64;
    Ok(match bound.edge {
        FrameEdge::UnboundedPreceding => 0,
        FrameEdge::UnboundedFollowing => len.saturating_sub(1) as i64,
        FrameEdge::CurrentRow => position,
        // Frames never extend past either end of the partition.
        FrameEdge::Preceding => position.saturating_sub(range_offset(bound, which)?.unwrap_or(0)).max(0),
        FrameEdge::Following => position.saturating_add(range_offset(bound, which)?.unwrap_or(0)).min(len.saturating_sub(1) as i64),
    })
}

/// Resolves one `GROUPS` frame edge to a position in the partition.
///
/// Peer groups are contiguous, so `group_of` (each row's dense group number)
/// both locates the target group and tells whether it lies outside the
/// partition.
fn groups_edge(
    bound: &ResolvedFrameBound,
    which: &str,
    slot: usize,
    len: usize,
    peer_start: &[usize],
    peer_end: &[usize],
    group_of: &[i64],
) -> Result<i64, SqlError> {
    let starting = which == "starting";
    // `CURRENT ROW` names the row's whole peer group: a start edge lands on its
    // first row and an end edge on its last.
    if bound.edge == FrameEdge::CurrentRow {
        return Ok(if starting { peer_start[slot] as i64 } else { peer_end[slot] as i64 });
    }
    let group = group_of[slot];
    let target = match bound.edge {
        FrameEdge::UnboundedPreceding => return Ok(if starting { 0 } else { -1 }),
        FrameEdge::UnboundedFollowing => {
            return Ok(if starting { len as i64 } else { len.saturating_sub(1) as i64 });
        }
        FrameEdge::Preceding => group.saturating_sub(range_offset(bound, which)?.unwrap_or(0)),
        FrameEdge::Following => group.saturating_add(range_offset(bound, which)?.unwrap_or(0)),
        FrameEdge::CurrentRow => unreachable!("handled above"),
    };
    if target < 0 {
        return Ok(if starting { 0 } else { -1 });
    }
    // Frames never extend past either end of the partition.
    let first = group_of.partition_point(|value| *value < target);
    if first >= len {
        return Ok(if starting { len as i64 } else { len as i64 - 1 });
    }
    Ok(if starting { peer_start[first] as i64 } else { peer_end[first] as i64 })
}

/// `EXCLUDE` decision for one frame position computed for `slot`.
fn frame_row_excluded(exclude: FrameExclude, position: usize, slot: usize, peer_start: &[usize], peer_end: &[usize]) -> bool {
    match exclude {
        FrameExclude::NoOthers => false,
        FrameExclude::CurrentRow => position == slot,
        FrameExclude::Group => position >= peer_start[slot] && position <= peer_end[slot],
        // `TIES` drops the peers but keeps the row being computed, even when it
        // is the only member of its group.
        FrameExclude::Ties => position >= peer_start[slot] && position <= peer_end[slot] && position != slot,
    }
}

/// Bucket count of `ntile(n)`; SQLite rejects zero and negative counts.
fn ntile_buckets(cell: &Cell) -> Result<usize, SqlError> {
    match numeric(cell) {
        Some(Number::Int(value)) if value > 0 => Ok(value as usize),
        Some(Number::Real(value)) if value.is_finite() && value.fract() == 0.0 && value > 0.0 && value <= usize::MAX as f64 => {
            Ok(value as usize)
        }
        _ => Err(SqlError::new("invalid-argument", "argument of ntile must be a positive integer").in_statement("select")),
    }
}

/// Position argument of `nth_value`; SQLite rejects zero and negative values.
fn nth_position(cell: &Cell) -> Result<i64, SqlError> {
    match numeric(cell) {
        Some(Number::Int(value)) if value > 0 => Ok(value),
        Some(Number::Real(value)) if value.is_finite() && value.fract() == 0.0 && value > 0.0 && value <= i64::MAX as f64 => Ok(value as i64),
        _ => Err(SqlError::new("invalid-argument", "second argument to nth_value must be a positive integer").in_statement("select")),
    }
}

/// Offset of `lag`/`lead`; a non-integer offset makes SQLite yield `NULL`.
fn lag_offset(cell: &Cell) -> Option<i64> {
    match numeric(cell)? {
        Number::Int(value) => Some(value),
        Number::Real(value) if value.is_finite() && value.fract() == 0.0 && value >= i64::MIN as f64 && value <= i64::MAX as f64 => {
            Some(value as i64)
        }
        Number::Real(_) => None,
    }
}

/// Converts a `LIMIT`/`OFFSET` expression result to an integer, or reports
/// SQLite's `datatype mismatch`.
fn limit_integer(cell: &Cell, clause: &str) -> Result<i64, SqlError> {
    let converted = match cell {
        Cell::Integer(value) => Some(*value),
        Cell::Real(value) => real_integer(*value),
        // Text first tries the exact 64-bit form, then the numeric one, so
        // `'9223372036854775807'` survives where an `f64` round-trip would not.
        Cell::Text(text) => match text.trim().parse::<i64>() {
            Ok(value) => Some(value),
            Err(_) => text.trim().parse::<f64>().ok().and_then(real_integer),
        },
        Cell::Null | Cell::Blob(_) => None,
    };
    converted.ok_or_else(|| {
        let mut error = SqlError::new("datatype-mismatch", "datatype mismatch").in_statement("select");
        error.clause = Some(clause.to_string());
        error
    })
}

fn real_integer(value: f64) -> Option<i64> {
    if value.is_finite() && value.fract() == 0.0 && value >= -9223372036854775808.0 && value < 9223372036854775808.0 {
        Some(value as i64)
    }
    else {
        None
    }
}

/// Clamps a limit pair to the rows actually available.
fn limit_range(len: usize, limit: Option<(usize, Option<usize>)>) -> (usize, usize) {
    match limit {
        None => (0, len),
        Some((offset, count)) => {
            let start = offset.min(len);
            let end = match count {
                Some(count) => start.saturating_add(count).min(len),
                None => len,
            };
            (start, end)
        }
    }
}

/// Statement name reported for one transaction action, matching
/// [`Statement::name`].
fn action_name(action: &TransactionAction) -> &'static str {
    match action {
        TransactionAction::Begin => "begin",
        TransactionAction::Commit => "commit",
        TransactionAction::Rollback => "rollback",
        TransactionAction::Savepoint(_) => "savepoint",
        TransactionAction::Release(_) => "release",
        TransactionAction::RollbackTo(_) => "rollback-to",
    }
}

/// Drops the rows a limit pair hides.
fn truncate_rows(rows: &mut Vec<Vec<Cell>>, limit: Option<(usize, Option<usize>)>) {
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
fn retain_first_distinct(rows: Vec<Vec<Cell>>, collations: &[Collation]) -> Vec<Vec<Cell>> {
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
fn compute_aggregate(spec: &AggregateSpec, rows: &[Vec<Cell>], ctx: &EvalContext) -> Result<Cell, SqlError> {
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
fn shape_aggregate_inputs(
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
fn aggregate_cells(function: AggregateFunction, values: &[Cell], separators: Option<&[Cell]>) -> Cell {
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
fn build_groups(
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
fn group_signature(key: &[Cell], group_keys: &[GroupKey]) -> Option<Vec<ProbeKey>> {
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
fn group_keys_equal(left: &[Cell], right: &[Cell], group_keys: &[GroupKey]) -> bool {
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
fn count_inner_join_rows(
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
fn join_level_conjuncts_hold(
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
struct RowidProbe<'a> {
    within: usize,
    probe: &'a ResolvedExpr,
    ascending: bool,
}

/// 一条 `ON <右侧列> = <前缀表达式>` 的等值探针:右表按该列按排序规则归一化后
/// 建一次哈希桶,左行求值后直接取桶,不再逐行扫描右表。
struct HashProbe<'a> {
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
enum JoinProbe<'a> {
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
fn column_join_probe<'a>(source: &Source, on: &'a Option<ResolvedExpr>, index: usize) -> Option<(usize, &'a ResolvedExpr, Collation)> {
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
fn build_probe_buckets(rows: &[Vec<Cell>], within: usize, collation: Collation) -> Option<HashMap<u64, Vec<usize>>> {
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
fn probe_hash(cell: &Cell, collation: Collation) -> Option<u64> {
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
fn count_join_level(
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
fn join_rows(
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
fn coalesce_using(row: &mut [Cell], pairs: &[UsingJoin]) {
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
fn rowid_join_probe<'a>(source: &Source, on: &'a Option<ResolvedExpr>, index: usize) -> Option<(usize, &'a ResolvedExpr)> {
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
fn join_product(tables: &[Rc<Vec<Vec<Cell>>>]) -> Vec<Vec<Cell>> {
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
fn is_bare_count_star(items: &SelectItems) -> bool {
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
fn strip_alias(expr: &Expr) -> &Expr {
    match expr {
        Expr::Alias { value, .. } => strip_alias(value),
        other => other,
    }
}

/// Marks the columns one `WHERE` expression can read, mirroring the resolver's
/// name lookup for a single table source.
///
/// Returns `false` when a read cannot be attributed from the raw syntax (a
/// subquery, a window call, another table's qualifier, an unknown name), which
/// tells the caller to decode every column instead of guessing.
fn predicate_columns(expr: &Expr, label: &str, def: &TableDef, keep: &mut [bool]) -> bool {
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

/// Flattens `a AND b AND c` into single conjuncts, each applied as soon as its
/// sources are bound.
fn split_conjuncts(predicate: ResolvedExpr, out: &mut Vec<ResolvedExpr>) {
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
fn conjunct_bind_levels(conjuncts: &[ResolvedExpr], source_count: usize) -> Vec<Option<usize>> {
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
fn binding_order(relation: &Relation, conjuncts: &[ResolvedExpr]) -> Vec<usize> {
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

/// Emits the Cartesian product, pruning a partial combination as soon as one
/// conjunct over the bound sources is false.
///
/// Sources are bound in `binding_order`; the merged row keeps `FROM` column
/// order throughout. A conjunct that contains a subquery cannot be attributed
/// to one source, so it waits for the complete row instead of pruning early.
/// Collects the `AND` conjuncts of the form `<column> = <literal>` (or the
/// mirrored shape) that name a column of one base table, as `(position, cell)`
/// pairs. A bound parameter counts as a literal once its value is known.
/// Anything outside a plain `AND` of `=` comparisons contributes nothing, so a
/// missed equality only costs the pushdown, never a row.
fn collect_index_equalities(expr: &Expr, def: &TableDef, label: &str, parameters: &[Cell], out: &mut Vec<(usize, Cell)>) {
    match expr {
        Expr::And(left, right) => {
            collect_index_equalities(left, def, label, parameters, out);
            collect_index_equalities(right, def, label, parameters, out);
        }
        Expr::Compare { left, op, right } if *op == CompareOp::Eq => {
            let (column, value) = match (left.as_ref(), right.as_ref()) {
                (Expr::Column(column), value) => (column, known_cell(value, parameters)),
                (value, Expr::Column(column)) => (column, known_cell(value, parameters)),
                _ => return,
            };
            if column.qualifier.as_deref().is_some_and(|qualifier| !qualifier.eq_ignore_ascii_case(label)) {
                return;
            }
            if let Some(cell) = value
                && let Some(position) = def.columns.iter().position(|entry| entry.name.eq_ignore_ascii_case(&column.name))
            {
                out.push((position, cell));
            }
        }
        _ => {}
    }
}

/// Collects the lower and upper bounds of an `AND` conjunction on one column.
///
/// Only a plain `AND` of `<column> <op> <literal>` (or the mirrored shape)
/// contributes, where a bound parameter counts as a literal once its value is
/// known; any other shape is ignored, so a missed bound only costs the
/// pushdown. When several bounds name the same side, the last one wins, which
/// stays a superset of the true result because the full predicate still runs.
fn collect_range_bounds(
    expr: &Expr,
    def: &TableDef,
    label: &str,
    position: usize,
    parameters: &[Cell],
    lower: &mut Option<IndexBound>,
    upper: &mut Option<IndexBound>,
) {
    match expr {
        Expr::And(left, right) => {
            collect_range_bounds(left, def, label, position, parameters, lower, upper);
            collect_range_bounds(right, def, label, position, parameters, lower, upper);
        }
        Expr::Compare { left, op, right } => {
            let (column, value, flipped) = match (left.as_ref(), right.as_ref()) {
                (Expr::Column(column), value) => (column, known_cell(value, parameters), false),
                (value, Expr::Column(column)) => (column, known_cell(value, parameters), true),
                _ => return,
            };
            let Some(cell) = value
            else {
                return;
            };
            if column.qualifier.as_deref().is_some_and(|qualifier| !qualifier.eq_ignore_ascii_case(label)) {
                return;
            }
            if def.columns.get(position).is_none_or(|entry| !entry.name.eq_ignore_ascii_case(&column.name)) {
                return;
            }
            let op = if flipped { flip_range_op(*op) } else { *op };
            let bound = IndexBound { cell, inclusive: matches!(op, CompareOp::GtEq | CompareOp::LtEq) };
            match op {
                CompareOp::Gt | CompareOp::GtEq => *lower = Some(bound),
                CompareOp::Lt | CompareOp::LtEq => *upper = Some(bound),
                _ => {}
            }
        }
        _ => {}
    }
}

/// Mirrors a range comparison written with the literal on the left.
fn flip_range_op(op: CompareOp) -> CompareOp {
    match op {
        CompareOp::Gt => CompareOp::Lt,
        CompareOp::GtEq => CompareOp::LtEq,
        CompareOp::Lt => CompareOp::Gt,
        CompareOp::LtEq => CompareOp::GtEq,
        other => other,
    }
}

/// 统计笛卡尔积中满足全部合取的行数,不物化结果行。
///
/// 绑定顺序与提前剪枝复用 [`cross_product`] 的同一套规则,因此计数与通用
/// 路径逐行一致,只是把最终结果行换成了计数。
fn count_cross_product_rows(
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
fn count_cross_level(
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

fn cross_product(
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
fn emit_rows(
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

/// 下推键的执行期取值：字面量，或绑定到该位置的参数。
///
/// 参数在语句执行前已经绑定，形态与字面量等价，所以把「仅字面量」放宽到「执行期
/// 已知的值」不改变结果：下推只收窄候选行，全量谓词照旧在下游执行。缺失的绑定
/// 退化为 `NULL`，既不会被当成可下推的整数键，也不会改变求值结果。
fn known_cell(expr: &Expr, parameters: &[Cell]) -> Option<Cell> {
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
fn integral_rowid(value: f64) -> Option<i64> {
    if !value.is_finite() || value.fract() != 0.0 || value.abs() > 9_007_199_254_740_992.0 {
        return None;
    }
    Some(value as i64)
}

/// SQLite storage-class order: `NULL` < numbers < text < blob.
fn storage_rank(cell: &Cell) -> u8 {
    match cell {
        Cell::Null => 0,
        Cell::Integer(_) | Cell::Real(_) => 1,
        Cell::Text(_) => 2,
        Cell::Blob(_) => 3,
    }
}

/// Compares two cells with the SQLite ordering rules.
///
/// Integers and reals compare numerically across the two storage classes, text
/// compares with `BINARY`, and blobs compare byte by byte.
fn compare_cells(left: &Cell, right: &Cell) -> Ordering {
    compare_cells_with(left, right, Collation::Binary)
}

/// 按给定排序规则比较两个单元格，文本以外的比较不受排序规则影响。
fn compare_cells_with(left: &Cell, right: &Cell, collation: Collation) -> Ordering {
    use Ordering::Equal;
    let (left_rank, right_rank) = (storage_rank(left), storage_rank(right));
    if left_rank != right_rank {
        return left_rank.cmp(&right_rank);
    }
    match (left, right) {
        (Cell::Null, Cell::Null) => Equal,
        (Cell::Integer(left), Cell::Integer(right)) => left.cmp(right),
        (Cell::Real(left), Cell::Real(right)) => left.partial_cmp(right).unwrap_or(Equal),
        (Cell::Integer(left), Cell::Real(right)) => (*left as f64).partial_cmp(right).unwrap_or(Equal),
        (Cell::Real(left), Cell::Integer(right)) => left.partial_cmp(&(*right as f64)).unwrap_or(Equal),
        (Cell::Text(left), Cell::Text(right)) => collation.compare_text(left, right),
        (Cell::Blob(left), Cell::Blob(right)) => left.cmp(right),
        _ => Equal,
    }
}

/// Applies one comparison operator. `None` means the result is SQL `NULL`.
fn compare_values(left: &Cell, right: &Cell, operator: CompareOp, collation: Collation) -> Option<bool> {
    use Ordering::{Equal, Greater, Less};
    if matches!(left, Cell::Null) || matches!(right, Cell::Null) {
        return None;
    }
    let ordering = compare_cells_with(left, right, collation);
    Some(match operator {
        CompareOp::Eq => ordering == Equal,
        CompareOp::NotEq => ordering != Equal,
        CompareOp::Lt => ordering == Less,
        CompareOp::LtEq => ordering != Greater,
        CompareOp::Gt => ordering == Greater,
        CompareOp::GtEq => ordering != Less,
    })
}

/// SQLite boolean conversion: `NULL` stays unknown, numbers are true when
/// non-zero, and text or blobs are converted numerically first (`'1'` and
/// `x'31'` are true, `'abc'` and `x'01'` are false).
fn truthiness(cell: &Cell) -> Option<bool> {
    match cell {
        Cell::Null => None,
        Cell::Integer(value) => Some(*value != 0),
        Cell::Real(value) => Some(*value != 0.0),
        Cell::Text(text) => Some(number_truthy(&text_number(text))),
        Cell::Blob(bytes) => Some(number_truthy(&text_number(&String::from_utf8_lossy(bytes)))),
    }
}

fn compare_key_rows(left: &[Cell], right: &[Cell], keys: &[(SortKey, SortDirection)]) -> Ordering {
    for (index, (_, direction)) in keys.iter().enumerate() {
        let ordering = direction.compare(&left[index], &right[index]);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// Compares two composite window sort keys, honouring each column's direction.
fn compare_ordered(left: &[Cell], right: &[Cell], directions: &[SortDirection]) -> Ordering {
    for (index, direction) in directions.iter().enumerate() {
        let ordering = direction.compare(&left[index], &right[index]);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// Numeric view of a cell used by arithmetic and the numeric functions.
#[derive(Clone, Copy)]
enum Number {
    /// 64-bit integer.
    Int(i64),
    /// Binary64 value.
    Real(f64),
}

/// SQLite numeric coercion: `NULL` has no numeric value, and text or blobs
/// contribute their longest numeric prefix (unparseable input becomes zero).
fn numeric(cell: &Cell) -> Option<Number> {
    match cell {
        Cell::Null => None,
        Cell::Integer(value) => Some(Number::Int(*value)),
        Cell::Real(value) => Some(Number::Real(*value)),
        Cell::Text(text) => Some(text_number(text)),
        // Blobs convert through their text form too, so `x'31' + 1` is 2.
        Cell::Blob(bytes) => Some(text_number(&String::from_utf8_lossy(bytes))),
    }
}

/// Numeric value of text: SQLite takes the longest numeric *prefix*
/// (`sqlite3AtoF`), so `'2abc'` is 2 and `'abc'` is 0.
fn text_number(text: &str) -> Number {
    let trimmed = text.trim_start();
    let bytes = trimmed.as_bytes();
    let mut index = 0;
    if matches!(bytes.first(), Some(b'+') | Some(b'-')) {
        index = 1;
    }
    let integer_start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    let mut has_digits = index > integer_start;
    let mut is_real = false;
    if index < bytes.len() && bytes[index] == b'.' {
        is_real = true;
        index += 1;
        let fraction_start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        has_digits |= index > fraction_start;
    }
    if !has_digits {
        return Number::Int(0);
    }
    // An exponent only counts when it is followed by at least one digit;
    // otherwise the number ends at the mantissa (`'1e'` is 1).
    let mut end = index;
    if index < bytes.len() && (bytes[index] == b'e' || bytes[index] == b'E') {
        let mut probe = index + 1;
        if probe < bytes.len() && matches!(bytes[probe], b'+' | b'-') {
            probe += 1;
        }
        let exponent_start = probe;
        while probe < bytes.len() && bytes[probe].is_ascii_digit() {
            probe += 1;
        }
        if probe > exponent_start {
            end = probe;
            is_real = true;
        }
    }
    let prefix = &trimmed[..end];
    if is_real {
        prefix.parse::<f64>().map(Number::Real).unwrap_or(Number::Int(0))
    }
    else {
        prefix.parse::<i64>().map(Number::Int).unwrap_or_else(|_| prefix.parse::<f64>().map(Number::Real).unwrap_or(Number::Int(0)))
    }
}

/// `CAST(expr AS type)`: SQLite converts between its five storage classes and
/// keeps `NULL` unchanged.
fn cast_cell(cell: &Cell, target: CastType) -> Cell {
    if matches!(cell, Cell::Null) {
        return Cell::Null;
    }
    match target {
        CastType::Integer => Cell::Integer(cast_integer(cell)),
        CastType::Real => Cell::Real(cast_real(cell)),
        CastType::Text => Cell::Text(cast_text(cell)),
        CastType::Blob => Cell::Blob(cast_text(cell).into_bytes()),
        CastType::Numeric => cast_numeric(cell),
    }
}

fn cast_integer(cell: &Cell) -> i64 {
    match cell {
        Cell::Integer(value) => *value,
        // SQLite truncates toward zero and saturates at the `i64` bounds.
        Cell::Real(value) => *value as i64,
        Cell::Text(text) => integer_prefix(text),
        // Blobs convert through their text form, so `cast(x'31' as integer)` is 1.
        Cell::Blob(bytes) => integer_prefix(&String::from_utf8_lossy(bytes)),
        Cell::Null => 0,
    }
}

/// Parses the leading integer prefix of `text`, stopping at the first
/// non-digit the way `sqlite3Atoi64` does. `'1e3'` therefore becomes `1`.
fn integer_prefix(text: &str) -> i64 {
    let trimmed = text.trim_start();
    let bytes = trimmed.as_bytes();
    let mut index = 0;
    let negative = match bytes.first() {
        Some(b'+') => {
            index = 1;
            false
        }
        Some(b'-') => {
            index = 1;
            true
        }
        _ => false,
    };
    let start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if index == start {
        return 0;
    }
    let magnitude = trimmed[start..index].parse::<i128>().unwrap_or(i128::MAX);
    let signed = if negative { -magnitude } else { magnitude };
    signed.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn cast_real(cell: &Cell) -> f64 {
    match cell {
        Cell::Integer(value) => *value as f64,
        Cell::Real(value) => *value,
        Cell::Text(text) => real_prefix(text),
        Cell::Blob(bytes) => real_prefix(&String::from_utf8_lossy(bytes)),
        Cell::Null => 0.0,
    }
}

/// Parses the leading real prefix of `text`, stopping at the first byte that
/// cannot be part of a number, the way `sqlite3AtoF` does.
fn real_prefix(text: &str) -> f64 {
    let trimmed = text.trim_start();
    let bytes = trimmed.as_bytes();
    let mut index = 0;
    if matches!(bytes.first(), Some(b'+') | Some(b'-')) {
        index = 1;
    }
    let start = index;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    let mut has_dot = false;
    if index < bytes.len() && bytes[index] == b'.' {
        has_dot = true;
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
    }
    if index == start && !has_dot {
        return 0.0;
    }
    let mut end = index;
    if index < bytes.len() && (bytes[index] == b'e' || bytes[index] == b'E') {
        let mut cursor = index + 1;
        if matches!(bytes.get(cursor), Some(b'+') | Some(b'-')) {
            cursor += 1;
        }
        let exponent_start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if cursor > exponent_start {
            end = cursor;
        }
    }
    trimmed[..end].parse::<f64>().unwrap_or(0.0)
}

fn cast_text(cell: &Cell) -> String {
    match cell {
        Cell::Null => String::new(),
        Cell::Integer(value) => value.to_string(),
        Cell::Real(value) => sqlite_real_text(*value),
        Cell::Text(text) => text.clone(),
        Cell::Blob(bytes) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

fn cast_numeric(cell: &Cell) -> Cell {
    match cell {
        Cell::Integer(value) => Cell::Integer(*value),
        Cell::Real(value) => Cell::Real(*value),
        Cell::Text(text) => numeric_text(text),
        Cell::Blob(bytes) => numeric_text(&String::from_utf8_lossy(bytes)),
        Cell::Null => Cell::Integer(0),
    }
}

/// Numeric view of text for `CAST(... AS NUMERIC)`: the same prefix rule as
/// arithmetic, collapsed back to an integer when the value is integral.
fn numeric_text(text: &str) -> Cell {
    match text_number(text) {
        Number::Int(value) => Cell::Integer(value),
        Number::Real(value) if value.is_finite() && value.fract() == 0.0 && (i64::MIN as f64..=i64::MAX as f64).contains(&value) => {
            Cell::Integer(value as i64)
        }
        Number::Real(value) => Cell::Real(value),
    }
}

/// Text form shared by `LIKE` and `||`; `NULL` has none and short-circuits.
fn text_form(cell: &Cell) -> Option<String> {
    match cell {
        Cell::Null => None,
        Cell::Integer(value) => Some(value.to_string()),
        Cell::Real(value) => Some(sqlite_real_text(*value)),
        Cell::Text(text) => Some(text.clone()),
        Cell::Blob(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
    }
}

/// `||`: concatenates the text forms of both sides. `NULL` propagates and the
/// result is always `TEXT`, even when both operands are numbers.
fn concat(left: &Cell, right: &Cell) -> Cell {
    match (text_form(left), text_form(right)) {
        (Some(left), Some(right)) => Cell::Text(format!("{left}{right}")),
        _ => Cell::Null,
    }
}

/// Renders a real the way SQLite does with `%!.15g`: 15 significant digits,
/// always a decimal point, and an exponent only outside `[1e-4, 1e15)`.
fn sqlite_real_text(value: f64) -> String {
    if value == 0.0 {
        return "0.0".to_string();
    }
    // `{:.14e}` carries 15 significant digits and always prints an exponent.
    let scientific = format!("{value:.14e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((scientific.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(|character| character.is_ascii_digit()).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    let mut text = if exponent < -4 || exponent >= 15 {
        let (head, tail) = digits.split_at(1);
        let tail = if tail.is_empty() { "0" } else { tail };
        format!("{head}.{tail}e{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.abs())
    }
    else {
        let point = exponent + 1;
        if point <= 0 {
            format!("0.{}{digits}", "0".repeat((-point) as usize))
        }
        else if point as usize >= digits.len() {
            format!("{digits}{}.0", "0".repeat(point as usize - digits.len()))
        }
        else {
            format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
        }
    };
    if negative {
        text.insert(0, '-');
    }
    text
}

/// Matches one `GLOB` pattern against a value, one character at a time.
///
/// `*` matches any run including the empty one, `?` matches exactly one
/// character, and `[...]` is a class: `^` negates it, `a-c` is an inclusive
/// range, a leading `]` is a literal member, and an unclosed `[` never matches.
/// Unlike `LIKE`, every comparison is case sensitive.
fn glob_match(value: &[char], pattern: &[char]) -> bool {
    let Some((first, rest)) = pattern.split_first()
    else {
        return value.is_empty();
    };
    match *first {
        '*' => glob_match(value, rest) || value.split_first().is_some_and(|(_, tail)| glob_match(tail, pattern)),
        '?' => value.split_first().is_some_and(|(_, tail)| glob_match(tail, rest)),
        '[' => {
            let Some((candidate, tail)) = value.split_first()
            else {
                return false;
            };
            match glob_class(*candidate, rest) {
                Some(true) => glob_match(tail, &rest[glob_class_width(rest)..]),
                _ => false,
            }
        }
        literal => value.split_first().is_some_and(|(candidate, tail)| *candidate == literal && glob_match(tail, rest)),
    }
}

/// Width of the `[...]` class that starts at `pattern[0]`, including both
/// brackets. Zero means the class is never closed.
fn glob_class_width(pattern: &[char]) -> usize {
    let mut index = usize::from(pattern.first() == Some(&'^'));
    let mut first_item = true;
    while index < pattern.len() {
        if pattern[index] == ']' && !first_item {
            return index + 1;
        }
        first_item = false;
        // `a-c` is a range only when a member follows the dash.
        if index + 2 < pattern.len() && pattern[index + 1] == '-' && pattern[index + 2] != ']' {
            index += 3;
        }
        else {
            index += 1;
        }
    }
    0
}

/// Matches one character against the class body that follows `[`.
fn glob_class(candidate: char, pattern: &[char]) -> Option<bool> {
    let width = glob_class_width(pattern);
    if width == 0 {
        return None;
    }
    let invert = pattern.first() == Some(&'^');
    let body = &pattern[usize::from(invert)..width - 1];
    let mut matched = false;
    let mut index = 0;
    let mut first_item = true;
    while index < body.len() {
        // A leading `]` is a member instead of the closing bracket.
        if body[index] == ']' && !first_item {
            break;
        }
        first_item = false;
        if index + 2 < body.len() && body[index + 1] == '-' && body[index + 2] != ']' {
            if candidate >= body[index] && candidate <= body[index + 2] {
                matched = true;
            }
            index += 3;
        }
        else {
            if candidate == body[index] {
                matched = true;
            }
            index += 1;
        }
    }
    Some(if invert { !matched } else { matched })
}

/// `LIKE` folds case only for ASCII letters; every other character compares
/// exactly, which is why `Ü` never matches `ü`.
fn like_eq(left: char, right: char) -> bool {
    if left.is_ascii() && right.is_ascii() { left.eq_ignore_ascii_case(&right) } else { left == right }
}

/// Matches one `LIKE` pattern against a value, one character at a time.
///
/// `%` matches any run including the empty one, `_` matches exactly one
/// character, and the `ESCAPE` character turns the next pattern character into
/// a literal.
fn like_match(value: &[char], pattern: &[char], escape: Option<char>) -> bool {
    let Some((first, rest)) = pattern.split_first()
    else {
        return value.is_empty();
    };
    if escape != Some(*first) && *first == '%' {
        return like_match(value, rest, escape) || value.split_first().is_some_and(|(_, tail)| like_match(tail, pattern, escape));
    }
    let literal = if escape == Some(*first) {
        // A trailing escape character has nothing left to quote.
        match rest.split_first() {
            Some((quoted, rest)) => (*quoted, rest),
            None => return false,
        }
    }
    else {
        (*first, rest)
    };
    let Some((candidate, tail)) = value.split_first()
    else {
        return false;
    };
    if escape != Some(*first) && *first == '_' {
        return like_match(tail, literal.1, escape);
    }
    like_eq(literal.0, *candidate) && like_match(tail, literal.1, escape)
}

/// One consumed pattern element.
enum LikeElement {
    /// The pattern ended.
    End,
    /// `_`: exactly one character.
    Any,
    /// A literal character, with any `ESCAPE` prefix already consumed.
    Literal(char),
}

/// 纯前缀模式的快速形态:模式是 `<prefix>%` 或 `<prefix>`,且 `prefix` 内没有
/// `%`、`_` 与转义字符。返回 `prefix` 与「是否允许更长」。
///
/// 命中时 `LIKE` 等价于一次前缀比较,不必启动逐字符匹配器。带 `ESCAPE` 的
/// 模式保持原匹配器:转义字符自身也要参与判定。
fn like_plain_prefix(pattern: &str, escape: Option<char>) -> Option<(&str, bool)> {
    if escape.is_some() {
        return None;
    }
    let (prefix, open) = match pattern.strip_suffix('%') {
        Some(prefix) => (prefix, true),
        None => (pattern, false),
    };
    if prefix.contains(['%', '_']) {
        return None;
    }
    Some((prefix, open))
}

/// `LIKE` 前缀匹配:`value` 是否以 `prefix` 开头(`open` 为假时要求整串相等),
/// ASCII 字母不区分大小写,与 [`like_eq`] 的逐字符规则一致。
///
/// UTF-8 是自同步编码,逐字节做 ASCII 折叠比较与前缀字符比较等价:多字节字符
/// 必须逐字节相同,ASCII 字节也不会出现在多字节序列内部。
fn like_prefix_text(value: &str, prefix: &str, open: bool) -> bool {
    let value = value.as_bytes();
    let prefix = prefix.as_bytes();
    let matched_length = if open { value.len() >= prefix.len() } else { value.len() == prefix.len() };
    matched_length && prefix.iter().zip(&value[..prefix.len()]).all(|(pattern, candidate)| pattern.eq_ignore_ascii_case(candidate))
}

/// Matches one `LIKE` pattern against one text value without allocating.
///
/// Same semantics as [`like_match`] — `%` matches any run including the empty
/// one, `_` exactly one character, and the `ESCAPE` character quotes the next
/// pattern character — but it walks `Chars` clones and remembers the last `%`
/// as a backtracking point instead of materializing two `Vec<char>` per call.
/// `Chars` is a pair of pointers, so cloning one is free.
fn like_match_text(value: &str, pattern: &str, escape: Option<char>) -> bool {
    let mut value: std::str::Chars<'_> = value.chars();
    let mut pattern: std::str::Chars<'_> = pattern.chars();
    // Pattern position right after the most recent `%`, plus the value
    // position that `%` started at; together they are the retry point.
    let mut retry_pattern: Option<std::str::Chars<'_>> = None;
    let mut retry_value: Option<std::str::Chars<'_>> = None;
    loop {
        let mut rest = pattern.clone();
        let element = match rest.next() {
            Some(first) if escape != Some(first) && first == '%' => {
                retry_pattern = Some(rest.clone());
                retry_value = Some(value.clone());
                pattern = rest;
                continue;
            }
            Some(first) if escape == Some(first) => match rest.next() {
                Some(quoted) => {
                    pattern = rest;
                    LikeElement::Literal(quoted)
                }
                // A trailing escape character has nothing left to quote.
                None => return false,
            },
            Some('_') => {
                pattern = rest;
                LikeElement::Any
            }
            Some(first) => {
                pattern = rest;
                LikeElement::Literal(first)
            }
            None => LikeElement::End,
        };
        let matched = match element {
            LikeElement::End => {
                if value.clone().next().is_none() {
                    return true;
                }
                // Extend the run the last `%` swallowed by one character and
                // retry the (already exhausted) pattern against the shorter tail.
                let Some(next_value) = retry_value.as_mut()
                else {
                    return false;
                };
                let Some(next_pattern) = retry_pattern.as_ref()
                else {
                    return false;
                };
                if next_value.next().is_none() {
                    return false;
                }
                value = next_value.clone();
                pattern = next_pattern.clone();
                continue;
            }
            LikeElement::Any => value.next().is_some(),
            LikeElement::Literal(literal) => matches!(value.next(), Some(candidate) if like_eq(literal, candidate)),
        };
        if matched {
            continue;
        }
        // Extend the run the last `%` swallowed by one character and retry.
        let Some(next_value) = retry_value.as_mut()
        else {
            return false;
        };
        let Some(next_pattern) = retry_pattern.as_ref()
        else {
            return false;
        };
        if next_value.next().is_none() {
            return false;
        }
        value = next_value.clone();
        pattern = next_pattern.clone();
    }
}

fn number_truthy(number: &Number) -> bool {
    match number {
        Number::Int(value) => *value != 0,
        Number::Real(value) => *value != 0.0,
    }
}

fn number_f64(number: Number) -> f64 {
    match number {
        Number::Int(value) => value as f64,
        Number::Real(value) => value,
    }
}

/// Unary negation: integer negation that overflows becomes a real, and `NULL`
/// stays `NULL`. Text and blobs are converted numerically first, as in SQLite.
fn negate_cell(cell: &Cell) -> Cell {
    match numeric(cell) {
        None => Cell::Null,
        Some(Number::Int(value)) => match value.checked_neg() {
            Some(value) => Cell::Integer(value),
            None => Cell::Real(-(value as f64)),
        },
        Some(Number::Real(value)) => Cell::Real(-value),
    }
}

/// Arithmetic with SQLite rules: `NULL` propagates, integer operators truncate
/// and fall back to a real on overflow, and division by zero yields `NULL`.
fn arithmetic(op: BinaryOp, left: &Cell, right: &Cell) -> Cell {
    let (Some(left), Some(right)) = (numeric(left), numeric(right))
    else {
        return Cell::Null;
    };
    match op {
        BinaryOp::Add => match (left, right) {
            (Number::Int(left), Number::Int(right)) => match left.checked_add(right) {
                Some(value) => Cell::Integer(value),
                None => Cell::Real(left as f64 + right as f64),
            },
            (left, right) => Cell::Real(number_f64(left) + number_f64(right)),
        },
        BinaryOp::Subtract => match (left, right) {
            (Number::Int(left), Number::Int(right)) => match left.checked_sub(right) {
                Some(value) => Cell::Integer(value),
                None => Cell::Real(left as f64 - right as f64),
            },
            (left, right) => Cell::Real(number_f64(left) - number_f64(right)),
        },
        BinaryOp::Multiply => match (left, right) {
            (Number::Int(left), Number::Int(right)) => match left.checked_mul(right) {
                Some(value) => Cell::Integer(value),
                None => Cell::Real(left as f64 * right as f64),
            },
            (left, right) => Cell::Real(number_f64(left) * number_f64(right)),
        },
        BinaryOp::Divide => match (left, right) {
            // SQLite returns NULL rather than infinity for any zero divisor.
            (_, Number::Int(0)) | (_, Number::Real(0.0)) => Cell::Null,
            (Number::Int(left), Number::Int(right)) => match left.checked_div(right) {
                Some(value) => Cell::Integer(value),
                None => Cell::Real(left as f64 / right as f64),
            },
            (left, right) => Cell::Real(number_f64(left) / number_f64(right)),
        },
        // `%` converts both operands to 64-bit integers, keeps the dividend's
        // sign, returns a real when either operand was real, and yields NULL
        // when the truncated divisor is zero.
        BinaryOp::Modulo => {
            let real = matches!(&left, Number::Real(_)) || matches!(&right, Number::Real(_));
            let (left, right) = (number_i64(left), number_i64(right));
            if right == 0 {
                Cell::Null
            }
            else {
                // `i64::MIN % -1` overflows, and SQLite answers `0` for it.
                let remainder = left.checked_rem(right).unwrap_or(0);
                if real { Cell::Real(remainder as f64) } else { Cell::Integer(remainder) }
            }
        }
        // `||` never reaches arithmetic: it is evaluated by its own branch.
        BinaryOp::Concat => Cell::Null,
        // `->` / `->>` 同样由独立分支求值，这里只是穷尽匹配。
        BinaryOp::Arrow | BinaryOp::ArrowText => Cell::Null,
        BinaryOp::ShiftLeft => Cell::Integer(shift(number_i64(left), number_i64(right), true)),
        BinaryOp::ShiftRight => Cell::Integer(shift(number_i64(left), number_i64(right), false)),
        BinaryOp::BitAnd => Cell::Integer(number_i64(left) & number_i64(right)),
        BinaryOp::BitOr => Cell::Integer(number_i64(left) | number_i64(right)),
    }
}

/// `~`: one's complement of the 64-bit integer conversion, `NULL` stays `NULL`.
fn bit_not(cell: &Cell) -> Cell {
    match numeric(cell) {
        Some(number) => Cell::Integer(!number_i64(number)),
        None => Cell::Null,
    }
}

fn number_i64(number: Number) -> i64 {
    match number {
        Number::Int(value) => value,
        Number::Real(value) => value as i64,
    }
}

/// SQLite shifts 64-bit integers: a negative count reverses the direction,
/// counts of 64 or more saturate, and `>>` keeps the sign bit.
fn shift(value: i64, count: i64, left: bool) -> i64 {
    let magnitude = count.unsigned_abs();
    let leftward = left != (count < 0);
    if leftward {
        if magnitude >= 64 { 0 } else { ((value as u64) << magnitude) as i64 }
    }
    else if magnitude >= 64 {
        if value < 0 { -1 } else { 0 }
    }
    else {
        value >> magnitude
    }
}

/// `IN` with three-valued logic: a hit is true (false for `NOT IN`), a miss
/// with any `NULL` candidate is unknown, and a `NULL` probe is always unknown.
fn in_result(value: &Cell, list: &[Cell], negated: bool) -> Cell {
    if matches!(value, Cell::Null) {
        return Cell::Null;
    }
    let mut saw_null = false;
    for item in list {
        if matches!(item, Cell::Null) {
            saw_null = true;
        }
        else if compare_values(value, item, CompareOp::Eq, Collation::Binary) == Some(true) {
            return Cell::Integer(i64::from(!negated));
        }
    }
    if saw_null { Cell::Null } else { Cell::Integer(i64::from(negated)) }
}

/// Canonical key set for an `IN (SELECT ...)` value list, or `None` when an
/// item has no canonical signature, so membership has to scan the list.
fn in_list_keys(values: &[Cell]) -> Option<HashSet<ProbeKey>> {
    let mut keys = HashSet::with_capacity(values.len());
    for value in values {
        match probe_key(value, Collation::Binary) {
            Some(key) if !matches!(key, ProbeKey::Wide(_)) => {
                keys.insert(key);
            }
            _ => return None,
        }
    }
    Some(keys)
}

/// Membership with the same three-valued result as [`in_result`], using the
/// canonical key set when the whole list had one.
fn in_result_cached(value: &Cell, list: &[Cell], keys: Option<&HashSet<ProbeKey>>, saw_null: bool, negated: bool) -> Cell {
    if matches!(value, Cell::Null) {
        return Cell::Null;
    }
    if let Some(keys) = keys
        && let Some(key) = probe_key(value, Collation::Binary)
        && !matches!(key, ProbeKey::Wide(_))
    {
        return if keys.contains(&key) {
            Cell::Integer(i64::from(!negated))
        }
        else if saw_null {
            Cell::Null
        }
        else {
            Cell::Integer(i64::from(negated))
        };
    }
    in_result(value, list, negated)
}

/// Canonical hash signature of one output row, or `None` when a cell falls
/// outside the canonical form and dedup must fall back to the linear scan.
fn row_signature(row: &[Cell], collations: &[Collation]) -> Option<Vec<ProbeKey>> {
    row.iter()
        .enumerate()
        .map(|(position, cell)| match probe_key(cell, collations.get(position).copied().unwrap_or_default()) {
            Some(part) if !matches!(part, ProbeKey::Wide(_)) => Some(part),
            _ => None,
        })
        .collect()
}

/// Row equality under the engine's comparison rules and the given collations.
fn rows_equal_with(left: &[Cell], right: &[Cell], collations: &[Collation]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).enumerate().all(|(position, (first, second))| {
            compare_cells_with(first, second, collations.get(position).copied().unwrap_or_default()) == Ordering::Equal
        })
}

fn eval_function(name: ScalarFunction, args: &[Cell]) -> Cell {
    match name {
        // `abs()` 对整数保持整数，对实数与文本一律走二进制浮点，所以
        // `typeof(abs('-7'))` 是 `real` 而不是 `integer`。
        ScalarFunction::Abs => match &args[0] {
            Cell::Null => Cell::Null,
            Cell::Integer(value) => match value.checked_abs() {
                Some(value) => Cell::Integer(value),
                None => Cell::Real(-(*value as f64)),
            },
            cell => match numeric(cell) {
                Some(Number::Int(value)) => Cell::Real((value as f64).abs()),
                Some(Number::Real(value)) => Cell::Real(value.abs()),
                None => Cell::Null,
            },
        },
        ScalarFunction::Coalesce => args.iter().find(|cell| !matches!(cell, Cell::Null)).cloned().unwrap_or(Cell::Null),
        ScalarFunction::Length => match &args[0] {
            Cell::Null => Cell::Null,
            Cell::Blob(bytes) => Cell::Integer(bytes.len() as i64),
            Cell::Text(text) => Cell::Integer(text.chars().count() as i64),
            other => Cell::Integer(number_text(other).chars().count() as i64),
        },
        ScalarFunction::Typeof => Cell::Text(
            match &args[0] {
                Cell::Null => "null",
                Cell::Integer(_) => "integer",
                Cell::Real(_) => "real",
                Cell::Text(_) => "text",
                Cell::Blob(_) => "blob",
            }
            .to_string(),
        ),
        ScalarFunction::Lower => match &args[0] {
            Cell::Text(text) => Cell::Text(text.to_ascii_lowercase()),
            other => other.clone(),
        },
        ScalarFunction::Upper => match &args[0] {
            Cell::Text(text) => Cell::Text(text.to_ascii_uppercase()),
            other => other.clone(),
        },
        ScalarFunction::Substr => substr_cell(args),
        ScalarFunction::Round => round_cell(args),
        // `hex()` hashes the text form, so `hex(255)` is `323535` and a blob
        // keeps its bytes; `NULL` becomes the empty string.
        ScalarFunction::Hex => Cell::Text(match text_form(&args[0]) {
            Some(text) => text.bytes().map(|byte| format!("{byte:02X}")).collect(),
            None => String::new(),
        }),
        ScalarFunction::Quote => Cell::Text(quote_text(&args[0])),
        ScalarFunction::Random => Cell::Integer(next_random()),
        ScalarFunction::RandomBlob => Cell::Blob(random_blob(&args[0])),
        ScalarFunction::Instr => instr_cell(args),
        ScalarFunction::Trim => trim_cell(args, true, true),
        ScalarFunction::Ltrim => trim_cell(args, true, false),
        ScalarFunction::Rtrim => trim_cell(args, false, true),
        ScalarFunction::Replace => replace_cell(args),
        ScalarFunction::IfNull => match &args[0] {
            Cell::Null => args[1].clone(),
            value => value.clone(),
        },
        // `nullif(x, y)` 与 `x = y` 共用比较语义，比较结果未知时返回 `x`。
        ScalarFunction::NullIf => match compare_values(&args[0], &args[1], CompareOp::Eq, Collation::Binary) {
            Some(true) => Cell::Null,
            _ => args[0].clone(),
        },
        ScalarFunction::Printf => printf_cell(args),
        ScalarFunction::Char => Cell::Text(args.iter().map(|cell| character_for_code(integer_argument(cell))).collect()),
        ScalarFunction::Unicode => match &args[0] {
            Cell::Null => Cell::Null,
            cell => match cast_text(cell).chars().next() {
                Some(character) => Cell::Integer(character as i64),
                None => Cell::Null,
            },
        },
        ScalarFunction::Soundex => soundex_cell(&args[0]),
        ScalarFunction::Min => min_max_cell(args, false),
        ScalarFunction::Max => min_max_cell(args, true),
        ScalarFunction::Ceil => integral_or_real(args, f64::ceil),
        ScalarFunction::Floor => integral_or_real(args, f64::floor),
        ScalarFunction::Trunc => integral_or_real(args, f64::trunc),
        ScalarFunction::Sqrt => real_function(args, f64::sqrt),
        ScalarFunction::Pow => real_function2(args, f64::powf),
        ScalarFunction::Mod => real_function2(args, |left, right| left % right),
        ScalarFunction::Exp => real_function(args, f64::exp),
        ScalarFunction::Ln => positive_real_function(args, f64::ln),
        ScalarFunction::Log => log_cell(args),
        ScalarFunction::Log2 => positive_real_function(args, f64::log2),
        ScalarFunction::Log10 => positive_real_function(args, f64::log10),
        ScalarFunction::Sin => real_function(args, f64::sin),
        ScalarFunction::Cos => real_function(args, f64::cos),
        ScalarFunction::Tan => real_function(args, f64::tan),
        ScalarFunction::Asin => real_function(args, f64::asin),
        ScalarFunction::Acos => real_function(args, f64::acos),
        ScalarFunction::Atan => real_function(args, f64::atan),
        ScalarFunction::Atan2 => real_function2(args, f64::atan2),
        ScalarFunction::Sinh => real_function(args, f64::sinh),
        ScalarFunction::Cosh => real_function(args, f64::cosh),
        ScalarFunction::Tanh => real_function(args, f64::tanh),
        ScalarFunction::Asinh => real_function(args, f64::asinh),
        ScalarFunction::Acosh => real_function(args, f64::acosh),
        ScalarFunction::Atanh => real_function(args, f64::atanh),
        ScalarFunction::Degrees => real_function(args, f64::to_degrees),
        ScalarFunction::Radians => real_function(args, f64::to_radians),
        ScalarFunction::Pi => Cell::Real(std::f64::consts::PI),
        ScalarFunction::Sign => sign_cell(args),
        ScalarFunction::Date => datetime::eval(DateTimeFunction::Date, args),
        ScalarFunction::Time => datetime::eval(DateTimeFunction::Time, args),
        ScalarFunction::Datetime => datetime::eval(DateTimeFunction::Datetime, args),
        ScalarFunction::JulianDay => datetime::eval(DateTimeFunction::JulianDay, args),
        ScalarFunction::UnixEpoch => datetime::eval(DateTimeFunction::UnixEpoch, args),
        ScalarFunction::Strftime => datetime::eval(DateTimeFunction::Strftime, args),
        ScalarFunction::Timediff => datetime::eval(DateTimeFunction::Timediff, args),
        // JSON 函数在进入本函数前已交给 JSON 模块，这里只为穷尽匹配。
        ScalarFunction::JsonValid
        | ScalarFunction::JsonType
        | ScalarFunction::JsonArray
        | ScalarFunction::JsonObject
        | ScalarFunction::JsonQuote
        | ScalarFunction::JsonExtract
        | ScalarFunction::JsonArrayLength
        | ScalarFunction::JsonRemove
        | ScalarFunction::JsonSet
        | ScalarFunction::JsonInsert
        | ScalarFunction::JsonReplace
        | ScalarFunction::JsonPatch => Cell::Null,
    }
}

/// 多参数 `min` / `max`：任一参数为 `NULL` 时返回 `NULL`。`min` 在相等时
/// 取后一个参数，`max` 在相等时保留前一个参数，与 SQLite 的比较循环一致。
fn min_max_cell(args: &[Cell], want_max: bool) -> Cell {
    if args.iter().any(|cell| matches!(cell, Cell::Null)) {
        return Cell::Null;
    }
    let mut result = &args[0];
    for candidate in &args[1..] {
        let replace = if want_max {
            compare_values(result, candidate, CompareOp::Lt, Collation::Binary) == Some(true)
        }
        else {
            compare_values(result, candidate, CompareOp::GtEq, Collation::Binary) == Some(true)
        };
        if replace {
            result = candidate;
        }
    }
    result.clone()
}

fn character_for_code(code: i64) -> char {
    u32::try_from(code).ok().and_then(char::from_u32).unwrap_or('\u{FFFD}')
}

/// `soundex`：首字母大写后按 SQLite 的分组追加数字，最多四位，不足补 `0`。
fn soundex_cell(cell: &Cell) -> Cell {
    let text = cast_text(cell);
    let mut letters = text.chars().filter(|character| character.is_ascii_alphabetic());
    let Some(first) = letters.next()
    else {
        return Cell::Text("?000".to_string());
    };
    let mut encoded = String::new();
    encoded.push(first.to_ascii_uppercase());
    let mut previous = soundex_code(first);
    for letter in letters {
        let code = soundex_code(letter);
        if code == 0 {
            previous = 0;
            continue;
        }
        if code != previous {
            encoded.push((b'0' + code) as char);
            if encoded.len() == 4 {
                break;
            }
        }
        previous = code;
    }
    while encoded.len() < 4 {
        encoded.push('0');
    }
    Cell::Text(encoded)
}

fn soundex_code(character: char) -> u8 {
    match character.to_ascii_uppercase() {
        'B' | 'F' | 'P' | 'V' => 1,
        'C' | 'G' | 'J' | 'K' | 'Q' | 'S' | 'X' | 'Z' => 2,
        'D' | 'T' => 3,
        'L' => 4,
        'M' | 'N' => 5,
        'R' => 6,
        _ => 0,
    }
}

fn integer_argument(cell: &Cell) -> i64 {
    match cast_numeric(cell) {
        Cell::Integer(value) => value,
        Cell::Real(value) => value as i64,
        _ => 0,
    }
}

fn real_argument(cell: &Cell) -> f64 {
    match cast_numeric(cell) {
        Cell::Integer(value) => value as f64,
        Cell::Real(value) => value,
        _ => 0.0,
    }
}

/// `printf` / `format`：`NULL` 格式串返回 `NULL`，位置参数写法同样返回 `NULL`。
fn printf_cell(args: &[Cell]) -> Cell {
    let Some(format) = text_form(&args[0])
    else {
        return Cell::Null;
    };
    if format.contains('$') {
        return Cell::Null;
    }
    let characters: Vec<char> = format.chars().collect();
    let mut output = String::new();
    let mut index = 0usize;
    let mut argument = 1usize;
    while index < characters.len() {
        if characters[index] != '%' {
            output.push(characters[index]);
            index += 1;
            continue;
        }
        index += 1;
        if index >= characters.len() {
            output.push('%');
            break;
        }
        if characters[index] == '%' {
            output.push('%');
            index += 1;
            continue;
        }
        let mut flags = String::new();
        while index < characters.len() && "-+ 0#".contains(characters[index]) {
            flags.push(characters[index]);
            index += 1;
        }
        let mut width = 0usize;
        while index < characters.len() && characters[index].is_ascii_digit() {
            width = width.saturating_mul(10).saturating_add((characters[index] as u8 - b'0') as usize);
            index += 1;
        }
        let mut precision = None;
        if index < characters.len() && characters[index] == '.' {
            index += 1;
            let mut digits = 0usize;
            while index < characters.len() && characters[index].is_ascii_digit() {
                digits = digits.saturating_mul(10).saturating_add((characters[index] as u8 - b'0') as usize);
                index += 1;
            }
            precision = Some(digits);
        }
        while index < characters.len() && "lhL".contains(characters[index]) {
            index += 1;
        }
        if index >= characters.len() {
            output.push('%');
            break;
        }
        let conversion = characters[index];
        index += 1;
        // `%n` 不消费参数也不产出字符，解析继续。
        if conversion == 'n' {
            continue;
        }
        if !"diufFeEgGxXocszqQwp".contains(conversion) {
            // 未知转换终止解析：已有输出原样返回，否则整体为 NULL。
            return if output.is_empty() { Cell::Null } else { Cell::Text(output) };
        }
        let cell = args.get(argument).cloned().unwrap_or(Cell::Null);
        argument += 1;
        let rendered = render_conversion(conversion, &cell, &flags, width, precision);
        output.push_str(&rendered);
    }
    Cell::Text(output)
}

fn render_conversion(conversion: char, cell: &Cell, flags: &str, width: usize, precision: Option<usize>) -> String {
    let left = flags.contains('-');
    let zero = flags.contains('0') && !left;
    let plus = flags.contains('+');
    let space = flags.contains(' ');
    let alternate = flags.contains('#');
    match conversion {
        'd' | 'i' => {
            let value = integer_argument(cell);
            let digits = format!("{}", value.unsigned_abs());
            let digits = apply_integer_precision(digits, precision);
            let sign = if value < 0 {
                "-"
            }
            else if plus {
                "+"
            }
            else if space {
                " "
            }
            else {
                ""
            };
            pad_text(&format!("{sign}{digits}"), width, left, zero, sign.len())
        }
        'u' => {
            let value = integer_argument(cell) as u64;
            let digits = apply_integer_precision(value.to_string(), precision);
            pad_text(&digits, width, left, zero, 0)
        }
        'x' | 'X' => {
            let digits = format!("{:x}", integer_argument(cell) as u64);
            let digits = apply_integer_precision(digits, precision);
            let digits = if conversion == 'X' { digits.to_ascii_uppercase() } else { digits };
            let prefix = if alternate && integer_argument(cell) != 0 { if conversion == 'X' { "0X" } else { "0x" } } else { "" };
            pad_text(&format!("{prefix}{digits}"), width, left, zero, prefix.len())
        }
        'o' => {
            let digits = format!("{:o}", integer_argument(cell) as u64);
            let digits = apply_integer_precision(digits, precision);
            let prefix = if alternate && !digits.starts_with('0') { "0" } else { "" };
            pad_text(&format!("{prefix}{digits}"), width, left, zero, prefix.len())
        }
        'f' | 'F' => {
            let value = real_argument(cell);
            let keep = precision.unwrap_or(6);
            let (integer, fraction) = fixed_parts(value.abs(), keep);
            let mut body = integer;
            if keep > 0 {
                body.push('.');
                body.push_str(&fraction);
            }
            else if alternate {
                body.push('.');
            }
            let sign = if value.is_sign_negative() || value < 0.0 {
                "-"
            }
            else if plus {
                "+"
            }
            else if space {
                " "
            }
            else {
                ""
            };
            pad_text(&format!("{sign}{body}"), width, left, zero, sign.len())
        }
        'e' | 'E' => {
            let value = real_argument(cell);
            let keep = precision.unwrap_or(6);
            let (integer, fraction, exponent) = scientific_parts(value.abs(), keep);
            let letter = if conversion == 'E' { 'E' } else { 'e' };
            let sign = if value.is_sign_negative() || value < 0.0 {
                "-"
            }
            else if plus {
                "+"
            }
            else if space {
                " "
            }
            else {
                ""
            };
            let body = if fraction.is_empty() {
                format!("{integer}{letter}{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.unsigned_abs())
            }
            else {
                format!("{integer}.{fraction}{letter}{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.unsigned_abs())
            };
            pad_text(&format!("{sign}{body}"), width, left, zero, sign.len())
        }
        'g' | 'G' => {
            let value = real_argument(cell);
            let significant = precision.unwrap_or(6).max(1);
            let (integer, fraction, exponent) = scientific_parts(value.abs(), significant - 1);
            let mut body = if exponent < -4 || exponent >= significant as i32 {
                let fraction = trim_decimal_zeros(&fraction, alternate);
                let letter = if conversion == 'G' { 'E' } else { 'e' };
                let mantissa = if fraction.is_empty() { integer } else { format!("{integer}.{fraction}") };
                format!("{mantissa}{letter}{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.unsigned_abs())
            }
            else {
                let keep = (significant as i32 - 1 - exponent).max(0) as usize;
                let (integer, fraction) = fixed_parts(value.abs(), keep);
                let fraction = trim_decimal_zeros(&fraction, alternate);
                if fraction.is_empty() { if alternate { format!("{integer}.") } else { integer } } else { format!("{integer}.{fraction}") }
            };
            if conversion == 'G' {
                body = body.to_ascii_uppercase();
            }
            let sign = if value.is_sign_negative() || value < 0.0 {
                "-"
            }
            else if plus {
                "+"
            }
            else if space {
                " "
            }
            else {
                ""
            };
            pad_text(&format!("{sign}{body}"), width, left, zero, sign.len())
        }
        'c' => {
            let first: String = cast_text(cell).chars().next().map(|character| character.to_string()).unwrap_or_else(|| "\u{0000}".to_string());
            // 精度对 `%c` 表示重复次数，零与一都只输出一个字符。
            let body = first.repeat(precision.unwrap_or(1).max(1));
            // 字符串类转换忽略 `0` 标志，补位一律用空格。
            pad_text(&body, width, left, false, 0)
        }
        's' | 'z' => {
            let body = match (conversion, cell) {
                ('z', Cell::Null) => String::new(),
                _ => cast_text(cell),
            };
            let body = truncate_characters(body, precision);
            pad_text(&body, width, left, false, 0)
        }
        'q' | 'w' => {
            let body = match cell {
                Cell::Null => "(NULL)".to_string(),
                _ => {
                    let text = truncate_characters(cast_text(cell), precision);
                    if conversion == 'q' { text.replace('\'', "''") } else { text }
                }
            };
            pad_text(&body, width, left, false, 0)
        }
        'Q' => {
            let body = match cell {
                Cell::Null => "NULL".to_string(),
                _ => {
                    let text = truncate_characters(cast_text(cell), precision);
                    format!("'{}'", text.replace('\'', "''"))
                }
            };
            pad_text(&body, width, left, false, 0)
        }
        'p' => {
            let digits = format!("{:X}", integer_argument(cell) as u64);
            let digits = apply_integer_precision(digits, precision);
            let prefix = if alternate && integer_argument(cell) != 0 { "0x" } else { "" };
            pad_text(&format!("{prefix}{digits}"), width, left, zero, prefix.len())
        }
        // SQLite 对未知转换按文本渲染，这里保持同样的宽松行为。
        _ => pad_text(&cast_text(cell), width, left, zero, 0),
    }
}

fn apply_integer_precision(digits: String, precision: Option<usize>) -> String {
    match precision {
        Some(minimum) if digits.len() < minimum => format!("{}{digits}", "0".repeat(minimum - digits.len())),
        _ => digits,
    }
}

fn truncate_characters(text: String, precision: Option<usize>) -> String {
    match precision {
        Some(limit) => text.chars().take(limit).collect(),
        None => text,
    }
}

fn pad_text(body: &str, width: usize, left: bool, zero: bool, prefix_len: usize) -> String {
    if left {
        let count = body.chars().count();
        if count >= width {
            return body.to_string();
        }
        return format!("{body}{}", " ".repeat(width - count));
    }
    if zero {
        let prefix: String = body.chars().take(prefix_len).collect();
        let rest: String = body.chars().skip(prefix_len).collect();
        // 符号计入宽度，`0x` / 八进制前导 `0` 一类前缀不计入宽度。
        let sign_like = prefix.starts_with('-') || prefix.starts_with('+') || prefix.starts_with(' ');
        let measured = if sign_like { body.chars().count() } else { rest.chars().count() };
        if measured >= width {
            return body.to_string();
        }
        return format!("{prefix}{}{rest}", "0".repeat(width - measured));
    }
    let count = body.chars().count();
    if count >= width {
        return body.to_string();
    }
    format!("{}{body}", " ".repeat(width - count))
}

fn trim_decimal_zeros(fraction: &str, keep_zero: bool) -> String {
    if keep_zero {
        return fraction.to_string();
    }
    fraction.trim_end_matches('0').to_string()
}

/// 把 `value` 按十进制四舍五入到 `keep` 位小数，返回整数段与小数段。
fn fixed_parts(value: f64, keep: usize) -> (String, String) {
    let probe = format!("{value:.precision$}", precision = keep + 20);
    let (probe_integer, probe_fraction) = probe.split_once('.').unwrap_or((probe.as_str(), ""));
    let tail: String = probe_fraction.chars().skip(keep).collect();
    if tail.starts_with('5') && tail.chars().skip(1).all(|character| character == '0') {
        let mut digits: String = probe_integer.chars().chain(probe_fraction.chars().take(keep)).collect();
        digits = increment_decimal(&digits);
        let split = digits.len() - keep;
        return (digits[..split].to_string(), digits[split..].to_string());
    }
    let rounded = format!("{value:.precision$}", precision = keep);
    match rounded.split_once('.') {
        Some((integer, fraction)) => (integer.to_string(), fraction.to_string()),
        None => (rounded, String::new()),
    }
}

/// 把 `value` 规格化为 `整数位.小数位 e 指数`，尾数按十进制四舍五入。
fn scientific_parts(value: f64, keep: usize) -> (String, String, i32) {
    if value == 0.0 {
        return ("0".to_string(), "0".repeat(keep), 0);
    }
    let probe = format!("{value:.precision$e}", precision = keep + 20);
    let (probe_mantissa, probe_exponent) = probe.split_once('e').unwrap_or((probe.as_str(), "0"));
    let probe_digits: String = probe_mantissa.chars().filter(|character| character.is_ascii_digit()).collect();
    let tail: String = probe_digits.chars().skip(keep + 1).collect();
    let exponent: i32 = probe_exponent.parse().unwrap_or(0);
    let digits = if tail.starts_with('5') && tail.chars().skip(1).all(|character| character == '0') {
        increment_decimal(&probe_digits.chars().take(keep + 1).collect::<String>())
    }
    else {
        let rounded = format!("{value:.precision$e}", precision = keep);
        rounded
            .split_once('e')
            .map(|(mantissa, _)| mantissa.chars().filter(|character| character.is_ascii_digit()).collect())
            .unwrap_or_else(|| "0".to_string())
    };
    if digits.len() > keep + 1 {
        let digit = digits.chars().take(keep + 1).collect::<String>();
        return (digit[..1].to_string(), digit[1..].to_string(), exponent + 1);
    }
    (digits[..1].to_string(), digits[1..].to_string(), exponent)
}

/// `trim` 家族：按字符集去除两端字符，第二个参数缺省时只去除空格。
fn trim_cell(args: &[Cell], left: bool, right: bool) -> Cell {
    let Some(text) = text_form(&args[0])
    else {
        return Cell::Null;
    };
    let set = match args.get(1) {
        Some(cell) => match text_form(cell) {
            Some(set) => set,
            None => return Cell::Null,
        },
        None => " ".to_string(),
    };
    if set.is_empty() {
        return Cell::Text(text);
    }
    let characters: Vec<char> = text.chars().collect();
    let mut start = 0;
    let mut end = characters.len();
    if left {
        while start < end && set.chars().any(|candidate| candidate == characters[start]) {
            start += 1;
        }
    }
    if right {
        while end > start && set.chars().any(|candidate| candidate == characters[end - 1]) {
            end -= 1;
        }
    }
    Cell::Text(characters[start..end].iter().collect())
}

/// `replace(x, y, z)`：空 `y` 原样返回 `x`，任一参数为 `NULL` 时返回 `NULL`。
fn replace_cell(args: &[Cell]) -> Cell {
    let (Some(text), Some(pattern), Some(replacement)) = (text_form(&args[0]), text_form(&args[1]), text_form(&args[2]))
    else {
        return Cell::Null;
    };
    if pattern.is_empty() {
        return Cell::Text(text);
    }
    Cell::Text(text.replace(&pattern, &replacement))
}

/// `instr(x, y)`: the one-based character position of the first `y` inside
/// `x`, or zero when `y` does not occur. Either `NULL` propagates.
fn instr_cell(args: &[Cell]) -> Cell {
    match (text_form(&args[0]), text_form(&args[1])) {
        (Some(haystack), Some(needle)) => match haystack.find(&needle) {
            Some(byte) => Cell::Integer(haystack[..byte].chars().count() as i64 + 1),
            None => Cell::Integer(0),
        },
        _ => Cell::Null,
    }
}

/// `quote()` renders one SQL literal: text is single-quoted with doubled
/// quotes, blobs become `X'..'`, `NULL` becomes the word `NULL`, and reals use
/// the 15-digit form when it round-trips.
fn quote_text(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Integer(value) => value.to_string(),
        Cell::Real(value) => quote_real(*value),
        Cell::Text(text) => format!("'{}'", text.replace('\'', "''")),
        Cell::Blob(bytes) => {
            let digits: String = bytes.iter().map(|byte| format!("{byte:02X}")).collect();
            format!("X'{digits}'")
        }
    }
}

fn quote_real(value: f64) -> String {
    let text = sqlite_real_text(value);
    if text.parse::<f64>() == Ok(value) {
        return text;
    }
    // SQLite falls back to 18 fractional digits when the short form loses
    // precision, with a sign and at least two exponent digits.
    let formatted = format!("{value:.18e}");
    match formatted.split_once('e') {
        Some((mantissa, exponent)) => {
            let exponent: i32 = exponent.parse().unwrap_or(0);
            format!("{mantissa}e{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.abs())
        }
        None => formatted,
    }
}

/// `random()` returns any 64-bit integer, so a counter-driven xorshift is
/// enough: tests only check the type and range.
fn next_random() -> i64 {
    use std::{
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut state = STATE.load(Ordering::Relaxed);
    if state == 0 {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos() as u64);
        state = nanos ^ 0x9e37_79b9_7f4a_7c15;
    }
    // xorshift64*: cheap, deterministic and good enough for a random value.
    let mut next = state ^ (state >> 12);
    next ^= next << 25;
    next ^= next >> 27;
    STATE.store(next, Ordering::Relaxed);
    (next.wrapping_mul(0x2545_f491_4f6c_dd1d)) as i64
}

/// `randomblob(n)` yields `n` random bytes; `n` below one means a single byte,
/// which is also what a `NULL` argument produces.
fn random_blob(size: &Cell) -> Vec<u8> {
    let count = match numeric(size) {
        Some(number) => number_i64(number).max(1) as usize,
        None => 1,
    };
    let mut bytes = Vec::with_capacity(count);
    while bytes.len() < count {
        let mut value = next_random();
        for _ in 0..8 {
            if bytes.len() == count {
                break;
            }
            bytes.push(value as u8);
            value >>= 8;
        }
    }
    bytes
}

/// Text form used by `length()` for numeric input.
fn number_text(cell: &Cell) -> String {
    match cell {
        Cell::Integer(value) => value.to_string(),
        Cell::Real(value) => {
            if value.is_finite() && *value == value.trunc() && value.abs() < 1e15 {
                format!("{value:.1}")
            }
            else {
                value.to_string()
            }
        }
        Cell::Text(text) => text.clone(),
        Cell::Blob(_) => String::new(),
        Cell::Null => String::new(),
    }
}

fn substr_cell(args: &[Cell]) -> Cell {
    let start = match numeric(&args[1]) {
        Some(number) => match number {
            Number::Int(value) => value,
            Number::Real(value) => value as i64,
        },
        None => return Cell::Null,
    };
    let count = match args.get(2) {
        None => None,
        Some(cell) => match numeric(cell) {
            Some(Number::Int(value)) => Some(value),
            Some(Number::Real(value)) => Some(value as i64),
            None => return Cell::Null,
        },
    };
    match &args[0] {
        Cell::Null => Cell::Null,
        Cell::Blob(bytes) => {
            let (begin, end) = substr_bounds(bytes.len(), start, count);
            Cell::Blob(bytes[begin..end].to_vec())
        }
        _ => {
            let text = match &args[0] {
                Cell::Text(text) => text.clone(),
                other => number_text(other),
            };
            let chars = text.chars().collect::<Vec<_>>();
            let (begin, end) = substr_bounds(chars.len(), start, count);
            Cell::Text(chars[begin..end].iter().collect())
        }
    }
}

/// SQLite `substr` bounds: positive start is 1-based, non-positive start counts
/// from the right, and a negative length runs backwards from the start point.
fn substr_bounds(length: usize, start: i64, count: Option<i64>) -> (usize, usize) {
    let length = length as i64;
    let begin = if start > 0 {
        start - 1
    }
    else if start == 0 {
        -1
    }
    else {
        length + start
    };
    let (begin, end) = match count {
        None => (begin, length),
        Some(count) if count >= 0 => (begin, begin + count),
        Some(count) => (begin + count, begin),
    };
    (begin.clamp(0, length) as usize, end.clamp(0, length) as usize)
}

fn round_cell(args: &[Cell]) -> Cell {
    let value = match numeric(&args[0]) {
        Some(Number::Int(value)) => value as f64,
        Some(Number::Real(value)) => value,
        None => return Cell::Null,
    };
    let digits = match args.get(1) {
        None => 0,
        Some(cell) => match numeric(cell) {
            Some(Number::Int(value)) => value,
            Some(Number::Real(value)) => value.trunc() as i64,
            None => return Cell::Null,
        },
    };
    Cell::Real(round_digits(value, digits))
}

/// SQLite's `round()`: the digit count is clamped to `0..=30`, a zero count
/// truncates `value ± 0.5` toward zero, and every other count re-reads the
/// fixed-point text its `printf` produces, which rounds an exact decimal tie
/// away from zero.
fn round_digits(value: f64, digits: i64) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let digits = digits.clamp(0, 30) as usize;
    if digits == 0 {
        return if value < 0.0 { (value - 0.5).trunc() } else { (value + 0.5).trunc() };
    }
    let magnitude = value.abs();
    let short = format!("{magnitude:.digits$}");
    // A tie is a `5` directly after the kept digits with nothing but zeros
    // behind it; Rust's own formatting rounds that tie to even, SQLite rounds
    // it away from zero.
    let wide = format!("{:.*}", digits + 25, magnitude);
    let fraction = wide.split_once('.').map(|(_, fraction)| fraction).unwrap_or("");
    let tie = fraction.len() > digits && fraction.as_bytes()[digits] == b'5' && fraction[digits + 1..].bytes().all(|byte| byte == b'0');
    let text = if tie { increment_decimal(&short) } else { short };
    let rounded: f64 = text.parse().unwrap_or(magnitude);
    if value < 0.0 { -rounded } else { rounded }
}

/// 数学函数的取数口径：整数与实数原样，文本必须整体可解析为数值，`NULL`
/// 与 blob 视为无值。SQLite 对文本套用 numeric affinity，因此 `'2abc'` 是
/// `NULL` 而不是前缀 `2`。
fn math_number(cell: &Cell) -> Option<Number> {
    match cell {
        Cell::Null => None,
        Cell::Integer(value) => Some(Number::Int(*value)),
        Cell::Real(value) => Some(Number::Real(*value)),
        Cell::Text(text) => parse_full_number(text),
        Cell::Blob(_) => None,
    }
}

/// 整体解析一段文本：两端空白可有，中途出现任何非数值字符即拒绝。整数
/// 字面量返回 [`Number::Int`]，带小数点或指数的返回 [`Number::Real`]。
fn parse_full_number(text: &str) -> Option<Number> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(value) = trimmed.parse::<i64>() {
        return Some(Number::Int(value));
    }
    match trimmed.parse::<f64>() {
        Ok(value) if value.is_finite() => Some(Number::Real(value)),
        _ => None,
    }
}

/// 数学结果统一出口：`NaN` 记为 `NULL`，正负无穷保留为实数，与 SQLite 的
/// `sqlite3_result_double` 一致。
fn real_result(value: f64) -> Cell {
    if value.is_nan() { Cell::Null } else { Cell::Real(value) }
}

/// 一元数学函数：入参按 [`math_number`] 取数，输出恒为实数。
fn real_function(args: &[Cell], function: fn(f64) -> f64) -> Cell {
    match args.first().and_then(math_number) {
        Some(Number::Int(value)) => real_result(function(value as f64)),
        Some(Number::Real(value)) => real_result(function(value)),
        None => Cell::Null,
    }
}

/// 二元数学函数：两个入参都按 [`math_number`] 取数，输出恒为实数。
fn real_function2(args: &[Cell], function: fn(f64, f64) -> f64) -> Cell {
    let left = match args.first().and_then(math_number) {
        Some(Number::Int(value)) => value as f64,
        Some(Number::Real(value)) => value,
        None => return Cell::Null,
    };
    let right = match args.get(1).and_then(math_number) {
        Some(Number::Int(value)) => value as f64,
        Some(Number::Real(value)) => value,
        None => return Cell::Null,
    };
    real_result(function(left, right))
}

/// 定义域为正数的数学函数（`ln`、`log2`、`log10`）：非正入参返回 `NULL`，
/// 不把 `-inf` 泄漏成实数。
fn positive_real_function(args: &[Cell], function: fn(f64) -> f64) -> Cell {
    let value = match args.first().and_then(math_number) {
        Some(Number::Int(value)) => value as f64,
        Some(Number::Real(value)) => value,
        None => return Cell::Null,
    };
    if value <= 0.0 {
        return Cell::Null;
    }
    real_result(function(value))
}

/// `ceil` / `floor` / `trunc`：整数入参原样返回整数，实数入参返回取整后的
/// 实数，非数值文本与 `NULL` 返回 `NULL`。
fn integral_or_real(args: &[Cell], function: fn(f64) -> f64) -> Cell {
    match args.first().and_then(math_number) {
        Some(Number::Int(value)) => Cell::Integer(value),
        Some(Number::Real(value)) => real_result(function(value)),
        None => Cell::Null,
    }
}

/// `sign()`：返回 `-1`、`0` 或 `1`，恒为整数；入参不是数值时返回 `NULL`。
fn sign_cell(args: &[Cell]) -> Cell {
    match args.first().and_then(math_number) {
        Some(Number::Int(value)) => Cell::Integer(value.signum()),
        Some(Number::Real(value)) => {
            let sign = if value > 0.0 {
                1
            }
            else if value < 0.0 {
                -1
            }
            else {
                0
            };
            Cell::Integer(sign)
        }
        None => Cell::Null,
    }
}

/// `log()`：单参是十进制对数，双参时底数必须大于 1 且真数必须为正，否则
/// 返回 `NULL`（底数为 1 时 SQLite 同样返回 `NULL` 而不是无穷）。
fn log_cell(args: &[Cell]) -> Cell {
    if args.len() == 1 {
        let value = match args.first().and_then(math_number) {
            Some(Number::Int(value)) => value as f64,
            Some(Number::Real(value)) => value,
            None => return Cell::Null,
        };
        if value <= 0.0 {
            return Cell::Null;
        }
        return real_result(value.log10());
    }
    let base = match args.first().and_then(math_number) {
        Some(Number::Int(value)) => value as f64,
        Some(Number::Real(value)) => value,
        None => return Cell::Null,
    };
    let value = match args.get(1).and_then(math_number) {
        Some(Number::Int(value)) => value as f64,
        Some(Number::Real(value)) => value,
        None => return Cell::Null,
    };
    if base <= 1.0 || value <= 0.0 {
        return Cell::Null;
    }
    real_result(value.ln() / base.ln())
}

/// Increments the last decimal digit of a non-negative fixed-point string,
/// carrying through `9`s and into the integer part.
fn increment_decimal(text: &str) -> String {
    let mut characters: Vec<char> = text.chars().collect();
    let mut index = characters.len();
    loop {
        if index == 0 {
            characters.insert(0, '1');
            break;
        }
        index -= 1;
        match characters[index] {
            '.' => continue,
            '9' => characters[index] = '0',
            digit => {
                characters[index] = char::from(digit as u8 + 1);
                break;
            }
        }
    }
    characters.into_iter().collect()
}

fn self_contained_arity(
    def: &TableDef,
    table: &str,
    columns: Option<&[String]>,
    values: &[crate::WriteValue],
) -> Result<Vec<Option<crate::WriteValue>>, SqlError> {
    let mut slots = vec![None; def.columns.len()];
    match columns {
        Some(names) => {
            if names.len() != values.len() {
                return Err(SqlError::new(
                    "arity-mismatch",
                    format!("insert into `{table}` has {} columns but {} values", names.len(), values.len()),
                )
                .in_statement("insert"));
            }
            let mut seen = Vec::new();
            for (name, value) in names.iter().zip(values) {
                let index = def.columns.iter().position(|column| column.name.eq_ignore_ascii_case(name)).ok_or_else(|| {
                    SqlError::new("unknown-column", format!("unknown column `{name}` in table `{table}`")).in_statement("insert")
                })?;
                if seen.contains(&index) {
                    return Err(
                        SqlError::new("duplicate-column", format!("column `{name}` is listed twice in the insert")).in_statement("insert")
                    );
                }
                seen.push(index);
                slots[index] = Some(value.clone());
            }
        }
        None => {
            if values.len() != def.columns.len() {
                return Err(SqlError::new(
                    "arity-mismatch",
                    format!("insert into `{table}` expects {} values but received {}", def.columns.len(), values.len()),
                )
                .in_statement("insert"));
            }
            for (index, value) in values.iter().enumerate() {
                slots[index] = Some(value.clone());
            }
        }
    }
    Ok(slots)
}

/// Finds the table one `PRAGMA` refers to. An unqualified name searches the
/// temp schema first, then `main`, matching SQLite's connection search order.
fn pragma_table_def<'a>(engine: &'a Engine, schema: Option<&str>, name: &str) -> Option<&'a TableDef> {
    let key = name.to_ascii_lowercase();
    let wants_temp = schema.map_or(true, |slot| slot.eq_ignore_ascii_case("temp"));
    let wants_main = schema.map_or(true, |slot| slot.eq_ignore_ascii_case("main"));
    if wants_temp {
        if let Some(position) = engine.catalog.temp_lookup.get(&key) {
            return Some(&engine.catalog.temp_tables[*position].def);
        }
    }
    if wants_main {
        if let Some(position) = engine.catalog.table_lookup.get(&key) {
            return Some(&engine.catalog.tables[*position]);
        }
    }
    None
}

/// Finds the view one `PRAGMA` refers to, preferring the temp schema.
fn pragma_view_def<'a>(engine: &'a Engine, schema: Option<&str>, name: &str) -> Option<&'a ViewDef> {
    let key = name.to_ascii_lowercase();
    let wants_temp = schema.map_or(true, |slot| slot.eq_ignore_ascii_case("temp"));
    let wants_main = schema.map_or(true, |slot| slot.eq_ignore_ascii_case("main"));
    if wants_temp {
        if let Some(view) = engine.catalog.temp_views.get(&key) {
            return Some(&view.def);
        }
    }
    if wants_main {
        if let Some(view) = engine.catalog.views.get(&key) {
            return Some(view);
        }
    }
    None
}

/// The declared type text `PRAGMA table_info` reports: SQLite normalises the
/// four storage-class keywords to upper case and echoes everything else as it
/// was written.
fn pragma_declared_type(declared: &str) -> String {
    let text = declared.trim();
    match text.to_ascii_lowercase().as_str() {
        "integer" | "text" | "real" | "blob" => text.to_ascii_uppercase(),
        _ => text.to_string(),
    }
}

/// Automatic indexes one table owns, in the writer's numbering order. Each
/// entry is (suffix number, keyed column positions, `PRAGMA` origin).
fn autoindex_definitions(def: &TableDef) -> Vec<(usize, Vec<usize>, &'static str)> {
    let mut number = 0usize;
    let mut definitions = Vec::new();
    for (position, column) in def.columns.iter().enumerate() {
        if (column.primary_key || column.unique) && def.rowid_alias != Some(position) {
            number += 1;
            definitions.push((number, vec![position], if column.primary_key { "pk" } else { "u" }));
        }
    }
    for constraint in &def.table_constraints {
        if constraint.primary && (def.rowid_alias.is_some() || def.without_rowid) {
            continue;
        }
        number += 1;
        definitions.push((number, constraint.columns.clone(), if constraint.primary { "pk" } else { "u" }));
    }
    definitions
}

/// `PRAGMA table_info(t)` / `PRAGMA table_xinfo(t)`.
fn pragma_table_info(engine: &Engine, schema: Option<&str>, table: &str, extended: bool) -> Result<ExecResult, SqlError> {
    let mut columns =
        vec!["cid".to_string(), "name".to_string(), "type".to_string(), "notnull".to_string(), "dflt_value".to_string(), "pk".to_string()];
    if extended {
        columns.push("hidden".to_string());
    }
    let mut rows = Vec::new();
    if let Some(def) = pragma_table_def(engine, schema, table) {
        let key_positions = def.primary_key_positions().unwrap_or_default();
        for (position, column) in def.columns.iter().enumerate() {
            let pk = key_positions.iter().position(|candidate| *candidate == position).map(|slot| slot as i64 + 1).unwrap_or(0);
            let mut row = vec![
                Cell::Integer(position as i64),
                Cell::Text(column.name.clone()),
                Cell::Text(pragma_declared_type(&column.declared_type)),
                Cell::Integer(i64::from(column.not_null)),
                column.default_text.clone().map(Cell::Text).unwrap_or(Cell::Null),
                Cell::Integer(pk),
            ];
            if extended {
                row.push(Cell::Integer(0));
            }
            rows.push(row);
        }
    }
    else if let Some(view) = pragma_view_def(engine, schema, table) {
        // SQLite infers a view column type from a plain column reference; this
        // engine reports the output names with an empty type.
        let (names, _) = engine.view_output(view)?;
        for (position, name) in names.iter().enumerate() {
            let mut row = vec![
                Cell::Integer(position as i64),
                Cell::Text(name.clone()),
                Cell::Text(String::new()),
                Cell::Integer(0),
                Cell::Null,
                Cell::Integer(0),
            ];
            if extended {
                row.push(Cell::Integer(0));
            }
            rows.push(row);
        }
    }
    Ok(ExecResult { statement: "pragma", columns, rows, affected: 0 })
}

/// `PRAGMA index_list(t)`: explicit indexes newest first, then the automatic
/// constraint indexes in reverse numbering order.
fn pragma_index_list(engine: &Engine, table: &str) -> ExecResult {
    let key = table.to_ascii_lowercase();
    let mut explicit: Vec<&IndexDef> = engine.catalog.indexes.values().filter(|index| index.table == key).collect();
    explicit.sort_by(|left, right| right.order.cmp(&left.order));
    let mut rows = Vec::new();
    for index in explicit {
        rows.push(vec![
            Cell::Integer(rows.len() as i64),
            Cell::Text(index.name.clone()),
            Cell::Integer(i64::from(index.unique)),
            Cell::Text("c".to_string()),
            Cell::Integer(0),
        ]);
    }
    if let Some(def) = pragma_table_def(engine, None, table) {
        for (number, _, origin) in autoindex_definitions(def).into_iter().rev() {
            rows.push(vec![
                Cell::Integer(rows.len() as i64),
                Cell::Text(format!("sqlite_autoindex_{}_{}", def.name, number)),
                Cell::Integer(1),
                Cell::Text(origin.to_string()),
                Cell::Integer(0),
            ]);
        }
    }
    ExecResult {
        statement: "pragma",
        columns: vec!["seq".to_string(), "name".to_string(), "unique".to_string(), "origin".to_string(), "partial".to_string()],
        rows,
        affected: 0,
    }
}

/// `PRAGMA index_info(i)`: `seqno`, `cid` and the column name of every key
/// column, in key order.
fn pragma_index_info(engine: &Engine, index: &str) -> ExecResult {
    let mut rows = Vec::new();
    if let Some(definition) = engine.catalog.indexes.get(&index.to_ascii_lowercase()) {
        if let Some(def) = pragma_table_def(engine, None, &definition.table) {
            for (seqno, position) in definition.columns.iter().enumerate() {
                rows.push(vec![Cell::Integer(seqno as i64), Cell::Integer(*position as i64), Cell::Text(def.columns[*position].name.clone())]);
            }
        }
    }
    else {
        for def in engine.catalog.tables.iter().chain(engine.catalog.temp_tables.iter().map(|temp| &temp.def)) {
            for (number, positions, _) in autoindex_definitions(def) {
                if !format!("sqlite_autoindex_{}_{}", def.name, number).eq_ignore_ascii_case(index) {
                    continue;
                }
                for (seqno, position) in positions.iter().enumerate() {
                    rows.push(vec![
                        Cell::Integer(seqno as i64),
                        Cell::Integer(*position as i64),
                        Cell::Text(def.columns[*position].name.clone()),
                    ]);
                }
            }
        }
    }
    ExecResult { statement: "pragma", columns: vec!["seqno".to_string(), "cid".to_string(), "name".to_string()], rows, affected: 0 }
}

/// `PRAGMA foreign_key_list(t)`: SQLite prepends each new key, so the list is
/// newest first.
fn pragma_foreign_key_list(engine: &Engine, table: &str) -> ExecResult {
    let mut rows = Vec::new();
    if let Some(def) = pragma_table_def(engine, None, table) {
        for (id, key) in def.foreign_keys.iter().rev().enumerate() {
            for (seq, position) in key.columns.iter().enumerate() {
                rows.push(vec![
                    Cell::Integer(id as i64),
                    Cell::Integer(seq as i64),
                    Cell::Text(key.parent_display.clone()),
                    Cell::Text(def.columns[*position].name.clone()),
                    key.parent_columns.get(seq).map(|name| Cell::Text(name.clone())).unwrap_or(Cell::Null),
                    Cell::Text(key.on_update.clone()),
                    Cell::Text(key.on_delete.clone()),
                    Cell::Text("NONE".to_string()),
                ]);
            }
        }
    }
    ExecResult {
        statement: "pragma",
        columns: vec![
            "id".to_string(),
            "seq".to_string(),
            "table".to_string(),
            "from".to_string(),
            "to".to_string(),
            "on_update".to_string(),
            "on_delete".to_string(),
            "match".to_string(),
        ],
        rows,
        affected: 0,
    }
}

/// Whether one parsed expression mentions `column`; `qualifiers` restricts the
/// match to `NEW` / `OLD` references when provided.
fn expression_mentions_column(expr: &Expr, column: &str, qualifiers: Option<&[&str]>) -> bool {
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
fn select_items_mention_column(items: &SelectItems, column: &str, qualifiers: Option<&[&str]>) -> bool {
    match items {
        SelectItems::Named(items) => items.iter().any(|item| expression_mentions_column(item, column, qualifiers)),
        SelectItems::Star | SelectItems::CountAll => false,
    }
}

/// Whether one window specification mentions the column in its partitioning,
/// ordering or frame offsets.
fn window_mentions_column(spec: &WindowSpec, column: &str, qualifiers: Option<&[&str]>) -> bool {
    let bound = |edge: &FrameBound| edge.offset.as_ref().is_some_and(|value| expression_mentions_column(value, column, qualifiers));
    spec.partition.iter().any(|expr| expression_mentions_column(expr, column, qualifiers))
        || spec.order.iter().any(|key| expression_mentions_column(&key.expr, column, qualifiers))
        || spec.frame.as_ref().is_some_and(|frame| bound(&frame.start) || bound(&frame.end))
}

/// Whether one `SELECT` core mentions the column.
fn core_mentions_column(core: &SelectCore, column: &str, qualifiers: Option<&[&str]>) -> bool {
    select_items_mention_column(&core.items, column, qualifiers)
        || core.from.iter().any(|source| source.query.as_ref().is_some_and(|query| statement_mentions_column(query, column, qualifiers)))
        || core.filter.as_ref().is_some_and(|expr| expression_mentions_column(expr, column, qualifiers))
        || core.group.iter().any(|expr| expression_mentions_column(expr, column, qualifiers))
        || core.having.as_ref().is_some_and(|expr| expression_mentions_column(expr, column, qualifiers))
        || core.windows.iter().any(|window| window_mentions_column(&window.spec, column, qualifiers))
}

/// Whether one statement mentions the column; used to refuse dropping a column
/// still read by a view or a trigger body.
fn statement_mentions_column(statement: &Statement, column: &str, qualifiers: Option<&[&str]>) -> bool {
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

/// The error SQLite raises when a parent key is not a unique key of its table.
fn foreign_key_mismatch(child: &TableDef, key: &ForeignKeyDef, statement: &'static str) -> SqlError {
    SqlError::new("SQLITE_ERROR", format!("foreign key mismatch - \"{}\" referencing \"{}\"", child.name, key.parent_display))
        .in_statement(statement)
}

/// The error SQLite raises when a non-NULL child key has no parent row.
fn foreign_key_violation(statement: &'static str) -> SqlError {
    SqlError::new("SQLITE_CONSTRAINT_FOREIGNKEY", "FOREIGN KEY constraint failed").in_statement(statement)
}

/// Selects the cells at `positions` from one decoded row.
fn pick(cells: &[Cell], positions: &[usize]) -> Vec<Cell> {
    positions.iter().map(|position| cells[*position].clone()).collect()
}

/// Whether one child key equals the parent key held by `parent_cells`.
fn keys_equal(child_values: &[Cell], parent_cells: &[Cell], parent_columns: &[usize]) -> bool {
    child_values.iter().zip(parent_columns.iter()).all(|(child, position)| compare_cells(child, &parent_cells[*position]) == Ordering::Equal)
}

/// Copies one pre-statement row and applies an `UPDATE` assignment to it.
fn apply_assignment(old: &[Cell], values: &[UpdateValue]) -> Vec<Cell> {
    let mut next = old.to_vec();
    for (index, value) in values.iter().enumerate() {
        if let UpdateValue::Set(value) = value {
            next[index] = write_value_cell(value);
        }
    }
    next
}

/// The pre-statement cells of one rowid; the plan only contains live rowids.
fn row_of<'a>(rows: &'a [(i64, Vec<Cell>)], rowid: i64) -> &'a [Cell] {
    rows.iter().find(|(id, _)| *id == rowid).map(|(_, cells)| cells.as_slice()).unwrap_or(&[])
}

/// Whether two column-position lists name the same key, ignoring order.
fn same_positions(left: &[usize], right: &[usize]) -> bool {
    let mut left = left.to_vec();
    let mut right = right.to_vec();
    left.sort_unstable();
    right.sort_unstable();
    left == right
}

/// Builds the row a `CHECK` is evaluated against right after an insert.
///
/// Missing slots take the column default, the rowid alias is filled from the
/// rowid the writer assigned, and one extra cell carries the hidden rowid so a
/// check that names `rowid` reads it.
fn check_row(def: &TableDef, slots: &[Option<crate::WriteValue>], rowid: i64) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(def.columns.len() + 1);
    for (index, slot) in slots.iter().enumerate() {
        let column = &def.columns[index];
        let value = match slot {
            Some(value) => value.clone(),
            None => column.default_value.clone().unwrap_or(crate::WriteValue::Null),
        };
        cells.push(write_value_cell(&crate::apply_write_affinity(&column.declared_type, value)));
    }
    if let Some(alias) = def.rowid_alias {
        if matches!(cells.get(alias), Some(Cell::Null)) {
            cells[alias] = Cell::Integer(rowid);
        }
    }
    cells.push(Cell::Integer(rowid));
    cells
}

/// `INSERT OR IGNORE` 吞掉 UNIQUE / NOT NULL / CHECK 冲突，其余错误照常上报。
fn ignoreable_insert_error(error: &SqlError) -> bool {
    let message = error.message.as_str();
    message.contains("UNIQUE constraint failed")
        || message.contains("NOT NULL constraint failed")
        || message.contains("CHECK constraint failed")
}

/// Reports one `STRICT` storage-class violation: the value is first converted
/// by the column affinity, then has to land in the declared storage class.
fn strict_value_violation(def: &TableDef, position: usize, value: &crate::WriteValue, statement: &'static str) -> Option<SqlError> {
    if !def.strict || matches!(value, crate::WriteValue::Null) {
        return None;
    }
    let column = &def.columns[position];
    let converted = crate::apply_write_affinity(&column.declared_type, value.clone());
    let declared = column.declared_type.to_ascii_uppercase();
    let conforms = match declared.as_str() {
        "INT" | "INTEGER" => matches!(converted, crate::WriteValue::Integer(_)),
        "REAL" => matches!(converted, crate::WriteValue::Real(_)),
        "TEXT" => matches!(converted, crate::WriteValue::Text(_)),
        "BLOB" => matches!(converted, crate::WriteValue::Blob(_)),
        _ => true,
    };
    if conforms {
        return None;
    }
    let stored = match converted {
        crate::WriteValue::Real(_) => "REAL",
        crate::WriteValue::Text(_) => "TEXT",
        crate::WriteValue::Blob(_) => "BLOB",
        _ => "INTEGER",
    };
    Some(
        SqlError::new("constraint-datatype", format!("cannot store {stored} value in {declared} column {}.{}", def.name, column.name))
            .in_statement(statement),
    )
}

fn cell_write_value(cell: &Cell) -> crate::WriteValue {
    match cell {
        Cell::Null => crate::WriteValue::Null,
        Cell::Integer(value) => crate::WriteValue::Integer(*value),
        Cell::Real(value) => crate::WriteValue::Real(*value),
        Cell::Text(value) => crate::WriteValue::Text(value.clone()),
        Cell::Blob(value) => crate::WriteValue::Blob(value.clone()),
    }
}

/// 把一行已解码的单元格整体转成写侧的存储值,供写语句复用自己已读出的行像。
fn cells_write_values(cells: &[Cell]) -> Vec<crate::WriteValue> {
    cells.iter().map(cell_write_value).collect()
}

/// Inserts one row into an in-memory temp table and returns its rowid.
///
/// The rowid is explicit when the table has an `INTEGER PRIMARY KEY` alias and
/// the statement supplied one, and `max(rowid) + 1` otherwise. NOT NULL and the
/// primary key / UNIQUE keys are enforced here because temp tables never reach
/// the format-3 writer.
fn temp_insert_row(
    def: &TableDef,
    rows: &mut BTreeMap<i64, Vec<Cell>>,
    slots: &[Option<crate::WriteValue>],
    keys: &[UniqueKey],
) -> Result<i64, SqlError> {
    let mut rowid = rows.keys().next_back().copied().unwrap_or(0) + 1;
    if let Some(alias) = def.rowid_alias {
        match slots.get(alias).and_then(|slot| slot.as_ref()) {
            Some(crate::WriteValue::Integer(value)) => rowid = *value,
            Some(crate::WriteValue::Null) | None => {}
            Some(_) => return Err(SqlError::new("datatype-mismatch", "datatype mismatch").in_statement("insert")),
        }
    }
    let cells = check_row(def, slots, rowid);
    // `check_row` appends the rowid as a hidden slot for CHECK/RETURNING
    // evaluation; the stored temp row keeps only the declared columns and the
    // row map key carries the rowid.
    let cells: Vec<Cell> = cells.into_iter().take(def.columns.len()).collect();
    for (position, column) in def.columns.iter().enumerate() {
        if column.not_null && matches!(cells[position], Cell::Null) {
            return Err(SqlError::new("not-null-constraint", format!("NOT NULL constraint failed: {}.{}", def.name, column.name))
                .in_statement("insert"));
        }
    }
    let live: Vec<(i64, Vec<Cell>)> = rows.iter().map(|(rowid, cells)| (*rowid, cells.clone())).collect();
    if let Some(label) = unique_conflict(keys, &cells, rowid, &live, i64::MIN) {
        return Err(SqlError::new("unique-constraint", format!("UNIQUE constraint failed: {label}")).in_statement("insert"));
    }
    rows.insert(rowid, cells);
    Ok(rowid)
}

/// Declared column positions of a managed index whose terms are all plain
/// columns, or `None` for a real expression key.
fn managed_term_positions(index: &IndexDef) -> Option<Vec<usize>> {
    index
        .terms
        .iter()
        .map(|term| match term {
            ResolvedExpr::Column { source: 0, index: column } => Some(*column),
            _ => None,
        })
        .collect()
}

/// Whether one managed index is the one an `ON CONFLICT (columns)` target names.
fn managed_target_matches(target: &ManagedTarget, positions: &[usize], predicate: Option<&ResolvedExpr>) -> bool {
    if target.index.predicate.as_ref() != predicate {
        return false;
    }
    match managed_term_positions(&target.index) {
        Some(terms) => !terms.is_empty() && same_positions(&terms, positions),
        None => false,
    }
}

/// `t.a, t.b` fragment of a managed index, matching SQLite's uniqueness text.
fn managed_index_label(def: &TableDef, index: &IndexDef) -> String {
    match managed_term_positions(index) {
        Some(positions) if !positions.is_empty() => {
            positions.iter().map(|position| format!("{}.{}", def.name, def.columns[*position].name)).collect::<Vec<_>>().join(", ")
        }
        _ => format!("index '{}'", index.name),
    }
}

/// Whether two computed managed-index keys collide, with SQLite's rule that a
/// `NULL` component never conflicts.
fn managed_keys_conflict(first: &[Cell], second: &[Cell], collations: &[Collation]) -> bool {
    first.len() == second.len()
        && first.iter().zip(second).enumerate().all(|(position, (left, right))| {
            !matches!(left, Cell::Null)
                && !matches!(right, Cell::Null)
                && compare_cells_with(left, right, collations.get(position).copied().unwrap_or_default()) == Ordering::Equal
        })
}

/// Whether one supplied upsert row collides with one stored row on `key`.
///
/// A `NULL` in any key column never conflicts, exactly like a unique index.
fn upsert_key_conflicts(key: &UniqueKey, candidate: &[Cell], candidate_rowid: i64, existing_rowid: i64, existing: &[Cell]) -> bool {
    match key {
        UniqueKey::Rowid(_) => candidate_rowid == existing_rowid,
        UniqueKey::Columns(positions, _, collations) => positions.iter().enumerate().all(|(index, position)| {
            let first = &candidate[*position];
            let second = &existing[*position];
            !matches!(first, Cell::Null)
                && !matches!(second, Cell::Null)
                && compare_cells_with(first, second, collations.get(index).copied().unwrap_or_default()) == Ordering::Equal
        }),
    }
}

/// Reports the first uniqueness rule the candidate row violates against the
/// live table, skipping the row being replaced.
fn unique_conflict(keys: &[UniqueKey], candidate: &[Cell], rowid: i64, live: &[(i64, Vec<Cell>)], skip: i64) -> Option<String> {
    for key in keys {
        let wanted = key.values(candidate, rowid);
        for (other_rowid, other_cells) in live {
            if *other_rowid == skip {
                continue;
            }
            let other = key.values(other_cells, *other_rowid);
            let collations = match key {
                UniqueKey::Rowid(_) => &[][..],
                UniqueKey::Columns(_, _, collations) => collations.as_slice(),
            };
            let collides = wanted.iter().zip(&other).enumerate().all(|(index, (first, second))| {
                !matches!(first, Cell::Null)
                    && !matches!(second, Cell::Null)
                    && compare_cells_with(first, second, collations.get(index).copied().unwrap_or_default()) == Ordering::Equal
            });
            if collides {
                return Some(key.label().to_string());
            }
        }
    }
    None
}

/// Converts a storage rejection of an `UPDATE` / `DELETE` into a structured
/// error, naming the table when the writer does not say which key failed.
fn write_failure(error: database_types::Error, statement: &'static str, def: &TableDef) -> SqlError {
    let mapped = storage_failure(error);
    let message = if mapped.code == "unique-constraint" {
        format!("UNIQUE constraint failed in table `{}`", def.name)
    }
    else if mapped.code == "not-null-constraint" || mapped.message.contains("NOT NULL") {
        format!("NOT NULL constraint failed in table `{}`", def.name)
    }
    else if mapped.message.contains("integer") || mapped.message.contains("datatype") {
        return SqlError::new("datatype-mismatch", "datatype mismatch").in_statement(statement);
    }
    else {
        mapped.message
    };
    SqlError::new(mapped.code, message).in_statement(statement)
}

/// 管理型索引键里是否存在按排序规则等价的重复项，`NULL` 键不算冲突。
fn managed_unique_violation(entries: &[(Vec<crate::WriteValue>, i64)], collations: &[Collation]) -> bool {
    for right in 1..entries.len() {
        for left in 0..right {
            let collides = entries[left].0.iter().zip(&entries[right].0).enumerate().all(|(position, (first, second))| {
                if matches!(first, crate::WriteValue::Null) || matches!(second, crate::WriteValue::Null) {
                    return false;
                }
                let collation = collations.get(position).copied().unwrap_or_default();
                match (first, second) {
                    (crate::WriteValue::Text(first), crate::WriteValue::Text(second)) => collation.compare_text(first, second).is_eq(),
                    _ => first == second,
                }
            });
            if collides {
                return true;
            }
        }
    }
    false
}

fn decode_row(payload: &[u8], def: &TableDef, rowid: i64) -> Result<Vec<Cell>, SqlError> {
    decode_row_masked(payload, def, rowid, None)
}

/// Decodes one stored row, keeping only the declared columns `keep` flags.
///
/// A skipped position still walks its serial type and body bounds, so a corrupt
/// payload fails exactly as in the full decode, but its text or blob value is
/// never materialized. `None` keeps every column.
fn decode_row_masked(payload: &[u8], def: &TableDef, rowid: i64, keep: Option<&[bool]>) -> Result<Vec<Cell>, SqlError> {
    let mut cells = Vec::new();
    decode_row_into(payload, def, rowid, keep, &mut cells)?;
    Ok(cells)
}

/// Decodes one stored row into `cells`, reusing that buffer's allocation.
///
/// The result is exactly [`decode_row_masked`]'s: kept columns decoded, skipped
/// positions as [`Cell::Null`], short records padded with their declared
/// defaults and an `INTEGER PRIMARY KEY` alias filled from the rowid.
fn decode_row_into(payload: &[u8], def: &TableDef, rowid: i64, keep: Option<&[bool]>, cells: &mut Vec<Cell>) -> Result<(), SqlError> {
    decode_cells_kept_into(payload, keep, cells)?;
    // A record written before `ALTER TABLE ADD COLUMN` is short; SQLite reads
    // the missing trailing columns as their declared default.
    while cells.len() < def.columns.len() {
        let kept = keep.is_none_or(|mask| mask.get(cells.len()).copied().unwrap_or(false));
        let value = if kept {
            let column = &def.columns[cells.len()];
            let value = column.default_value.clone().unwrap_or(crate::WriteValue::Null);
            write_value_cell(&crate::apply_write_affinity(&column.declared_type, value))
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
fn mask_covers(broad: Option<&[bool]>, narrow: Option<&[bool]>) -> bool {
    match (broad, narrow) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(broad), Some(narrow)) => {
            narrow.iter().enumerate().all(|(position, needed)| !*needed || broad.get(position).copied().unwrap_or(false))
        }
    }
}

/// 一次惰性游标读取返回的行窗口。
pub(crate) struct LazyBatch {
    /// 本窗口的行,保持扫描顺序。
    pub(crate) rows: Vec<Vec<Cell>>,
    /// 扫描是否已经到达表末尾。
    pub(crate) done: bool,
}

/// 一个已经解析好的惰性单表扫描计划。
///
/// 计划里的表达式都已解析成 owned 形态,不借用引擎目录;执行期只需要逐行
/// 求值。整页快照与 b-tree 遍历状态在 [`LazyTableScan`] 里。
pub(crate) struct LazyCursor {
    plan: LazyPlan,
    scan: LazyTableScan,
    budget: Budget,
    parameters: Vec<Cell>,
    finished: bool,
}

impl LazyCursor {
    /// 打开时推导出的结果列标签。
    pub(crate) fn columns(&self) -> &[String] {
        &self.plan.labels
    }
}

struct LazyPlan {
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
struct LazyTableScan {
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
            limits: TableLimits::default(),
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
fn lazy_order_matches_scan(def: &TableDef, entry: &TableRef, items: &SelectItems, order: &[OrderKey]) -> bool {
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
fn lazy_page(pages: &[u8], page_size: usize, page_count: usize, number: u32) -> Result<&[u8], SqlError> {
    let index = number as usize;
    if index == 0 || index > page_count {
        return Err(corrupt_error("sqlite page reference out of bounds"));
    }
    let start = (index - 1) * page_size;
    pages.get(start..start + page_size).ok_or_else(|| corrupt_error("sqlite page truncated"))
}

/// 与上游 SQLite 的 U 值公式一致,返回载荷在本页存放的本地字节数。
fn lazy_local(usable: usize, length: usize) -> usize {
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
fn lazy_cell_payload(
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

fn lazy_read_u16(bytes: &[u8], offset: usize) -> Result<usize, SqlError> {
    let value = bytes.get(offset..offset + 2).ok_or_else(|| corrupt_error("sqlite b-tree field truncated"))?;
    Ok(usize::from(u16::from_be_bytes(value.try_into().expect("two bytes"))))
}

fn lazy_read_u32(bytes: &[u8], offset: usize) -> Result<u32, SqlError> {
    let value = bytes.get(offset..offset + 4).ok_or_else(|| corrupt_error("sqlite page field truncated"))?;
    Ok(u32::from_be_bytes(value.try_into().expect("four bytes")))
}

/// Decodes stored row payloads into merged rows, appending each hidden rowid.
fn decode_stored_rows(stored: &[TableRow], def: &TableDef, keep: Option<&[bool]>) -> Result<Vec<Vec<Cell>>, SqlError> {
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
fn column_qualified_as(column: &ColumnRef, label: &str) -> bool {
    column.qualifier.as_deref().is_some_and(|qualifier| qualifier.eq_ignore_ascii_case(label))
}

/// Collects the column references one expression can read, refusing any node
/// whose reads cannot be attributed from the raw AST.
///
/// Subqueries may read the enclosing row through a correlation, and window
/// functions carry their own partition and order expressions; both keep the
/// full decode instead of guessing.
fn collect_unmasked_columns<'a>(expr: &'a Expr, out: &mut Vec<&'a ColumnRef>) -> bool {
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
fn decode_cells_kept_into(payload: &[u8], keep: Option<&[bool]>, cells: &mut Vec<Cell>) -> Result<(), SqlError> {
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
fn record_text(bytes: &[u8]) -> Result<String, SqlError> {
    std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| SqlError::new("corrupt", "sqlite non-unicode UTF-8 text").in_statement("select"))
}

/// Sign-extends a big-endian SQLite integer field of one to eight bytes.
fn signed_record_integer(bytes: &[u8]) -> i64 {
    let mut value = if bytes[0] & 0x80 != 0 { u64::MAX } else { 0 };
    for byte in bytes {
        value = (value << 8) | u64::from(*byte);
    }
    value as i64
}

/// Serializes results as a JSON array for the wasm host.
pub fn results_json(results: &[ExecResult]) -> String {
    let mut out = String::from("[");
    for (index, result) in results.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&result.to_json());
    }
    out.push(']');
    out
}

fn cell_json(cell: &Cell) -> String {
    match cell {
        Cell::Null => "null".to_string(),
        Cell::Integer(value) => value.to_string(),
        Cell::Real(value) => {
            if value.is_finite() {
                // Keep the value visibly real (`1.0`, `9.223372036854776e18`)
                // so a whole-valued real never degrades into an integer when
                // the JSON envelope is parsed back.
                format!("{value:?}")
            }
            else {
                "null".to_string()
            }
        }
        Cell::Text(value) => json_string(value),
        Cell::Blob(bytes) => {
            let mut hex = String::with_capacity(bytes.len() * 2 + 12);
            hex.push_str("{\"$blob\":\"");
            for byte in bytes {
                hex.push_str(&format!("{byte:02x}"));
            }
            hex.push_str("\"}");
            hex
        }
    }
}

/// One step of the schematic program returned by `EXPLAIN`.
struct ProgramStep {
    opcode: &'static str,
    p1: i64,
    p2: i64,
    p4: Option<String>,
    p5: i64,
}

impl ProgramStep {
    fn plain(opcode: &'static str, p1: i64) -> Self {
        Self { opcode, p1, p2: 0, p4: None, p5: 0 }
    }

    fn next(p1: i64) -> Self {
        Self { opcode: "Next", p1, p2: 0, p4: None, p5: 1 }
    }

    fn read(cursor: i64, label: String) -> Self {
        Self { opcode: "OpenRead", p1: cursor, p2: 0, p4: Some(label), p5: 0 }
    }

    fn write(name: &str) -> Self {
        Self { opcode: "OpenWrite", p1: 0, p2: 0, p4: Some(name.to_string()), p5: 0 }
    }

    fn named(opcode: &'static str, name: &str) -> Self {
        Self { opcode, p1: 0, p2: 0, p4: Some(name.to_string()), p5: 0 }
    }
}

/// `FROM` label used by `EXPLAIN` rows. A derived table has no name, so the
/// empty label becomes `(subquery)` to stay readable.
fn table_ref_label(table: &TableRef) -> String {
    match table.label() {
        label if label.is_empty() => "(subquery)".to_string(),
        label => label.to_string(),
    }
}

fn push_plan_row(rows: &mut Vec<Vec<Cell>>, next_id: &mut i64, detail: &str) {
    rows.push(vec![Cell::Integer(*next_id), Cell::Integer(0), Cell::Integer(0), Cell::Text(detail.to_string())]);
    *next_id += 1;
}

fn program_row(addr: i64, opcode: &str, p1: i64, p2: i64, p3: i64, p4: Option<&str>, p5: i64) -> Vec<Cell> {
    vec![
        Cell::Integer(addr),
        Cell::Text(opcode.to_string()),
        Cell::Integer(p1),
        Cell::Integer(p2),
        Cell::Integer(p3),
        p4.map(|p4| Cell::Text(p4.to_string())).unwrap_or(Cell::Null),
        Cell::Integer(p5),
        Cell::Null,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一台空内存引擎。
    fn engine() -> Engine {
        Engine::new(None).expect("blank engine")
    }

    /// 用固定语句序列建好一台引擎。
    fn joined_engine(statements: &[&str]) -> Engine {
        let mut engine = engine();
        for statement in statements {
            engine.execute(*statement).unwrap_or_else(|error| panic!("`{statement}` failed: {error}"));
        }
        engine
    }

    // 详细正确性矩阵已整体下移到下游 SQLite 产品仓 `sqlite-conformance`（tests/engine.rs），
    // 经固定 rev 消费本门面公开面。本用例读取私有字段 `index_probes`，
    // 暂时无法用公开 API 表达，保留于此。
    #[test]
    fn integral_real_keys_match_their_literal_forms() {
        let mut engine = joined_engine(&[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, label TEXT, n INTEGER)",
            "INSERT INTO t VALUES (1,'a',10),(2,'b',20),(3,'c',30)",
            "CREATE INDEX tn ON t (n)",
        ]);
        // JS 绑定的数字是 REAL 存储类；无小数部分时亲和转换后应命中同一行集。
        for (bound_sql, parameters, literal_sql) in [
            ("SELECT label FROM t WHERE id = ?", vec![Cell::Real(3.0)], "SELECT label FROM t WHERE id = 3.0"),
            ("SELECT label FROM t WHERE id = ?", vec![Cell::Real(3.5)], "SELECT label FROM t WHERE id = 3.5"),
            ("SELECT id FROM t WHERE n = ?", vec![Cell::Real(20.0)], "SELECT id FROM t WHERE n = 20.0"),
            (
                "SELECT id FROM t WHERE n BETWEEN ? AND ?",
                vec![Cell::Real(10.0), Cell::Real(20.0)],
                "SELECT id FROM t WHERE n BETWEEN 10.0 AND 20.0",
            ),
        ] {
            let bound = engine.execute_with_parameters(bound_sql, &parameters).expect("bound form").remove(0).rows;
            let literal = engine.execute_one(literal_sql).expect("literal form").rows;
            assert_eq!(bound, literal, "`{bound_sql}` must match `{literal_sql}`");
        }
        // 整数索引探针确实吃到了 REAL 键：缓存里应留下该索引的键集。
        assert!(!engine.index_probes.borrow().is_empty(), "an integral REAL key must still use the index probe");
    }
}
