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
    /// ExecutePrepared 回复（body：JSON 行，形状同 KvGetOk 值）。
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

/// 编码 `ExecutePrepared` 请求 body：statement_id + 可选 JSON 参数。
///
/// 语句无参数时 `params_json` 可为 `None` 或空串。
pub fn encode_execute_prepared(statement_id: &str, params_json: Option<&str>) -> Vec<u8> {
    let mut body = Vec::new();
    push_bytes(&mut body, statement_id.as_bytes());
    match params_json {
        Some(p) if !p.is_empty() => push_bytes(&mut body, p.as_bytes()),
        _ => push_u32(&mut body, 0),
    }
    body
}

/// 解码 `ExecutePrepared` 请求 body → (statement_id, 可选 JSON 参数)。
pub fn decode_execute_prepared(body: &[u8]) -> Result<(String, Option<String>)> {
    let mut offset = 0;
    let id_len = read_u32(body, &mut offset)?;
    let id_bytes = read_bytes(body, &mut offset, id_len)?;
    let statement_id = std::str::from_utf8(id_bytes).map_err(|_| Error::Corrupt("execute prepared statement_id is not utf-8"))?.to_owned();
    let params_len = read_u32(body, &mut offset)?;
    let params = if params_len > 0 {
        let bytes = read_bytes(body, &mut offset, params_len)?;
        let text = std::str::from_utf8(bytes).map_err(|_| Error::Corrupt("execute prepared params is not utf-8"))?.to_owned();
        Some(text)
    }
    else {
        None
    };
    Ok((statement_id, params))
}

/// 编码 `ExecutePreparedOk` 回复 body —— 原始 JSON 行。
pub fn encode_execute_prepared_ok(rows_json: &str) -> Vec<u8> {
    let mut body = Vec::new();
    push_bytes(&mut body, rows_json.as_bytes());
    body
}

/// 解码 `ExecutePreparedOk` 回复 body。
pub fn decode_execute_prepared_ok(body: &[u8]) -> Result<String> {
    let mut offset = 0;
    let len = read_u32(body, &mut offset)?;
    let bytes = read_bytes(body, &mut offset, len)?;
    std::str::from_utf8(bytes).map_err(|_| Error::Corrupt("execute prepared ok rows is not utf-8")).map(|s| s.to_owned())
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

// ─── 带标签的值 JSON（客户端与服务端共用） ────────────────────────────────

/// 把运行时值编码成可逆 JSON。
///
/// 标量沿用 JSON 原生形状（`null` / 布尔 / 数字 / 字符串），其余类型用单键标签
/// 对象表达：`{"$bytes":"<hex>"}`、`{"$uuid":"…"}`、`{"$decimal":["<unscaled>",p,s]}`、
/// `{"$timestamp":micros}`、`{"$date":days}`、`{"$time":nanos}`、`{"$u64":"…"}`、
/// `{"$f64":"…"}`（仅非有限值）、`{"$list":[…]}`、`{"$row":{…}}`。
pub fn value_to_json(value: &Value) -> Result<serde_json::Value> {
    use serde_json::{Value as Json, json};
    Ok(match value {
        Value::Null => Json::Null,
        Value::Bool(value) => Json::Bool(*value),
        Value::I64(value) => Json::from(*value),
        Value::U64(value) => match i64::try_from(*value) {
            Ok(value) => Json::from(value),
            Err(_) => json!({ "$u64": value.to_string() }),
        },
        Value::F64(value) => match serde_json::Number::from_f64(*value) {
            Some(number) => Json::Number(number),
            None => json!({ "$f64": value.to_string() }),
        },
        Value::Text(value) => Json::String(value.clone()),
        Value::Bytes(bytes) => json!({ "$bytes": hex_encode(bytes) }),
        Value::Uuid(value) => json!({ "$uuid": value.to_string() }),
        Value::Date(value) => json!({ "$date": value.days() }),
        Value::Time(value) => json!({ "$time": value.nanos() }),
        Value::Timestamp(value) => json!({ "$timestamp": value.unix_micros() }),
        Value::Decimal(value) => json!({ "$decimal": [value.unscaled().to_string(), value.precision(), value.scale()] }),
        Value::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(value_to_json(item)?);
            }
            json!({ "$list": out })
        }
        Value::Row(fields) => {
            let mut out = serde_json::Map::with_capacity(fields.len());
            for (key, value) in fields {
                out.insert(key.clone(), value_to_json(value)?);
            }
            json!({ "$row": Json::Object(out) })
        }
        other => {
            return Err(Error::Protocol { message: format!("value kind `{}` has no wire JSON encoding", other.kind()) });
        }
    })
}

