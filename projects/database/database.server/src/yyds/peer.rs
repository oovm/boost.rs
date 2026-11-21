//! 节点间写入口:先做 owner / epoch 准入,再交给本地执行器。
//!
//! 控制面(成员、epoch、owner)归 `database.cluster::peer`;执行器归
//! `database.execution`。本模块只把两者接在一条路径上,使旧 owner 在隔离解除
//! 后仍能收到写请求时,得到可识别的 `database.shard.fencing.rejected`,而不是
//! 静默写入分片。

use std::sync::Arc;

use database_cluster::peer::{Ownership, PeerNode, PeerRole};
use database_execution::cluster::KeyValueExecutor;
use database_query::cluster_kv::{KeyValueCommand, KeyValueResult};
use database_types::cluster::{FenceToken, Result as ClusterResult, ShardId};

/// 节点本地的 peer 写入口。
#[derive(Debug)]
pub struct NodePeer {
    peer: Arc<PeerNode>,
    executor: Arc<dyn KeyValueExecutor>,
}

impl NodePeer {
    /// 用已启动的 peer 监听端和本节点执行器组装写入口。
    pub fn new(peer: Arc<PeerNode>, executor: Arc<dyn KeyValueExecutor>) -> Self {
        Self { peer, executor }
    }

    /// 底层 peer 监听端。
    pub fn peer(&self) -> &Arc<PeerNode> {
        &self.peer
    }

    /// 本节点当前的 owner / epoch 真值。
    pub fn ownership(&self) -> &Arc<Ownership> {
        self.peer.ownership()
    }

    /// 本节点角色。
    pub fn role(&self) -> PeerRole {
        if self.peer.membership().is_leader() {
            match self.peer.view() {
                Some(view) => PeerRole::Leader { epoch: view.epoch, term: view.term },
                None => PeerRole::Follower,
            }
        }
        else {
            PeerRole::Follower
        }
    }

    /// 带 fence 的写路径:准入失败时返回 `FencingRejected`,不触碰分片。
    pub fn execute(&self, shard: &ShardId, fence: &FenceToken, command: KeyValueCommand) -> ClusterResult<KeyValueResult> {
        self.ownership().admit(shard, fence)?;
        self.executor.execute(Some(fence), command)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::Arc,
        time::{Duration, Instant},
    };

    use database_cluster::{
        peer::{EpochAnnouncement, JoinRejectCode, OwnerSwitch, PeerError, PeerIdentity, PeerNode, PeerOptions, PeerRole},
        topology::ShardMap,
    };
    use database_execution::cluster::{KeyValueExecutor, LocalExecutor};
    use database_query::cluster_kv::{KeyValueAction, KeyValueCommand};
    use database_storage::cluster_kv::MemoryShard;
    use database_types::cluster::{
        Error as ClusterError, FenceToken, LeaderTerm, Namespace, OwnerEntry, ReplicaNodeId, ShardEpoch, ShardId,
    };

    use super::NodePeer;

    const SHARD: &str = "shard-a";

    fn shard_id() -> ShardId {
        ShardId(SHARD.to_owned())
    }

    fn owner(node: &str, term: u64) -> OwnerEntry {
        OwnerEntry { shard_id: shard_id(), node_id: ReplicaNodeId(node.to_owned()), leader_term: LeaderTerm(term) }
    }

    fn fence(epoch: u64, term: u64, node: &str) -> FenceToken {
        FenceToken { epoch: ShardEpoch(epoch), term: LeaderTerm(term), leader_node_id: ReplicaNodeId(node.to_owned()) }
    }

    fn executor(owners: Vec<OwnerEntry>) -> Arc<LocalExecutor<MemoryShard>> {
        let routing = ShardMap::new(ShardEpoch(1), vec![shard_id()]).expect("routing");
        let mut shards = HashMap::new();
        shards.insert(shard_id(), MemoryShard::new());
        Arc::new(LocalExecutor::new(routing, shards, owners).expect("executor"))
    }

    fn put(key: &str, value: &str) -> KeyValueCommand {
        KeyValueCommand {
            namespace: Namespace("default".to_owned()),
            key: key.as_bytes().to_vec(),
            action: KeyValueAction::Put(value.as_bytes().to_vec()),
        }
    }

    fn get(key: &str) -> KeyValueCommand {
        KeyValueCommand { namespace: Namespace("default".to_owned()), key: key.as_bytes().to_vec(), action: KeyValueAction::Get }
    }

    fn identity(node: &str) -> PeerIdentity {
        PeerIdentity::new("cluster-a", node).expect("identity")
    }

    fn leader_node() -> PeerNode {
        PeerNode::start(PeerOptions::leader(identity("node-1"), "127.0.0.1:0".parse().expect("bind"), ShardEpoch(1), LeaderTerm(1), None))
            .expect("leader")
    }

    fn follower_node(node: &str) -> PeerNode {
        PeerNode::start(PeerOptions::follower(identity(node), "127.0.0.1:0".parse().expect("bind"), None)).expect("follower")
    }

