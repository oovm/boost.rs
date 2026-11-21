//! join 握手与成员视图状态机。
//!
//! leader 单独决定成员表:它接纳 join、拒绝重复身份与旧 epoch,并把完整成员表
//! 广播给每个人。follower 只接受不比本地更新的成员表;不知道 epoch 的首次加入
//! 用 `0` 表示,不会被误判为陈旧。

use std::{
    fmt,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        RwLock,
    },
};

use database_types::cluster::{LeaderTerm, ShardEpoch};

use super::frame::{
    JoinAccepted, JoinRejectCode, JoinRejected, JoinRequest, PeerIdentity, PeerIdentityError, PeerMember, Roster,
};

/// 成员表的一份快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberView {
    /// 集群 id。
    pub cluster_id: String,
    /// 当前 leader。
    pub leader: PeerMember,
    /// 成员 epoch。
    pub epoch: ShardEpoch,
    /// leader term。
    pub term: LeaderTerm,
    /// 成员列表,按 node id 稳定排序。
    pub members: Vec<PeerMember>,
}

impl MemberView {
    /// 成员数量(包含 leader)。
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// 成员表是否为空;合法视图至少包含 leader。
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// 按 node id 查找成员。
    pub fn member(&self, node_id: &str) -> Option<&PeerMember> {
        self.members.iter().find(|member| member.node_id == node_id)
    }
}

/// 本节点在成员协议中的角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipRole {
    /// 接纳 join 并广播成员表。
    Leader,
    /// 只接受 leader 广播的成员表。
    Follower,
}

/// 成员协议失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MembershipError {
    /// 只有 leader 可以接纳 join 或提升 epoch。
    NotLeader,
    /// 集群 id 不一致。
    ClusterMismatch,
    /// 节点 id 或地址不合法。
    InvalidIdentity,
    /// 同一 node id 已从另一个地址加入。
    DuplicateIdentity {
        /// 冲突的 node id。
        node_id: String,
    },
    /// 请求 epoch 早于已知 epoch。
    StaleEpoch {
        /// 请求携带的 epoch。
        requested: ShardEpoch,
        /// 已知 epoch。
        current: ShardEpoch,
    },
    /// 请求 term 早于已知 term。
    StaleTerm {
        /// 请求携带的 term。
        requested: LeaderTerm,
        /// 已知 term。
        current: LeaderTerm,
    },
    /// 请求 epoch 或 term 领先于本地 leader。
    EpochAhead {
        /// 请求携带的 epoch。
        requested: ShardEpoch,
        /// 请求携带的 term。
        requested_term: LeaderTerm,
        /// 已知 epoch。
        current: ShardEpoch,
        /// 已知 term。
        current_term: LeaderTerm,
    },
    /// leader 不接受外部广播的成员表。
    RoleViolation,
    /// 成员表不比本地新。
    StaleRoster {
        /// 广播的 epoch。
        epoch: ShardEpoch,
        /// 广播的 term。
        term: LeaderTerm,
    },
    /// 同一 `(epoch, term)` 出现了不同 leader。
    ConflictingRoster,
    /// 成员视图状态损坏。
    Corrupt,
}

impl fmt::Display for MembershipError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLeader => formatter.write_str("this node is not the membership leader"),
            Self::ClusterMismatch => formatter.write_str("peer belongs to a different cluster"),
            Self::InvalidIdentity => PeerIdentityError.fmt(formatter),
            Self::DuplicateIdentity { node_id } => {
                write!(formatter, "node `{node_id}` already joined from a different address")
            }
            Self::StaleEpoch { requested, current } => {
                write!(formatter, "join epoch {} is older than the current {}", requested.0, current.0)
            }
            Self::StaleTerm { requested, current } => {
                write!(formatter, "join term {} is older than the current {}", requested.0, current.0)
            }
            Self::EpochAhead { requested, requested_term, current, current_term } => write!(
                formatter,
                "join (epoch {}, term {}) is ahead of leader (epoch {}, term {})",
                requested.0, requested_term.0, current.0, current_term.0
            ),
            Self::RoleViolation => formatter.write_str("only followers accept a broadcast roster"),
            Self::StaleRoster { epoch, term } => {
                write!(formatter, "roster (epoch {}, term {}) is older than the local view", epoch.0, term.0)
            }
            Self::ConflictingRoster => formatter.write_str("roster at the same epoch and term names a different leader"),
            Self::Corrupt => formatter.write_str("membership view is corrupt"),
        }
    }
}