/// [`value_to_json`] 的逆变换；未带标签的对象解成 [`Value::Row`]，数组解成列表。
pub fn json_to_value(value: &serde_json::Value) -> Result<Value> {
    use serde_json::Value as Json;
    match value {
        Json::Null => Ok(Value::Null),
        Json::Bool(value) => Ok(Value::Bool(*value)),
        Json::Number(number) => {
            if let Some(value) = number.as_i64() {
                Ok(Value::I64(value))
            }
            else if let Some(value) = number.as_u64() {
                Ok(Value::U64(value))
            }
            else if let Some(value) = number.as_f64() {
                Ok(Value::F64(value))
            }
            else {
                Err(Error::Corrupt("wire value has an unrepresentable JSON number"))
            }
        }
        Json::String(value) => Ok(Value::Text(value.clone())),
        Json::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(json_to_value(item)?);
            }
            Ok(Value::List(out))
        }
        Json::Object(fields) => {
            if let Some((tag, payload)) = single_tag(fields) {
                return tagged_to_value(tag, payload);
            }
            let mut out = std::collections::BTreeMap::new();
            for (key, value) in fields {
                out.insert(key.clone(), json_to_value(value)?);
            }
            Ok(Value::Row(out))
        }
    }
}

fn single_tag(fields: &serde_json::Map<String, serde_json::Value>) -> Option<(&str, &serde_json::Value)> {
    if fields.len() != 1 {
        return None;
    }
    let (tag, payload) = fields.iter().next()?;
    tag.strip_prefix('$').map(|tag| (tag, payload))
}

fn tagged_to_value(tag: &str, payload: &serde_json::Value) -> Result<Value> {
    let message = |detail: String| Error::Protocol { message: format!("wire value `${{{tag}}}`: {detail}") };
    match tag {
        "u64" => payload
            .as_str()
            .and_then(|text| text.parse::<u64>().ok())
            .map(Value::U64)
            .ok_or_else(|| message("expected a u64 string".to_owned())),
        "f64" => payload
            .as_str()
            .and_then(|text| text.parse::<f64>().ok())
            .map(Value::F64)
            .ok_or_else(|| message("expected an f64 string".to_owned())),
        "bytes" => payload
            .as_str()
            .and_then(hex_decode)
            .map(Value::Bytes)
            .ok_or_else(|| message("expected lowercase hex".to_owned())),
        "uuid" => payload
            .as_str()
            .and_then(|text| uuid::Uuid::parse_str(text).ok())
            .map(Value::Uuid)
            .ok_or_else(|| message("expected a uuid string".to_owned())),
        "date" => payload
            .as_i64()
            .and_then(|days| i32::try_from(days).ok())
            .and_then(|days| database_types::Date::from_days(days).ok())
            .map(Value::Date)
            .ok_or_else(|| message("expected epoch days".to_owned())),
        "time" => payload
            .as_i64()
            .and_then(|nanos| database_types::Time::from_nanos(nanos).ok())
            .map(Value::Time)
            .ok_or_else(|| message("expected nanoseconds".to_owned())),
        "timestamp" => payload
            .as_i64()
            .and_then(|micros| database_types::Timestamp::from_unix_micros(micros).ok())
            .map(Value::Timestamp)
            .ok_or_else(|| message("expected unix micros".to_owned())),
        "decimal" => {
            let parts = payload.as_array().filter(|parts| parts.len() == 3).ok_or_else(|| message("expected [unscaled, precision, scale]".to_owned()))?;
            let unscaled = json_scalar_text(&parts[0]).and_then(|text| text.parse::<i128>().ok());
            let precision = parts[1].as_u64().and_then(|value| u8::try_from(value).ok());
            let scale = parts[2].as_u64().and_then(|value| u8::try_from(value).ok());
            match (unscaled, precision, scale) {
                (Some(unscaled), Some(precision), Some(scale)) => {
                    database_types::Decimal::new(unscaled, precision, scale).map(Value::Decimal).map_err(|error| message(error.to_string()))
                }
                _ => Err(message("expected [unscaled, precision, scale]".to_owned())),
            }
        }
        "list" => {
            let items = payload.as_array().ok_or_else(|| message("expected an array".to_owned()))?;
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(json_to_value(item)?);
            }
            Ok(Value::List(out))
        }
        "row" => match json_to_value(payload)? {
            Value::Row(fields) => Ok(Value::Row(fields)),
            _ => Err(message("expected an object".to_owned())),
        },
        _ => Err(Error::Protocol { message: format!("unknown wire value tag `${tag}`") }),
    }
}

