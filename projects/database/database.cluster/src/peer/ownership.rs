//! epoch / owner 控制面与 fencing 准入。
//!
//! leader 以 `(epoch, term)` 单调推进 owner 表;任何节点在接收写之前先用本模块
//! 校验 `FenceToken`。旧 owner 在新 epoch/term 发布后即使仍能处理连接,也会在
//! 准入处得到 [`database_types::cluster::Error::FencingRejected`],不会写分片。

use std::{
    collections::BTreeMap,
    fmt,
    sync::RwLock,
};

use database_types::cluster::{Error, FenceToken, LeaderTerm, OwnerEntry, ReplicaNodeId, Result as ClusterResult, ShardEpoch, ShardId};

use super::frame::{EpochAnnouncement, OwnerSwitch};

/// owner 表控制面失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnershipError {
    /// 通告不比本地新;拒绝以免回退到旧 owner。
    StaleEpoch {
        /// 通告的 epoch。
        epoch: ShardEpoch,
        /// 通告的 term。
        term: LeaderTerm,
        /// 本地 epoch。
        current_epoch: ShardEpoch,
        /// 本地 term。
        current_term: LeaderTerm,
    },
    /// 切换声明的旧 primary 与当前 owner 不符,可能来自并发切换。
    NotOwner {
        /// 分片 id。
        shard_id: String,
        /// 切换声明的旧 primary。
        expected: String,
        /// 当前实际 owner。
        current: String,
    },
    /// 切换的分片当前没有 owner。
    OwnerNotFound {
        /// 分片 id。
        shard_id: String,
    },
    /// 通告内部自相矛盾,例如同一分片出现两条 owner。
    DuplicateOwner {
        /// 重复的分片 id。
        shard_id: String,
    },
    /// 状态锁中毒。
    Corrupt,
}

impl fmt::Display for OwnershipError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleEpoch { epoch, term, current_epoch, current_term } => write!(
                formatter,
                "owner announcement (epoch {}, term {}) is not newer than local (epoch {}, term {})",
                epoch.0, term.0, current_epoch.0, current_term.0
            ),
            Self::NotOwner { shard_id, expected, current } => {
                write!(formatter, "owner switch for `{shard_id}` expects `{expected}` but the current owner is `{current}`")
            }
            Self::OwnerNotFound { shard_id } => write!(formatter, "shard `{shard_id}` has no published owner"),
            Self::DuplicateOwner { shard_id } => write!(formatter, "owner announcement repeats shard `{shard_id}`"),
            Self::Corrupt => formatter.write_str("ownership state is corrupt"),
        }
    }
}

impl std::error::Error for OwnershipError {}

impl OwnershipError {
    /// 稳定诊断码。
    pub fn code(&self) -> &'static str {
        match self {
            Self::StaleEpoch { .. } => "database.cluster.stale_epoch",
            Self::NotOwner { .. } => "database.cluster.owner_conflict",
            Self::OwnerNotFound { .. } => "database.cluster.owner_missing",
            Self::DuplicateOwner { .. } => "database.cluster.owner_duplicate",
            Self::Corrupt => "database.cluster.corrupt",
        }
    }
}

#[derive(Debug, Clone)]
struct OwnershipState {
    epoch: ShardEpoch,
    term: LeaderTerm,
    owners: BTreeMap<String, OwnerEntry>,
}

/// 节点本地的 owner 真值快照。
#[derive(Debug)]
pub struct Ownership {
    state: RwLock<OwnershipState>,
}

impl Ownership {
    /// 用已发布的 epoch / term / owner 表建立快照。
    pub fn new(epoch: ShardEpoch, term: LeaderTerm, owners: Vec<OwnerEntry>) -> Self {
        let mut table = BTreeMap::new();
        for owner in owners {
            table.insert(owner.shard_id.0.clone(), owner);
        }
        Self { state: RwLock::new(OwnershipState { epoch, term, owners: table }) }
    }

    /// 当前路由 epoch。
    pub fn epoch(&self) -> ShardEpoch {
        self.state.read().map(|state| state.epoch).unwrap_or(ShardEpoch(0))
    }

    /// 当前 leader term。
    pub fn term(&self) -> LeaderTerm {
        self.state.read().map(|state| state.term).unwrap_or(LeaderTerm(0))
    }

