//! YYDB 与 YYDS 共享的线协议：帧布局、消息体与 `0000` 版本前缀。
//!
//! 嵌入式与分布式宿主共用同一套帧布局与消息集合；产品身份只体现在 4 字节 magic
//! （`YYDB` 与 `YYDS`，也可视为不同 URI 头）。本 crate 不打开数据库，也不分派请求。

#![warn(missing_docs)]

use std::io::{Read, Write};

use database_types::{Error, Result, SchemaVersion, Value};
use database_udf::{HostFunctionHandle, HostMicroDefinition, UdfType, UdfValue};

/// 嵌入式宿主自声明的 magic（与分布式宿主同一线协议）。
pub const MAGIC_YYDB: &[u8; 4] = b"YYDB";

/// 分布式宿主自声明的 magic（与嵌入式宿主同一线协议）。
pub const MAGIC_YYDS: &[u8; 4] = b"YYDS";

/// 当前线协议版本数字（`0000`）。
pub const WIRE_VERSION: &[u8; 4] = b"0000";

/// 下一版计划中的版本数字（当前不接受）。
pub const WIRE_VERSION_NEXT: &[u8; 4] = b"0001";

/// 绝对头长度（字节）：`magic` + `version` + 其余字段。
pub const HEADER_LEN: usize = 20;

/// 参考服务端 / 客户端的 body 软上限。
pub const MAX_BODY_LEN: u32 = 16 * 1024 * 1024;

/// 线消息类型码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    /// 客户端握手。
    Hello,
    /// 服务端握手回复（版本字符串）。
    HelloOk,
    /// 数据库信息请求。
    Info,
    /// 数据库信息回复。
    InfoOk,
    /// 拉取 schema。
    SchemaGet,
    /// schema 载荷。
    SchemaGetOk,
    /// 确保 schema。
    SchemaEnsure,
    /// 确保 schema 确认。
    SchemaEnsureOk,
    /// 发布 schema 迁移（body：version、document、renames JSON、mode）。
    SchemaMigrate,
    /// schema 迁移确认。
    SchemaMigrateOk,
    /// 收敛延后迁移遗留行（body：budget）。
    SchemaCompact,
    /// 收敛结果（body：升级行数）。
    SchemaCompactOk,
    /// KV 读取。
    KvGet,
    /// KV 读取回复。
    KvGetOk,
    /// KV 写入。
    KvPut,
    /// KV 写入确认。
    KvPutOk,
    /// 注册会话级宿主微函数（仅元数据）。
    MicroRegister,
    /// 宿主微函数注册确认。
    MicroRegisterOk,
    /// 服务端请求对端执行宿主微函数（S→C）。
    MicroHostInvoke,
    /// 宿主微函数结果（C→S）。
    MicroHostInvokeOk,
    /// 调用已注册的标量 UDF（C→S）。
    ScalarCall,
    /// 标量 UDF 结果（S→C）。
    ScalarCallOk,
    /// Hello 之后的客户端认证（令牌或挑战应答）。
    Authenticate,
    /// 认证回复（body 含可用于重连的不透明会话令牌）。
    AuthenticateOk,
    /// 客户端把查询源编译成可复用计划。
    Prepare,
    /// Prepare 回复（body：statement_id 字符串）。
    PrepareOk,
    /// 执行已准备语句，可带绑定参数。
    ExecutePrepared,
    /// ExecutePrepared 回复（body：普通原生行集或具名 wire 投影）。
    ExecutePreparedOk,
    /// 打开显式事务。
    TxnBegin,
    /// 事务已打开。
    TxnBeginOk,
    /// 提交显式事务。
    TxnCommit,
    /// 事务已提交。
    TxnCommitOk,
    /// 回滚显式事务。
    TxnRollback,
    /// 事务已回滚。
    TxnRollbackOk,
    /// 建立保存点（body：保存点名）。
    TxnSavepoint,
    /// 保存点已建立。
    TxnSavepointOk,
    /// 回滚到保存点（body：保存点名）。
    TxnRollbackTo,
    /// 已回滚到保存点。
    TxnRollbackToOk,
    /// 释放保存点（body：保存点名）。
    TxnRelease,
    /// 保存点已释放。
    TxnReleaseOk,
    /// 运行时性能快照请求（含卡点与阶段耗时）。
    PerfReport,
    /// 性能快照回复（body：JSON）。
    PerfReportOk,
    /// 客户端显式关闭会话并释放其租约。
    SessionClose,
    /// 会话已关闭（连接随后由服务端关闭）。
    SessionCloseOk,
    /// 错误回复。
    Error,
    /// 未识别类型（仍按帧发出，便于对端回 Error）。
    Unknown(u16),
}

impl MessageType {
    /// 解析原始 `u16` 类型码。
    pub fn from_u16(value: u16) -> Self {
        match value {
            1 => Self::Hello,
            2 => Self::HelloOk,
            3 => Self::Info,
            4 => Self::InfoOk,
            5 => Self::SchemaGet,
            6 => Self::SchemaGetOk,
            7 => Self::SchemaEnsure,
            8 => Self::SchemaEnsureOk,
            9 => Self::KvGet,
            10 => Self::KvGetOk,
            11 => Self::KvPut,
            12 => Self::KvPutOk,
            13 => Self::MicroRegister,
            14 => Self::MicroRegisterOk,
            15 => Self::MicroHostInvoke,
            16 => Self::MicroHostInvokeOk,
            17 => Self::ScalarCall,
            18 => Self::ScalarCallOk,
            19 => Self::Authenticate,
            20 => Self::AuthenticateOk,
            21 => Self::Prepare,
            22 => Self::PrepareOk,
            23 => Self::ExecutePrepared,
            24 => Self::ExecutePreparedOk,
            25 => Self::TxnBegin,
            26 => Self::TxnBeginOk,
            27 => Self::TxnCommit,
            28 => Self::TxnCommitOk,
            29 => Self::TxnRollback,
            30 => Self::TxnRollbackOk,
            31 => Self::TxnSavepoint,
            32 => Self::TxnSavepointOk,
            33 => Self::TxnRollbackTo,
            34 => Self::TxnRollbackToOk,
            35 => Self::TxnRelease,
            36 => Self::TxnReleaseOk,
            37 => Self::PerfReport,
            38 => Self::PerfReportOk,
            39 => Self::SessionClose,
            40 => Self::SessionCloseOk,
            41 => Self::SchemaMigrate,
            42 => Self::SchemaMigrateOk,
            43 => Self::SchemaCompact,
            44 => Self::SchemaCompactOk,
            255 => Self::Error,
            other => Self::Unknown(other),
        }
    }

    /// 原始类型码。
    pub fn as_u16(self) -> u16 {
        match self {
            Self::Hello => 1,
            Self::HelloOk => 2,
            Self::Info => 3,
            Self::InfoOk => 4,
            Self::SchemaGet => 5,
            Self::SchemaGetOk => 6,
            Self::SchemaEnsure => 7,
            Self::SchemaEnsureOk => 8,
            Self::KvGet => 9,
            Self::KvGetOk => 10,
            Self::KvPut => 11,
            Self::KvPutOk => 12,
            Self::MicroRegister => 13,
            Self::MicroRegisterOk => 14,
            Self::MicroHostInvoke => 15,
            Self::MicroHostInvokeOk => 16,
            Self::ScalarCall => 17,
            Self::ScalarCallOk => 18,
            Self::Authenticate => 19,
            Self::AuthenticateOk => 20,
            Self::Prepare => 21,
            Self::PrepareOk => 22,
            Self::ExecutePrepared => 23,
            Self::ExecutePreparedOk => 24,
            Self::TxnBegin => 25,
            Self::TxnBeginOk => 26,
            Self::TxnCommit => 27,
            Self::TxnCommitOk => 28,
            Self::TxnRollback => 29,
            Self::TxnRollbackOk => 30,
            Self::TxnSavepoint => 31,
            Self::TxnSavepointOk => 32,
            Self::TxnRollbackTo => 33,
            Self::TxnRollbackToOk => 34,
            Self::TxnRelease => 35,
            Self::TxnReleaseOk => 36,
            Self::PerfReport => 37,
            Self::PerfReportOk => 38,
            Self::SessionClose => 39,
            Self::SessionCloseOk => 40,
            Self::SchemaMigrate => 41,
            Self::SchemaMigrateOk => 42,
            Self::SchemaCompact => 43,
            Self::SchemaCompactOk => 44,
            Self::Error => 255,
            Self::Unknown(code) => code,
        }
    }
}

/// The response type a peer must answer `request` with, if it is a request at all.
///
/// This table is the single source of truth for the request/response pairing:
/// hosts (including wasm-bound TypeScript surfaces) build their request maps
/// from it instead of keeping parallel tables that can drift. Response types,
/// `Error`, and `Unknown` are not requests and yield `None`.
pub fn response_type(request: MessageType) -> Option<MessageType> {
    match request {
        MessageType::Hello => Some(MessageType::HelloOk),
        MessageType::Info => Some(MessageType::InfoOk),
        MessageType::SchemaGet => Some(MessageType::SchemaGetOk),
        MessageType::SchemaEnsure => Some(MessageType::SchemaEnsureOk),
        MessageType::SchemaMigrate => Some(MessageType::SchemaMigrateOk),
        MessageType::SchemaCompact => Some(MessageType::SchemaCompactOk),
        MessageType::KvGet => Some(MessageType::KvGetOk),
        MessageType::KvPut => Some(MessageType::KvPutOk),
        MessageType::MicroRegister => Some(MessageType::MicroRegisterOk),
        MessageType::MicroHostInvoke => Some(MessageType::MicroHostInvokeOk),
        MessageType::ScalarCall => Some(MessageType::ScalarCallOk),
        MessageType::Authenticate => Some(MessageType::AuthenticateOk),
        MessageType::Prepare => Some(MessageType::PrepareOk),
        MessageType::ExecutePrepared => Some(MessageType::ExecutePreparedOk),
        MessageType::TxnBegin => Some(MessageType::TxnBeginOk),
        MessageType::TxnCommit => Some(MessageType::TxnCommitOk),
        MessageType::TxnRollback => Some(MessageType::TxnRollbackOk),
        MessageType::TxnSavepoint => Some(MessageType::TxnSavepointOk),
        MessageType::TxnRollbackTo => Some(MessageType::TxnRollbackToOk),
        MessageType::TxnRelease => Some(MessageType::TxnReleaseOk),
        MessageType::PerfReport => Some(MessageType::PerfReportOk),
        MessageType::SessionClose => Some(MessageType::SessionCloseOk),
        _ => None,
    }
}

/// 一条带长度前缀的二进制帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// 消息类型。
    pub msg_type: MessageType,
    /// 保留标志（v0 必须为 0）。
    pub flags: u16,
    /// 客户端选定的请求 id（回复原样回显）。
    pub request_id: u32,
    /// 载荷字节。
    pub body: Vec<u8>,
}

impl Frame {
    /// 构造 flags 为空的帧。
    pub fn new(msg_type: MessageType, request_id: u32, body: impl Into<Vec<u8>>) -> Self {
        Self { msg_type, flags: 0, request_id, body: body.into() }
    }

    /// 编码为线上字节布局（默认 magic `YYDB`）。
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.encode_as(MAGIC_YYDB)
    }

    /// 使用显式宿主 magic（`YYDB` 或 `YYDS`，同一协议）编码。
    pub fn encode_as(&self, product: &[u8; 4]) -> Result<Vec<u8>> {
        if product != MAGIC_YYDB && product != MAGIC_YYDS {
            return Err(Error::Unsupported("wire product magic must be YYDB or YYDS"));
        }
        if self.body.len() > MAX_BODY_LEN as usize {
            return Err(Error::Unsupported("wire body too large"));
        }
        let mut out = Vec::with_capacity(HEADER_LEN + self.body.len());
        out.extend_from_slice(product);
        out.extend_from_slice(WIRE_VERSION);
        out.extend_from_slice(&self.msg_type.as_u16().to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.request_id.to_le_bytes());
        out.extend_from_slice(&(self.body.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.body);
        Ok(out)
    }

    /// 从完整缓冲区（头 + body）解码一帧。
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            return Err(Error::Corrupt("wire frame truncated header"));
        }
        check_prefix(&bytes[..8])?;
        let msg_raw = u16::from_le_bytes([bytes[8], bytes[9]]);
        let msg_type = MessageType::from_u16(msg_raw);
        let flags = u16::from_le_bytes([bytes[10], bytes[11]]);
        let request_id = u32::from_le_bytes([bytes[12], bytes[13], bytes[14], bytes[15]]);
        let body_len = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        if body_len > MAX_BODY_LEN {
            return Err(Error::Unsupported("wire body too large"));
        }
        let need = HEADER_LEN + body_len as usize;
        if bytes.len() < need {
            return Err(Error::Corrupt("wire frame truncated body"));
        }
        Ok(Self { msg_type, flags, request_id, body: bytes[HEADER_LEN..need].to_vec() })
    }
}

