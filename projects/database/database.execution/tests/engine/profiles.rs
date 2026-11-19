//! 读路径 profile：扫描/解码/输出/索引探测分别采集，失败不污染后续执行。

use database_execution::engine::{ClockSource, Connection, ReadProfile};

const SCHEMA: &str = "table User { @@id: i64, name: utf8 }";

fn seeded() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.ensure_schema(SCHEMA).unwrap();
    for id in 1..=64 {
        conn.execute_vos_write(&format!(r#"User.insert({{ id: {id}, name: "user-{id}" }})"#)).unwrap();
    }
    conn
}

#[test]
fn full_scan_reports_scanned_rows_separately_from_the_result() {
    let conn = seeded();
    let stmt = conn.prepare_vos(r#"User.all().filter(x => x.name == "user-7").collect()"#).unwrap();
    let mut profile = ReadProfile::default();
    let rows = conn.query_prepared_ordered_profiled(&stmt, &[], &mut profile).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(profile.emitted_rows, 1, "one row left the pipeline");
    assert_eq!(profile.scanned_rows, 64, "the scan read 64 stored rows, not the output count");
    assert_eq!(profile.decoded_rows, 64);
    assert_eq!(profile.index_probes, 0);
    assert_eq!(profile.fused_ops, 0);
    assert!(profile.scan_ns > 0, "scan timing must be recorded");
    assert_eq!(profile.clock.source, ClockSource::Monotonic);
    assert!(profile.clock.resolution_ns > 0);
}

#[test]
fn a_primary_key_lookup_reports_its_fused_probe() {
    let conn = seeded();
    let stmt = conn.prepare_vos("User.all().filter(x => x.id == 7).collect()").unwrap();
    let mut profile = ReadProfile::default();
    let rows = conn.query_prepared_ordered_profiled(&stmt, &[], &mut profile).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(profile.emitted_rows, 1);
    assert_eq!(profile.index_probes, 1, "one primary-key probe replaced the scan");
    assert_eq!(profile.fused_ops, 1, "the probe also answered the filter");
    assert_eq!(profile.scanned_rows, 1, "only the probed record was read");
    assert_eq!(profile.decoded_rows, 1);
}

#[test]
fn a_failed_profiled_read_does_not_leak_into_the_next_one() {
    let conn = seeded();
    // Bind failure happens before execution, then a successful profiled read
    // must start from a fresh profile slot.
    let stmt = conn.prepare_vos("User.all().collect()").unwrap();
    let mut failed = ReadProfile::default();
    assert!(conn.query_prepared_ordered_profiled(&stmt, &[database_types::Value::Null], &mut failed).is_err());
    assert_eq!(failed.scanned_rows, 0, "nothing ran, so nothing was counted");

    let mut fresh = ReadProfile::default();
    let rows = conn.query_prepared_ordered_profiled(&stmt, &[], &mut fresh).unwrap();
    assert_eq!(rows.len(), 64);
    assert_eq!(fresh.emitted_rows, 64);
    assert_eq!(fresh.scanned_rows, 64);
}
