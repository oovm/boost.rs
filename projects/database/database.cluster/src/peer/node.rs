//! 监听端与连接端:真实 TCP peer 通道。
//!
//! 每个节点只监听一个地址,连接按帧一问一答;leader 在接受 join 后向所有成员
//! 重新广播成员表,epoch 与 owner 切换同样以广播推进。分片写以 [`ShardReplica`]
//! 帧复制:leader 本地应用后广播,成员按 `(epoch, term)` 准入、应用并回报修订号。
//! 启用 TLS 时握手在任何帧之前完成,失败即断开,不退回明文。

use std::{
    collections::BTreeMap,
    fmt,
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, RwLock,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use database_types::cluster::{Error as ClusterError, FenceToken, LeaderTerm, ReplicaNodeId, ShardEpoch, ShardId};
use rustls::{ClientConnection, ServerConnection, StreamOwned};

use super::{
    frame::{
        read_frame, write_frame, EpochAnnouncement, Heartbeat, JoinRejected, JoinRequest, OwnerSwitch, PeerAck, PeerFrame,
        PeerFrameError, PeerIdentity, PeerMember, ReplicaAck, ReplicaRecord, Roster, ShardReplica,
    },
    membership::{MemberView, Membership, MembershipError},
    ownership::{Ownership, OwnershipError},
    replica::{ReplicaError, ReplicaSink},
    tls::{PeerTls, PeerTlsError},
};

/// 默认连接与读写超时。
pub const DEFAULT_PEER_TIMEOUT: Duration = Duration::from_secs(5);

/// 节点在成员协议中的初始角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRole {
    /// 引导节点:接纳 join 并发布 epoch / owner。
    Leader {
        /// 初始路由 epoch。
        epoch: ShardEpoch,
        /// 初始 leader term。
        term: LeaderTerm,
    },
    /// 只加入现有 leader 的节点。
    Follower,
}

/// 启动一个 peer 监听端所需的选项。
#[derive(Debug, Clone)]
pub struct PeerOptions {
    /// 本节点身份。
    pub identity: PeerIdentity,
    /// 监听地址;端口 `0` 由操作系统分配。
    pub bind: SocketAddr,
    /// 初始角色。
    pub role: PeerRole,
    /// 可选的 mTLS 材料。
    pub tls: Option<PeerTls>,
    /// 连接与读写超时。
    pub timeout: Duration,
}

impl PeerOptions {
    /// 以默认超时建立选项。
    pub fn new(identity: PeerIdentity, bind: SocketAddr, role: PeerRole, tls: Option<PeerTls>) -> Self {
        Self { identity, bind, role, tls, timeout: DEFAULT_PEER_TIMEOUT }
    }

    /// 引导节点选项。
    pub fn leader(identity: PeerIdentity, bind: SocketAddr, epoch: ShardEpoch, term: LeaderTerm, tls: Option<PeerTls>) -> Self {
        Self::new(identity, bind, PeerRole::Leader { epoch, term }, tls)
    }

    /// follower 选项。
    pub fn follower(identity: PeerIdentity, bind: SocketAddr, tls: Option<PeerTls>) -> Self {
        Self::new(identity, bind, PeerRole::Follower, tls)
    }
}

/// peer 通道失败。
#[derive(Debug)]
pub enum PeerError {
    /// 底层网络错误。
    Io(io::Error),
    /// 帧布局或消息体错误。
    Frame(PeerFrameError),
    /// 成员协议拒绝。
    Membership(MembershipError),
    /// owner / epoch 控制面拒绝。
    Ownership(OwnershipError),
    /// 复制数据面失败。
    Replica(ReplicaError),
    /// 本节点的 fence 已被更新的 owner 取代。
    Fenced(ClusterError),
    /// TLS 材料或握手失败。
    Tls(PeerTlsError),
    /// join 被 leader 显式拒绝。
    Rejected(JoinRejected),
    /// 本节点已被移出成员表,不再接受写。
    Revoked,
    /// 本节点尚未加入任何成员视图。
    NotJoined,
    /// 只有 leader 可以推进成员或 owner。
    NotLeader,
    /// 对端回复了与请求不匹配的帧。
    Protocol(String),
}

