//! DDL 目录变更状态机（`SqlSchemaRegistry::apply_ddl` 的纯变更部分）。
//!
//! 这里只做「一条 DDL 如何改写 schema 状态」：身份分配、约束校验、外键解析与
//! 索引登记。修订号推进与持久发布由上层注册表负责，编解码在
//! `database.formats::yy::sql_schema`，文件原子发布在 `database.storage::local::publish`。

use std::collections::BTreeSet;

use database_query::{
    RegistryState, SchemaField, SchemaForeignKey, SchemaIndex, SchemaTable, SqlFieldType,
    sql::{SqlErrorKind, SqlFrontendError},
    sql_ddl::{DdlColumn, DdlReference, DdlTableConstraint, SqlDdlStatement},
};

/// Declared octet capacity MySQL uses for an unqualified `BLOB`.
const MYSQL_BLOB_CAPACITY: u32 = 65_535;

/// Result of one applied DDL statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlDdlOutcome {
    /// `CREATE TABLE`, with `created = false` when `IF NOT EXISTS` matched.
    CreateTable {
        /// Whether the table was actually created.
        created: bool,
    },
    /// `ALTER TABLE ADD COLUMN`, with `added = false` when `IF NOT EXISTS` matched.
    AlterTableAddColumn {
        /// Whether the column was actually added.
        added: bool,
    },
    /// `ALTER TABLE ADD CONSTRAINT`.
    AlterTableAddConstraint,
    /// `DROP TABLE`, with the number of tables that existed and were removed.
    DropTable {
        /// Number of tables removed.
        dropped: usize,
    },
    /// `CREATE INDEX`, with `created = false` when `IF NOT EXISTS` matched.
    CreateIndex {
        /// Whether the index was actually created.
        created: bool,
    },
    /// `DROP INDEX`, with `dropped = false` when `IF EXISTS` matched nothing.
    DropIndex {
        /// Whether an index was actually removed.
        dropped: bool,
    },
}

impl SqlDdlOutcome {
    /// Command tag reported by a PostgreSQL `CommandComplete`.
    pub fn command_tag(&self) -> &'static str {
        match self {
            Self::CreateTable { .. } => "CREATE TABLE",
            Self::AlterTableAddColumn { .. } => "ALTER TABLE",
            Self::AlterTableAddConstraint => "ALTER TABLE",
            Self::DropTable { .. } => "DROP TABLE",
            Self::CreateIndex { .. } => "CREATE INDEX",
            Self::DropIndex { .. } => "DROP INDEX",
        }
    }
}

/// Validates one foreign key against the registry and resolves the referenced
/// column names when the statement omitted the column list, mirroring
/// PostgreSQL: the parent table must exist at declaration time and the
/// referenced columns must exist with matching arity.
fn resolve_foreign_key(
    state: &RegistryState,
    table: &str,
    columns: &[String],
    reference: &DdlReference,
) -> Result<SchemaForeignKey, SqlFrontendError> {
    if columns.is_empty() {
        return Err(SqlFrontendError::syntax(format!("table `{table}` declares a FOREIGN KEY without columns")));
    }
    let parent = state.table(&reference.table).ok_or_else(|| {
        SqlFrontendError::new(
            SqlErrorKind::UnknownTable,
            format!("foreign key of table `{table}` references table `{}` that does not exist", reference.table),
        )
    })?;
    let referenced: Vec<String> = match &reference.column {
        Some(column) => {
            if !parent.fields.iter().any(|field| &field.name == column) {
                return Err(SqlFrontendError::new(
                    SqlErrorKind::UnknownColumn,
                    format!("foreign key of table `{table}` references column `{}.{column}` that does not exist", parent.name),
                ));
            }
            vec![column.clone()]
        }
        None => {
            if parent.primary_key.is_empty() {
                return Err(SqlFrontendError::syntax(format!(
                    "foreign key of table `{table}` references table `{}` without a column list, but the parent table has no primary key",
                    parent.name
                )));
            }
            parent.primary_key.clone()
        }
    };
    if referenced.len() != columns.len() {
        return Err(SqlFrontendError::syntax(format!(
            "foreign key of table `{table}` declares {} columns but references {} columns",
            columns.len(),
            referenced.len()
        )));
    }
    Ok(SchemaForeignKey {
        columns: columns.to_vec(),
        references_table: reference.table.clone(),
        references_columns: referenced,
        on_delete: reference.on_delete.clone(),
        on_update: reference.on_update.clone(),
    })
}

