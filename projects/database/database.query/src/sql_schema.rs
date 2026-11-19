//! Read-only SQL schema state model and binding projection.
//!
//! The mutable catalog handle — DDL application, sequence allocation, checkpoints
//! and durable publishing — lives in `database_execution::sql_schema`. This module
//! only models what a published schema says and projects it onto the binding
//! catalog the SQL frontend consumes, so planning can read schema state without
//! owning files or mutation.
//!
//! The gateways start from an empty schema unless the host points them at a
//! catalog. Clients then describe tables with the DDL subset in [`crate::sql_ddl`]
//! and the execution-side registry keeps the resulting identities, optionally
//! saving them as a strict `yyds-sql-schema/v0` document so a restart sees the
//! same schema. Dropping a table only removes its metadata: rows already written
//! to the key/value store are retained on purpose.

use std::collections::BTreeSet;

use crate::{
    catalog_bind::{SqlCatalog, SqlCatalogField, SqlCatalogForeignKey, SqlCatalogTable, SqlFieldType},
    sql::SqlFrontendError,
};

/// Distance between the per-dialect DDL identity lanes.
const TYPE_ID_LANE_STRIDE: u64 = 1 << 32;

/// First type id a catalog-derived registry hands to DDL tables in `lane`.
///
/// Lane 0 is the catalog range and is never offset. MySQL uses lane 1 and
/// PostgreSQL lane 2, so both gateways can share one executor without writing
/// different tables under one type id.
fn catalog_type_id_lane(lane: u8) -> u64 {
    TYPE_ID_LANE_STRIDE * u64::from(lane)
}

/// One registered column as the schema state keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaField {
    /// Column name.
    pub name: String,
    /// Stable cluster field identity.
    pub field_id: u64,
    /// Gateway field type, nullable wrapping included.
    pub field_type: SqlFieldType,
    /// Declared character/octet capacity.
    pub capacity: Option<u32>,
    /// SQL type text as written.
    pub sql_type: String,
    /// `DEFAULT` expression text, stored as written.
    pub default: Option<String>,
    /// Whether writes that omit this column allocate a value from an owned
    /// sequence: PostgreSQL `SERIAL` / `BIGSERIAL` or MySQL `AUTO_INCREMENT`.
    pub auto_increment: bool,
    /// Next value the owned sequence hands out. Persisted with the schema so a
    /// restart never repeats a value the client already observed.
    pub next_value: u64,
    /// Inline `CHECK` expression text, stored as written inside the
    /// parentheses and re-parsed by the SQL frontend when a write validates it.
    pub check: Option<String>,
    /// Stored generated-column expression text. A non-`None` value means the
    /// column is computed from the row on every write and cannot be written
    /// directly.
    pub generated: Option<String>,
}

/// One registered table as the schema state keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaTable {
    /// Table name.
    pub name: String,
    /// Stable cluster type identity.
    pub type_id: u64,
    /// Primary key column names.
    pub primary_key: Vec<String>,
    /// Table-level `UNIQUE` constraint column names.
    pub unique_constraints: Vec<Vec<String>>,
    /// Table-level `CHECK` expression texts, stored as written inside the
    /// parentheses.
    pub check_constraints: Vec<String>,
    /// `FOREIGN KEY` constraints with the referenced columns already resolved
    /// to column names of the parent table.
    pub foreign_keys: Vec<SchemaForeignKey>,
    /// Columns in declaration order.
    pub fields: Vec<SchemaField>,
}

/// One stored foreign key: child column names, the referenced table, and the
/// referenced column names resolved at DDL time (the parent primary key when
/// the statement omitted a column list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaForeignKey {
    /// Child column names.
    pub columns: Vec<String>,
    /// Referenced table name.
    pub references_table: String,
    /// Referenced column names.
    pub references_columns: Vec<String>,
    /// `ON DELETE` action text.
    pub on_delete: Option<String>,
    /// `ON UPDATE` action text.
    pub on_update: Option<String>,
}

