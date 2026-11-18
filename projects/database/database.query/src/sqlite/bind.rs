//! SQLite name and column resolution: the row-set shapes a `FROM` clause
//! produces, the scopes its columns resolve against, and the resolved
//! expression tree the evaluator consumes.
//!
//! Evaluation itself (`ResolvedExpr::eval`) stays with the facade until the
//! evaluation closure lands; this module owns only the resolution machinery
//! and the resolved shapes.

use database_storage::sqlite::{Collation, ColumnSpec};

use super::{
    catalog::{CheckDef, ForeignKeyDef, TableDef},
    error::SqlError,
    frontend::{
        AggregateFunction, BinaryOp, CastType, CheckConstraint, ColumnRef, CompareOp, Expr, ForeignKey, ForeignKeyAction, Frame, FrameBound,
        FrameEdge, FrameExclude, FrameUnits, JoinKind, Literal, NamedWindow, OrderKey, ScalarFunction, SelectItems, Statement, TableRef,
        UnaryOp, WindowFunction, WindowSpec,
    },
    value::{Cell, ProbeKey, literal_cell, probe_key},
};

/// Sentinel source indexes that mark a `NEW` / `OLD` reference in a resolved
/// expression. They never collide with a `FROM` source position.
pub const TRIGGER_SOURCE_NEW: usize = usize::MAX;
/// Sentinel source index of an `OLD` reference.
pub const TRIGGER_SOURCE_OLD: usize = usize::MAX - 1;
/// Pseudo source of `excluded.<column>` inside `ON CONFLICT DO UPDATE`.
pub const EXCLUDED_SOURCE: usize = usize::MAX - 2;

/// The `NEW` / `OLD` row one trigger body evaluates against.
pub struct TriggerFrame {
    /// Column names of the trigger's table, followed by `rowid`.
    pub columns: Vec<String>,
    /// Values of the row after the change; `None` for `DELETE`.
    pub new: Option<Vec<Cell>>,
    /// Values of the row before the change; `None` for `INSERT`.
    pub old: Option<Vec<Cell>>,
}

/// A resolved `RETURNING` projection.
pub struct ReturningPlan {
    /// Result column labels.
    pub columns: Vec<String>,
    /// Projection expressions resolved against the written row.
    pub items: Vec<ResolvedExpr>,
}

/// The single-source relation one `UPDATE` / `DELETE` resolves against.
pub struct TargetShape<'a> {
    /// The one-source relation column references bind against.
    pub relation: Relation<'a>,
    /// Flattened column catalog of that relation.
    pub scope: ScopeShape,
}

impl<'a> TargetShape<'a> {
    /// Builds the target relation and its scope catalog for one table.
    pub fn new(def: &'a TableDef, label: &str, trigger: Option<&'a TriggerFrame>) -> Self {
        let relation = target_relation(def, label, trigger);
        let scope = relation.shape();
        Self { relation, scope }
    }

    /// Resolves one expression against the target relation.
    pub fn resolve(&self, expr: &Expr, statement: &'static str) -> Result<ResolvedExpr, SqlError> {
        let mut windows = Vec::new();
        let mut aggregates = Vec::new();
        resolve_expr(expr, &self.relation, &[], &[], &mut windows, &mut aggregates, false, false).map_err(|error| error.in_statement(statement))
    }

    /// Resolves an optional expression, mapping `None` through.
    pub fn resolve_optional(&self, expr: Option<&Expr>, statement: &'static str) -> Result<Option<ResolvedExpr>, SqlError> {
        expr.map(|expr| self.resolve(expr, statement)).transpose()
    }

    /// Builds the `RETURNING` projection plan.
    pub fn plan(&self, def: &TableDef, items: &SelectItems, statement: &'static str) -> Result<ReturningPlan, SqlError> {
        match items {
            SelectItems::Star => Ok(ReturningPlan {
                columns: def.columns.iter().map(|column| column.name.clone()).collect(),
                items: (0..def.columns.len()).map(|index| ResolvedExpr::Column { source: 0, index }).collect(),
            }),
            SelectItems::Named(items) => Ok(ReturningPlan {
                columns: items.iter().map(|item| item.label()).collect(),
                items: items.iter().map(|item| self.resolve(item, statement)).collect::<Result<Vec<_>, _>>()?,
            }),
            SelectItems::CountAll => {
                Err(SqlError::new("unsupported-clause", "`RETURNING count(*)` is not supported").in_clause("RETURNING").in_statement(statement))
            }
        }
    }
}

/// One column of an enclosing `SELECT`, paired with the value of the row
/// currently being evaluated.
#[derive(Clone)]
pub struct ScopeColumn {
    /// Table label the column was written under.
    pub label: String,
    /// Column name as the schema spells it.
    pub name: String,
    /// Current value of that column in the enclosing row.
    pub value: Cell,
}

/// One enclosing `SELECT` row: every column it exposes and its current value.
#[derive(Clone)]
pub struct Scope {
    /// Exposed columns in merged order.
    pub columns: Vec<ScopeColumn>,
}

impl Scope {
    /// Flattens the row into a reusable static catalog.
    pub fn shape(&self) -> ScopeShape {
        ScopeShape { columns: self.columns.iter().map(|column| (column.label.clone(), column.name.clone())).collect() }
    }
}

/// Static column catalog of one `SELECT` core.
///
/// An inner expression resolves an outer reference to `(depth, index)` once;
/// evaluation then reads the value from the matching `Scope` of the current
/// outer row.
#[derive(Clone)]
pub struct ScopeShape {
    /// `(label, name)` pair per merged column.
    pub columns: Vec<(String, String)>,
}

impl ScopeShape {
    /// Pairs the catalog with one concrete outer row.
    pub fn instantiate(&self, values: &[Cell]) -> Scope {
        Scope {
            columns: self
                .columns
                .iter()
                .enumerate()
                .map(|(index, (label, name))| ScopeColumn {
                    label: label.clone(),
                    name: name.clone(),
                    value: values.get(index).cloned().unwrap_or(Cell::Null),
                })
                .collect(),
        }
    }
}

/// One whole-result aggregate to compute over the filtered rows.
pub struct AggregateSpec {
    /// Aggregate the spec computes.
    pub function: AggregateFunction,
    /// Single argument; `None` for `count(*)`.
    pub argument: Option<ResolvedExpr>,
    /// `group_concat` separator, evaluated per row like SQLite does.
    pub separator: Option<ResolvedExpr>,
    /// `DISTINCT` was written before the argument.
    pub distinct: bool,
    /// `FILTER (WHERE ...)` applied before the aggregate sees a row.
    pub filter: Option<ResolvedExpr>,
    /// `ORDER BY` inside the argument list, used by `group_concat`.
    pub order: Vec<(ResolvedExpr, SortDirection)>,
    /// `DISTINCT` 去重时使用的排序规则，取自参数表达式的生效排序规则。
    pub collation: Collation,
}

/// One table bound in a `FROM` clause.
pub struct Source<'a> {
    /// Label used by column references: the alias when written, else the table
    /// name. An unaliased derived table has the empty label.
    pub label: String,
    /// Column catalog: a base table's definition, or the materialized output
    /// names of a CTE / derived table.
    pub columns: SourceColumns<'a>,
    /// Index of this source's first column inside the merged row.
    pub offset: usize,
    /// How this source is joined to the ones before it; `None` means a plain
    /// product (comma or `CROSS JOIN`).
    pub join: Option<JoinKind>,
    /// Columns `SELECT *` must not expose: the right side of a `USING` /
    /// `NATURAL` join, whose value lives in the coalesced slot instead.
    pub star_hidden: Vec<bool>,
    /// Columns that cannot be named without a qualifier because a coalesced
    /// `USING` column owns the name.
    pub unqualified_hidden: Vec<bool>,
    /// For a column that owns a `USING` name, the coalesced slot `SELECT *`
    /// and unqualified references read instead.
    pub star_override: Vec<Option<usize>>,
}

/// Column catalog of one `FROM` source.
pub enum SourceColumns<'a> {
    /// Base table still read through the storage layer.
    Table(&'a TableDef),
    /// CTE, derived table or reserved system table whose rows were evaluated
    /// up front. `rowid` marks sources that carry a trailing hidden rowid slot.
    Materialized {
        /// Column labels in row order.
        names: Vec<String>,
        /// The materialized row carries an implicit rowid alias.
        rowid: bool,
    },
}

impl<'a> SourceColumns<'a> {
    /// Number of declared columns.
    pub fn width(&self) -> usize {
        match self {
            Self::Table(def) => def.columns.len(),
            Self::Materialized { names, .. } => names.len(),
        }
    }

    /// Name of one declared column.
    pub fn name_at(&self, local: usize) -> &str {
        match self {
            Self::Table(def) => &def.columns[local].name,
            Self::Materialized { names, .. } => &names[local],
        }
    }

