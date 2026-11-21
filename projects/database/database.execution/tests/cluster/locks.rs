//! 事务行锁：同一行的第二个写事务等待第一个事务结束，而不是先缓冲、让先提交者失败。
//!
//! 用例在真实 `FileShard` 上驱动 `begin_transaction`：只有带 durable 原子提交边界的
//! 存储才允许开事务，`MemoryShard` 会在 `begin_transaction` 处被拒绝。

use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
    time::Duration,
};

use database_cluster::topology::ShardMap;
use database_execution::cluster::{KeyValueExecutor, LocalExecutor};
use database_query::cluster_kv::{KeyValueAction, KeyValueCommand, KeyValueResult};
use database_storage::cluster_kv::FileShard;
use database_types::cluster::{Namespace, ShardEpoch, ShardId};

const NAMESPACE: &str = "table:locks";
const KEY: &[u8] = b"row-1";

struct Fixture {
    executor: Arc<LocalExecutor<FileShard>>,
    directory: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let directory = std::env::temp_dir().join(format!("database-execution-locks-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let shard = ShardId("local-0".into());
        let executor = LocalExecutor::new(
            ShardMap::new(ShardEpoch(1), vec![shard.clone()]).unwrap(),
            HashMap::from([(shard, FileShard::open(directory.join("local-0.yykv")).unwrap())]),
            Vec::new(),
        )
        .unwrap();
        Self { executor: Arc::new(executor), directory }
    }

    fn command(&self, action: KeyValueAction) -> KeyValueCommand {
        KeyValueCommand { namespace: Namespace(NAMESPACE.to_string()), key: KEY.to_vec(), action }
    }

    fn put(&self, value: &[u8]) -> KeyValueCommand {
        self.command(KeyValueAction::Put(value.to_vec()))
    }

    fn seed(&self, value: &[u8]) {
        self.executor.execute(None, self.put(value)).expect("seed row");
    }

    fn read(&self) -> Option<Vec<u8>> {
        match self.executor.execute(None, self.command(KeyValueAction::Get)).expect("read row") {
            KeyValueResult::Get(value) => value,
            other => panic!("expected a Get result, got {other:?}"),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// 第一个事务 COMMIT 成功；第二个写同一行的事务在它结束前不得推进。
#[test]
fn a_second_writer_waits_for_the_first_transaction_to_commit() {
    let fixture = Fixture::new("commit-wins");
    fixture.seed(b"v0");

    let first = fixture.executor.clone().begin_transaction(None).expect("begin first");
    first.write_if_unchanged(fixture.put(b"v1"), Some(b"v0".to_vec())).expect("first write takes the row lock");

    let (sender, receiver) = mpsc::channel();
    let executor = fixture.executor.clone();
    let command = fixture.put(b"v2");
    let waiter = thread::spawn(move || {
        let second = executor.begin_transaction(None).expect("begin second");
        let buffered = second.write_if_unchanged(command, Some(b"v0".to_vec()));
        let committed = second.commit();
        sender.send((buffered.is_ok(), committed.is_ok())).expect("report the outcome");
    });

    // 第一个事务还持有行锁：第二个写必须阻塞，而不是立即缓冲返回。
    assert!(
        receiver.recv_timeout(Duration::from_millis(250)).is_err(),
        "a second writer must wait for the row lock instead of buffering immediately"
    );

    // 先提交的事务成功发布；这正是修复前失败的一方。
    first.commit().expect("the first transaction commits");
    let (buffered, committed) = receiver.recv_timeout(Duration::from_secs(10)).expect("the waiter wakes after the commit");
    waiter.join().expect("waiter thread");

    // 第二个事务醒来后 pre-image 已过期：它被 OCC 拒绝（可重试冲突），绝不静默覆盖。
    assert!(buffered, "the write itself is accepted once the row lock is granted");
    assert!(!committed, "the waiter read a stale pre-image, so its commit must be refused");
    assert_eq!(fixture.read().as_deref(), Some(b"v1".as_slice()), "the first committer's value survives");
}

/// 第一个事务回滚时释放行锁，等待者随后照常写入并提交。
#[test]
fn rollback_releases_the_row_lock_for_the_waiter() {
    let fixture = Fixture::new("rollback-wakes");
    fixture.seed(b"v0");

    let first = fixture.executor.clone().begin_transaction(None).expect("begin first");
    first.write_if_unchanged(fixture.put(b"v1"), Some(b"v0".to_vec())).expect("first write takes the row lock");

    let (sender, receiver) = mpsc::channel();
    let executor = fixture.executor.clone();
    let command = fixture.put(b"v2");
    let waiter = thread::spawn(move || {
        let second = executor.begin_transaction(None).expect("begin second");
        let buffered = second.write_if_unchanged(command, Some(b"v0".to_vec()));
        let committed = second.commit();
        sender.send((buffered.is_ok(), committed.is_ok())).expect("report the outcome");
    });

    assert!(receiver.recv_timeout(Duration::from_millis(250)).is_err(), "the second writer must wait for the row lock");
    drop(first); // ROLLBACK：丢弃句柄即释放行锁。

    let (buffered, committed) = receiver.recv_timeout(Duration::from_secs(10)).expect("the waiter wakes after the rollback");
    waiter.join().expect("waiter thread");
    assert!(buffered, "the waiter writes once the rollback released the row");
    assert!(committed, "the pre-image is still valid, so the waiter commits");
    assert_eq!(fixture.read().as_deref(), Some(b"v2".as_slice()));
}
