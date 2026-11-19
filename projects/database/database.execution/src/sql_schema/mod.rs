//! 本地 SQL schema 注册表：执行侧的正式 schema/sequence 入口。
//!
//! PG 与 MySQL 网关共用这一份可变目录：DDL 变更状态机在 [`ddl`]，序列分配与
//! checkpoint/restore 的状态迁移在本模块，`yyds-sql-schema/v0` 文档编解码归
//! `database.formats::yy::sql_schema`，temp/rename 原子发布归
//! `database.storage::local::publish`。查询侧（`database.query::sql_schema`）只
//! 保留只读状态模型与绑定投影，不再持文件 IO 与可变发布。
//!
//! 注册表默认只在内存中推进；宿主给出文件路径后，每次改变状态的 DDL、序列分配
//! 与 checkpoint 恢复都会把完整状态原子发布到该路径，重启读回同一 schema。
//! 删除表只移除元数据：已写入键值存储的行按设计保留。

mod ddl;

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};

use database_formats::yy::sql_schema::{
    SqlSchemaDocument, SqlSchemaFieldDocument, SqlSchemaFieldTypeDocument, SqlSchemaForeignKeyDocument, SqlSchemaIndexDocument,
    SqlSchemaTableDocument, decode, encode,
};
use database_query::{
    RegistryState, SchemaField, SchemaForeignKey, SchemaIndex, SchemaTable, SqlAutoIncrementField, SqlCatalog, SqlDdlStatement, SqlErrorKind,
    SqlFieldType, SqlFrontendError,
    catalog_bind::{SqlCatalogBoundColumn, SqlCatalogBoundExpression, SqlCatalogBoundInsert, SqlCatalogBoundStatement},
    sql::SqlBoundLiteral,
};
use database_storage::local::publish::publish_atomic;

pub use ddl::SqlDdlOutcome;
use ddl::apply_ddl;

/// Local schema registry shared by a gateway runtime.
#[derive(Debug)]
pub struct SqlSchemaRegistry {
    state: Mutex<RegistryState>,
    path: Option<PathBuf>,
}

/// Opaque registry snapshot used to discard DDL from a failed batch or a
/// rolled-back transaction.
#[derive(Debug, Clone)]
pub struct SqlSchemaCheckpoint {
    state: RegistryState,
}

/// 为 INSERT 补齐客户端未给出的 AUTO_INCREMENT / serial 列。
///
/// 只处理 `VALUES` 形态的 INSERT；`INSERT … SELECT` 原样返回。每行各自
/// [`SqlSchemaRegistry::allocate_auto_increment`]，计数在执行前推进，回滚不回退。
pub fn apply_auto_increment(
    schema: &SqlSchemaRegistry,
    statement: SqlCatalogBoundStatement,
) -> Result<SqlCatalogBoundStatement, SqlFrontendError> {
    let SqlCatalogBoundStatement::Insert(insert) = statement
    else {
        return Ok(statement);
    };
    Ok(SqlCatalogBoundStatement::Insert(apply_auto_increment_insert(schema, insert)?))
}

fn apply_auto_increment_insert(
    schema: &SqlSchemaRegistry,
    mut insert: SqlCatalogBoundInsert,
) -> Result<SqlCatalogBoundInsert, SqlFrontendError> {
    if insert.select.is_some() {
        return Ok(insert);
    }
    let type_id = insert.table.type_id;
    let auto_fields = schema.auto_increment_fields(type_id);
    for auto in auto_fields {
        if insert.fields.iter().any(|field| field.field_id == auto.field_id) {
            continue;
        }
        let column = SqlCatalogBoundColumn {
            type_id,
            field_id: auto.field_id,
            row_id: insert.table.row_id(auto.field_id),
            table_name: insert.table.name.clone(),
            field_name: auto.name.clone(),
            visible_name: auto.name.clone(),
            capacity: auto.capacity,
        };
        insert.fields.push(column);
        for row in &mut insert.rows {
            let value = schema.allocate_auto_increment(type_id, auto.field_id)?;
            row.push(SqlCatalogBoundExpression::Literal(SqlBoundLiteral::Integer(value)));
        }
    }
    Ok(insert)
}

impl SqlSchemaRegistry {
    /// Creates an empty in-memory registry at revision `1` without persistence.
    pub fn new_in_memory() -> Self {
        Self { state: Mutex::new(RegistryState::empty()), path: None }
    }