fn check_prefix(prefix: &[u8]) -> Result<()> {
    if prefix.len() < 8 {
        return Err(Error::Corrupt("wire frame truncated magic/version"));
    }
    let magic = &prefix[0..4];
    let version = &prefix[4..8];
    if magic != MAGIC_YYDB && magic != MAGIC_YYDS {
        return Err(Error::Corrupt("wire frame bad product magic (want YYDB|YYDS)"));
    }
    if version != WIRE_VERSION {
        return Err(Error::Unsupported("wire version not supported (want 0000)"));
    }
    Ok(())
}

/// 从阻塞 reader 精确读取一帧。
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Frame> {
    let mut header = [0_u8; HEADER_LEN];
    reader.read_exact(&mut header)?;
    check_prefix(&header)?;
    let body_len = u32::from_le_bytes([header[16], header[17], header[18], header[19]]);
    if body_len > MAX_BODY_LEN {
        return Err(Error::Unsupported("wire body too large"));
    }
    let mut body = vec![0_u8; body_len as usize];
    if body_len > 0 {
        reader.read_exact(&mut body)?;
    }
    let mut full = Vec::with_capacity(HEADER_LEN + body.len());
    full.extend_from_slice(&header);
    full.extend_from_slice(&body);
    Frame::decode(&full)
}

/// 向阻塞 writer 写入一帧。
pub fn write_frame<W: Write>(writer: &mut W, frame: &Frame) -> Result<()> {
    let bytes = frame.encode()?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

fn read_u16(buf: &[u8], offset: &mut usize) -> Result<u16> {
    if *offset + 2 > buf.len() {
        return Err(Error::Corrupt("wire body truncated u16"));
    }
    let value = u16::from_le_bytes([buf[*offset], buf[*offset + 1]]);
    *offset += 2;
    Ok(value)
}

fn read_u32(buf: &[u8], offset: &mut usize) -> Result<u32> {
    if *offset + 4 > buf.len() {
        return Err(Error::Corrupt("wire body truncated u32"));
    }
    let value = u32::from_le_bytes([buf[*offset], buf[*offset + 1], buf[*offset + 2], buf[*offset + 3]]);
    *offset += 4;
    Ok(value)
}

fn read_bytes<'a>(buf: &'a [u8], offset: &mut usize, len: u32) -> Result<&'a [u8]> {
    let len = len as usize;
    if *offset + len > buf.len() {
        return Err(Error::Corrupt("wire body truncated bytes"));
    }
    let slice = &buf[*offset..*offset + len];
    *offset += len;
    Ok(slice)
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    push_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

/// 构造一条 `Error` 回复帧。
pub fn error_frame(request_id: u32, error: &Error) -> Frame {
    Frame::new(MessageType::Error, request_id, encode_error_body(error))
}

/// 编码 `SchemaGetOk` body。
pub fn encode_schema_ok(schema: Option<SchemaVersion>) -> Vec<u8> {
    match schema {
        None => vec![0],
        Some(schema) => {
            let mut body = vec![1];
            push_u32(&mut body, schema.version);
            push_bytes(&mut body, schema.document.as_bytes());
            body
        }
    }
}

/// 解码 `SchemaEnsure` body。
pub fn decode_schema_ensure(body: &[u8]) -> Result<(u32, String)> {
    let mut offset = 0;
    let version = read_u32(body, &mut offset)?;
    let doc_len = read_u32(body, &mut offset)?;
    let doc = read_bytes(body, &mut offset, doc_len)?;
    let document = std::str::from_utf8(doc).map_err(|_| Error::Corrupt("schema document is not utf-8"))?.to_owned();
    Ok((version, document))
}

/// 解码 `KvGet` body。
pub fn decode_kv_get(body: &[u8]) -> Result<String> {
    let mut offset = 0;
    let key_len = read_u32(body, &mut offset)?;
    let key = read_bytes(body, &mut offset, key_len)?;
    Ok(std::str::from_utf8(key).map_err(|_| Error::Corrupt("kv key is not utf-8"))?.to_owned())
}

/// 解码 `KvPut` body。
pub fn decode_kv_put(body: &[u8]) -> Result<(String, Vec<u8>)> {
    let mut offset = 0;
    let key_len = read_u32(body, &mut offset)?;
    let key = read_bytes(body, &mut offset, key_len)?;
    let key = std::str::from_utf8(key).map_err(|_| Error::Corrupt("kv key is not utf-8"))?.to_owned();
    let val_len = read_u32(body, &mut offset)?;
    let value = read_bytes(body, &mut offset, val_len)?.to_vec();
    Ok((key, value))
}

/// 编码 `KvGetOk` body。
pub fn encode_kv_get_ok(value: Option<Vec<u8>>) -> Vec<u8> {
    match value {
        None => vec![0],
        Some(bytes) => {
            let mut body = vec![1];
            push_bytes(&mut body, &bytes);
            body
        }
    }
}

/// 编码 `SchemaEnsure` body。
pub fn encode_schema_ensure(version: u32, document: &str) -> Vec<u8> {
    let mut body = Vec::new();
    push_u32(&mut body, version);
    push_bytes(&mut body, document.as_bytes());
    body
}

/// 迁移模式 wire 值：立即重写。
pub const MIGRATION_MODE_REWRITE: u8 = 0;
/// 迁移模式 wire 值：仅元数据变更。
pub const MIGRATION_MODE_METADATA_ONLY: u8 = 1;
/// 迁移模式 wire 值：延后收敛。
pub const MIGRATION_MODE_DEFERRED: u8 = 2;
/// 迁移模式 wire 值：仅重建。
pub const MIGRATION_MODE_REBUILD_ONLY: u8 = 3;

/// 编码 `SchemaMigrate` body。
///
/// 布局：`version u32`、`document`、`renames JSON`（空串表示无重命名）、
/// `mode u8`。迁移策略随语句传输，不进入 schema 文档。
pub fn encode_schema_migrate(version: u32, document: &str, renames_json: &str, mode: u8) -> Vec<u8> {
    let mut body = Vec::new();
    push_u32(&mut body, version);
    push_bytes(&mut body, document.as_bytes());
    push_bytes(&mut body, renames_json.as_bytes());
    body.push(mode);
    body
}

/// 解码 `SchemaMigrate` body。
pub fn decode_schema_migrate(body: &[u8]) -> Result<(u32, String, String, u8)> {
    let mut offset = 0;
    let version = read_u32(body, &mut offset)?;
    let document = read_utf8(body, &mut offset)?;
    let renames = read_utf8(body, &mut offset)?;
    let mode = *body.get(offset).ok_or(Error::Corrupt("schema migrate body is truncated"))?;
    Ok((version, document, renames, mode))
}

/// 编码 `SchemaCompact` body。
pub fn encode_schema_compact(budget: u32) -> Vec<u8> {
    let mut body = Vec::new();
    push_u32(&mut body, budget);
    body
}

/// 解码 `SchemaCompact` body。
pub fn decode_schema_compact(body: &[u8]) -> Result<u32> {
    let mut offset = 0;
    read_u32(body, &mut offset)
}

/// 编码 `SchemaCompactOk` body。
pub fn encode_schema_compact_ok(count: u32) -> Vec<u8> {
    let mut body = Vec::new();
    push_u32(&mut body, count);
    body
}

/// 解码 `SchemaCompactOk` body。
pub fn decode_schema_compact_ok(body: &[u8]) -> Result<u32> {
    let mut offset = 0;
    read_u32(body, &mut offset)
}

fn read_utf8(body: &[u8], offset: &mut usize) -> Result<String> {
    let len = read_u32(body, offset)?;
    let bytes = read_bytes(body, offset, len)?;
    std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| Error::Corrupt("schema payload is not utf-8"))
}

/// 编码 `KvGet` body。
pub fn encode_kv_get(key: &str) -> Vec<u8> {
    let mut body = Vec::new();
    push_bytes(&mut body, key.as_bytes());
    body
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn read_u64(buf: &[u8], offset: &mut usize) -> Result<u64> {
    if *offset + 8 > buf.len() {
        return Err(Error::Corrupt("wire body truncated u64"));
    }
    let value = u64::from_le_bytes([
        buf[*offset],
        buf[*offset + 1],
        buf[*offset + 2],
        buf[*offset + 3],
        buf[*offset + 4],
        buf[*offset + 5],
        buf[*offset + 6],
        buf[*offset + 7],
    ]);
    *offset += 8;
    Ok(value)
}

fn encode_udf_type_tag(ty: UdfType) -> u8 {
    match ty {
        UdfType::Null => 0,
        UdfType::Bool => 1,
        UdfType::I64 => 2,
        UdfType::Text => 3,
        UdfType::Any => {
            unreachable!("UdfType::Any is not encodable on the wire")
        }
    }
}

fn decode_udf_type_tag(tag: u8) -> Result<UdfType> {
    match tag {
        0 => Ok(UdfType::Null),
        1 => Ok(UdfType::Bool),
        2 => Ok(UdfType::I64),
        3 => Ok(UdfType::Text),
        _ => Err(Error::Corrupt("wire scalar value has invalid type tag")),
    }
}

fn encode_udf_value(out: &mut Vec<u8>, value: &UdfValue) {
    match value {
        UdfValue::Null => out.push(0),
        UdfValue::Bool(value) => {
            out.push(1);
            out.push(if *value { 1 } else { 0 });
        }
        UdfValue::I64(value) => {
            out.push(2);
            out.extend_from_slice(&value.to_le_bytes());
        }
        UdfValue::Text(value) => {
            out.push(3);
            push_bytes(out, value.as_bytes());
        }
    }
}

fn decode_udf_value(body: &[u8], offset: &mut usize) -> Result<UdfValue> {
    if *offset >= body.len() {
        return Err(Error::Corrupt("wire scalar value missing tag"));
    }
    let tag = body[*offset];
    *offset += 1;
    match tag {
        0 => Ok(UdfValue::Null),
        1 => {
            if *offset >= body.len() {
                return Err(Error::Corrupt("wire bool value truncated"));
            }
            let value = body[*offset] != 0;
            *offset += 1;
            Ok(UdfValue::Bool(value))
        }
        2 => {
            if *offset + 8 > body.len() {
                return Err(Error::Corrupt("wire i64 value truncated"));
            }
            let value = i64::from_le_bytes([
                body[*offset],
                body[*offset + 1],
                body[*offset + 2],
                body[*offset + 3],
                body[*offset + 4],
                body[*offset + 5],
                body[*offset + 6],
                body[*offset + 7],
            ]);
            *offset += 8;
            Ok(UdfValue::I64(value))
        }
        3 => {
            let len = read_u32(body, offset)?;
            let bytes = read_bytes(body, offset, len)?;
            let text = std::str::from_utf8(bytes).map_err(|_| Error::Corrupt("wire text value is not utf-8"))?.to_owned();
            Ok(UdfValue::Text(text))
        }
        _ => Err(Error::Corrupt("wire scalar value has invalid type tag")),
    }
}

fn encode_udf_values(values: &[UdfValue]) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    if values.len() > 255 {
        return Err(Error::Unsupported("wire scalar arg count exceeds u8"));
    }
    body.push(values.len() as u8);
    for value in values {
        encode_udf_value(&mut body, value);
    }
    Ok(body)
}

fn decode_udf_values(body: &[u8], offset: &mut usize) -> Result<Vec<UdfValue>> {
    if *offset >= body.len() {
        return Err(Error::Corrupt("wire scalar args missing count"));
    }
    let count = body[*offset] as usize;
    *offset += 1;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(decode_udf_value(body, offset)?);
    }
    Ok(values)
}

fn udf_value_to_runtime(value: UdfValue) -> Result<Value> {
    match value {
        UdfValue::Null => Ok(Value::Null),
        UdfValue::Bool(value) => Ok(Value::Bool(value)),
        UdfValue::I64(value) => Ok(Value::I64(value)),
        UdfValue::Text(value) => Ok(Value::Text(value)),
    }
}

fn runtime_value_to_udf(value: &Value) -> Result<UdfValue> {
    match value {
        Value::Null => Ok(UdfValue::Null),
        Value::Bool(value) => Ok(UdfValue::Bool(*value)),
        Value::I64(value) => Ok(UdfValue::I64(*value)),
        Value::Text(value) => Ok(UdfValue::Text(value.clone())),
        _ => Err(Error::Unsupported("scalar call value type is outside the Phase-1 wire subset")),
    }
}

/// 编码 `MicroHostInvoke` body。
pub fn encode_micro_host_invoke(handle: &HostFunctionHandle, args: &[UdfValue]) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    push_u64(&mut body, handle.host_id);
    push_u32(&mut body, handle.version);
    push_bytes(&mut body, handle.function_id.as_bytes());
    body.extend_from_slice(&encode_udf_values(args)?);
    Ok(body)
}

