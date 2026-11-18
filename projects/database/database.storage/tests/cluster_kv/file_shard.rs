use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use database_formats::yy::yykv::{KV_MAGIC, decode_shard_image};
use database_storage::cluster_kv::{FileShard, InlineValue, Key, KvStore, ObjectRef, ShardChange, StoredValue};
use database_types::cluster::Namespace;

fn temp_path(label: &str) -> PathBuf {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    std::env::temp_dir().join(format!("cluster-kv-{label}-{nonce}.yykv"))
}

fn cleanup(path: &PathBuf) {
    let _ = fs::remove_file(path);
}

/// The append path must leave exactly the live state a compaction of the same shard writes.
///
/// Commits only append delta frames, so the file is decoded (base + replayed frames) before
/// and after `flush()` rewrites it as one base snapshot; both views must be identical.
fn assert_append_matches_compaction(shard: &mut FileShard, path: &Path) {
    let appended = decode_shard_image(&fs::read(path).expect("read appended shard")).expect("decode appended shard");
    shard.flush().expect("compact");
    let compacted = decode_shard_image(&fs::read(path).expect("read compacted shard")).expect("decode compacted shard");
    assert_eq!(appended.records, compacted.records, "the append path must hold the same live records as compaction");
    assert_eq!(appended.next_revision, compacted.next_revision, "the append path must agree on the next revision");
}

#[test]
fn an_empty_shard_is_a_version_four_base_without_delta_frames() {
    let empty = database_formats::yy::yykv::encode_snapshot(&std::collections::BTreeMap::new(), 1).expect("encode");
    assert_eq!(&empty[..KV_MAGIC.len()], KV_MAGIC);
    let image = decode_shard_image(&empty).expect("decode");
    assert_eq!(image.base_len, empty.len(), "an empty shard is a base section only");
    assert_eq!(image.delta_len, 0);
    assert!(!image.torn_tail);
}

#[test]
fn file_shard_append_path_matches_compaction_state() {
    let path = temp_path("incremental-bytes");
    let mut shard = FileShard::open(&path).expect("open");
    let now = database_storage::cluster_kv::now_unix_ms();
    let a = Key::new("ns", b"a");
    let b = Key::new("ns", b"b");
    let c = Key::new("ns", b"c");
    let d = Key::new("other", b"d");

    assert_append_matches_compaction(&mut shard, &path);

    shard.put(a.clone(), StoredValue::Inline(InlineValue(b"1".to_vec()))).unwrap();
    assert_append_matches_compaction(&mut shard, &path);

    // Overwriting the same key must replace its cached segment, never append a second one.
    shard.put(a.clone(), StoredValue::Inline(InlineValue(b"22".to_vec()))).unwrap();
    assert_append_matches_compaction(&mut shard, &path);

    shard.put_expiring(b.clone(), StoredValue::Inline(InlineValue(b"expiring".to_vec())), now + 60_000).unwrap();
    assert_append_matches_compaction(&mut shard, &path);

    shard.put(d.clone(), StoredValue::Object(ObjectRef { hash_hex: "ff00".into() })).unwrap();
    assert_append_matches_compaction(&mut shard, &path);

    // One batch touching a key twice, deleting another and adding an expiry.
    shard
        .commit_changes(vec![
            ShardChange::Put { key: c.clone(), value: StoredValue::Inline(InlineValue(b"c1".to_vec())), expires_at: None },
            ShardChange::Put { key: a.clone(), value: StoredValue::Inline(InlineValue(b"a3".to_vec())), expires_at: Some(now + 30_000) },
            ShardChange::Delete { key: b.clone() },
            ShardChange::Put { key: c.clone(), value: StoredValue::Inline(InlineValue(b"c2".to_vec())), expires_at: None },
        ])
        .expect("batch commit");
    assert_append_matches_compaction(&mut shard, &path);

    // A delete of an absent key must not rewrite the snapshot at all.
    let before = fs::read(&path).unwrap();
    assert!(!shard.delete(&b).unwrap());
    assert_eq!(fs::read(&path).unwrap(), before, "an absent-key delete must leave the file untouched");
    assert_append_matches_compaction(&mut shard, &path);

    // Expiry reclamation goes through the same commit boundary.
    shard.put_expiring(Key::new("ns", b"gone"), StoredValue::Inline(InlineValue(b"x".to_vec())), now - 1).unwrap();
    assert_eq!(shard.sweep_expired(now, None).unwrap().reclaimed, 1);
    assert_append_matches_compaction(&mut shard, &path);

    // A namespace flush removes every key of one namespace in a single commit.
    assert_eq!(KvStore::flush(&mut shard, &Namespace("ns".into())).unwrap(), 2);
    assert_append_matches_compaction(&mut shard, &path);

    drop(shard);

    // A reopened shard rebuilds the byte cache from the snapshot and keeps committing identically.
    let mut reopened = FileShard::open(&path).expect("reopen");
    assert_append_matches_compaction(&mut reopened, &path);
    reopened.put(a, StoredValue::Inline(InlineValue(b"after-reopen".to_vec()))).unwrap();
    assert_append_matches_compaction(&mut reopened, &path);
    cleanup(&path);
}