    /// Creates an empty in-memory registry whose table and field identities live
    /// in `lane`.
    ///
    /// Gateways that merge several registries into one binding view need the
    /// identities to stay unique across them, because a catalog rejects a
    /// duplicate type or field identity. The session's temporary tables use a
    /// lane of their own for exactly that reason.
    pub fn new_in_memory_in_lane(lane: u8) -> Self {
        Self { state: Mutex::new(RegistryState::empty_in_lane(lane)), path: None }
    }

    /// Seeds an in-memory registry from a published or locally built catalog.
    ///
    /// DDL remains allowed, but nothing is written to disk.
    pub fn from_catalog(catalog: &SqlCatalog) -> Self {
        Self::from_catalog_with_lane(catalog, 0)
    }

    /// Seeds a catalog registry whose later DDL tables live in one dialect lane.
    ///
    /// Two gateways started from the same catalog would otherwise hand out the
    /// same type id to their next `CREATE TABLE`, so a MySQL table and a
    /// PostgreSQL table would share one row namespace.
    pub fn from_catalog_with_lane(catalog: &SqlCatalog, lane: u8) -> Self {
        Self { state: Mutex::new(RegistryState::from_catalog(catalog, lane)), path: None }
    }

    /// Opens a persisted registry, creating an empty state when the file is absent.
    ///
    /// A malformed file or an unknown contract tag is an error — the registry
    /// never resets schema state silently.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, SqlFrontendError> {
        let path = path.into();
        let state = if path.exists() { load(&path)? } else { RegistryState::empty() };
        Ok(Self { state: Mutex::new(state), path: Some(path) })
    }

    /// Opens a persisted registry, seeding it from `catalog` when the file is
    /// still absent, and saving that seed so later restarts read it back.
    ///
    /// An existing file always wins: the catalog only seeds a directory that
    /// has no schema document yet, so a restart never reverts the DDL applied
    /// after the catalog was published.
    pub fn open_or_seeded(path: impl Into<PathBuf>, catalog: &SqlCatalog, lane: u8) -> Result<Self, SqlFrontendError> {
        let path = path.into();
        if path.exists() {
            return Self::open(path);
        }
        let state = RegistryState::from_catalog(catalog, lane);
        save(&path, &state)?;
        Ok(Self { state: Mutex::new(state), path: Some(path) })
    }

    /// Returns the current schema revision.
    pub fn revision(&self) -> u32 {
        self.lock().revision
    }

    /// Returns a consistent snapshot of the registered schema.
    pub fn catalog(&self) -> SqlCatalog {
        self.lock().catalog()
    }

    /// Cloned registry state for read-only metadata synthesis.
    pub fn snapshot(&self) -> RegistryState {
        self.lock().clone()
    }

    /// Captures the whole registry state so a failed batch or aborted
    /// transaction can discard every DDL change made since the capture.
    ///
    /// The checkpoint is opaque: it is a full state copy, not a delta, and it
    /// is never merged with a newer state.
    pub fn checkpoint(&self) -> SqlSchemaCheckpoint {
        SqlSchemaCheckpoint { state: self.snapshot() }
    }

    /// Restores a previously captured checkpoint and persists the restored
    /// state when the registry is file-backed.
    ///
    /// Callers restore only on an explicit rollback path. Committing a
    /// transaction never needs to restore a checkpoint.
    ///
    /// Sequence counters are the one exception to a full restore: PostgreSQL
    /// `nextval` and MySQL `AUTO_INCREMENT` allocations survive a rollback, so
    /// the restored fields keep the higher counter the rollback already
    /// consumed instead of handing the same value out twice.
    pub fn restore(&self, checkpoint: SqlSchemaCheckpoint) -> Result<(), SqlFrontendError> {
        let mut guard = self.lock();
        let current = guard.clone();
        let mut restored = checkpoint.state;
        for table in &mut restored.tables {
            for field in &mut table.fields {
                if let Some(consumed) = current.next_value(table.type_id, field.field_id) {
                    field.next_value = field.next_value.max(consumed);
                }
            }
        }
        *guard = restored;
        if let Some(path) = &self.path {
            save(path, &guard)?;
        }
        Ok(())
    }

    /// Returns every column of `type_id` that owns a value sequence.
    ///
    /// The gateway calls this after binding an INSERT: a column that the client
    /// omitted is filled from the registry, exactly as PostgreSQL defaults a
    /// serial column and MySQL defaults an `AUTO_INCREMENT` column.
    pub fn auto_increment_fields(&self, type_id: u64) -> Vec<SqlAutoIncrementField> {
        self.lock().auto_increment_fields(type_id)
    }

    /// Allocates the next value from the sequence owned by one column.
    ///
    /// The counter advances before the caller executes the write and the new
    /// value is persisted immediately, so neither a failed statement nor a
    /// rolled-back transaction reuses it. That is PostgreSQL's `nextval`
    /// contract and MySQL's `AUTO_INCREMENT` contract for a committed-value
    /// sequence.
    pub fn allocate_auto_increment(&self, type_id: u64, field_id: u64) -> Result<i64, SqlFrontendError> {
        let mut guard = self.lock();
        let mut next = guard.clone();
        let field = next
            .tables
            .iter_mut()
            .find(|table| table.type_id == type_id)
            .and_then(|table| table.fields.iter_mut().find(|field| field.field_id == field_id))
            .ok_or_else(|| SqlFrontendError::internal(format!("auto increment field {field_id} is not registered")))?;
        if !field.auto_increment {
            return Err(SqlFrontendError::internal(format!("field `{}` does not own a value sequence", field.name)));
        }
        let value = i64::try_from(field.next_value)
            .map_err(|_| SqlFrontendError::new(SqlErrorKind::NumericOverflow, "auto increment sequence exhausted the 64-bit signed range"))?;
        field.next_value += 1;
        if let Some(path) = &self.path {
            save(path, &next)?;
        }
        *guard = next;
        Ok(value)
    }

    /// Applies one DDL statement and persists the result when the state changed.
    pub fn apply_ddl(&self, statement: &SqlDdlStatement) -> Result<SqlDdlOutcome, SqlFrontendError> {
        let mut guard = self.lock();
        let mut next = guard.clone();
        let (outcome, changed) = apply_ddl(&mut next, statement)?;
        if changed {
            next.revision = next.revision.saturating_add(1);
            if let Some(path) = &self.path {
                save(path, &next)?;
            }
            *guard = next;
        }
        Ok(outcome)
    }

    fn lock(&self) -> MutexGuard<'_, RegistryState> {
        self.state.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

