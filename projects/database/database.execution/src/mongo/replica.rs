//! 单成员副本集状态与 `replSet*` 命令语义。
//!
//! `mongod --replSet` 把集合配置持久化在 `local.system.replset`，拓扑计数随进程
//! 存活；本模块对同一成员做同样的事。所有形态（`replSetInitiate`、
//! `replSetGetConfig`、`replSetGetStatus`、initiate 前后的 `hello`、以及重启后
//! 读回的 `local.system.replset` 文档）都在 `mongod 8.0.9` 上实测过。
//!
//! 状态里没有任何宿主持有物：签名密钥、进程标识与连接计数都由本模块生成并
//! 管理，宿主只负责把应答写回线。

use std::time::{SystemTime, UNIX_EPOCH};

use boost_random::Secure;
use database_protocol_mongo::bson::{BinaryData, BsonDocument, BsonValue};

use crate::mongo::{
    commands::{self, CommandError},
    cursor::CursorStore,
    storage::MongoStorage,
    write_concern,
};

/// 为宿主安全随机填充字节；失败按错误上报，不回退弱随机。
fn random_bytes(length: usize) -> Result<Vec<u8>, String> {
    let mut bytes = vec![0_u8; length];
    Secure::system().fill(&mut bytes).map_err(|error| format!("host entropy unavailable: {error}"))?;
    Ok(bytes)
}

/// 当前墙钟毫秒。
fn now_millis() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_millis() as i64).unwrap_or(0)
}