    /// A base table is a rowid table, so each of its rows carries one extra,
    /// unnamed slot holding the rowid. A WITHOUT ROWID table has none, and
    /// derived tables and CTEs never do, while `sqlite_master` and
    /// `sqlite_sequence` do.
    pub fn hidden_rowid(&self) -> bool {
        match self {
            Self::Table(def) => !def.without_rowid,
            Self::Materialized { rowid, .. } => *rowid,
        }
    }
}

/// SQLite exposes the implicit integer primary key under three interchangeable
/// names, unless a declared column shadows that name.
pub fn is_rowid_alias(name: &str) -> bool {
    ["rowid", "_rowid_", "oid"].iter().any(|alias| alias.eq_ignore_ascii_case(name))
}

impl<'a> Source<'a> {
    /// Width of this source inside the merged row, hidden rowid included.
    pub fn width(&self) -> usize {
        self.columns.width() + usize::from(self.columns.hidden_rowid())
    }

    /// Width of the columns `SELECT *` exposes.
    pub fn visible_width(&self) -> usize {
        self.columns.width()
    }

    /// Merged index of one named column, rowid aliases included.
    pub fn index(&self, name: &str) -> Option<usize> {
        if let Some(local) = (0..self.visible_width()).find(|local| self.columns.name_at(*local).eq_ignore_ascii_case(name)) {
            return Some(self.offset + local);
        }
        // A declared column with the same name shadows the implicit rowid.
        if self.columns.hidden_rowid() && is_rowid_alias(name) {
            return Some(self.offset + self.visible_width());
        }
        None
    }

    /// Resolves an unqualified reference, skipping columns a coalesced `USING`
    /// slot owns.
    pub fn unqualified_index(&self, name: &str) -> Option<usize> {
        if let Some(local) =
            (0..self.visible_width()).find(|local| !self.unqualified_hidden[*local] && self.columns.name_at(*local).eq_ignore_ascii_case(name))
        {
            return Some(self.offset + local);
        }
        if self.columns.hidden_rowid() && is_rowid_alias(name) {
            return Some(self.offset + self.visible_width());
        }
        None
    }
}

/// One `USING` / `NATURAL` column pair of a join.
#[derive(Clone)]
pub struct UsingJoin {
    /// Merged index of the left operand: the previous coalesced slot for this
    /// name, or a source column.
    pub left: usize,
    /// Merged index of the right source's column.
    pub right: usize,
    /// Merged index of the coalesced slot appended after all sources.
    pub slot: usize,
}

/// One coalesced slot, in slot order. Its merged index is the source width
/// plus the position in this vector.
pub struct UsingSlot {
    /// Coalesced column name.
    pub name: String,
    /// Label reported by ambiguity errors and outer references.
    pub label: String,
    /// Source owning the coalesced value.
    pub source: usize,
    /// False once a later join extends the same name; the newest slot owns it.
    pub nameable: bool,
    /// Source column `SELECT *` prints at the owner's position.
    pub owner: Option<(usize, usize)>,
}

/// Current left operand of a `USING` name: an earlier coalesced slot or a
/// source column.
pub enum UsingOperand {
    /// Earlier coalesced slot, by position.
    Slot(usize),
    /// Source column by source and local position.
    Column {
        /// Source index.
        source: usize,
        /// Column position inside that source.
        local: usize,
    },
}

/// The sources of one `FROM` clause, flattened into a single row shape.
pub struct Relation<'a> {
    /// Bound sources in `FROM` order.
    pub sources: Vec<Source<'a>>,
    /// Per source: the coalesced pairs its join introduced.
    pub using: Vec<Vec<UsingJoin>>,
    /// Coalesced slots, appended after every source column.
    pub using_slots: Vec<UsingSlot>,
    /// `NEW` / `OLD` frame of the trigger body being resolved, if any. A
    /// `new.` or `old.` reference binds here instead of to a `FROM` source.
    pub trigger: Option<&'a TriggerFrame>,
    /// `excluded.` references bind to the supplied row of an upsert.
    pub excluded: bool,
}

impl<'a> Relation<'a> {
    /// Width of the merged row: every source column, in `FROM` order, then the
    /// coalesced `USING` slots.
    pub fn columns(&self) -> usize {
        self.base_width() + self.using_slots.len()
    }

    /// Width of the merged row before the coalesced slots.
    pub fn base_width(&self) -> usize {
        self.sources.last().map_or(0, |source| source.offset + source.width())
    }

    /// Resolves one reference to `(source index, merged row index)`.
    pub fn resolve(&self, reference: &ColumnRef) -> Result<(usize, usize), SqlError> {
        if let Some(qualifier) = &reference.qualifier {
            if qualifier.eq_ignore_ascii_case("new") || qualifier.eq_ignore_ascii_case("old") {
                return self.resolve_trigger(reference, qualifier);
            }
            if qualifier.eq_ignore_ascii_case("excluded") && self.excluded {
                let source = self
                    .sources
                    .first()
                    .ok_or_else(|| SqlError::new("unknown-table", "no table to bind `excluded` against").in_statement("insert"))?;
                let index = source.index(&reference.name).ok_or_else(|| {
                    SqlError::new("unknown-column", format!("no such column: excluded.{}", reference.name)).in_statement("insert")
                })?;
                return Ok((EXCLUDED_SOURCE, index));
            }
            let at = match self.sources.iter().position(|source| source.label.eq_ignore_ascii_case(qualifier)) {
                Some(at) => at,
                // Outside an upsert, `excluded` is not a pseudo table: SQLite
                // reports the qualified name as a missing column.
                None if qualifier.eq_ignore_ascii_case("excluded") => {
                    return Err(SqlError::new("unknown-column", format!("no such column: excluded.{}", reference.name)).in_statement("select"));
                }
                None => {
                    return Err(
                        SqlError::new("unknown-table", format!("unknown table `{qualifier}` in the `FROM` clause")).in_statement("select")
                    );
                }
            };
            let source = &self.sources[at];
            let index = source.index(&reference.name).ok_or_else(|| {
                SqlError::new("unknown-column", format!("unknown column `{}` in table `{}`", reference.display(), source.label))
                    .in_statement("select")
            })?;
            return Ok((at, index));
        }
        // A coalesced `USING` column owns its name; the newest slot wins.
        let mut matches: Vec<(usize, usize, String)> = Vec::new();
        for (position, slot) in self.using_slots.iter().enumerate().rev() {
            if slot.nameable && slot.name.eq_ignore_ascii_case(&reference.name) {
                matches.push((slot.source, self.base_width() + position, slot.label.clone()));
                break;
            }
        }
        for (at, source) in self.sources.iter().enumerate() {
            if let Some(index) = source.unqualified_index(&reference.name) {
                matches.push((at, index, source.label.clone()));
            }
        }
        match matches.len() {
            0 => Err(self.unknown_column(reference)),
            1 => Ok((matches[0].0, matches[0].1)),
            _ => {
                let left = &matches[0].2;
                let right = &matches[1].2;
                Err(SqlError::new(
                    "ambiguous-column",
                    format!("ambiguous column `{}`: it appears in tables `{left}` and `{right}`", reference.name),
                )
                .in_statement("select"))
            }
        }
    }

    /// Name of one resolved reference as the schema spells it, used for
    /// result-column labels. `None` for rowid aliases, `USING` slots and
    /// trigger frames, where the written name is reported instead.
    pub fn resolved_column_name(&self, reference: &ColumnRef) -> Option<String> {
        let (source, index) = self.resolve(reference).ok()?;
        if source >= self.sources.len() {
            return None;
        }
        let offset = self.sources[source].offset;
        let local = index.checked_sub(offset)?;
        if local < self.sources[source].visible_width() { Some(self.sources[source].columns.name_at(local).to_string()) } else { None }
    }

    /// Resolves an `new.col` / `old.col` reference against the active trigger
    /// frame. A side the event does not provide reports the same
    /// `no such column` error SQLite raises.
    pub fn resolve_trigger(&self, reference: &ColumnRef, qualifier: &str) -> Result<(usize, usize), SqlError> {
        let old = qualifier.eq_ignore_ascii_case("old");
        if let Some(frame) = self.trigger {
            let available = if old { frame.old.is_some() } else { frame.new.is_some() };
            if available {
                if let Some(index) = frame.columns.iter().position(|column| column.eq_ignore_ascii_case(&reference.name)) {
                    return Ok((if old { TRIGGER_SOURCE_OLD } else { TRIGGER_SOURCE_NEW }, index));
                }
            }
        }
        Err(SqlError::new("unknown-column", format!("no such column: {}.{}", qualifier, reference.name)).in_statement("select"))
    }

