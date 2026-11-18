//! In-memory execution layer: SQL subset over the pure-Rust format-3 writer.
//!
//! The engine keeps the whole database image in `SqliteWriter::pages()`. It
//! never imports an upstream SQLite engine; [`Engine::from_writer`] wraps an
//! image read by `SqliteWriter::open`, and [`Engine::save`] flushes the current
//! image back to a file.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use database_execution::sqlite::{
    cells::{compare_cells, compare_cells_with, truthiness},
    eval::{Budget, EvalContext, EvalExpr, EvalReturningPlan},
    host::HostFunctionTable,
    probe::*,
    select::*,
    window::*,
};
use database_udf::{HostMicroDefinition, HostRuntimeAdapter};

use crate::{
    Collation, ColumnSpec, DropIndexOutcome, RecordLimits, SchemaObjectKind, SqliteWriter, TableLimits, TableRow, TextEncoding, UpdateValue,
    decode_library_version, decode_record, read_index_table_with, read_table, validate_database,
};

use super::{
    AlterAction, Assignment, ColumnRef, CompareOp, CompoundTerm, ConflictAction, ConflictClause, DropColumnConflict, Expr, ForeignKeyAction,
    IndexColumn, InsertValue, Limit, Literal, OrderKey, SelectCore, SelectItems, SqlError, Statement, TableRef, TransactionAction,
    TriggerEvent, TriggerTiming, json_string, parse, parse_with_host_functions, rewrite_create_index_column, rewrite_create_index_table,
    rewrite_create_table_column, rewrite_create_table_drop_column, rewrite_create_table_name, rewrite_view_column, rewrite_view_table,
    statement_references, storage_failure, strip_schema_qualifiers,
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

/// Object family an unqualified `DROP` or `ALTER` statement may target in an
/// attached database.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Table,
    Index,
    View,
    Trigger,
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
    /// SELECT 机器共享的语句级缓存状态(写世代、索引探针、子查询记忆等)。
    exec_state: SqliteExecState,
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
            exec_state: SqliteExecState::default(),

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

    /// 组装本引擎的 SELECT 执行上下文:按引用共享目录、写入器、缓存与参数。
    fn exec(&self) -> ExecContext<'_> {
        ExecContext {
            writer: &self.writer,
            catalog: &self.catalog,
            state: &self.exec_state,
            host_functions: &self.host_functions,
            parameters: &self.parameters,
            upsert_excluded: &self.upsert_excluded,
            trigger_frames: &self.trigger_frames,
            alias: self.alias.as_deref(),
            attached: self.attached.iter().map(|db| db.engine.exec()).collect::<Vec<_>>().into(),
            attached_lookup: &self.attached_lookup,
        }
    }

    /// 打开一条只读惰性游标:先定型计划并冲刷待物化写入,再交给执行树只读打开。
    ///
    /// 计划在此刻一次成型,整页快照也在这一刻克隆;之后的逐页解码不再回到
    /// 目录,因此游标读取与并发写入完全隔离。
    pub(crate) fn open_lazy_cursor(&mut self, statement: &Statement, parameters: &[Cell]) -> Result<Option<LazyCursor>, SqlError> {
        let Some(plan) = self.exec().plan_lazy_scan(statement)?
        else {
            return Ok(None);
        };
        // 游标快照直接克隆镜像页,推迟的索引重建必须先落下。
        self.flush_pending_materialization("cursor")?;
        self.exec().open_lazy_scan(plan, parameters)
    }

    /// 从惰性游标读取至多 limit 行,保持 b-tree 扫描顺序。
    pub(crate) fn lazy_cursor_next(&mut self, cursor: &mut LazyCursor, limit: usize) -> Result<LazyBatch, SqlError> {
        if cursor.finished || limit == 0 {
            return Ok(LazyBatch { rows: Vec::new(), done: cursor.finished });
        }
        let saved = std::mem::take(&mut self.parameters);
        self.parameters = cursor.parameters.clone();
        let result = self.exec().lazy_cursor_next_inner(cursor, limit);
        self.parameters = saved;
        result
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
            exec: self.exec(),
            parameters: &self.parameters,
            upsert_excluded: &self.upsert_excluded,
            trigger_frames: &self.trigger_frames,
            outer_read: &self.exec_state.outer_read,
            subquery_cache: &self.exec_state.subquery_cache,
            host_functions: &self.host_functions,
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
            exec: self.exec(),
            parameters: &self.parameters,
            upsert_excluded: &self.upsert_excluded,
            trigger_frames: &self.trigger_frames,
            outer_read: &self.exec_state.outer_read,
            subquery_cache: &self.exec_state.subquery_cache,
            host_functions: &self.host_functions,
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
        let (names, _) = self.exec().view_output(&view)?;
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
        let (names, rows) = self.exec().view_output(&view)?;
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
            match row_matches(&predicate, &row, &self.exec(), &shape, &budget) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
            let ctx = EvalContext {
                exec: self.exec(),
                parameters: &self.parameters,
                upsert_excluded: &self.upsert_excluded,
                trigger_frames: &self.trigger_frames,
                outer_read: &self.exec_state.outer_read,
                subquery_cache: &self.exec_state.subquery_cache,
                host_functions: &self.host_functions,
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
        let (names, rows) = self.exec().view_output(&view)?;
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
            match row_matches(&predicate, &row, &self.exec(), &shape, &budget) {
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
        self.exec_state.subquery_cache.borrow_mut().clear();
        self.exec_state.exists_probes.borrow_mut().clear();
        self.exec_state.statement_scans.borrow_mut().clear();
        self.exec_state.outer_read.set(false);
        let result = self.run_statement_dispatch(statement);
        // 释放本条语句解码的整表行,避免大表在下一条语句前仍驻留内存。
        self.exec_state.statement_scans.borrow_mut().clear();
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
                Statement::Insert { schema: None, table, conflict: None, replace: false, .. }
                    if self.exec().insert_keeps_index_content(table) => {}
                _ => match self.exec().statement_table_key(&statement) {
                    Some(key) => self.exec().invalidate_table_indexes(&key),
                    None => self.exec().invalidate_all_indexes(),
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
            self.exec_state.index_probes.borrow_mut().clear();
            self.exec_state.subquery_cache.borrow_mut().clear();
            self.exec_state.exists_probes.borrow_mut().clear();
            self.exec_state.write_epoch.set(self.exec_state.write_epoch.get() + 1);
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
        let selected = self.exec().select_statement(query, &[], &budget, None)?;
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
                        Some(self.exec().select_statement(query, &[], &budget, None)?.rows)
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
                let watch_probes = conflict.is_none()
                    && !replace
                    && temp_position.is_none()
                    && !self.exec().table_has_triggers(&key)
                    && self.exec().table_has_probes(&key);
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
                let queue_triggers = self.exec().table_has_triggers(&key);
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
                        exec: self.exec(),
                        parameters: &self.parameters,
                        upsert_excluded: &self.upsert_excluded,
                        trigger_frames: &self.trigger_frames,
                        outer_read: &self.exec_state.outer_read,
                        subquery_cache: &self.exec_state.subquery_cache,
                        host_functions: &self.host_functions,
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
                        self.exec().patch_index_probes_after_insert(&key, &probe_rows);
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
                    self.exec().install_empty_index_probe(&name);
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
                    let exec = self.exec();
                    for entry in &from {
                        let (columns, rows) = exec.select_source(entry, &budget, None, None, None)?;
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
                            exec: self.exec(),
                            parameters: &self.parameters,
                            upsert_excluded: &self.upsert_excluded,
                            trigger_frames: &self.trigger_frames,
                            outer_read: &self.exec_state.outer_read,
                            subquery_cache: &self.exec_state.subquery_cache,
                            host_functions: &self.host_functions,
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
                let queue_triggers = self.exec().table_has_triggers(&key);
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
                    self.exec().invalidate_all_indexes();
                }
                else {
                    self.exec().invalidate_indexes_after_update(def, &key, &assigned);
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
                let queue_triggers = self.exec().table_has_triggers(&key);
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
                    self.exec().invalidate_all_indexes();
                }
                else {
                    self.exec().patch_index_probes_after_delete(&key, &plan.removed, &plan.live);
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
                let outcome = self.exec().run_select(&with, recursive, &first, &compounds, order, limit, &[], &budget, None)?;
                Ok(ExecResult { statement: "select", columns: outcome.columns, rows: outcome.rows, affected: 0 })
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
        if let Some(rowid) = ExecContext::rowid_eq_conjunct(filter, alias_name.as_deref(), label, &self.parameters) {
            return Some(vec![rowid]);
        }
        // `IN (<rowids>)` 与 `>= a AND <= b` 同样是 rowid 上的精确候选集,
        // 不必为一条写语句解码整表再逐行判断。
        if let Some(rowids) = ExecContext::rowid_in_conjunct(filter, alias_name.as_deref(), label, &self.parameters) {
            return Some(rowids);
        }
        if let Some(rowids) = ExecContext::rowid_range_candidates(filter, alias_name.as_deref(), label, &self.parameters) {
            return Some(rowids);
        }
        for index in self.catalog.indexes.values() {
            if index.managed || index.table != table_key || index.columns.is_empty() {
                continue;
            }
            if let Some(rowids) = self.exec().index_probe_rows(index, def, filter, label) {
                return Some(rowids);
            }
            if let Some(rowids) = self.exec().index_range_rows(index, def, filter, label) {
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
            exec: self.exec(),
            parameters: &self.parameters,
            upsert_excluded: &self.upsert_excluded,
            trigger_frames: &self.trigger_frames,
            outer_read: &self.exec_state.outer_read,
            subquery_cache: &self.exec_state.subquery_cache,
            host_functions: &self.host_functions,
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
            && !self.exec().table_has_triggers(table_key)
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
            let ctx = EvalContext {
                exec: self.exec(),
                parameters: &self.parameters,
                upsert_excluded: &self.upsert_excluded,
                trigger_frames: &self.trigger_frames,
                outer_read: &self.exec_state.outer_read,
                subquery_cache: &self.exec_state.subquery_cache,
                host_functions: &self.host_functions,
                outer: &[],
                current: shape,
                aggregates: &[],
                windows: &[],
                window_row: 0,
                budget,
                ctes: None,
            };
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
        let queue_triggers = self.exec().table_has_triggers(table_key);
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
        if temp_position.is_none() && !self.exec().table_has_triggers(table_key) {
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
                exec: self.exec(),
                parameters: &self.parameters,
                upsert_excluded: &self.upsert_excluded,
                trigger_frames: &self.trigger_frames,
                outer_read: &self.exec_state.outer_read,
                subquery_cache: &self.exec_state.subquery_cache,
                host_functions: &self.host_functions,
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
            let fires = self.exec().table_has_triggers(&child_key);
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
            let ctx = EvalContext {
                exec: self.exec(),
                parameters: &self.parameters,
                upsert_excluded: &self.upsert_excluded,
                trigger_frames: &self.trigger_frames,
                outer_read: &self.exec_state.outer_read,
                subquery_cache: &self.exec_state.subquery_cache,
                host_functions: &self.host_functions,
                outer: &[],
                current: shape,
                aggregates: &[],
                windows: &[],
                window_row: 0,
                budget,
                ctes: None,
            };
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
            let ctx = EvalContext {
                exec: self.exec(),
                parameters: &self.parameters,
                upsert_excluded: &self.upsert_excluded,
                trigger_frames: &self.trigger_frames,
                outer_read: &self.exec_state.outer_read,
                subquery_cache: &self.exec_state.subquery_cache,
                host_functions: &self.host_functions,
                outer: &[],
                current: shape,
                aggregates: &[],
                windows: &[],
                window_row: 0,
                budget,
                ctes: None,
            };
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
            let columns = match self.exec().recursive_start(entry)? {
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
        let (names, _) = engine.exec().view_output(view)?;
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
        assert!(!engine.exec_state.index_probes.borrow().is_empty(), "an integral REAL key must still use the index probe");
    }
}
