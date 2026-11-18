//! Schema and data migration from an external PostgreSQL source into a local engine.
//!
//! The crate owns exactly one product behaviour: replay a source schema as DDL
//! against a target, copy every row in primary-key order, then re-read the
//! target and compare it row by row. Source and target are traits so the
//! product facade decides which engine is written and which server is read.
//!
//! Deliberate boundaries, reported instead of faked:
//! * A table without a primary key is copied but not row-verified, because the
//!   two sides cannot be put into a stable identical order.
//! * A column type outside the mapped set aborts the table with [`Error::Unsupported`]
//!   rather than writing a lossy approximation.

mod ddl;
mod literal;
mod source;

use std::fmt;

pub use ddl::{create_table, insert_statement, quote_ident, quote_literal, select_ordered, ColumnDef, TableShape};
pub use literal::sql_literal;
pub use source::{MigrationSource, PgSource};

/// Everything a migration can fail on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The source server rejected a statement or returned an unusable shape.
    Source(String),
    /// The target rejected a statement or could not be read back.
    Target(String),
    /// A source feature the first version refuses to approximate.
    Unsupported(String),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Source(message) => write!(formatter, "migration source: {message}"),
            Error::Target(message) => write!(formatter, "migration target: {message}"),
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
    /// Rows fetched per source round trip.
    pub batch_rows: usize,
    /// Plans and reports without touching the target.
    pub dry_run: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { schema: "public".into(), tables: None, batch_rows: 500, dry_run: false }
    }
}

/// What one table did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableReport {
    /// Source table name.
    pub name: String,
    /// Target DDL replayed for this table.
    pub ddl: String,
    /// Rows the source reported.
    pub source_rows: u64,
    /// Rows written to the target.
    pub target_rows: u64,
    /// `true` only when the target was re-read and matched the source exactly.
    pub verified: bool,
    /// Why verification did not run or did not match, when relevant.
    pub note: Option<String>,
}

/// Outcome of one migration run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub tables: Vec<TableReport>,
}

impl Report {
    /// Rows copied across every table.
    pub fn total_rows(&self) -> u64 {
        self.tables.iter().map(|table| table.target_rows).sum()
    }

    /// `true` when every table was verified row by row.
    pub fn fully_verified(&self) -> bool {
        self.tables.iter().all(|table| table.verified)
    }
}

/// Plans and copies every selected table from `source` into `target`.
pub fn migrate(
    source: &mut dyn MigrationSource,
    target: &mut dyn MigrationTarget,
    options: &Options,
) -> Result<Report, Error> {
    let tables = match &options.tables {
        Some(tables) => tables.clone(),
        None => source.tables(&options.schema)?,
    };
    let mut report = Report::default();
    for name in tables {
        report.tables.push(migrate_table(source, target, options, &name)?);
    }
    Ok(report)
}

fn migrate_table(
    source: &mut dyn MigrationSource,
    target: &mut dyn MigrationTarget,
    options: &Options,
    name: &str,
) -> Result<TableReport, Error> {
    let shape = source.shape(&options.schema, name)?;
    let ddl = ddl::create_table(&shape)?;
    let source_rows = source.count(&options.schema, name)?;
    if options.dry_run {
        return Ok(TableReport {
            name: shape.name.clone(),
            ddl,
            source_rows,
            target_rows: 0,
            verified: false,
            note: Some("dry run: target untouched".into()),
        });
    }

    target.execute(&ddl)?;
    let order = shape.order_columns();
    let mut written = 0_u64;
    let mut offset = 0_u64;
    loop {
        let rows = source.rows(&options.schema, name, &order, offset, options.batch_rows)?;
        if rows.is_empty() {
            break;
        }
        written += rows.len() as u64;
        target.execute(&ddl::insert_statement(&shape, &rows)?)?;
        offset += rows.len() as u64;
        if rows.len() < options.batch_rows {
            break;
        }
    }

    let target_rows = target
        .query(&format!("SELECT COUNT(*) FROM {}", ddl::quote_ident(name)))?
        .rows
        .first()
        .and_then(|row| row.first().cloned().flatten())
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or_default();

    let (verified, note) = match &shape.order_columns() {
        Some(order) => verify_rows(source, target, options, name, &shape, order)?,
        None => (
            false,
            Some("no primary key: rows copied but not row-verified".to_string()),
        ),
    };

    Ok(TableReport { name: shape.name.clone(), ddl, source_rows, target_rows, verified, note })
}

fn verify_rows(
    source: &mut dyn MigrationSource,
    target: &mut dyn MigrationTarget,
    options: &Options,
    name: &str,
    shape: &TableShape,
    order: &[String],
) -> Result<(bool, Option<String>), Error> {
    let ordered = Some(order.to_vec());
    let mut offset = 0_u64;
    loop {
        let expected = source.rows(&options.schema, name, &ordered, offset, options.batch_rows)?;
        let actual = target.query(&ddl::select_ordered(shape, order, offset, options.batch_rows))?.rows;
        if expected != actual {
            let first = expected
                .iter()
                .zip(actual.iter())
                .position(|(left, right)| left != right)
                .unwrap_or(expected.len().min(actual.len()));
            return Ok((
                false,
                Some(format!(
                    "row {first} within window at offset {offset} differs (source {} cells, target {} cells)",
                    expected.get(first).map(|row| row.len()).unwrap_or(0),
                    actual.get(first).map(|row| row.len()).unwrap_or(0)
                )),
            ));
        }
        if expected.len() < options.batch_rows {
            return Ok((true, None));
        }
        offset += expected.len() as u64;
    }
}
