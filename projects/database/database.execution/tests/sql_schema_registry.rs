//! `database_execution::sql_schema::SqlSchemaRegistry` 的正式入口 smoke。
//!
//! DDL 变更、约束投影、持久文档往返、序列重启恢复与回滚不回退。详细故障矩阵
//! 下移下游产品仓，这里只留上游最小行为证据。

use database_execution::sql_schema::{SqlSchemaRegistry, apply_auto_increment};
use database_query::{SqlCatalogBoundStatement, SqlDialect, SqlErrorKind, SqlFrontendError, parse_ddl};

fn apply(registry: &SqlSchemaRegistry, sql: &str) {
    let statement = parse_ddl(sql, SqlDialect::Postgres).expect("parse").expect("DDL statement");
    registry.apply_ddl(&statement).expect("apply DDL");
}

fn parse(sql: &str) -> database_query::SqlDdlStatement {
    parse_ddl(sql, SqlDialect::Postgres).expect("parse").expect("DDL statement")
}

fn temp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("yyds-sql-schema-{name}-{}-{}.json", std::process::id(), unique_suffix()))
}

fn unique_suffix() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos()
}

#[test]
fn added_column_default_reaches_the_catalog() {
    let registry = SqlSchemaRegistry::new_in_memory();
    apply(&registry, "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR(32))");
    apply(&registry, "ALTER TABLE users ADD COLUMN status VARCHAR(8) DEFAULT 'active'");
    let catalog = registry.catalog();
    let table = catalog.tables().iter().find(|table| table.name == "users").expect("users table");
    let field = table.fields.iter().find(|field| field.name == "status").expect("status column");
    assert_eq!(field.default.as_deref(), Some("'active'"));
}

#[test]
fn inline_and_table_constraints_are_stored_and_projected() {
    let registry = SqlSchemaRegistry::new_in_memory();
    apply(&registry, "CREATE TABLE tenants (id BIGINT PRIMARY KEY, name TEXT)");
    apply(
        &registry,
        "CREATE TABLE users (\
         id BIGSERIAL PRIMARY KEY, \
         tenant_id BIGINT NOT NULL REFERENCES tenants, \
         email TEXT CHECK (email <> ''), \
         quota INT DEFAULT 5, \
         CONSTRAINT quota_positive CHECK (quota >= 0), \
         FOREIGN KEY (tenant_id) REFERENCES tenants (id) ON DELETE CASCADE)",
    );
    let catalog = registry.catalog();
    let users = catalog_table(&catalog, "users");
    let tenants = catalog_table(&catalog, "tenants");
    let tenant_id = tenants.fields.iter().find(|field| field.name == "id").expect("tenant id").field_id;
    assert_eq!(users.check_constraints, vec!["quota >= 0".to_string()]);
    let email = users.fields.iter().find(|field| field.name == "email").expect("email column");
    assert_eq!(email.check.as_deref(), Some("email <> ''"));
    assert_eq!(users.foreign_keys.len(), 2, "inline and table-level references both survive");
    for key in &users.foreign_keys {
        assert_eq!(key.parent_type_id, tenants.type_id);
        assert_eq!(key.parent_columns, vec![tenant_id], "omitted column list resolves to the parent primary key");
    }
    assert!(users.foreign_keys.iter().any(|key| key.on_delete.as_deref() == Some("CASCADE")));
    let quota = users.fields.iter().find(|field| field.name == "quota").expect("quota column");
    assert_eq!(quota.default.as_deref(), Some("5"));
}

fn catalog_table<'a>(catalog: &'a database_query::SqlCatalog, name: &str) -> &'a database_query::SqlCatalogTable {
    catalog.tables().iter().find(|table| table.name == name).unwrap_or_else(|| panic!("table `{name}` missing"))
}

