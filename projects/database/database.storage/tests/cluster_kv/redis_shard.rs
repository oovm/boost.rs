//! `.yykv` Redis 键空间分片：值种类命名空间、revision、过期、`WATCH` 与持久化边界。

use std::io;

use database_formats::yy::yykv::{self, InlineValue, Key, ObjectRef, Record as UpstreamRecord, StoredValue};
use database_storage::cluster_kv::redis::{
    KINDS, RedisShard, RenameOutcome, decode_catalog_shard_id, decode_shard, encode_catalog, encode_shard,
    format::{Records, record_bytes},
    kind_namespace, namespace, repair_missing_shard,
};

fn temporary_dir(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("redis-shard-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

#[test]
fn namespaces_follow_the_six_value_kinds() {
    assert_eq!(namespace(0), "redis:db0");
    assert_eq!(kind_namespace(15, ""), "redis:db15");
    assert_eq!(kind_namespace(3, "hash"), "redis:db3:hash");
    assert_eq!(KINDS, ["", "hash", "list", "set", "zset", "stream"]);
}

#[test]
fn a_reopened_shard_keeps_kinds_and_deadlines() -> io::Result<()> {
    let data_dir = temporary_dir("reopen");
    {
        let mut shard = RedisShard::open(&data_dir)?;
        shard.set(0, b"greeting", b"hello", None)?;
        shard.set_kind(0, "hash", b"greeting", b"body", Some(4_000_000_000_000))?;
        shard.flush()?;
        assert_eq!(shard.find_kind(0, b"greeting"), Some(""));
    }
    let reopened = RedisShard::open(&data_dir)?;
    assert_eq!(reopened.get(0, b"greeting").map(record_bytes), Some(b"hello".as_slice()));
    assert_eq!(reopened.deadline_kind(0, "hash", b"greeting"), Some(4_000_000_000_000));
    assert_eq!(reopened.len(0), 1, "one logical key, not one per kind");
    let _ = std::fs::remove_dir_all(&data_dir);
    Ok(())
}

#[test]
fn a_batch_defers_the_snapshot_until_it_closes() -> io::Result<()> {
    let data_dir = temporary_dir("batch");
    let mut shard = RedisShard::open(&data_dir)?;
    let before = std::fs::read(shard.shard_path())?;
    shard.begin_batch();
    shard.set(0, b"first", b"1", None)?;
    shard.set(0, b"second", b"2", None)?;
    assert_eq!(std::fs::read(shard.shard_path())?, before, "a batched mutation must not rewrite the shard");
    assert_eq!(shard.get(0, b"first").map(record_bytes), Some(b"1".as_slice()));
    shard.end_batch()?;
    assert_ne!(std::fs::read(shard.shard_path())?, before);
    let reopened = RedisShard::open(&data_dir)?;
    assert_eq!(reopened.get(0, b"second").map(record_bytes), Some(b"2".as_slice()));
    let _ = std::fs::remove_dir_all(&data_dir);
    Ok(())
}

#[test]
fn expiry_hides_reads_and_the_sweep_reclaims_them() -> io::Result<()> {
    let data_dir = temporary_dir("expiry");
    let mut shard = RedisShard::open(&data_dir)?;
    shard.set(2, b"soon", b"value", Some(1))?;
    assert!(shard.get(2, b"soon").is_none(), "an elapsed deadline reads as absent");
    assert!(!shard.contains(2, b"soon"));
    let expired = shard.sweep_expired(1_000)?;
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].db, 2);
    assert_eq!(expired[0].key, b"soon");
    assert!(shard.sweep_expired(1_000)?.is_empty(), "a second pass touches nothing");
    let _ = std::fs::remove_dir_all(&data_dir);
    Ok(())
}

#[test]
fn watch_versions_bump_on_write_and_release_at_zero() -> io::Result<()> {
    let data_dir = temporary_dir("watch");
    let mut shard = RedisShard::open(&data_dir)?;
    assert_eq!(shard.watch_key(0, b"key"), 0);
    assert_eq!(shard.watch_key(0, b"key"), 0, "a repeated watch shares one slot");
    shard.set(0, b"key", b"1", None)?;
    assert_eq!(shard.watch_version(0, b"key"), 1);
    shard.set(0, b"key", b"1", None)?;
    assert_eq!(shard.watch_version(0, b"key"), 2, "a byte-identical rewrite is a modification");
    shard.set(0, b"other", b"1", None)?;
    assert_eq!(shard.watch_version(0, b"key"), 2, "an unrelated key does not bump the version");
    shard.unwatch_key(0, b"key");
    assert_eq!(shard.watch_version(0, b"key"), 2, "one reference is still held");
    shard.unwatch_key(0, b"key");
    assert_eq!(shard.watch_version(0, b"key"), 0);
    let _ = std::fs::remove_dir_all(&data_dir);
    Ok(())
}

