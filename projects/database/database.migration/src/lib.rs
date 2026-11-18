//! Schema and data migration from an external PostgreSQL source into a local engine.
//!
//! One run does three things in order: finish the first full scan if it has not
//! completed, replay everything the source changed since the scan started, and
//! optionally reconcile rows the source deleted.
//!
//! Every write is an upsert keyed by the primary key, and the scan position is
//! persisted after each batch, so a run killed at any point resumes from the
//! last saved cursor instead of restarting or duplicating rows.
//!
//! Deliberate boundaries, reported instead of faked:
//! * A table without a primary key cannot be scanned incrementally and aborts
//!   with [`Error::Unsupported`].
//! * A table without `updated_at` is copied once but not replayed afterwards.
//! * Deletions are reconciled by comparing primary keys, not by reading the
//!   write-ahead log, so a delete that lands mid-scan is only seen next run.

mod ddl;
mod literal;
mod source;
mod state;

use std::{collections::HashSet, fmt, path::{Path, PathBuf}};

pub use ddl::{create_table, delete_by_keys, quote_ident, quote_literal, select_keys, select_ordered, upsert_statement, ColumnDef, TableShape};
pub use literal::{raw_literal, sql_literal};
pub use source::{MigrationSource, PageRequest, PgSource, Row};
pub use state::{MigrationState, Phase, TableState};

/// Everything a migration can fail on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The source server rejected a statement or returned an unusable shape.
    Source(String),
    /// The target rejected a statement or could not be read back.
    Target(String),
    /// The breakpoint file is unreadable or written by another format version.
    State(String),
    /// A source feature this version refuses to approximate.
    Unsupported(String),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Source(message) => write!(formatter, "migration source: {message}"),
            Error::Target(message) => write!(formatter, "migration target: {message}"),
            Error::State(message) => write!(formatter, "migration state: {message}"),
            Error::Unsupported(message) => write!(formatter, "migration unsupported: {message}"),
        }
    }
}

impl std::error::Error for Error {}

/// One result set as text cells, matching what both sides return.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResultSet {
    /// Column labels in result order.
    pub columns: Vec<String>,
    /// Row-major cells; `None` is SQL `NULL`.
    pub rows: Vec<Vec<Option<String>>>,
}

/// The local engine a migration writes into.
///
/// Implemented by the product facade that owns the storage, so this crate never
/// depends on a concrete engine.
pub trait MigrationTarget {
    /// Runs one statement that returns no row set.
    fn execute(&mut self, sql: &str) -> Result<(), Error>;
    /// Runs one statement and returns its rows as text.
    fn query(&mut self, sql: &str) -> Result<ResultSet, Error>;
}

/// Knobs for one migration run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Source schema searched for base tables.
    pub schema: String,
    /// Restricts the run to these tables, in the given order.
    pub tables: Option<Vec<String>>,
    /// Rows copied per round trip.
    pub batch_rows: usize,
    /// Plans and reports without touching the target.
    pub dry_run: bool,
    /// Breakpoint file. `None` keeps progress in memory only.
    pub state_path: Option<PathBuf>,
    /// Ignores the saved breakpoint and scans every table from the beginning.
    pub restart: bool,
    /// Reconciles rows the source deleted since the last run.
    pub prune: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            schema: "public".into(),
            tables: None,
            batch_rows: 500,
            dry_run: false,
            state_path: None,
            restart: false,
            prune: false,
        }
    }
}

/// What one table did during this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableReport {
    /// Source table name.
    pub name: String,
    /// Target DDL replayed for this table.
    pub ddl: String,
    /// Phase the table is in after this run.
    pub phase: Phase,
    /// Rows the source reported for the whole table.
    pub source_rows: u64,
    /// Rows the target holds after this run.
    pub target_rows: u64,
    /// Rows written by this run.
    pub written: u64,
    /// Rows removed because the source no longer holds them.
    pub pruned: u64,
    /// `true` only when this run verified the full table row by row.
    pub verified: bool,
    /// Why verification did not run or did not match, when relevant.
    pub note: Option<String>,
}

/// Outcome of one migration run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Per-table results.
    pub tables: Vec<TableReport>,
}

impl Report {
    /// Rows written by this run across every table.
    pub fn written_rows(&self) -> u64 {
        self.tables.iter().map(|table| table.written).sum()
    }
}