/// One registered index as the schema state keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaIndex {
    /// Index name.
    pub name: String,
    /// Owning table name.
    pub table: String,
    /// Whether the index enforces uniqueness.
    pub unique: bool,
    /// Indexed column names.
    pub columns: Vec<String>,
    /// Partial-index `WHERE` predicate text, stored as written.
    pub predicate: Option<String>,
}

/// The whole schema state a gateway registry keeps: identities, tables and
/// indexes at one revision.
///
/// This is read-only data for the query side. The execution-side registry
/// applies DDL to a state, advances sequence counters and publishes the result;
/// nothing here touches a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryState {
    /// Schema revision, nonzero and bumped by every applied change.
    pub revision: u32,
    /// Next table type identity to hand out.
    pub next_type_id: u64,
    /// Next column identity to hand out.
    pub next_field_id: u64,
    /// Registered tables in creation order.
    pub tables: Vec<SchemaTable>,
    /// Registered indexes in creation order.
    pub indexes: Vec<SchemaIndex>,
}

impl RegistryState {
    /// An empty state at revision `1`.
    pub fn empty() -> Self {
        Self { revision: 1, next_type_id: 1, next_field_id: 1, tables: Vec::new(), indexes: Vec::new() }
    }

    /// An empty state whose table and field identities live in `lane`.
    pub fn empty_in_lane(lane: u8) -> Self {
        let mut state = Self::empty();
        if lane > 0 {
            let stride = catalog_type_id_lane(lane);
            state.next_type_id = state.next_type_id.max(stride);
            state.next_field_id = state.next_field_id.max(stride);
        }
        state
    }

    /// The table registered under `name`, if any.
    pub fn table(&self, name: &str) -> Option<&SchemaTable> {
        self.tables.iter().find(|table| table.name == name)
    }

    /// Index of the table registered under `name` inside [`RegistryState::tables`].
    pub fn table_index(&self, name: &str) -> Option<usize> {
        self.tables.iter().position(|table| table.name == name)
    }

    /// Next sequence value of one field, when it owns a sequence.
    pub fn next_value(&self, type_id: u64, field_id: u64) -> Option<u64> {
        self.table_by_type_id(type_id)
            .and_then(|table| table.fields.iter().find(|field| field.field_id == field_id))
            .filter(|field| field.auto_increment)
            .map(|field| field.next_value)
    }

    /// The table registered with this type identity, if any.
    pub fn table_by_type_id(&self, type_id: u64) -> Option<&SchemaTable> {
        self.tables.iter().find(|table| table.type_id == type_id)
    }

    /// Seeds a state from a published or locally built catalog.
    ///
    /// DDL remains allowed on the execution side, but nothing here is written to
    /// disk. `lane` keeps identities unique across registries that share one
    /// binding view, because a catalog rejects a duplicate type or field
    /// identity.
    pub fn from_catalog(catalog: &SqlCatalog, lane: u8) -> Self {
        let tables = catalog
            .tables()
            .iter()
            .map(|table| SchemaTable {
                name: table.name.clone(),
                type_id: table.type_id,
                primary_key: Vec::new(),
                unique_constraints: Vec::new(),
                check_constraints: Vec::new(),
                foreign_keys: Vec::new(),
                fields: table
                    .fields
                    .iter()
                    .map(|field| SchemaField {
                        name: field.name.clone(),
                        field_id: field.field_id,
                        field_type: field.field_type.clone(),
                        capacity: field.capacity,
                        sql_type: canonical_sql_type(&field.field_type).to_string(),
                        default: None,
                        auto_increment: false,
                        next_value: 1,
                        check: None,
                        generated: None,
                    })
                    .collect(),
            })
            .collect();
        let mut state = RegistryState::empty().seeded(tables);
        if lane > 0 {
            state.next_type_id = state.next_type_id.max(catalog_type_id_lane(lane));
        }
        state
    }

    /// Fills identity counters above the seeded tables.
    pub fn seeded(mut self, tables: Vec<SchemaTable>) -> Self {
        self.next_type_id = tables.iter().map(|table| table.type_id).max().unwrap_or(0) + 1;
        self.next_field_id = tables.iter().flat_map(|table| table.fields.iter()).map(|field| field.field_id).max().unwrap_or(0) + 1;
        self.tables = tables;
        self
    }