/// 解码 `MicroHostInvoke` body。
pub fn decode_micro_host_invoke(body: &[u8]) -> Result<(HostFunctionHandle, Vec<UdfValue>)> {
    let mut offset = 0;
    let host_id = read_u64(body, &mut offset)?;
    let handle_version = read_u32(body, &mut offset)?;
    let function_id_len = read_u32(body, &mut offset)?;
    let function_id = read_bytes(body, &mut offset, function_id_len)?;
    let function_id = std::str::from_utf8(function_id).map_err(|_| Error::Corrupt("micro host invoke function_id is not utf-8"))?.to_owned();
    let args = decode_udf_values(body, &mut offset)?;
    Ok((HostFunctionHandle { host_id, function_id, version: handle_version }, args))
}

/// 编码 `MicroHostInvokeOk` body。
pub fn encode_micro_host_invoke_ok(value: UdfValue) -> Vec<u8> {
    let mut body = Vec::new();
    encode_udf_value(&mut body, &value);
    body
}

/// 解码 `MicroHostInvokeOk` body。
pub fn decode_micro_host_invoke_ok(body: &[u8]) -> Result<UdfValue> {
    let mut offset = 0;
    decode_udf_value(body, &mut offset)
}

/// 编码 `ScalarCall` body。
pub fn encode_scalar_call(name: &str, version: u32, args: &[Value]) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    push_u32(&mut body, version);
    push_bytes(&mut body, name.as_bytes());
    let mut udf_args = Vec::with_capacity(args.len());
    for value in args {
        udf_args.push(runtime_value_to_udf(value)?);
    }
    body.extend_from_slice(&encode_udf_values(&udf_args)?);
    Ok(body)
}

/// 解码 `ScalarCall` body。
pub fn decode_scalar_call(body: &[u8]) -> Result<(String, u32, Vec<Value>)> {
    let mut offset = 0;
    let version = read_u32(body, &mut offset)?;
    let name_len = read_u32(body, &mut offset)?;
    let name = read_bytes(body, &mut offset, name_len)?;
    let name = std::str::from_utf8(name).map_err(|_| Error::Corrupt("scalar call name is not utf-8"))?.to_owned();
    let udf_args = decode_udf_values(body, &mut offset)?;
    let mut args = Vec::with_capacity(udf_args.len());
    for value in udf_args {
        args.push(udf_value_to_runtime(value)?);
    }
    Ok((name, version, args))
}

/// 编码 `ScalarCallOk` body。
pub fn encode_scalar_call_ok(value: Value) -> Vec<u8> {
    let udf_value = runtime_value_to_udf(&value).expect("scalar call ok value must be wire-safe");
    encode_micro_host_invoke_ok(udf_value)
}

/// 解码 `ScalarCallOk` body。
pub fn decode_scalar_call_ok(body: &[u8]) -> Result<Value> {
    udf_value_to_runtime(decode_micro_host_invoke_ok(body)?)
}

/// 编码 `MicroRegister` body。
pub fn encode_micro_register(definition: &HostMicroDefinition) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    push_u64(&mut body, definition.handle.host_id);
    push_u32(&mut body, definition.handle.version);
    push_u32(&mut body, definition.identity.version);
    push_bytes(&mut body, definition.identity.name().as_bytes());
    push_bytes(&mut body, definition.handle.function_id.as_bytes());
    let arg_count = definition.signature.args.len();
    if arg_count > 255 {
        return Err(Error::Unsupported("wire micro register arg count exceeds u8"));
    }
    body.push(arg_count as u8);
    for ty in &definition.signature.args {
        body.push(encode_udf_type_tag(*ty));
    }
    body.push(encode_udf_type_tag(definition.signature.returns));
    body.extend_from_slice(&definition.fingerprint);
    Ok(body)
}

/// 解码 `MicroRegister` body。
pub fn decode_micro_register(body: &[u8]) -> Result<HostMicroDefinition> {
    let mut offset = 0;
    let host_id = read_u64(body, &mut offset)?;
    let handle_version = read_u32(body, &mut offset)?;
    let udf_version = read_u32(body, &mut offset)?;
    let name_len = read_u32(body, &mut offset)?;
    let name = read_bytes(body, &mut offset, name_len)?;
    let name = std::str::from_utf8(name).map_err(|_| Error::Corrupt("micro register name is not utf-8"))?;
    let function_id_len = read_u32(body, &mut offset)?;
    let function_id = read_bytes(body, &mut offset, function_id_len)?;
    let function_id = std::str::from_utf8(function_id).map_err(|_| Error::Corrupt("micro register function_id is not utf-8"))?.to_owned();
    if offset >= body.len() {
        return Err(Error::Corrupt("micro register missing arg count"));
    }
    let arg_count = body[offset] as usize;
    offset += 1;
    let mut args = Vec::with_capacity(arg_count);
    for _ in 0..arg_count {
        if offset >= body.len() {
            return Err(Error::Corrupt("micro register truncated arg types"));
        }
        args.push(decode_udf_type_tag(body[offset])?);
        offset += 1;
    }
    if offset >= body.len() {
        return Err(Error::Corrupt("micro register missing return type"));
    }
    let returns = decode_udf_type_tag(body[offset])?;
    offset += 1;
    if offset + 32 > body.len() {
        return Err(Error::Corrupt("micro register missing fingerprint"));
    }
    let mut fingerprint = [0_u8; 32];
    fingerprint.copy_from_slice(&body[offset..offset + 32]);
    HostMicroDefinition::from_scalar(name, udf_version, args, returns, host_id, function_id, handle_version, fingerprint)
        .map_err(|error| Error::Udf { name: name.to_owned(), message: error.to_string() })
}

/// 编码 `KvPut` body。
pub fn encode_kv_put(key: &str, value: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    push_bytes(&mut body, key.as_bytes());
    push_bytes(&mut body, value);
    body
}

/// 解码 `SchemaGetOk` body。
pub fn decode_schema_get_ok(body: &[u8]) -> Result<Option<SchemaVersion>> {
    if body.is_empty() {
        return Err(Error::Corrupt("schema get ok empty"));
    }
    if body[0] == 0 {
        return Ok(None);
    }
    let mut offset = 1;
    let version = read_u32(body, &mut offset)?;
    let doc_len = read_u32(body, &mut offset)?;
    let doc = read_bytes(body, &mut offset, doc_len)?;
    let document = std::str::from_utf8(doc).map_err(|_| Error::Corrupt("schema document is not utf-8"))?.to_owned();
    Ok(Some(SchemaVersion { version, document }))
}

/// 解码 `KvGetOk` body。
pub fn decode_kv_get_ok(body: &[u8]) -> Result<Option<Vec<u8>>> {
    if body.is_empty() {
        return Err(Error::Corrupt("kv get ok empty"));
    }
    if body[0] == 0 {
        return Ok(None);
    }
    let mut offset = 1;
    let val_len = read_u32(body, &mut offset)?;
    Ok(Some(read_bytes(body, &mut offset, val_len)?.to_vec()))
}

// ─── Authenticate ─────────────────────────────────────────────────────────

/// 编码 `Authenticate` 请求 body：原始不透明 bearer 令牌。
pub fn encode_authenticate(token: &str) -> Vec<u8> {
    let mut body = Vec::new();
    push_bytes(&mut body, token.as_bytes());
    body
}

/// 解码 `Authenticate` 请求 body。
pub fn decode_authenticate(body: &[u8]) -> Result<String> {
    let mut offset = 0;
    let len = read_u32(body, &mut offset)?;
    let bytes = read_bytes(body, &mut offset, len)?;
    std::str::from_utf8(bytes).map_err(|_| Error::Corrupt("authenticate token is not utf-8")).map(|s| s.to_owned())
}

/// 编码 `AuthenticateOk` 回复 body：可供后续连接使用的会话令牌。
pub fn encode_authenticate_ok(session_token: &str) -> Vec<u8> {
    let mut body = Vec::new();
    push_bytes(&mut body, session_token.as_bytes());
    body
}

/// 解码 `AuthenticateOk` 回复 body，取回会话令牌。
pub fn decode_authenticate_ok(body: &[u8]) -> Result<String> {
    let mut offset = 0;
    let len = read_u32(body, &mut offset)?;
    let bytes = read_bytes(body, &mut offset, len)?;
    std::str::from_utf8(bytes).map_err(|_| Error::Corrupt("authenticate ok token is not utf-8")).map(|s| s.to_owned())
}

// ─── Prepare / ExecutePrepared ────────────────────────────────────────────

/// 编码 `Prepare` 请求 body：原始查询源文本。
pub fn encode_prepare(source: &str) -> Vec<u8> {
    let mut body = Vec::new();
    push_bytes(&mut body, source.as_bytes());
    body
}

/// 解码 `Prepare` 请求 body。
pub fn decode_prepare(body: &[u8]) -> Result<String> {
    let mut offset = 0;
    let len = read_u32(body, &mut offset)?;
    let bytes = read_bytes(body, &mut offset, len)?;
    std::str::from_utf8(bytes).map_err(|_| Error::Corrupt("prepare source is not utf-8")).map(|s| s.to_owned())
}

/// 编码 `PrepareOk` 回复 body：statement_id 字符串。
pub fn encode_prepare_ok(statement_id: &str) -> Vec<u8> {
    let mut body = Vec::new();
    push_bytes(&mut body, statement_id.as_bytes());
    body
}

/// 解码 `PrepareOk` 回复 body。
pub fn decode_prepare_ok(body: &[u8]) -> Result<String> {
    let mut offset = 0;
    let len = read_u32(body, &mut offset)?;
    let bytes = read_bytes(body, &mut offset, len)?;
    std::str::from_utf8(bytes).map_err(|_| Error::Corrupt("prepare ok statement_id is not utf-8")).map(|s| s.to_owned())
}

/// 编码 `ExecutePrepared` 请求 body：statement_id + 可选原生类型化参数。
pub fn encode_execute_prepared(statement_id: &str, params: Option<&[u8]>) -> Vec<u8> {
    let mut body = Vec::new();
    push_bytes(&mut body, statement_id.as_bytes());
    match params {
        Some(p) if !p.is_empty() => push_bytes(&mut body, p),
        _ => push_u32(&mut body, 0),
    }
    body
}

/// 解码 `ExecutePrepared` 请求 body → (statement_id, 可选原生参数体)。
pub fn decode_execute_prepared(body: &[u8]) -> Result<(String, Option<Vec<u8>>)> {
    let mut offset = 0;
    let id_len = read_u32(body, &mut offset)?;
    let id_bytes = read_bytes(body, &mut offset, id_len)?;
    let statement_id = std::str::from_utf8(id_bytes).map_err(|_| Error::Corrupt("execute prepared statement_id is not utf-8"))?.to_owned();
    let params_len = read_u32(body, &mut offset)?;
    let params = if params_len > 0 { Some(read_bytes(body, &mut offset, params_len)?.to_vec()) } else { None };
    Ok((statement_id, params))
}

const OK_ROWS_PLAIN: u8 = 0;
const OK_ROWS_NAMED: u8 = 1;

/// `ExecutePreparedOk` body 携带的行投影。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutePreparedRows {
    /// 无具名类型的普通行集（[`encode_rows`] / [`decode_rows`]）。
    Plain(Vec<u8>),
    /// 具名 wire 投影（[`encode_named_rows`] / [`decode_named_rows`]）及解码目录。
    Named {
        /// 服务端这次投影所用的目录。
        directory: WireNamedDirectory,
        /// 投影后的行集 body。
        rows: Vec<u8>,
    },
}

/// 编码 `ExecutePreparedOk` 回复 body —— 普通原生类型化行集。
pub fn encode_execute_prepared_ok(rows: &[u8]) -> Vec<u8> {
    let mut body = vec![OK_ROWS_PLAIN];
    push_bytes(&mut body, rows);
    body
}

/// 编码 `ExecutePreparedOk` 回复 body —— 具名 wire 投影及其目录。
///
/// 目录随帧发送，客户端不必自己持有 schema 合同就能校验 revision、判别值和
/// 载荷成员；未知版本或标签仍按 `database.protocol` 拒绝。
pub fn encode_named_execute_prepared_ok(directory: &WireNamedDirectory, rows: &[u8]) -> Result<Vec<u8>> {
    let mut body = vec![OK_ROWS_NAMED];
    push_bytes(&mut body, &encode_named_directory(directory)?);
    push_bytes(&mut body, rows);
    Ok(body)
}

