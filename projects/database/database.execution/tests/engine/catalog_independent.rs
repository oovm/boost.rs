//! The durable catalog must not depend on any source dialect.
//!
//! CAT-01 publishes a hand-built resolved contract plus its identity ledger
//! through `install_catalog` and proves the database survives a reopen with
//! every identity, constraint and row intact — without a single VOS or SQL
//! string. PLAN-01 proves the hand-built plan entry point and the two text
//! frontends agree row by row over the same data.

use database_execution::engine::{CatalogDefinition, Connection, PlanOutcome};
use database_query::local::{BoundValue, Dialect, PreparedStatement, QueryKind, QueryOp, WriteField, WriteOp};
use database_types::Value;
use schema_types::{
    AttributeContract, BoundFieldContract, BoundTypeContract, CanonicalScalar, CanonicalType, IDENTITY_HISTORY_FORMAT_VERSION,
    IDENTITY_MANIFEST_VERSION, IdentityBoundProjection, IdentityHistory, RESOLVED_CONTRACT_FORMAT_VERSION, ResolvedCanonicalType,
    ResolvedContract, ResolvedFieldContract, ResolvedTypeContract, TypeContractKind, resolved_schema_fingerprint,
};

fn path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("database-execution-catalog-independent-{}-{label}.yydb", std::process::id()))
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

fn attribute(name: &str, syntax: &str) -> AttributeContract {
    AttributeContract { name: name.to_owned(), syntax: syntax.to_owned(), span: (0_usize..0_usize).into() }
}

fn scalar_field(field_id: u64, slot: u32, name: &str, scalar: CanonicalScalar, attributes: Vec<AttributeContract>) -> ResolvedFieldContract {
    ResolvedFieldContract {
        field_id,
        virtual_field_index: Some(slot),
        canonical_name: name.to_owned(),
        canonical_type: ResolvedCanonicalType::Scalar(scalar),
        attributes,
        default_value: None,
    }
}

/// Two tables built straight from the contract types: `Account` with an `i64`
/// primary key and a unique `utf8` column, `Session` with a `uuid` primary key.
fn contract() -> ResolvedContract {
    let types = vec![
        ResolvedTypeContract {
            type_id: 1,
            canonical_path: vec!["Account".to_owned()],
            kind: TypeContractKind::Table,
            fields: vec![
                scalar_field(1, 0, "id", CanonicalScalar::I64, vec![attribute("primary", "@@id")]),
                scalar_field(2, 1, "email", CanonicalScalar::Utf8, vec![attribute("unique", "unique")]),
            ],
        },
        ResolvedTypeContract {
            type_id: 2,
            canonical_path: vec!["Session".to_owned()],
            kind: TypeContractKind::Table,
            fields: vec![scalar_field(3, 0, "key", CanonicalScalar::Uuid, vec![attribute("primary", "@@key")])],
        },
    ];
    ResolvedContract {
        format_version: RESOLVED_CONTRACT_FORMAT_VERSION.to_owned(),
        identity_manifest_version: IDENTITY_MANIFEST_VERSION.to_owned(),
        schema_fingerprint: resolved_schema_fingerprint(&types),
        types,
    }
}

/// The identity ledger matching [`contract`], spelled in bound-projection
/// form rather than derived from any document.
fn history() -> IdentityHistory {
    let bound = |field_id: u64, slot: u32, name: &str, path: &[&str], attributes: Vec<AttributeContract>| BoundFieldContract {
        field_id,
        virtual_field_index: Some(slot),
        canonical_name: name.to_owned(),
        canonical_type: CanonicalType::Named(path.iter().map(|segment| (*segment).to_owned()).collect()),
        attributes,
        default_value: None,
    };
    IdentityHistory {
        format_version: IDENTITY_HISTORY_FORMAT_VERSION.to_owned(),
        manifest_version: IDENTITY_MANIFEST_VERSION.to_owned(),
        revision: 1,
        layout_epoch: 1,
        snapshot: IdentityBoundProjection {
            manifest_version: IDENTITY_MANIFEST_VERSION.to_owned(),
            types: vec![
                BoundTypeContract {
                    type_id: 1,
                    canonical_path: vec!["Account".to_owned()],
                    kind: TypeContractKind::Table,
                    fields: vec![
                        bound(1, 0, "id", &["i64"], vec![attribute("primary", "@@id")]),
                        bound(2, 1, "email", &["utf8"], vec![attribute("unique", "unique")]),
                    ],
                },
                BoundTypeContract {
                    type_id: 2,
                    canonical_path: vec!["Session".to_owned()],
                    kind: TypeContractKind::Table,
                    fields: vec![bound(3, 0, "key", &["uuid"], vec![attribute("primary", "@@key")])],
                },
            ],
        },
        retired_types: Vec::new(),
        retired_fields: Vec::new(),
    }
}

