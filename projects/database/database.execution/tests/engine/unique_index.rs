//! The connection-wide unique-key index: duplicates stay rejected across
//! statements, replaced and deleted rows free their keys, and paths that do
//! not maintain the index (rollback, raw record writes) invalidate it so the
//! next check rebuilds from the table instead of trusting stale entries.

use database_execution::engine::Connection;
use database_types::Error;

const SCHEMA: &str = "table User { @@id: i64, [unique] email: utf8, name: utf8 }";

fn connection() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();
    conn
}

fn unique_violation(error: Error) -> String {
    match error {
        Error::UniqueViolation { constraint, .. } => constraint,
        other => panic!("expected a unique violation, got {other}"),
    }
}

#[test]
fn duplicate_unique_value_across_separate_writes_is_rejected() {
    let conn = connection();
    conn.execute_vos_write(r#"User.insert({ id: 1, email: "a@x.test", name: "one" })"#).unwrap();
    let error = conn.execute_vos_write(r#"User.insert({ id: 2, email: "a@x.test", name: "two" })"#).unwrap_err();
    assert_eq!(unique_violation(error), "User_email_key");
    let rows = conn.query_vos("User.all().collect()").unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn rewriting_a_row_with_its_own_value_is_not_a_conflict() {
    let conn = connection();
    conn.execute_vos_write(r#"User.insert({ id: 1, email: "a@x.test", name: "one" })"#).unwrap();
    conn.execute_vos_write(r#"User.insert({ id: 1, email: "a@x.test", name: "renamed" })"#).unwrap();
    let rows = conn.query_vos("User.all().collect()").unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn an_insert_that_overwrites_a_row_frees_the_value_it_leaves() {
    let conn = connection();
    conn.execute_vos_write(r#"User.insert({ id: 1, email: "a@x.test", name: "one" })"#).unwrap();
    // Rewriting the whole row through insert must retract the unique value it
    // leaves behind, exactly like an update does.
    conn.execute_vos_write(r#"User.insert({ id: 1, email: "b@x.test", name: "one" })"#).unwrap();
    conn.execute_vos_write(r#"User.insert({ id: 2, email: "a@x.test", name: "two" })"#).unwrap();
    let rows = conn.query_vos("User.all().collect()").unwrap();
    assert_eq!(rows.len(), 2);
}

#[test]
fn update_frees_the_old_value_and_blocks_a_taken_one() {
    let conn = connection();
    conn.execute_vos_write(r#"User.insert({ id: 1, email: "a@x.test", name: "one" })"#).unwrap();
    conn.execute_vos_write(r#"User.insert({ id: 2, email: "b@x.test", name: "two" })"#).unwrap();
    conn.execute_vos_write(r#"User::update({ id: 1, email: "c@x.test" })"#).unwrap();
    // The freed value can be taken by the other row.
    conn.execute_vos_write(r#"User::update({ id: 2, email: "a@x.test" })"#).unwrap();
    // ...but the value it just left is still owned by row 1.
    let error = conn.execute_vos_write(r#"User::update({ id: 2, email: "c@x.test" })"#).unwrap_err();
    assert_eq!(unique_violation(error), "User_email_key");
}

#[test]
fn delete_frees_the_unique_value() {
    let conn = connection();
    conn.execute_vos_write(r#"User.insert({ id: 1, email: "a@x.test", name: "one" })"#).unwrap();
    conn.execute_vos_write(r#"User::delete({ id: 1 })"#).unwrap();
    conn.execute_vos_write(r#"User.insert({ id: 2, email: "a@x.test", name: "two" })"#).unwrap();
    let rows = conn.query_vos("User.all().collect()").unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn rollback_invalidates_keys_the_discarded_transaction_had_taken() {
    let conn = connection();
    conn.begin().unwrap();
    conn.execute_vos_write(r#"User.insert({ id: 1, email: "a@x.test", name: "one" })"#).unwrap();
    conn.rollback().unwrap();
    // The row never published, so its unique value is free again.
    conn.execute_vos_write(r#"User.insert({ id: 2, email: "a@x.test", name: "two" })"#).unwrap();
}

#[test]
fn raw_record_write_invalidates_the_index_and_duplicates_are_still_caught() {
    let conn = connection();
    conn.execute_vos_write(r#"User.insert({ id: 1, email: "a@x.test", name: "one" })"#).unwrap();
    // Any raw record write drops the cache; the next duplicate check must
    // rebuild from the table rather than answer "free" from stale entries.
    conn.write_batch(&[("cache/probe".to_owned(), Some(b"1".to_vec()))]).unwrap();
    let error = conn.execute_vos_write(r#"User.insert({ id: 2, email: "a@x.test", name: "two" })"#).unwrap_err();
    assert_eq!(unique_violation(error), "User_email_key");
}

#[test]
fn bulk_inserts_with_a_unique_column_stay_rejected_at_the_first_duplicate() {
    let conn = connection();
    for id in 1..=500_i64 {
        let source = format!(r#"User.insert({{ id: {id}, email: "user{id}@x.test", name: "n{id}" }})"#);
        conn.execute_vos_write(&source).unwrap();
    }
    let error = conn.execute_vos_write(r#"User.insert({ id: 501, email: "user250@x.test", name: "dup" })"#).unwrap_err();
    assert_eq!(unique_violation(error), "User_email_key");
    let rows = conn.query_vos("User.all().collect()").unwrap();
    assert_eq!(rows.len(), 500);
}

#[test]
fn a_partial_rule_keeps_its_predicate_through_bulk_writes_and_cold_rebuilds() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema("table Job { @@id: i64, [unique(ux_job_open where status = 1)] scope: utf8, status: i64 }").unwrap();
    // Bulk writes through the maintained index: rows outside the predicate
    // share one key freely while rows inside it collide.
    for id in 1..=50_i64 {
        let status = if id % 2 == 0 { 0 } else { 1 };
        let source = format!(r#"Job.insert({{ id: {id}, scope: "s{id}", status: {status} }})"#);
        conn.execute_vos_write(&source).unwrap();
    }
    for id in 51..=60_i64 {
        conn.execute_vos_write(&format!(r#"Job.insert({{ id: {id}, scope: "shared", status: 0 }})"#)).unwrap();
    }
    conn.execute_vos_write(r#"Job.insert({ id: 61, scope: "shared", status: 1 })"#).unwrap();
    let error = conn.execute_vos_write(r#"Job.insert({ id: 62, scope: "shared", status: 1 })"#).unwrap_err();
    assert_eq!(unique_violation(error), "ux_job_open");
    // A raw record write drops the cache; the cold rebuild must skip the very
    // same rows the warm index never entered.
    conn.write_batch(&[("cache/probe".to_owned(), Some(b"1".to_vec()))]).unwrap();
    let error = conn.execute_vos_write(r#"Job.insert({ id: 63, scope: "shared", status: 1 })"#).unwrap_err();
    assert_eq!(unique_violation(error), "ux_job_open");
    conn.execute_vos_write(r#"Job.insert({ id: 64, scope: "shared", status: 0 })"#).unwrap();
    let rows = conn.query_vos("Job.all().collect()").unwrap();
    assert_eq!(rows.len(), 62);
}