/// 解码 `ExecutePreparedOk` 回复 body。
pub fn decode_execute_prepared_ok(body: &[u8]) -> Result<ExecutePreparedRows> {
    let mut offset = 0;
    let projection = read_u8(body, &mut offset).map_err(protocol_from_read)?;
    let rows = match projection {
        OK_ROWS_PLAIN => {
            let len = read_u32(body, &mut offset).map_err(protocol_from_read)?;
            read_bytes(body, &mut offset, len).map_err(protocol_from_read)?.to_vec()
        }
        OK_ROWS_NAMED => {
            let directory_len = read_u32(body, &mut offset).map_err(protocol_from_read)?;
            let directory_bytes = read_bytes(body, &mut offset, directory_len).map_err(protocol_from_read)?;
            let mut directory_offset = 0;
            let directory = read_wire_directory(directory_bytes, &mut directory_offset)?;
            if directory_offset != directory_bytes.len() {
                return Err(protocol_error("wire named directory has trailing bytes"));
            }
            let len = read_u32(body, &mut offset).map_err(protocol_from_read)?;
            let rows = read_bytes(body, &mut offset, len).map_err(protocol_from_read)?.to_vec();
            if offset != body.len() {
                return Err(protocol_error("wire execute result has trailing bytes"));
            }
            return Ok(ExecutePreparedRows::Named { directory, rows });
        }
        other => return Err(protocol_error(format!("wire execute result has an unknown rows projection `{other}`"))),
    };
    if offset != body.len() {
        return Err(protocol_error("wire execute result has trailing bytes"));
    }
    Ok(ExecutePreparedRows::Plain(rows))
}

/// 结构化的 `Error` 帧 body：错误码加可读消息。
///
/// 完整诊断合同见 [`Error::to_json`]（code、operation、retryable、params、message）。
/// [`encode_error_body`] / [`decode_error`] 负责该紧凑 JSON 对象的线上编解码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError {
    /// 稳定错误码（如 `yy.schema.invalid`）；见 [`Error::code`]。
    pub code: String,
    /// 对端给出的可读消息。宿主不得按此文本分支，应按 [`Self::code`] 分支。
    pub message: String,
    /// 高层操作类别（`"schema"`、`"udf"` 等）。
    pub operation: String,
    /// 退避后是否可重试。
    pub retryable: bool,
    /// 可选的结构化键值参数，供目录化模板使用。
    pub params: Option<std::collections::BTreeMap<String, String>>,
}

/// 把 [`Error`] 编码为 `Error` 帧 body（紧凑诊断 JSON）。
pub fn encode_error_body(error: &Error) -> Vec<u8> {
    let json = serde_json::to_vec(&error.to_json()).expect("yy diagnostic JSON must never fail to encode");
    json
}

/// 从诊断 JSON 解码 `Error` 帧 body。
pub fn decode_error(body: &[u8]) -> Result<WireError> {
    let value: serde_json::Value = serde_json::from_slice(body).map_err(|_| Error::Corrupt("wire error body is not valid JSON"))?;
    let code = value["code"].as_str().ok_or_else(|| Error::Corrupt("wire error body missing code"))?.to_owned();
    let message = value["message"].as_str().ok_or_else(|| Error::Corrupt("wire error body missing message"))?.to_owned();
    let operation = value["operation"].as_str().ok_or_else(|| Error::Corrupt("wire error body missing operation"))?.to_owned();
    let retryable = value["retryable"].as_bool().ok_or_else(|| Error::Corrupt("wire error body missing retryable"))?;
    let params = value["params"].as_object().map(|obj| obj.iter().filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned()))).collect());
    Ok(WireError { code, message, operation, retryable, params })
}

// ─── 原生类型化值编码（客户端与服务端共用） ────────────────────────────────
//
// 线协议不承载 JSON：VOS 没有 json 类型，动态列就是 `any`，在运行时是原生
// `Value::Row` / `Value::List`。所以参数与行直接走与行记录同一套类型标签的
// 二进制编码，不做文本中转，也不做十六进制/字符串化的标量伪装。

const TAG_NULL: u8 = 0;
const TAG_BOOL: u8 = 1;
const TAG_I64: u8 = 2;
const TAG_U64: u8 = 3;
const TAG_F64: u8 = 4;
const TAG_TEXT: u8 = 5;
const TAG_BYTES: u8 = 6;
const TAG_UUID: u8 = 7;
const TAG_DATE: u8 = 8;
const TAG_TIME: u8 = 9;
const TAG_TIMESTAMP: u8 = 10;
const TAG_DECIMAL: u8 = 11;
const TAG_LIST: u8 = 12;
const TAG_ROW: u8 = 13;
const TAG_VECTOR: u8 = 14;

/// 值嵌套深度的硬上限；超限 fail-closed，避免恶意载荷打穿栈。
const MAX_WIRE_VALUE_DEPTH: u8 = 64;

/// 把单个运行时值编码成原生类型化字节。
///
/// 覆盖 `any` 能装下的全部形态：标量、`List`、嵌套 `Row`、`Vector`。
/// `Object` / `File` 是宿主本地 CAS 句柄，不可跨进程搬运，编码即拒绝；
/// 读路径在编码前已经通过对象解析把引用换成了内联值。
pub fn encode_value(value: &Value) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    encode_value_into(&mut body, value, 0)?;
    Ok(body)
}

/// [`encode_value`] 的逆变换。
pub fn decode_value(body: &[u8]) -> Result<Value> {
    let mut offset = 0;
    let value = decode_value_at(body, &mut offset, 0)?;
    if offset != body.len() {
        return Err(Error::Corrupt("wire value has trailing bytes"));
    }
    Ok(value)
}

fn encode_value_into(out: &mut Vec<u8>, value: &Value, depth: u8) -> Result<()> {
    match value {
        Value::Null => out.push(TAG_NULL),
        Value::Bool(flag) => {
            out.push(TAG_BOOL);
            out.push(u8::from(*flag));
        }
        Value::I64(integer) => {
            out.push(TAG_I64);
            out.extend_from_slice(&integer.to_le_bytes());
        }
        Value::U64(integer) => {
            out.push(TAG_U64);
            out.extend_from_slice(&integer.to_le_bytes());
        }
        Value::F64(number) => {
            out.push(TAG_F64);
            out.extend_from_slice(&number.to_bits().to_le_bytes());
        }
        Value::Text(text) => {
            out.push(TAG_TEXT);
            push_bytes(out, text.as_bytes());
        }
        Value::Bytes(bytes) => {
            out.push(TAG_BYTES);
            push_bytes(out, bytes);
        }
        Value::Uuid(id) => {
            out.push(TAG_UUID);
            out.extend_from_slice(id.as_bytes());
        }
        Value::Date(date) => {
            out.push(TAG_DATE);
            out.extend_from_slice(&date.days().to_le_bytes());
        }
        Value::Time(time) => {
            out.push(TAG_TIME);
            out.extend_from_slice(&time.nanos().to_le_bytes());
        }
        Value::Timestamp(timestamp) => {
            out.push(TAG_TIMESTAMP);
            out.extend_from_slice(&timestamp.unix_micros().to_le_bytes());
        }
        Value::Decimal(decimal) => {
            out.push(TAG_DECIMAL);
            out.extend_from_slice(&decimal.unscaled().to_le_bytes());
            out.push(decimal.precision());
            out.push(decimal.scale());
        }
        Value::List(items) => {
            out.push(TAG_LIST);
            let nested = next_depth(depth)?;
            let count = u32::try_from(items.len()).map_err(|_| Error::Unsupported("wire list has more than 4294967295 elements"))?;
            push_u32(out, count);
            for item in items {
                encode_value_into(out, item, nested)?;
            }
        }
        Value::Row(fields) => {
            out.push(TAG_ROW);
            encode_row_fields(out, fields, next_depth(depth)?)?;
        }
        Value::Vector(vector) => {
            out.push(TAG_VECTOR);
            let count =
                u32::try_from(vector.data.len()).map_err(|_| Error::Unsupported("wire vector has more than 4294967295 components"))?;
            out.extend_from_slice(&vector.dim.to_le_bytes());
            push_u32(out, count);
            for component in vector.data.iter() {
                out.extend_from_slice(&component.to_le_bytes());
            }
        }
        Value::Object(_) | Value::File(_) => {
            return Err(Error::Unsupported("wire values must be resolved: object and file descriptors are host-local CAS handles"));
        }
        other => {
            return Err(Error::Protocol { message: format!("value kind `{}` has no portable wire encoding", other.kind()) });
        }
    }
    Ok(())
}

fn decode_value_at(body: &[u8], offset: &mut usize, depth: u8) -> Result<Value> {
    let tag = read_u8(body, offset)?;
    match tag {
        TAG_NULL => Ok(Value::Null),
        TAG_BOOL => Ok(Value::Bool(read_u8(body, offset)? != 0)),
        TAG_I64 => Ok(Value::I64(read_i64(body, offset)?)),
        TAG_U64 => Ok(Value::U64(read_u64(body, offset)?)),
        TAG_F64 => Ok(Value::F64(f64::from_bits(read_u64(body, offset)?))),
        TAG_TEXT => {
            let len = read_u32(body, offset)?;
            let bytes = read_bytes(body, offset, len)?;
            let text = std::str::from_utf8(bytes).map_err(|_| Error::Corrupt("wire text value is not utf-8"))?;
            Ok(Value::Text(text.to_owned()))
        }
        TAG_BYTES => {
            let len = read_u32(body, offset)?;
            Ok(Value::Bytes(read_bytes(body, offset, len)?.to_vec()))
        }
        TAG_UUID => {
            if *offset + 16 > body.len() {
                return Err(Error::Corrupt("wire uuid value truncated"));
            }
            let mut bytes = [0_u8; 16];
            bytes.copy_from_slice(&body[*offset..*offset + 16]);
            *offset += 16;
            Ok(Value::Uuid(uuid::Uuid::from_bytes(bytes)))
        }
        TAG_DATE => database_types::Date::from_days(read_i32(body, offset)?).map(Value::Date).map_err(|_| Error::Corrupt("wire date is out of range")),
        TAG_TIME => {
            database_types::Time::from_nanos(read_i64(body, offset)?).map(Value::Time).map_err(|_| Error::Corrupt("wire time is out of range"))
        }
        TAG_TIMESTAMP => database_types::Timestamp::from_unix_micros(read_i64(body, offset)?)
            .map(Value::Timestamp)
            .map_err(|_| Error::Corrupt("wire timestamp is out of range")),
        TAG_DECIMAL => {
            let unscaled = read_i128(body, offset)?;
            let precision = read_u8(body, offset)?;
            let scale = read_u8(body, offset)?;
            database_types::Decimal::new(unscaled, precision, scale)
                .map(Value::Decimal)
                .map_err(|_| Error::Corrupt("wire decimal is out of range"))
        }
        TAG_LIST => {
            let nested = next_depth(depth)?;
            let count = read_u32(body, offset)? as usize;
            if count > body.len() - *offset {
                return Err(Error::Corrupt("wire list count exceeds the remaining body"));
            }
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                items.push(decode_value_at(body, offset, nested)?);
            }
            Ok(Value::List(items))
        }
        TAG_ROW => {
            let nested = next_depth(depth)?;
            Ok(Value::Row(read_row_fields(body, offset, nested)?))
        }
        TAG_VECTOR => {
            let dim = read_u32(body, offset)?;
            let count = read_u32(body, offset)? as usize;
            if count != dim as usize {
                return Err(Error::Corrupt("wire vector dimension does not match its component count"));
            }
            let mut data = Vec::with_capacity(count);
            for _ in 0..count {
                data.push(read_f32(body, offset)?);
            }
            Ok(Value::Vector(database_types::Vector { dim, data: data.into() }))
        }
        _ => Err(Error::Corrupt("wire value has an unknown type tag")),
    }
}

fn next_depth(depth: u8) -> Result<u8> {
    depth.checked_add(1).filter(|nested| *nested <= MAX_WIRE_VALUE_DEPTH).ok_or(Error::Corrupt("wire value nests deeper than the supported depth"))
}

fn encode_row_fields(out: &mut Vec<u8>, fields: &std::collections::BTreeMap<String, Value>, depth: u8) -> Result<()> {
    let count = u32::try_from(fields.len()).map_err(|_| Error::Unsupported("wire row has more than 4294967295 fields"))?;
    push_u32(out, count);
    for (key, value) in fields {
        push_bytes(out, key.as_bytes());
        encode_value_into(out, value, depth)?;
    }
    Ok(())
}

fn read_row_fields(body: &[u8], offset: &mut usize, depth: u8) -> Result<std::collections::BTreeMap<String, Value>> {
    let count = read_u32(body, offset)? as usize;
    if count > body.len() - *offset {
        return Err(Error::Corrupt("wire row field count exceeds the remaining body"));
    }
    let mut fields = std::collections::BTreeMap::new();
    for _ in 0..count {
        let key_len = read_u32(body, offset)?;
        let key = read_bytes(body, offset, key_len)?;
        let key = std::str::from_utf8(key).map_err(|_| Error::Corrupt("wire row field name is not utf-8"))?.to_owned();
        fields.insert(key, decode_value_at(body, offset, depth)?);
    }
    Ok(fields)
}

