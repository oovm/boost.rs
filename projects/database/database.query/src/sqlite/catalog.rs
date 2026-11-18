//! SQLite catalog: the parsed schema-object shapes and the catalog container
//! that owns them, including the `sqlite_master` replay shared by open and
//! DDL.

use std::collections::{BTreeMap, BTreeSet};

use database_formats::sqlite::WriteValue;
use database_storage::sqlite::{Collation, ColumnSpec, SchemaObject, apply_write_affinity};

use super::{
    bind::{ResolvedExpr, TargetShape, resolve_checks, resolve_foreign_keys},
    error::SqlError,
    frontend::{CheckConstraint, Expr, ForeignKey, ForeignKeyAction, Statement, TableConstraint, TriggerEvent, TriggerTiming, parse},
    value::{Cell, decode_cells, write_value_cell},
};

/// One table-level `PRIMARY KEY` or `UNIQUE` constraint resolved against the
/// declared columns, in written order.
#[derive(Clone)]
pub struct TableConstraintDef {
    /// `true` for `PRIMARY KEY`, `false` for `UNIQUE`.
    pub primary: bool,
    /// Key columns in key order.
    pub columns: Vec<usize>,
}

/// One table in the catalog: declared columns, keys, constraints and storage
/// shape, resolved once at `CREATE TABLE` or replay time.
#[derive(Clone)]
pub struct TableDef {
    /// Name exactly as written in `CREATE TABLE`.
    pub name: String,
    /// Declared columns in declaration order.
    pub columns: Vec<ColumnSpec>,
    /// Position of the column that aliases the implicit rowid.
    pub rowid_alias: Option<usize>,
    /// A table-level `PRIMARY KEY (a, b)` clause created an automatic unique
    /// index that has to be materialized after inserts.
    pub primary_key_index: bool,
    /// Column positions of a table-level `PRIMARY KEY (a, b)`, in key order.
    /// Empty for a rowid alias or a table without a table-level key.
    pub primary_key: Vec<usize>,
    /// Table-level `PRIMARY KEY` / `UNIQUE` clauses in written order, so
    /// automatic index numbers match `sqlite_autoindex_<table>_<n>`.
    pub table_constraints: Vec<TableConstraintDef>,
    /// `CHECK` constraints, table- and column-level, in declaration order.
    pub checks: Vec<CheckDef>,
    /// `REFERENCES` clauses, table- and column-level, in declaration order.
    pub foreign_keys: Vec<ForeignKeyDef>,
    /// `STRICT` table option was present, so every non-NULL value has to
    /// conform to its column's declared type after affinity.
    pub strict: bool,
    /// The table is stored as a primary-key index b-tree and has no implicit
    /// `rowid`.
    pub without_rowid: bool,
    /// The table belongs to the temp schema and is not stored in the image.
    pub temp: bool,
}

/// One `CREATE TEMP TABLE` and its in-memory rows.
pub struct TempTable {
    /// Resolved table definition of the temp table.
    pub def: TableDef,
    /// `sqlite_temp_master.sql` text: the statement without `TEMP`, exactly
    /// like SQLite stores it.
    pub sql: String,
    /// Root page of the temp b-tree, allocated the same way SQLite's temp
    /// database allocates pages.
    pub rootpage: u32,
    /// `sqlite_temp_master.rowid`, a monotonic insertion counter.
    pub master_rowid: i64,
    /// Declared column values keyed by rowid, in ascending rowid order.
    pub rows: BTreeMap<i64, Vec<Cell>>,
}

/// One resolved `REFERENCES` clause of a table.
#[derive(Clone)]
pub struct ForeignKeyDef {
    /// Child column positions in key order.
    pub columns: Vec<usize>,
    /// Lowercase parent table name.
    pub parent: String,
    /// Parent table name as written for diagnostics.
    pub parent_display: String,
    /// Parent column names as written; empty means the parent primary key.
    pub parent_columns: Vec<String>,
    /// `ON DELETE` action, spelled for `PRAGMA foreign_key_list`.
    pub on_delete: String,
    /// `ON UPDATE` action, spelled for `PRAGMA foreign_key_list`.
    pub on_update: String,
    /// `ON DELETE` action as parsed, applied when parent rows are removed.
    pub on_delete_action: ForeignKeyAction,
    /// `ON UPDATE` action as parsed, applied when parent keys are rewritten.
    pub on_update_action: ForeignKeyAction,
    /// `DEFERRABLE INITIALLY DEFERRED`: verification waits for `COMMIT` while
    /// an explicit transaction is open.
    pub deferred: bool,
}