/// Reads one persisted schema document and validates the state it describes.
fn load(path: &Path) -> Result<RegistryState, SqlFrontendError> {
    let source = fs::read_to_string(path)
        .map_err(|error| SqlFrontendError::internal(format!("cannot read SQL schema `{}`: {error}", path.display())))?;
    let document =
        decode(&source).map_err(|error| SqlFrontendError::internal(format!("cannot parse SQL schema `{}`: {error}", path.display())))?;
    let state = state_from_document(document);
    state.validate()?;
    Ok(state)
}

/// Encodes the state into the contract document and publishes it atomically.
///
/// The publish result says whether the commit point was crossed and whether the
/// old document is still intact; the registry only swaps memory state after a
/// durable receipt.
fn save(path: &Path, state: &RegistryState) -> Result<(), SqlFrontendError> {
    let document = document_from_state(state);
    let json = encode(&document).map_err(|error| SqlFrontendError::internal(format!("cannot encode SQL schema: {error}")))?;
    publish_atomic(path, json.as_bytes()).map(|_receipt| ()).map_err(|error| {
        SqlFrontendError::internal(format!(
            "cannot publish SQL schema `{}` at {:?} ({:?}, retryable={}): {error}",
            path.display(),
            error.stage(),
            error.outcome(),
            error.retryable()
        ))
    })
}

fn document_from_state(state: &RegistryState) -> SqlSchemaDocument {
    SqlSchemaDocument {
        contract: database_formats::yy::sql_schema::CONTRACT_TAG.to_string(),
        revision: state.revision,
        next_type_id: state.next_type_id,
        next_field_id: state.next_field_id,
        tables: state
            .tables
            .iter()
            .map(|table| SqlSchemaTableDocument {
                name: table.name.clone(),
                type_id: table.type_id,
                primary_key: table.primary_key.clone(),
                unique_constraints: table.unique_constraints.clone(),
                check_constraints: table.check_constraints.clone(),
                foreign_keys: table
                    .foreign_keys
                    .iter()
                    .map(|key| SqlSchemaForeignKeyDocument {
                        columns: key.columns.clone(),
                        references_table: key.references_table.clone(),
                        references_columns: key.references_columns.clone(),
                        on_delete: key.on_delete.clone(),
                        on_update: key.on_update.clone(),
                    })
                    .collect(),
                fields: table
                    .fields
                    .iter()
                    .map(|field| SqlSchemaFieldDocument {
                        name: field.name.clone(),
                        field_id: field.field_id,
                        field_type: field_type_document(&field.field_type),
                        nullable: field.field_type.is_nullable(),
                        capacity: field.capacity,
                        sql_type: field.sql_type.clone(),
                        default: field.default.clone(),
                        auto_increment: field.auto_increment,
                        next_value: field.next_value,
                        check: field.check.clone(),
                        generated: field.generated.clone(),
                    })
                    .collect(),
            })
            .collect(),
        indexes: state
            .indexes
            .iter()
            .map(|index| SqlSchemaIndexDocument {
                name: index.name.clone(),
                table: index.table.clone(),
                unique: index.unique,
                columns: index.columns.clone(),
                predicate: index.predicate.clone(),
            })
            .collect(),
    }
}