/// A hand-built insert program: literal values, no parameters, no source text.
fn insert_plan(table: &str, rows: Vec<Vec<(&str, Value)>>) -> PreparedStatement {
    PreparedStatement {
        statement_id: format!("hand-insert-{table}"),
        dialect: Dialect::Vos,
        ops: Vec::new(),
        write_ops: vec![WriteOp::Insert {
            table: table.to_owned(),
            rows: rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|(name, value)| WriteField { name: (*name).to_owned(), value: BoundValue::Literal(value.clone()) })
                        .collect()
                })
                .collect(),
            conflict: None,
            returning: None,
        }],
        session_ops: Vec::new(),
        query_kind: QueryKind::Write,
        parameters: Vec::new(),
        read_set: Vec::new(),
        write_set: vec![table.to_owned()],
        source_fingerprint: String::new(),
        source: String::new(),
        schema_fingerprint: String::new(),
        schema_revision: 0,
    }
}

/// A hand-built whole-table read: `Scan` straight into `Collect`.
fn scan_plan(table: &str) -> PreparedStatement {
    PreparedStatement {
        statement_id: format!("hand-scan-{table}"),
        dialect: Dialect::Vos,
        ops: vec![QueryOp::Scan { table: table.to_owned() }, QueryOp::Collect],
        write_ops: Vec::new(),
        session_ops: Vec::new(),
        query_kind: QueryKind::Read,
        parameters: Vec::new(),
        read_set: vec![table.to_owned()],
        write_set: Vec::new(),
        source_fingerprint: String::new(),
        source: String::new(),
        schema_fingerprint: String::new(),
        schema_revision: 0,
    }
}

fn rows(outcome: PlanOutcome) -> Vec<database_query::local::QueryRow> {
    match outcome {
        PlanOutcome::Rows(rows) => rows,
        PlanOutcome::Affected(affected) => panic!("read plan must return rows, got {affected} affected"),
    }
}

#[test]
fn cat_01_installed_catalog_survives_reopen_without_any_source_text() {
    let path = path("cat01");
    cleanup(&path);
    let contract = contract();
    let history = history();
    let key = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
    {
        let conn = Connection::open(&path).unwrap();
        conn.install_catalog(CatalogDefinition::new(contract.clone(), history.clone(), 1, None)).unwrap();
        assert_eq!(conn.execute_plan(&insert_plan("Account", vec![vec![("id", Value::I64(1)), ("email", Value::Text("ada@example.com".into()))]]), &[]).unwrap(), PlanOutcome::Affected(1));
        assert_eq!(conn.execute_plan(&insert_plan("Session", vec![vec![("key", Value::Uuid(key))]]), &[]).unwrap(), PlanOutcome::Affected(1));
        let account = rows(conn.execute_plan(&scan_plan("Account"), &[]).unwrap());
        assert_eq!(account.len(), 1);
        assert_eq!(account[0].get("id"), Some(&Value::I64(1)));
        assert_eq!(account[0].get("email"), Some(&Value::Text("ada@example.com".into())));
    }
    {
        let conn = Connection::open(&path).unwrap();
        assert_eq!(conn.identity_history().unwrap().as_ref(), Some(&history), "the durable ledger must round-trip unchanged");
        assert_eq!(conn.resolved_contract().unwrap().as_deref(), Some(&contract), "the contract must round-trip unchanged");
        let catalog = conn.execution_catalog().unwrap().expect("the published catalog must survive reopen");
        let names: Vec<&str> = catalog.types.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, vec!["Account", "Session"]);
        let constraint_names: Vec<&str> = catalog.types[0].unique_constraints.iter().map(|constraint| constraint.name()).collect();
        assert_eq!(constraint_names, vec!["Account_email_key"], "the declared unique rule must come from the contract attributes");
        let account = rows(conn.execute_plan(&scan_plan("Account"), &[]).unwrap());
        assert_eq!(account.len(), 1);
        let sessions = rows(conn.execute_plan(&scan_plan("Session"), &[]).unwrap());
        assert_eq!(sessions[0].get("key"), Some(&Value::Uuid(key)));
        // Uniqueness keeps being enforced after a reopen, still without text.
        let duplicate =
            conn.execute_plan(&insert_plan("Account", vec![vec![("id", Value::I64(2)), ("email", Value::Text("ada@example.com".into()))]]), &[]);
        assert!(matches!(duplicate, Err(database_types::Error::UniqueViolation { .. })), "{duplicate:?}");
    }
    cleanup(&path);
}

#[test]
fn plan_01_three_entry_points_agree_row_by_row() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema("table Note { @@id: i64, body: utf8 }").unwrap();
    // One row per entry point: hand-built plan, SQL text, VOS text.
    conn.execute_plan(&insert_plan("Note", vec![vec![("id", Value::I64(1)), ("body", Value::Text("same".into()))]]), &[]).unwrap();
    conn.execute_sql_write("INSERT INTO Note (id, body) VALUES (2, 'same')", &[]).unwrap();
    conn.execute_vos_write(r#"Note.insert({ id: 3, body: "same" })"#).unwrap();
    let via_plan = rows(conn.execute_plan(&scan_plan("Note"), &[]).unwrap());
    let via_sql = conn.query_sql("SELECT id, body FROM Note").unwrap();
    let via_vos = conn.query_vos("Note.all().collect()").unwrap();
    assert_eq!(via_plan.len(), 3);
    assert_eq!(via_plan, via_sql, "the plan and SQL readers must agree row by row");
    assert_eq!(via_plan, via_vos, "the plan and VOS readers must agree row by row");
}