/// Rejects a generated column declaration the gateway cannot honour.
///
/// The expression must be non-empty, the column cannot own a value sequence,
/// cannot carry a `DEFAULT` and cannot be part of the primary key, which are
/// exactly the combinations PostgreSQL itself refuses.
fn validate_generated_column(
    table: &str,
    column: &DdlColumn,
    expression: &str,
    primary_key: &[String],
) -> Result<(), SqlFrontendError> {
    if expression.trim().is_empty() {
        return Err(SqlFrontendError::syntax(format!(
            "generated column `{}` of table `{table}` declares an empty expression",
            column.name
        )));
    }
    if column.auto_increment {
        return Err(SqlFrontendError::syntax(format!(
            "generated column `{}` of table `{table}` cannot own a value sequence",
            column.name
        )));
    }
    if column.default.is_some() {
        return Err(SqlFrontendError::syntax(format!(
            "generated column `{}` of table `{table}` cannot declare a DEFAULT",
            column.name
        )));
    }
    if column.inline_primary_key || primary_key.iter().any(|candidate| candidate == &column.name) {
        return Err(SqlFrontendError::syntax(format!(
            "generated column `{}` of table `{table}` cannot be part of the primary key",
            column.name
        )));
    }
    Ok(())
}

/// Applies one DDL statement to the state, reporting whether it changed.
pub(crate) fn apply_ddl(state: &mut RegistryState, statement: &SqlDdlStatement) -> Result<(SqlDdlOutcome, bool), SqlFrontendError> {
    match statement {
        SqlDdlStatement::CreateTable { name, if_not_exists, columns, primary_key, unique_constraints, check_constraints, foreign_keys, .. } => {
            if state.table(name).is_some() {
                if *if_not_exists {
                    return Ok((SqlDdlOutcome::CreateTable { created: false }, false));
                }
                return Err(SqlFrontendError::new(SqlErrorKind::TableAlreadyExists, format!("table `{name}` already exists")));
            }
            if columns.is_empty() {
                return Err(SqlFrontendError::syntax(format!("table `{name}` must declare at least one column")));
            }
            let mut seen = BTreeSet::new();
            for column in columns {
                if !seen.insert(column.name.clone()) {
                    return Err(SqlFrontendError::syntax(format!("table `{name}` declares column `{}` twice", column.name)));
                }
            }
            let inline: Vec<String> = columns.iter().filter(|column| column.inline_primary_key).map(|column| column.name.clone()).collect();
            if inline.len() > 1 {
                return Err(SqlFrontendError::syntax(format!("table `{name}` declares PRIMARY KEY inline on more than one column")));
            }
            let primary = if primary_key.is_empty() {
                inline
            }
            else if inline.is_empty() {
                primary_key.clone()
            }
            else if *primary_key == inline {
                primary_key.clone()
            }
            else {
                return Err(SqlFrontendError::syntax(format!(
                    "table `{name}` declares inline and table-level PRIMARY KEY columns that disagree"
                )));
            };
            let mut distinct = BTreeSet::new();
            for column in &primary {
                if !distinct.insert(column.clone()) {
                    return Err(SqlFrontendError::syntax(format!("table `{name}` lists primary key column `{column}` twice")));
                }
                if !columns.iter().any(|candidate| &candidate.name == column) {
                    return Err(SqlFrontendError::new(
                        SqlErrorKind::UnknownColumn,
                        format!("primary key column `{column}` does not exist in table `{name}`"),
                    ));
                }
            }
            for constraint in unique_constraints {
                for column in constraint {
                    if !columns.iter().any(|candidate| &candidate.name == column) {
                        return Err(SqlFrontendError::new(
                            SqlErrorKind::UnknownColumn,
                            format!("unique constraint column `{column}` does not exist in table `{name}`"),
                        ));
                    }
                }
            }
            for check in check_constraints.iter().chain(columns.iter().filter_map(|column| column.check.as_ref())) {
                if check.trim().is_empty() {
                    return Err(SqlFrontendError::syntax(format!("table `{name}` declares an empty CHECK constraint")));
                }
            }
            let mut resolved_keys = Vec::with_capacity(foreign_keys.len() + columns.iter().filter(|column| column.reference.is_some()).count());
            for key in foreign_keys {
                for column in &key.columns {
                    if !columns.iter().any(|candidate| &candidate.name == column) {
                        return Err(SqlFrontendError::new(
                            SqlErrorKind::UnknownColumn,
                            format!("foreign key column `{column}` does not exist in table `{name}`"),
                        ));
                    }
                }
                resolved_keys.push(resolve_foreign_key(state, name, &key.columns, &key.reference)?);
            }
            for column in columns {
                if let Some(reference) = &column.reference {
                    resolved_keys.push(resolve_foreign_key(state, name, std::slice::from_ref(&column.name), reference)?);
                }
            }
            let mut fields = Vec::with_capacity(columns.len());
            for column in columns {
                if let Some(expression) = &column.generated {
                    validate_generated_column(name, column, expression, &primary)?;
                }
                let (base, capacity) = column_type(column, name)?;
                let not_null = column.not_null || primary.iter().any(|candidate| candidate == &column.name);
                let field_type = if not_null { base } else { SqlFieldType::Optional(Box::new(base)) };
                let field_id = state.next_field_id;
                state.next_field_id += 1;
                fields.push(SchemaField {
                    name: column.name.clone(),
                    field_id,
                    field_type,
                    capacity,
                    sql_type: column.sql_type.clone(),
                    default: column.default.clone(),
                    auto_increment: column.auto_increment,
                    next_value: 1,
                    check: column.check.clone(),
                    generated: column.generated.clone(),
                });
            }
            let type_id = state.next_type_id;
            state.next_type_id += 1;
            state.tables.push(SchemaTable {
                name: name.clone(),
                type_id,
                primary_key: primary,
                unique_constraints: unique_constraints.clone(),
                check_constraints: check_constraints.clone(),
                foreign_keys: resolved_keys,
                fields,
            });
            Ok((SqlDdlOutcome::CreateTable { created: true }, true))
        }
        SqlDdlStatement::AlterTableAddColumn { table, column, if_not_exists } => {
            let index = state
                .table_index(table)
                .ok_or_else(|| SqlFrontendError::new(SqlErrorKind::UnknownTable, format!("table `{table}` does not exist")))?;
            if state.tables[index].fields.iter().any(|field| field.name == column.name) {
                if *if_not_exists {
                    return Ok((SqlDdlOutcome::AlterTableAddColumn { added: false }, false));
                }
                return Err(SqlFrontendError::new(
                    SqlErrorKind::ColumnAlreadyExists,
                    format!("column `{}` already exists in table `{table}`", column.name),
                ));
            }
            if column.inline_primary_key && !state.tables[index].primary_key.is_empty() {
                return Err(SqlFrontendError::syntax(format!(
                    "table `{table}` already has a PRIMARY KEY, cannot add another inline PRIMARY KEY column"
                )));
            }
            if column.check.as_deref().is_some_and(|check| check.trim().is_empty()) {
                return Err(SqlFrontendError::syntax(format!("table `{table}` declares an empty CHECK constraint")));
            }
            if let Some(reference) = &column.reference {
                let key = resolve_foreign_key(state, table, std::slice::from_ref(&column.name), reference)?;
                state.tables[index].foreign_keys.push(key);
            }
            if let Some(expression) = &column.generated {
                validate_generated_column(table, column, expression, &state.tables[index].primary_key.clone())?;
            }
            let (base, capacity) = column_type(column, table)?;
            let not_null = column.not_null || column.inline_primary_key;
            let field_type = if not_null { base } else { SqlFieldType::Optional(Box::new(base)) };
            let field_id = state.next_field_id;
            state.next_field_id += 1;
            state.tables[index].fields.push(SchemaField {
                name: column.name.clone(),
                field_id,
                field_type,
                capacity,
                sql_type: column.sql_type.clone(),
                default: column.default.clone(),
                auto_increment: column.auto_increment,
                next_value: 1,
                check: column.check.clone(),
                generated: column.generated.clone(),
            });
            if column.inline_primary_key {
                state.tables[index].primary_key.push(column.name.clone());
            }
            if column.inline_unique {
                let constraint = vec![column.name.clone()];
                if !state.tables[index].unique_constraints.contains(&constraint) {
                    state.tables[index].unique_constraints.push(constraint);
                }
            }
            Ok((SqlDdlOutcome::AlterTableAddColumn { added: true }, true))
        }
        SqlDdlStatement::AlterTableAddConstraint { table, constraint } => {
            let index = state
                .table_index(table)
                .ok_or_else(|| SqlFrontendError::new(SqlErrorKind::UnknownTable, format!("table `{table}` does not exist")))?;
            match constraint {
                DdlTableConstraint::Unique { columns } => {
                    if columns.is_empty() {
                        return Err(SqlFrontendError::syntax(format!("table `{table}` declares a UNIQUE constraint without columns")));
                    }
                    for column in columns {
                        if !state.tables[index].fields.iter().any(|field| &field.name == column) {
                            return Err(SqlFrontendError::new(
                                SqlErrorKind::UnknownColumn,
                                format!("unique constraint column `{column}` does not exist in table `{table}`"),
                            ));
                        }
                    }
                    if state.tables[index].unique_constraints.contains(columns) {
                        return Ok((SqlDdlOutcome::AlterTableAddConstraint, false));
                    }
                    state.tables[index].unique_constraints.push(columns.clone());
                }
                DdlTableConstraint::Check { expression } => {
                    if expression.trim().is_empty() {
                        return Err(SqlFrontendError::syntax(format!("table `{table}` declares an empty CHECK constraint")));
                    }
                    state.tables[index].check_constraints.push(expression.clone());
                }
                DdlTableConstraint::ForeignKey { key } => {
                    for column in &key.columns {
                        if !state.tables[index].fields.iter().any(|field| &field.name == column) {
                            return Err(SqlFrontendError::new(
                                SqlErrorKind::UnknownColumn,
                                format!("foreign key column `{column}` does not exist in table `{table}`"),
                            ));
                        }
                    }
                    let resolved = resolve_foreign_key(state, table, &key.columns, &key.reference)?;
                    state.tables[index].foreign_keys.push(resolved);
                }
            }
            Ok((SqlDdlOutcome::AlterTableAddConstraint, true))
        }
        SqlDdlStatement::DropTable { names, if_exists } => {
            let mut dropped = 0;
            for name in names {
                let index = match state.table_index(name) {
                    Some(index) => index,
                    None if *if_exists => continue,
                    None => {
                        return Err(SqlFrontendError::new(SqlErrorKind::UnknownTable, format!("table `{name}` does not exist")));
                    }
                };
                state.tables.remove(index);
                state.indexes.retain(|index| &index.table != name);
                dropped += 1;
            }
            Ok((SqlDdlOutcome::DropTable { dropped }, dropped > 0))
        }
        SqlDdlStatement::CreateIndex { name, table, if_not_exists, unique, columns, predicate } => {
            let target = state
                .table(table)
                .ok_or_else(|| SqlFrontendError::new(SqlErrorKind::UnknownTable, format!("table `{table}` does not exist")))?;
            if state.indexes.iter().any(|index| index.name == *name) {
                if *if_not_exists {
                    return Ok((SqlDdlOutcome::CreateIndex { created: false }, false));
                }
                return Err(SqlFrontendError::new(SqlErrorKind::IndexAlreadyExists, format!("index `{name}` already exists")));
            }
            for column in columns {
                if !target.fields.iter().any(|field| &field.name == column) {
                    return Err(SqlFrontendError::new(
                        SqlErrorKind::UnknownColumn,
                        format!("index `{name}` references column `{column}` that does not exist in table `{table}`"),
                    ));
                }
            }
            state.indexes.push(SchemaIndex {
                name: name.clone(),
                table: table.clone(),
                unique: *unique,
                columns: columns.clone(),
                predicate: predicate.clone(),
            });
            Ok((SqlDdlOutcome::CreateIndex { created: true }, true))
        }
        SqlDdlStatement::DropIndex { name, table, if_exists } => {
            if let Some(table) = table {
                if state.table(table).is_none() {
                    return Err(SqlFrontendError::new(SqlErrorKind::UnknownTable, format!("table `{table}` does not exist")));
                }
            }
            let position =
                state.indexes.iter().position(|index| index.name == *name && table.as_ref().is_none_or(|table| index.table == *table));
            match position {
                Some(position) => {
                    state.indexes.remove(position);
                    Ok((SqlDdlOutcome::DropIndex { dropped: true }, true))
                }
                None if *if_exists => Ok((SqlDdlOutcome::DropIndex { dropped: false }, false)),
                None => Err(SqlFrontendError::new(SqlErrorKind::UnknownIndex, format!("index `{name}` does not exist"))),
            }
        }
    }
}