#[test]
fn failed_commit_restores_records_and_the_next_commit_is_byte_correct() {
    let path = temp_path("failed-commit");
    let backup = path.with_extension("saved");
    let mut shard = FileShard::open(&path).expect("open");
    let a = Key::new("ns", b"a");
    let b = Key::new("ns", b"b");
    shard.put(a.clone(), StoredValue::Inline(InlineValue(b"original".to_vec()))).unwrap();
    let published = fs::read(&path).unwrap();
    fs::rename(&path, &backup).unwrap();
    fs::create_dir(&path).unwrap();

    shard
        .commit_changes(vec![
            ShardChange::Put { key: b.clone(), value: StoredValue::Inline(InlineValue(b"new".to_vec())), expires_at: None },
            ShardChange::Put { key: a.clone(), value: StoredValue::Inline(InlineValue(b"replaced".to_vec())), expires_at: None },
        ])
        .expect_err("the durable boundary is blocked");
    assert!(shard.get(&b).unwrap().is_none(), "a failed batch must not leak the new key");
    assert_eq!(
        shard.get(&a).unwrap().unwrap().value,
        StoredValue::Inline(InlineValue(b"original".to_vec())),
        "the touched key keeps its pre-image"
    );
    assert_eq!(fs::read(&backup).unwrap(), published, "the durable snapshot keeps the pre-commit bytes");

    fs::remove_dir(&path).unwrap();
    fs::rename(&backup, &path).unwrap();

    // A failed commit must not consume revisions: the retry starts where the shard did.
    shard
        .commit_changes(vec![ShardChange::Put { key: b.clone(), value: StoredValue::Inline(InlineValue(b"new".to_vec())), expires_at: None }])
        .expect("retry");
    assert_eq!(shard.get(&b).unwrap().unwrap().revision, 2);
    assert_eq!(shard.get(&a).unwrap().unwrap().revision, 1);
    assert_append_matches_compaction(&mut shard, &path);

    drop(shard);
    let reopened = FileShard::open(&path).expect("reopen after recovery");
    assert_eq!(reopened.get(&b).unwrap().unwrap().revision, 2);
    assert_eq!(reopened.get(&a).unwrap().unwrap().revision, 1);
    cleanup(&path);
}