/// 十六进制编码。
fn hex_encode(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

/// 十六进制解码：长度必须是 12 字节，非法输入返回 `None`。
fn object_id_from_hex(text: &str) -> Option<BsonValue> {
    if text.len() != 24 {
        return None;
    }
    let mut bytes = [0_u8; 12];
    for (index, chunk) in text.as_bytes().chunks(2).enumerate() {
        let pair = std::str::from_utf8(chunk).ok()?;
        bytes[index] = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(BsonValue::ObjectId(bytes))
}

/// 把 BSON 数值读成整数，非数值按 0 处理。
fn integer_text(value: &BsonValue) -> i64 {
    match value {
        BsonValue::Int32(number) => i64::from(*number),
        BsonValue::Int64(number) => *number,
        BsonValue::Double(number) => *number as i64,
        _ => 0,
    }
}

/// 整数回显：`Int32` 范围内用 `$numberInt`，其余用 `$numberLong`。
pub fn number_value(value: i64) -> BsonValue {
    if i32::try_from(value).is_ok() { BsonValue::Int32(value as i32) } else { BsonValue::Int64(value) }
}

/// BSON 标量在错误文案里的文本形态。
fn scalar_text(value: Option<&BsonValue>) -> String {
    match value {
        None | Some(BsonValue::Null) => "null".to_string(),
        Some(BsonValue::String(text)) => text.clone(),
        Some(BsonValue::Int32(number)) => number.to_string(),
        Some(BsonValue::Int64(number)) => number.to_string(),
        Some(BsonValue::Double(number)) => format!("{number}"),
        Some(BsonValue::Decimal128(number)) => number.as_str().to_string(),
        Some(BsonValue::Bool(flag)) => flag.to_string(),
        Some(BsonValue::ObjectId(bytes)) => hex_encode(bytes),
        _ => "[object]".to_string(),
    }
}

/// `true` 表示 BSON 数值恰好等于 1（不论编码宽度）。
fn numeric_one(value: &BsonValue) -> bool {
    match value {
        BsonValue::Int32(number) => *number == 1,
        BsonValue::Int64(number) => *number == 1,
        BsonValue::Double(number) => *number == 1.0,
        BsonValue::Decimal128(number) => number.as_str().parse::<f64>().is_ok_and(|parsed| parsed == 1.0),
        _ => false,
    }
}

/// `{ ok: 0, errmsg, code, codeName }` 的副本集错误应答。
pub fn replica_error(code: i64, code_name: &str, message: &str) -> BsonDocument {
    let mut document = BsonDocument::new();
    document.push("ok".to_string(), BsonValue::Double(0.0));
    document.push("errmsg".to_string(), BsonValue::String(message.to_string()));
    document.push("code".to_string(), number_value(code));
    document.push("codeName".to_string(), BsonValue::String(code_name.to_string()));
    document
}

/// 单成员副本集状态。
///
/// `name` 为 `None` 表示进程没有以 `--replSet` 启动。签名密钥与进程标识在
/// 构造时从宿主熵取得，随实例销毁，`mongod` 的 `--keyFile` 秘密同样只在集合
/// 生命周期内有效。
#[derive(Debug)]
pub struct ReplicaSet {
    name: Option<String>,
    process_id: BsonValue,
    counter: i64,
    connection_counter: u32,
    initialized: bool,
    config: Option<BsonDocument>,
    election_id: Option<BsonValue>,
    started_at: i64,
    elected_at: i64,
    seconds: u32,
    increment: u32,
    writes: u64,
    cluster_time: (u32, u32),
    signing_key: Vec<u8>,
    signing_key_id: i64,
}

impl ReplicaSet {
    /// 创建一个副本集状态；`name` 为 `None` 表示单机。
    pub fn new(name: Option<String>) -> Result<Self, String> {
        let process_id = BsonValue::ObjectId(random_bytes(12)?.try_into().expect("12 bytes requested"));
        let now = now_millis();
        let signing_key_id = i64::from_be_bytes(random_bytes(8)?.try_into().expect("8 bytes requested"));
        Ok(Self {
            name,
            process_id,
            counter: 0,
            connection_counter: 0,
            initialized: false,
            config: None,
            election_id: None,
            started_at: now,
            elected_at: now,
            seconds: (now / 1000) as u32,
            increment: 0,
            writes: 0,
            cluster_time: ((now / 1000) as u32, 1),
            signing_key: random_bytes(32)?,
            signing_key_id,
        })
    }

    /// `--replSet` 名字；`None` 表示未启用复制。
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// 当前集合配置。
    pub fn config(&self) -> Option<&BsonDocument> {
        self.config.as_ref()
    }

    /// 按接受顺序分配一个连接标识。
    pub fn next_connection_id(&mut self) -> u32 {
        self.connection_counter += 1;
        self.connection_counter
    }

    /// `true` 表示应答携带 `$clusterTime` / `operationTime`。
    pub fn replies_are_stamped(&self) -> bool {
        self.name.is_some() && self.initialized && self.election_id.is_some()
    }

    /// 记录一次写入，让 `optime` 随真实活动推进。
    ///
    /// 写入产生了 oplog 条目时直接采用最后一条的时间戳，应答与
    /// `local.oplog.rs` 因此不会漂移。
    pub fn note_write(&mut self, optime: Option<(u32, u32)>) {
        if self.name.is_none() || !self.initialized {
            return;
        }
        self.writes += 1;
        if let Some((t, i)) = optime {
            self.seconds = t;
            self.increment = i;
            self.advance_cluster_time(t, i);
            return;
        }
        let seconds = (now_millis() / 1000) as u32;
        if seconds != self.seconds {
            self.seconds = seconds;
            self.increment = 0;
        }
        else {
            self.increment += 1;
        }
        self.advance_cluster_time(self.seconds, self.increment);
    }

    /// 把集群时间推到 `max(current, candidate)`，跨集合绝不回退。
    fn advance_cluster_time(&mut self, seconds: u32, increment: u32) {
        let current = self.cluster_time;
        if seconds > current.0 || (seconds == current.0 && increment > current.1) {
            self.cluster_time = (seconds, increment);
        }
    }

    /// 采纳请求里比本地更新的 `$clusterTime`，与 `mongod` 一致。
    ///
    /// 只读时间戳：客户端签名在真机上只在集合内部成员之间校验。
    fn adopt_client_cluster_time(&mut self, command: &BsonDocument) {
        let Some(BsonValue::Document(cluster)) = command.get("$clusterTime") else {
            return;
        };
        let Some(BsonValue::Timestamp { t, i }) = cluster.get("clusterTime") else {
            return;
        };
        self.advance_cluster_time(*t, *i);
    }

    /// `$clusterTime` 应答字段：当前集群时间加上临时 HMAC-SHA256 签名。
    fn cluster_time_field(&self) -> BsonValue {
        let mut encoded = [0_u8; 8];
        encoded[0..4].copy_from_slice(&self.cluster_time.1.to_le_bytes());
        encoded[4..8].copy_from_slice(&self.cluster_time.0.to_le_bytes());
        let digest = boost_crypto::mac::hmac_sha256(&self.signing_key, &encoded).expect("HMAC-SHA256 is infallible");
        let mut signature = BsonDocument::new();
        signature.push("hash".to_string(), BsonValue::Binary(BinaryData::new(0, digest[..20].to_vec())));
        signature.push("keyId".to_string(), BsonValue::Int64(self.signing_key_id));
        let mut field = BsonDocument::new();
        field.push("clusterTime".to_string(), BsonValue::Timestamp { t: self.cluster_time.0, i: self.cluster_time.1 });
        field.push("signature".to_string(), BsonValue::Document(signature));
        BsonValue::Document(field)
    }

    /// 追加 `$clusterTime` 与 `operationTime`，位置与 `mongod 8.0.9` 一致：
    /// `ok` 之后先是 `$clusterTime`。
    pub fn stamp_reply(&mut self, command: &BsonDocument, body: &BsonDocument) -> BsonDocument {
        if !self.replies_are_stamped() {
            return body.clone();
        }
        self.adopt_client_cluster_time(command);
        let mut result = body.clone();
        result.push("$clusterTime".to_string(), self.cluster_time_field());
        let operation_time = match body.get("opTime") {
            Some(BsonValue::Document(op_time)) => match op_time.get("ts") {
                Some(BsonValue::Timestamp { t, i }) => BsonValue::Timestamp { t: *t, i: *i },
                _ => BsonValue::Timestamp { t: self.seconds, i: self.increment },
            },
            _ => BsonValue::Timestamp { t: self.seconds, i: self.increment },
        };
        result.push("operationTime".to_string(), operation_time);
        result
    }

    /// 副本集写应答的 `electionId` 与 `opTime`；未初始化的成员返回 `None`。
    pub fn write_stamps(&self) -> Option<BsonDocument> {
        if self.name.is_none() || !self.initialized {
            return None;
        }
        let election_id = self.election_id.clone()?;
        let mut stamps = BsonDocument::new();
        stamps.push("electionId".to_string(), election_id);
        stamps.push("opTime".to_string(), self.op_time());
        Some(stamps)
    }

    /// `opTime`：`t: -1` 表示还没有写入过任何 op 的未初始化 term。
    fn op_time(&self) -> BsonValue {
        let term = if self.writes > 0 { 1 } else { -1 };
        let mut document = BsonDocument::new();
        document.push("ts".to_string(), BsonValue::Timestamp { t: self.seconds, i: self.increment });
        document.push("t".to_string(), BsonValue::Int64(term));
        BsonValue::Document(document)
    }

    /// 最后一次写入的完整时间块。
    fn last_write_block(&self) -> BsonValue {
        let written = BsonValue::DateTime(i64::from(self.seconds) * 1000 + i64::from(self.increment));
        let mut document = BsonDocument::new();
        document.push("opTime".to_string(), self.op_time());
        document.push("lastWriteDate".to_string(), written.clone());
        document.push("majorityOpTime".to_string(), self.op_time());
        document.push("majorityWriteDate".to_string(), written);
        BsonValue::Document(document)
    }

    /// 握手里的拓扑部分，字段顺序与 `mongod` 一致。
    ///
    /// `hello` 与 `isMaster` 只差可写主节点字段的名字；`connectionId` 与静态
    /// 上限由宿主接在后面。
    pub fn topology_handshake(&self, command: &str) -> BsonDocument {
        let primary_field = if command == "hello" { "isWritablePrimary" } else { "ismaster" };
        let mut entries = BsonDocument::new();
        let mut topology_version = BsonDocument::new();
        topology_version.push("processId".to_string(), self.process_id.clone());
        topology_version.push("counter".to_string(), BsonValue::Int64(self.counter));
        if self.name.is_none() {
            entries.push(primary_field.to_string(), BsonValue::Bool(true));
            entries.push("topologyVersion".to_string(), BsonValue::Document(topology_version));
            return entries;
        }
        entries.push("topologyVersion".to_string(), BsonValue::Document(topology_version));
        if !self.initialized {
            entries.push(primary_field.to_string(), BsonValue::Bool(false));
            entries.push("secondary".to_string(), BsonValue::Bool(false));
            entries.push("info".to_string(), BsonValue::String("Does not have a valid replica set config".to_string()));
            entries.push("isreplicaset".to_string(), BsonValue::Bool(true));
            return entries;
        }
        let config = self.config.as_ref().expect("an initialized member has a configuration");
        let members: Vec<BsonValue> = match config.get("members") {
            Some(BsonValue::Array(members)) => members.clone(),
            _ => Vec::new(),
        };
        let host_at = |index: usize| member_host(&members[index]);
        entries.push("hosts".to_string(), BsonValue::Array(members.iter().map(member_host).collect()));
        entries.push("setName".to_string(), BsonValue::String(self.name.clone().unwrap_or_default()));
        entries.push("setVersion".to_string(), number_value(integer_text(config.get("version").unwrap_or(&BsonValue::Int32(1)))));
        entries.push(primary_field.to_string(), BsonValue::Bool(true));
        entries.push("secondary".to_string(), BsonValue::Bool(false));
        entries.push("primary".to_string(), host_at(0));
        entries.push("me".to_string(), host_at(0));
        entries.push("electionId".to_string(), self.election_id.clone().unwrap_or(BsonValue::Null));
        entries.push("lastWrite".to_string(), self.last_write_block());
        entries
    }

    /// `replSetInitiate`：单成员集合立即成为 PRIMARY。
    ///
    /// `mongosh` 的 `rs.initiate()` 发送空文档，`mongod 8.0.9` 用默认的单成员
    /// 配置应答并回填 `info2`。校验文案与真机一致。
    fn initiate(&mut self, command: &BsonDocument, me: &str) -> Result<BsonDocument, String> {
        let Some(name) = self.name.clone() else {
            return Ok(replica_error(76, "NoReplicationEnabled", "This node was not started with replication enabled."));
        };
        if self.initialized {
            return Ok(replica_error(23, "AlreadyInitialized", "already initialized"));
        }
        let requested = command.get("replSetInitiate");
        let empty_request = match requested {
            Some(BsonValue::Document(document)) if document.fields.is_empty() => true,
            Some(value) => numeric_one(value),
            None => false,
        };
        // 请求里缺失的成员 host 只能由发起连接的地址补齐，`mongod` 的默认
        // 配置就是这样。
        let config = if empty_request {
            let mut member = BsonDocument::new();
            member.push("_id".to_string(), BsonValue::Int32(0));
            member.push("host".to_string(), BsonValue::String(me.to_string()));
            let mut config = BsonDocument::new();
            config.push("_id".to_string(), BsonValue::String(name.clone()));
            config.push("members".to_string(), BsonValue::Array(vec![BsonValue::Document(member)]));
            config
        }
        else {
            match requested {
                Some(BsonValue::Document(document)) => document.clone(),
                _ => return Ok(replica_error(93, "InvalidReplicaSetConfig", "replSetInitiate requires a configuration document")),
            }
        };
        if config.get("_id").is_none() {
            return Ok(replica_error(93, "InvalidReplicaSetConfig", "BSON field 'ReplSetConfig._id' is missing but a required field"));
        }
        let requested_name = config.get("_id").map(|value| scalar_text(Some(value))).unwrap_or_default();
        if requested_name != name {
            return Ok(replica_error(
                93,
                "InvalidReplicaSetConfig",
                &format!(
                    "Rejecting initiate with a set name that differs from command line set name, initiate set name: {requested_name}, command line set name: {name}"
                ),
            ));
        }
        let members = match config.get("members") {
            Some(BsonValue::Array(members)) => members.clone(),
            Some(_) => Vec::new(),
            None => {
                return Ok(replica_error(93, "InvalidReplicaSetConfig", "BSON field 'ReplSetConfig.members' is missing but a required field"));
            }
        };
        if members.is_empty() {
            return Ok(replica_error(
                93,
                "InvalidReplicaSetConfig",
                &format!("Replica set configuration contains {} members, but must have at least 1 and no more than  50", members.len()),
            ));
        }
        for member in &members {
            let host_missing = match member {
                BsonValue::Document(document) => document.get("host").is_none(),
                _ => true,
            };
            if host_missing {
                let identifier = match member {
                    BsonValue::Document(document) => document.get("_id"),
                    _ => None,
                };
                return Ok(replica_error(
                    93,
                    "InvalidReplicaSetConfig",
                    &format!(
                        "member: {{ _id: {} }} :: caused by :: BSON field 'MemberConfig.host' is missing but a required field",
                        scalar_text(identifier)
                    ),
                ));
            }
        }

        let now = now_millis();
        let replica_set_id = BsonValue::ObjectId(random_bytes(12)?.try_into().expect("12 bytes requested"));
        self.config = Some(normalize_config(&config, replica_set_id));
        self.election_id = Some(random_election_id()?);
        self.initialized = true;
        self.elected_at = now;
        self.seconds = (now / 1000) as u32;
        self.increment = 1;
        self.counter += 1;
        self.advance_cluster_time(self.seconds, self.increment);
        if empty_request {
            let mut reply = BsonDocument::new();
            reply.push(
                "info2".to_string(),
                BsonValue::String("no configuration specified. Using a default configuration for the set".to_string()),
            );
            reply.push("me".to_string(), BsonValue::String(me.to_string()));
            reply.push("ok".to_string(), BsonValue::Double(1.0));
            return Ok(reply);
        }
        let mut reply = BsonDocument::new();
        reply.push("ok".to_string(), BsonValue::Double(1.0));
        Ok(reply)
    }

    /// `replSetInitiate` / `replSetGetConfig` / `replSetGetStatus`。
    pub fn command(&mut self, name: &str, command: &BsonDocument, me: &str) -> Result<BsonDocument, String> {
        if name == "replSetInitiate" {
            return self.initiate(command, me);
        }
        if self.name.is_none() {
            return Ok(replica_error(76, "NoReplicationEnabled", "not running with --replSet"));
        }
        if !self.initialized {
            return Ok(replica_error(94, "NotYetInitialized", "no replset config has been received"));
        }
        if name == "replSetGetConfig" {
            let mut reply = BsonDocument::new();
            reply.push("config".to_string(), BsonValue::Document(self.config.clone().unwrap_or_default()));
            reply.push("ok".to_string(), BsonValue::Double(1.0));
            return Ok(reply);
        }
        Ok(self.status())
    }

    /// `replSetGetStatus` 应答。
    fn status(&self) -> BsonDocument {
        let written = BsonValue::DateTime(i64::from(self.seconds) * 1000 + i64::from(self.increment));
        let mut optimes = BsonDocument::new();
        optimes.push("lastCommittedOpTime".to_string(), self.op_time());
        optimes.push("lastCommittedWallTime".to_string(), written.clone());
        optimes.push("readConcernMajorityOpTime".to_string(), self.op_time());
        optimes.push("appliedOpTime".to_string(), self.op_time());
        optimes.push("durableOpTime".to_string(), self.op_time());
        optimes.push("writtenOpTime".to_string(), self.op_time());
        optimes.push("lastAppliedWallTime".to_string(), written.clone());
        optimes.push("lastDurableWallTime".to_string(), written.clone());
        optimes.push("lastWrittenWallTime".to_string(), written);
        let mut document = BsonDocument::new();
        document.push("set".to_string(), BsonValue::String(self.name.clone().unwrap_or_default()));
        document.push("date".to_string(), BsonValue::DateTime(now_millis()));
        document.push("myState".to_string(), BsonValue::Int32(1));
        document.push("term".to_string(), BsonValue::Int64(1));
        document.push("syncSourceHost".to_string(), BsonValue::String(String::new()));
        document.push("syncSourceId".to_string(), BsonValue::Int32(-1));
        document.push("heartbeatIntervalMillis".to_string(), BsonValue::Int64(2000));
        document.push("majorityVoteCount".to_string(), BsonValue::Int32(1));
        document.push("writeMajorityCount".to_string(), BsonValue::Int32(1));
        document.push("votingMembersCount".to_string(), BsonValue::Int32(1));
        document.push("writableVotingMembersCount".to_string(), BsonValue::Int32(1));
        document.push("optimes".to_string(), BsonValue::Document(optimes));
        document.push("lastStableRecoveryTimestamp".to_string(), BsonValue::Timestamp { t: self.seconds, i: self.increment });
        document.push("members".to_string(), BsonValue::Array(vec![BsonValue::Document(self.member_status())]));
        document.push("ok".to_string(), BsonValue::Double(1.0));
        document
    }

    /// `replSetGetStatus.members[0]`。
    fn member_status(&self) -> BsonDocument {
        let config = self.config.as_ref().expect("an initialized member has a configuration");
        let member = match config.get("members") {
            Some(BsonValue::Array(members)) => match members.first() {
                Some(BsonValue::Document(member)) => member.clone(),
                _ => BsonDocument::new(),
            },
            _ => BsonDocument::new(),
        };
        let written = BsonValue::DateTime(i64::from(self.seconds) * 1000 + i64::from(self.increment));
        let mut document = BsonDocument::new();
        document.push("_id".to_string(), member.get("_id").cloned().unwrap_or(BsonValue::Int32(0)));
        document.push("name".to_string(), member.get("host").cloned().unwrap_or(BsonValue::String(String::new())));
        document.push("health".to_string(), BsonValue::Int32(1));
        document.push("state".to_string(), BsonValue::Int32(1));
        document.push("stateStr".to_string(), BsonValue::String("PRIMARY".to_string()));
        document.push("uptime".to_string(), BsonValue::Int32(((now_millis() - self.started_at) / 1000) as i32));
        document.push("optime".to_string(), self.op_time());
        document.push("optimeDate".to_string(), written.clone());
        document.push("optimeWritten".to_string(), self.op_time());
        document.push("optimeWrittenDate".to_string(), written.clone());
        document.push("lastAppliedWallTime".to_string(), written.clone());
        document.push("lastDurableWallTime".to_string(), written.clone());
        document.push("lastWrittenWallTime".to_string(), written);
        document.push("syncSourceHost".to_string(), BsonValue::String(String::new()));
        document.push("syncSourceId".to_string(), BsonValue::Int32(-1));
        document.push("infoMessage".to_string(), BsonValue::String("Could not find member to sync from".to_string()));
        document.push("electionTime".to_string(), BsonValue::Timestamp { t: (self.elected_at / 1000) as u32, i: 1 });
        document.push("electionDate".to_string(), BsonValue::DateTime(self.elected_at));
        document.push("configVersion".to_string(), member_config_version(config));
        document.push("configTerm".to_string(), BsonValue::Int32(1));
        document.push("self".to_string(), BsonValue::Bool(true));
        document.push("lastHeartbeatMessage".to_string(), BsonValue::String(String::new()));
        document
    }

    /// 从 `local.system.replset` 恢复集合配置，与 `mongod` 重启时一样。
    ///
    /// 集合名不匹配或形态不可用的文档被忽略，成员保持未初始化。`replicaSetId`
    /// 与成员列表沿用存储文档，`electionId` 因重新选举而重生成。
    pub fn restore_config(&mut self, stored: &BsonDocument) -> Result<bool, String> {
        if self.name.is_none() || self.initialized {
            return Ok(false);
        }
        let Some(set_name) = stored.get("_id").map(|value| scalar_text(Some(value))) else {
            return Ok(false);
        };
        if self.name.as_deref() != Some(set_name.as_str()) {
            return Ok(false);
        }
        let Some(BsonValue::Array(members)) = stored.get("members") else {
            return Ok(false);
        };
        if members.is_empty() {
            return Ok(false);
        }
        if members.iter().any(|member| match member {
            BsonValue::Document(document) => !matches!(document.get("host"), Some(BsonValue::String(_))),
            _ => true,
        }) {
            return Ok(false);
        }
        let stored_replica_set_id = stored.get("settings").and_then(|settings| match settings {
            BsonValue::Document(settings) => settings.get("replicaSetId"),
            _ => None,
        });
        let replica_set_id = match stored_replica_set_id {
            Some(BsonValue::String(text)) => object_id_from_hex(text),
            Some(BsonValue::ObjectId(bytes)) => Some(BsonValue::ObjectId(*bytes)),
            _ => None,
        };
        let replica_set_id = match replica_set_id {
            Some(value) => value,
            None => BsonValue::ObjectId(random_bytes(12)?.try_into().expect("12 bytes requested")),
        };
        self.config = Some(normalize_config(stored, replica_set_id));
        self.election_id = Some(random_election_id()?);
        self.initialized = true;
        self.elected_at = now_millis();
        self.seconds = (self.elected_at / 1000) as u32;
        self.increment = 1;
        Ok(true)
    }
}

/// 一次写命令落定后的 oplog 提交或丢弃。
///
/// 成功的写把暂存条目提交到 oplog 并用最后一条的时间戳推进副本集 optime；
/// 失败的写必须把暂存条目丢弃，否则失败的数据会进入 oplog。不产生应答确认
/// 的命令（非写命令）不动 oplog。`mongod` 在应答前完成同样的提交。
pub fn settle_write(
    store: &dyn MongoStorage,
    replica: &mut ReplicaSet,
    name: &str,
    ok: bool,
    wall_millis: i64,
) -> Result<Option<(u32, u32)>, CommandError> {
    if !write_concern::is_write_concern_command(name) {
        return Ok(None);
    }
    if !ok {
        store.discard_staged_oplog();
        return Ok(None);
    }
    let last = store.commit_oplog(wall_millis)?;
    if let Some(optime) = last {
        replica.note_write(Some(optime));
    }
    Ok(last)
}

/// 从 `local.system.replset` 读回集合配置并恢复成当前状态。
///
/// `mongod` 启动时同样读这个集合；没有文档或文档形态不可用时成员保持未初始化。
pub fn restore_stored_config(store: &dyn MongoStorage, replica: &mut ReplicaSet) -> Result<bool, CommandError> {
    let Some(stored) = stored_config(store)? else {
        return Ok(false);
    };
    replica.restore_config(&stored).map_err(|message| CommandError::new(96, message))
}

/// 把当前集合配置写进 `local.system.replset`，与 `mongod` 的 initiate 一致。
///
/// 集合文档是复制元数据而非数据，因此这次写入暂存的 oplog 条目一律丢弃。
pub fn persist_config(store: &dyn MongoStorage, replica: &ReplicaSet) -> Result<bool, CommandError> {
    let Some(config) = replica.config() else {
        return Ok(false);
    };
    let mut command = BsonDocument::new();
    command.push("insert".to_string(), BsonValue::String("system.replset".to_string()));
    command.push("documents".to_string(), BsonValue::Array(vec![BsonValue::Document(config.clone())]));
    command.push("$db".to_string(), BsonValue::String("local".to_string()));
    let stored = match commands::insert(&command, store) {
        Ok(reply) => matches!(reply.get("ok"), Some(BsonValue::Double(value)) if *value == 1.0),
        Err(_) => false,
    };
    store.discard_staged_oplog();
    Ok(stored)
}

/// `local.system.replset` 里的第一份集合文档。
fn stored_config(store: &dyn MongoStorage) -> Result<Option<BsonDocument>, CommandError> {
    let mut command = BsonDocument::new();
    command.push("find".to_string(), BsonValue::String("system.replset".to_string()));
    command.push("filter".to_string(), BsonValue::Document(BsonDocument::new()));
    command.push("$db".to_string(), BsonValue::String("local".to_string()));
    let reply = commands::find(&command, store, &mut CursorStore::new())?;
    let Some(BsonValue::Document(cursor)) = reply.get("cursor") else {
        return Ok(None);
    };
    match cursor.get("firstBatch") {
        Some(BsonValue::Array(batch)) => match batch.first() {
            Some(BsonValue::Document(document)) => Ok(Some(document.clone())),
            _ => Ok(None),
        },
        _ => Ok(None),
    }
}

/// 成员文档的 `host`。
fn member_host(member: &BsonValue) -> BsonValue {
    match member {
        BsonValue::Document(document) => document.get("host").cloned().unwrap_or(BsonValue::String(String::new())),
        _ => BsonValue::String(String::new()),
    }
}

/// 随机 `electionId`：高 4 字节固定 `7fffffff`，后 8 字节来自宿主熵。
fn random_election_id() -> Result<BsonValue, String> {
    let tail = random_bytes(8)?;
    let mut bytes = [0_u8; 12];
    bytes[0..4].copy_from_slice(&0x7fff_ffff_u32.to_be_bytes());
    bytes[4..12].copy_from_slice(&tail);
    Ok(BsonValue::ObjectId(bytes))
}

/// 把一个成员文档归一化成 `mongod` 回显的形态。
fn normalize_member(member: &BsonDocument) -> BsonDocument {
    let mut document = BsonDocument::new();
    document.push("_id".to_string(), member.get("_id").cloned().unwrap_or(BsonValue::Null));
    document.push("host".to_string(), member.get("host").cloned().unwrap_or(BsonValue::String(String::new())));
    for (key, default) in [("arbiterOnly", false), ("buildIndexes", true), ("hidden", false)] {
        let value = match member.get(key) {
            Some(BsonValue::Bool(flag)) => BsonValue::Bool(*flag),
            _ => BsonValue::Bool(default),
        };
        document.push(key.to_string(), value);
    }
    document.push(
        "priority".to_string(),
        match member.get("priority") {
            Some(BsonValue::Int32(priority)) => BsonValue::Int32(*priority),
            Some(BsonValue::Int64(priority)) => BsonValue::Int64(*priority),
            Some(BsonValue::Double(priority)) => BsonValue::Double(*priority),
            _ => BsonValue::Int32(1),
        },
    );
    document.push(
        "tags".to_string(),
        match member.get("tags") {
            Some(BsonValue::Document(tags)) => BsonValue::Document(tags.clone()),
            _ => BsonValue::Document(BsonDocument::new()),
        },
    );
    let delay = member.get("secondaryDelaySecs").map(integer_text).unwrap_or(0);
    document.push("secondaryDelaySecs".to_string(), BsonValue::Int64(delay));
    document.push(
        "votes".to_string(),
        match member.get("votes") {
            Some(BsonValue::Int32(votes)) => BsonValue::Int32(*votes),
            Some(BsonValue::Int64(votes)) => BsonValue::Int32(*votes as i32),
            _ => BsonValue::Int32(1),
        },
    );
    document
}

/// 用给定 `replicaSetId` 把请求配置归一化成集合配置。
fn normalize_config(config: &BsonDocument, replica_set_id: BsonValue) -> BsonDocument {
    let members = match config.get("members") {
        Some(BsonValue::Array(members)) => {
            members.iter().map(|member| match member {
                BsonValue::Document(member) => BsonValue::Document(normalize_member(member)),
                other => other.clone(),
            }).collect()
        }
        _ => Vec::new(),
    };
    let mut settings = BsonDocument::new();
    settings.push("chainingAllowed".to_string(), BsonValue::Bool(true));
    settings.push("heartbeatIntervalMillis".to_string(), BsonValue::Int32(2000));
    settings.push("heartbeatTimeoutSecs".to_string(), BsonValue::Int32(10));
    settings.push("electionTimeoutMillis".to_string(), BsonValue::Int32(10000));
    settings.push("catchUpTimeoutMillis".to_string(), BsonValue::Int32(-1));
    settings.push("catchUpTakeoverDelayMillis".to_string(), BsonValue::Int32(30000));
    settings.push("getLastErrorModes".to_string(), BsonValue::Document(BsonDocument::new()));
    let mut defaults = BsonDocument::new();
    defaults.push("w".to_string(), BsonValue::Int32(1));
    defaults.push("wtimeout".to_string(), BsonValue::Int32(0));
    settings.push("getLastErrorDefaults".to_string(), BsonValue::Document(defaults));
    settings.push("replicaSetId".to_string(), replica_set_id);
    let mut document = BsonDocument::new();
    document.push("_id".to_string(), config.get("_id").cloned().unwrap_or(BsonValue::Null));
    document.push("version".to_string(), BsonValue::Int32(1));
    document.push("term".to_string(), BsonValue::Int32(0));
    document.push("members".to_string(), BsonValue::Array(members));
    document.push("protocolVersion".to_string(), BsonValue::Int64(1));
    document.push("writeConcernMajorityJournalDefault".to_string(), BsonValue::Bool(true));
    document.push("settings".to_string(), BsonValue::Document(settings));
    document
}

/// 配置文档的 `version`。
fn member_config_version(config: &BsonDocument) -> BsonValue {
    config.get("version").cloned().unwrap_or(BsonValue::Int32(1))
}