    /// Projects the state onto the catalog the SQL frontend binds against.
    pub fn catalog(&self) -> SqlCatalog {
        let tables = self
            .tables
            .iter()
            .map(|table| {
                let mut unique_constraints: Vec<Vec<u64>> = table
                    .unique_constraints
                    .iter()
                    .map(|constraint| constraint.iter().filter_map(|name| field_id(table, name)).collect())
                    .collect();
                // `CREATE UNIQUE INDEX` registers an index, not a table constraint,
                // so the write path only enforces it when it is projected here.
                for index in self.indexes.iter().filter(|index| index.unique && index.table == table.name) {
                    let columns: Vec<u64> = index.columns.iter().filter_map(|name| field_id(table, name)).collect();
                    if !columns.is_empty() && !unique_constraints.contains(&columns) {
                        unique_constraints.push(columns);
                    }
                }
                // Foreign keys are projected onto field identities. A constraint
                // whose parent table was dropped afterwards disappears from the
                // projection, exactly as the write path can no longer enforce it.
                let mut foreign_keys = Vec::new();
                for key in &table.foreign_keys {
                    let Some(parent) = self.table(&key.references_table) else { continue };
                    let columns: Option<Vec<u64>> = key.columns.iter().map(|name| field_id(table, name)).collect();
                    let parent_columns: Option<Vec<u64>> = key.references_columns.iter().map(|name| field_id(parent, name)).collect();
                    if let (Some(columns), Some(parent_columns)) = (columns, parent_columns) {
                        foreign_keys.push(SqlCatalogForeignKey {
                            columns,
                            parent_type_id: parent.type_id,
                            parent_columns,
                            on_delete: key.on_delete.clone(),
                            on_update: key.on_update.clone(),
                        });
                    }
                }
                SqlCatalogTable {
                    type_id: table.type_id,
                    name: table.name.clone(),
                    fields: table
                        .fields
                        .iter()
                        .map(|field| SqlCatalogField {
                            field_id: field.field_id,
                            name: field.name.clone(),
                            field_type: field.field_type.clone(),
                            vos_type_name: None,
                            capacity: field.capacity,
                            default: field.default.clone(),
                            check: field.check.clone(),
                            generated: field.generated.clone(),
                        })
                        .collect(),
                    unique_constraints,
                    indexes: self
                        .indexes
                        .iter()
                        .filter(|index| !index.unique && index.table == table.name)
                        .filter_map(|index| {
                            let columns: Vec<u64> = index.columns.iter().filter_map(|name| field_id(table, name)).collect();
                            // 单列二级索引才有条目维护与探测路径；多列索引用作全表扫描。
                            (columns.len() == 1).then_some(columns)
                        })
                        .collect(),
                    check_constraints: table.check_constraints.clone(),
                    foreign_keys,
                }
            })
            .collect();
        SqlCatalog::assemble(self.revision, tables)
    }

