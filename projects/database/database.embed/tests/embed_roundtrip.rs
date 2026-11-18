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

/// 一条记录超过一个叶子页时必须在落盘时拆分，读回时重组，
/// 否则宿主的大文本字段会以 `record is too large for one leaf page` 失败。
#[test]
fn a_record_larger_than_one_leaf_page_round_trips() {
    let path = temp_yydx("large-record");
    let text = "yydb-large-record-".repeat(400);
    assert!(text.len() > 6_000, "the fixture must exceed one leaf page");
    {
        let db = EmbedDb::open(&path).unwrap();
        db.ensure_schema(SCHEMA).unwrap();
        insert(&db, 1, "c-1", 1, &text);
    }

    let reopened = EmbedDb::open(&path).unwrap();
    let rows = select(&reopened, "c-1");
    assert_eq!(rows.len(), 1, "rows after reopen: {rows:?}");
    assert_eq!(rows[0]["text"], json!(text.as_str()));

    // 缩短后旧分块不能残留成幽灵记录，长值也必须在同一连接内立即可读。
    insert(&reopened, 2, "c-2", 1, "short");
    insert(&reopened, 3, "c-3", 1, &"z".repeat(9_000));
    assert_eq!(select(&reopened, "c-2")[0]["text"], json!("short"));
    assert_eq!(select(&reopened, "c-3")[0]["text"], json!("z".repeat(9_000)));
}

/// 同一路径第二次打开必须给出可判定的「忙」错误，而不是裸露的锁失败。
#[test]
fn a_second_open_reports_busy() {
    let path = temp_yydx("busy");
    let first = EmbedDb::open(&path).unwrap();
    let second = EmbedDb::open(&path).expect_err("a second handle must be refused");
    assert!(second.to_string().contains("database busy"), "unexpected error: {second}");
    drop(first);
    EmbedDb::open(&path).expect("the path reopens once every handle is released");
}

/// 多组件宿主用共享入口复用同一份连接，而不是各自开库互相撞锁。
#[test]
fn open_shared_reuses_one_handle() {
    let path = temp_yydx("shared");
    let first = EmbedDb::open_shared(&path).unwrap();
    let second = EmbedDb::open_shared(&path).unwrap();
    assert!(std::sync::Arc::ptr_eq(&first, &second), "the same path must share one handle");
    assert!(EmbedDb::open(&path).is_err(), "an exclusive open must not coexist with a shared one");
    drop(first);
    drop(second);
    EmbedDb::open(&path).expect("the path reopens after every shared handle is released");
}

#[test]
fn the_handle_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<EmbedDb>();
}

#[test]
fn rejects_non_yydx_path() {
    let dir = std::env::temp_dir().join(format!("database-embed-bad-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let error = EmbedDb::open(dir.join("app.db")).expect_err("must reject non-yydx path");
    assert!(error.to_string().contains("yydx"), "unexpected error: {error}");
}
/// 多表 schema 的 identity 与 contract 远超一页，落盘必须分块并在重开后重组。
#[test]
fn large_schema_survives_reopen() {
    let mut document = String::new();
    for index in 0..8 {
        document.push_str(&format!(
            "table Sheet{index} {{\n    @@id: utf8\n    title: utf8\n    owner: utf8\n    body: utf8\n    revision: i64\n    updated_at: i64\n}}\n\n"
        ));
    }
    let path = temp_yydx("large-schema");
    {
        let db = EmbedDb::open(&path).unwrap();
        db.ensure_schema(&document).unwrap();
        db.execute(
            "Sheet7.insert({ id: $id, title: $title, owner: $owner, body: $body, revision: $revision, updated_at: $updated_at })",
            &params(&[
                ("id", json!("s-1")),
                ("title", json!("chunked")),
                ("owner", json!("me")),
                ("body", json!("bind")),
                ("revision", json!(1)),
                ("updated_at", json!(0)),
            ]),
        )
        .unwrap();
    }

    let reopened = EmbedDb::open(&path).unwrap();
    assert_eq!(reopened.schema_document().unwrap().as_deref(), Some(document.as_str()));
    let rows = reopened.query("Sheet7.all().collect()", &JsonMap::new()).unwrap();
    assert_eq!(rows.len(), 1, "rows after reopen: {rows:?}");
    assert_eq!(rows[0]["title"], json!("chunked"));

    // 追加表走 schema 演进，既有表身份保持不变。
    reopened.migrate_schema(&format!("{document}\ntable Sheet8 {{\n    @@id: utf8\n    title: utf8\n}}\n")).unwrap();
    assert_eq!(reopened.query("Sheet7.all().collect()", &JsonMap::new()).unwrap().len(), 1);
}