/// Finishes the full scan, replays increments, then reconciles deletions.
pub fn migrate(
    source: &mut dyn MigrationSource,
    target: &mut dyn MigrationTarget,
    options: &Options,
) -> Result<Report, Error> {
    let mut state = match (&options.state_path, options.restart) {
        (Some(path), false) => MigrationState::load(path)?,
        _ => MigrationState::default(),
    };
    let tables = match &options.tables {
        Some(tables) => tables.clone(),
        None => source.tables(&options.schema)?,
    };
    let mut report = Report::default();
    for name in tables {
        let table = migrate_table(source, target, options, &mut state, &name)?;
        report.tables.push(table);
        save(&options.state_path, &state)?;
    }
    Ok(report)
}

fn migrate_table(
    source: &mut dyn MigrationSource,
    target: &mut dyn MigrationTarget,
    options: &Options,
    state: &mut MigrationState,
    name: &str,
) -> Result<TableReport, Error> {
    let shape = source.shape(&options.schema, name)?;
    let ddl = create_table(&shape)?;
    let order = shape.order_columns().ok_or_else(|| {
        Error::Unsupported(format!("table `{name}` has no primary key, so incremental replay is impossible"))
    })?;
    let incremental_order = if shape.column("updated_at").is_some() {
        let mut columns = vec!["updated_at".to_owned()];
        columns.extend(order.iter().cloned());
        Some(columns)
    } else {
        None
    };
    let source_rows = source.count(&options.schema, name)?;

    if options.dry_run {
        return Ok(TableReport {
            name: shape.name.clone(),
            ddl,
            phase: state.entry(name).map(|entry| entry.phase).unwrap_or(Phase::Full),
            source_rows,
            target_rows: 0,
            written: 0,
            pruned: 0,
            verified: false,
            note: Some("dry run: target untouched".into()),
        });
    }

    let mut entry = state.entry(name).cloned().unwrap_or(TableState {
        phase: Phase::Full,
        watermark: String::new(),
        cursor: None,
        written: 0,
    });
    let starting_full = entry.phase == Phase::Full;
    if starting_full && entry.watermark.is_empty() {
        entry.watermark = source.now()?;
    }
    state.tables.insert(name.to_owned(), entry);

    target.execute(&ddl)?;

    let mut written = 0;
    if starting_full {
        written += scan(
            source,
            target,
            options,
            state,
            name,
            &shape,
            &order,
            None,
        )?;
        let entry = state.tables.get_mut(name).expect("entry inserted above");
        entry.phase = Phase::Incremental;
        entry.cursor = None;
        save(&options.state_path, state)?;
    }

    let note = if let Some(incremental_order) = &incremental_order {
        let watermark = state.tables[name].watermark.clone();
        written += scan(
            source,
            target,
            options,
            state,
            name,
            &shape,
            incremental_order,
            Some(&watermark),
        )?;
        None
    } else {
        Some("table has no `updated_at`, so later runs will not replay changes".to_string())
    };

    let (verified, verify_note) = if starting_full {
        verify_rows(source, target, options, name, &shape, &order)?
    } else {
        (false, Some("incremental run: full verification happens on the first full scan".into()))
    };

    let pruned = if options.prune { prune(source, target, options, name, &shape, &order)? } else { 0 };

    let target_rows = count_target(target, name)?;
    Ok(TableReport {
        name: shape.name.clone(),
        ddl,
        phase: state.tables[name].phase,
        source_rows,
        target_rows,
        written,
        pruned,
        verified,
        note: note.or(verify_note),
    })
}

/// Copies every window of one scan and persists the cursor after each batch.
#[allow(clippy::too_many_arguments)]
fn scan(
    source: &mut dyn MigrationSource,
    target: &mut dyn MigrationTarget,
    options: &Options,
    state: &mut MigrationState,
    name: &str,
    shape: &TableShape,
    order: &[String],
    since: Option<&str>,
) -> Result<u64, Error> {
    let mut written = 0;
    loop {
        let cursor = state.tables[name].cursor.clone();
        let rows = source.rows(&PageRequest {
            schema: &options.schema,
            table: name,
            order,
            after: cursor.as_deref(),
            since,
            limit: options.batch_rows,
        })?;
        if rows.is_empty() {
            return Ok(written);
        }
        let short = rows.len() < options.batch_rows;
        target.execute(&upsert_statement(shape, &rows)?)?;
        written += rows.len() as u64;
        let last = rows.last().expect("non-empty window");
        let next = cursor_from_row(shape, order, last)?;
        let entry = state.tables.get_mut(name).expect("entry inserted before scanning");
        entry.cursor = Some(next);
        entry.written += rows.len() as u64;
        save(&options.state_path, state)?;
        if short {
            return Ok(written);
        }
    }
}

