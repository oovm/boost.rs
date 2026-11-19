//! Where rows come from.
//!
//! [`PgSource`] speaks PostgreSQL through the family's wire client, so the same
//! trait covers an external server and a loopback engine that exposes the
//! protocol. Tests implement the trait directly instead of opening a socket.

use database_client::pgsql::{ClientConfig, Connection};

use crate::{
    ColumnDef, Error, TableShape,
    ddl::{quote_ident, quote_literal},
    literal::raw_literal,
};

/// One row as text cells; `None` is SQL `NULL`.
pub type Row = Vec<Option<String>>;

/// One window request shared by the full scan and the incremental scan.
#[derive(Debug, Clone)]
pub struct PageRequest<'a> {
    /// Source schema.
    pub schema: &'a str,
    /// Source table.
    pub table: &'a str,
    /// Ordering columns: the primary key for a full scan, `updated_at` followed
    /// by the primary key for an incremental scan.
    pub order: &'a [String],
    /// Keyset cursor: the `order` values of the last row of the previous window.
    pub after: Option<&'a [Option<String>]>,
    /// Incremental lower bound: only rows whose `updated_at` is at or after it.
    pub since: Option<&'a str>,
    /// Rows requested.
    pub limit: usize,
}

/// One side of a migration that can be read.
pub trait MigrationSource {
    /// Base tables in `schema`, ordered by name.
    fn tables(&mut self, schema: &str) -> Result<Vec<String>, Error>;
    /// Column order, types and primary key of one table.
    fn shape(&mut self, schema: &str, table: &str) -> Result<TableShape, Error>;
    /// Row count of one table.
    fn count(&mut self, schema: &str, table: &str) -> Result<u64, Error>;
    /// The source clock, read once before a full scan to seed the incremental
    /// lower bound so writes made during the scan are replayed afterwards.
    fn now(&mut self) -> Result<String, Error>;
    /// One window of rows.
    fn rows(&mut self, request: &PageRequest<'_>) -> Result<Vec<Row>, Error>;

    /// One window of primary keys only.
    ///
    /// Deletion reconciliation needs nothing but keys, and a wide table's payload
    /// dwarfs its keys, so sources that can project the key columns must override
    /// this instead of re-reading every column.
    fn keys(&mut self, request: &PageRequest<'_>) -> Result<Vec<Row>, Error> {
        self.rows(request)
    }
}

/// PostgreSQL source read over the family wire client.
pub struct PgSource {
    config: ClientConfig,
    connection: Connection,
}

impl PgSource {
    /// Opens the source connection and completes the startup handshake.
    pub fn open(config: &ClientConfig) -> Result<Self, Error> {
        Ok(Self { config: config.clone(), connection: Self::open_pinned(config)? })
    }

    /// Open a connection and pin the session settings that decide how values
    /// render as text. `Row` compares text cells and the engine renders
    /// timestamps with a `+00` offset, ISO dates and the `postgres` interval
    /// style, so a server default such as `Asia/Hong_Kong` would otherwise
    /// make every `timestamptz` column differ from its own stored value.
    fn open_pinned(config: &ClientConfig) -> Result<Connection, Error> {
        let mut connection = Connection::open(config).map_err(source_error)?;
        for sql in ["SET TIME ZONE 'UTC'", "SET DateStyle = 'ISO, MDY'", "SET IntervalStyle = 'postgres'"] {
            connection.execute(sql).map_err(source_error)?;
        }
        Ok(connection)
    }

    /// Reads one statement's rows.
    ///
    /// A migration over a remote link outlives an idle timeout sooner or later,
    /// so one failed round trip reconnects and retries instead of losing the
    /// whole run. Statements are reads and single idempotent writes, so a retry
    /// cannot duplicate work.
    fn rows_of(&mut self, sql: &str) -> Result<Vec<Row>, Error> {
        match self.connection.execute(sql) {
            Ok(mut results) => Ok(results.pop().map(|result| result.rows).unwrap_or_default()),
            Err(_) => {
                self.reconnect()?;
                let mut results = self.connection.execute(sql).map_err(source_error)?;
                Ok(results.pop().map(|result| result.rows).unwrap_or_default())
            }
        }
    }

    fn reconnect(&mut self) -> Result<(), Error> {
        self.connection = Self::open_pinned(&self.config)?;
        Ok(())
    }
}

impl MigrationSource for PgSource {
    fn tables(&mut self, schema: &str) -> Result<Vec<String>, Error> {
        let sql = format!(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = {} AND table_type = 'BASE TABLE' ORDER BY table_name",
            quote_literal(schema)
        );
        Ok(self.rows_of(&sql)?.into_iter().filter_map(|row| row.into_iter().next().flatten()).collect())
    }