#[test]
fn file_shard_round_trips_records_and_revisions() {
    let path = temp_path("round-trip");
    let mut shard = FileShard::open(&path).expect("open");
    let inline_key = Key::new("tenant-a", b"inline".to_vec());
    let object_key = Key::new("tenant-b", b"object".to_vec());
    assert_eq!(shard.put(object_key.clone(), StoredValue::Object(ObjectRef { hash_hex: "ab12".into() }),).expect("put object"), 1);
    assert_eq!(shard.put(inline_key.clone(), StoredValue::Inline(InlineValue(b"payload".to_vec()))).expect("put inline"), 2);
    drop(shard);

    let mut reopened = FileShard::open(&path).expect("reopen");
    assert_eq!(reopened.get(&inline_key).unwrap().unwrap().revision, 2);
    assert_eq!(reopened.get(&object_key).unwrap().unwrap().revision, 1);
    assert!(reopened.delete(&inline_key).unwrap());
    drop(reopened);

    let reopened = FileShard::open(&path).expect("reopen after delete");
    assert_eq!(reopened.get(&inline_key).unwrap(), None);
    assert_eq!(reopened.get(&object_key).unwrap().unwrap().revision, 1);
    cleanup(&path);
}

#[test]
fn file_shard_persists_expiry_across_reopen() {
    let path = temp_path("expiry");
    let mut shard = FileShard::open(&path).expect("open");
    let live_key = Key::new("tenant-a", b"live".to_vec());
    let gone_key = Key::new("tenant-a", b"gone".to_vec());
    let now = database_storage::cluster_kv::now_unix_ms();
    shard.put_expiring(live_key.clone(), StoredValue::Inline(InlineValue(b"live".to_vec())), now + 60_000).expect("put live");
    shard.put_expiring(gone_key.clone(), StoredValue::Inline(InlineValue(b"gone".to_vec())), now - 1).expect("put gone");
    // An already-elapsed deadline reads as absent immediately.
    assert!(shard.get(&gone_key).unwrap().is_none());
    drop(shard);

    let mut reopened = FileShard::open(&path).expect("reopen");
    let live = reopened.get(&live_key).unwrap().expect("live record survives reopen");
    assert_eq!(live.expires_at, Some(now + 60_000), "the wall-clock deadline must round-trip the snapshot");
    assert!(reopened.get(&gone_key).unwrap().is_none(), "an expired record stays absent after reopen");
    // A plain put clears the persisted expiry.
    reopened.put(live_key.clone(), StoredValue::Inline(InlineValue(b"plain".to_vec()))).expect("plain put");
    drop(reopened);

    let reopened = FileShard::open(&path).expect("reopen after plain put");
    assert_eq!(reopened.get(&live_key).unwrap().unwrap().expires_at, None);
    cleanup(&path);
}

#[test]
fn file_shard_rejects_corrupt_bytes() {
    let path = temp_path("corrupt");
    let shard = FileShard::open(&path).expect("open");
    drop(shard);
    let mut bytes = fs::read(&path).expect("read");
    bytes[0] ^= 0xff;
    fs::write(&path, bytes).expect("corrupt");
    assert!(FileShard::open(&path).is_err());
    cleanup(&path);
}

#[test]
fn file_shard_rejects_a_flipped_value_byte_via_checksum() {
    let path = temp_path("checksum");
    let mut shard = FileShard::open(&path).expect("open");
    let key = Key::new("tenant", b"row".to_vec());
    shard.put(key, StoredValue::Inline(InlineValue(b"payload".to_vec()))).expect("put");
    drop(shard);
    let mut bytes = fs::read(&path).expect("read");
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&path, bytes).expect("corrupt");
    let error = FileShard::open(&path).expect_err("checksum must reject the flipped value");
    assert!(error.to_string().contains("checksum"), "{error}");
    cleanup(&path);
}

#[test]
fn file_shard_rejects_legacy_v2_snapshots_explicitly() {
    let path = temp_path("legacy");
    fs::write(&path, b"YYKV\x02\x00\x00\x00\x00\x00\x00\x00\x00").expect("write legacy");
    let error = FileShard::open(&path).expect_err("legacy header must be rejected");
    assert!(error.to_string().contains("legacy yykv v2"), "{error}");
    cleanup(&path);
}