impl fmt::Display for PeerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "peer I/O error: {error}"),
            Self::Frame(error) => error.fmt(formatter),
            Self::Membership(error) => error.fmt(formatter),
            Self::Ownership(error) => error.fmt(formatter),
            Self::Replica(error) => error.fmt(formatter),
            Self::Fenced(error) => error.fmt(formatter),
            Self::Tls(error) => error.fmt(formatter),
            Self::Rejected(rejected) => write!(formatter, "join rejected ({}): {}", rejected.code.as_str(), rejected.message),
            Self::Revoked => formatter.write_str("node was removed from the cluster roster"),
            Self::NotJoined => formatter.write_str("node has not joined a cluster view"),
            Self::NotLeader => formatter.write_str("only the membership leader may advance the cluster"),
            Self::Protocol(detail) => write!(formatter, "peer protocol violation: {detail}"),
        }
    }
}

impl std::error::Error for PeerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Frame(error) => Some(error),
            Self::Membership(error) => Some(error),
            Self::Ownership(error) => Some(error),
            Self::Replica(error) => Some(error),
            Self::Fenced(error) => Some(error),
            Self::Tls(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ClusterError> for PeerError {
    fn from(error: ClusterError) -> Self {
        Self::Fenced(error)
    }
}

impl From<io::Error> for PeerError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<PeerFrameError> for PeerError {
    fn from(error: PeerFrameError) -> Self {
        match error {
            // 对端在传输层断开时,调用方应看到传输错误而不是帧格式错误。
            PeerFrameError::Io(error) => Self::Io(error),
            other => Self::Frame(other),
        }
    }
}

impl From<PeerTlsError> for PeerError {
    fn from(error: PeerTlsError) -> Self {
        Self::Tls(error)
    }
}

impl PeerError {
    /// 稳定诊断码;调用方用它区分传输、TLS 与协议拒绝。
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io(_) => "database.cluster.peer.io",
            Self::Frame(_) => "database.cluster.peer.frame",
            Self::Membership(_) => "database.cluster.peer.membership",
            Self::Ownership(_) => "database.cluster.peer.ownership",
            Self::Replica(_) => "database.cluster.peer.replica",
            Self::Fenced(error) => error.code(),
            Self::Tls(_) => "database.cluster.peer.tls",
            Self::Rejected(_) => "database.cluster.peer.join_rejected",
            Self::Revoked => "database.cluster.peer.revoked",
            Self::NotJoined => "database.cluster.peer.not_joined",
            Self::NotLeader => "database.cluster.peer.not_leader",
            Self::Protocol(_) => "database.cluster.peer.protocol",
        }
    }
}

/// 一条广播的投递结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BroadcastFailure {
    /// 目标节点 id。
    pub node_id: String,
    /// 失败说明。
    pub message: String,
}

/// 一次成员/epoch 广播的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BroadcastReport {
    /// 已确认的节点 id。
    pub delivered: Vec<String>,
    /// 未确认的节点及原因。
    pub failed: Vec<BroadcastFailure>,
}

impl BroadcastReport {
    /// 成功投递数量。
    pub fn delivered_count(&self) -> usize {
        self.delivered.len()
    }

    /// 失败数量。
    pub fn failed_count(&self) -> usize {
        self.failed.len()
    }
}

/// 一条复制写在一个成员上的应用结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaDelivery {
    /// 应用该写的节点 id。
    pub node_id: String,
    /// 该节点持有的记录修订号。
    pub revision: u64,
}

/// 一次复制广播的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplicaReport {
    /// leader 分配的复制序号。
    pub sequence: u64,
    /// 已确认应用的成员。
    pub delivered: Vec<ReplicaDelivery>,
    /// 未确认的成员及原因。
    pub failed: Vec<BroadcastFailure>,
}

impl ReplicaReport {
    /// 成功应用数量。
    pub fn delivered_count(&self) -> usize {
        self.delivered.len()
    }

    /// 失败数量。
    pub fn failed_count(&self) -> usize {
        self.failed.len()
    }
}

/// 由 epoch / term 限定的复制水位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReplicaWatermark {
    epoch: ShardEpoch,
    term: LeaderTerm,
    sequence: u64,
}

