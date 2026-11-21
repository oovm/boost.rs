//! 复制接口：逐字采用 leader 分配的修订号、幂等重放与状态转移游标。

use database_storage::cluster_kv::{FileShard, InlineValue, Key, KvStore, MemoryShard, Record, StoredValue};
use database_types::cluster::Namespace;

fn record(namespace: &str, key: &[u8], revision: u64) -> Record {
    Record {
        key: Key::new(namespace, key),
        value: StoredValue::Inline(InlineValue(key.to_vec())),
        revision,
        expires_at: None,
    }
}

#[test]
fn a_replicated_record_adopts_the_leader_revision() {
    let mut shard = MemoryShard::new();
    let applied = shard.apply_replicated(record("default", b"k1", 7)).expect("apply");
    assert_eq!(applied, 7);
    let stored = shard.get(&Key::new("default", b"k1")).expect("get").expect("record");
    assert_eq!(stored.revision, 7, "the follower must not allocate its own revision");
    // 下一次本地写必须排在复制修订号之后，序号不会回退。
    let local = shard.put(Key::new("default", b"k2"), StoredValue::Inline(InlineValue(b"v".to_vec()))).expect("put");
    assert_eq!(local, 8);
}

#[test]
fn replaying_same_or_older_revisions_is_idempotent() {
    let mut shard = MemoryShard::new();
    shard.apply_replicated(record("default", b"k1", 3)).expect("apply");
    shard.apply_replicated(record("default", b"k1", 3)).expect("replay");
    shard.apply_replicated(record("default", b"k1", 2)).expect("older");
    let stored = shard.get(&Key::new("default", b"k1")).expect("get").expect("record");
    assert_eq!(stored.revision, 3);
}

#[test]
fn a_replicated_delete_removes_the_key_idempotently() {
    let mut shard = MemoryShard::new();
    shard.apply_replicated(record("default", b"k1", 1)).expect("apply");
    assert!(shard.delete_replicated(&Key::new("default", b"k1")).expect("delete"));
    assert!(!shard.delete_replicated(&Key::new("default", b"k1")).expect("replay"));
    assert!(shard.get(&Key::new("default", b"k1")).expect("get").is_none());
}

#[test]
fn state_transfer_cursor_walks_namespaces_in_key_order() {
    let mut shard = MemoryShard::new();
    shard.apply_replicated(record("a", b"k2", 2)).expect("a/k2");
    shard.apply_replicated(record("a", b"k1", 1)).expect("a/k1");
    shard.apply_replicated(record("b", b"k1", 3)).expect("b/k1");

    let first = shard.replicated_after(None, 2).expect("first page");
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].key, Key::new("a", b"k1"));
    assert_eq!(first[1].key, Key::new("a", b"k2"));

    let second = shard.replicated_after(Some(&first[1].key), 2).expect("second page");
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].key, Key::new("b", b"k1"));
    assert!(shard.replicated_after(Some(&second[0].key), 2).expect("end").is_empty());
}

#[test]
fn file_shard_persists_the_replicated_revision_across_reopen() {
    let directory = std::env::temp_dir().join(format!("boost-yykv-replica-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("directory");
    let path = directory.join("shard-a.yykv");

    {
        let mut shard = FileShard::open(&path).expect("open");
        shard.apply_replicated(record("default", b"k1", 9)).expect("apply");
    }
    {
        let shard = FileShard::open_existing(&path).expect("reopen");
        let stored = shard.get(&Key::new("default", b"k1")).expect("get").expect("record");
        assert_eq!(stored.revision, 9);
        let pages = shard.replicated_after(None, 0).expect("state");
        assert_eq!(pages.len(), 1);
    }
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn a_store_without_replication_support_refuses_instead_of_allocating_revisions() {
    struct Bare;
    impl KvStore for Bare {
        fn get(&self, _key: &Key) -> database_types::cluster::Result<Option<Record>> {
            Ok(None)
        }
        fn put(&mut self, _key: Key, _value: StoredValue) -> database_types::cluster::Result<u64> {
            Ok(1)
        }
        fn put_expiring(&mut self, _key: Key, _value: StoredValue, _expires_at: i64) -> database_types::cluster::Result<u64> {
            Ok(1)
        }
        fn delete(&mut self, _key: &Key) -> database_types::cluster::Result<bool> {
            Ok(false)
        }
        fn scan(
            &self,
            _namespace: &Namespace,
            _prefix: &[u8],
            _after: Option<&[u8]>,
            _before: Option<&[u8]>,
            _limit: usize,
        ) -> database_types::cluster::Result<Vec<Record>> {
            Ok(Vec::new())
        }
    }

    let mut bare = Bare;
    let error = bare.apply_replicated(record("default", b"k1", 1)).expect_err("unsupported");
    assert_eq!(error.code(), "database.shard.unsupported");
}