    /// 当前 owner 表快照,按 shard id 稳定排序。
    pub fn owners(&self) -> Vec<OwnerEntry> {
        self.state
            .read()
            .map(|state| state.owners.values().cloned().collect())
            .unwrap_or_default()
    }

    /// 某分片当前 owner。
    pub fn owner(&self, shard_id: &ShardId) -> Option<OwnerEntry> {
        self.state.read().ok().and_then(|state| state.owners.get(&shard_id.0).cloned())
    }

    /// 某分片在当前 epoch 下的 fence token。
    pub fn fence(&self, shard_id: &ShardId) -> Option<FenceToken> {
        let state = self.state.read().ok()?;
        let owner = state.owners.get(&shard_id.0)?;
        Some(FenceToken { epoch: state.epoch, term: owner.leader_term, leader_node_id: owner.node_id.clone() })
    }

    /// 应用 leader 的 epoch 通告。
    ///
    /// 要求 `(epoch, term)` 严格新于本地,否则拒绝;这保证旧 owner 不会因为
    /// 迟到的通告回退。
    pub fn publish(&self, announcement: EpochAnnouncement) -> Result<(), OwnershipError> {
        let mut guard = self.state.write().map_err(|_| OwnershipError::Corrupt)?;
        if announcement.epoch < guard.epoch
            || (announcement.epoch == guard.epoch && announcement.term <= guard.term)
        {
            return Err(OwnershipError::StaleEpoch {
                epoch: announcement.epoch,
                term: announcement.term,
                current_epoch: guard.epoch,
                current_term: guard.term,
            });
        }
        let mut owners = BTreeMap::new();
        for owner in announcement.owners {
            let shard_id = owner.shard_id.0.clone();
            if owners.insert(shard_id.clone(), owner).is_some() {
                return Err(OwnershipError::DuplicateOwner { shard_id });
            }
        }
        guard.epoch = announcement.epoch;
        guard.term = announcement.term;
        guard.owners = owners;
        Ok(())
    }

    /// 应用单分片 owner 切换,并要求 `(epoch, term)` 严格新于本地。
    pub fn promote(&self, switch: OwnerSwitch) -> Result<FenceToken, OwnershipError> {
        let mut guard = self.state.write().map_err(|_| OwnershipError::Corrupt)?;
        if switch.epoch < guard.epoch || (switch.epoch == guard.epoch && switch.term <= guard.term) {
            return Err(OwnershipError::StaleEpoch {
                epoch: switch.epoch,
                term: switch.term,
                current_epoch: guard.epoch,
                current_term: guard.term,
            });
        }
        let shard_id = ShardId(switch.shard_id.clone());
        match guard.owners.get(&switch.shard_id) {
            None => return Err(OwnershipError::OwnerNotFound { shard_id: switch.shard_id }),
            Some(current) if current.node_id.0 != switch.old_primary => {
                return Err(OwnershipError::NotOwner {
                    shard_id: switch.shard_id,
                    expected: switch.old_primary,
                    current: current.node_id.0.clone(),
                })
            }
            Some(_) => {}
        }
        guard.epoch = switch.epoch;
        guard.term = switch.term;
        guard.owners.insert(
            switch.shard_id.clone(),
            OwnerEntry { shard_id: shard_id.clone(), node_id: ReplicaNodeId(switch.new_primary), leader_term: switch.term },
        );
        Ok(FenceToken {
            epoch: switch.epoch,
            term: switch.term,
            leader_node_id: ReplicaNodeId(guard.owners.get(&switch.shard_id).expect("inserted owner").node_id.0.clone()),
        })
    }

    /// 写准入:token 的 epoch、term 与 leader 必须与本节点发布的 owner 完全一致。
    ///
    /// 失败返回 [`Error::FencingRejected`],其 `code()` 为
    /// `database.shard.fencing.rejected`,调用方可据此区分旧 owner 写。
    pub fn admit(&self, shard_id: &ShardId, fence: &FenceToken) -> ClusterResult<()> {
        let state = self.state.read().map_err(|_| Error::Corrupt("ownership state lock poisoned"))?;
        let Some(owner) = state.owners.get(&shard_id.0) else {
            return Err(Error::FencingRejected {
                fence: fence.clone(),
                current: OwnerEntry {
                    shard_id: shard_id.clone(),
                    node_id: ReplicaNodeId("<unpublished>".to_owned()),
                    leader_term: LeaderTerm(0),
                },
            });
        };
        if fence.epoch != state.epoch || fence.term != owner.leader_term || fence.leader_node_id != owner.node_id {
            return Err(Error::FencingRejected { fence: fence.clone(), current: owner.clone() });
        }
        Ok(())
    }
}