impl std::error::Error for MembershipError {}

impl MembershipError {
    /// 该失败对应的 join 拒绝码;非 join 路径返回 `None`。
    pub fn reject_code(&self) -> Option<JoinRejectCode> {
        match self {
            Self::NotLeader => Some(JoinRejectCode::NotLeader),
            Self::ClusterMismatch => Some(JoinRejectCode::ClusterMismatch),
            Self::InvalidIdentity => Some(JoinRejectCode::InvalidIdentity),
            Self::DuplicateIdentity { .. } => Some(JoinRejectCode::DuplicateIdentity),
            Self::StaleEpoch { .. } => Some(JoinRejectCode::StaleEpoch),
            Self::StaleTerm { .. } => Some(JoinRejectCode::StaleTerm),
            Self::EpochAhead { .. } => Some(JoinRejectCode::EpochAhead),
            Self::RoleViolation | Self::StaleRoster { .. } | Self::ConflictingRoster | Self::Corrupt => None,
        }
    }
}

/// join 握手与成员视图。
///
/// 同一类型被 leader 与 follower 共用:leader 的 `admit_join` 推进成员表,
/// follower 的 `apply_roster` 只接受不旧于本地的成员表。
#[derive(Debug)]
pub struct Membership {
    identity: PeerIdentity,
    role: MembershipRole,
    view: RwLock<Option<MemberView>>,
    generation: AtomicU64,
}

impl Membership {
    /// 建立 leader 视图,成员表只含自己。
    pub fn leader(identity: PeerIdentity, address: impl Into<String>, epoch: ShardEpoch, term: LeaderTerm) -> Result<Self, MembershipError> {
        let address = address.into();
        if !valid_address(&address) {
            return Err(MembershipError::InvalidIdentity);
        }
        let own = PeerMember { node_id: identity.node_id.clone(), address, epoch, term };
        let view = MemberView { cluster_id: identity.cluster_id.clone(), leader: own.clone(), epoch, term, members: vec![own] };
        Ok(Self { identity, role: MembershipRole::Leader, view: RwLock::new(Some(view)), generation: AtomicU64::new(1) })
    }

    /// 建立 follower 视图,在加入前没有任何成员认知。
    pub fn follower(identity: PeerIdentity) -> Self {
        Self { identity, role: MembershipRole::Follower, view: RwLock::new(None), generation: AtomicU64::new(0) }
    }

    /// 本节点身份。
    pub fn identity(&self) -> &PeerIdentity {
        &self.identity
    }

    /// 本节点角色。
    pub fn role(&self) -> MembershipRole {
        self.role
    }

    /// 是否为 leader。
    pub fn is_leader(&self) -> bool {
        self.role == MembershipRole::Leader
    }

    /// 当前成员视图;follower 加入前为 `None`。
    pub fn view(&self) -> Option<MemberView> {
        self.view.read().ok().and_then(|guard| guard.clone())
    }

    /// 当前成员 epoch 与 term;无视图时返回 `(0, 0)`。
    pub fn epoch_term(&self) -> (ShardEpoch, LeaderTerm) {
        let view = self.view();
        match view {
            Some(view) => (view.epoch, view.term),
            None => (ShardEpoch(0), LeaderTerm(0)),
        }
    }

