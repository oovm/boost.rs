use database_cluster::peer::{EpochAnnouncement, OwnerSwitch, OwnershipError, PeerError};
use database_types::cluster::{FenceToken, LeaderTerm, OwnerEntry, ReplicaNodeId, ShardEpoch, ShardId};

use crate::{address, follower, leader, wait_for, WAIT};

fn owner(shard: &str, node: &str, term: u64) -> OwnerEntry {
    OwnerEntry { shard_id: ShardId(shard.to_owned()), node_id: ReplicaNodeId(node.to_owned()), leader_term: LeaderTerm(term) }
}

fn fence(epoch: u64, term: u64, node: &str) -> FenceToken {
    FenceToken { epoch: ShardEpoch(epoch), term: LeaderTerm(term), leader_node_id: ReplicaNodeId(node.to_owned()) }
}

#[test]
fn an_epoch_announcement_reaches_every_member() {
    let leader = leader("node-1", 1, 1);
    let second = follower("node-2");
    second.join(address(&leader)).expect("join");

    let report = leader
        .announce_epoch(EpochAnnouncement { epoch: ShardEpoch(2), term: LeaderTerm(2), owners: vec![owner("shard-a", "node-1", 2)] })
        .expect("announce");
    assert_eq!(report.failed_count(), 0, "broadcast failures: {:?}", report.failed);
    assert!(wait_for(WAIT, || second.ownership().epoch() == ShardEpoch(2)), "follower must apply the announcement");
    assert_eq!(second.ownership().owner(&ShardId("shard-a".to_owned())).expect("owner").node_id.0, "node-1");
}

#[test]
fn an_old_owner_write_is_rejected_after_a_promotion() {
    let leader = leader("node-1", 1, 1);
    let second = follower("node-2");
    second.join(address(&leader)).expect("join");
    leader
        .announce_epoch(EpochAnnouncement { epoch: ShardEpoch(2), term: LeaderTerm(2), owners: vec![owner("shard-a", "node-1", 2)] })
        .expect("announce epoch 2");
    assert!(wait_for(WAIT, || second.ownership().epoch() == ShardEpoch(2)));
    let old = fence(2, 2, "node-1");
    second.ownership().admit(&ShardId("shard-a".to_owned()), &old).expect("the published owner may write");

    let report = leader
        .switch_owner(OwnerSwitch {
            epoch: ShardEpoch(3),
            term: LeaderTerm(3),
            shard_id: "shard-a".to_owned(),
            old_primary: "node-1".to_owned(),
            new_primary: "node-2".to_owned(),
        })
        .expect("switch owner");
    assert_eq!(report.failed_count(), 0, "broadcast failures: {:?}", report.failed);
    assert!(wait_for(WAIT, || second.ownership().term() == LeaderTerm(3)));

    let rejected = second.ownership().admit(&ShardId("shard-a".to_owned()), &old).expect_err("stale fence must be rejected");
    assert_eq!(rejected.code(), "database.shard.fencing.rejected");
    assert_eq!(second.ownership().epoch(), ShardEpoch(3));
    assert_eq!(second.ownership().owner(&ShardId("shard-a".to_owned())).expect("owner").node_id.0, "node-2");
    second.ownership().admit(&ShardId("shard-a".to_owned()), &fence(3, 3, "node-2")).expect("the new owner may write");
}

#[test]
fn a_replayed_older_announcement_never_rolls_the_owner_back() {
    let leader = leader("node-1", 1, 1);
    let second = follower("node-2");
    second.join(address(&leader)).expect("join");
    leader
        .announce_epoch(EpochAnnouncement { epoch: ShardEpoch(4), term: LeaderTerm(4), owners: vec![owner("shard-a", "node-2", 4)] })
        .expect("announce epoch 4");
    assert!(wait_for(WAIT, || second.ownership().epoch() == ShardEpoch(4)));

    let error = second
        .ownership()
        .publish(EpochAnnouncement { epoch: ShardEpoch(3), term: LeaderTerm(4), owners: vec![owner("shard-a", "node-1", 4)] })
        .expect_err("an older announcement must be refused");
    assert_eq!(error.code(), "database.cluster.stale_epoch");
    assert_eq!(second.ownership().owner(&ShardId("shard-a".to_owned())).expect("owner").node_id.0, "node-2");
}

#[test]
fn only_the_leader_can_advance_the_epoch() {
    let leader = leader("node-1", 1, 1);
    let second = follower("node-2");
    second.join(address(&leader)).expect("join");
    let error = second
        .announce_epoch(EpochAnnouncement { epoch: ShardEpoch(2), term: LeaderTerm(2), owners: Vec::new() })
        .expect_err("followers must not announce");
    assert!(matches!(error, PeerError::NotLeader), "unexpected error: {error}");
    assert_eq!(second.ownership().epoch(), ShardEpoch(0), "a follower keeps no owner truth until the leader announces one");
    assert_eq!(leader.ownership().epoch(), ShardEpoch(1));
}

#[test]
fn a_switch_that_disagrees_with_the_current_owner_is_refused() {
    let leader = leader("node-1", 1, 1);
    leader
        .announce_epoch(EpochAnnouncement { epoch: ShardEpoch(2), term: LeaderTerm(2), owners: vec![owner("shard-a", "node-1", 2)] })
        .expect("announce");
    let error = leader
        .switch_owner(OwnerSwitch {
            epoch: ShardEpoch(3),
            term: LeaderTerm(3),
            shard_id: "shard-a".to_owned(),
            old_primary: "node-9".to_owned(),
            new_primary: "node-2".to_owned(),
        })
        .expect_err("wrong old primary");
    match error {
        PeerError::Ownership(OwnershipError::NotOwner { .. }) => {}
        other => panic!("expected an owner conflict, got {other}"),
    }
    assert_eq!(leader.ownership().owner(&ShardId("shard-a".to_owned())).expect("owner").node_id.0, "node-1");
}