/// 运行中的 peer 监听端。
#[derive(Debug)]
pub struct PeerNode {
    inner: Arc<Inner>,
    address: SocketAddr,
    thread: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct Inner {
    options: PeerOptions,
    membership: Membership,
    ownership: Arc<Ownership>,
    /// 本节点的复制落点;未附着时只承载控制面。
    replica: RwLock<Option<Arc<dyn ReplicaSink>>>,
    /// 每个分片已应用的最高复制序号,按 `(epoch, term)` 重置。
    watermarks: Mutex<BTreeMap<String, ReplicaWatermark>>,
    /// 每个 `(shard, epoch, term)` 已分配的复制序号。
    sequences: Mutex<BTreeMap<(String, u64, u64), u64>>,
    /// 串行化 leader 复制写,保证序号连续。
    replication: Mutex<()>,
    shutdown: AtomicBool,
}

struct PeerStream {
    kind: StreamKind,
}

enum StreamKind {
    Plain(TcpStream),
    Server(Box<StreamOwned<ServerConnection, TcpStream>>),
    Client(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Read for PeerStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match &mut self.kind {
            StreamKind::Plain(stream) => stream.read(buffer),
            StreamKind::Server(stream) => stream.read(buffer),
            StreamKind::Client(stream) => stream.read(buffer),
        }
    }
}

impl Write for PeerStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match &mut self.kind {
            StreamKind::Plain(stream) => stream.write(buffer),
            StreamKind::Server(stream) => stream.write(buffer),
            StreamKind::Client(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match &mut self.kind {
            StreamKind::Plain(stream) => stream.flush(),
            StreamKind::Server(stream) => stream.flush(),
            StreamKind::Client(stream) => stream.flush(),
        }
    }
}

impl PeerNode {
    /// 绑定监听地址并启动接收线程。
    pub fn start(options: PeerOptions) -> Result<Self, PeerError> {
        let listener = TcpListener::bind(options.bind)?;
        listener.set_nonblocking(true)?;
        let address = listener.local_addr()?;
        let membership = match options.role {
            PeerRole::Leader { epoch, term } => {
                Membership::leader(options.identity.clone(), address.to_string(), epoch, term).map_err(PeerError::Membership)?
            }
            PeerRole::Follower => Membership::follower(options.identity.clone()),
        };
        let ownership = match options.role {
            PeerRole::Leader { epoch, term } => Arc::new(Ownership::new(epoch, term, Vec::new())),
            PeerRole::Follower => Arc::new(Ownership::default()),
        };
        let inner = Arc::new(Inner {
            options,
            membership,
            ownership,
            replica: RwLock::new(None),
            watermarks: Mutex::new(BTreeMap::new()),
            sequences: Mutex::new(BTreeMap::new()),
            replication: Mutex::new(()),
            shutdown: AtomicBool::new(false),
        });
        let thread_inner = Arc::clone(&inner);
        let thread = thread::Builder::new().name(format!("yyds-peer-{}", thread_inner.options.identity.node_id)).spawn(move || {
            accept_loop(thread_inner, listener);
        })?;
        Ok(Self { inner, address, thread: Some(thread) })
    }

    /// 本节点实际监听地址。
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// 本节点身份。
    pub fn identity(&self) -> &PeerIdentity {
        &self.inner.options.identity
    }

    /// 成员协议状态。
    pub fn membership(&self) -> &Membership {
        &self.inner.membership
    }

    /// owner / epoch 控制面状态。
    pub fn ownership(&self) -> &Arc<Ownership> {
        &self.inner.ownership
    }

    /// 附着本节点的复制落点。
    ///
    /// 附着后,leader 的 [`Self::replicate`] 会在本地先应用再广播,成员则把收到的
    /// [`ShardReplica`] 应用到同一落点。未附着时复制帧一律拒绝,不静默丢弃。
    pub fn attach_replica(&self, sink: Arc<dyn ReplicaSink>) {
        if let Ok(mut replica) = self.inner.replica.write() {
            *replica = Some(sink);
        }
    }

    /// 是否已附着复制落点。
    pub fn has_replica(&self) -> bool {
        self.inner.replica.read().map(|replica| replica.is_some()).unwrap_or(false)
    }