    /// leader 接纳 join,推进成员表并返回给 joiner 的回复与全员广播。
    pub fn admit_join(&self, request: JoinRequest) -> Result<(JoinAccepted, Roster), JoinRejected> {
        let (current_epoch, current_term) = self.epoch_term();
        let reject = |code: JoinRejectCode, message: String| JoinRejected {
            code,
            message,
            epoch: current_epoch,
            term: current_term,
        };
        if !self.is_leader() {
            return Err(reject(JoinRejectCode::NotLeader, MembershipError::NotLeader.to_string()));
        }
        if request.identity.validate().is_err() {
            return Err(reject(JoinRejectCode::InvalidIdentity, MembershipError::InvalidIdentity.to_string()));
        }
        if request.identity.cluster_id != self.identity.cluster_id {
            return Err(reject(JoinRejectCode::ClusterMismatch, MembershipError::ClusterMismatch.to_string()));
        }
        if !valid_address(&request.address) {
            return Err(reject(JoinRejectCode::InvalidIdentity, "join address is not `host:port`".to_owned()));
        }
        let mut guard = self.view.write().map_err(|_| reject(JoinRejectCode::NotLeader, MembershipError::Corrupt.to_string()))?;
        let Some(view) = guard.as_mut()
        else {
            return Err(reject(JoinRejectCode::NotLeader, MembershipError::Corrupt.to_string()));
        };
        if let Some(existing) = view.member(&request.identity.node_id) {
            if existing.address != request.address {
                return Err(reject(
                    JoinRejectCode::DuplicateIdentity,
                    MembershipError::DuplicateIdentity { node_id: request.identity.node_id.clone() }.to_string(),
                ));
            }
        }
        // `0` 表示首次加入、尚无认知;只有明确的更旧 epoch/term 才拒绝。
        if request.epoch.0 != 0 && request.epoch < view.epoch {
            return Err(reject(
                JoinRejectCode::StaleEpoch,
                MembershipError::StaleEpoch { requested: request.epoch, current: view.epoch }.to_string(),
            ));
        }
        if request.term.0 != 0 && request.term < view.term {
            return Err(reject(
                JoinRejectCode::StaleTerm,
                MembershipError::StaleTerm { requested: request.term, current: view.term }.to_string(),
            ));
        }
        if request.epoch > view.epoch || request.term > view.term {
            return Err(reject(
                JoinRejectCode::EpochAhead,
                MembershipError::EpochAhead {
                    requested: request.epoch,
                    requested_term: request.term,
                    current: view.epoch,
                    current_term: view.term,
                }
                .to_string(),
            ));
        }
        let member = PeerMember {
            node_id: request.identity.node_id,
            address: request.address,
            epoch: view.epoch,
            term: view.term,
        };
        match view.members.iter_mut().find(|existing| existing.node_id == member.node_id) {
            Some(existing) => *existing = member,
            None => view.members.push(member),
        }
        view.members.sort_by(|left, right| left.node_id.cmp(&right.node_id));
        self.generation.fetch_add(1, Ordering::SeqCst);
        let members = view.members.clone();
        let leader = view.leader.clone();
        let epoch = view.epoch;
        let term = view.term;
        Ok((
            JoinAccepted { leader: leader.clone(), epoch, term, members: members.clone() },
            Roster { leader, epoch, term, members },
        ))
    }

