//! `YYNF` 节点帧布局与消息体。
//!
//! 帧头固定为 `magic(4) + version(1) + len(4)`;body 是带 `frame` 标签的 JSON,
//! 这样控制面消息可以在不重编译对端的情况下扩展字段,而长度上限仍由帧头强制。
//! KV 命令与结果不进入本层:peer 通道只承载成员、epoch 与 owner 事实。

use std::{
    fmt,
    io::{self, Read, Write},
};

use database_types::cluster::{LeaderTerm, OwnerEntry, ShardEpoch};
use serde::{Deserialize, Serialize};

/// 节点帧 magic。
pub const PEER_MAGIC: &[u8; 4] = b"YYNF";

/// 当前节点帧版本。
pub const PEER_VERSION: u8 = 1;

/// 单帧 body 软上限:控制面消息远小于此值,超出即拒绝,不缓冲无界输入。
pub const MAX_PEER_BODY: usize = 256 * 1024;

/// 帧头长度:`magic` + `version` + `len`。
pub const PEER_HEADER_LEN: usize = 9;

/// 节点身份:集群 id 与节点 id。
///
/// 与 [`crate::control::NodeIdentity`] 同义,但本类型需要 serde 才能在 peer 帧
/// 中携带;两者的字符校验规则保持一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerIdentity {
    /// 集群 id。
    pub cluster_id: String,
    /// 节点 id。
    pub node_id: String,
}

impl PeerIdentity {
    /// 校验并构造节点身份。
    pub fn new(cluster_id: impl Into<String>, node_id: impl Into<String>) -> Result<Self, PeerIdentityError> {
        let identity = Self { cluster_id: cluster_id.into(), node_id: node_id.into() };
        identity.validate()?;
        Ok(identity)
    }

    /// 字符集与长度校验:ASCII 字母、数字、`_`、`-`,每段不超过 128 字节。
    pub fn validate(&self) -> Result<(), PeerIdentityError> {
        for identifier in [&self.cluster_id, &self.node_id] {
            if identifier.is_empty()
                || identifier.len() > 128
                || !identifier.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
            {
                return Err(PeerIdentityError);
            }
        }
        Ok(())
    }
}

/// 节点身份不合法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerIdentityError;

impl fmt::Display for PeerIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("peer identity must be 1..=128 ASCII alphanumeric, `_` or `-` characters")
    }
}

impl std::error::Error for PeerIdentityError {}

/// 成员视图中的一条成员记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerMember {
    /// 节点 id。
    pub node_id: String,
    /// 该节点接收 peer 连接的地址(`host:port`)。
    pub address: String,
    /// 接纳该成员时使用的路由 epoch。
    pub epoch: ShardEpoch,
    /// 接纳该成员时使用的 leader term。
    pub term: LeaderTerm,
}

/// follower 发起的 join 请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRequest {
    /// joiner 身份。
    pub identity: PeerIdentity,
    /// joiner 的监听地址。
    pub address: String,
    /// joiner 已知的成员 epoch;`0` 表示首次加入、尚无认知。
    pub epoch: ShardEpoch,
    /// joiner 已知的 leader term;`0` 表示首次加入、尚无认知。
    pub term: LeaderTerm,
}

/// leader 拒绝 join 的机器可读原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinRejectCode {
    /// 集群 id 不一致。
    ClusterMismatch,
    /// 节点 id 或地址不合法。
    InvalidIdentity,
    /// 同一 node id 已从另一个地址加入。
    DuplicateIdentity,
    /// 请求 epoch 早于 leader 已知 epoch。
    StaleEpoch,
    /// 请求 term 早于 leader 已知 term。
    StaleTerm,
    /// 请求 epoch 或 term 领先于 leader;joiner 必须改投新 leader。
    EpochAhead,
    /// 目标节点不是 leader。
    NotLeader,
}

impl JoinRejectCode {
    /// 稳定的诊断码。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClusterMismatch => "cluster_mismatch",
            Self::InvalidIdentity => "invalid_identity",
            Self::DuplicateIdentity => "duplicate_identity",
            Self::StaleEpoch => "stale_epoch",
            Self::StaleTerm => "stale_term",
            Self::EpochAhead => "epoch_ahead",
            Self::NotLeader => "not_leader",
        }
    }
}

/// leader 拒绝 join 的完整回复。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinRejected {
    /// 机器可读原因。
    pub code: JoinRejectCode,
    /// 人类可读说明。
    pub message: String,
    /// leader 当前的成员 epoch。
    pub epoch: ShardEpoch,
    /// leader 当前的 leader term。
    pub term: LeaderTerm,
}

/// leader 接纳 join 的回复,同时携带当前成员视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinAccepted {
    /// 当前 leader。
    pub leader: PeerMember,
    /// 成员 epoch。
    pub epoch: ShardEpoch,
    /// leader term。
    pub term: LeaderTerm,
    /// 成员列表(包含 leader 与本次 joiner)。
    pub members: Vec<PeerMember>,
}

