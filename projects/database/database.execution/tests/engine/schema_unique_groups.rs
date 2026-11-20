//! `[unique(group)]` 投影为一条多列唯一规则，裸 `[unique]` / `@name` 仍是单列规则，
//! `[unique(group where ...)]` 在同一条规则上附带部分索引谓词。

use database_execution::engine::Connection;
use database_types::PredicateCondition;

const SCHEMA: &str = "\
table Job {
    @@id: i64,
    [unique(ux_job_active_scope)] scope_type: utf8,
    [unique(ux_job_active_scope)] scope_user_id: i64,
    status: utf8,
}

table Account {
    @@id: i64,
    @email: utf8,
}
";

#[test]
fn grouped_unique_attributes_become_one_multi_column_rule() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();

    let catalog = conn.execution_catalog().unwrap().expect("catalog");
    let job = catalog.types.iter().find(|item| item.name == "Job").expect("Job");
    let grouped = job.unique_constraints.iter().find(|rule| rule.name() == "ux_job_active_scope").expect("grouped rule");
    assert_eq!(grouped.columns().iter().map(String::as_str).collect::<Vec<_>>(), ["scope_type", "scope_user_id"]);
    assert!(grouped.nulls_distinct(), "grouped rules keep PostgreSQL NULLS DISTINCT defaults");

    let account = catalog.types.iter().find(|item| item.name == "Account").expect("Account");
    let single = account.unique_constraints.iter().find(|rule| rule.name() == "Account_email_key").expect("single rule");
    assert_eq!(single.columns().iter().map(String::as_str).collect::<Vec<_>>(), ["email"]);
}

#[test]
fn a_malformed_unique_group_fails_instead_of_degrading_to_a_single_column_rule() {
    let conn = Connection::open_in_memory().unwrap();
    let error = conn.ensure_schema("table T { @@id: i64, [unique(a-b)] x: i64 }").expect_err("a malformed group name must not be accepted");
    assert!(error.to_string().contains("invalid unique group name"), "{error}");
    assert!(conn.schema().unwrap().is_none(), "the rejected document must not become database truth");
}

const PARTIAL_SCHEMA: &str = "\
table Job {
    @@id: i64,
    [unique(ux_job_active_scope where status in ('pending', 'running') and deleted_at is not null)] scope_type: utf8,
    [unique(ux_job_active_scope where status in ('pending','running') and deleted_at is not null)] scope_user_id: i64,
    status: utf8,
    deleted_at: utf8?,
}
";

#[test]
fn partial_unique_attributes_project_one_rule_carrying_the_predicate() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(PARTIAL_SCHEMA).unwrap();

    let catalog = conn.execution_catalog().unwrap().expect("catalog");
    let job = catalog.types.iter().find(|item| item.name == "Job").expect("Job");
    let grouped = job.unique_constraints.iter().find(|rule| rule.name() == "ux_job_active_scope").expect("grouped rule");
    assert_eq!(grouped.columns().iter().map(String::as_str).collect::<Vec<_>>(), ["scope_type", "scope_user_id"]);
    let predicate = grouped.predicate().expect("the partial rule keeps its predicate");
    assert_eq!(predicate.conditions.len(), 2, "{predicate}");
    match &predicate.conditions[0] {
        PredicateCondition::In { column, values } => {
            assert_eq!(column, "status");
            assert_eq!(values.len(), 2);
        }
        other => panic!("expected an `in` condition, got {other:?}"),
    }
    match &predicate.conditions[1] {
        PredicateCondition::IsNotNull { column } => assert_eq!(column, "deleted_at"),
        other => panic!("expected an `is not null` condition, got {other:?}"),
    }
    assert!(
        grouped.to_string().contains(" where status in ('pending', 'running') and deleted_at is not null"),
        "the Display summary carries the predicate: {grouped}"
    );
}

#[test]
fn a_malformed_unique_predicate_fails_instead_of_degrading_to_a_whole_table_rule() {
    for document in [
        "table T { @@id: i64, [unique(ux where)] x: i64 }",
        "table T { @@id: i64, [unique(ux where status == 1)] x: i64, status: i64 }",
        "table T { @@id: i64, [unique(ux where status in ())] x: i64, status: i64 }",
        "table T { @@id: i64, [unique(ux where status = 1 x = 2)] x: i64, status: i64 }",
        "table T { @@id: i64, [unique(ux where status like 'a%')] x: i64, status: utf8 }",
    ] {
        let conn = Connection::open_in_memory().unwrap();
        let error = conn.ensure_schema(document).expect_err("a malformed predicate must not be accepted");
        assert!(error.to_string().contains("malformed unique attribute"), "{error}");
    }
}

#[test]
fn a_predicate_literal_that_mismatches_the_column_type_is_rejected() {
    let conn = Connection::open_in_memory().unwrap();
    let error = conn.ensure_schema("table T { @@id: i64, [unique(ux where status = 1)] status: utf8 }")
        .expect_err("an integer literal cannot guard a text column");
    assert!(error.to_string().contains("does not match the declared type"), "{error}");
}

#[test]
fn a_predicate_may_only_reference_columns_of_the_same_table() {
    let conn = Connection::open_in_memory().unwrap();
    let error = conn.ensure_schema("table T { @@id: i64, [unique(ux where other = 1)] x: i64 }")
        .expect_err("the predicate column must exist on the table");
    assert!(error.to_string().contains("references unknown column"), "{error}");
}
