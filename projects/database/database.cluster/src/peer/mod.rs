//! 节点间 peer 通道:join 握手、成员视图、epoch / owner 推进与可选 mTLS。
//!
//! 控制面归本模块:leader 接纳成员、广播成员表并推进 epoch 与 owner;节点在
//! 写入口用 [`ownership::Ownership::admit`] 校验 `FenceToken`。协议只承载控制
//! 事实,不携带 SQL、KV 结果或存储格式。

pub mod frame;
pub mod membership;
pub mod node;
pub mod ownership;
pub mod replica;
pub mod tls;

pub use frame::{
    EpochAnnouncement, Heartbeat, JoinAccepted, JoinRejectCode, JoinRejected, JoinRequest, OwnerSwitch, PeerAck, PeerFrame,
    PeerIdentity, PeerIdentityError, PeerMember, ReplicaAck, ReplicaRecord, Roster, ShardReplica,
};
pub use membership::{MemberView, Membership, MembershipError, MembershipRole};
pub use node::{BroadcastFailure, BroadcastReport, PeerError, PeerNode, PeerOptions, PeerRole, ReplicaDelivery, ReplicaReport, DEFAULT_PEER_TIMEOUT};
pub use ownership::{Ownership, OwnershipError};
pub use replica::{ReplicaCursor, ReplicaError, ReplicaSink};
pub use tls::{PeerTls, PeerTlsError};