    /// 当前 owner 应用一条分片记录变更,并广播给所有成员。
    ///
    /// 本地应用先于广播;成员在准入处按 `(epoch, term, leader)` 拒绝旧复制,并把
    /// 自己持有的修订号回报在 [`ReplicaAck`] 中。序号在 `(shard, epoch, term)` 内
    /// 单调分配,调用被串行化以保证连续。授权只来自 owner 表:成员协议 leader 与
    /// 分片 owner 可以不同,接管的 owner 不需要同时是成员 leader。
    pub fn replicate(
        &self,
        shard_id: impl Into<String>,
        epoch: ShardEpoch,
        term: LeaderTerm,
        record: ReplicaRecord,
    ) -> Result<ReplicaReport, PeerError> {
        let shard_id = shard_id.into();
        let view = self.inner.membership.view().ok_or(PeerError::NotJoined)?;
        if self.inner.membership.is_revoked() {
            return Err(PeerError::Revoked);
        }
        let fence = FenceToken {
            epoch,
            term,
            leader_node_id: ReplicaNodeId(self.inner.options.identity.node_id.clone()),
        };
        self.inner.ownership.admit(&ShardId(shard_id.clone()), &fence)?;
        let _serial = self.inner.replication.lock().map_err(|_| PeerError::Protocol("replication lock poisoned".to_owned()))?;
        let sequence = self.inner.next_sequence(&shard_id, epoch, term);
        let write = ShardReplica {
            epoch,
            term,
            leader_node_id: self.inner.options.identity.node_id.clone(),
            shard_id,
            sequence,
            record,
        };
        if let Some(sink) = self.inner.replica_sink() {
            sink.apply(&write).map_err(PeerError::Replica)?;
        }
        Ok(self.inner.broadcast_replica(&write, &view.members))
    }

    /// 当前成员视图。
    pub fn view(&self) -> Option<MemberView> {
        self.inner.membership.view()
    }

    /// 作为 follower 加入 `leader` 的集群,并保存返回的成员表。
    pub fn join(&self, leader: SocketAddr) -> Result<MemberView, PeerError> {
        let (epoch, term) = self.inner.membership.epoch_term();
        let request = JoinRequest {
            identity: self.inner.options.identity.clone(),
            address: self.address.to_string(),
            epoch,
            term,
        };
        let mut stream = self.connect(leader)?;
        write_frame(&mut stream, &PeerFrame::Join(request))?;
        match read_frame(&mut stream)? {
            PeerFrame::JoinAccepted(accepted) => {
                let roster = Roster {
                    leader: accepted.leader.clone(),
                    epoch: accepted.epoch,
                    term: accepted.term,
                    members: accepted.members.clone(),
                };
                self.inner.membership.apply_roster(roster).map_err(PeerError::Membership)?;
                self.inner.membership.view().ok_or(PeerError::NotJoined)
            }
            PeerFrame::JoinRejected(rejected) => Err(PeerError::Rejected(rejected)),
            other => Err(PeerError::Protocol(format!("leader answered join with {other:?}"))),
        }
    }

    /// 向成员广播成员表;返回投递结果。
    pub fn broadcast_roster(&self, roster: &Roster) -> BroadcastReport {
        self.inner.broadcast(PeerFrame::Roster(roster.clone()), &roster.members)
    }

    /// leader 推进 epoch 与 owner 表,并广播给所有成员。
    pub fn announce_epoch(&self, announcement: EpochAnnouncement) -> Result<BroadcastReport, PeerError> {
        if !self.inner.membership.is_leader() {
            return Err(PeerError::NotLeader);
        }
        self.inner.ownership.publish(announcement.clone()).map_err(PeerError::Ownership)?;
        let view = self.inner.membership.advance_epoch(announcement.epoch, announcement.term).map_err(PeerError::Membership)?;
        let mut report = self.inner.broadcast(
            PeerFrame::Roster(Roster { leader: view.leader.clone(), epoch: view.epoch, term: view.term, members: view.members.clone() }),
            &view.members,
        );
        let announcement_report = self.inner.broadcast(PeerFrame::EpochAnnouncement(announcement), &view.members);
        report.delivered.extend(announcement_report.delivered);
        report.failed.extend(announcement_report.failed);
        Ok(report)
    }