    /// Error text for a column that resolves nowhere, phrased by scope size
    /// exactly like SQLite does.
    pub fn unknown_column(&self, reference: &ColumnRef) -> SqlError {
        let scope = match self.sources.len() {
            // SQLite reports `no such column`; a projection without `FROM` has
            // no scope at all, so the message must not name a `FROM` clause.
            0 => "the `SELECT` clause".to_string(),
            1 => {
                let label = &self.sources[0].label;
                if label.is_empty() { "the derived table".to_string() } else { format!("table `{label}`") }
            }
            _ => "the `FROM` clause".to_string(),
        };
        SqlError::new("unknown-column", format!("unknown column `{}` in {scope}", reference.display())).in_statement("select")
    }

    /// Builds the coalesced slots for `USING` / `NATURAL` joins. Runs before
    /// any expression is resolved, so unqualified references bind to them.
    pub fn plan_joins(&mut self, from: &[TableRef]) -> Result<(), SqlError> {
        for index in 0..self.sources.len() {
            let reference = &from[index];
            let mut names: Vec<String> = Vec::new();
            let raw = if reference.natural { self.natural_columns(index) } else { reference.using.clone().unwrap_or_default() };
            for name in raw {
                if !names.iter().any(|existing| existing.eq_ignore_ascii_case(&name)) {
                    names.push(name);
                }
            }
            for name in names {
                let right_local = (0..self.sources[index].visible_width())
                    .find(|local| self.sources[index].columns.name_at(*local).eq_ignore_ascii_case(&name))
                    .ok_or_else(|| {
                        SqlError::new("unknown-column", format!("cannot join using column {name} - column not present in both tables"))
                            .in_statement("select")
                    })?;
                let left = match self.using_operand(&name, index) {
                    Some(operand) => operand,
                    None => {
                        return Err(SqlError::new(
                            "unknown-column",
                            format!("cannot join using column {name} - column not present in both tables"),
                        )
                        .in_statement("select"));
                    }
                };
                let (left_index, label, owner, source, matched) = match left {
                    UsingOperand::Slot(position) => {
                        let slot = &self.using_slots[position];
                        let (label, owner, source, matched) = (slot.label.clone(), slot.owner, slot.source, slot.name.clone());
                        self.using_slots[position].nameable = false;
                        if let Some((owner_source, owner_local)) = owner {
                            self.sources[owner_source].star_override[owner_local] = Some(self.base_width() + self.using_slots.len());
                        }
                        (self.base_width() + position, label, owner, source, matched)
                    }
                    UsingOperand::Column { source, local } => {
                        self.sources[source].unqualified_hidden[local] = true;
                        self.sources[source].star_override[local] = Some(self.base_width() + self.using_slots.len());
                        let matched = self.sources[source].columns.name_at(local).to_string();
                        (self.sources[source].offset + local, self.sources[source].label.clone(), Some((source, local)), source, matched)
                    }
                };
                let slot = self.base_width() + self.using_slots.len();
                self.sources[index].star_hidden[right_local] = true;
                self.sources[index].unqualified_hidden[right_local] = true;
                self.using[index].push(UsingJoin { left: left_index, right: self.sources[index].offset + right_local, slot });
                self.using_slots.push(UsingSlot { name: matched, label, source, nameable: true, owner });
            }
        }
        Ok(())
    }

    /// Finds the current left operand for one `USING` column name among the
    /// sources joined so far.
    pub fn using_operand(&self, name: &str, before: usize) -> Option<UsingOperand> {
        if let Some(position) = self.using_slots.iter().rposition(|slot| slot.nameable && slot.name.eq_ignore_ascii_case(name)) {
            return Some(UsingOperand::Slot(position));
        }
        for (source, item) in self.sources[..before].iter().enumerate() {
            for local in 0..item.visible_width() {
                if !item.unqualified_hidden[local] && item.columns.name_at(local).eq_ignore_ascii_case(name) {
                    return Some(UsingOperand::Column { source, local });
                }
            }
        }
        None
    }

    /// Shared column names for `NATURAL JOIN`, in right-source order.
    pub fn natural_columns(&self, index: usize) -> Vec<String> {
        let right = &self.sources[index];
        let mut names = Vec::new();
        for local in 0..right.visible_width() {
            let name = right.columns.name_at(local);
            if self.using_operand(name, index).is_some() && !names.iter().any(|existing: &String| existing.eq_ignore_ascii_case(name)) {
                names.push(name.to_string());
            }
        }
        names
    }

    /// Column catalog of the merged row: `(label, declared name)` per column.
    pub fn shape(&self) -> ScopeShape {
        let mut columns = Vec::with_capacity(self.columns());
        for source in &self.sources {
            for local in 0..source.visible_width() {
                if source.star_hidden[local] || source.unqualified_hidden[local] {
                    columns.push((source.label.clone(), String::new()));
                }
                else {
                    columns.push((source.label.clone(), source.columns.name_at(local).to_string()));
                }
            }
            if source.columns.hidden_rowid() {
                // The hidden rowid slot has no name, so no identifier can bind
                // to it; it only has to keep merged-row indexes aligned.
                columns.push((source.label.clone(), String::new()));
            }
        }
        for slot in &self.using_slots {
            if slot.nameable {
                columns.push((slot.label.clone(), slot.name.clone()));
            }
            else {
                columns.push((String::new(), String::new()));
            }
        }
        ScopeShape { columns }
    }

    /// Column labels `SELECT *` exposes.
    pub fn star_columns(&self) -> Vec<String> {
        let mut names = Vec::new();
        for source in &self.sources {
            for local in 0..source.visible_width() {
                if !source.star_hidden[local] {
                    names.push(source.columns.name_at(local).to_string());
                }
            }
        }
        names
    }

    /// Drops the hidden rowid slots and right-side `USING` columns from one
    /// merged row for `SELECT *`, printing the coalesced value where a join
    /// folded a column into its left neighbour.
    pub fn visible_row(&self, row: &[Cell]) -> Vec<Cell> {
        let mut visible = Vec::with_capacity(self.star_columns().len());
        for source in &self.sources {
            for local in 0..source.visible_width() {
                if source.star_hidden[local] {
                    continue;
                }
                match source.star_override[local] {
                    Some(slot) => visible.push(row[slot].clone()),
                    None => visible.push(row[source.offset + local].clone()),
                }
            }
        }
        visible
    }

    /// 合并行某一列声明的排序规则。`USING` 合并槽取左侧列的规则，rowid 空槽
    /// 与派生表、CTE 一律按 `BINARY`。
    pub fn column_collation(&self, merged: usize) -> Collation {
        for source in &self.sources {
            let end = source.offset + source.width();
            if merged >= source.offset && merged < end {
                if merged >= source.offset + source.visible_width() {
                    return Collation::Binary;
                }
                return match &source.columns {
                    SourceColumns::Table(def) => def.columns[merged - source.offset].collate.unwrap_or_default(),
                    SourceColumns::Materialized { .. } => Collation::Binary,
                };
            }
        }
        for join in self.using.iter().flatten() {
            if join.slot == merged {
                return self.column_collation(join.left);
            }
        }
        Collation::Binary
    }

    /// `SELECT *` 输出列各自的排序规则。
    pub fn visible_collations(&self) -> Vec<Collation> {
        let mut collations = Vec::new();
        for source in &self.sources {
            for local in 0..source.visible_width() {
                if source.star_hidden[local] {
                    continue;
                }
                collations.push(match source.star_override[local] {
                    Some(slot) => self.column_collation(slot),
                    None => self.column_collation(source.offset + local),
                });
            }
        }
        collations
    }
}