#[test]
fn failed_snapshot_publication_preserves_records_and_revision() {
    let path = temp_path("failed-publication");
    let backup = path.with_extension("saved");
    let mut shard = FileShard::open(&path).expect("open");
    let existing = Key::new("tenant", b"existing".to_vec());
    let inserted = Key::new("tenant", b"inserted".to_vec());
    let original = StoredValue::Inline(InlineValue(b"original".to_vec()));
    let replacement = StoredValue::Inline(InlineValue(b"replacement".to_vec()));
    assert_eq!(shard.put(existing.clone(), original.clone()).unwrap(), 1);
    let published = fs::read(&path).unwrap();
    fs::rename(&path, &backup).unwrap();
    fs::create_dir(&path).unwrap();

    assert!(shard.put(existing.clone(), replacement.clone()).is_err());
    let record = shard.get(&existing).unwrap().expect("original remains visible");
    assert_eq!(record.value, original);
    assert_eq!(record.revision, 1);
    assert!(shard.put(inserted.clone(), replacement.clone()).is_err());
    assert_eq!(shard.get(&inserted).unwrap(), None);
    assert!(shard.delete(&existing).is_err());
    assert_eq!(shard.get(&existing).unwrap(), Some(record));
    assert_eq!(fs::read(&backup).unwrap(), published);

    fs::remove_dir(&path).unwrap();
    fs::rename(&backup, &path).unwrap();
    let reopened = FileShard::open(&path).unwrap();
    assert_eq!(reopened.get(&existing).unwrap().unwrap().value, original);
    assert_eq!(reopened.get(&inserted).unwrap(), None);
    drop(reopened);
    assert_eq!(shard.put(existing.clone(), replacement.clone()).unwrap(), 2);
    drop(shard);
    let reopened = FileShard::open(&path).unwrap();
    let record = reopened.get(&existing).unwrap().unwrap();
    assert_eq!(record.value, replacement);
    assert_eq!(record.revision, 2);
    drop(reopened);
    cleanup(&path);
}

#[test]
fn file_shard_scans_ordered_records_in_one_namespace() {
    let path = temp_path("fs-scan");
    let mut shard = FileShard::open(&path).expect("open");
    for &k in &[b"z" as &[u8], b"a", b"m"] {
        shard.put(Key::new("ns", k), StoredValue::Inline(InlineValue(k.to_vec()))).unwrap();
    }
    drop(shard);

    let shard = FileShard::open(&path).expect("reopen");
    let results = shard.scan(&Namespace("ns".into()), b"", None, None, 0).unwrap();
    let keys: Vec<&[u8]> = results.iter().map(|r| r.key.bytes.as_slice()).collect();
    assert_eq!(keys, vec![b"a", b"m", b"z"]);
    cleanup(&path);
}

#[test]
fn file_shard_scan_prefix_and_cursor() {
    let path = temp_path("fs-prefix");
    let mut shard = FileShard::open(&path).expect("open");
    for &k in &[b"10" as &[u8], b"11", b"20", b"21", b"30"] {
        shard.put(Key::new("t", k), StoredValue::Inline(InlineValue(k.to_vec()))).unwrap();
    }
    drop(shard);

    let shard = FileShard::open(&path).expect("reopen");
    // prefix "1" — should be 10, 11
    let mut r = shard.scan(&Namespace("t".into()), b"1", None, None, 0).unwrap();
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].key.bytes, b"10".to_vec());
    // after "20" exclusive — should skip 10, 11, 20, 21 → only 30
    r = shard.scan(&Namespace("t".into()), b"", Some(b"21"), None, 0).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].key.bytes, b"30".to_vec());
    // limit 1 — first only
    r = shard.scan(&Namespace("t".into()), b"", None, None, 1).unwrap();
    assert_eq!(r.len(), 1);
    cleanup(&path);
}

