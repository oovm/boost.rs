//! Turning a source description into target DDL and insert statements.

use crate::Error;

/// One column as `information_schema.columns` described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    /// Column name.
    pub name: String,
    /// `data_type` value, for example `character varying`.
    pub data_type: String,
    /// `udt_name` value, for example `varchar`.
    pub udt: String,
    /// Declared character length, when the type has one.
    pub max_length: Option<u64>,
    /// Declared numeric precision, when the type has one.
    pub numeric_precision: Option<u64>,
    /// Declared numeric scale, when the type has one.
    pub numeric_scale: Option<u64>,
    /// `false` when the column is `NOT NULL`.
    pub nullable: bool,
    /// Raw `DEFAULT` text, when written.
    pub default: Option<String>,
    /// `true` when the column belongs to the primary key.
    pub primary: bool,
}

/// One table as the source described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableShape {
    /// Table name.
    pub name: String,
    /// Columns in source declaration order.
    pub columns: Vec<ColumnDef>,
}

impl TableShape {
    /// Looks a column up by name.
    pub fn column(&self, name: &str) -> Option<&ColumnDef> {
        self.columns.iter().find(|column| column.name == name)
    }

    /// Primary-key column names, or `None` when the table has no primary key.
    pub fn order_columns(&self) -> Option<Vec<String>> {
        let keys: Vec<String> =
            self.columns.iter().filter(|column| column.primary).map(|column| column.name.clone()).collect();
        if keys.is_empty() { None } else { Some(keys) }
    }
}

/// Maps a source column onto the target type, refusing lossy approximations.
fn target_type(column: &ColumnDef) -> Result<String, Error> {
    let rendered = match column.udt.as_str() {
        "int2" => "smallint".to_owned(),
        "int4" => "integer".to_owned(),
        "int8" => "bigint".to_owned(),
        "float4" => "real".to_owned(),
        "float8" => "double precision".to_owned(),
        "bool" => "boolean".to_owned(),
        "varchar" => match column.max_length {
            Some(length) => format!("varchar({length})"),
            None => "text".to_owned(),
        },
        "bpchar" => match column.max_length {
            Some(length) => format!("char({length})"),
            None => "text".to_owned(),
        },
        "text" | "uuid" | "json" | "jsonb" | "date" | "time" | "bytea" => column.udt.clone(),
        "timestamptz" => "timestamptz".to_owned(),
        "timestamp" => "timestamp".to_owned(),
        "numeric" | "decimal" => match (column.numeric_precision, column.numeric_scale) {
            (Some(precision), Some(scale)) => format!("numeric({precision},{scale})"),
            _ => "numeric".to_owned(),
        },
        other => {
            return Err(Error::Unsupported(format!(
                "column `{}` has source type `{other}` with no target mapping",
                column.name
            )))
        }
    };
    Ok(rendered)
}

/// `CREATE TABLE` for one source table, including the primary key.
pub fn create_table(shape: &TableShape) -> Result<String, Error> {
    let mut definitions = Vec::with_capacity(shape.columns.len() + 1);
    for column in &shape.columns {
        let mut definition = format!("{} {}", quote_ident(&column.name), target_type(column)?);
        if !column.nullable {
            definition.push_str(" NOT NULL");
        }
        if let Some(default) = column.default.as_deref().and_then(safe_default) {
            definition.push_str(&format!(" DEFAULT {default}"));
        }
        definitions.push(definition);
    }
    if let Some(keys) = shape.order_columns() {
        let keys: Vec<String> = keys.iter().map(|key| quote_ident(key)).collect();
        definitions.push(format!("PRIMARY KEY ({})", keys.join(", ")));
    }
    Ok(format!("CREATE TABLE IF NOT EXISTS {} ({})", quote_ident(&shape.name), definitions.join(", ")))
}