impl TableDef {
    /// Column positions of this table's primary key, or `None` when it has no
    /// primary key at all.
    pub fn primary_key_positions(&self) -> Option<Vec<usize>> {
        if let Some(alias) = self.rowid_alias {
            return Some(vec![alias]);
        }
        if !self.primary_key.is_empty() {
            return Some(self.primary_key.clone());
        }
        let declared: Vec<usize> =
            self.columns.iter().enumerate().filter(|(_, column)| column.primary_key).map(|(position, _)| position).collect();
        if declared.is_empty() { None } else { Some(declared) }
    }

    /// Record layout of a WITHOUT ROWID table: stored record position to
    /// declared column position. SQLite stores the primary-key columns first,
    /// then the remaining columns in declaration order.
    pub fn without_rowid_layout(&self) -> Vec<usize> {
        let mut layout = self.primary_key_positions().unwrap_or_default();
        for position in 0..self.columns.len() {
            if !layout.contains(&position) {
                layout.push(position);
            }
        }
        layout
    }

    /// Decodes one WITHOUT ROWID record and maps it back to declaration order.
    pub fn decode_without_rowid(&self, payload: &[u8]) -> Result<Vec<Cell>, SqlError> {
        let layout = self.without_rowid_layout();
        let mut values = decode_cells(payload)?;
        // A record written before `ALTER TABLE ADD COLUMN` is short; the
        // missing trailing record slots read as their declared defaults.
        while values.len() < layout.len() {
            let position = layout[values.len()];
            let column = &self.columns[position];
            let value = column.default_value.clone().unwrap_or(WriteValue::Null);
            values.push(write_value_cell(&apply_write_affinity(&column.declared_type, value)));
        }
        values.truncate(layout.len());
        let mut cells = vec![Cell::Null; self.columns.len()];
        for (record_position, declared_position) in layout.iter().enumerate() {
            cells[*declared_position] = values[record_position].clone();
        }
        Ok(cells)
    }
}

/// One resolved `CHECK` constraint of a table.
#[derive(Clone)]
pub struct CheckDef {
    /// `CONSTRAINT name CHECK (...)` name, echoed instead of the expression.
    pub name: Option<String>,
    /// Expression over the table's declared columns.
    pub expr: ResolvedExpr,
    /// Source text of the expression for the failure message.
    pub text: String,
}

/// One index in the engine catalog, keyed by its lowercase name.
#[derive(Clone)]
pub struct IndexDef {
    /// Index name as written, used by `PRAGMA index_list`.
    pub name: String,
    /// Lowercase name of the indexed table.
    pub table: String,
    /// Indexed column positions, in key order. Empty for a managed index
    /// whose keys are expression results or filtered by a predicate.
    pub columns: Vec<usize>,
    /// The index enforces uniqueness.
    pub unique: bool,
    /// Creation counter, so `PRAGMA index_list` reports newest first.
    pub order: u64,
    /// The keys are computed by the engine (expression, `COLLATE` or partial
    /// index), not derived from column positions.
    pub managed: bool,
    /// Key terms resolved against the table, evaluated per row.
    pub terms: Vec<ResolvedExpr>,
    /// Partial-index predicate resolved against the table.
    pub predicate: Option<ResolvedExpr>,
    /// 每个键项生效的排序规则，来自索引项显式 `COLLATE` 或列声明。
    pub collations: Vec<Collation>,
}

/// One view in the engine catalog, keyed by its lowercase name.
#[derive(Clone)]
pub struct ViewDef {
    /// View name as written, used for diagnostics.
    pub name: String,
    /// Optional `CREATE VIEW v(a, b)` list that renames the query output.
    pub columns: Option<Vec<String>>,
    /// Parsed query the view re-evaluates on every read.
    pub query: Statement,
}

/// One `CREATE TEMP VIEW` in the temp schema, kept beside the temp tables so
/// the connection owns it and `sqlite_temp_master` reports it.
pub struct TempView {
    /// Resolved view definition of the temp view.
    pub def: ViewDef,
    /// `sqlite_temp_master.sql` text, the statement without `TEMP`.
    pub sql: String,
    /// `sqlite_temp_master.rowid`, a monotonic insertion counter.
    pub master_rowid: i64,
}

