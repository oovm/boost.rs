//! Where rows come from.
//!
//! [`PgSource`] speaks PostgreSQL through the family's wire client, so the same
//! trait covers an external server and a loopback engine that exposes the
//! protocol. Tests implement the trait directly instead of opening a socket.

use database_client::pgsql::{ClientConfig, Connection};

use crate::{
    ddl::{quote_ident, quote_literal},
    ColumnDef, Error, TableShape,
};

/// One side of a migration that can be read.
pub trait MigrationSource {
    /// Base tables in `schema`, ordered by name.
    fn tables(&mut self, schema: &str) -> Result<Vec<String>, Error>;
    /// Column order, types and primary key of one table.
    fn shape(&mut self, schema: &str, table: &str) -> Result<TableShape, Error>;
    /// Row count of one table.
    fn count(&mut self, schema: &str, table: &str) -> Result<u64, Error>;
    /// One window of rows; `order` is the primary key when the table has one.
    fn rows(
        &mut self,
        schema: &str,
        table: &str,
        order: &Option<Vec<String>>,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<Vec<Option<String>>>, Error>;
}

/// PostgreSQL source read over the family wire client.
pub struct PgSource {
    connection: Connection,
}

impl PgSource {
    /// Opens the source connection and completes the startup handshake.
    pub fn open(config: &ClientConfig) -> Result<Self, Error> {
        Connection::open(config).map(|connection| Self { connection }).map_err(source_error)
    }

    fn rows_of(&mut self, sql: &str) -> Result<Vec<Vec<Option<String>>>, Error> {
        let mut results = self.connection.execute(sql).map_err(source_error)?;
        Ok(results.pop().map(|result| result.rows).unwrap_or_default())
    }
}

impl MigrationSource for PgSource {
    fn tables(&mut self, schema: &str) -> Result<Vec<String>, Error> {
        let sql = format!(
            "SELECT table_name FROM information_schema.tables WHERE table_schema = {} AND table_type = 'BASE TABLE' ORDER BY table_name",
            quote_literal(schema)
        );
        Ok(self
            .rows_of(&sql)?
            .into_iter()
            .filter_map(|row| row.into_iter().next().flatten())
            .collect())
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

    fn rows(
        &mut self,
        schema: &str,
        table: &str,
        order: &Option<Vec<String>>,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<Vec<Option<String>>>, Error> {
        let mut sql = format!("SELECT * FROM {}.{}", quote_ident(schema), quote_ident(table));
        if let Some(order) = order {
            let keys: Vec<String> = order.iter().map(|column| quote_ident(column)).collect();
            sql.push_str(&format!(" ORDER BY {}", keys.join(", ")));
        }
        sql.push_str(&format!(" LIMIT {limit} OFFSET {offset}"));
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
        Ok(self
            .rows_of(&sql)?
            .into_iter()
            .filter_map(|row| row.into_iter().next().flatten())
            .collect())
    }
}

fn source_error(error: impl std::fmt::Display) -> Error {
    Error::Source(error.to_string())
}