impl Default for Ownership {
    fn default() -> Self {
        Self::new(ShardEpoch(0), LeaderTerm(0), Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(shard: &str, node: &str, term: u64) -> OwnerEntry {
        OwnerEntry { shard_id: ShardId(shard.to_owned()), node_id: ReplicaNodeId(node.to_owned()), leader_term: LeaderTerm(term) }
    }

    #[test]
    fn an_old_owner_is_fenced_after_a_promotion() {
        let ownership = Ownership::new(ShardEpoch(1), LeaderTerm(1), vec![owner("shard-a", "node-1", 1)]);
        let old = ownership.fence(&ShardId("shard-a".to_owned())).expect("fence");
        ownership
            .promote(OwnerSwitch {
                epoch: ShardEpoch(2),
                term: LeaderTerm(2),
                shard_id: "shard-a".to_owned(),
                old_primary: "node-1".to_owned(),
                new_primary: "node-2".to_owned(),
            })
            .expect("promote");
        let error = ownership.admit(&ShardId("shard-a".to_owned()), &old).expect_err("old fence must be rejected");
        assert_eq!(error.code(), "database.shard.fencing.rejected");
        let fresh = ownership.fence(&ShardId("shard-a".to_owned())).expect("fresh fence");
        ownership.admit(&ShardId("shard-a".to_owned()), &fresh).expect("new owner writes");
    }

    #[test]
    fn an_equal_or_older_epoch_never_replaces_the_current_owner() {
        let ownership = Ownership::new(ShardEpoch(3), LeaderTerm(3), vec![owner("shard-a", "node-1", 3)]);
        let error = ownership
            .publish(EpochAnnouncement { epoch: ShardEpoch(3), term: LeaderTerm(3), owners: vec![owner("shard-a", "node-2", 3)] })
            .expect_err("equal epoch must be refused");
        assert_eq!(error.code(), "database.cluster.stale_epoch");
        assert_eq!(ownership.owner(&ShardId("shard-a".to_owned())).expect("owner").node_id.0, "node-1");
    }

    #[test]
    fn a_switch_from_the_wrong_old_primary_is_refused() {
        let ownership = Ownership::new(ShardEpoch(1), LeaderTerm(1), vec![owner("shard-a", "node-1", 1)]);
        let error = ownership
            .promote(OwnerSwitch {
                epoch: ShardEpoch(2),
                term: LeaderTerm(2),
                shard_id: "shard-a".to_owned(),
                old_primary: "node-9".to_owned(),
                new_primary: "node-2".to_owned(),
            })
            .expect_err("unknown old primary");
        assert_eq!(error.code(), "database.cluster.owner_conflict");
    }

    #[test]
    fn an_unpublished_shard_fails_closed() {
        let ownership = Ownership::default();
        let fence = FenceToken { epoch: ShardEpoch(1), term: LeaderTerm(1), leader_node_id: ReplicaNodeId("node-1".to_owned()) };
        let error = ownership.admit(&ShardId("shard-a".to_owned()), &fence).expect_err("no owner");
        assert_eq!(error.code(), "database.shard.fencing.rejected");
    }

    #[test]
    fn an_epoch_that_matches_no_owner_token_is_rejected() {
        let ownership = Ownership::new(ShardEpoch(2), LeaderTerm(2), vec![owner("shard-a", "node-1", 2)]);
        let stale_epoch = FenceToken { epoch: ShardEpoch(1), term: LeaderTerm(2), leader_node_id: ReplicaNodeId("node-1".to_owned()) };
        assert_eq!(
            ownership.admit(&ShardId("shard-a".to_owned()), &stale_epoch).expect_err("stale epoch").code(),
            "database.shard.fencing.rejected"
        );
    }
}