/// One trigger in the engine catalog.
#[derive(Clone)]
pub struct TriggerDef {
    /// Trigger name as written, used for diagnostics.
    pub name: String,
    /// Lowercase table or view the trigger is attached to.
    pub table: String,
    /// `BEFORE`, `AFTER` or `INSTEAD OF`.
    pub timing: TriggerTiming,
    /// Statement event that fires the trigger.
    pub event: TriggerEvent,
    /// Optional `WHEN` predicate, evaluated against `NEW` / `OLD`.
    pub when: Option<Expr>,
    /// Body statements run in order.
    pub body: Vec<Statement>,
    /// Monotonic creation counter. SQLite fires the triggers of one event in
    /// reverse creation order, so the newest trigger runs first.
    pub order: u64,
    /// The trigger belongs to the temp schema and only appears in
    /// `sqlite_temp_master`.
    pub temp: bool,
    /// `sqlite_master.sql` / `sqlite_temp_master.sql` text.
    pub sql: String,
    /// `sqlite_temp_master.rowid`; zero for a main trigger.
    pub master_rowid: i64,
}

/// The catalog container: every schema object the engine resolves names
/// against, plus the temp-table allocator state.
#[derive(Default)]
pub struct SqliteCatalog {
    /// Base tables in creation order.
    pub tables: Vec<TableDef>,
    /// Lowercase table name to its position in [`SqliteCatalog::tables`].
    pub table_lookup: BTreeMap<String, usize>,
    /// Indexes keyed by lowercase name.
    pub indexes: BTreeMap<String, IndexDef>,
    /// Creation counter handed to each new explicit index, so `PRAGMA
    /// index_list` lists the newest first like SQLite.
    pub index_sequence: u64,
    /// Views keyed by lowercase name. A view shares the object namespace with
    /// tables and indexes and re-evaluates its query on every read.
    pub views: BTreeMap<String, ViewDef>,
    /// `CREATE TEMP VIEW` definitions keyed by lowercase name. They shadow an
    /// equally named main view and disappear with the connection.
    pub temp_views: BTreeMap<String, TempView>,
    /// Triggers keyed by lowercase name, shared with the other object names.
    pub triggers: BTreeMap<String, TriggerDef>,
    /// Creation counter handed to each new trigger, so firing order follows
    /// SQLite's newest-first rule.
    pub trigger_sequence: u64,
    /// `CREATE TEMP TABLE` definitions and their rows. Temp tables live in a
    /// separate schema that disappears with the connection, so they never
    /// touch the persistent image or `sqlite_master`.
    pub temp_tables: Vec<TempTable>,
    /// Lowercase temp table name to its position in [`SqliteCatalog::temp_tables`].
    /// Lookups search this map before [`SqliteCatalog::table_lookup`], matching
    /// SQLite's schema search order.
    pub temp_lookup: BTreeMap<String, usize>,
    /// First unallocated page of the temp database, starting at 2 like every
    /// fresh SQLite database.
    pub temp_next_page: u32,
    /// Pages freed by `DROP TABLE` and reusable by the next temp object.
    pub temp_free_pages: BTreeSet<u32>,
    /// Next `sqlite_temp_master.rowid`, monotonic like SQLite's.
    pub temp_next_rowid: i64,
    /// The temp database was materialized once, so `PRAGMA database_list`
    /// keeps listing it even after its last object was dropped.
    pub temp_created: bool,
}

