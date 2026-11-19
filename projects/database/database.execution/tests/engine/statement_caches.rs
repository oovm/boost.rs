//! The connection statement caches must never serve a stale image.
//!
//! The metadata cache is keyed by the durable commit sequence, so every
//! publish — schema DDL, narrowed writes, transactions — has to move the key.
//! The prepared-plan and parsed-program caches are keyed by the same schema
//! basis the drift gate uses, so a migration has to miss them.

use std::collections::BTreeMap;

use database_execution::engine::{catalog::RenameMap, Connection};

fn path(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("database-execution-statement-caches-{}-{label}.yydb", std::process::id()))
}

fn cleanup(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path.display()));
    let _ = std::fs::remove_file(format!("{}-shm", path.display()));
}

#[test]
fn reads_after_every_publish_kind_see_fresh_metadata() {
    let path = path("publishes");
    cleanup(&path);
    let conn = Connection::open(&path).unwrap();
    conn.ensure_schema("table Item { @@id: i64, name: utf8 }").unwrap();
    // Warm the cache with a read that decodes nothing else.
    assert_eq!(conn.query_vos("Item.all().collect()").unwrap().len(), 0);
    // Narrowed autocommit writes bump the sequence per statement.
    for id in 1..=5_i64 {
        conn.execute_vos_write(&format!(r#"Item.insert({{ id: {id}, name: "n{id}" }})"#)).unwrap();
        assert_eq!(conn.query_vos("Item.all().collect()").unwrap().len(), usize::try_from(id).unwrap());
    }
    // A transaction publishes one bumped image at commit.
    conn.begin().unwrap();
    conn.execute_vos_write(r#"Item.insert({ id: 6, name: "n6" })"#).unwrap();
    conn.execute_vos_write(r#"Item.insert({ id: 7, name: "n7" })"#).unwrap();
    conn.commit().unwrap();
    assert_eq!(conn.query_vos("Item.all().collect()").unwrap().len(), 7);
    // A raw record write goes through the full publish path.
    conn.write_batch(&[("cache/probe".to_owned(), Some(b"1".to_vec()))]).unwrap();
    assert_eq!(conn.query_vos("Item.all().collect()").unwrap().len(), 7);
    // A schema change moves the typed metadata and must not be served stale.
    conn.migrate_schema("table Item { @@id: i64, name: utf8, note: utf8 }", &RenameMap::default()).unwrap();
    assert_eq!(conn.query_vos("Item.all().collect()").unwrap().len(), 7);
    // The migrated contract is what the next write plans against.
    conn.execute_vos_write(r#"Item.insert({ id: 8, name: "n8", note: "fresh" })"#).unwrap();
    let rows = conn.query_vos("Item.all().collect()").unwrap();
    assert_eq!(rows.len(), 8);
    assert_eq!(
        rows.iter().find(|row| row.get("id") == Some(&database_types::Value::I64(8))).and_then(|row| row.get("note")),
        Some(&database_types::Value::Text("fresh".to_owned()))
    );
    drop(conn);
    // Reopen decodes the same durable image.
    let conn = Connection::open(&path).unwrap();
    assert_eq!(conn.query_vos("Item.all().collect()").unwrap().len(), 8);
    drop(conn);
    cleanup(&path);
}

#[test]
fn prepared_plans_and_write_programs_follow_the_schema_basis() {
    let path = path("basis");
    cleanup(&path);
    let conn = Connection::open(&path).unwrap();
    conn.ensure_schema("table Doc { @@id: i64, title: utf8 }").unwrap();
    let stmt = conn.prepare_vos("Doc.all().collect()").unwrap();
    conn.query_prepared(&stmt, &BTreeMap::new()).unwrap();
    // The same source re-prepares to the same plan under the same basis.
    let again = conn.prepare_vos("Doc.all().collect()").unwrap();
    assert_eq!(again.schema_fingerprint, stmt.schema_fingerprint);
    assert_eq!(again.schema_revision, stmt.schema_revision);
    // A repeated parameterized write runs through the cached program and still
    // binds fresh values each time.
    let mut first = BTreeMap::new();
    first.insert("id".to_owned(), database_types::Value::I64(1));
    first.insert("title".to_owned(), database_types::Value::Text("one".to_owned()));
    conn.execute_vos_write_with_params(r#"Doc.insert({ id: $id, title: $title })"#, &first).unwrap();
    let mut second = BTreeMap::new();
    second.insert("id".to_owned(), database_types::Value::I64(2));
    second.insert("title".to_owned(), database_types::Value::Text("two".to_owned()));
    conn.execute_vos_write_with_params(r#"Doc.insert({ id: $id, title: $title })"#, &second).unwrap();
    assert_eq!(conn.query_vos("Doc.all().collect()").unwrap().len(), 2);
    // After a migration the old plan is rejected and a fresh prepare works.
    conn.migrate_schema("table Doc { @@id: i64, title: utf8, extra: utf8 }", &RenameMap::default()).unwrap();
    let error = conn.query_prepared(&stmt, &BTreeMap::new()).unwrap_err();
    assert_eq!(error.code(), "database.schema.invalidated");
    let fresh = conn.prepare_vos("Doc.all().collect()").unwrap();
    assert_ne!(fresh.schema_fingerprint, stmt.schema_fingerprint);
    conn.query_prepared(&fresh, &BTreeMap::new()).unwrap();
    drop(conn);
    cleanup(&path);
}