/// leader 向所有成员广播的成员表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roster {
    /// 当前 leader。
    pub leader: PeerMember,
    /// 成员 epoch。
    pub epoch: ShardEpoch,
    /// leader term。
    pub term: LeaderTerm,
    /// 完整成员列表。
    pub members: Vec<PeerMember>,
}

/// leader 发布的 epoch 通告:路由 epoch、term 与完整 owner 表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpochAnnouncement {
    /// 新路由 epoch。
    pub epoch: ShardEpoch,
    /// 新 leader term。
    pub term: LeaderTerm,
    /// 该 epoch 下的完整 owner 表。
    pub owners: Vec<OwnerEntry>,
}

/// leader 发布的单分片 owner 切换。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerSwitch {
    /// 新路由 epoch。
    pub epoch: ShardEpoch,
    /// 新 leader term。
    pub term: LeaderTerm,
    /// 被切换的分片 id。
    pub shard_id: String,
    /// 期望的旧 primary。
    pub old_primary: String,
    /// 新 primary。
    pub new_primary: String,
}

/// 存活探测。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// 发送方已知的 epoch。
    pub epoch: ShardEpoch,
    /// 发送方已知的 term。
    pub term: LeaderTerm,
}

/// 一条由 leader 分配的分片记录变更。
///
/// `value` 为 `Some` 是写入后的内联值,`None` 表示删除该键。`revision` 由 leader
/// 在写入时分配,成员逐字采用,保证同一键在所有副本上的序号一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaRecord {
    /// 记录所属命名空间。
    pub namespace: String,
    /// 不透明键字节。
    pub key: Vec<u8>,
    /// 写入值;`None` 表示删除。
    pub value: Option<Vec<u8>>,
    /// 可选的过期截止时间(Unix 毫秒)。
    pub expires_at: Option<i64>,
    /// leader 分配的记录修订号。
    pub revision: u64,
}

impl ReplicaRecord {
    /// 该变更是否为删除。
    pub fn is_delete(&self) -> bool {
        self.value.is_none()
    }
}

/// leader → member:一条带 epoch / term 的分片复制写。
///
/// `sequence` 是 leader 在 `(shard, epoch, term)` 内单调分配的复制序号;成员据此
/// 拒绝重放并发现缺口。接收端在 `ownership` 准入失败时整帧拒绝,不会写分片。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardReplica {
    /// 路由 epoch。
    pub epoch: ShardEpoch,
    /// leader term。
    pub term: LeaderTerm,
    /// 授权该写的 leader 节点 id。
    pub leader_node_id: String,
    /// 目标分片 id。
    pub shard_id: String,
    /// leader 分配的复制序号。
    pub sequence: u64,
    /// 记录变更。
    pub record: ReplicaRecord,
}

/// member → leader:复制写的应用确认。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaAck {
    /// 对端是否应用(或幂等识别为重放)。
    pub accepted: bool,
    /// 回复方当前的 epoch。
    pub epoch: ShardEpoch,
    /// 回复方当前的 term。
    pub term: LeaderTerm,
    /// 分片 id。
    pub shard_id: String,
    /// 被确认的复制序号。
    pub sequence: u64,
    /// 接收端持有的记录修订号。
    pub revision: u64,
    /// 拒绝原因或诊断说明。
    pub message: String,
}

/// 控制帧的通用确认。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerAck {
    /// 对端是否接受该帧。
    pub accepted: bool,
    /// 回复方当前的 epoch。
    pub epoch: ShardEpoch,
    /// 回复方当前的 term。
    pub term: LeaderTerm,
    /// 拒绝原因或诊断说明。
    pub message: String,
}

/// 一条 peer 控制帧。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum PeerFrame {
    /// follower → leader:加入集群。
    Join(JoinRequest),
    /// leader → follower:接纳。
    JoinAccepted(JoinAccepted),
    /// leader → follower:拒绝。
    JoinRejected(JoinRejected),
    /// leader → member:成员表广播。
    Roster(Roster),
    /// leader → member:epoch 与 owner 表通告。
    EpochAnnouncement(EpochAnnouncement),
    /// leader → member:单分片 owner 切换。
    OwnerSwitch(OwnerSwitch),
    /// leader → member:分片复制写。
    ReplicaWrite(ShardReplica),
    /// member → leader:复制写确认。
    ReplicaAck(ReplicaAck),
    /// 任意 → 任意:存活探测。
    Heartbeat(Heartbeat),
    /// 任意 → 任意:确认。
    Ack(PeerAck),
}

/// 帧编解码失败。
#[derive(Debug)]
pub enum PeerFrameError {
    /// 底层读写失败。
    Io(io::Error),
    /// magic 不是 `YYNF`。
    BadMagic,
    /// 版本不受支持。
    UnsupportedVersion(u8),
    /// body 长度超过上限。
    Oversized {
        /// 帧头声明的长度。
        declared: usize,
        /// 允许的上限。
        max: usize,
    },
    /// JSON body 形态不符合当前消息集。
    Malformed(String),
}