/// A projection / `WHERE` / `ORDER BY` expression with every column resolved
/// to a merged row index.
#[derive(Clone, PartialEq)]
pub enum ResolvedExpr {
    /// Value of a bound parameter placeholder, or `NULL` when the host did not
    /// supply one for that slot.
    Parameter(
        /// Placeholder index, one-based in written order.
        usize,
    ),
    /// Literal value.
    Cell(
        /// The literal cell.
        Cell,
    ),
    /// Bound column: owning source plus merged row index.
    Column {
        /// Source that owns the column.
        source: usize,
        /// Index inside the merged row.
        index: usize,
    },
    /// `new.col` / `old.col` of the trigger body currently executing.
    TriggerColumn {
        /// Read the `OLD` row instead of `NEW`.
        old: bool,
        /// Column position inside the trigger frame.
        index: usize,
    },
    /// `excluded.col` of an `ON CONFLICT DO UPDATE` clause: the supplied row.
    ExcludedColumn {
        /// Column position inside the supplied row.
        index: usize,
    },
    /// Column borrowed from an enclosing scope: `depth` counts outward from the
    /// current core, `index` addresses that scope's flattened row.
    OuterColumn {
        /// Outward nesting depth from the current core.
        depth: usize,
        /// Index inside that scope's flattened row.
        index: usize,
    },
    /// Whole-result aggregate value already computed for this row.
    AggregateRef(
        /// Position in the core's aggregate list.
        usize,
    ),
    /// Prefix `-` or `+`.
    Unary {
        /// The operator.
        op: UnaryOp,
        /// The operand.
        value: Box<ResolvedExpr>,
    },
    /// Arithmetic `+`, `-`, `*` or `/`.
    Binary {
        /// Left operand.
        left: Box<ResolvedExpr>,
        /// The operator.
        op: BinaryOp,
        /// Right operand.
        right: Box<ResolvedExpr>,
    },
    /// Value comparison; yields `1`, `0` or `NULL`.
    ///
    /// `collation` 是两个操作数合并后生效的排序规则，解析期确定，求值期只按
    /// 它比较文本。
    Compare {
        /// Left operand.
        left: Box<ResolvedExpr>,
        /// The comparison operator.
        op: CompareOp,
        /// Right operand.
        right: Box<ResolvedExpr>,
        /// Collation governing text comparison.
        collation: Collation,
    },
    /// `IS NULL` or `IS NOT NULL`.
    IsNull {
        /// Tested operand.
        value: Box<ResolvedExpr>,
        /// `true` for `IS NOT NULL`.
        negated: bool,
    },
    /// `IS TRUE`, `IS NOT TRUE`, `IS FALSE` or `IS NOT FALSE`.
    IsTruth {
        /// Tested operand.
        value: Box<ResolvedExpr>,
        /// `true` tests `TRUE`, `false` tests `FALSE`.
        truth: bool,
        /// `true` for the `NOT` form.
        negated: bool,
    },
    /// `NOT`, with three-valued logic.
    Not(
        /// The negated operand.
        Box<ResolvedExpr>,
    ),
    /// `AND`, with three-valued logic.
    And(
        /// Left operand.
        Box<ResolvedExpr>,
        /// Right operand.
        Box<ResolvedExpr>,
    ),
    /// `OR`, with three-valued logic.
    Or(
        /// Left operand.
        Box<ResolvedExpr>,
        /// Right operand.
        Box<ResolvedExpr>,
    ),
    /// `CASE` in both the searched and the simple form.
    Case {
        /// Simple-form operand; absent in the searched form.
        operand: Option<Box<ResolvedExpr>>,
        /// 简单形式里操作数生效的排序规则；分支自带显式 `COLLATE` 时优先。
        collation: Option<Collation>,
        /// `WHEN condition THEN result` pairs.
        branches: Vec<(ResolvedExpr, ResolvedExpr)>,
        /// `ELSE result`; absent means `NULL`.
        otherwise: Option<Box<ResolvedExpr>>,
    },
    /// `x IN (...)` or `x NOT IN (...)`.
    In {
        /// Tested operand.
        value: Box<ResolvedExpr>,
        /// List members.
        list: Vec<ResolvedExpr>,
        /// `true` for `NOT IN`.
        negated: bool,
        /// Collation governing text comparison.
        collation: Option<Collation>,
        /// All-literal list pre-hashed for membership tests.
        constant: Option<InListConstant>,
    },
    /// Scalar function call.
    Function {
        /// The builtin function.
        name: ScalarFunction,
        /// Resolved arguments.
        args: Vec<ResolvedExpr>,
    },
    /// 宿主注册的自定义标量函数调用;名字已折叠为小写。
    HostFunction {
        /// 折叠小写后的注册名。
        name: String,
        /// 实参,按书写顺序。
        args: Vec<ResolvedExpr>,
    },
    /// `CAST(expr AS type)`.
    Cast {
        /// Cast operand.
        value: Box<ResolvedExpr>,
        /// Declared target type.
        target: CastType,
    },
    /// `expr COLLATE name`；只改变文本比较规则，求值结果与内层表达式相同。
    Collate {
        /// 被施加排序规则的表达式。
        value: Box<ResolvedExpr>,
        /// 解析后的内置排序规则。
        collation: Collation,
    },
    /// `x [NOT] LIKE y [ESCAPE z]`.
    Like {
        /// Tested operand.
        value: Box<ResolvedExpr>,
        /// Pattern operand.
        pattern: Box<ResolvedExpr>,
        /// `true` for `NOT LIKE`.
        negated: bool,
        /// `ESCAPE` character, if written.
        escape: Option<Box<ResolvedExpr>>,
    },
    /// `x IS DISTINCT FROM y` or `x IS NOT DISTINCT FROM y`.
    IsDistinct {
        /// Left operand.
        left: Box<ResolvedExpr>,
        /// Right operand.
        right: Box<ResolvedExpr>,
        /// `true` for `IS DISTINCT FROM`.
        negated: bool,
        /// Collation governing text comparison.
        collation: Collation,
    },
    /// `x GLOB y` or `x NOT GLOB y`.
    Glob {
        /// Tested operand.
        value: Box<ResolvedExpr>,
        /// Pattern operand.
        pattern: Box<ResolvedExpr>,
        /// `true` for `NOT GLOB`.
        negated: bool,
    },
    /// Scalar subquery `(SELECT ...)`.
    Subquery(
        /// The subquery statement.
        Box<Statement>,
    ),
    /// `EXISTS (SELECT ...)`.
    Exists(
        /// The subquery statement.
        Box<Statement>,
    ),
    /// `x [NOT] IN (SELECT ...)`.
    InSubquery {
        /// Tested operand.
        value: Box<ResolvedExpr>,
        /// The subquery statement.
        query: Box<Statement>,
        /// `true` for `NOT IN`.
        negated: bool,
    },
    /// Value of a window already computed for the current row.
    Window(
        /// Position in the core's window list.
        usize,
    ),
}

/// One resolved window call: function, partitioning, ordering and frame.
pub struct ResolvedWindow {
    /// Function and its resolved arguments.
    pub function: ResolvedWindowFunction,
    /// `PARTITION BY` expressions.
    pub partition: Vec<ResolvedExpr>,
    /// `ORDER BY` keys with their direction.
    pub order: Vec<(ResolvedExpr, SortDirection)>,
    /// Explicit frame, already merged with a base window's frame.
    pub frame: Option<ResolvedFrame>,
}

/// A window function with resolved arguments.
pub enum ResolvedWindowFunction {
    /// `row_number()`.
    RowNumber,
    /// `rank()`.
    Rank,
    /// `dense_rank()`.
    DenseRank,
    /// `percent_rank()`.
    PercentRank,
    /// `cume_dist()`.
    CumeDist,
    /// `ntile(n)`.
    Ntile(
        /// Bucket count expression.
        ResolvedExpr,
    ),
    /// `lag(expr[, offset[, default]])`.
    Lag {
        /// Value expression.
        value: ResolvedExpr,
        /// Row offset, defaulting to one.
        offset: Option<ResolvedExpr>,
        /// Value for a missing row, defaulting to `NULL`.
        default: Option<ResolvedExpr>,
    },
    /// `lead(expr[, offset[, default]])`.
    Lead {
        /// Value expression.
        value: ResolvedExpr,
        /// Row offset, defaulting to one.
        offset: Option<ResolvedExpr>,
        /// Value for a missing row, defaulting to `NULL`.
        default: Option<ResolvedExpr>,
    },
    /// `first_value(expr)`.
    FirstValue(
        /// Value expression.
        ResolvedExpr,
    ),
    /// `last_value(expr)`.
    LastValue(
        /// Value expression.
        ResolvedExpr,
    ),
    /// `nth_value(expr, n)`.
    NthValue {
        /// Value expression.
        value: ResolvedExpr,
        /// One-based position inside the frame.
        position: ResolvedExpr,
    },
    /// Aggregate over the frame.
    Aggregate {
        /// The aggregate to fold.
        function: AggregateFunction,
        /// Single argument; `None` for `count(*)`.
        argument: Option<ResolvedExpr>,
        /// `group_concat` separator argument.
        separator: Option<ResolvedExpr>,
        /// `DISTINCT` was written before the argument.
        distinct: bool,
        /// `FILTER (WHERE ...)` applied before the aggregate sees a row.
        filter: Option<ResolvedExpr>,
        /// `ORDER BY` inside the argument list, used by `group_concat`.
        order: Vec<(ResolvedExpr, SortDirection)>,
        /// `DISTINCT` 去重时使用的排序规则，取自参数表达式的生效排序规则。
        collation: Collation,
    },
}

/// One resolved frame edge.
pub struct ResolvedFrameBound {
    /// Which edge this is.
    pub edge: FrameEdge,
    /// Constant offset; `None` for `UNBOUNDED` and `CURRENT ROW`.
    pub offset: Option<ResolvedExpr>,
}

/// A resolved explicit frame.
pub struct ResolvedFrame {
    /// `ROWS`, `RANGE` or `GROUPS`.
    pub units: FrameUnits,
    /// Starting edge.
    pub start: ResolvedFrameBound,
    /// Ending edge.
    pub end: ResolvedFrameBound,
    /// Rows dropped from the computed frame.
    pub exclude: FrameExclude,
}

