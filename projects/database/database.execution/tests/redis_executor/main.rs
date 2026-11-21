//! Redis 键值执行面：命名空间解析、值种类写入、计数与扫描。

use std::sync::Mutex;

use database_execution::cluster::{KeyValueExecutor, redis::RedisExecutor, redis::collections::handle_set};
use database_protocol_redis::{codec::Reply, resp::Request};
use database_query::cluster_kv::{KeyValueAction, KeyValueCommand, KeyValueResult};
use database_storage::cluster_kv::redis::{RedisShard, namespace};
use database_types::cluster::{FenceToken, Namespace};

fn temporary_dir(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("redis-executor-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    path
}

fn command(db: u8, kind: &str, key: &[u8], action: KeyValueAction) -> KeyValueCommand {
    KeyValueCommand { namespace: Namespace(database_storage::cluster_kv::redis::kind_namespace(db, kind)), key: key.to_vec(), action }
}

#[test]
fn a_stored_value_round_trips_through_the_executor() {
    let data_dir = temporary_dir("roundtrip");
    let shard = Mutex::new(RedisShard::open(&data_dir).expect("open"));
    let executor = RedisExecutor::new(&shard);

    executor.execute(None, command(0, "", b"key", KeyValueAction::Put(b"value".to_vec()))).expect("put");
    let KeyValueResult::Get(Some(value)) = executor.execute(None, command(0, "", b"key", KeyValueAction::Get)).expect("get")
    else {
        panic!("the stored value must be readable");
    };
    assert_eq!(value, b"value");
    assert_eq!(executor.execute(None, command(0, "", b"key", KeyValueAction::Exists)).expect("exists"), KeyValueResult::Exists(true));
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn the_string_namespace_is_the_database_namespace() {
    let data_dir = temporary_dir("namespace");
    let shard = Mutex::new(RedisShard::open(&data_dir).expect("open"));
    let executor = RedisExecutor::new(&shard);
    executor.execute(None, command(4, "", b"key", KeyValueAction::Put(b"value".to_vec()))).expect("put");
    assert_eq!(database_storage::cluster_kv::redis::kind_namespace(4, ""), namespace(4));
    assert!(shard.lock().expect("lock").get(4, b"key").is_some());
    assert!(shard.lock().expect("lock").get_kind(4, "hash", b"key").is_none());
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn hash_values_carry_their_field_count_across_the_boundary() {
    let data_dir = temporary_dir("hash");
    let shard = Mutex::new(RedisShard::open(&data_dir).expect("open"));
    let executor = RedisExecutor::new(&shard);

    // 上游形状：`u32 字段数`，随后每字段 `u32 名长 + 名 + u32 值长 + 值`。
    let mut counted = Vec::new();
    counted.extend_from_slice(&1_u32.to_le_bytes());
    counted.extend_from_slice(&1_u32.to_le_bytes());
    counted.push(b'f');
    counted.extend_from_slice(&1_u32.to_le_bytes());
    counted.push(b'v');
    executor.execute(None, command(0, "hash", b"key", KeyValueAction::Put(counted.clone()))).expect("put");

    let flat = shard
        .lock()
        .expect("lock")
        .get_kind(0, "hash", b"key")
        .map(|record| database_storage::cluster_kv::redis::format::record_bytes(record).to_vec());
    assert_eq!(flat.expect("hash record").len(), counted.len() - 4, "the shard keeps the flat layout");

    let KeyValueResult::Get(Some(value)) = executor.execute(None, command(0, "hash", b"key", KeyValueAction::Get)).expect("get")
    else {
        panic!("the stored hash must be readable");
    };
    assert_eq!(value, counted);
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn counters_and_renames_stay_typed() {
    let data_dir = temporary_dir("counter");
    let shard = Mutex::new(RedisShard::open(&data_dir).expect("open"));
    let executor = RedisExecutor::new(&shard);

    assert_eq!(
        executor.execute(None, command(0, "", b"n", KeyValueAction::IncrementBy(2))).expect("increment"),
        KeyValueResult::Increment { value: 2 }
    );
    assert_eq!(
        executor.execute(None, command(0, "", b"n", KeyValueAction::IncrementBy(-3))).expect("decrement"),
        KeyValueResult::Increment { value: -1 }
    );
    assert_eq!(
        executor.execute(None, command(0, "hash", b"n", KeyValueAction::IncrementBy(1))).unwrap_err().to_string(),
        "unsupported: increments are only defined for the string kind"
    );
    assert_eq!(
        executor.execute(None, command(0, "", b"n", KeyValueAction::Rename { destination: b"m".to_vec(), if_absent: false })).expect("rename"),
        KeyValueResult::Rename { source_present: true, applied: true }
    );
    let _ = std::fs::remove_dir_all(&data_dir);
}

#[test]
fn a_scan_reads_only_the_requested_kind() {
    let data_dir = temporary_dir("scan");
    let shard = Mutex::new(RedisShard::open(&data_dir).expect("open"));
    let executor = RedisExecutor::new(&shard);
    executor.execute(None, command(0, "", b"a", KeyValueAction::Put(b"1".to_vec()))).expect("put string");
    executor.execute(None, command(0, "set", b"b", KeyValueAction::Put(b"2".to_vec()))).expect("put set");

    let scanned = executor.execute_scan(None, &Namespace(namespace(0)), b"", None, None, 0).expect("scan");
    assert_eq!(scanned, vec![(b"a".to_vec(), b"1".to_vec())]);

    let set_scan = executor
        .execute_scan(None, &Namespace(database_storage::cluster_kv::redis::kind_namespace(0, "set")), b"", None, None, 0)
        .expect("scan set");
    assert_eq!(set_scan, vec![(b"b".to_vec(), b"2".to_vec())]);
    let _ = std::fs::remove_dir_all(&data_dir);
}

/// 服务器面命名空间（`redis:<db>[:kind]`）与执行器使用的 `redis:db<db>` 之间的薄适配器。
///
/// 生产宿主自己做这层映射；这里复用它来驱动真实的集合命令处理器。
#[derive(Debug)]
struct GatewayExecutor(RedisExecutor<'static>);

impl GatewayExecutor {
    fn rewrite(namespace: &Namespace) -> Namespace {
        match namespace.0.strip_prefix("redis:") {
            Some(rest) if !rest.starts_with("db") => Namespace(format!("redis:db{rest}")),
            _ => namespace.clone(),
        }
    }
}

impl KeyValueExecutor for GatewayExecutor {
    fn execute(&self, fence: Option<&FenceToken>, command: KeyValueCommand) -> database_types::cluster::Result<KeyValueResult> {
        self.0.execute(fence, KeyValueCommand { namespace: Self::rewrite(&command.namespace), ..command })
    }
}

fn request(arguments: &[&'static [u8]]) -> Request<'static> {
    Request { arguments: arguments.to_vec(), consumed: 0 }
}

fn integer(reply: Reply) -> i64 {
    match reply {
        Reply::Integer(value) => value,
        other => panic!("expected an integer reply, got {other:?}"),
    }
}

fn revision(executor: &dyn KeyValueExecutor, key: &[u8]) -> Option<u64> {
    match executor.execute(None, command(0, "set", key, KeyValueAction::GetRevision)).expect("revision") {
        KeyValueResult::Revision(revision) => revision,
        other => panic!("expected a revision result, got {other:?}"),
    }
}

/// `SREM` 未命中任何成员时集合内容不变：不得重写同值记录、不得推进修订号。
///
/// `WATCH` 以修订号判定键是否被改过，同值重写会让一次未命中的 `SREM` 误使事务失效。
#[test]
fn srem_of_absent_members_keeps_the_set_revision() {
    let data_dir = temporary_dir("srem-revision");
    let shard: &'static Mutex<RedisShard> = Box::leak(Box::new(Mutex::new(RedisShard::open(&data_dir).expect("open"))));
    let executor = GatewayExecutor(RedisExecutor::new(shard));
    let base = Namespace("redis:0".to_string());

    assert_eq!(integer(handle_set(&request(&[b"SADD", b"tags", b"a", b"b"]), Some(&executor), &base, 2).expect("sadd")), 2);
    let before = revision(&executor, b"tags");
    assert!(before.is_some(), "the set must exist after SADD");

    assert_eq!(integer(handle_set(&request(&[b"SREM", b"tags", b"missing"]), Some(&executor), &base, 2).expect("srem")), 0);
    assert_eq!(revision(&executor, b"tags"), before, "a no-op SREM must leave the record and its revision untouched");

    assert_eq!(integer(handle_set(&request(&[b"SREM", b"tags", b"a"]), Some(&executor), &base, 2).expect("srem")), 1);
    assert_ne!(revision(&executor, b"tags"), before, "a real removal writes a new record");
    let _ = std::fs::remove_dir_all(&data_dir);
}