#[test]
fn file_shard_sweep_expired_reclaims_elapsed_records_durably() {
    let path = temp_path("sweep");
    let mut shard = FileShard::open(&path).expect("open");
    let now = database_storage::cluster_kv::now_unix_ms();
    let live = Key::new("ns", b"live");
    let fresh = Key::new("ns", b"fresh");
    let gone = Key::new("ns", b"gone");
    shard.put(live.clone(), StoredValue::Inline(InlineValue(b"v".to_vec()))).unwrap();
    shard.put_expiring(fresh.clone(), StoredValue::Inline(InlineValue(b"v".to_vec())), now + 60_000).unwrap();
    shard.put_expiring(gone.clone(), StoredValue::Inline(InlineValue(b"payload".to_vec())), now - 1).unwrap();

    assert_eq!(shard.sweep_expired(now, None).expect("sweep").reclaimed, 1);
    // Reclaim must survive reopen: the expired key is physically gone, not just filtered.
    drop(shard);
    let mut reopened = FileShard::open(&path).expect("reopen after sweep");
    assert_eq!(
        reopened.sweep_expired(now, None).unwrap(),
        database_types::cluster::SweepReport { examined: 2, reclaimed: 0 },
        "a reopened shard has nothing left to reclaim"
    );
    assert!(reopened.get(&live).unwrap().is_some());
    assert!(reopened.get(&fresh).unwrap().is_some());
    assert!(reopened.get(&gone).unwrap().is_none());
    cleanup(&path);
}

#[test]
fn file_shard_sweep_without_expired_records_leaves_the_snapshot_untouched() {
    let path = temp_path("sweep-noop");
    let mut shard = FileShard::open(&path).expect("open");
    let now = database_storage::cluster_kv::now_unix_ms();
    shard.put(Key::new("ns", b"live"), StoredValue::Inline(InlineValue(b"v".to_vec()))).unwrap();
    shard.put_expiring(Key::new("ns", b"fresh"), StoredValue::Inline(InlineValue(b"v".to_vec())), now + 60_000).unwrap();
    let published = fs::read(&path).unwrap();

    assert_eq!(shard.sweep_expired(now, None).unwrap(), database_types::cluster::SweepReport { examined: 2, reclaimed: 0 });
    assert_eq!(fs::read(&path).unwrap(), published, "a no-op sweep must not rewrite the snapshot");
    cleanup(&path);
}

#[test]
fn file_shard_failed_sweep_preserves_the_previous_snapshot() {
    let path = temp_path("sweep-failure");
    let backup = path.with_extension("saved");
    let mut shard = FileShard::open(&path).expect("open");
    let now = database_storage::cluster_kv::now_unix_ms();
    let gone = Key::new("ns", b"gone");
    let live = Key::new("ns", b"live");
    shard.put_expiring(gone.clone(), StoredValue::Inline(InlineValue(b"v".to_vec())), now - 1).unwrap();
    shard.put(live.clone(), StoredValue::Inline(InlineValue(b"v".to_vec()))).unwrap();
    let published = fs::read(&path).unwrap();
    fs::rename(&path, &backup).unwrap();
    fs::create_dir(&path).unwrap();

    assert!(shard.sweep_expired(now, None).is_err(), "the reclaim must fail when the snapshot cannot be replaced");
    // The in-memory state is not published either: both records stay in their pre-sweep
    // shape (the expired one still filtered from reads, the live one intact).
    assert!(shard.get(&gone).unwrap().is_none());
    assert!(shard.get(&live).unwrap().is_some());
    assert_eq!(fs::read(&backup).unwrap(), published, "the durable snapshot keeps the pre-sweep bytes");

    fs::remove_dir(&path).unwrap();
    fs::rename(&backup, &path).unwrap();
    // After the storage fault clears, the same shard can reclaim again.
    assert_eq!(shard.sweep_expired(now, None).unwrap().reclaimed, 1);
    drop(shard);
    let mut reopened = FileShard::open(&path).unwrap();
    assert_eq!(reopened.sweep_expired(now, None).unwrap().reclaimed, 0);
    cleanup(&path);
}