/// One multi-row upsert for a window of source rows.
///
/// Every write is an upsert keyed by the primary key so a re-run after an
/// interruption is idempotent and an incremental pass overwrites changed rows.
pub fn upsert_statement(shape: &TableShape, rows: &[Vec<Option<String>>]) -> Result<String, Error> {
    let columns: Vec<String> = shape.columns.iter().map(|column| quote_ident(&column.name)).collect();
    let types = shape.columns.iter().map(target_type).collect::<Result<Vec<_>, _>>()?;
    let mut values = Vec::with_capacity(rows.len());
    for row in rows {
        if row.len() != shape.columns.len() {
            return Err(Error::Source(format!(
                "table `{}` returned {} cells for {} columns",
                shape.name,
                row.len(),
                shape.columns.len()
            )));
        }
        let cells: Vec<String> = types
            .iter()
            .zip(row.iter())
            .map(|(target_type, value)| crate::literal::sql_literal(target_type, value.as_deref()))
            .collect();
        values.push(format!("({})", cells.join(", ")));
    }
    let mut statement = format!(
        "INSERT INTO {} ({}) VALUES {}",
        quote_ident(&shape.name),
        columns.join(", "),
        values.join(", ")
    );
    if let Some(keys) = shape.order_columns() {
        let keys: Vec<String> = keys.iter().map(|key| quote_ident(key)).collect();
        let updates: Vec<String> = shape
            .columns
            .iter()
            .filter(|column| !column.primary)
            .map(|column| {
                let name = quote_ident(&column.name);
                format!("{name} = EXCLUDED.{name}")
            })
            .collect();
        statement.push_str(&format!(" ON CONFLICT ({}) ", keys.join(", ")));
        if updates.is_empty() {
            statement.push_str("DO NOTHING");
        } else {
            statement.push_str(&format!("DO UPDATE SET {}", updates.join(", ")));
        }
    }
    Ok(statement)
}

/// `DELETE` removing keys the source no longer holds.
pub fn delete_by_keys(shape: &TableShape, order: &[String], keys: &[Vec<Option<String>>]) -> String {
    let values: Vec<String> = keys
        .iter()
        .map(|key| {
            let cells: Vec<String> = key
                .iter()
                .map(|value| match value {
                    Some(text) => crate::literal::raw_literal(text),
                    None => "NULL".to_owned(),
                })
                .collect();
            format!("({})", cells.join(", "))
        })
        .collect();
    let columns: Vec<String> = order.iter().map(|column| quote_ident(column)).collect();
    format!(
        "DELETE FROM {} WHERE ({}) IN ({})",
        quote_ident(&shape.name),
        columns.join(", "),
        values.join(", ")
    )
}

/// Primary-key-only window used to reconcile rows removed at the source.
pub fn select_keys(shape: &TableShape, order: &[String], offset: u64, limit: usize) -> String {
    let columns: Vec<String> = order.iter().map(|column| quote_ident(column)).collect();
    format!(
        "SELECT {} FROM {} ORDER BY {} LIMIT {limit} OFFSET {offset}",
        columns.join(", "),
        quote_ident(&shape.name),
        columns.join(", ")
    )
}

/// Ordered read of the same window the source produced, for verification.
pub fn select_ordered(shape: &TableShape, order: &[String], offset: u64, limit: usize) -> String {
    let keys: Vec<String> = order.iter().map(|key| quote_ident(key)).collect();
    format!(
        "SELECT * FROM {} ORDER BY {} LIMIT {limit} OFFSET {offset}",
        quote_ident(&shape.name),
        keys.join(", ")
    )
}

/// Keeps a source default only when the target can replay it.
///
/// PostgreSQL spells many defaults with a `::type` cast the target DDL parser
/// rejects, so the cast is stripped and the literal kept. A sequence default is
/// dropped entirely: identity belongs to the source, and the target must not
/// invent its own generator.
fn safe_default(default: &str) -> Option<String> {
    if default.contains("nextval(") {
        return None;
    }
    let literal = match default.split_once("::") {
        Some((value, _cast)) => value.trim(),
        None => default.trim(),
    };
    if literal.is_empty() { None } else { Some(literal.to_owned()) }
}

/// Double-quoted identifier with embedded quotes doubled.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Single-quoted string literal with embedded quotes doubled.
pub fn quote_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}
