//! Unique constraint metadata and enforcement on the VOS write path.

use std::collections::BTreeMap;

use database_execution::local::execute;
use database_execution::local::execute_write_with_constraints;
use database_query::local::resolved_contract_from_document;
use database_types::{Error, TableUniqueConstraint, UniqueConstraint, Value};
use schema_types::ResolvedContract;

const ACCOUNT_SCHEMA: &str = r#"
table Account {
    @@account_id: i64,
    @email: utf8,
    tenant: utf8,
    handle: utf8,
}
"#;

fn contract() -> ResolvedContract {
    resolved_contract_from_document(ACCOUNT_SCHEMA).expect("account schema resolves")
}

fn declared(name: &str, columns: &[&str], nulls_distinct: bool) -> TableUniqueConstraint {
    let constraint =
        UniqueConstraint::new(name, columns.iter().map(|column| (*column).to_owned()).collect(), nulls_distinct).expect("constraint shape");
    TableUniqueConstraint::new("Account", constraint).expect("table binding")
}

fn literal(value: Option<&str>) -> String {
    match value {
        Some(text) => format!("\"{text}\""),
        None => "null".to_owned(),
    }
}

fn insert(
    contract: &ResolvedContract,
    declared: &[TableUniqueConstraint],
    records: &mut BTreeMap<String, Vec<u8>>,
    account_id: i64,
    email: Option<&str>,
    tenant: &str,
    handle: Option<&str>,
) -> database_types::Result<()> {
    let source = format!(
        r#"Account::insert({{
            account_id: {account_id},
            email: {},
            tenant: "{tenant}",
            handle: {},
        }})"#,
        literal(email),
        literal(handle),
    );
    execute_write_with_constraints(&source, contract, declared, None, records)
}

fn conflict(error: Error) -> (String, Vec<String>) {
    match error {
        Error::UniqueViolation { constraint, columns, .. } => (constraint, columns),
        other => panic!("expected a unique violation, got {other}"),
    }
}

#[test]
fn unique_attribute_rejects_a_duplicate_single_column_value() {
    let contract = contract();
    let mut records = BTreeMap::new();
    insert(&contract, &[], &mut records, 1, Some("ada@example.test"), "t1", Some("ada")).expect("first insert");
    let (name, columns) =
        conflict(insert(&contract, &[], &mut records, 2, Some("ada@example.test"), "t1", Some("linus")).expect_err("duplicate email"));
    assert_eq!(name, "Account_email_key");
    assert_eq!(columns, vec!["email".to_owned()]);
    assert_eq!(records.len(), 1);
}

#[test]
fn declared_composite_constraint_rejects_a_duplicate_column_set() {
    let contract = contract();
    let composite = declared("Account_tenant_handle_key", &["tenant", "handle"], true);
    let mut records = BTreeMap::new();
    insert(&contract, &[composite.clone()], &mut records, 1, Some("a@example.test"), "t1", Some("ada")).expect("first insert");
    insert(&contract, &[composite.clone()], &mut records, 2, Some("b@example.test"), "t1", Some("linus")).expect("distinct set");
    let (name, columns) =
        conflict(insert(&contract, &[composite], &mut records, 3, Some("c@example.test"), "t1", Some("ada")).expect_err("duplicate set"));
    assert_eq!(name, "Account_tenant_handle_key");
    assert_eq!(columns, vec!["tenant".to_owned(), "handle".to_owned()]);
    assert_eq!(records.len(), 2);
}

#[test]
fn nulls_never_collide_when_nulls_are_distinct() {
    let contract = contract();
    let mut records = BTreeMap::new();
    insert(&contract, &[], &mut records, 1, None, "t1", None).expect("first null row");
    insert(&contract, &[], &mut records, 2, None, "t1", None).expect("second null row");
    assert_eq!(records.len(), 2);
}

#[test]
fn nulls_collide_when_the_constraint_declares_one_shared_null() {
    let contract = contract();
    let single_null = declared("Account_handle_null_key", &["handle"], false);
    let mut records = BTreeMap::new();
    insert(&contract, &[single_null.clone()], &mut records, 1, Some("a@example.test"), "t1", None).expect("first null");
    let (name, columns) =
        conflict(insert(&contract, &[single_null], &mut records, 2, Some("b@example.test"), "t1", None).expect_err("shared null"));
    assert_eq!(name, "Account_handle_null_key");
    assert_eq!(columns, vec!["handle".to_owned()]);
    assert_eq!(records.len(), 1);
}

#[test]
fn conflicting_write_leaves_the_previous_row_intact() {
    let contract = contract();
    let mut records = BTreeMap::new();
    insert(&contract, &[], &mut records, 1, Some("ada@example.test"), "t1", Some("ada")).expect("first insert");
    insert(&contract, &[], &mut records, 2, Some("linus@example.test"), "t1", Some("linus")).expect("second insert");

    insert(&contract, &[], &mut records, 2, Some("ada@example.test"), "t1", Some("linus")).expect_err("conflicting replace");

    let rows = execute("Account.all().collect()", Some(&contract), &records).expect("read rows");
    assert_eq!(rows.len(), 2);
    let second = rows.iter().find(|row| row.get("account_id") == Some(&Value::I64(2))).expect("second row still present");
    assert_eq!(second.get("email"), Some(&Value::Text("linus@example.test".into())));
    assert_eq!(second.get("handle"), Some(&Value::Text("linus".into())));
}

const JOB_SCHEMA: &str = r#"
table Job {
    @@job_id: i64,
    [unique(ux_job_active_scope)] scope_type: utf8,
    [unique(ux_job_active_scope)] scope_user_id: i64,
    status: utf8,
}
"#;