fn cursor_from_row(shape: &TableShape, order: &[String], row: &Row) -> Result<Row, Error> {
    order
        .iter()
        .map(|name| {
            shape
                .columns
                .iter()
                .position(|column| &column.name == name)
                .and_then(|index| row.get(index).cloned())
                .ok_or_else(|| Error::Source(format!("scanned row has no `{name}` value")))
        })
        .collect()
}

fn verify_rows(
    source: &mut dyn MigrationSource,
    target: &mut dyn MigrationTarget,
    options: &Options,
    name: &str,
    shape: &TableShape,
    order: &[String],
) -> Result<(bool, Option<String>), Error> {
    let mut offset = 0_u64;
    let mut cursor: Option<Row> = None;
    loop {
        let expected = source.rows(&PageRequest {
            schema: &options.schema,
            table: name,
            order,
            after: cursor.as_deref(),
            since: None,
            limit: options.batch_rows,
        })?;
        if expected.is_empty() {
            return Ok((true, None));
        }
        let actual = target.query(&select_ordered(shape, order, offset, options.batch_rows))?.rows;
        if expected != actual {
            let first = expected
                .iter()
                .zip(actual.iter())
                .position(|(left, right)| left != right)
                .unwrap_or(expected.len().min(actual.len()));
            return Ok((
                false,
                Some(format!(
                    "row {first} at offset {offset} differs ({} source rows, {} target rows)",
                    expected.len(),
                    actual.len()
                )),
            ));
        }
        offset += expected.len() as u64;
        if expected.len() < options.batch_rows {
            return Ok((true, None));
        }
        cursor = Some(cursor_from_row(shape, order, expected.last().expect("non-empty window"))?);
    }
}

fn prune(
    source: &mut dyn MigrationSource,
    target: &mut dyn MigrationTarget,
    options: &Options,
    name: &str,
    shape: &TableShape,
    order: &[String],
) -> Result<u64, Error> {
    let mut remaining: HashSet<String> = HashSet::new();
    let mut offset = 0_u64;
    loop {
        let rows = target.query(&select_keys(shape, order, offset, options.batch_rows))?.rows;
        if rows.is_empty() {
            break;
        }
        offset += rows.len() as u64;
        for row in rows {
            remaining.insert(key_of(&row));
        }
    }

    let mut cursor: Option<Row> = None;
    loop {
        let rows = source.rows(&PageRequest {
            schema: &options.schema,
            table: name,
            order,
            after: cursor.as_deref(),
            since: None,
            limit: options.batch_rows,
        })?;
        if rows.is_empty() {
            break;
        }
        let short = rows.len() < options.batch_rows;
        for row in &rows {
            remaining.remove(&key_of(&cursor_from_row(shape, order, row)?));
        }
        cursor = Some(cursor_from_row(shape, order, rows.last().expect("non-empty window"))?);
        if short {
            break;
        }
    }

    let stale: Vec<Vec<Option<String>>> = remaining.into_iter().map(|key| split_key(&key)).collect();
    let mut pruned = 0_u64;
    for chunk in stale.chunks(options.batch_rows.max(1)) {
        if chunk.is_empty() {
            continue;
        }
        target.execute(&delete_by_keys(shape, order, chunk))?;
        pruned += chunk.len() as u64;
    }
    Ok(pruned)
}

fn key_of(row: &[Option<String>]) -> String {
    row.iter().map(|cell| cell.clone().unwrap_or_else(|| "\u{0}".to_owned())).collect::<Vec<_>>().join("\u{1}")
}

fn split_key(key: &str) -> Vec<Option<String>> {
    key.split('\u{1}')
        .map(|part| if part == "\u{0}" { None } else { Some(part.to_owned()) })
        .collect()
}

fn count_target(target: &mut dyn MigrationTarget, name: &str) -> Result<u64, Error> {
    Ok(target
        .query(&format!("SELECT COUNT(*) FROM {}", quote_ident(name)))?
        .rows
        .first()
        .and_then(|row| row.first().cloned().flatten())
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or_default())
}

fn save(path: &Option<PathBuf>, state: &MigrationState) -> Result<(), Error> {
    match path {
        Some(path) => state.save(Path::new(path)),
        None => Ok(()),
    }
}