    /// leader 切换单分片 owner,并广播新 epoch / term。
    pub fn switch_owner(&self, switch: OwnerSwitch) -> Result<BroadcastReport, PeerError> {
        if !self.inner.membership.is_leader() {
            return Err(PeerError::NotLeader);
        }
        self.inner.ownership.promote(switch.clone()).map_err(PeerError::Ownership)?;
        let view = self.inner.membership.advance_epoch(switch.epoch, switch.term).map_err(PeerError::Membership)?;
        let mut report = self.inner.broadcast(
            PeerFrame::Roster(Roster { leader: view.leader.clone(), epoch: view.epoch, term: view.term, members: view.members.clone() }),
            &view.members,
        );
        // `OwnerSwitch` 自身携带新 `(epoch, term)` 并把该分片切到新 owner,
        // 因此不再先发同值的 `EpochAnnouncement`;否则同一 `(epoch, term)` 的
        // 第二次推进会被严格单调检查拒绝。
        let owner = self.inner.broadcast(PeerFrame::OwnerSwitch(switch), &view.members);
        report.delivered.extend(owner.delivered);
        report.failed.extend(owner.failed);
        Ok(report)
    }

    /// 向指定 peer 发送存活探测。
    pub fn heartbeat(&self, peer: SocketAddr) -> Result<PeerAck, PeerError> {
        let mut stream = self.connect(peer)?;
        write_frame(&mut stream, &PeerFrame::Heartbeat(Heartbeat { epoch: self.inner.ownership.epoch(), term: self.inner.ownership.term() }))?;
        match read_frame(&mut stream)? {
            PeerFrame::Ack(ack) => Ok(ack),
            other => Err(PeerError::Protocol(format!("heartbeat answered with {other:?}"))),
        }
    }

