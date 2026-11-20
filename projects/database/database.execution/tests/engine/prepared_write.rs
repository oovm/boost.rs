//! Prepared VOS write programs must execute through the plan entry point.
//!
//! `prepare` classifies a VOS write program as `QueryKind::Write` but leaves
//! both operator lists empty, so the plan tail has to route it back to the VOS
//! write executor instead of reporting a silent zero affected count.

use database_execution::engine::{Connection, PlanOutcome};
use database_query::local::QueryKind;
use database_types::Value;

const SCHEMA: &str = "table Note { @@id: i64, body: utf8 }";

fn note_rows(conn: &Connection) -> Vec<database_query::local::QueryRow> {
    conn.query_vos("Note.all().collect()").expect("read back")
}

#[test]
fn a_prepared_vos_insert_lands_its_row_through_execute_plan() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();
    let stmt = conn.prepare_vos(r#"Note { id: 1, body: "first" }.insert()"#).unwrap();
    assert_eq!(stmt.query_kind, QueryKind::Write, "a VOS write program must prepare as a write");
    assert!(stmt.write_ops.is_empty(), "a VOS write program carries no lowered write ops");

    assert_eq!(conn.execute_plan(&stmt, &[]).unwrap(), PlanOutcome::Affected(1));
    let rows = note_rows(&conn);
    assert_eq!(rows.len(), 1, "the prepared insert must have stored its row");
    assert_eq!(rows[0].get("body"), Some(&Value::Text("first".into())));
}

#[test]
fn a_prepared_vos_update_and_delete_take_effect_through_execute_plan() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();
    conn.execute_vos_write(r#"Note { id: 1, body: "first" }.insert()"#).unwrap();

    let update = conn.prepare_vos(r#"Note::update({ id: 1, body: "patched" })"#).unwrap();
    assert_eq!(conn.execute_plan(&update, &[]).unwrap(), PlanOutcome::Affected(1));
    let rows = note_rows(&conn);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("body"), Some(&Value::Text("patched".into())));

    let delete = conn.prepare_vos("Note::delete({ id: 1 })").unwrap();
    assert_eq!(conn.execute_plan(&delete, &[]).unwrap(), PlanOutcome::Affected(1));
    assert!(note_rows(&conn).is_empty(), "the prepared delete must have removed the row");
}

#[test]
fn a_prepared_vos_write_binds_named_parameters_through_execute_plan() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();
    let stmt = conn.prepare_vos("Note { id: $id, body: $body }.insert()").unwrap();
    assert_eq!(stmt.parameters.len(), 2);

    let outcome = conn.execute_plan(&stmt, &[Value::I64(7), Value::Text("seven".into())]).unwrap();
    assert_eq!(outcome, PlanOutcome::Affected(1));
    let rows = note_rows(&conn);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("id"), Some(&Value::I64(7)));
    assert_eq!(rows[0].get("body"), Some(&Value::Text("seven".into())));
}

#[test]
fn a_prepared_read_still_runs_the_read_pipeline_through_execute_plan() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();
    conn.execute_vos_write(r#"Note { id: 1, body: "first" }.insert()"#).unwrap();

    let stmt = conn.prepare_vos("Note.all().collect()").unwrap();
    assert_eq!(stmt.query_kind, QueryKind::Read);
    match conn.execute_plan(&stmt, &[]).unwrap() {
        PlanOutcome::Rows(rows) => assert_eq!(rows.len(), 1),
        PlanOutcome::Affected(affected) => panic!("a read plan must return rows, got {affected} affected"),
    }
}

#[test]
fn a_prepared_write_program_with_a_read_statement_fails_closed() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();
    let stmt = conn
        .prepare_vos(
            r#"Note.all().collect()
Note { id: 1, body: "first" }.insert()"#,
        )
        .unwrap();
    assert_eq!(stmt.query_kind, QueryKind::Write);

    let error = conn.execute_plan(&stmt, &[]).unwrap_err();
    assert_eq!(error.code(), "database.unsupported", "a read mixed into a write program must be refused");
    assert!(note_rows(&conn).is_empty(), "a refused program must not store a partial row");
}

#[test]
fn a_collection_write_program_fails_closed() {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();
    let stmt = conn.prepare_vos("Note.all().delete()").unwrap();
    assert_eq!(stmt.query_kind, QueryKind::Write);

    let error = conn.execute_plan(&stmt, &[]).unwrap_err();
    assert_eq!(error.code(), "database.unsupported", "a collection write must not pass as an executed write");
}
