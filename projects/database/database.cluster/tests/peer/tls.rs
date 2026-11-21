//! peer mTLS:正确材料、错误凭据与明文降级都必须有明确结果。

use std::{path::PathBuf, time::Duration};

use database_cluster::peer::{
    PeerError, PeerIdentity, PeerNode, PeerOptions, PeerTls,
};
use database_types::cluster::{LeaderTerm, ShardEpoch};

use crate::{address, identity, wait_for, WAIT};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/peer-tls").join(name)
}

fn node_tls() -> PeerTls {
    PeerTls::from_files(fixture("node.pem"), fixture("node.key"), fixture("ca.pem")).expect("node TLS material")
}

fn rogue_tls() -> PeerTls {
    PeerTls::from_files(fixture("rogue-node.pem"), fixture("rogue-node.key"), fixture("ca.pem")).expect("rogue TLS material")
}

fn tls_leader(node: &str) -> PeerNode {
    PeerNode::start(PeerOptions::leader(
        identity("cluster-a", node),
        "127.0.0.1:0".parse().expect("bind"),
        ShardEpoch(1),
        LeaderTerm(1),
        Some(node_tls()),
    ))
    .expect("TLS leader listener")
}

fn tls_follower(node: &str) -> PeerNode {
    PeerNode::start(PeerOptions::follower(identity("cluster-a", node), "127.0.0.1:0".parse().expect("bind"), Some(node_tls())))
        .expect("TLS follower listener")
}

fn member_count(node: &PeerNode) -> usize {
    node.view().map(|view| view.len()).unwrap_or(0)
}

#[test]
fn peers_with_matching_ca_join_over_mutual_tls() {
    let leader = tls_leader("node-1");
    let second = tls_follower("node-2");
    second.join(address(&leader)).expect("mTLS join");
    assert!(wait_for(WAIT, || member_count(&leader) == 2));
    assert!(wait_for(WAIT, || member_count(&second) == 2));

    let heartbeat = second.heartbeat(address(&leader)).expect("heartbeat over TLS");
    assert!(heartbeat.accepted);
    assert_eq!(heartbeat.epoch, ShardEpoch(1));
}

#[test]
fn a_client_certificate_from_an_untrusted_ca_is_refused_before_any_frame() {
    let leader = tls_leader("node-1");
    let rogue = PeerNode::start(PeerOptions::follower(
        PeerIdentity::new("cluster-a", "rogue").expect("identity"),
        "127.0.0.1:0".parse().expect("bind"),
        Some(rogue_tls()),
    ))
    .expect("rogue listener");

    let error = rogue.join(address(&leader)).expect_err("untrusted client certificate must fail");
    let code = error.code();
    assert!(
        code == "database.cluster.peer.tls" || code == "database.cluster.peer.io",
        "TLS rejection must surface as a transport failure, got {code}: {error}"
    );
    assert!(!matches!(error, PeerError::Rejected(_)), "the rogue peer must not reach join processing");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(member_count(&leader), 1, "a failed handshake must not add a member");

    let second = tls_follower("node-2");
    second.join(address(&leader)).expect("a legitimate peer still joins");
    assert!(wait_for(WAIT, || member_count(&leader) == 2));
}

#[test]
fn a_plaintext_peer_cannot_downgrade_a_tls_listener() {
    let leader = tls_leader("node-1");
    let plaintext = PeerNode::start(PeerOptions::follower(identity("cluster-a", "node-2"), "127.0.0.1:0".parse().expect("bind"), None))
        .expect("plaintext listener");

    let error = plaintext.join(address(&leader)).expect_err("plaintext must not be served");
    let code = error.code();
    assert!(
        code == "database.cluster.peer.tls" || code == "database.cluster.peer.io",
        "plaintext must fail at the transport layer, got {code}: {error}"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(member_count(&leader), 1, "no silent plaintext fallback may create a member");
}