    /// 停止接收并等待监听线程退出。
    pub fn shutdown(mut self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    fn connect(&self, address: SocketAddr) -> Result<PeerStream, PeerError> {
        self.inner.open_stream(address)
    }
}

impl Drop for PeerNode {
    fn drop(&mut self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
    }
}

fn handshake_error(error: io::Error) -> PeerError {
    PeerError::Tls(PeerTlsError::Rejected(error.to_string()))
}

fn accept_loop(inner: Arc<Inner>, listener: TcpListener) {
    while !inner.shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let inner = Arc::clone(&inner);
                let _ = thread::Builder::new().name("yyds-peer-conn".to_owned()).spawn(move || {
                    let _ = serve_connection(&inner, stream);
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(10)),
            Err(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn serve_connection(inner: &Inner, stream: TcpStream) -> Result<(), PeerError> {
    // 监听 socket 是非阻塞的;Windows 上 accepted socket 会继承该模式,必须显式
    // 切回阻塞,否则 TLS 握手的 `complete_io` 会立即返回 WouldBlock。
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(inner.options.timeout))?;
    stream.set_write_timeout(Some(inner.options.timeout))?;
    let _ = stream.set_nodelay(true);
    let mut stream = match &inner.options.tls {
        Some(tls) => {
            let mut stream = StreamOwned::new(tls.server_connection()?, stream);
            stream.conn.complete_io(&mut stream.sock).map_err(handshake_error)?;
            PeerStream { kind: StreamKind::Server(Box::new(stream)) }
        }
        None => PeerStream { kind: StreamKind::Plain(stream) },
    };
    let frame = read_frame(&mut stream)?;
    if let Some(reply) = inner.dispatch(frame) {
        write_frame(&mut stream, &reply)?;
    }
    Ok(())
}

impl Inner {
    fn dispatch(&self, frame: PeerFrame) -> Option<PeerFrame> {
        match frame {
            PeerFrame::Join(request) => match self.membership.admit_join(request) {
                Ok((accepted, roster)) => {
                    self.broadcast(PeerFrame::Roster(roster.clone()), &roster.members);
                    Some(PeerFrame::JoinAccepted(accepted))
                }
                Err(rejected) => Some(PeerFrame::JoinRejected(rejected)),
            },
            PeerFrame::Roster(roster) => Some(self.ack(self.membership.apply_roster(roster).map(|_| ()))),
            PeerFrame::EpochAnnouncement(announcement) => {
                Some(self.ack(self.ownership.publish(announcement).map(|_| ())))
            }
            PeerFrame::OwnerSwitch(switch) => Some(self.ack(self.ownership.promote(switch).map(|_| ()))),
            PeerFrame::ReplicaWrite(write) => Some(self.apply_replica(write)),
            PeerFrame::Heartbeat(_) => Some(PeerFrame::Ack(PeerAck {
                accepted: true,
                epoch: self.ownership.epoch(),
                term: self.ownership.term(),
                message: String::new(),
            })),
            PeerFrame::Ack(_)
            | PeerFrame::ReplicaAck(_)
            | PeerFrame::JoinAccepted(_)
            | PeerFrame::JoinRejected(_) => None,
        }
    }

    /// 应用一条入站复制写,并回报本地修订号。
    ///
    /// 准入先于写入:owner 表与 `(epoch, term, leader)` 不一致时整帧拒绝,分片不被
    /// 触碰。序号按 `(epoch, term)` 比较,旧 epoch 的重放与缺口同样被拒绝。
    fn apply_replica(&self, write: ShardReplica) -> PeerFrame {
        let shard = ShardId(write.shard_id.clone());
        let reject = |message: String| {
            PeerFrame::ReplicaAck(ReplicaAck {
                accepted: false,
                epoch: self.ownership.epoch(),
                term: self.ownership.term(),
                shard_id: write.shard_id.clone(),
                sequence: write.sequence,
                revision: write.record.revision,
                message,
            })
        };
        if self.membership.is_revoked() {
            return reject("node was removed from the cluster roster".to_owned());
        }
        let fence = FenceToken {
            epoch: write.epoch,
            term: write.term,
            leader_node_id: ReplicaNodeId(write.leader_node_id.clone()),
        };
        if let Err(error) = self.ownership.admit(&shard, &fence) {
            return reject(error.to_string());
        }
        if let Some(watermark) = self.watermark(&shard) {
            if watermark.epoch == write.epoch && watermark.term == write.term {
                if write.sequence <= watermark.sequence {
                    return PeerFrame::ReplicaAck(ReplicaAck {
                        accepted: true,
                        epoch: write.epoch,
                        term: write.term,
                        shard_id: write.shard_id,
                        sequence: write.sequence,
                        revision: write.record.revision,
                        message: "replayed sequence".to_owned(),
                    });
                }
                let expected = watermark.sequence.saturating_add(1);
                if write.sequence != expected {
                    return reject(format!("replication gap: expected sequence {expected}"));
                }
            }
        }
        let Some(sink) = self.replica_sink()
        else {
            return reject("node has no replica sink".to_owned());
        };
        match sink.apply(&write) {
            Ok(revision) => {
                self.advance_watermark(&shard, write.epoch, write.term, write.sequence);
                PeerFrame::ReplicaAck(ReplicaAck {
                    accepted: true,
                    epoch: write.epoch,
                    term: write.term,
                    shard_id: write.shard_id,
                    sequence: write.sequence,
                    revision,
                    message: String::new(),
                })
            }
            Err(error) => reject(error.to_string()),
        }
    }

    fn replica_sink(&self) -> Option<Arc<dyn ReplicaSink>> {
        self.replica.read().ok().and_then(|replica| replica.clone())
    }

    fn watermark(&self, shard: &ShardId) -> Option<ReplicaWatermark> {
        self.watermarks.lock().ok().and_then(|watermarks| watermarks.get(&shard.0).copied())
    }

    fn advance_watermark(&self, shard: &ShardId, epoch: ShardEpoch, term: LeaderTerm, sequence: u64) {
        if let Ok(mut watermarks) = self.watermarks.lock() {
            watermarks.insert(shard.0.clone(), ReplicaWatermark { epoch, term, sequence });
        }
    }

    fn next_sequence(&self, shard: &str, epoch: ShardEpoch, term: LeaderTerm) -> u64 {
        let mut sequences = match self.sequences.lock() {
            Ok(sequences) => sequences,
            Err(_) => return 0,
        };
        let key = (shard.to_owned(), epoch.0, term.0);
        let next = sequences.get(&key).copied().unwrap_or(0).saturating_add(1);
        sequences.insert(key, next);
        next
    }

    fn ack(&self, result: Result<(), impl fmt::Display>) -> PeerFrame {
        let (accepted, message) = match result {
            Ok(()) => (true, String::new()),
            Err(error) => (false, error.to_string()),
        };
        PeerFrame::Ack(PeerAck { accepted, epoch: self.ownership.epoch(), term: self.ownership.term(), message })
    }

    fn broadcast(&self, frame: PeerFrame, members: &[super::frame::PeerMember]) -> BroadcastReport {
        let mut report = BroadcastReport::default();
        for member in members {
            if member.node_id == self.options.identity.node_id {
                continue;
            }
            let Ok(address) = member.address.parse::<SocketAddr>()
            else {
                report.failed.push(BroadcastFailure { node_id: member.node_id.clone(), message: "invalid member address".to_owned() });
                continue;
            };
            match self.send(address, &frame) {
                Ok(()) => report.delivered.push(member.node_id.clone()),
                Err(error) => report.failed.push(BroadcastFailure { node_id: member.node_id.clone(), message: error.to_string() }),
            }
        }
        report
    }

    fn send(&self, address: SocketAddr, frame: &PeerFrame) -> Result<(), PeerError> {
        match self.request(address, frame)? {
            PeerFrame::Ack(ack) if ack.accepted => Ok(()),
            PeerFrame::Ack(ack) => Err(PeerError::Protocol(ack.message)),
            other => Err(PeerError::Protocol(format!("broadcast answered with {other:?}"))),
        }
    }

    /// 打开一条到 `address` 的连接,完成 TLS 握手(若启用)并返回可读写流。
    fn open_stream(&self, address: SocketAddr) -> Result<PeerStream, PeerError> {
        let stream = TcpStream::connect_timeout(&address, self.options.timeout)?;
        stream.set_read_timeout(Some(self.options.timeout))?;
        stream.set_write_timeout(Some(self.options.timeout))?;
        let _ = stream.set_nodelay(true);
        let Some(tls) = &self.options.tls
        else {
            return Ok(PeerStream { kind: StreamKind::Plain(stream) });
        };
        let mut stream = StreamOwned::new(tls.client_connection(address)?, stream);
        stream.conn.complete_io(&mut stream.sock).map_err(handshake_error)?;
        Ok(PeerStream { kind: StreamKind::Client(Box::new(stream)) })
    }

    /// 一问一答:写入一帧并返回对端的下一帧。
    fn request(&self, address: SocketAddr, frame: &PeerFrame) -> Result<PeerFrame, PeerError> {
        let mut stream = self.open_stream(address)?;
        write_frame(&mut stream, frame)?;
        Ok(read_frame(&mut stream)?)
    }

    /// 广播一条复制写,并把每个成员的修订号收集进报告。
    fn broadcast_replica(&self, write: &ShardReplica, members: &[PeerMember]) -> ReplicaReport {
        let mut report = ReplicaReport { sequence: write.sequence, delivered: Vec::new(), failed: Vec::new() };
        for member in members {
            if member.node_id == self.options.identity.node_id {
                continue;
            }
            let Ok(address) = member.address.parse::<SocketAddr>()
            else {
                report.failed.push(BroadcastFailure { node_id: member.node_id.clone(), message: "invalid member address".to_owned() });
                continue;
            };
            match self.send_replica(address, write) {
                Ok(revision) => report.delivered.push(ReplicaDelivery { node_id: member.node_id.clone(), revision }),
                Err(error) => report.failed.push(BroadcastFailure { node_id: member.node_id.clone(), message: error.to_string() }),
            }
        }
        report
    }

    fn send_replica(&self, address: SocketAddr, write: &ShardReplica) -> Result<u64, PeerError> {
        match self.request(address, &PeerFrame::ReplicaWrite(write.clone()))? {
            PeerFrame::ReplicaAck(ack) if ack.accepted => Ok(ack.revision),
            PeerFrame::ReplicaAck(ack) => Err(PeerError::Protocol(ack.message)),
            other => Err(PeerError::Protocol(format!("replica write answered with {other:?}"))),
        }
    }
}
