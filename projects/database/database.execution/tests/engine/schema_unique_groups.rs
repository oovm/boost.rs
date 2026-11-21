//! `[unique(group)]` 投影为一条多列唯一规则，裸 `[unique]` / `@name` 仍是单列规则。
//!
//! VOS 属性只接受组名，不接受 SQL 式的 `where` 谓词：唯一规则始终是全表规则。

use database_execution::engine::Connection;

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

#[test]
fn unique_attributes_reject_where_clauses() {
    for document in [
        "table T { @@id: i64, [unique(ux where status = 1)] x: i64, status: i64 }",
        "table T { @@id: i64, [unique(ux where status is not null)] x: i64, status: i64 }",
        "table T { @@id: i64, [unique(ux where)] x: i64 }",
    ] {
        let conn = Connection::open_in_memory().unwrap();
        let error = conn.ensure_schema(document).expect_err("a unique attribute must not embed a predicate");
        assert!(error.to_string().contains("invalid unique group name"), "{error}");
        assert!(conn.schema().unwrap().is_none(), "the rejected document must not become database truth");
    }
}