impl fmt::Display for PeerFrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "peer frame I/O error: {error}"),
            Self::BadMagic => formatter.write_str("peer frame magic is not YYNF"),
            Self::UnsupportedVersion(version) => write!(formatter, "unsupported peer frame version {version}"),
            Self::Oversized { declared, max } => write!(formatter, "peer frame body is {declared} bytes, limit is {max}"),
            Self::Malformed(detail) => write!(formatter, "malformed peer frame: {detail}"),
        }
    }
}

impl std::error::Error for PeerFrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for PeerFrameError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// 编码一帧(含帧头)。
pub fn encode_frame(frame: &PeerFrame) -> Result<Vec<u8>, PeerFrameError> {
    let body = serde_json::to_vec(frame).map_err(|error| PeerFrameError::Malformed(error.to_string()))?;
    if body.len() > MAX_PEER_BODY {
        return Err(PeerFrameError::Oversized { declared: body.len(), max: MAX_PEER_BODY });
    }
    let mut bytes = Vec::with_capacity(PEER_HEADER_LEN + body.len());
    bytes.extend_from_slice(PEER_MAGIC);
    bytes.push(PEER_VERSION);
    bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&body);
    Ok(bytes)
}

/// 解码一帧(含帧头),并拒绝多余尾随字节。
pub fn decode_frame(bytes: &[u8]) -> Result<PeerFrame, PeerFrameError> {
    if bytes.len() < PEER_HEADER_LEN {
        return Err(PeerFrameError::Malformed("frame is shorter than its header".to_owned()));
    }
    if &bytes[..4] != PEER_MAGIC {
        return Err(PeerFrameError::BadMagic);
    }
    let version = bytes[4];
    if version != PEER_VERSION {
        return Err(PeerFrameError::UnsupportedVersion(version));
    }
    let declared = u32::from_le_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) as usize;
    if declared > MAX_PEER_BODY {
        return Err(PeerFrameError::Oversized { declared, max: MAX_PEER_BODY });
    }
    let body = &bytes[PEER_HEADER_LEN..];
    if body.len() != declared {
        return Err(PeerFrameError::Malformed(format!(
            "frame header declares {declared} body bytes but carries {}",
            body.len()
        )));
    }
    serde_json::from_slice(body).map_err(|error| PeerFrameError::Malformed(error.to_string()))
}

/// 同步写入一帧。
pub fn write_frame<W: Write>(writer: &mut W, frame: &PeerFrame) -> Result<(), PeerFrameError> {
    writer.write_all(&encode_frame(frame)?)?;
    writer.flush()?;
    Ok(())
}

/// 同步读取一帧。
pub fn read_frame<R: Read>(reader: &mut R) -> Result<PeerFrame, PeerFrameError> {
    let mut header = [0u8; PEER_HEADER_LEN];
    reader.read_exact(&mut header)?;
    if &header[..4] != PEER_MAGIC {
        return Err(PeerFrameError::BadMagic);
    }
    if header[4] != PEER_VERSION {
        return Err(PeerFrameError::UnsupportedVersion(header[4]));
    }
    let declared = u32::from_le_bytes([header[5], header[6], header[7], header[8]]) as usize;
    if declared > MAX_PEER_BODY {
        return Err(PeerFrameError::Oversized { declared, max: MAX_PEER_BODY });
    }
    let mut body = vec![0u8; declared];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|error| PeerFrameError::Malformed(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_frame() -> PeerFrame {
        PeerFrame::Join(JoinRequest {
            identity: PeerIdentity::new("cluster-a", "node-1").expect("identity"),
            address: "127.0.0.1:4101".to_owned(),
            epoch: ShardEpoch(0),
            term: LeaderTerm(0),
        })
    }

    #[test]
    fn a_frame_round_trips_through_the_wire_layout() {
        let frame = sample_frame();
        let bytes = encode_frame(&frame).expect("encode");
        assert_eq!(&bytes[..4], PEER_MAGIC);
        assert_eq!(bytes[4], PEER_VERSION);
        assert_eq!(decode_frame(&bytes).expect("decode"), frame);
    }

    #[test]
    fn a_foreign_magic_is_refused() {
        let mut bytes = encode_frame(&sample_frame()).expect("encode");
        bytes[0] = b'X';
        assert!(matches!(decode_frame(&bytes), Err(PeerFrameError::BadMagic)));
    }

    #[test]
    fn a_declared_length_above_the_cap_is_refused_before_buffering() {
        let mut bytes = encode_frame(&sample_frame()).expect("encode");
        bytes[5..9].copy_from_slice(&((MAX_PEER_BODY + 1) as u32).to_le_bytes());
        assert!(matches!(decode_frame(&bytes), Err(PeerFrameError::Oversized { .. })));
    }

    #[test]
    fn a_body_shorter_than_its_header_is_refused() {
        let mut bytes = encode_frame(&sample_frame()).expect("encode");
        bytes.pop();
        assert!(matches!(decode_frame(&bytes), Err(PeerFrameError::Malformed(_))));
    }

    #[test]
    fn identities_enforce_the_shared_character_set() {
        assert!(PeerIdentity::new("cluster-a", "node_1").is_ok());
        assert!(PeerIdentity::new("", "node-1").is_err());
        assert!(PeerIdentity::new("cluster a", "node-1").is_err());
    }
}