    fn wait_for(deadline: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let started = Instant::now();
        while started.elapsed() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        condition()
    }

    #[test]
    fn an_isolated_old_owner_is_fenced_and_the_new_owner_keeps_one_sequence() {
        let leader = Arc::new(leader_node());
        let follower = follower_node("node-2");
        follower.join(leader.address()).expect("join");

        leader
            .announce_epoch(EpochAnnouncement { epoch: ShardEpoch(2), term: LeaderTerm(2), owners: vec![owner("node-1", 2)] })
            .expect("announce epoch 2");
        assert!(wait_for(Duration::from_secs(5), || follower.ownership().epoch() == ShardEpoch(2)));

        let old_executor = executor(vec![owner("node-1", 2)]);
        let old_owner = NodePeer::new(Arc::clone(&leader), old_executor.clone());
        old_owner.execute(&shard_id(), &fence(2, 2, "node-1"), put("k1", "v1")).expect("the published owner writes");
        assert_eq!(revision(&old_executor, "k1"), Some(1));

        old_owner.execute(&shard_id(), &fence(1, 1, "node-1"), put("k1", "stale")).expect_err("the old fence must be rejected");
        assert_eq!(value(&old_executor, "k1"), Some(b"v1".to_vec()), "a rejected write must not touch the shard");
        assert_eq!(revision(&old_executor, "k1"), Some(1));

        leader
            .switch_owner(OwnerSwitch {
                epoch: ShardEpoch(3),
                term: LeaderTerm(3),
                shard_id: SHARD.to_owned(),
                old_primary: "node-1".to_owned(),
                new_primary: "node-2".to_owned(),
            })
            .expect("switch owner");
        assert!(wait_for(Duration::from_secs(5), || follower.ownership().term() == LeaderTerm(3)));

        let error = old_owner.execute(&shard_id(), &fence(2, 2, "node-1"), put("k1", "late")).expect_err("old owner is fenced");
        match error {
            ClusterError::FencingRejected { current, .. } => assert_eq!(current.node_id.0, "node-2"),
            other => panic!("expected FencingRejected, got {other}"),
        }
        assert_eq!(value(&old_executor, "k1"), Some(b"v1".to_vec()));
        assert_eq!(revision(&old_executor, "k1"), Some(1), "the rejected late write must not advance the sequence");

        let new_executor = executor(vec![owner("node-2", 3)]);
        let new_owner = NodePeer::new(Arc::new(follower), new_executor.clone());
        new_owner.execute(&shard_id(), &fence(3, 3, "node-2"), put("k1", "v2")).expect("the new owner writes");
        assert_eq!(value(&new_executor, "k1"), Some(b"v2".to_vec()));
        assert_eq!(revision(&new_executor, "k1"), Some(1), "the new owner starts its own sequence");

        assert_eq!(new_owner.ownership().epoch(), ShardEpoch(3));
        assert_eq!(new_owner.ownership().term(), LeaderTerm(3));
        assert_eq!(new_owner.ownership().owner(&shard_id()).expect("owner").node_id.0, "node-2");
        assert_eq!(old_owner.ownership().epoch(), ShardEpoch(3), "both sides converge on the routing epoch");
        assert_eq!(old_owner.ownership().term(), LeaderTerm(3), "both sides converge on the leader term");
    }

    #[test]
    fn a_follower_that_never_joined_cannot_admit_writes() {
        let follower = Arc::new(follower_node("node-3"));
        let executor = executor(Vec::new());
        let node = NodePeer::new(Arc::clone(&follower), executor);
        let error = node.execute(&shard_id(), &fence(1, 1, "node-3"), put("k1", "v1")).expect_err("no owner truth yet");
        assert_eq!(error.code(), "database.shard.fencing.rejected");
        assert_eq!(node.role(), PeerRole::Follower);
    }

    #[test]
    fn a_rejected_join_is_reported_with_its_code() {
        let leader = leader_node();
        let first = follower_node("node-2");
        first.join(leader.address()).expect("join");
        let duplicate = follower_node("node-2");
        match duplicate.join(leader.address()).expect_err("duplicate identity") {
            PeerError::Rejected(rejected) => assert_eq!(rejected.code, JoinRejectCode::DuplicateIdentity),
            other => panic!("expected duplicate_identity, got {other}"),
        }
    }

    fn value(executor: &LocalExecutor<MemoryShard>, key: &str) -> Option<Vec<u8>> {
        match executor.execute(None, get(key)).expect("read") {
            database_query::cluster_kv::KeyValueResult::Get(value) => value,
            other => panic!("expected a Get result, got {other:?}"),
        }
    }

    fn revision(executor: &LocalExecutor<MemoryShard>, key: &str) -> Option<u64> {
        let command = KeyValueCommand {
            namespace: Namespace("default".to_owned()),
            key: key.as_bytes().to_vec(),
            action: KeyValueAction::GetRevision,
        };
        match executor.execute(None, command).expect("revision") {
            database_query::cluster_kv::KeyValueResult::Revision(revision) => revision,
            other => panic!("expected a Revision result, got {other:?}"),
        }
    }
}