fn read_u8(body: &[u8], offset: &mut usize) -> Result<u8> {
    if *offset >= body.len() {
        return Err(Error::Corrupt("wire body truncated u8"));
    }
    let value = body[*offset];
    *offset += 1;
    Ok(value)
}

fn read_i32(body: &[u8], offset: &mut usize) -> Result<i32> {
    Ok(read_u32(body, offset)? as i32)
}

fn read_i64(body: &[u8], offset: &mut usize) -> Result<i64> {
    Ok(read_u64(body, offset)? as i64)
}

fn read_i128(body: &[u8], offset: &mut usize) -> Result<i128> {
    if *offset + 16 > body.len() {
        return Err(Error::Corrupt("wire body truncated i128"));
    }
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&body[*offset..*offset + 16]);
    *offset += 16;
    Ok(i128::from_le_bytes(bytes))
}

fn read_f32(body: &[u8], offset: &mut usize) -> Result<f32> {
    if *offset + 4 > body.len() {
        return Err(Error::Corrupt("wire body truncated f32"));
    }
    let mut bytes = [0_u8; 4];
    bytes.copy_from_slice(&body[*offset..*offset + 4]);
    *offset += 4;
    Ok(f32::from_le_bytes(bytes))
}

/// 把具名参数编码成 `ExecutePrepared` 的 body。
pub fn encode_params(params: &std::collections::BTreeMap<String, Value>) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    encode_row_fields(&mut body, params, 0)?;
    Ok(body)
}

/// 解码 [`encode_params`] 产出的 body。
pub fn decode_params(body: &[u8]) -> Result<std::collections::BTreeMap<String, Value>> {
    let mut offset = 0;
    let params = read_row_fields(body, &mut offset, 0)?;
    if offset != body.len() {
        return Err(Error::Corrupt("wire params have trailing bytes"));
    }
    Ok(params)
}

/// 把查询行编码成 `ExecutePreparedOk` 的 body。
pub fn encode_rows(rows: &[std::collections::BTreeMap<String, Value>]) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    let count = u32::try_from(rows.len()).map_err(|_| Error::Unsupported("wire row set has more than 4294967295 rows"))?;
    push_u32(&mut body, count);
    for row in rows {
        encode_row_fields(&mut body, row, 0)?;
    }
    Ok(body)
}

/// 解码 [`encode_rows`] 产出的 body。
pub fn decode_rows(body: &[u8]) -> Result<Vec<std::collections::BTreeMap<String, Value>>> {
    let mut offset = 0;
    let count = read_u32(body, &mut offset)? as usize;
    if count > body.len() - offset {
        return Err(Error::Corrupt("wire row count exceeds the remaining body"));
    }
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        rows.push(read_row_fields(body, &mut offset, 0)?);
    }
    if offset != body.len() {
        return Err(Error::Corrupt("wire rows have trailing bytes"));
    }
    Ok(rows)
}

// ─── 具名类型值的线投影 ────────────────────────────────────────────────────
//
// 存储行格式的标签（`TAG_UNITE = 0x11`、`TAG_STRUCTURE = 0x12`）属于持久层，
// 不能出现在客户端可见的帧里。线投影使用自己的一组标签和独立的版本字节，
// 把具名 `class` / `structure` / `unite` 表达成客户端能按名读取的投影；版本、标签、
// revision 或载荷不符都返回 `database.protocol`，不伪装成存储损坏。
//
// 事务帧本身只带命令；行帧（`ExecutePreparedOk`）用 `encode_named_rows` 生成
// 投影 body，并把这次投影用到的目录一并放进同一帧，客户端据此校验并解码。
// 错误帧用 `encode_error_body` 生成结构化诊断。二者都不经过存储行格式。

/// 具名类型值线投影的版本字节。
///
/// 与存储行格式的 `ROW_CODEC_VERSION` 无关：线投影可以独立演进，未知版本按
/// 协议错误拒绝。
pub const NAMED_VALUE_WIRE_VERSION: u8 = 1;

/// 线投影里的封闭 `structure` 标签。刻意避开存储格式的 `0x12`。
const WIRE_TAG_NAMED_STRUCTURE: u8 = 0x20;
/// 线投影里的 `unite` 标签。刻意避开存储格式的 `0x11`。
const WIRE_TAG_NAMED_UNITE: u8 = 0x21;

/// 线投影中的一个值的布局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireValueLayout {
    /// 自描述值，没有具名声明。
    Dynamic,
    /// 列表，元素按内层布局投影。
    List(Box<WireValueLayout>),
    /// 具名声明，按持久 schema 身份引用。
    Named(u64),
}

/// 线投影中的一个具名声明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireNamedLayout {
    /// 具名领域值：线投影保持逐实例字段名，成员布局取自声明。
    ///
    /// `class` 不是封闭位置记录，线投影沿用普通记录的 `TAG_ROW` 形状，只把成员
    /// 类型换成声明里的布局，因此嵌套的 `structure` / `unite` 仍按具名投影编码。
    Class {
        /// 持久 schema 身份。
        schema_id: u64,
        /// 声明顺序的字段。
        fields: Vec<WireNamedField>,
    },
    /// 封闭字段记录：线投影同时携带 schema 身份、revision 和字段名。
    Structure {
        /// 持久 schema 身份。
        schema_id: u64,
        /// 布局 revision；未知 revision 按协议错误拒绝。
        revision: u32,
        /// 声明顺序的字段。
        fields: Vec<WireNamedField>,
    },
    /// 带稳定判别值的联合：线投影携带 schema 身份、revision、判别值和载荷。
    Unite {
        /// 持久 schema 身份。
        schema_id: u64,
        /// 布局 revision；声明顺序变化必须换 revision。
        revision: u32,
        /// 声明顺序的变体。
        variants: Vec<WireNamedVariant>,
    },
}

/// 线投影的一个具名字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireNamedField {
    /// 字段名。
    pub name: String,
    /// 字段值的布局。
    pub ty: WireValueLayout,
}

/// 线投影的一个联合变体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireNamedVariant {
    /// 稳定判别值（声明源序索引）。
    pub discriminant: i64,
    /// 该变体的封闭载荷字段。
    pub fields: Vec<WireNamedField>,
}

/// 线投影使用的目录：列布局按列名索引，具名声明按 schema 身份索引。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WireNamedDirectory {
    columns: std::collections::BTreeMap<String, WireValueLayout>,
    declarations: std::collections::BTreeMap<u64, WireNamedLayout>,
}

impl WireNamedDirectory {
    /// 用列布局和具名声明构造目录。
    pub fn new(
        columns: std::collections::BTreeMap<String, WireValueLayout>,
        declarations: std::collections::BTreeMap<u64, WireNamedLayout>,
    ) -> Self {
        Self { columns, declarations }
    }

    /// 某一列的布局；`None` 表示按自描述值投影。
    pub fn column(&self, name: &str) -> Option<&WireValueLayout> {
        self.columns.get(name)
    }

    /// 某一 schema 身份的具名声明。
    pub fn declaration(&self, schema_id: u64) -> Option<&WireNamedLayout> {
        self.declarations.get(&schema_id)
    }
}

/// 把单个具名值投影成线字节（含版本字节）。
pub fn encode_named_value(value: &Value, directory: &WireNamedDirectory) -> Result<Vec<u8>> {
    let mut body = vec![NAMED_VALUE_WIRE_VERSION];
    encode_named_value_into(&mut body, value, None, directory, 0)?;
    Ok(body)
}

/// 解码 [`encode_named_value`] 产出的单个值。
pub fn decode_named_value(body: &[u8], directory: &WireNamedDirectory) -> Result<Value> {
    let mut offset = 0;
    read_named_version(body, &mut offset)?;
    let value = read_named_value(body, &mut offset, None, directory, 0)?;
    if offset != body.len() {
        return Err(protocol_error("wire named value has trailing bytes"));
    }
    Ok(value)
}

/// 把一行投影成线字节（含版本字节），列布局取自目录。
pub fn encode_named_row(
    row: &std::collections::BTreeMap<String, Value>,
    directory: &WireNamedDirectory,
) -> Result<Vec<u8>> {
    let mut body = vec![NAMED_VALUE_WIRE_VERSION];
    encode_named_fields(&mut body, row, directory, 0)?;
    Ok(body)
}

/// 解码 [`encode_named_row`] 产出的行。
pub fn decode_named_row(body: &[u8], directory: &WireNamedDirectory) -> Result<std::collections::BTreeMap<String, Value>> {
    let mut offset = 0;
    read_named_version(body, &mut offset)?;
    let row = read_named_fields(body, &mut offset, directory, 0)?;
    if offset != body.len() {
        return Err(protocol_error("wire named row has trailing bytes"));
    }
    Ok(row)
}

/// 把查询行集合投影成 `ExecutePreparedOk` 的 body（含版本字节）。
pub fn encode_named_rows(
    rows: &[std::collections::BTreeMap<String, Value>],
    directory: &WireNamedDirectory,
) -> Result<Vec<u8>> {
    let mut body = vec![NAMED_VALUE_WIRE_VERSION];
    let count = u32::try_from(rows.len()).map_err(|_| Error::Unsupported("wire row set has more than 4294967295 rows"))?;
    push_u32(&mut body, count);
    for row in rows {
        encode_named_fields(&mut body, row, directory, 0)?;
    }
    Ok(body)
}

/// 解码 [`encode_named_rows`] 产出的行集合。
pub fn decode_named_rows(
    body: &[u8],
    directory: &WireNamedDirectory,
) -> Result<Vec<std::collections::BTreeMap<String, Value>>> {
    let mut offset = 0;
    read_named_version(body, &mut offset)?;
    let count = read_u32(body, &mut offset)? as usize;
    if count > body.len() - offset {
        return Err(protocol_error("wire row count exceeds the remaining body"));
    }
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        rows.push(read_named_fields(body, &mut offset, directory, 0)?);
    }
    if offset != body.len() {
        return Err(protocol_error("wire named rows have trailing bytes"));
    }
    Ok(rows)
}

/// 具名 wire 目录的版本字节。
///
/// 目录只描述这次投影用到的列布局与具名声明，和行 body 的版本分开演进；未知
/// 版本按协议错误拒绝。
pub const NAMED_DIRECTORY_WIRE_VERSION: u8 = 1;

const WIRE_LAYOUT_DYNAMIC: u8 = 0;
const WIRE_LAYOUT_LIST: u8 = 1;
const WIRE_LAYOUT_NAMED: u8 = 2;

const WIRE_DECLARATION_CLASS: u8 = 0;
const WIRE_DECLARATION_STRUCTURE: u8 = 1;
const WIRE_DECLARATION_UNITE: u8 = 2;

/// 把一个线投影目录编码成自带版本字节的字节串。
pub fn encode_named_directory(directory: &WireNamedDirectory) -> Result<Vec<u8>> {
    let mut body = vec![NAMED_DIRECTORY_WIRE_VERSION];
    let column_count = u32::try_from(directory.columns.len()).map_err(|_| Error::Unsupported("wire directory has too many columns"))?;
    push_u32(&mut body, column_count);
    for (name, layout) in &directory.columns {
        push_bytes(&mut body, name.as_bytes());
        encode_wire_layout(&mut body, layout, 0)?;
    }
    let declaration_count =
        u32::try_from(directory.declarations.len()).map_err(|_| Error::Unsupported("wire directory has too many declarations"))?;
    push_u32(&mut body, declaration_count);
    for (schema_id, declaration) in &directory.declarations {
        encode_wire_declaration(&mut body, *schema_id, declaration)?;
    }
    Ok(body)
}

/// 解码 [`encode_named_directory`] 产出的目录。
pub fn decode_named_directory(body: &[u8]) -> Result<WireNamedDirectory> {
    let mut offset = 0;
    let directory = read_wire_directory(body, &mut offset)?;
    if offset != body.len() {
        return Err(protocol_error("wire named directory has trailing bytes"));
    }
    Ok(directory)
}

fn read_wire_directory(body: &[u8], offset: &mut usize) -> Result<WireNamedDirectory> {
    let version = read_u8(body, offset).map_err(protocol_from_read)?;
    if version != NAMED_DIRECTORY_WIRE_VERSION {
        return Err(protocol_error(format!(
            "wire directory uses version `{version}`, this client speaks `{NAMED_DIRECTORY_WIRE_VERSION}`"
        )));
    }
    let column_count = read_u32(body, offset).map_err(protocol_from_read)? as usize;
    if column_count > body.len() - *offset {
        return Err(protocol_error("wire directory column count exceeds the remaining body"));
    }
    let mut columns = std::collections::BTreeMap::new();
    for _ in 0..column_count {
        let name = read_name(body, offset)?;
        let layout = read_wire_layout(body, offset, 0)?;
        if columns.insert(name.clone(), layout).is_some() {
            return Err(protocol_error(format!("wire directory repeats column `{name}`")));
        }
    }
    let declaration_count = read_u32(body, offset).map_err(protocol_from_read)? as usize;
    if declaration_count > body.len() - *offset {
        return Err(protocol_error("wire directory declaration count exceeds the remaining body"));
    }
    let mut declarations = std::collections::BTreeMap::new();
    for _ in 0..declaration_count {
        let tag = read_u8(body, offset).map_err(protocol_from_read)?;
        let schema_id = read_u64(body, offset).map_err(protocol_from_read)?;
        let declaration = read_wire_declaration(body, offset, tag, schema_id, 0)?;
        if declarations.insert(schema_id, declaration).is_some() {
            return Err(protocol_error(format!("wire directory repeats schema id `{schema_id}`")));
        }
    }
    Ok(WireNamedDirectory::new(columns, declarations))
}