/// Maps one DDL column type onto a gateway field type and declared capacity.
fn column_type(column: &DdlColumn, table: &str) -> Result<(SqlFieldType, Option<u32>), SqlFrontendError> {
    map_sql_type(&column.sql_type)
        .map_err(|error| SqlFrontendError::new(error.kind, format!("column `{}` of table `{table}`: {}", column.name, error.message)))
}

fn map_sql_type(sql_type: &str) -> Result<(SqlFieldType, Option<u32>), SqlFrontendError> {
    let (base, parameters) = split_type(sql_type);
    let declared = parameters.and_then(|parameters| parameters.split(',').next()).and_then(|first| first.trim().parse::<u32>().ok());
    let field_type = match base {
        "TINYINT" if parameters == Some("1") => SqlFieldType::Bool,
        "BOOL" | "BOOLEAN" => SqlFieldType::Bool,
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER" | "INT2" | "INT4" | "SMALLSERIAL" | "SERIAL" => SqlFieldType::Int32,
        "BIGINT" | "INT8" | "BIGSERIAL" => SqlFieldType::Int64,
        "FLOAT" | "REAL" | "DOUBLE" | "DOUBLE PRECISION" | "FLOAT4" | "FLOAT8" => SqlFieldType::Float64,
        "VARCHAR" | "CHAR" | "CHARACTER" | "CHARACTER VARYING" => SqlFieldType::String,
        "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" => SqlFieldType::String,
        "DATE" => SqlFieldType::Date,
        "DATETIME" | "TIMESTAMP" | "TIMESTAMPTZ" | "TIMESTAMP WITH TIME ZONE" | "TIMESTAMP WITHOUT TIME ZONE" => SqlFieldType::Datetime,
        "TIME" => SqlFieldType::String,
        "UUID" => SqlFieldType::Uuid,
        // `DECIMAL(p,s)` keeps its value as decimal text; the declared precision
        // and scale are dialect display facts and do not narrow the gateway type.
        "DECIMAL" | "NUMERIC" | "DEC" | "FIXED" => SqlFieldType::Decimal,
        // JSON/JSONB 以文本存储（与 SQLite 同口径）；键路径访问运算符另行建设。
        "JSON" | "JSONB" => SqlFieldType::String,
        "BINARY" | "VARBINARY" | "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BYTEA" => SqlFieldType::Bytes,
        _ => {
            return Err(SqlFrontendError::unsupported(format!("column type `{sql_type}` has no 集群 SQL gateway equivalent")));
        }
    };
    let capacity = match base {
        "BLOB" => Some(MYSQL_BLOB_CAPACITY),
        "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BYTEA" => None,
        "BINARY" | "VARBINARY" | "VARCHAR" | "CHAR" | "CHARACTER" | "CHARACTER VARYING" => declared,
        _ => None,
    };
    Ok((field_type, capacity))
}

/// Splits `TINYINT(1)` / `DOUBLE PRECISION` into a base word and parameter text.
fn split_type(sql_type: &str) -> (&str, Option<&str>) {
    let Some(open) = sql_type.find('(')
    else {
        return (sql_type, None);
    };
    let close = sql_type.rfind(')').unwrap_or(sql_type.len());
    let parameters = if open + 1 < close { Some(&sql_type[open + 1..close]) } else { None };
    (sql_type[..open].trim_end(), parameters)
}
