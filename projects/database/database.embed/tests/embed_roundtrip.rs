//! `EmbedDb` 往返：VOS schema、写入、查询、重启后仍在。

use std::path::{Path, PathBuf};

use database_embed::EmbedDb;
use serde_json::{Map as JsonMap, Value as JsonValue, json};

const SCHEMA: &str = r#"
table Message {
    @@id: i64
    conversation: utf8
    seq: i64
    text: utf8
}
"#;

fn temp_yydx(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("database-embed-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("app.yydx")
}

fn params(pairs: &[(&str, JsonValue)]) -> JsonMap<String, JsonValue> {
    let mut map = JsonMap::new();
    for (key, value) in pairs {
        map.insert((*key).to_string(), value.clone());
    }
    map
}

fn insert(db: &EmbedDb, id: i64, conversation: &str, seq: i64, text: &str) {
    db.execute(
        "Message.insert({ id: $id, conversation: $conversation, seq: $seq, text: $text })",
        &params(&[("id", json!(id)), ("conversation", json!(conversation)), ("seq", json!(seq)), ("text", json!(text))]),
    )
    .expect("insert message");
}

fn select(db: &EmbedDb, conversation: &str) -> Vec<JsonValue> {
    db.query("Message.all().filter(x => x.conversation == $conversation).collect()", &params(&[("conversation", json!(conversation))]))
        .expect("select messages")
}

#[test]
fn in_memory_roundtrip() {
    let db = EmbedDb::open_in_memory().unwrap();
    db.ensure_schema(SCHEMA).unwrap();
    insert(&db, 1, "c-1", 1, "hello");
    insert(&db, 2, "c-1", 2, "world");

    let rows = select(&db, "c-1");
    assert_eq!(rows.len(), 2, "rows: {rows:?}");
    let first = &rows[0];
    assert_eq!(first["text"], json!("hello"));
    assert_eq!(first["seq"], json!(1));
}

#[test]
fn schema_kind_filter_isolates_conversations() {
    let db = EmbedDb::open_in_memory().unwrap();
    db.ensure_schema(SCHEMA).unwrap();
    insert(&db, 1, "c-1", 1, "one");
    insert(&db, 2, "c-2", 1, "two");

    assert_eq!(select(&db, "c-1").len(), 1);
    assert_eq!(select(&db, "c-2").len(), 1);
    assert!(select(&db, "c-3").is_empty());
}

#[test]
fn yydx_file_survives_reopen() {
    let path = temp_yydx("reopen");
    {
        let db = EmbedDb::open(&path).unwrap();
        db.ensure_schema(SCHEMA).unwrap();
        insert(&db, 1, "c-1", 1, "persisted");
        assert!(db.schema_version().unwrap().is_some());
    }
    assert!(Path::new(&path).is_file(), "main file missing: {}", path.display());

    let reopened = EmbedDb::open(&path).unwrap();
    let rows = select(&reopened, "c-1");
    assert_eq!(rows.len(), 1, "rows after reopen: {rows:?}");
    assert_eq!(rows[0]["text"], json!("persisted"));
}

#[test]
fn rejects_non_yydx_path() {
    let dir = std::env::temp_dir().join(format!("database-embed-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let error = EmbedDb::open(dir.join("app.db")).expect_err("must reject non-yydx path");
    assert!(error.to_string().contains("yydx"), "unexpected error: {error}");
}