fn encode_wire_layout(out: &mut Vec<u8>, layout: &WireValueLayout, depth: u8) -> Result<()> {
    match layout {
        WireValueLayout::Dynamic => out.push(WIRE_LAYOUT_DYNAMIC),
        WireValueLayout::List(inner) => {
            out.push(WIRE_LAYOUT_LIST);
            encode_wire_layout(out, inner, next_named_depth(depth)?)?;
        }
        WireValueLayout::Named(schema_id) => {
            out.push(WIRE_LAYOUT_NAMED);
            out.extend_from_slice(&schema_id.to_le_bytes());
        }
    }
    Ok(())
}

fn read_wire_layout(body: &[u8], offset: &mut usize, depth: u8) -> Result<WireValueLayout> {
    let tag = read_u8(body, offset).map_err(protocol_from_read)?;
    match tag {
        WIRE_LAYOUT_DYNAMIC => Ok(WireValueLayout::Dynamic),
        WIRE_LAYOUT_LIST => Ok(WireValueLayout::List(Box::new(read_wire_layout(body, offset, next_named_depth(depth)?)?))),
        WIRE_LAYOUT_NAMED => Ok(WireValueLayout::Named(read_u64(body, offset).map_err(protocol_from_read)?)),
        _ => Err(protocol_error(format!("wire directory has an unknown layout tag `{tag}`"))),
    }
}

fn encode_wire_declaration(out: &mut Vec<u8>, schema_id: u64, declaration: &WireNamedLayout) -> Result<()> {
    match declaration {
        WireNamedLayout::Class { fields, .. } => {
            out.push(WIRE_DECLARATION_CLASS);
            out.extend_from_slice(&schema_id.to_le_bytes());
            encode_wire_fields(out, fields)?;
        }
        WireNamedLayout::Structure { revision, fields, .. } => {
            out.push(WIRE_DECLARATION_STRUCTURE);
            out.extend_from_slice(&schema_id.to_le_bytes());
            out.extend_from_slice(&revision.to_le_bytes());
            encode_wire_fields(out, fields)?;
        }
        WireNamedLayout::Unite { revision, variants, .. } => {
            out.push(WIRE_DECLARATION_UNITE);
            out.extend_from_slice(&schema_id.to_le_bytes());
            out.extend_from_slice(&revision.to_le_bytes());
            let count = u16::try_from(variants.len()).map_err(|_| Error::Unsupported("wire declaration has too many variants"))?;
            out.extend_from_slice(&count.to_le_bytes());
            for variant in variants {
                out.extend_from_slice(&variant.discriminant.to_le_bytes());
                encode_wire_fields(out, &variant.fields)?;
            }
        }
    }
    Ok(())
}

fn read_wire_declaration(body: &[u8], offset: &mut usize, tag: u8, schema_id: u64, depth: u8) -> Result<WireNamedLayout> {
    match tag {
        WIRE_DECLARATION_CLASS => Ok(WireNamedLayout::Class { schema_id, fields: read_wire_fields(body, offset, depth)? }),
        WIRE_DECLARATION_STRUCTURE => {
            let revision = read_u32(body, offset).map_err(protocol_from_read)?;
            Ok(WireNamedLayout::Structure { schema_id, revision, fields: read_wire_fields(body, offset, depth)? })
        }
        WIRE_DECLARATION_UNITE => {
            let revision = read_u32(body, offset).map_err(protocol_from_read)?;
            let count = read_u16(body, offset).map_err(protocol_from_read)? as usize;
            if count > body.len() - *offset {
                return Err(protocol_error("wire directory variant count exceeds the remaining body"));
            }
            let mut variants = Vec::with_capacity(count);
            for _ in 0..count {
                let discriminant = read_i64(body, offset).map_err(protocol_from_read)?;
                variants.push(WireNamedVariant { discriminant, fields: read_wire_fields(body, offset, depth)? });
            }
            Ok(WireNamedLayout::Unite { schema_id, revision, variants })
        }
        _ => Err(protocol_error(format!("wire directory has an unknown declaration tag `{tag}`"))),
    }
}

fn encode_wire_fields(out: &mut Vec<u8>, fields: &[WireNamedField]) -> Result<()> {
    let count = u16::try_from(fields.len()).map_err(|_| Error::Unsupported("wire declaration has more than 65535 members"))?;
    out.extend_from_slice(&count.to_le_bytes());
    for field in fields {
        push_bytes(out, field.name.as_bytes());
        encode_wire_layout(out, &field.ty, 0)?;
    }
    Ok(())
}

fn read_wire_fields(body: &[u8], offset: &mut usize, depth: u8) -> Result<Vec<WireNamedField>> {
    let count = read_u16(body, offset).map_err(protocol_from_read)? as usize;
    if count > body.len() - *offset {
        return Err(protocol_error("wire declaration member count exceeds the remaining body"));
    }
    let mut fields = Vec::with_capacity(count);
    for _ in 0..count {
        let name = read_name(body, offset)?;
        fields.push(WireNamedField { name, ty: read_wire_layout(body, offset, depth)? });
    }
    Ok(fields)
}

fn read_named_version(body: &[u8], offset: &mut usize) -> Result<()> {
    let version = read_u8(body, offset).map_err(protocol_from_read)?;
    if version != NAMED_VALUE_WIRE_VERSION {
        return Err(protocol_error(format!(
            "wire named value uses projection version `{version}`, this client speaks `{NAMED_VALUE_WIRE_VERSION}`"
        )));
    }
    Ok(())
}

fn protocol_error(message: impl Into<String>) -> Error {
    Error::Protocol { message: message.into() }
}

/// Converts a malformed-frame read into a protocol error, never a storage one.
fn protocol_from_read(error: Error) -> Error {
    match error {
        Error::Corrupt(_) | Error::CorruptReason { .. } => protocol_error(format!("malformed YY wire frame: {error}")),
        other => other,
    }
}

fn encode_named_fields(
    out: &mut Vec<u8>,
    fields: &std::collections::BTreeMap<String, Value>,
    directory: &WireNamedDirectory,
    depth: u8,
) -> Result<()> {
    let count = u32::try_from(fields.len()).map_err(|_| Error::Unsupported("wire row has more than 4294967295 fields"))?;
    push_u32(out, count);
    for (name, value) in fields {
        push_bytes(out, name.as_bytes());
        encode_named_value_into(out, value, directory.column(name), directory, depth)?;
    }
    Ok(())
}

fn read_named_fields(
    body: &[u8],
    offset: &mut usize,
    directory: &WireNamedDirectory,
    depth: u8,
) -> Result<std::collections::BTreeMap<String, Value>> {
    let count = read_u32(body, offset).map_err(protocol_from_read)? as usize;
    if count > body.len() - *offset {
        return Err(protocol_error("wire row field count exceeds the remaining body"));
    }
    let mut fields = std::collections::BTreeMap::new();
    for _ in 0..count {
        let name = read_name(body, offset)?;
        let layout = directory.column(&name);
        let value = read_named_value(body, offset, layout, directory, depth)?;
        fields.insert(name, value);
    }
    Ok(fields)
}

fn read_named_record_fields(
    body: &[u8],
    offset: &mut usize,
    declared: Option<&[WireNamedField]>,
    directory: &WireNamedDirectory,
    depth: u8,
) -> Result<std::collections::BTreeMap<String, Value>> {
    let count = read_u32(body, offset).map_err(protocol_from_read)? as usize;
    if count > body.len() - *offset {
        return Err(protocol_error("wire record field count exceeds the remaining body"));
    }
    let mut fields = std::collections::BTreeMap::new();
    for _ in 0..count {
        let name = read_name(body, offset)?;
        let layout = match declared {
            Some(declared) => {
                let field = declared
                    .iter()
                    .find(|field| field.name == name)
                    .ok_or_else(|| protocol_error(format!("wire `class` does not declare member `{name}`")))?;
                Some(&field.ty)
            }
            None => directory.column(&name),
        };
        if fields.contains_key(&name) {
            return Err(protocol_error(format!("wire record repeats member `{name}`")));
        }
        fields.insert(name, read_named_value(body, offset, layout, directory, depth)?);
    }
    Ok(fields)
}

fn read_name(body: &[u8], offset: &mut usize) -> Result<String> {
    let len = read_u32(body, offset).map_err(protocol_from_read)?;
    let bytes = read_bytes(body, offset, len).map_err(protocol_from_read)?;
    std::str::from_utf8(bytes).map(str::to_owned).map_err(|_| protocol_error("wire name is not utf-8"))
}

fn encode_named_value_into(
    out: &mut Vec<u8>,
    value: &Value,
    layout: Option<&WireValueLayout>,
    directory: &WireNamedDirectory,
    depth: u8,
) -> Result<()> {
    match value {
        Value::Row(fields) => match resolve_wire_layout(layout, directory)? {
            Some(WireNamedLayout::Class { fields: declared, .. }) => encode_wire_class(out, fields, declared, directory, depth),
            Some(WireNamedLayout::Structure { schema_id, revision, fields: declared }) => {
                encode_wire_structure(out, fields, *schema_id, *revision, declared, directory, depth)
            }
            Some(WireNamedLayout::Unite { .. }) => {
                Err(protocol_error("a `unite` value must carry a discriminant and payload, not a bare record"))
            }
            None => {
                out.push(TAG_ROW);
                encode_named_fields(out, fields, directory, next_named_depth(depth)?)
            }
        },
        Value::List(items) => match resolve_wire_layout(layout, directory)? {
            Some(WireNamedLayout::Unite { schema_id, revision, variants }) => {
                encode_wire_unite(out, items, *schema_id, *revision, variants, directory, depth)
            }
            _ => {
                let element_layout = match layout {
                    Some(WireValueLayout::List(inner)) => Some(inner.as_ref()),
                    _ => None,
                };
                out.push(TAG_LIST);
                let count = u32::try_from(items.len()).map_err(|_| Error::Unsupported("wire list has more than 4294967295 elements"))?;
                push_u32(out, count);
                let nested = next_named_depth(depth)?;
                for item in items {
                    encode_named_value_into(out, item, element_layout, directory, nested)?;
                }
                Ok(())
            }
        },
        other => encode_value_into(out, other, depth).map_err(protocol_from_read),
    }
}

fn resolve_wire_layout<'a>(
    layout: Option<&WireValueLayout>,
    directory: &'a WireNamedDirectory,
) -> Result<Option<&'a WireNamedLayout>> {
    let Some(WireValueLayout::Named(schema_id)) = layout
    else {
        return Ok(None);
    };
    directory
        .declaration(*schema_id)
        .map(Some)
        .ok_or_else(|| protocol_error(format!("wire projection names schema id `{schema_id}`, which the directory does not declare")))
}

fn encode_wire_class(
    out: &mut Vec<u8>,
    fields: &std::collections::BTreeMap<String, Value>,
    declared: &[WireNamedField],
    directory: &WireNamedDirectory,
    depth: u8,
) -> Result<()> {
    for name in fields.keys() {
        if !declared.iter().any(|field| field.name == *name) {
            return Err(protocol_error(format!("wire `class` does not declare member `{name}`")));
        }
    }
    let count = u32::try_from(fields.len()).map_err(|_| Error::Unsupported("wire class has more than 4294967295 members"))?;
    out.push(TAG_ROW);
    push_u32(out, count);
    let nested = next_named_depth(depth)?;
    for (name, value) in fields {
        push_bytes(out, name.as_bytes());
        let ty = declared.iter().find(|field| field.name == *name).map(|field| &field.ty);
        encode_named_value_into(out, value, ty, directory, nested)?;
    }
    Ok(())
}