#[test]
fn file_shard_budgeted_sweep_persists_partial_progress() {
    let path = temp_path("sweep-budget");
    let mut shard = FileShard::open(&path).expect("open");
    let now = database_storage::cluster_kv::now_unix_ms();
    for index in 0..6 {
        let key = Key::new("ns", format!("k{index}").into_bytes());
        shard.put_expiring(key, StoredValue::Inline(InlineValue(b"v".to_vec())), now - 1).unwrap();
    }

    let first = shard.sweep_expired(now, Some(2)).unwrap();
    assert_eq!(first, database_types::cluster::SweepReport { examined: 2, reclaimed: 2 });
    // The partial pass is durable: a reopen sees exactly four expired records left.
    drop(shard);
    let mut reopened = FileShard::open(&path).expect("reopen after budgeted sweep");
    assert_eq!(reopened.sweep_expired(now, None).unwrap(), database_types::cluster::SweepReport { examined: 4, reclaimed: 4 });
    assert_eq!(reopened.sweep_expired(now, None).unwrap(), database_types::cluster::SweepReport::default());
    cleanup(&path);
}

#[test]
fn file_shard_namespaces_len_and_flush_persist() {
    let path = temp_path("namespaces");
    let mut shard = FileShard::open(&path).expect("open");
    shard.put(Key::new("a", b"1"), StoredValue::Inline(InlineValue(b"v1".to_vec()))).unwrap();
    shard.put(Key::new("a", b"2"), StoredValue::Inline(InlineValue(b"v2".to_vec()))).unwrap();
    shard.put(Key::new("b", b"1"), StoredValue::Inline(InlineValue(b"v3".to_vec()))).unwrap();

    assert_eq!(shard.namespaces().unwrap(), vec![Namespace("a".into()), Namespace("b".into())]);
    assert_eq!(shard.len(&Namespace("a".into())).unwrap(), 2);
    assert_eq!(KvStore::flush(&mut shard, &Namespace("a".into())).unwrap(), 2);
    assert_eq!(shard.len(&Namespace("a".into())).unwrap(), 0);
    drop(shard);

    let reopened = FileShard::open(&path).expect("reopen after flush");
    assert_eq!(reopened.len(&Namespace("a".into())).unwrap(), 0);
    assert_eq!(reopened.len(&Namespace("b".into())).unwrap(), 1);
    assert_eq!(reopened.namespaces().unwrap(), vec![Namespace("b".into())]);
    cleanup(&path);
}

#[test]
fn write_batch_publishes_to_memory_and_persists_once_on_end() {
    let path = temp_path("batch");
    let mut shard = FileShard::open(&path).expect("open");
    let a = Key::new("ns", b"a");
    let b = Key::new("ns", b"b");
    shard.put(a.clone(), StoredValue::Inline(InlineValue(b"1".to_vec()))).expect("seed");
    let published = fs::read(&path).expect("read seed snapshot");

    shard.begin_write_batch();
    shard
        .commit_changes(vec![
            ShardChange::Put { key: b.clone(), value: StoredValue::Inline(InlineValue(b"new".to_vec())), expires_at: None },
            ShardChange::Delete { key: a.clone() },
        ])
        .expect("stage");
    // Staged writes are immediately visible to reads, including the reads a later statement
    // of the same command performs, but the snapshot file is untouched until the scope ends.
    assert!(shard.get(&b).expect("get").is_some(), "a staged write must be readable");
    assert!(shard.get(&a).expect("get").is_none(), "a staged delete must be readable");
    assert_eq!(fs::read(&path).expect("read"), published, "a batch scope must not persist per write");

    shard.end_write_batch().expect("end batch");
    assert_ne!(fs::read(&path).expect("read"), published, "ending the scope publishes the snapshot");
    drop(shard);

    let reopened = FileShard::open(&path).expect("reopen after batch");
    assert!(reopened.get(&a).expect("get").is_none());
    assert!(reopened.get(&b).expect("get").is_some());
    cleanup(&path);
}