fn json_scalar_text(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX_DIGITS[(byte >> 4) as usize] as char);
        out.push(HEX_DIGITS[(byte & 0xf) as usize] as char);
    }
    out
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    let bytes = text.as_bytes();
    let digit = |byte: u8| -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        }
    };
    let mut out = Vec::with_capacity(text.len() / 2);
    for pair in bytes.chunks_exact(2) {
        out.push((digit(pair[0])? << 4) | digit(pair[1])?);
    }
    Some(out)
}

/// 把具名参数编码成 `ExecutePrepared` 的 JSON body。
pub fn encode_params(params: &std::collections::BTreeMap<String, Value>) -> Result<String> {
    let mut out = serde_json::Map::with_capacity(params.len());
    for (key, value) in params {
        out.insert(key.clone(), value_to_json(value)?);
    }
    serde_json::to_string(&serde_json::Value::Object(out)).map_err(|_| Error::Corrupt("wire params JSON encoding failed"))
}

/// 解码 [`encode_params`] 产出的 JSON。
pub fn decode_params(text: &str) -> Result<std::collections::BTreeMap<String, Value>> {
    let parsed: serde_json::Value =
        serde_json::from_str(text).map_err(|_| Error::QueryBind { parameter: String::new(), message: "params must be valid JSON".to_owned() })?;
    let object = parsed
        .as_object()
        .ok_or_else(|| Error::QueryBind { parameter: String::new(), message: "params must be a JSON object".to_owned() })?;
    let mut out = std::collections::BTreeMap::new();
    for (key, value) in object {
        out.insert(key.clone(), json_to_value(value)?);
    }
    Ok(out)
}

/// 把查询行编码成 `ExecutePreparedOk` / `PerfReportOk` 的 JSON body。
pub fn encode_rows(rows: &[std::collections::BTreeMap<String, Value>]) -> Result<String> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(value_to_json(&Value::Row(row.clone()))?);
    }
    serde_json::to_string(&serde_json::Value::Array(out)).map_err(|_| Error::Corrupt("wire rows JSON encoding failed"))
}

/// 解码 [`encode_rows`] 产出的 JSON。
pub fn decode_rows(text: &str) -> Result<Vec<std::collections::BTreeMap<String, Value>>> {
    let parsed: serde_json::Value = serde_json::from_str(text).map_err(|_| Error::Corrupt("wire rows body is not valid JSON"))?;
    let items = parsed.as_array().ok_or_else(|| Error::Corrupt("wire rows body must be an array"))?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        match json_to_value(item)? {
            Value::Row(fields) => out.push(fields),
            _ => return Err(Error::Corrupt("wire row must be a JSON object")),
        }
    }
    Ok(out)
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
    fn typed_values_roundtrip_through_json() {
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
            let encoded = value_to_json(&value).expect("encode");
            let decoded = json_to_value(&encoded).expect("decode");
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
    fn untagged_objects_decode_as_rows_and_arrays_as_lists() {
        let row = json_to_value(&serde_json::json!({"a": 1, "b": [true, null]})).expect("decode");
        let Value::Row(fields) = row
        else {
            panic!("plain object must decode as a row");
        };
        assert_eq!(fields["a"], Value::I64(1));
        assert_eq!(fields["b"], Value::List(vec![Value::Bool(true), Value::Null]));
        assert!(json_to_value(&serde_json::json!({"$nope": 1})).is_err());
    }
}