    fn shape(&mut self, schema: &str, table: &str) -> Result<TableShape, Error> {
        let sql = format!(
            "SELECT column_name, data_type, udt_name, character_maximum_length, \
             numeric_precision, numeric_scale, is_nullable, column_default \
             FROM information_schema.columns WHERE table_schema = {} AND table_name = {} \
             ORDER BY ordinal_position",
            quote_literal(schema),
            quote_literal(table)
        );
        let primary = self.primary_key(schema, table)?;
        let mut columns = Vec::new();
        for row in self.rows_of(&sql)? {
            let cell = |index: usize| row.get(index).cloned().flatten();
            let name = cell(0).ok_or_else(|| Error::Source(format!("column without a name in `{table}`")))?;
            columns.push(ColumnDef {
                primary: primary.iter().any(|key| key == &name),
                name,
                data_type: cell(1).unwrap_or_default(),
                udt: cell(2).unwrap_or_default(),
                max_length: cell(3).and_then(|text| text.parse().ok()),
                numeric_precision: cell(4).and_then(|text| text.parse().ok()),
                numeric_scale: cell(5).and_then(|text| text.parse().ok()),
                nullable: cell(6).as_deref() != Some("NO"),
                default: cell(7),
            });
        }
        if columns.is_empty() {
            return Err(Error::Source(format!("table `{schema}.{table}` has no columns")));
        }
        Ok(TableShape { name: table.to_owned(), columns })
    }

    fn count(&mut self, schema: &str, table: &str) -> Result<u64, Error> {
        let sql = format!("SELECT COUNT(*) FROM {}.{}", quote_ident(schema), quote_ident(table));
        self.rows_of(&sql)?
            .first()
            .and_then(|row| row.first().cloned().flatten())
            .and_then(|text| text.trim().parse().ok())
            .ok_or_else(|| Error::Source(format!("source count for `{table}` is not a number")))
    }

    fn now(&mut self) -> Result<String, Error> {
        self.rows_of("SELECT to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"')")?
            .first()
            .and_then(|row| row.first().cloned().flatten())
            .ok_or_else(|| Error::Source("source clock is unavailable".into()))
    }

    fn rows(&mut self, request: &PageRequest<'_>) -> Result<Vec<Row>, Error> {
        let mut sql = format!("SELECT * FROM {}.{}", quote_ident(request.schema), quote_ident(request.table));
        let mut conditions = Vec::new();
        if let Some(since) = request.since {
            conditions.push(format!("{} >= {}", quote_ident("updated_at"), quote_literal(since)));
        }
        if let Some(after) = request.after {
            let columns: Vec<String> = request.order.iter().map(|column| quote_ident(column)).collect();
            let values: Vec<String> = after
                .iter()
                .map(|value| match value {
                    Some(text) => raw_literal(text),
                    None => "NULL".to_owned(),
                })
                .collect();
            conditions.push(format!("({}) > ({})", columns.join(", "), values.join(", ")));
        }
        if !conditions.is_empty() {
            sql.push_str(&format!(" WHERE {}", conditions.join(" AND ")));
        }
        let order: Vec<String> = request.order.iter().map(|column| quote_ident(column)).collect();
        sql.push_str(&format!(" ORDER BY {}", order.join(", ")));
        sql.push_str(&format!(" LIMIT {}", request.limit));
        self.rows_of(&sql)
    }

    fn keys(&mut self, request: &PageRequest<'_>) -> Result<Vec<Row>, Error> {
        let columns: Vec<String> = request.order.iter().map(|column| quote_ident(column)).collect();
        let mut sql = format!("SELECT {} FROM {}.{}", columns.join(", "), quote_ident(request.schema), quote_ident(request.table));
        if let Some(after) = request.after {
            let values: Vec<String> = after
                .iter()
                .map(|value| match value {
                    Some(text) => raw_literal(text),
                    None => "NULL".to_owned(),
                })
                .collect();
            sql.push_str(&format!(" WHERE ({}) > ({})", columns.join(", "), values.join(", ")));
        }
        sql.push_str(&format!(" ORDER BY {} LIMIT {}", columns.join(", "), request.limit));
        self.rows_of(&sql)
    }
}

impl PgSource {
    fn primary_key(&mut self, schema: &str, table: &str) -> Result<Vec<String>, Error> {
        let sql = format!(
            "SELECT kcu.column_name FROM information_schema.table_constraints tc \
             JOIN information_schema.key_column_usage kcu \
               ON tc.constraint_name = kcu.constraint_name AND tc.table_schema = kcu.table_schema \
             WHERE tc.table_schema = {} AND tc.table_name = {} AND tc.constraint_type = 'PRIMARY KEY' \
             ORDER BY kcu.ordinal_position",
            quote_literal(schema),
            quote_literal(table)
        );
        Ok(self.rows_of(&sql)?.into_iter().filter_map(|row| row.into_iter().next().flatten()).collect())
    }
}

fn source_error(error: impl std::fmt::Display) -> Error {
    Error::Source(error.to_string())
}