impl SqliteCatalog {
    /// Resolves one parsed `CREATE TABLE` into a catalog entry without
    /// writing storage, shared by the DDL path and the reopen path.
    pub fn table_entry(
        name: String,
        mut columns: Vec<ColumnSpec>,
        primary_key: Vec<String>,
        table_constraints: Vec<TableConstraint>,
        checks: Vec<CheckConstraint>,
        foreign_keys: Vec<ForeignKey>,
        strict: bool,
        without_rowid: bool,
    ) -> Result<TableDef, SqlError> {
        if strict {
            // SQLite limits STRICT columns to its five storage classes plus
            // `ANY`; the message quotes the declared text exactly as written.
            for column in &columns {
                let declared = column.declared_type.to_ascii_uppercase();
                if !matches!(declared.as_str(), "INT" | "INTEGER" | "REAL" | "TEXT" | "BLOB" | "ANY") {
                    return Err(SqlError::new(
                        "unknown-datatype",
                        format!("unknown datatype for {}.{}: \"{}\"", name, column.name, column.declared_type),
                    )
                    .in_statement("create-table"));
                }
            }
        }
        let mut key_positions = Vec::with_capacity(primary_key.len());
        for column in &primary_key {
            let position = columns.iter().position(|existing| existing.name.eq_ignore_ascii_case(column)).ok_or_else(|| {
                SqlError::new("unknown-column", format!("unknown column `{column}` in table `{name}`")).in_statement("create-table")
            })?;
            key_positions.push(position);
        }
        let mut resolved_constraints = Vec::with_capacity(table_constraints.len());
        for constraint in table_constraints {
            let mut positions = Vec::with_capacity(constraint.columns().len());
            for column in constraint.columns() {
                let position = columns
                    .iter()
                    .position(|existing| existing.name.eq_ignore_ascii_case(column))
                    .ok_or_else(|| SqlError::new("unknown-column", format!("no such column: {column}")).in_statement("create-table"))?;
                positions.push(position);
            }
            resolved_constraints
                .push(TableConstraintDef { primary: matches!(constraint, TableConstraint::PrimaryKey { .. }), columns: positions });
        }
        if without_rowid && resolved_constraints.iter().any(|constraint| !constraint.primary) {
            return Err(SqlError::new("unsupported-clause", "UNIQUE constraints on WITHOUT ROWID tables are not supported")
                .in_statement("create-table"));
        }
        if without_rowid {
            // A WITHOUT ROWID table needs a primary key: either the table-level
            // clause or exactly one inline `PRIMARY KEY`. The key columns are
            // implicitly NOT NULL and are stored first in every record.
            let inline: Vec<usize> =
                columns.iter().enumerate().filter(|(_, column)| column.primary_key).map(|(position, _)| position).collect();
            if !primary_key.is_empty() && !inline.is_empty() {
                return Err(SqlError::new("SQLITE_ERROR", format!("table {name} has more than one primary key")).in_statement("create-table"));
            }
            if key_positions.is_empty() {
                if inline.len() > 1 {
                    return Err(
                        SqlError::new("SQLITE_ERROR", format!("table {name} has more than one primary key")).in_statement("create-table")
                    );
                }
                key_positions = inline;
            }
            if key_positions.is_empty() {
                return Err(SqlError::new("SQLITE_ERROR", format!("PRIMARY KEY missing on table {name}")).in_statement("create-table"));
            }
            for position in &key_positions {
                columns[*position].not_null = true;
            }
        }
        // A lone INTEGER key column is the rowid alias whether the key is
        // declared inline or as a table constraint.
        let rowid_alias = if without_rowid {
            None
        }
        else {
            match key_positions.as_slice() {
                [position] if columns[*position].declared_type.eq_ignore_ascii_case("INTEGER") => Some(*position),
                _ => columns.iter().position(|column| column.primary_key && column.declared_type.eq_ignore_ascii_case("INTEGER")),
            }
        };
        let primary_key_index = !without_rowid && !primary_key.is_empty() && rowid_alias.is_none();
        // Checks are resolved before the table exists, so an unknown column
        // fails the statement instead of leaving a half-built table behind,
        // exactly like upstream.
        let resolved_checks = resolve_checks(&name, &columns, &checks, "create-table", without_rowid)?;
        // A `FOREIGN KEY (...)` list naming a missing child column is rejected
        // at DDL time, exactly like SQLite; the parent tables may still be
        // created later.
        let resolved_foreign_keys = resolve_foreign_keys(&columns, &foreign_keys)?;
        Ok(TableDef {
            name,
            columns,
            rowid_alias,
            primary_key_index,
            primary_key: key_positions,
            table_constraints: resolved_constraints,
            checks: resolved_checks,
            foreign_keys: resolved_foreign_keys,
            strict,
            without_rowid,
            temp: false,
        })
    }