    /// Returns every column of `type_id` that owns a value sequence.
    ///
    /// The gateway calls this after binding an INSERT: a column that the client
    /// omitted is filled from the registry, exactly as PostgreSQL defaults a
    /// serial column and MySQL defaults an `AUTO_INCREMENT` column.
    pub fn auto_increment_fields(&self, type_id: u64) -> Vec<SqlAutoIncrementField> {
        self.table_by_type_id(type_id)
            .map(|table| {
                table
                    .fields
                    .iter()
                    .filter(|field| field.auto_increment)
                    .map(|field| SqlAutoIncrementField { field_id: field.field_id, name: field.name.clone(), capacity: field.capacity })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Checks the model invariants: identity uniqueness and monotone counters,
    /// declared constraint shapes and sequence rules.
    ///
    /// Loading a persisted document runs this before the state becomes visible,
    /// so a corrupt or hand-edited file fails closed instead of silently
    /// resetting schema state.
    pub fn validate(&self) -> Result<(), SqlFrontendError> {
        if self.revision == 0 {
            return Err(SqlFrontendError::internal("SQL schema revision must be nonzero"));
        }
        let mut type_ids = BTreeSet::new();
        let mut field_ids = BTreeSet::new();
        for table in &self.tables {
            if table.name.is_empty() {
                return Err(SqlFrontendError::internal("SQL schema table has no name"));
            }
            if table.type_id == 0 || !type_ids.insert(table.type_id) {
                return Err(SqlFrontendError::internal("invalid or duplicate SQL schema type identity"));
            }
            for field in &table.fields {
                if field.name.is_empty() {
                    return Err(SqlFrontendError::internal("SQL schema field has no name"));
                }
                if field.field_id == 0 || !field_ids.insert(field.field_id) {
                    return Err(SqlFrontendError::internal("invalid or duplicate SQL schema field identity"));
                }
                if field.sql_type.trim().is_empty() {
                    return Err(SqlFrontendError::internal(format!(
                        "SQL schema field `{}` of table `{}` has no sql_type",
                        field.name, table.name
                    )));
                }
                if field.auto_increment {
                    if !matches!(field.field_type.inner(), SqlFieldType::Int32 | SqlFieldType::Int64) {
                        return Err(SqlFrontendError::internal(format!(
                            "SQL schema field `{}` of table `{}` owns a sequence but is not an integer column",
                            field.name, table.name
                        )));
                    }
                    if field.next_value == 0 {
                        return Err(SqlFrontendError::internal(format!(
                            "SQL schema field `{}` of table `{}` has a zero sequence counter",
                            field.name, table.name
                        )));
                    }
                }
                if let Some(expression) = &field.generated {
                    if expression.trim().is_empty() {
                        return Err(SqlFrontendError::internal(format!(
                            "SQL schema field `{}` of table `{}` declares an empty generated expression",
                            field.name, table.name
                        )));
                    }
                    if field.auto_increment {
                        return Err(SqlFrontendError::internal(format!(
                            "SQL schema field `{}` of table `{}` is generated and owns a sequence",
                            field.name, table.name
                        )));
                    }
                    if table.primary_key.iter().any(|column| column == &field.name) {
                        return Err(SqlFrontendError::internal(format!(
                            "SQL schema field `{}` of table `{}` is generated and part of the primary key",
                            field.name, table.name
                        )));
                    }
                }
            }
            for constraint in &table.unique_constraints {
                if constraint.is_empty() {
                    return Err(SqlFrontendError::internal(format!("SQL schema table `{}` has an empty unique constraint", table.name)));
                }
                for column in constraint {
                    if !table.fields.iter().any(|field| &field.name == column) {
                        return Err(SqlFrontendError::internal(format!(
                            "unique constraint column `{column}` does not exist in table `{}`",
                            table.name
                        )));
                    }
                }
            }
            for check in &table.check_constraints {
                if check.trim().is_empty() {
                    return Err(SqlFrontendError::internal(format!("SQL schema table `{}` has an empty CHECK constraint", table.name)));
                }
            }
            for key in &table.foreign_keys {
                if key.columns.is_empty() || key.columns.len() != key.references_columns.len() {
                    return Err(SqlFrontendError::internal(format!("SQL schema table `{}` has a malformed foreign key", table.name)));
                }
                for column in &key.columns {
                    if !table.fields.iter().any(|field| &field.name == column) {
                        return Err(SqlFrontendError::internal(format!(
                            "foreign key column `{column}` does not exist in table `{}`",
                            table.name
                        )));
                    }
                }
                let parent = self.table(&key.references_table).ok_or_else(|| {
                    SqlFrontendError::internal(format!(
                        "foreign key of table `{}` references missing table `{}`",
                        table.name, key.references_table
                    ))
                })?;
                for column in &key.references_columns {
                    if !parent.fields.iter().any(|field| &field.name == column) {
                        return Err(SqlFrontendError::internal(format!(
                            "foreign key column `{column}` does not exist in table `{}`",
                            key.references_table
                        )));
                    }
                }
            }
        }
        let next_type_id = type_ids.iter().max().copied().unwrap_or(0) + 1;
        if self.next_type_id < next_type_id {
            return Err(SqlFrontendError::internal("SQL schema type identity counter would reuse a registered identity"));
        }
        let next_field_id = field_ids.iter().max().copied().unwrap_or(0) + 1;
        if self.next_field_id < next_field_id {
            return Err(SqlFrontendError::internal("SQL schema field identity counter would reuse a registered identity"));
        }
        Ok(())
    }
}

fn field_id(table: &SchemaTable, name: &str) -> Option<u64> {
    table.fields.iter().find(|field| field.name == name).map(|field| field.field_id)
}

/// SQL type text reported for a catalog-seeded canonical field type.
fn canonical_sql_type(field_type: &SqlFieldType) -> &'static str {
    match field_type.inner() {
        SqlFieldType::Int32 => "INTEGER",
        SqlFieldType::Int64 => "BIGINT",
        SqlFieldType::Float64 => "DOUBLE PRECISION",
        SqlFieldType::String => "TEXT",
        SqlFieldType::Bool => "BOOLEAN",
        SqlFieldType::Bytes => "BYTEA",
        SqlFieldType::Decimal => "DECIMAL",
        SqlFieldType::Date => "DATE",
        SqlFieldType::Datetime => "DATETIME",
        SqlFieldType::Uuid => "UUID",
        SqlFieldType::Unknown | SqlFieldType::Optional(_) => "TEXT",
    }
}

/// One registered column that owns a value sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlAutoIncrementField {
    /// Stable 集群 field identity.
    pub field_id: u64,
    /// Catalog field name.
    pub name: String,
    /// Declared character/octet capacity of the column.
    pub capacity: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(name: &str, type_id: u64, fields: Vec<SchemaField>) -> SchemaTable {
        SchemaTable {
            name: name.into(),
            type_id,
            primary_key: vec!["id".into()],
            unique_constraints: Vec::new(),
            check_constraints: Vec::new(),
            foreign_keys: Vec::new(),
            fields,
        }
    }

    fn field(name: &str, field_id: u64, field_type: SqlFieldType) -> SchemaField {
        SchemaField {
            name: name.into(),
            field_id,
            field_type,
            capacity: None,
            sql_type: "INTEGER".into(),
            default: None,
            auto_increment: false,
            next_value: 1,
            check: None,
            generated: None,
        }
    }

    #[test]
    fn catalog_projection_keeps_declared_defaults_and_checks() {
        let mut email = field("email", 2, SqlFieldType::String);
        email.check = Some("email <> ''".into());
        email.default = Some("''".into());
        let state = RegistryState::empty().seeded(vec![table("users", 1, vec![field("id", 1, SqlFieldType::Int32), email])]);
        state.validate().expect("valid");
        let catalog = state.catalog();
        let users = catalog.tables().iter().find(|table| table.name == "users").expect("users");
        let email = users.fields.iter().find(|field| field.name == "email").expect("email");
        assert_eq!(email.check.as_deref(), Some("email <> ''"));
        assert_eq!(email.default.as_deref(), Some("''"));
    }

    #[test]
    fn duplicate_identities_fail_validation() {
        let state = RegistryState::empty().seeded(vec![
            table("a", 1, vec![field("id", 1, SqlFieldType::Int32)]),
            table("b", 1, vec![field("id", 2, SqlFieldType::Int32)]),
        ]);
        let error = state.validate().expect_err("duplicate type id");
        assert!(error.message.contains("type identity"));
    }

    #[test]
    fn sequence_listing_reports_only_owning_columns() {
        let mut serial = field("id", 1, SqlFieldType::Int64);
        serial.auto_increment = true;
        serial.next_value = 9;
        let state = RegistryState::empty().seeded(vec![table("users", 4, vec![serial, field("quota", 2, SqlFieldType::Int32)])]);
        let owned = state.auto_increment_fields(4);
        assert_eq!(owned.iter().map(|field| field.field_id).collect::<Vec<_>>(), vec![1]);
        assert_eq!(state.next_value(4, 1), Some(9));
        assert_eq!(state.next_value(4, 2), None);
    }
}