    /// follower 应用 leader 广播的成员表。
    pub fn apply_roster(&self, roster: Roster) -> Result<(), MembershipError> {
        if self.is_leader() {
            return Err(MembershipError::RoleViolation);
        }
        if roster.leader.node_id.is_empty() || !valid_address(&roster.leader.address) {
            return Err(MembershipError::InvalidIdentity);
        }
        let mut guard = self.view.write().map_err(|_| MembershipError::Corrupt)?;
        if let Some(current) = guard.as_ref() {
            if roster.epoch < current.epoch || (roster.epoch == current.epoch && roster.term < current.term) {
                return Err(MembershipError::StaleRoster { epoch: roster.epoch, term: roster.term });
            }
            if roster.epoch == current.epoch && roster.term == current.term && roster.leader.node_id != current.leader.node_id {
                return Err(MembershipError::ConflictingRoster);
            }
        }
        let mut members = roster.members;
        members.sort_by(|left, right| left.node_id.cmp(&right.node_id));
        *guard = Some(MemberView { cluster_id: self.identity.cluster_id.clone(), leader: roster.leader, epoch: roster.epoch, term: roster.term, members });
        self.generation.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// leader 提升 epoch / term,并要求严格单调。
    pub fn advance_epoch(&self, epoch: ShardEpoch, term: LeaderTerm) -> Result<MemberView, MembershipError> {
        if !self.is_leader() {
            return Err(MembershipError::NotLeader);
        }
        let mut guard = self.view.write().map_err(|_| MembershipError::Corrupt)?;
        let Some(view) = guard.as_mut()
        else {
            return Err(MembershipError::Corrupt);
        };
        if epoch < view.epoch || (epoch == view.epoch && term <= view.term) {
            return Err(MembershipError::StaleRoster { epoch, term });
        }
        view.epoch = epoch;
        view.term = term;
        view.leader.epoch = epoch;
        view.leader.term = term;
        if let Some(own) = view.members.iter_mut().find(|member| member.node_id == view.leader.node_id) {
            own.epoch = epoch;
            own.term = term;
        }
        self.generation.fetch_add(1, Ordering::SeqCst);
        Ok(view.clone())
    }

    /// 当前成员表代数,仅用于观测。
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}

fn valid_address(address: &str) -> bool {
    address.parse::<SocketAddr>().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(cluster: &str, node: &str) -> PeerIdentity {
        PeerIdentity::new(cluster, node).expect("identity")
    }

    fn join(node: &str, address: &str, epoch: u64, term: u64) -> JoinRequest {
        JoinRequest { identity: identity("cluster-a", node), address: address.to_owned(), epoch: ShardEpoch(epoch), term: LeaderTerm(term) }
    }

    #[test]
    fn a_leader_accepts_a_fresh_member_and_broadcasts_the_roster() {
        let leader = Membership::leader(identity("cluster-a", "node-1"), "127.0.0.1:4101", ShardEpoch(1), LeaderTerm(1)).expect("leader");
        let (accepted, roster) = leader.admit_join(join("node-2", "127.0.0.1:4102", 0, 0)).expect("join");
        assert_eq!(accepted.members.len(), 2);
        assert_eq!(roster.members.len(), 2);
        assert_eq!(leader.view().expect("view").len(), 2);
    }

    #[test]
    fn a_duplicate_node_id_from_another_address_is_refused() {
        let leader = Membership::leader(identity("cluster-a", "node-1"), "127.0.0.1:4101", ShardEpoch(1), LeaderTerm(1)).expect("leader");
        leader.admit_join(join("node-2", "127.0.0.1:4102", 0, 0)).expect("first join");
        let rejected = leader.admit_join(join("node-2", "127.0.0.1:9999", 0, 0)).expect_err("duplicate");
        assert_eq!(rejected.code, JoinRejectCode::DuplicateIdentity);
    }

    #[test]
    fn a_rejoin_from_the_same_address_is_idempotent() {
        let leader = Membership::leader(identity("cluster-a", "node-1"), "127.0.0.1:4101", ShardEpoch(2), LeaderTerm(3)).expect("leader");
        leader.admit_join(join("node-2", "127.0.0.1:4102", 0, 0)).expect("first join");
        leader.admit_join(join("node-2", "127.0.0.1:4102", 2, 3)).expect("rejoin");
        assert_eq!(leader.view().expect("view").len(), 2);
    }

    #[test]
    fn an_older_epoch_or_term_is_refused() {
        let leader = Membership::leader(identity("cluster-a", "node-1"), "127.0.0.1:4101", ShardEpoch(5), LeaderTerm(4)).expect("leader");
        assert_eq!(leader.admit_join(join("node-2", "127.0.0.1:4102", 4, 4)).expect_err("stale epoch").code, JoinRejectCode::StaleEpoch);
        assert_eq!(leader.admit_join(join("node-3", "127.0.0.1:4103", 5, 3)).expect_err("stale term").code, JoinRejectCode::StaleTerm);
        assert_eq!(leader.admit_join(join("node-4", "127.0.0.1:4104", 6, 4)).expect_err("ahead").code, JoinRejectCode::EpochAhead);
    }

    #[test]
    fn a_follower_refuses_a_stale_roster() {
        let follower = Membership::follower(identity("cluster-a", "node-2"));
        let leader = PeerMember { node_id: "node-1".to_owned(), address: "127.0.0.1:4101".to_owned(), epoch: ShardEpoch(2), term: LeaderTerm(2) };
        let roster = |epoch: u64, term: u64| Roster { leader: leader.clone(), epoch: ShardEpoch(epoch), term: LeaderTerm(term), members: vec![leader.clone()] };
        follower.apply_roster(roster(2, 2)).expect("fresh roster");
        assert_eq!(follower.apply_roster(roster(1, 2)).expect_err("stale"), MembershipError::StaleRoster { epoch: ShardEpoch(1), term: LeaderTerm(2) });
        assert_eq!(follower.view().expect("view").epoch, ShardEpoch(2));
    }

    #[test]
    fn a_leader_refuses_an_external_roster() {
        let leader = Membership::leader(identity("cluster-a", "node-1"), "127.0.0.1:4101", ShardEpoch(1), LeaderTerm(1)).expect("leader");
        let roster = Roster {
            leader: PeerMember { node_id: "node-9".to_owned(), address: "127.0.0.1:4109".to_owned(), epoch: ShardEpoch(1), term: LeaderTerm(1) },
            epoch: ShardEpoch(1),
            term: LeaderTerm(1),
            members: Vec::new(),
        };
        assert_eq!(leader.apply_roster(roster).expect_err("role"), MembershipError::RoleViolation);
    }
}