/// 全字面量 `IN` 列表的预计算形态：解析期建一次，求值期只做一次哈希查找。
///
/// 逐行重算 `IN` 列表会让 `where id in (<100 个字面量>)` 退化成每行 100 次
/// 表达式求值与比较（`in_list` 基准的热点）。列表项全是无 `COLLATE` 的字面量
/// 时语义与按 `BINARY` 归一化的键集完全一致，因此可以安全地折叠成键集。
#[derive(Clone, PartialEq)]
pub struct InListConstant {
    /// 非 NULL 列表项按 `BINARY` 归一化后的键集。
    pub keys: std::collections::HashSet<ProbeKey>,
    /// 列表里出现过 `NULL`：未命中时结果为 `NULL` 而不是布尔假。
    pub saw_null: bool,
}

/// 把全字面量 `IN` 列表折叠成键集；含表达式、参数或逐项 `COLLATE` 时返回
/// `None`，调用方回退到逐项求值。
pub fn constant_in_list(list: &[ResolvedExpr]) -> Option<InListConstant> {
    let mut keys = std::collections::HashSet::with_capacity(list.len());
    let mut saw_null = false;
    for item in list {
        let ResolvedExpr::Cell(cell) = item
        else {
            return None;
        };
        if matches!(cell, Cell::Null) {
            saw_null = true;
            continue;
        }
        let key = probe_key(cell, Collation::Binary)?;
        // 超出 binary64 精确范围的整数与实数比较会走数值语义；键集区分
        // `Wide` / `Number`，此时折叠后的相等判定不再等价，回退逐项比较。
        if matches!(key, ProbeKey::Wide(_)) {
            return None;
        }
        keys.insert(key);
    }
    Some(InListConstant { keys, saw_null })
}

impl ResolvedExpr {
    /// Appends the source indices this expression reads.
    pub fn collect_sources(&self, out: &mut Vec<usize>) {
        match self {
            Self::Cell(_) | Self::OuterColumn { .. } | Self::AggregateRef(_) | Self::TriggerColumn { .. } | Self::ExcludedColumn { .. } => {}
            // A subquery's own scope is resolved when it runs, so it cannot be
            // attributed to a single source here; `contains_subquery` makes
            // such a conjunct wait for the complete row instead.
            Self::Subquery(_) | Self::Exists(_) => {}
            Self::Column { source, .. } => out.push(*source),
            Self::Unary { value, .. }
            | Self::Not(value)
            | Self::IsNull { value, .. }
            | Self::IsTruth { value, .. }
            | Self::Cast { value, .. }
            | Self::Collate { value, .. } => value.collect_sources(out),
            Self::Binary { left, right, .. } | Self::Compare { left, right, .. } | Self::And(left, right) | Self::Or(left, right) => {
                left.collect_sources(out);
                right.collect_sources(out);
            }
            Self::IsDistinct { left, right, .. } => {
                left.collect_sources(out);
                right.collect_sources(out);
            }
            Self::Glob { value, pattern, .. } => {
                value.collect_sources(out);
                pattern.collect_sources(out);
            }
            Self::Case { operand, branches, otherwise, .. } => {
                if let Some(operand) = operand {
                    operand.collect_sources(out);
                }
                for (condition, result) in branches {
                    condition.collect_sources(out);
                    result.collect_sources(out);
                }
                if let Some(otherwise) = otherwise {
                    otherwise.collect_sources(out);
                }
            }
            Self::In { value, list, .. } => {
                value.collect_sources(out);
                for item in list {
                    item.collect_sources(out);
                }
            }
            Self::InSubquery { value, .. } => value.collect_sources(out),
            Self::Function { args, .. } | Self::HostFunction { args, .. } => {
                for arg in args {
                    arg.collect_sources(out);
                }
            }
            // Window values are already materialized for the current row.
            Self::Window(_) => {}
            Self::Parameter(_) => {}
            Self::Like { value, pattern, escape, .. } => {
                value.collect_sources(out);
                pattern.collect_sources(out);
                if let Some(escape) = escape {
                    escape.collect_sources(out);
                }
            }
        }
    }

    /// Whether this expression runs a subquery and therefore cannot be
    /// evaluated before the complete current row is bound.
    pub fn contains_subquery(&self) -> bool {
        match self {
            Self::Subquery(_) | Self::Exists(_) | Self::InSubquery { .. } => true,
            Self::Unary { value, .. }
            | Self::Not(value)
            | Self::IsNull { value, .. }
            | Self::IsTruth { value, .. }
            | Self::Cast { value, .. }
            | Self::Collate { value, .. } => value.contains_subquery(),
            Self::Binary { left, right, .. } | Self::Compare { left, right, .. } | Self::And(left, right) | Self::Or(left, right) => {
                left.contains_subquery() || right.contains_subquery()
            }
            Self::IsDistinct { left, right, .. } => left.contains_subquery() || right.contains_subquery(),
            Self::Glob { value, pattern, .. } => value.contains_subquery() || pattern.contains_subquery(),
            Self::Case { operand, branches, otherwise, .. } => {
                operand.as_ref().is_some_and(|operand| operand.contains_subquery())
                    || branches.iter().any(|(condition, result)| condition.contains_subquery() || result.contains_subquery())
                    || otherwise.as_ref().is_some_and(|otherwise| otherwise.contains_subquery())
            }
            Self::In { value, list, .. } => value.contains_subquery() || list.iter().any(ResolvedExpr::contains_subquery),
            Self::Function { args, .. } | Self::HostFunction { args, .. } => args.iter().any(ResolvedExpr::contains_subquery),
            Self::Like { value, pattern, escape, .. } => {
                value.contains_subquery() || pattern.contains_subquery() || escape.as_ref().is_some_and(|escape| escape.contains_subquery())
            }
            Self::Cell(_)
            | Self::Parameter(_)
            | Self::Column { .. }
            | Self::OuterColumn { .. }
            | Self::AggregateRef(_)
            | Self::Window(_)
            | Self::TriggerColumn { .. }
            | Self::ExcludedColumn { .. } => false,
        }
    }

    /// Whether this expression only reads the current row, literals and
    /// parameters — no outer column, subquery, aggregate or window. Used by the
    /// correlated-`EXISTS` probe, whose residual predicates must stay local so
    /// one cached plan answers every outer row.
    pub fn is_inner_only(&self) -> bool {
        match self {
            Self::Cell(_) | Self::Parameter(_) | Self::Column { .. } => true,
            Self::Unary { value, .. }
            | Self::Not(value)
            | Self::IsNull { value, .. }
            | Self::IsTruth { value, .. }
            | Self::Cast { value, .. }
            | Self::Collate { value, .. } => value.is_inner_only(),
            Self::Binary { left, right, .. } | Self::Compare { left, right, .. } | Self::And(left, right) | Self::Or(left, right) => {
                left.is_inner_only() && right.is_inner_only()
            }
            Self::IsDistinct { left, right, .. } => left.is_inner_only() && right.is_inner_only(),
            Self::Glob { value, pattern, .. } => value.is_inner_only() && pattern.is_inner_only(),
            Self::Case { operand, branches, otherwise, .. } => {
                operand.as_ref().is_none_or(|operand| operand.is_inner_only())
                    && branches.iter().all(|(condition, result)| condition.is_inner_only() && result.is_inner_only())
                    && otherwise.as_ref().is_none_or(|otherwise| otherwise.is_inner_only())
            }
            Self::In { value, list, .. } => value.is_inner_only() && list.iter().all(ResolvedExpr::is_inner_only),
            Self::Function { args, .. } | Self::HostFunction { args, .. } => args.iter().all(ResolvedExpr::is_inner_only),
            Self::Like { value, pattern, escape, .. } => {
                value.is_inner_only() && pattern.is_inner_only() && escape.as_ref().is_none_or(|escape| escape.is_inner_only())
            }
            Self::OuterColumn { .. }
            | Self::Subquery(_)
            | Self::Exists(_)
            | Self::InSubquery { .. }
            | Self::AggregateRef(_)
            | Self::Window(_)
            | Self::TriggerColumn { .. }
            | Self::ExcludedColumn { .. } => false,
        }
    }
}

/// Direction of one resolved sort key plus its explicit `NULL` placement.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SortDirection {
    /// `DESC` was written.
    pub descending: bool,
    /// `NULLS FIRST` / `NULLS LAST`; `None` keeps the SQLite default, which is
    /// first for `ASC` and last for `DESC`.
    pub nulls_first: Option<bool>,
    /// 键表达式的文本排序规则，默认为 `BINARY`。
    pub collation: Collation,
}

impl SortDirection {
    /// Direction of a parsed key.
    pub fn of(key: &OrderKey) -> Self {
        Self { descending: key.descending, nulls_first: key.nulls_first, collation: Collation::Binary }
    }