fn state_from_document(document: SqlSchemaDocument) -> RegistryState {
    RegistryState {
        revision: document.revision,
        next_type_id: document.next_type_id,
        next_field_id: document.next_field_id,
        tables: document
            .tables
            .into_iter()
            .map(|table| SchemaTable {
                name: table.name,
                type_id: table.type_id,
                primary_key: table.primary_key,
                unique_constraints: table.unique_constraints,
                check_constraints: table.check_constraints,
                foreign_keys: table
                    .foreign_keys
                    .into_iter()
                    .map(|key| SchemaForeignKey {
                        columns: key.columns,
                        references_table: key.references_table,
                        references_columns: key.references_columns,
                        on_delete: key.on_delete,
                        on_update: key.on_update,
                    })
                    .collect(),
                fields: table
                    .fields
                    .into_iter()
                    .map(|field| {
                        let base = field_type_base(field.field_type);
                        let field_type = if field.nullable { SqlFieldType::Optional(Box::new(base)) } else { base };
                        SchemaField {
                            name: field.name,
                            field_id: field.field_id,
                            field_type,
                            capacity: field.capacity,
                            sql_type: field.sql_type,
                            default: field.default,
                            auto_increment: field.auto_increment,
                            next_value: field.next_value,
                            check: field.check,
                            generated: field.generated,
                        }
                    })
                    .collect(),
            })
            .collect(),
        indexes: document
            .indexes
            .into_iter()
            .map(|index| SchemaIndex {
                name: index.name,
                table: index.table,
                unique: index.unique,
                columns: index.columns,
                predicate: index.predicate,
            })
            .collect(),
    }
}

fn field_type_document(field_type: &SqlFieldType) -> SqlSchemaFieldTypeDocument {
    match field_type.inner() {
        SqlFieldType::Int32 => SqlSchemaFieldTypeDocument::Int32,
        SqlFieldType::Int16 => SqlSchemaFieldTypeDocument::Int16,
        SqlFieldType::Int64 => SqlSchemaFieldTypeDocument::Int64,
        SqlFieldType::Float64 => SqlSchemaFieldTypeDocument::Float64,
        SqlFieldType::String => SqlSchemaFieldTypeDocument::String,
        SqlFieldType::Bool => SqlSchemaFieldTypeDocument::Bool,
        SqlFieldType::Bytes => SqlSchemaFieldTypeDocument::Bytes,
        SqlFieldType::Decimal => SqlSchemaFieldTypeDocument::Decimal,
        SqlFieldType::Date => SqlSchemaFieldTypeDocument::Date,
        SqlFieldType::Datetime => SqlSchemaFieldTypeDocument::Datetime,
        SqlFieldType::Uuid => SqlSchemaFieldTypeDocument::Uuid,
        SqlFieldType::Json => SqlSchemaFieldTypeDocument::Json,
        SqlFieldType::Optional(_) | SqlFieldType::Unknown => SqlSchemaFieldTypeDocument::Unknown,
    }
}

fn field_type_base(document: SqlSchemaFieldTypeDocument) -> SqlFieldType {
    match document {
        SqlSchemaFieldTypeDocument::Int32 => SqlFieldType::Int32,
        SqlSchemaFieldTypeDocument::Int16 => SqlFieldType::Int16,
        SqlSchemaFieldTypeDocument::Int64 => SqlFieldType::Int64,
        SqlSchemaFieldTypeDocument::Float64 => SqlFieldType::Float64,
        SqlSchemaFieldTypeDocument::String => SqlFieldType::String,
        SqlSchemaFieldTypeDocument::Bool => SqlFieldType::Bool,
        SqlSchemaFieldTypeDocument::Bytes => SqlFieldType::Bytes,
        SqlSchemaFieldTypeDocument::Decimal => SqlFieldType::Decimal,
        SqlSchemaFieldTypeDocument::Date => SqlFieldType::Date,
        SqlSchemaFieldTypeDocument::Datetime => SqlFieldType::Datetime,
        SqlSchemaFieldTypeDocument::Uuid => SqlFieldType::Uuid,
        SqlSchemaFieldTypeDocument::Json => SqlFieldType::Json,
        SqlSchemaFieldTypeDocument::Unknown => SqlFieldType::Unknown,
    }
}