#[test]
fn failed_write_batch_commit_restores_the_pre_scope_state() {
    let path = temp_path("batch-failure");
    let backup = path.with_extension("saved");
    let mut shard = FileShard::open(&path).expect("open");
    let a = Key::new("ns", b"a");
    let b = Key::new("ns", b"b");
    shard.put(a.clone(), StoredValue::Inline(InlineValue(b"1".to_vec()))).expect("seed");
    let published = fs::read(&path).expect("read seed snapshot");
    fs::rename(&path, &backup).unwrap();
    fs::create_dir(&path).unwrap();

    shard.begin_write_batch();
    shard
        .commit_changes(vec![ShardChange::Put { key: b.clone(), value: StoredValue::Inline(InlineValue(b"new".to_vec())), expires_at: None }])
        .expect("stage");
    shard.commit_changes(vec![ShardChange::Delete { key: a.clone() }]).expect("stage delete");
    assert!(shard.get(&b).expect("get").is_some());
    assert!(shard.end_write_batch().is_err(), "the durable boundary is blocked");

    // The whole scope is rolled back: no staged write stays visible and no revision is spent.
    assert!(shard.get(&b).expect("get").is_none(), "a failed batch must not leak the new key");
    assert!(shard.get(&a).expect("get").is_some(), "a failed batch must restore the deleted key");
    assert_eq!(shard.get(&a).unwrap().unwrap().revision, 1);

    fs::remove_dir(&path).unwrap();
    fs::rename(&backup, &path).unwrap();
    shard.begin_write_batch();
    shard
        .commit_changes(vec![ShardChange::Put { key: b.clone(), value: StoredValue::Inline(InlineValue(b"retry".to_vec())), expires_at: None }])
        .expect("stage retry");
    shard.end_write_batch().expect("retry batch");
    assert_eq!(shard.get(&b).unwrap().unwrap().revision, 2, "a failed batch must not consume revisions");
    drop(shard);
    let reopened = FileShard::open(&path).expect("reopen after retry");
    assert_eq!(reopened.get(&b).unwrap().unwrap().revision, 2);
    cleanup(&path);
}

#[test]
fn a_commit_appends_a_frame_instead_of_rewriting_the_shard() {
    let path = temp_path("append-only");
    let mut shard = FileShard::open(&path).expect("open");
    for index in 0..4_u8 {
        shard.put(Key::new("ns", vec![b'k', b'0' + index]), StoredValue::Inline(InlineValue(vec![b'v'; 64]))).expect("seed");
    }
    let before = fs::read(&path).expect("read").len();
    shard.put(Key::new("ns", b"extra"), StoredValue::Inline(InlineValue(b"payload".to_vec()))).expect("append");
    let after = fs::read(&path).expect("read").len();
    assert!(after > before, "a commit must make its own bytes durable");
    assert!(after - before < 256, "a commit must append one frame, not rewrite the {before}-byte shard");
    cleanup(&path);
}

#[test]
fn an_interrupted_append_is_cut_back_to_the_last_frame_on_reopen() {
    let path = temp_path("torn-tail");
    let mut shard = FileShard::open(&path).expect("open");
    shard.put(Key::new("ns", b"kept"), StoredValue::Inline(InlineValue(b"v".to_vec()))).expect("seed");
    let committed = fs::read(&path).expect("read commit");
    drop(shard);
    // Simulate a crash inside an append: a partial frame after the last commit boundary.
    let mut torn = committed.clone();
    torn.extend_from_slice(&[16, 0, 0, 0, 1, 2, 3]);
    fs::write(&path, &torn).expect("write torn tail");

    let mut reopened = FileShard::open(&path).expect("reopen cuts the torn tail");
    assert_eq!(
        reopened.get(&Key::new("ns", b"kept")).expect("get").expect("kept").value,
        StoredValue::Inline(InlineValue(b"v".to_vec()))
    );
    reopened.put(Key::new("ns", b"next"), StoredValue::Inline(InlineValue(b"w".to_vec()))).expect("append after recovery");
    drop(reopened);
    let reopened = FileShard::open(&path).expect("reopen after recovery append");
    assert!(reopened.get(&Key::new("ns", b"next")).expect("get").is_some());
    cleanup(&path);
}

#[test]
fn open_existing_does_not_create_a_missing_shard() {
    let path = temp_path("missing");
    assert!(matches!(FileShard::open_existing(&path), Err(database_types::cluster::Error::NotFound { .. })));
    assert!(!path.exists(), "a read-only open must not install a shard");
}