    /// 附加键表达式的排序规则。
    pub fn with_collation(mut self, collation: Collation) -> Self {
        self.collation = collation;
        self
    }
}

/// 排序规则未知时的统一诊断文案，与上游 SQLite 一致。
pub fn unknown_collation(name: &str) -> SqlError {
    SqlError::new("unknown-collation", format!("no such collation sequence: {name}"))
}

/// 解析期求一个表达式生效的排序规则：`COLLATE` 用显式声明，裸列用列声明，
/// 其余表达式按 `BINARY`。
pub fn parsed_collation(expr: &Expr, relation: &Relation) -> Result<Collation, SqlError> {
    match expr {
        Expr::Collate { collation, .. } => Collation::parse(collation).ok_or_else(|| unknown_collation(collation)),
        Expr::Column(reference) => Ok(match relation.resolve(reference) {
            Ok((_, merged)) => relation.column_collation(merged),
            Err(_) => Collation::Binary,
        }),
        Expr::Alias { value, .. } => parsed_collation(value, relation),
        _ => Ok(Collation::Binary),
    }
}

/// 返回带显式 `COLLATE` 的那一层表达式，供二元比较按左侧优先取规则。
pub fn explicit_collate(expr: &Expr) -> Option<&Expr> {
    match expr {
        Expr::Collate { .. } => Some(expr),
        Expr::Alias { value, .. } => match value.as_ref() {
            Expr::Collate { .. } => Some(value),
            _ => None,
        },
        _ => None,
    }
}

/// 二元比较合并后的排序规则：左显式、右显式、左列、右列，最后 `BINARY`。
pub fn comparison_collation(left: &Expr, right: &Expr, relation: &Relation) -> Result<Collation, SqlError> {
    if let Some(expr) = explicit_collate(left) {
        return parsed_collation(expr, relation);
    }
    if let Some(expr) = explicit_collate(right) {
        return parsed_collation(expr, relation);
    }
    match parsed_collation(left, relation)? {
        Collation::Binary => parsed_collation(right, relation),
        collation => Ok(collation),
    }
}

/// 已解析表达式生效的排序规则，供 `ORDER BY` / `GROUP BY` 使用。
pub fn resolved_collation(expr: &ResolvedExpr, relation: &Relation) -> Collation {
    match expr {
        ResolvedExpr::Collate { collation, .. } => *collation,
        ResolvedExpr::Column { index, .. } => relation.column_collation(*index),
        _ => Collation::Binary,
    }
}

