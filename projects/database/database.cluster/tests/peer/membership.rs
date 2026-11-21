use std::{io::Read, net::TcpStream, time::Duration};

use database_cluster::peer::{
    frame::{write_frame, PeerFrame},
    JoinRejectCode, JoinRequest, PeerError,
};
use database_types::cluster::{LeaderTerm, ShardEpoch};

use crate::{address, follower, identity, leader, member_count, wait_for, WAIT};

#[test]
fn a_duplicate_identity_from_another_address_is_refused() {
    let leader = leader("node-1", 1, 1);
    let first = follower("node-2");
    first.join(address(&leader)).expect("first join");

    let impostor = follower("node-2");
    let error = impostor.join(address(&leader)).expect_err("duplicate identity");
    match error {
        PeerError::Rejected(rejected) => assert_eq!(rejected.code, JoinRejectCode::DuplicateIdentity),
        other => panic!("expected duplicate_identity, got {other}"),
    }
    assert_eq!(member_count(&leader), 2, "the duplicate must not be added");
}

#[test]
fn a_stale_epoch_join_is_refused_over_a_real_connection() {
    let leader = leader("node-1", 2, 2);
    let mut stream = TcpStream::connect(address(&leader)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    let request = JoinRequest {
        identity: identity("cluster-a", "node-9"),
        address: "127.0.0.1:4999".to_owned(),
        epoch: ShardEpoch(1),
        term: LeaderTerm(2),
    };
    write_frame(&mut stream, &PeerFrame::Join(request)).expect("write join");
    match database_cluster::peer::frame::read_frame(&mut stream).expect("read reply") {
        PeerFrame::JoinRejected(rejected) => assert_eq!(rejected.code, JoinRejectCode::StaleEpoch),
        other => panic!("expected stale_epoch, got {other:?}"),
    }
    assert_eq!(member_count(&leader), 1);
}

#[test]
fn a_roster_reaches_members_that_join_later() {
    let leader = leader("node-1", 1, 1);
    let second = follower("node-2");
    second.join(address(&leader)).expect("node-2 joins");
    assert!(wait_for(WAIT, || member_count(&second) == 2));

    let third = follower("node-3");
    third.join(address(&leader)).expect("node-3 joins");
    assert!(wait_for(WAIT, || member_count(&second) == 3), "the existing member must learn about node-3");
    assert!(wait_for(WAIT, || member_count(&third) == 3));
}

#[test]
fn a_foreign_cluster_id_is_refused_before_membership_changes() {
    let leader = leader("node-1", 1, 1);
    let mut stream = TcpStream::connect(address(&leader)).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("timeout");
    let request = JoinRequest {
        identity: identity("cluster-b", "node-9"),
        address: "127.0.0.1:4999".to_owned(),
        epoch: ShardEpoch(0),
        term: LeaderTerm(0),
    };
    write_frame(&mut stream, &PeerFrame::Join(request)).expect("write join");
    match database_cluster::peer::frame::read_frame(&mut stream).expect("read reply") {
        PeerFrame::JoinRejected(rejected) => assert_eq!(rejected.code, JoinRejectCode::ClusterMismatch),
        other => panic!("expected cluster_mismatch, got {other:?}"),
    }
    assert_eq!(member_count(&leader), 1);
    let _ = stream.read(&mut [0u8; 1]);
}