fn insert_job(
    contract: &ResolvedContract,
    records: &mut BTreeMap<String, Vec<u8>>,
    job_id: i64,
    scope_type: &str,
    scope_user_id: i64,
) -> database_types::Result<()> {
    let source = format!(
        r#"Job::insert({{
            job_id: {job_id},
            scope_type: "{scope_type}",
            scope_user_id: {scope_user_id},
            status: "pending",
        }})"#
    );
    execute_write_with_constraints(&source, contract, &[], None, records)
}

#[test]
fn grouped_unique_attribute_enforces_one_multi_column_rule_on_write() {
    let contract = resolved_contract_from_document(JOB_SCHEMA).expect("job schema resolves");
    let mut records = BTreeMap::new();
    insert_job(&contract, &mut records, 1, "salesperson", 10).expect("first pair");
    insert_job(&contract, &mut records, 2, "salesperson", 20).expect("same type distinct user is allowed");
    let (name, columns) = conflict(insert_job(&contract, &mut records, 3, "salesperson", 10).expect_err("duplicate pair"));
    assert_eq!(name, "ux_job_active_scope");
    assert_eq!(columns, vec!["scope_type".to_owned(), "scope_user_id".to_owned()]);
    assert_eq!(records.len(), 2);
}

#[test]
fn deleting_a_row_frees_its_unique_key_for_reinsertion() {
    let contract = contract();
    let mut records = BTreeMap::new();
    insert(&contract, &[], &mut records, 1, Some("ada@example.test"), "t1", Some("ada")).expect("first insert");
    records.retain(|key, _| !key.ends_with("/1"));
    assert!(records.is_empty());
    insert(&contract, &[], &mut records, 2, Some("ada@example.test"), "t1", Some("ada")).expect("reinsert after delete");
    assert_eq!(records.len(), 1);
}

const PARTIAL_JOB_SCHEMA: &str = r#"
table Job {
    @@job_id: i64,
    [unique(ux_job_active_scope where status = "active")] scope_type: utf8,
    [unique(ux_job_active_scope where status = "active")] scope_user_id: i64,
    status: utf8,
}
"#;

fn insert_partial_job(
    contract: &ResolvedContract,
    records: &mut BTreeMap<String, Vec<u8>>,
    job_id: i64,
    scope_type: &str,
    scope_user_id: i64,
    status: &str,
) -> database_types::Result<()> {
    let source = format!(
        r#"Job::insert({{
            job_id: {job_id},
            scope_type: "{scope_type}",
            scope_user_id: {scope_user_id},
            status: "{status}",
        }})"#
    );
    execute_write_with_constraints(&source, contract, &[], None, records)
}

#[test]
fn partial_unique_rule_rejects_duplicates_only_for_matching_rows() {
    let contract = resolved_contract_from_document(PARTIAL_JOB_SCHEMA).expect("partial job schema resolves");
    let mut records = BTreeMap::new();
    insert_partial_job(&contract, &mut records, 1, "salesperson", 10, "active").expect("first active pair");
    insert_partial_job(&contract, &mut records, 2, "salesperson", 10, "closed").expect("same pair but outside the predicate");
    insert_partial_job(&contract, &mut records, 3, "salesperson", 20, "active").expect("same type, distinct user");
    let (name, columns) =
        conflict(insert_partial_job(&contract, &mut records, 4, "salesperson", 10, "active").expect_err("duplicate active pair"));
    assert_eq!(name, "ux_job_active_scope");
    assert_eq!(columns, vec!["scope_type".to_owned(), "scope_user_id".to_owned()]);
    assert_eq!(records.len(), 3, "the row outside the predicate stays in the table");
}

const NULLABLE_SCHEMA: &str = r#"
table Draft {
    @@draft_id: i64,
    [unique(ux_draft_named where slug is not null)] owner: utf8,
    slug: utf8?,
}
"#;

fn insert_draft(
    contract: &ResolvedContract,
    records: &mut BTreeMap<String, Vec<u8>>,
    draft_id: i64,
    owner: &str,
    slug: Option<&str>,
) -> database_types::Result<()> {
    let source = format!(
        r#"Draft::insert({{
            draft_id: {draft_id},
            owner: "{owner}",
            slug: {},
        }})"#,
        literal(slug),
    );
    execute_write_with_constraints(&source, contract, &[], None, records)
}

#[test]
fn is_not_null_condition_lets_null_rows_skip_the_rule() {
    let contract = resolved_contract_from_document(NULLABLE_SCHEMA).expect("nullable schema resolves");
    let mut records = BTreeMap::new();
    insert_draft(&contract, &mut records, 1, "ada", None).expect("first draft without a slug");
    insert_draft(&contract, &mut records, 2, "ada", Some("hello")).expect("first draft with a slug");
    insert_draft(&contract, &mut records, 3, "ada", None).expect("another slugless draft");
    let (name, columns) =
        conflict(insert_draft(&contract, &mut records, 4, "ada", Some("hello")).expect_err("duplicate slug among the participating rows"));
    assert_eq!(name, "ux_draft_named");
    assert_eq!(columns, vec!["owner".to_owned()]);
    assert_eq!(records.len(), 3);
}

#[test]
fn a_write_against_a_schema_with_a_mismatched_predicate_literal_reports_a_schema_error() {
    let contract = resolved_contract_from_document(
        r#"table Bad { @@id: i64, [unique(ux where flag = 1)] flag: utf8 }"#,
    )
    .expect("the document parses");
    let mut records = BTreeMap::new();
    let error =
        execute_write_with_constraints(r#"Bad::insert({ id: 1, flag: "x" })"#, &contract, &[], None, &mut records).expect_err("schema error");
    match error {
        Error::Schema { message } => assert!(message.contains("does not match the declared type"), "{message}"),
        other => panic!("expected a schema error, got {other}"),
    }
}