fn encode_wire_structure(
    out: &mut Vec<u8>,
    fields: &std::collections::BTreeMap<String, Value>,
    schema_id: u64,
    revision: u32,
    declared: &[WireNamedField],
    directory: &WireNamedDirectory,
    depth: u8,
) -> Result<()> {
    for name in fields.keys() {
        if !declared.iter().any(|field| field.name == *name) {
            return Err(protocol_error(format!("wire `structure` does not declare member `{name}`")));
        }
    }
    let count = u16::try_from(declared.len()).map_err(|_| Error::Unsupported("wire structure has more than 65535 members"))?;
    out.push(WIRE_TAG_NAMED_STRUCTURE);
    out.extend_from_slice(&schema_id.to_le_bytes());
    out.extend_from_slice(&revision.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    let nested = next_named_depth(depth)?;
    for field in declared {
        push_bytes(out, field.name.as_bytes());
        let value = fields.get(&field.name).unwrap_or(&Value::Null);
        encode_named_value_into(out, value, Some(&field.ty), directory, nested)?;
    }
    Ok(())
}

fn encode_wire_unite(
    out: &mut Vec<u8>,
    items: &[Value],
    schema_id: u64,
    revision: u32,
    variants: &[WireNamedVariant],
    directory: &WireNamedDirectory,
    depth: u8,
) -> Result<()> {
    let [tag, payload] = items
    else {
        return Err(protocol_error("a `unite` value must be `[discriminant, payload]` with exactly two elements"));
    };
    let Value::I64(discriminant) = tag
    else {
        return Err(protocol_error("a `unite` discriminant must be an integer"));
    };
    let Value::Row(payload) = payload
    else {
        return Err(protocol_error("a `unite` payload must be a record"));
    };
    let variant = variants
        .iter()
        .find(|variant| variant.discriminant == *discriminant)
        .ok_or_else(|| protocol_error(format!("`{discriminant}` is not a declared `unite` variant")))?;
    for name in payload.keys() {
        if !variant.fields.iter().any(|field| field.name == *name) {
            return Err(protocol_error(format!("wire `unite` variant `{discriminant}` does not declare payload member `{name}`")));
        }
    }
    for field in &variant.fields {
        if !payload.contains_key(&field.name) {
            return Err(protocol_error(format!(
                "wire `unite` variant `{discriminant}` is missing required payload member `{}`",
                field.name
            )));
        }
    }
    let count = u16::try_from(variant.fields.len()).map_err(|_| Error::Unsupported("wire unite variant has more than 65535 members"))?;
    out.push(WIRE_TAG_NAMED_UNITE);
    out.extend_from_slice(&schema_id.to_le_bytes());
    out.extend_from_slice(&revision.to_le_bytes());
    out.extend_from_slice(&discriminant.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    let nested = next_named_depth(depth)?;
    for field in &variant.fields {
        push_bytes(out, field.name.as_bytes());
        let value = payload.get(&field.name).expect("validated payload member");
        encode_named_value_into(out, value, Some(&field.ty), directory, nested)?;
    }
    Ok(())
}

fn read_named_value(
    body: &[u8],
    offset: &mut usize,
    layout: Option<&WireValueLayout>,
    directory: &WireNamedDirectory,
    depth: u8,
) -> Result<Value> {
    let start = *offset;
    let tag = read_u8(body, offset).map_err(protocol_from_read)?;
    match tag {
        WIRE_TAG_NAMED_STRUCTURE => read_wire_structure(body, offset, directory, depth),
        WIRE_TAG_NAMED_UNITE => read_wire_unite(body, offset, directory, depth),
        TAG_ROW => {
            let declared = match resolve_wire_layout(layout, directory)? {
                Some(WireNamedLayout::Class { fields, .. }) => Some(fields.as_slice()),
                Some(WireNamedLayout::Structure { .. }) => {
                    return Err(protocol_error("wire record is a bare record where a `structure` was declared"));
                }
                Some(WireNamedLayout::Unite { .. }) => {
                    return Err(protocol_error("wire record is a bare record where a `unite` was declared"));
                }
                None => None,
            };
            Ok(Value::Row(read_named_record_fields(body, offset, declared, directory, next_named_depth(depth)?)?))
        }
        TAG_LIST => {
            let count = read_u32(body, offset).map_err(protocol_from_read)? as usize;
            if count > body.len() - *offset {
                return Err(protocol_error("wire list count exceeds the remaining body"));
            }
            let element_layout = match layout {
                Some(WireValueLayout::List(inner)) => Some(inner.as_ref()),
                _ => None,
            };
            let nested = next_named_depth(depth)?;
            let mut items = Vec::with_capacity(count);
            for _ in 0..count {
                items.push(read_named_value(body, offset, element_layout, directory, nested)?);
            }
            Ok(Value::List(items))
        }
        _ => {
            *offset = start;
            // Scalar and dynamic value tags keep the existing wire encoding
            // (numbers, text, plain records); only named tags are projected here.
            let value = decode_value_at(body, offset, depth).map_err(protocol_from_read)?;
            Ok(value)
        }
    }
}

fn read_wire_structure(
    body: &[u8],
    offset: &mut usize,
    directory: &WireNamedDirectory,
    depth: u8,
) -> Result<Value> {
    let schema_id = read_u64(body, offset).map_err(protocol_from_read)?;
    let revision = read_u32(body, offset).map_err(protocol_from_read)?;
    let field_count = read_u16(body, offset).map_err(protocol_from_read)? as usize;
    let declaration = directory
        .declaration(schema_id)
        .ok_or_else(|| protocol_error("wire record names a `structure` schema id that the directory does not declare"))?;
    let WireNamedLayout::Structure { revision: expected, fields: declared, .. } = declaration
    else {
        return Err(protocol_error("wire record names a schema id that is not a `structure` declaration"));
    };
    if *expected != revision {
        return Err(protocol_error("wire record uses a `structure` layout revision the directory does not declare"));
    }
    if declared.len() != field_count {
        return Err(protocol_error("wire record `structure` field count disagrees with its declaration"));
    }
    let nested = next_named_depth(depth)?;
    let mut fields = std::collections::BTreeMap::new();
    for _ in 0..field_count {
        let name = read_name(body, offset)?;
        let Some(declared_field) = declared.iter().find(|field| field.name == name)
        else {
            return Err(protocol_error(format!("wire `structure` does not declare member `{name}`")));
        };
        if fields.contains_key(&name) {
            return Err(protocol_error(format!("wire `structure` repeats member `{name}`")));
        }
        fields.insert(name, read_named_value(body, offset, Some(&declared_field.ty), directory, nested)?);
    }
    for field in declared {
        fields.entry(field.name.clone()).or_insert(Value::Null);
    }
    Ok(Value::Row(fields))
}

fn read_wire_unite(body: &[u8], offset: &mut usize, directory: &WireNamedDirectory, depth: u8) -> Result<Value> {
    let schema_id = read_u64(body, offset).map_err(protocol_from_read)?;
    let revision = read_u32(body, offset).map_err(protocol_from_read)?;
    let discriminant = read_i64(body, offset).map_err(protocol_from_read)?;
    let member_count = read_u16(body, offset).map_err(protocol_from_read)? as usize;
    let declaration = directory
        .declaration(schema_id)
        .ok_or_else(|| protocol_error("wire record names a `unite` schema id that the directory does not declare"))?;
    let WireNamedLayout::Unite { revision: expected, variants, .. } = declaration
    else {
        return Err(protocol_error("wire record names a schema id that is not a `unite` declaration"));
    };
    if *expected != revision {
        return Err(protocol_error("wire record uses a `unite` layout revision the directory does not declare"));
    }
    let variant = variants
        .iter()
        .find(|variant| variant.discriminant == discriminant)
        .ok_or_else(|| protocol_error("wire record names a `unite` variant the declaration does not contain"))?;
    if variant.fields.len() != member_count {
        return Err(protocol_error("wire record `unite` payload member count disagrees with its variant"));
    }
    let nested = next_named_depth(depth)?;
    let mut payload = std::collections::BTreeMap::new();
    for _ in 0..member_count {
        let name = read_name(body, offset)?;
        let Some(declared_field) = variant.fields.iter().find(|field| field.name == name)
        else {
            return Err(protocol_error(format!("wire `unite` variant `{discriminant}` does not declare payload member `{name}`")));
        };
        if payload.contains_key(&name) {
            return Err(protocol_error(format!("wire `unite` variant `{discriminant}` repeats payload member `{name}`")));
        }
        payload.insert(name, read_named_value(body, offset, Some(&declared_field.ty), directory, nested)?);
    }
    Ok(Value::List(vec![Value::I64(discriminant), Value::Row(payload)]))
}

fn next_named_depth(depth: u8) -> Result<u8> {
    depth
        .checked_add(1)
        .filter(|nested| *nested <= MAX_WIRE_VALUE_DEPTH)
        .ok_or_else(|| protocol_error("wire value nests deeper than the supported depth"))
}

// ─── 事务帧 ────────────────────────────────────────────────────────────────

/// 编码保存点名（`TxnSavepoint` / `TxnRollbackTo` / `TxnRelease` 的 body）。
pub fn encode_savepoint_name(name: &str) -> Vec<u8> {
    let mut body = Vec::new();
    push_bytes(&mut body, name.as_bytes());
    body
}

/// 解码保存点名。
pub fn decode_savepoint_name(body: &[u8]) -> Result<String> {
    let mut offset = 0;
    let len = read_u32(body, &mut offset)?;
    let bytes = read_bytes(body, &mut offset, len)?;
    std::str::from_utf8(bytes).map_err(|_| Error::Corrupt("savepoint name is not utf-8")).map(|name| name.to_owned())
}

// ─── 会话关闭 ──────────────────────────────────────────────────────────────

/// 编码 `SessionClose` 的可选原因文本。
///
/// 原因只用于诊断；服务端按帧本身释放会话与租约，不解析原因做分支。
pub fn encode_session_close(reason: Option<&str>) -> Vec<u8> {
    let mut body = Vec::new();
    push_bytes(&mut body, reason.unwrap_or_default().as_bytes());
    body
}

/// 解码 `SessionClose` 原因（空文本视为 `None`）。
pub fn decode_session_close(body: &[u8]) -> Result<Option<String>> {
    let mut offset = 0;
    let len = read_u32(body, &mut offset)?;
    let bytes = read_bytes(body, &mut offset, len)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    std::str::from_utf8(bytes).map(|reason| Some(reason.to_owned())).map_err(|_| Error::Corrupt("session close reason is not utf-8"))
}

/// 把 `host:port` / `[::1]:port` 拆成主机与端口。
pub fn split_bind(bind: &str) -> Result<(String, u16)> {
    let bind = bind.trim();
    if let Some(rest) = bind.strip_prefix('[') {
        let (host, port) = rest.split_once("]:").ok_or(Error::Unsupported("bind must look like [host]:port"))?;
        let port: u16 = port.parse().map_err(|_| Error::Unsupported("bind port is not a u16"))?;
        return Ok((host.to_owned(), port));
    }
    let (host, port) = bind.rsplit_once(':').ok_or(Error::Unsupported("bind must look like host:port"))?;
    let port: u16 = port.parse().map_err(|_| Error::Unsupported("bind port is not a u16"))?;
    Ok((host.to_owned(), port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_request_maps_to_its_ok_response_and_back() {
        let requests = [
            MessageType::Hello,
            MessageType::Info,
            MessageType::SchemaGet,
            MessageType::SchemaEnsure,
            MessageType::KvGet,
            MessageType::KvPut,
            MessageType::MicroRegister,
            MessageType::MicroHostInvoke,
            MessageType::ScalarCall,
            MessageType::Authenticate,
            MessageType::Prepare,
            MessageType::ExecutePrepared,
            MessageType::TxnBegin,
            MessageType::TxnCommit,
            MessageType::TxnRollback,
            MessageType::TxnSavepoint,
            MessageType::TxnRollbackTo,
            MessageType::TxnRelease,
            MessageType::PerfReport,
            MessageType::SessionClose,
        ];
        for request in requests {
            let response = response_type(request).unwrap_or_else(|| panic!("{request:?} must have a response type"));
            assert_eq!(response.as_u16(), request.as_u16() + 1, "v0 pairs each request with the next code");
            assert!(response_type(response).is_none(), "a response type is not itself a request");
            assert_eq!(MessageType::from_u16(response.as_u16()), response);
        }
    }

    #[test]
    fn error_and_unknown_are_not_requests() {
        assert!(response_type(MessageType::Error).is_none());
        assert!(response_type(MessageType::Unknown(99)).is_none());
        assert!(response_type(MessageType::Unknown(2)).is_none());
    }

    #[test]
    fn authenticate_ok_token_roundtrips() {
        let body = encode_authenticate_ok("session-token-42");
        assert_eq!(decode_authenticate_ok(&body).expect("decode"), "session-token-42");
        assert!(decode_authenticate_ok(&[1, 0, 0]).is_err());
    }

    #[test]
    fn session_close_reason_roundtrips_and_empty_is_none() {
        assert_eq!(decode_session_close(&encode_session_close(Some("client closed"))).expect("decode"), Some("client closed".to_owned()));
        assert_eq!(decode_session_close(&encode_session_close(None)).expect("decode"), None);
        assert_eq!(decode_session_close(&[]).expect_err("truncated body").code(), "database.corrupt");
    }

    #[test]
    fn typed_values_roundtrip_natively() {
        let uuid = uuid::Uuid::nil();
        let decimal = database_types::Decimal::new(-123_456, 20, 6).expect("decimal");
        let timestamp = database_types::Timestamp::from_unix_micros(1_700_000_000_123_456).expect("timestamp");
        let date = database_types::Date::from_days(19_000).expect("date");
        let time = database_types::Time::from_nanos(3_600_000_000_001).expect("time");
        let values = vec![
            Value::Null,
            Value::Bool(true),
            Value::I64(-7),
            Value::U64(u64::MAX),
            Value::F64(1.5),
            Value::F64(f64::NAN),
            Value::Text("文本".to_owned()),
            Value::Bytes(vec![0, 1, 255]),
            Value::Uuid(uuid),
            Value::Decimal(decimal),
            Value::Timestamp(timestamp),
            Value::Date(date),
            Value::Time(time),
            Value::List(vec![Value::I64(1), Value::Text("a".to_owned())]),
            Value::Row(std::collections::BTreeMap::from([("k".to_owned(), Value::Bool(false))])),
        ];
        for value in values {
            let encoded = encode_value(&value).expect("encode");
            let decoded = decode_value(&encoded).expect("decode");
            match (&value, &decoded) {
                (Value::F64(left), Value::F64(right)) if left.is_nan() => assert!(right.is_nan()),
                _ => assert_eq!(decoded, value),
            }
        }
    }

    #[test]
    fn params_and_rows_roundtrip_with_named_columns() {
        let params = std::collections::BTreeMap::from([
            ("id".to_owned(), Value::Uuid(uuid::Uuid::nil())),
            ("amount".to_owned(), Value::Decimal(database_types::Decimal::new(1_000, 20, 2).expect("decimal"))),
            ("note".to_owned(), Value::Text("ok".to_owned())),
        ]);
        let encoded = encode_params(&params).expect("encode params");
        assert_eq!(decode_params(&encoded).expect("decode params"), params);

        let rows = vec![std::collections::BTreeMap::from([
            ("id".to_owned(), Value::Uuid(uuid::Uuid::nil())),
            ("count".to_owned(), Value::I64(3)),
        ])];
        let encoded = encode_rows(&rows).expect("encode rows");
        assert_eq!(decode_rows(&encoded).expect("decode rows"), rows);
    }

    #[test]
    fn wire_values_reject_trailing_bytes_and_unknown_tags() {
        let mut encoded = encode_value(&Value::I64(7)).expect("encode");
        encoded.push(0);
        assert!(decode_value(&encoded).is_err());
        assert!(decode_value(&[0xff]).is_err());
    }

    const ADDRESS_ID: u64 = 51;
    const CONTACT_ID: u64 = 52;
    const PROFILE_ID: u64 = 53;

    fn named_directory() -> WireNamedDirectory {
        let declarations = std::collections::BTreeMap::from([
            (
                ADDRESS_ID,
                WireNamedLayout::Structure {
                    schema_id: ADDRESS_ID,
                    revision: 4,
                    fields: vec![
                        WireNamedField { name: "zip".to_owned(), ty: WireValueLayout::Dynamic },
                        WireNamedField { name: "country".to_owned(), ty: WireValueLayout::Dynamic },
                    ],
                },
            ),
            (
                CONTACT_ID,
                WireNamedLayout::Unite {
                    schema_id: CONTACT_ID,
                    revision: 9,
                    variants: vec![
                        WireNamedVariant {
                            discriminant: 0,
                            fields: vec![
                                WireNamedField { name: "address".to_owned(), ty: WireValueLayout::Dynamic },
                                WireNamedField { name: "label".to_owned(), ty: WireValueLayout::Dynamic },
                            ],
                        },
                        WireNamedVariant { discriminant: 1, fields: Vec::new() },
                    ],
                },
            ),
            (
                PROFILE_ID,
                WireNamedLayout::Class {
                    schema_id: PROFILE_ID,
                    fields: vec![
                        WireNamedField { name: "name".to_owned(), ty: WireValueLayout::Dynamic },
                        WireNamedField { name: "age".to_owned(), ty: WireValueLayout::Dynamic },
                        WireNamedField { name: "bio".to_owned(), ty: WireValueLayout::Dynamic },
                    ],
                },
            ),
        ]);
        let columns = std::collections::BTreeMap::from([
            ("address".to_owned(), WireValueLayout::Named(ADDRESS_ID)),
            ("contact".to_owned(), WireValueLayout::Named(CONTACT_ID)),
            ("profile".to_owned(), WireValueLayout::Named(PROFILE_ID)),
        ]);
        WireNamedDirectory::new(columns, declarations)
    }

    fn named_row() -> std::collections::BTreeMap<String, Value> {
        std::collections::BTreeMap::from([
            (
                "address".to_owned(),
                Value::Row(std::collections::BTreeMap::from([
                    ("zip".to_owned(), Value::Text("100000".to_owned())),
                    ("country".to_owned(), Value::Null),
                ])),
            ),
            (
                "contact".to_owned(),
                Value::List(vec![
                    Value::I64(0),
                    Value::Row(std::collections::BTreeMap::from([
                        ("address".to_owned(), Value::Text("ada@example.com".to_owned())),
                        ("label".to_owned(), Value::Null),
                    ])),
                ]),
            ),
            (
                "profile".to_owned(),
                Value::Row(std::collections::BTreeMap::from([
                    ("name".to_owned(), Value::Text("Ada".to_owned())),
                    ("age".to_owned(), Value::I64(36)),
                    ("bio".to_owned(), Value::Null),
                ])),
            ),
        ])
    }

    #[test]
    fn prepared_results_carry_class_structure_and_union_values_across_the_frame() {
        let directory = named_directory();
        let rows = vec![named_row()];
        let rows_body = encode_named_rows(&rows, &directory).expect("encode named rows");
        let frame = encode_named_execute_prepared_ok(&directory, &rows_body).expect("encode result frame");

        assert_eq!(frame[0], OK_ROWS_NAMED, "the result frame declares the named projection");
        match decode_execute_prepared_ok(&frame).expect("decode result frame") {
            ExecutePreparedRows::Named { directory: decoded, rows: body } => {
                assert_eq!(decoded, directory, "the frame carries the directory the server projected with");
                assert_eq!(decode_named_rows(&body, &decoded).expect("decode named rows"), rows);
            }
            other => panic!("expected the named projection, got {other:?}"),
        }
        let directory_bytes = encode_named_directory(&directory).expect("encode directory");
        assert_eq!(decode_named_directory(&directory_bytes).expect("decode directory"), directory);
    }

    #[test]
    fn the_result_frame_rejects_unknown_projections_and_truncation_as_protocol_errors() {
        let directory = named_directory();
        let rows_body = encode_named_rows(&[named_row()], &directory).expect("encode named rows");
        let frame = encode_named_execute_prepared_ok(&directory, &rows_body).expect("encode result frame");

        assert_eq!(decode_execute_prepared_ok(&[9, 0, 0, 0, 0]).expect_err("unknown projection").code(), "database.protocol");

        let mut unknown_directory_version = frame.clone();
        unknown_directory_version[5] = NAMED_DIRECTORY_WIRE_VERSION + 1;
        assert_eq!(
            decode_execute_prepared_ok(&unknown_directory_version).expect_err("unknown directory version").code(),
            "database.protocol"
        );

        let truncated = &frame[..frame.len() - 1];
        assert_eq!(decode_execute_prepared_ok(truncated).expect_err("truncated frame").code(), "database.protocol");
    }

    #[test]
    fn the_class_projection_rejects_unknown_tags_and_payload_mismatches() {
        let directory = named_directory();

        // An unknown wire tag must not fall through to the storage or plain codec.
        assert_eq!(decode_named_value(&[NAMED_VALUE_WIRE_VERSION, 0x7f], &directory).expect_err("unknown tag").code(), "database.protocol");

        // A discriminant that names a variant with another payload shape is refused.
        let body = encode_named_row(&named_row(), &directory).expect("encode named row");
        let unite_at = body.iter().position(|byte| *byte == WIRE_TAG_NAMED_UNITE).expect("unite value");
        let mut wrong_variant = body.clone();
        wrong_variant[unite_at + 1 + 8 + 4] = 1;
        assert_eq!(
            decode_named_row(&wrong_variant, &directory).expect_err("discriminant/payload mismatch").code(),
            "database.protocol"
        );
    }

    #[test]
    fn named_rows_project_structures_and_unions_with_wire_tags() {
        let directory = named_directory();
        let row = named_row();
        let body = encode_named_rows(&[row.clone()], &directory).expect("encode named rows");
        assert_eq!(body[0], NAMED_VALUE_WIRE_VERSION, "the wire projection has its own version byte");
        // version + row count + column count + "address" length + the name.
        assert_eq!(body[20], WIRE_TAG_NAMED_STRUCTURE, "a structure uses the wire tag at its value position");
        assert!(body.contains(&WIRE_TAG_NAMED_UNITE), "a union uses the wire tag at its value position");
        assert_eq!(decode_named_rows(&body, &directory).expect("decode named rows"), vec![row]);
    }

    #[test]
    fn named_projection_rejects_unknown_versions_and_storage_tags_as_protocol_errors() {
        let directory = named_directory();
        let body = encode_named_row(&named_row(), &directory).expect("encode named row");

        let mut newer = body.clone();
        newer[0] = NAMED_VALUE_WIRE_VERSION + 1;
        assert_eq!(decode_named_row(&newer, &directory).expect_err("unknown version").code(), "database.protocol");

        // A storage-positional value tag is not a wire tag: version + column
        // count + "address" length + the name, then the value tag.
        let mut storage_tag = body.clone();
        storage_tag[16] = 0x12;
        assert_eq!(decode_named_row(&storage_tag, &directory).expect_err("storage tag").code(), "database.protocol");

        let truncated = &body[..body.len() - 1];
        assert_eq!(decode_named_row(truncated, &directory).expect_err("truncated frame").code(), "database.protocol");

        // The diagnostic for a protocol failure stays a protocol code, not storage.
        let wire = decode_error(&encode_error_body(&Error::Protocol { message: "bad frame".to_owned() })).expect("error body");
        assert_eq!(wire.code, "database.protocol");
    }

    #[test]
    fn named_projection_rejects_revision_and_variant_mismatches() {
        let renamed_revision = changed_revision(named_directory(), 10);
        let row = encode_named_row(&named_row(), &named_directory()).expect("encode named row");
        assert_eq!(
            decode_named_row(&row, &renamed_revision).expect_err("unknown revision").code(),
            "database.protocol"
        );

        // A union discriminant that the declaration does not contain is refused.
        let mut unknown_variant = named_row();
        unknown_variant.insert(
            "contact".to_owned(),
            Value::List(vec![Value::I64(7), Value::Row(std::collections::BTreeMap::new())]),
        );
        assert_eq!(
            encode_named_row(&unknown_variant, &named_directory()).expect_err("unknown discriminant").code(),
            "database.protocol"
        );

        // A single value without a column layout stays a plain dynamic value; the
        // named projection is only applied where a declaration is in scope.
        let plain = encode_named_value(&Value::Text("ok".to_owned()), &named_directory()).expect("encode plain value");
        assert_eq!(decode_named_value(&plain, &named_directory()).expect("decode plain value"), Value::Text("ok".to_owned()));
    }

    fn changed_revision(directory: WireNamedDirectory, revision: u32) -> WireNamedDirectory {
        let mut declarations = std::collections::BTreeMap::new();
        declarations.insert(
            ADDRESS_ID,
            WireNamedLayout::Structure {
                schema_id: ADDRESS_ID,
                revision,
                fields: vec![
                    WireNamedField { name: "zip".to_owned(), ty: WireValueLayout::Dynamic },
                    WireNamedField { name: "country".to_owned(), ty: WireValueLayout::Dynamic },
                ],
            },
        );
        declarations.insert(
            CONTACT_ID,
            match directory.declaration(CONTACT_ID).expect("contact declaration") {
                WireNamedLayout::Unite { schema_id, revision, variants } => WireNamedLayout::Unite {
                    schema_id: *schema_id,
                    revision: *revision,
                    variants: variants.clone(),
                },
                other => panic!("expected a union declaration, got {other:?}"),
            },
        );
        declarations.insert(
            PROFILE_ID,
            match directory.declaration(PROFILE_ID).expect("profile declaration") {
                WireNamedLayout::Class { schema_id, fields } => {
                    WireNamedLayout::Class { schema_id: *schema_id, fields: fields.clone() }
                }
                other => panic!("expected a class declaration, got {other:?}"),
            },
        );
        WireNamedDirectory::new(
            std::collections::BTreeMap::from([
                ("address".to_owned(), WireValueLayout::Named(ADDRESS_ID)),
                ("contact".to_owned(), WireValueLayout::Named(CONTACT_ID)),
                ("profile".to_owned(), WireValueLayout::Named(PROFILE_ID)),
            ]),
            declarations,
        )
    }
}