#[test]
fn foreign_keys_reject_unknown_parents_and_columns() {
    let registry = SqlSchemaRegistry::new_in_memory();
    apply(&registry, "CREATE TABLE tenants (id BIGINT PRIMARY KEY)");
    let error = registry.apply_ddl(&parse("CREATE TABLE users (tenant_id BIGINT REFERENCES ghosts)")).expect_err("missing parent");
    assert_eq!(error.kind, SqlErrorKind::UnknownTable);
    let error = registry
        .apply_ddl(&parse("CREATE TABLE users (tenant_id BIGINT REFERENCES tenants (ghost_col))"))
        .expect_err("missing parent column");
    assert_eq!(error.kind, SqlErrorKind::UnknownColumn);
    let error = registry
        .apply_ddl(&parse("CREATE TABLE users (id BIGINT PRIMARY KEY, a BIGINT, b BIGINT, FOREIGN KEY (a, b) REFERENCES tenants)"))
        .expect_err("arity mismatch");
    assert_eq!(error.kind, SqlErrorKind::Syntax);
}

#[test]
fn function_default_and_partial_index_predicate_are_stored() {
    let registry = SqlSchemaRegistry::new_in_memory();
    apply(&registry, "CREATE TABLE events (id BIGINT PRIMARY KEY, created_at TIMESTAMPTZ DEFAULT now(), kind TEXT)");
    apply(&registry, "CREATE INDEX idx_events_kind ON events (kind) WHERE kind <> 'internal'");
    let catalog = registry.catalog();
    let events = catalog_table(&catalog, "events");
    let created = events.fields.iter().find(|field| field.name == "created_at").expect("created_at column");
    assert_eq!(created.default.as_deref(), Some("now()"));
    let state = registry.snapshot();
    let index = state.indexes.iter().find(|index| index.name == "idx_events_kind").expect("partial index");
    assert_eq!(index.predicate.as_deref(), Some("kind <> 'internal'"));
}

#[test]
fn constraints_round_trip_through_the_persisted_document() {
    let path = temp_path("constraints");
    {
        let registry = SqlSchemaRegistry::open(&path).expect("open");
        apply(&registry, "CREATE TABLE tenants (id BIGINT PRIMARY KEY)");
        apply(
            &registry,
            "CREATE TABLE users (\
             id BIGINT PRIMARY KEY, \
             tenant_id BIGINT REFERENCES tenants ON DELETE CASCADE, \
             email TEXT CHECK (email <> ''))",
        );
        apply(&registry, "CREATE UNIQUE INDEX idx_email ON users (email) WHERE email IS NOT NULL");
    }
    let reopened = SqlSchemaRegistry::open(&path).expect("reopen");
    let catalog = reopened.catalog();
    let users = catalog_table(&catalog, "users");
    let email = users.fields.iter().find(|field| field.name == "email").expect("email column");
    assert_eq!(email.check.as_deref(), Some("email <> ''"));
    assert_eq!(users.foreign_keys.len(), 1);
    assert_eq!(users.foreign_keys[0].on_delete.as_deref(), Some("CASCADE"));
    let state = reopened.snapshot();
    let index = state.indexes.iter().find(|index| index.name == "idx_email").expect("index");
    assert_eq!(index.predicate.as_deref(), Some("email IS NOT NULL"));
    let _ = std::fs::remove_file(&path);
}

