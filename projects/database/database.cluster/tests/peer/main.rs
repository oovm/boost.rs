//! 节点间 peer 通道的多 listener 测试。

mod membership;
mod ownership;
mod tls;

use std::{net::SocketAddr, time::Duration};

use database_cluster::peer::{PeerIdentity, PeerNode, PeerOptions};
use database_types::cluster::{LeaderTerm, ShardEpoch};

const WAIT: Duration = Duration::from_secs(5);

fn identity(cluster: &str, node: &str) -> PeerIdentity {
    PeerIdentity::new(cluster, node).expect("identity")
}

fn leader(node: &str, epoch: u64, term: u64) -> PeerNode {
    PeerNode::start(PeerOptions::leader(
        identity("cluster-a", node),
        "127.0.0.1:0".parse().expect("bind"),
        ShardEpoch(epoch),
        LeaderTerm(term),
        None,
    ))
    .expect("leader listener")
}

fn follower(node: &str) -> PeerNode {
    PeerNode::start(PeerOptions::follower(identity("cluster-a", node), "127.0.0.1:0".parse().expect("bind"), None))
        .expect("follower listener")
}

fn wait_for(deadline: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let started = std::time::Instant::now();
    while started.elapsed() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    condition()
}

fn member_count(node: &PeerNode) -> usize {
    node.view().map(|view| view.len()).unwrap_or(0)
}

fn address(node: &PeerNode) -> SocketAddr {
    node.address()
}

#[test]
fn three_listeners_form_one_member_view() {
    let leader = leader("node-1", 1, 1);
    let second = follower("node-2");
    let third = follower("node-3");

    second.join(address(&leader)).expect("node-2 joins");
    third.join(address(&leader)).expect("node-3 joins");

    assert!(wait_for(WAIT, || member_count(&leader) == 3), "leader must observe all three members");
    assert!(wait_for(WAIT, || member_count(&second) == 3), "node-2 must receive the broadcast roster");
    assert!(wait_for(WAIT, || member_count(&third) == 3), "node-3 must receive the broadcast roster");

    for node in [&leader, &second, &third] {
        let view = node.view().expect("view");
        assert_eq!(view.epoch, ShardEpoch(1));
        assert_eq!(view.term, LeaderTerm(1));
        assert_eq!(view.leader.node_id, "node-1");
        assert!(view.member("node-3").is_some());
    }

    let heartbeat = second.heartbeat(address(&leader)).expect("heartbeat");
    assert!(heartbeat.accepted);
    assert_eq!(heartbeat.epoch, ShardEpoch(1));
}

#[test]
fn the_role_is_reported_and_only_the_leader_accepts_joins() {
    let leader = leader("node-1", 1, 1);
    let second = follower("node-2");
    second.join(address(&leader)).expect("join");

    assert!(leader.membership().is_leader());
    assert!(!second.membership().is_leader());

    let third = follower("node-3");
    let error = third.join(address(&second)).expect_err("a follower must not accept joins");
    match error {
        database_cluster::peer::PeerError::Rejected(rejected) => {
            assert_eq!(rejected.code, database_cluster::peer::JoinRejectCode::NotLeader)
        }
        other => panic!("expected a structured join rejection, got {other}"),
    }
    assert_eq!(member_count(&third), 0);
}