    /// Replays the stored `CREATE TABLE` / `CREATE INDEX` text of every
    /// schema row into the in-memory catalog without touching storage.
    pub fn load_schema(&mut self, objects: &[SchemaObject]) -> Result<(), SqlError> {
        for object in objects {
            let Some(sql) = object.sql.as_deref()
            else {
                continue;
            };
            let mut statements = parse(sql)?.into_iter();
            match statements.next() {
                Some(Statement::CreateTable {
                    name,
                    columns,
                    primary_key,
                    table_constraints,
                    checks,
                    foreign_keys,
                    strict,
                    without_rowid,
                    ..
                }) => {
                    let def =
                        Self::table_entry(name.clone(), columns, primary_key, table_constraints, checks, foreign_keys, strict, without_rowid)?;
                    let key = name.to_ascii_lowercase();
                    if !self.table_lookup.contains_key(&key) {
                        let position = self.tables.len();
                        self.table_lookup.insert(key, position);
                        self.tables.push(def);
                    }
                }
                Some(Statement::CreateIndex { name, table, unique, columns, predicate, .. }) => {
                    let Some(def) = self.tables.iter().find(|def| def.name.eq_ignore_ascii_case(&table))
                    else {
                        continue;
                    };
                    let managed = predicate.is_some() || columns.iter().any(|column| column.expr.is_some() || column.collate.is_some());
                    // 键项显式 `COLLATE` 优先，裸列继承列声明，表达式按 `BINARY`。
                    let collations: Vec<Collation> = columns
                        .iter()
                        .map(|column| match &column.collate {
                            Some(name) => Collation::parse(name).unwrap_or_default(),
                            None => {
                                match (&column.expr, def.columns.iter().find(|existing| existing.name.eq_ignore_ascii_case(&column.name))) {
                                    (None, Some(existing)) => existing.collate.unwrap_or_default(),
                                    _ => Collation::Binary,
                                }
                            }
                        })
                        .collect();
                    let (positions, terms, predicate) = if managed {
                        let shape = TargetShape::new(def, &table, None);
                        let mut terms = Vec::with_capacity(columns.len());
                        let mut failed = false;
                        for column in &columns {
                            let resolved = match &column.expr {
                                Some(expr) => match shape.resolve(expr, "create-index") {
                                    Ok(resolved) => resolved,
                                    Err(_) => {
                                        failed = true;
                                        break;
                                    }
                                },
                                None => match def.columns.iter().position(|existing| existing.name.eq_ignore_ascii_case(&column.name)) {
                                    Some(position) => ResolvedExpr::Column { source: 0, index: position },
                                    None => {
                                        failed = true;
                                        break;
                                    }
                                },
                            };
                            terms.push(resolved);
                        }
                        if failed {
                            continue;
                        }
                        let predicate = match &predicate {
                            Some(predicate) => match shape.resolve(predicate, "create-index") {
                                Ok(resolved) => Some(resolved),
                                Err(_) => continue,
                            },
                            None => None,
                        };
                        (Vec::new(), terms, predicate)
                    }
                    else {
                        let mut positions = Vec::with_capacity(columns.len());
                        for column in &columns {
                            match def.columns.iter().position(|existing| existing.name.eq_ignore_ascii_case(&column.name)) {
                                Some(position) => positions.push(position),
                                None => break,
                            }
                        }
                        if positions.len() != columns.len() {
                            continue;
                        }
                        (positions, Vec::new(), None)
                    };
                    let order = self.index_sequence;
                    self.index_sequence += 1;
                    self.indexes.insert(
                        name.to_ascii_lowercase(),
                        IndexDef {
                            name: name.clone(),
                            table: table.to_ascii_lowercase(),
                            columns: positions,
                            unique,
                            order,
                            managed,
                            terms,
                            predicate,
                            collations,
                        },
                    );
                }
                Some(Statement::CreateView { name, columns, query, .. }) => {
                    let key = name.to_ascii_lowercase();
                    if !self.table_lookup.contains_key(&key) && !self.views.contains_key(&key) {
                        self.views.insert(key, ViewDef { name, columns, query: *query });
                    }
                }
                Some(Statement::CreateTrigger { name, timing, event, table, when, body, .. }) => {
                    let key = name.to_ascii_lowercase();
                    if !self.triggers.contains_key(&key) {
                        let order = self.trigger_sequence;
                        self.trigger_sequence += 1;
                        self.triggers.insert(
                            key,
                            TriggerDef {
                                name,
                                table: table.to_ascii_lowercase(),
                                timing,
                                event,
                                when,
                                body,
                                order,
                                temp: false,
                                sql: sql.to_string(),
                                master_rowid: 0,
                            },
                        );
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}