/// DDL 与序列重启恢复：分配并持久化的计数在重开后继续，回滚只回退目录、
/// 不回退已发放的序列值（合法序列回滚规则）。
#[test]
fn ddl_and_sequence_survive_restart_and_rollback() {
    let path = temp_path("sequence-restart");
    let first_value;
    {
        let registry = SqlSchemaRegistry::open(&path).expect("open");
        apply(&registry, "CREATE TABLE counters (id BIGSERIAL PRIMARY KEY, note TEXT)");
        let type_id = registry.catalog().tables()[0].type_id;
        let field_id = registry.auto_increment_fields(type_id)[0].field_id;
        first_value = registry.allocate_auto_increment(type_id, field_id).expect("allocate");
        assert_eq!(first_value, 1);
        // 失败的 DDL 不推进目录，也不留下半发布文档。
        let error = registry.apply_ddl(&parse("CREATE TABLE counters (id BIGINT)")).expect_err("duplicate table");
        assert_eq!(error.kind, SqlErrorKind::TableAlreadyExists);
    }
    let reopened = SqlSchemaRegistry::open(&path).expect("reopen");
    let type_id = reopened.catalog().tables()[0].type_id;
    let field_id = reopened.auto_increment_fields(type_id)[0].field_id;
    assert_eq!(reopened.snapshot().next_value(type_id, field_id), Some(2), "the consumed counter is persisted");
    let second_value = reopened.allocate_auto_increment(type_id, field_id).expect("allocate after restart");
    assert_eq!(second_value, 2, "a restart never repeats an observed value");

    // checkpoint/restore：目录变更回滚，序列计数不回退。
    let checkpoint = reopened.checkpoint();
    apply(&reopened, "CREATE TABLE dropped_later (id BIGINT PRIMARY KEY)");
    assert!(reopened.catalog().tables().iter().any(|table| table.name == "dropped_later"));
    reopened.restore(checkpoint).expect("restore");
    assert!(!reopened.catalog().tables().iter().any(|table| table.name == "dropped_later"), "DDL rolled back");
    assert_eq!(reopened.snapshot().next_value(type_id, field_id), Some(3), "allocated sequence survives the rollback");
    let reread = SqlSchemaRegistry::open(&path).expect("reread");
    assert_eq!(reread.snapshot().next_value(type_id, field_id), Some(3), "the restored state is persisted");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn malformed_document_never_resets_schema_state() {
    let path = temp_path("malformed");
    std::fs::write(&path, "{\"contract\":\"yyds-sql-schema/v9\"}").expect("write");
    let error = SqlSchemaRegistry::open(&path).expect_err("unknown contract must fail closed");
    assert_eq!(error.kind, SqlErrorKind::Internal);
    let _ = std::fs::remove_file(&path);
}

/// `apply_auto_increment` 只补 `VALUES` 形态 INSERT 省略的序列列，每行各取一个值。
#[test]
fn apply_auto_increment_fills_omitted_serial_columns() {
    let registry = SqlSchemaRegistry::new_in_memory();
    apply(&registry, "CREATE TABLE counters (id BIGSERIAL PRIMARY KEY, note TEXT)");
    let catalog = registry.catalog();
    let bound = database_query::bind_sql_catalog(
        "INSERT INTO counters (note) VALUES ('a'), ('b')",
        &catalog,
        catalog.schema_version(),
        SqlDialect::Postgres,
    )
    .expect("bind");
    let filled: SqlCatalogBoundStatement = apply_auto_increment(&registry, bound).expect("fill");
    let SqlCatalogBoundStatement::Insert(insert) = filled else {
        panic!("INSERT stays INSERT")
    };
    assert_eq!(insert.fields.len(), 2, "the serial column is appended");
    let allocated: Vec<i64> = insert
        .rows
        .iter()
        .map(|row| match row.last().expect("the serial column is appended last") {
            database_query::SqlCatalogBoundExpression::Literal(database_query::SqlBoundLiteral::Integer(value)) => *value,
            other => panic!("expected an allocated integer, got {other:?}"),
        })
        .collect();
    assert_eq!(allocated, vec![1, 2]);
}

/// 未注册的序列列直接失败，不静默补值。
#[test]
fn allocating_an_unknown_sequence_field_fails() {
    let registry = SqlSchemaRegistry::new_in_memory();
    apply(&registry, "CREATE TABLE plain (id BIGINT PRIMARY KEY)");
    let error: SqlFrontendError = registry.allocate_auto_increment(1, 99).expect_err("unknown field");
    assert!(error.message.contains("is not registered"));
}
