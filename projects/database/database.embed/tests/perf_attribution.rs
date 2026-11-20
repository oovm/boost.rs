//! 写路径与读路径的成本归因：内存 vs 文件、逐阶段 ReadProfile。
//!
//! 回答两个问题：宿主报出的单行写 ~ms 成本里多少是落盘（publish/fsync），
//! 多少是引擎 CPU；普通查询的固定开销里多少花在每语句重读元数据与解码
//! schema 契约。用 `--nocapture` 查看归因表。

use std::{path::PathBuf, time::Instant};

use database_embed::EmbedDb;
use serde_json::{Map as JsonMap, Value as JsonValue, json};

const SCHEMA: &str = r#"
table Item {
    @@id: utf8
    label: utf8
    group_name: utf8
    seq: i64
    note: utf8
}
"#;

const ROWS: usize = 2_000;

fn params(pairs: &[(&str, JsonValue)]) -> JsonMap<String, JsonValue> {
    let mut map = JsonMap::new();
    for (key, value) in pairs {
        map.insert((*key).to_string(), value.clone());
    }
    map
}

fn insert(db: &EmbedDb, id: &str, seq: i64) {
    db.execute(
        "Item.insert({ id: $id, label: $label, group_name: $group_name, seq: $seq, note: $note })",
        &params(&[
            ("id", json!(id)),
            ("label", json!("acct")),
            ("group_name", json!("team")),
            ("seq", json!(seq)),
            ("note", json!("x".repeat(64))),
        ]),
    )
    .expect("insert");
}

fn temp_yydx(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("database-embed-perf-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("app.yydx")
}

#[test]
fn write_and_read_cost_split_between_memory_and_disk() {
    // 内存：纯引擎 CPU（B 树、编码、事务）。
    let memory = EmbedDb::open_in_memory().unwrap();
    memory.ensure_schema(SCHEMA).unwrap();
    let started = Instant::now();
    for index in 0..ROWS {
        insert(&memory, &format!("id-{index:05}"), index as i64);
    }
    let memory_write = started.elapsed();

    // 文件：同一批写再走一次发布路径。
    let path = temp_yydx("split");
    let file = EmbedDb::open(&path).unwrap();
    file.ensure_schema(SCHEMA).unwrap();
    let started = Instant::now();
    for index in 0..ROWS {
        insert(&file, &format!("id-{index:05}"), index as i64);
    }
    let file_write = started.elapsed();

    // 读：逐阶段归因。2000 行表上的全表过滤与主键点查各跑 200 次。
    let mut scan_profile = database_embed::ReadProfile::default();
    let mut probe_profile = database_embed::ReadProfile::default();
    for _ in 0..200 {
        let (_, profile) = file
            .query_profiled("Item.all().filter(x => x.group_name == $g).collect()", &params(&[("g", json!("team"))]))
            .expect("scan");
        scan_profile.bind_ns += profile.bind_ns;
        scan_profile.metadata_ns += profile.metadata_ns;
        scan_profile.drift_ns += profile.drift_ns;
        scan_profile.plan_ns += profile.plan_ns;
        scan_profile.window_read_ns += profile.window_read_ns;
        scan_profile.exec_ns += profile.exec_ns;
        scan_profile.scan_ns += profile.scan_ns;
        scan_profile.filter_ns += profile.filter_ns;
        scan_profile.project_ns += profile.project_ns;
        scan_profile.sort_ns += profile.sort_ns;
        scan_profile.exec_other_ns += profile.exec_other_ns;
        scan_profile.scanned_rows += profile.scanned_rows;
        scan_profile.decoded_rows += profile.decoded_rows;
        scan_profile.emitted_rows += profile.emitted_rows;
        scan_profile.index_probes += profile.index_probes;
        scan_profile.fused_ops += profile.fused_ops;
        let (_, profile) =
            file.query_profiled("Item.all().filter(x => x.id == $id).collect()", &params(&[("id", json!("id-00001"))])).expect("probe");
        probe_profile.metadata_ns += profile.metadata_ns;
        probe_profile.drift_ns += profile.drift_ns;
        probe_profile.window_read_ns += profile.window_read_ns;
        probe_profile.exec_ns += profile.exec_ns;
    }

    println!(
        "PERF ATTRIBUTION rows={ROWS}\n memory_write: {memory_write:?} ({:.1} us/row)\n file_write:     {file_write:?} ({:.1} us/row)\n disk_share_of_write: {:.1}%\n scan_query x200: total {} us, metadata {} us, drift {} us, plan {} us, window_read {} us, exec {} us (scan {} filter {} project {}), scanned {} decoded {} emitted {}\n pk_probe x200: total {} us, metadata {} us, drift {} us, window_read {} us, exec {} us",
        memory_write.as_micros() as f64 / ROWS as f64,
        file_write.as_micros() as f64 / ROWS as f64,
        100.0 * (file_write.saturating_sub(memory_write).as_secs_f64()) / file_write.as_secs_f64(),
        scan_profile.total_ns() / 1_000,
        scan_profile.metadata_ns / 1_000,
        scan_profile.drift_ns / 1_000,
        scan_profile.plan_ns / 1_000,
        scan_profile.window_read_ns / 1_000,
        scan_profile.exec_ns / 1_000,
        scan_profile.scan_ns / 1_000,
        scan_profile.filter_ns / 1_000,
        scan_profile.project_ns / 1_000,
        scan_profile.scanned_rows,
        scan_profile.decoded_rows,
        scan_profile.emitted_rows,
        probe_profile.total_ns() / 1_000,
        probe_profile.metadata_ns / 1_000,
        probe_profile.drift_ns / 1_000,
        probe_profile.window_read_ns / 1_000,
        probe_profile.exec_ns / 1_000,
    );
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// 事务合并提交的收益实测：同样的写，逐条 vs 一个事务。
#[test]
fn transaction_batches_the_publish_cost() {
    let path = temp_yydx("batch");
    let file = EmbedDb::open(&path).unwrap();
    file.ensure_schema(SCHEMA).unwrap();
    let started = Instant::now();
    for index in 0..200 {
        insert(&file, &format!("one-{index:05}"), index);
    }
    let one_by_one = started.elapsed();

    let started = Instant::now();
    file
        .transaction(|tx| {
            for index in 0..200 {
                insert(tx, &format!("txn-{index:05}"), index);
            }
            Ok(())
        })
        .expect("batched commit");
    let batched = started.elapsed();

    println!(
        "PERF BATCH rows=200\n one_by_one: {one_by_one:?} ({:.1} us/op)\n transaction: {batched:?} ({:.1} us/op)\n speedup: {:.1}x",
        one_by_one.as_micros() as f64 / 200.0,
        batched.as_micros() as f64 / 200.0,
        one_by_one.as_secs_f64() / batched.as_secs_f64(),
    );
    assert!(batched < one_by_one, "a single commit must beat {one_by_one:?} separate publishes");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}