/// Resolves one parsed expression against the current core.
///
/// An unresolved column is retried in each enclosing scope, innermost first.
/// Because the current core is always tried first, a same-named inner column
/// shadows an outer one, exactly as in SQLite.
#[allow(clippy::too_many_arguments)]
pub fn resolve_expr(
    expr: &Expr,
    relation: &Relation,
    outer: &[ScopeShape],
    named: &[NamedWindow],
    windows: &mut Vec<ResolvedWindow>,
    aggregates: &mut Vec<AggregateSpec>,
    allow_aggregate: bool,
    allow_window: bool,
) -> Result<ResolvedExpr, SqlError> {
    Ok(match expr {
        Expr::Column(reference) => match relation.resolve(reference) {
            Ok((TRIGGER_SOURCE_NEW, index)) => ResolvedExpr::TriggerColumn { old: false, index },
            Ok((TRIGGER_SOURCE_OLD, index)) => ResolvedExpr::TriggerColumn { old: true, index },
            Ok((EXCLUDED_SOURCE, index)) => ResolvedExpr::ExcludedColumn { index },
            Ok((source, index)) => ResolvedExpr::Column { source, index },
            Err(error) if error.code == "unknown-column" || error.code == "unknown-table" => match resolve_outer(reference, outer)? {
                Some(found) => found,
                None => return Err(error),
            },
            Err(error) => return Err(error),
        },
        Expr::Literal(literal) => ResolvedExpr::Cell(literal_cell(literal)),
        Expr::Parameter { index, .. } => ResolvedExpr::Parameter(*index),
        // A projection alias only affects the header, which is taken from the
        // unresolved expression, so evaluation delegates straight through.
        Expr::Alias { value, .. } => resolve_expr(value, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?,
        Expr::Unary { op, value } => ResolvedExpr::Unary {
            op: *op,
            value: Box::new(resolve_expr(value, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
        },
        Expr::Binary { left, op, right } => ResolvedExpr::Binary {
            left: Box::new(resolve_expr(left, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
            op: *op,
            right: Box::new(resolve_expr(right, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
        },
        Expr::Compare { left, op, right } => ResolvedExpr::Compare {
            left: Box::new(resolve_expr(left, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
            op: *op,
            collation: comparison_collation(left, right, relation)?,
            right: Box::new(resolve_expr(right, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
        },
        Expr::IsNull { value, negated } => ResolvedExpr::IsNull {
            value: Box::new(resolve_expr(value, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
            negated: *negated,
        },
        Expr::IsTruth { value, truth, negated } => ResolvedExpr::IsTruth {
            value: Box::new(resolve_expr(value, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
            truth: *truth,
            negated: *negated,
        },
        Expr::Not(inner) => {
            ResolvedExpr::Not(Box::new(resolve_expr(inner, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?))
        }
        Expr::And(left, right) => ResolvedExpr::And(
            Box::new(resolve_expr(left, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
            Box::new(resolve_expr(right, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
        ),
        Expr::Or(left, right) => ResolvedExpr::Or(
            Box::new(resolve_expr(left, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
            Box::new(resolve_expr(right, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
        ),
        Expr::Case { operand, branches, otherwise } => ResolvedExpr::Case {
            collation: match operand {
                Some(operand) => match parsed_collation(operand, relation)? {
                    Collation::Binary => None,
                    collation => Some(collation),
                },
                None => None,
            },
            operand: operand
                .as_ref()
                .map(|inner| {
                    Ok::<_, SqlError>(Box::new(resolve_expr(
                        inner,
                        relation,
                        outer,
                        named,
                        windows,
                        aggregates,
                        allow_aggregate,
                        allow_window,
                    )?))
                })
                .transpose()?,
            branches: branches
                .iter()
                .map(|(condition, result)| {
                    Ok((
                        resolve_expr(condition, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?,
                        resolve_expr(result, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?,
                    ))
                })
                .collect::<Result<Vec<_>, SqlError>>()?,
            otherwise: otherwise
                .as_ref()
                .map(|inner| {
                    Ok::<_, SqlError>(Box::new(resolve_expr(
                        inner,
                        relation,
                        outer,
                        named,
                        windows,
                        aggregates,
                        allow_aggregate,
                        allow_window,
                    )?))
                })
                .transpose()?,
        },
        Expr::In { value, list, negated } => {
            let collation = match parsed_collation(value, relation)? {
                Collation::Binary => None,
                collation => Some(collation),
            };
            let list = list
                .iter()
                .map(|item| resolve_expr(item, relation, outer, named, windows, aggregates, allow_aggregate, allow_window))
                .collect::<Result<Vec<_>, _>>()?;
            // 左操作数没有显式 `COLLATE` 且列表项全是字面量时，逐项比较等价于
            // 对 `BINARY` 键集做一次哈希查找，可在解析期折叠。
            let constant = if collation.is_none() { constant_in_list(&list) } else { None };
            ResolvedExpr::In {
                collation,
                value: Box::new(resolve_expr(value, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
                list,
                negated: *negated,
                constant,
            }
        }
        Expr::Function { name, args } => ResolvedExpr::Function {
            name: *name,
            args: args
                .iter()
                .map(|arg| resolve_expr(arg, relation, outer, named, windows, aggregates, allow_aggregate, allow_window))
                .collect::<Result<Vec<_>, _>>()?,
        },
        Expr::HostFunction { name, args } => ResolvedExpr::HostFunction {
            name: name.clone(),
            args: args
                .iter()
                .map(|arg| resolve_expr(arg, relation, outer, named, windows, aggregates, allow_aggregate, allow_window))
                .collect::<Result<Vec<_>, _>>()?,
        },
        Expr::Cast { value, target } => ResolvedExpr::Cast {
            value: Box::new(resolve_expr(value, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
            target: *target,
        },
        Expr::Collate { value, collation } => ResolvedExpr::Collate {
            value: Box::new(resolve_expr(value, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
            collation: Collation::parse(collation).ok_or_else(|| unknown_collation(collation))?,
        },
        Expr::Like { value, pattern, negated, escape } => ResolvedExpr::Like {
            value: Box::new(resolve_expr(value, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
            pattern: Box::new(resolve_expr(pattern, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
            negated: *negated,
            escape: match escape {
                Some(escape) => {
                    Some(Box::new(resolve_expr(escape, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?))
                }
                None => None,
            },
        },
        Expr::Glob { value, pattern, negated } => ResolvedExpr::Glob {
            value: Box::new(resolve_expr(value, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
            pattern: Box::new(resolve_expr(pattern, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
            negated: *negated,
        },
        Expr::IsDistinct { left, right, negated } => ResolvedExpr::IsDistinct {
            left: Box::new(resolve_expr(left, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
            right: Box::new(resolve_expr(right, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
            negated: *negated,
            collation: comparison_collation(left, right, relation)?,
        },
        Expr::Subquery(query) => ResolvedExpr::Subquery(query.clone()),
        Expr::Exists(query) => ResolvedExpr::Exists(query.clone()),
        Expr::InSubquery { value, query, negated } => ResolvedExpr::InSubquery {
            value: Box::new(resolve_expr(value, relation, outer, named, windows, aggregates, allow_aggregate, allow_window)?),
            query: query.clone(),
            negated: *negated,
        },
        Expr::Aggregate { function, argument, separator, distinct, filter, order } => {
            if !allow_aggregate {
                let mut error =
                    SqlError::new("misuse-of-aggregate", format!("misuse of aggregate function {}()", function.name())).in_statement("select");
                error.clause = Some(function.name().to_ascii_uppercase());
                return Err(error);
            }
            // An aggregate argument is evaluated per row, so it may not contain
            // another aggregate; subqueries inside it are still allowed.
            let argument =
                argument.as_ref().map(|arg| resolve_expr(arg, relation, outer, named, windows, aggregates, false, false)).transpose()?;
            let separator =
                separator.as_ref().map(|sep| resolve_expr(sep, relation, outer, named, windows, aggregates, false, false)).transpose()?;
            let filter =
                filter.as_ref().map(|expr| resolve_expr(expr, relation, outer, named, windows, aggregates, false, false)).transpose()?;
            let order = order
                .iter()
                .map(|key| {
                    let resolved = resolve_expr(&key.expr, relation, outer, named, windows, aggregates, false, false)?;
                    let collation = resolved_collation(&resolved, relation);
                    Ok((resolved, SortDirection::of(key).with_collation(collation)))
                })
                .collect::<Result<Vec<_>, SqlError>>()?;
            let index = aggregates.len();
            let collation = argument.as_ref().map(|arg| resolved_collation(arg, relation)).unwrap_or_default();
            aggregates.push(AggregateSpec { function: *function, argument, separator, distinct: *distinct, filter, order, collation });
            ResolvedExpr::AggregateRef(index)
        }
        Expr::Window { function, spec } => {
            if !allow_window {
                let mut error = SqlError::new("misuse-of-window", "misuse of window function").in_statement("select");
                error.clause = Some(window_call_name(function).to_ascii_uppercase());
                return Err(error);
            }
            let resolved = resolve_window(function, spec, relation, outer, named, windows, aggregates)?;
            let index = windows.len();
            windows.push(resolved);
            ResolvedExpr::Window(index)
        }
    })
}

/// Lowercase name of a window function, used in `misuse` diagnostics.
pub fn window_call_name(function: &WindowFunction) -> &'static str {
    match function {
        WindowFunction::RowNumber => "row_number",
        WindowFunction::Rank => "rank",
        WindowFunction::DenseRank => "dense_rank",
        WindowFunction::PercentRank => "percent_rank",
        WindowFunction::CumeDist => "cume_dist",
        WindowFunction::Ntile(_) => "ntile",
        WindowFunction::Lag { .. } => "lag",
        WindowFunction::Lead { .. } => "lead",
        WindowFunction::FirstValue(_) => "first_value",
        WindowFunction::LastValue(_) => "last_value",
        WindowFunction::NthValue { .. } => "nth_value",
        WindowFunction::Aggregate { function, .. } => function.name(),
    }
}

/// Resolves a window call, merging the `OVER` clause with any base window.
pub fn resolve_window(
    function: &WindowFunction,
    spec: &WindowSpec,
    relation: &Relation,
    outer: &[ScopeShape],
    named: &[NamedWindow],
    windows: &mut Vec<ResolvedWindow>,
    aggregates: &mut Vec<AggregateSpec>,
) -> Result<ResolvedWindow, SqlError> {
    let merged = merge_window_spec(spec, named, &mut Vec::new(), None)?;
    let partition = merged
        .partition
        .iter()
        .map(|expr| resolve_expr(expr, relation, outer, named, windows, aggregates, true, false))
        .collect::<Result<Vec<_>, _>>()?;
    let order = merged
        .order
        .iter()
        .map(|key| {
            let resolved = resolve_expr(&key.expr, relation, outer, named, windows, aggregates, true, false)?;
            let collation = resolved_collation(&resolved, relation);
            Ok((resolved, SortDirection::of(key).with_collation(collation)))
        })
        .collect::<Result<Vec<_>, SqlError>>()?;
    let frame = match merged.frame {
        Some(frame) => Some(resolve_frame(frame, relation, outer, named, windows, aggregates)?),
        None => None,
    };
    let function = resolve_window_function(function, relation, outer, named, windows, aggregates)?;
    Ok(ResolvedWindow { function, partition, order, frame })
}

/// Flattened window definition after following base-window references.
pub struct MergedWindow<'a> {
    /// Inherited `PARTITION BY` expressions.
    pub partition: Vec<&'a Expr>,
    /// Inherited `ORDER BY` keys.
    pub order: Vec<&'a OrderKey>,
    /// Inherited frame, if any.
    pub frame: Option<&'a Frame>,
}

/// Follows `w1 -> w2 -> ...` references and rejects clauses that would
/// override a base window's own clause, as SQLite does.
pub fn merge_window_spec<'a>(
    spec: &'a WindowSpec,
    named: &'a [NamedWindow],
    seen: &mut Vec<String>,
    window_name: Option<&str>,
) -> Result<MergedWindow<'a>, SqlError> {
    let mut partition = Vec::new();
    let mut order = Vec::new();
    let mut frame = None;
    if let Some(base) = &spec.base {
        let self_reference = window_name.is_some_and(|name| name.eq_ignore_ascii_case(base));
        if !self_reference && !seen.iter().any(|name| name.eq_ignore_ascii_case(base)) {
            let definition = named.iter().rev().find(|window| window.name.eq_ignore_ascii_case(base)).ok_or_else(|| {
                let mut error = SqlError::new("unknown-window", format!("no such window: {base}")).in_statement("select");
                error.clause = Some(base.to_ascii_uppercase());
                error
            })?;
            seen.push(base.clone());
            let inherited = merge_window_spec(&definition.spec, named, seen, Some(base))?;
            partition = inherited.partition;
            order = inherited.order;
            frame = inherited.frame;
        }
    }
    if !spec.partition.is_empty() {
        if !partition.is_empty() {
            return Err(SqlError::new(
                "window-override",
                format!("cannot override PARTITION clause of window: {}", spec.base.clone().unwrap_or_default()),
            )
            .in_statement("select"));
        }
        partition = spec.partition.iter().collect();
    }
    if !spec.order.is_empty() {
        if !order.is_empty() {
            return Err(SqlError::new(
                "window-override",
                format!("cannot override ORDER BY clause of window: {}", spec.base.clone().unwrap_or_default()),
            )
            .in_statement("select"));
        }
        order = spec.order.iter().collect();
    }
    if let Some(explicit) = &spec.frame {
        if frame.is_some() {
            return Err(SqlError::new(
                "window-override",
                format!("cannot override frame clause of window: {}", spec.base.clone().unwrap_or_default()),
            )
            .in_statement("select"));
        }
        frame = Some(explicit);
    }
    Ok(MergedWindow { partition, order, frame })
}

/// Resolves an explicit frame; offsets must be constant non-negative integers.
pub fn resolve_frame(
    frame: &Frame,
    _relation: &Relation,
    _outer: &[ScopeShape],
    _named: &[NamedWindow],
    _windows: &mut Vec<ResolvedWindow>,
    _aggregates: &mut Vec<AggregateSpec>,
) -> Result<ResolvedFrame, SqlError> {
    // SQLite only accepts a literal non-negative integer here; `abs(-1)` and
    // `1.5` are both rejected at prepare time. Validating now keeps evaluation
    // free of column lookups, which a frame offset can never have.
    let resolve_bound = |bound: &FrameBound, which: &str| -> Result<ResolvedFrameBound, SqlError> {
        let offset = match &bound.offset {
            Some(offset) => match offset.as_ref() {
                Expr::Literal(Literal::Integer(value)) if *value >= 0 => Some(ResolvedExpr::Cell(Cell::Integer(*value))),
                _ => {
                    return Err(
                        SqlError::new("invalid-frame", format!("frame {which} offset must be a non-negative integer")).in_statement("select")
                    );
                }
            },
            None => None,
        };
        Ok(ResolvedFrameBound { edge: bound.edge, offset })
    };
    let start = resolve_bound(&frame.start, "starting")?;
    let end = resolve_bound(&frame.end, "ending")?;
    Ok(ResolvedFrame { units: frame.units, start, end, exclude: frame.exclude })
}

/// Resolves the arguments of a window function.
pub fn resolve_window_function(
    function: &WindowFunction,
    relation: &Relation,
    outer: &[ScopeShape],
    named: &[NamedWindow],
    windows: &mut Vec<ResolvedWindow>,
    aggregates: &mut Vec<AggregateSpec>,
) -> Result<ResolvedWindowFunction, SqlError> {
    // Arguments are evaluated per row of the frame, so they may contain
    // aggregates but never another window function.
    let mut argument = |expr: &Expr| resolve_expr(expr, relation, outer, named, windows, aggregates, true, false);
    Ok(match function {
        WindowFunction::RowNumber => ResolvedWindowFunction::RowNumber,
        WindowFunction::Rank => ResolvedWindowFunction::Rank,
        WindowFunction::DenseRank => ResolvedWindowFunction::DenseRank,
        WindowFunction::PercentRank => ResolvedWindowFunction::PercentRank,
        WindowFunction::CumeDist => ResolvedWindowFunction::CumeDist,
        WindowFunction::Ntile(buckets) => ResolvedWindowFunction::Ntile(argument(buckets)?),
        WindowFunction::Lag { value, offset, default } => ResolvedWindowFunction::Lag {
            value: argument(value)?,
            offset: offset.as_ref().map(|expr| argument(expr)).transpose()?,
            default: default.as_ref().map(|expr| argument(expr)).transpose()?,
        },
        WindowFunction::Lead { value, offset, default } => ResolvedWindowFunction::Lead {
            value: argument(value)?,
            offset: offset.as_ref().map(|expr| argument(expr)).transpose()?,
            default: default.as_ref().map(|expr| argument(expr)).transpose()?,
        },
        WindowFunction::FirstValue(value) => ResolvedWindowFunction::FirstValue(argument(value)?),
        WindowFunction::LastValue(value) => ResolvedWindowFunction::LastValue(argument(value)?),
        WindowFunction::NthValue { value, position } => {
            ResolvedWindowFunction::NthValue { value: argument(value)?, position: argument(position)? }
        }
        WindowFunction::Aggregate { function, argument: value, separator, distinct, filter, order } => {
            let mut resolved_order = Vec::with_capacity(order.len());
            for key in order {
                let resolved = argument(&key.expr)?;
                let collation = resolved_collation(&resolved, relation);
                resolved_order.push((resolved, SortDirection::of(key).with_collation(collation)));
            }
            let resolved_argument = value.as_ref().map(|expr| argument(expr)).transpose()?;
            let collation = resolved_argument.as_ref().map(|arg| resolved_collation(arg, relation)).unwrap_or_default();
            ResolvedWindowFunction::Aggregate {
                function: *function,
                argument: resolved_argument,
                separator: separator.as_ref().map(|expr| argument(expr)).transpose()?,
                distinct: *distinct,
                filter: filter.as_ref().map(|expr| argument(expr)).transpose()?,
                order: resolved_order,
                collation,
            }
        }
    })
}

/// Retries a column reference in the enclosing scopes, innermost first.
pub fn resolve_outer(reference: &ColumnRef, outer: &[ScopeShape]) -> Result<Option<ResolvedExpr>, SqlError> {
    for (depth, scope) in outer.iter().enumerate() {
        let mut matches = Vec::new();
        for (index, (label, name)) in scope.columns.iter().enumerate() {
            let qualifier_matches = reference.qualifier.as_ref().is_none_or(|qualifier| qualifier.eq_ignore_ascii_case(label));
            if qualifier_matches && name.eq_ignore_ascii_case(&reference.name) {
                matches.push(index);
            }
        }
        match matches.len() {
            0 => continue,
            1 => return Ok(Some(ResolvedExpr::OuterColumn { depth, index: matches[0] })),
            _ => {
                return Err(SqlError::new("ambiguous-column", format!("ambiguous column `{}` in an enclosing `SELECT`", reference.display()))
                    .in_statement("select"));
            }
        }
    }
    Ok(None)
}

/// Resolves every `CHECK` of one `CREATE TABLE` against the declared columns.
///
/// The table does not exist yet, so a synthetic source exposing its column
/// names stands in for it. A reference to an undeclared column therefore fails
/// the statement with `unknown-column`, and a subquery is rejected by name
/// because SQLite forbids one inside a check.
pub fn resolve_checks(
    table: &str,
    columns: &[ColumnSpec],
    checks: &[CheckConstraint],
    statement: &'static str,
    without_rowid: bool,
) -> Result<Vec<CheckDef>, SqlError> {
    if checks.is_empty() {
        return Ok(Vec::new());
    }
    let names: Vec<String> = columns.iter().map(|column| column.name.clone()).collect();
    let width = names.len();
    // A base-table source also exposes the hidden rowid slot, so `CHECK
    // (rowid > 0)` resolves exactly like SQLite accepts it.
    let def = TableDef {
        name: table.to_string(),
        columns: columns.to_vec(),
        rowid_alias: None,
        primary_key_index: false,
        primary_key: Vec::new(),
        table_constraints: Vec::new(),
        checks: Vec::new(),
        foreign_keys: Vec::new(),
        strict: false,
        without_rowid,
        temp: false,
    };
    let source = Source {
        label: table.to_string(),
        columns: SourceColumns::Table(&def),
        offset: 0,
        join: None,
        star_hidden: vec![false; width],
        unqualified_hidden: vec![false; width],
        star_override: vec![None; width],
    };
    let relation = Relation { sources: vec![source], using: vec![Vec::new()], using_slots: Vec::new(), trigger: None, excluded: false };
    let mut resolved = Vec::with_capacity(checks.len());
    for check in checks {
        let mut aggregates = Vec::new();
        let mut windows = Vec::new();
        let expr = resolve_expr(&check.expr, &relation, &[], &[], &mut windows, &mut aggregates, false, false)?;
        if expr.contains_subquery() {
            let mut error = SqlError::new("unsupported-clause", "subqueries prohibited in CHECK constraints").in_statement(statement);
            error.clause = Some("CHECK".to_string());
            return Err(error);
        }
        resolved.push(CheckDef { name: check.name.clone(), expr, text: check.text.clone() });
    }
    Ok(resolved)
}

/// Resolves every `REFERENCES` clause of one `CREATE TABLE` against the
/// declared child columns.
///
/// Only the child side is validated here: SQLite allows a foreign key to name a
/// parent table that does not exist yet, and reports an undeclared child column
/// as `SQLITE_ERROR` with the same message the reference engine uses.
pub fn resolve_foreign_keys(columns: &[ColumnSpec], keys: &[ForeignKey]) -> Result<Vec<ForeignKeyDef>, SqlError> {
    let mut resolved = Vec::with_capacity(keys.len());
    for key in keys {
        let mut positions = Vec::with_capacity(key.columns.len());
        for name in &key.columns {
            let position = columns.iter().position(|column| column.name.eq_ignore_ascii_case(name)).ok_or_else(|| {
                SqlError::new("SQLITE_ERROR", format!("unknown column \"{name}\" in foreign key definition")).in_statement("create-table")
            })?;
            positions.push(position);
        }
        resolved.push(ForeignKeyDef {
            columns: positions,
            parent: key.parent.to_ascii_lowercase(),
            parent_display: key.parent.clone(),
            parent_columns: key.parent_columns.clone(),
            on_delete: foreign_key_action_text(key.on_delete),
            on_update: foreign_key_action_text(key.on_update),
            on_delete_action: key.on_delete,
            on_update_action: key.on_update,
            deferred: key.deferred,
        });
    }
    Ok(resolved)
}

/// Spells one referential action the way `PRAGMA foreign_key_list` reports it.
pub fn foreign_key_action_text(action: ForeignKeyAction) -> String {
    match action {
        ForeignKeyAction::NoAction => "NO ACTION",
        ForeignKeyAction::Restrict => "RESTRICT",
        ForeignKeyAction::SetNull => "SET NULL",
        ForeignKeyAction::SetDefault => "SET DEFAULT",
        ForeignKeyAction::Cascade => "CASCADE",
    }
    .to_string()
}

/// Builds the single-source relation an `UPDATE` / `DELETE` evaluates
/// against, labelled with the name column references may use.
pub fn target_relation<'a>(def: &'a TableDef, label: &str, trigger: Option<&'a TriggerFrame>) -> Relation<'a> {
    let width = def.columns.len();
    Relation {
        sources: vec![Source {
            label: label.to_string(),
            columns: SourceColumns::Table(def),
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

/// Relation used by `ON CONFLICT DO UPDATE`: `excluded.<column>` binds
/// against the supplied row while bare columns keep the stored row.
pub fn upsert_relation<'a>(def: &'a TableDef, label: &str) -> Relation<'a> {
    let mut relation = target_relation(def, label, None);
    relation.excluded = true;
    relation
}