#[test]
fn rename_carries_the_kind_and_keeps_one_representation() -> io::Result<()> {
    let data_dir = temporary_dir("rename");
    let mut shard = RedisShard::open(&data_dir)?;
    shard.set_kind(0, "zset", b"source", b"members", Some(4_000_000_000_000))?;
    shard.set(0, b"destination", b"old", None)?;
    assert_eq!(shard.rename(0, b"source", b"destination", false)?, RenameOutcome::Renamed);
    assert_eq!(shard.find_kind(0, b"destination"), Some("zset"));
    assert_eq!(shard.deadline_kind(0, "zset", b"destination"), Some(4_000_000_000_000));
    assert_eq!(shard.find_kind(0, b"source"), None);
    shard.set(0, b"occupied", b"value", None)?;
    assert_eq!(shard.rename(0, b"destination", b"occupied", true)?, RenameOutcome::DestinationPresent);
    assert_eq!(shard.rename(0, b"missing", b"destination", false)?, RenameOutcome::Missing);
    shard.flush_db(0)?;
    assert!(shard.is_empty(0));
    let _ = std::fs::remove_dir_all(&data_dir);
    Ok(())
}

#[test]
fn counters_stay_buffered_until_a_flush() -> io::Result<()> {
    let data_dir = temporary_dir("counter");
    let mut shard = RedisShard::open(&data_dir)?;
    shard.set(0, b"counter", b"41", None)?;
    shard.flush()?;
    let before = std::fs::read(shard.shard_path())?;
    assert_eq!(shard.incr_by(0, b"counter", 1)?, Ok(42));
    assert_eq!(shard.get(0, b"counter").map(record_bytes), Some(b"42".as_slice()));
    assert_eq!(std::fs::read(shard.shard_path())?, before, "a counter update does not rewrite the shard");
    shard.set(0, b"text", b"not a number", None)?;
    assert_eq!(shard.incr_by(0, b"text", 1)?, Err(database_storage::cluster_kv::redis::IncrementError::NotAnInteger));
    shard.flush()?;
    let reopened = RedisShard::open(&data_dir)?;
    assert_eq!(reopened.get(0, b"counter").map(record_bytes), Some(b"42".as_slice()));
    let _ = std::fs::remove_dir_all(&data_dir);
    Ok(())
}

#[test]
fn a_missing_shard_is_recreated_from_its_catalog() -> io::Result<()> {
    let data_dir = temporary_dir("repair");
    {
        let mut shard = RedisShard::open(&data_dir)?;
        shard.set(0, b"key", b"value", None)?;
    }
    let shard_path = data_dir.join("local-0.yykv");
    std::fs::remove_file(&shard_path)?;
    assert!(RedisShard::open(&data_dir).is_err(), "a registered shard that vanished is an open error");
    assert_eq!(repair_missing_shard(&data_dir)?, "local-0");
    assert!(shard_path.exists());
    let shard = RedisShard::open(&data_dir)?;
    assert!(shard.is_empty(0), "the recovered shard starts empty");
    let _ = std::fs::remove_dir_all(&data_dir);
    Ok(())
}

#[test]
fn the_catalog_round_trips_and_validates_its_fields() {
    let bytes = encode_catalog("local-0");
    assert_eq!(decode_catalog_shard_id(&bytes).expect("decode").as_str(), "local-0");
    assert_eq!(decode_catalog_shard_id(b"nope").unwrap_err().field, "length");
}

#[test]
fn snapshots_match_the_upstream_layout_and_reject_unstorable_values() {
    let key = Key::new(namespace(0), b"alpha".to_vec());
    let mut records = Records::new();
    records.insert(
        key.clone(),
        UpstreamRecord { key: key.clone(), value: StoredValue::Inline(InlineValue(b"one".to_vec())), revision: 1, expires_at: None },
    );
    let bytes = encode_shard(&records, 2).expect("encode");
    assert_eq!(bytes, yykv::encode_snapshot(&records, 2).expect("upstream encode"));
    assert_eq!(decode_shard(&bytes).expect("decode").0, records);

    let object = Key::new(namespace(0), b"large".to_vec());
    let mut shared = Records::new();
    shared.insert(
        object.clone(),
        UpstreamRecord { key: object, value: StoredValue::Object(ObjectRef { hash_hex: "ab".to_owned() }), revision: 1, expires_at: None },
    );
    let shared_bytes = yykv::encode_snapshot(&shared, 2).expect("upstream writes object values");
    assert_eq!(decode_shard(&shared_bytes).unwrap_err().0, "shared object payloads are not supported here");

    let mut legacy = yykv::KV_LEGACY_MAGIC.to_vec();
    legacy.extend_from_slice(&[0_u8; 8]);
    assert_eq!(
        decode_shard(&legacy).unwrap_err().0,
        "legacy .yykv v2 snapshot carries no checksum: delete the shard and let the node recreate it"
    );
}
