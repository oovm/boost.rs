//! MongoDB 线协议原语。
//!
//! 读取本栈应答的两类帧：普通命令用 OP_MSG；驱动在切换到 OP_MSG 之前
//! 仍会发的遗留握手用 OP_QUERY。载荷（body）与 BSON 文档之间的转换也在
//! 本模块，组帧信息只留在调用方。
//!
//! 流式 API（[`read_frame`] / [`write_frame`]）供本仓服务端直接使用；
//! Buffer 形态 API（[`peek_header`] / [`decode_frame`] / [`encode_msg_frame`]
//! 等）供宿主绑定（如 N-API）把整帧字节交给协议层，包括 `OP_COMPRESSED`
//! 封套的展开与压缩器分派。压缩算法本身归 `boost.data.compression`，
//! Zlib（id 2）由宿主回调提供。

use std::io::{self, Read, Write};

use crate::bson::{BsonDocument, BsonError, BsonValue, encode_bson, parse_bson};

/// 现代（3.6+）消息组帧的 MongoDB 线协议操作码。
pub const OP_MSG: i32 = 2013;

/// 压缩封套操作码。载荷是 9 字节封套加压缩后的另一条消息。
pub const OP_COMPRESSED: i32 = 2012;

/// 遗留查询操作码。真实驱动用它做首次 `isMaster` / `hello` 探测，
/// 在确认对端支持 OP_MSG 之前发送。
pub const OP_QUERY: i32 = 2004;

/// 遗留回复操作码，对应 [`OP_QUERY`] 的应答形状。
pub const OP_REPLY: i32 = 1;

/// `OP_COMPRESSED` 压缩器 id：Snappy block 格式（线协议规范）。
pub const COMPRESSOR_SNAPPY: u8 = 1;

/// `OP_COMPRESSED` 压缩器 id：zlib（由宿主回调解压）。
pub const COMPRESSOR_ZLIB: u8 = 2;

/// `OP_COMPRESSED` 压缩器 id：Zstandard（线协议规范）。
pub const COMPRESSOR_ZSTD: u8 = 3;

/// 检查载荷前施加于入站帧的上限。
#[derive(Debug, Clone, Copy)]
pub struct FrameLimits {
    /// 单个客户端可接受的最大整帧字节数。
    pub max_frame_size: u32,
}

impl Default for FrameLimits {
    fn default() -> Self {
        Self { max_frame_size: 48 * 1024 * 1024 }
    }
}

/// 已解析的 OP_MSG 头：从线上读出的长度与操作码字段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MsgHeader {
    /// 整帧长度，含 4 字节长度前缀自身。
    pub total_length: u32,
    /// 客户端请求标识，回复时原样回显。
    pub request_id: i32,
    /// 应答对象标识（客户端请求时为 0）。
    pub response_to: i32,
    /// 从帧头读出的线协议操作码。
    pub opcode: i32,
}

/// 一条入站 MongoDB 帧：头 + 原始 body 字节。
#[derive(Debug, Clone)]
pub struct Frame {
    /// 从线上解码的头字段。
    pub header: MsgHeader,
    /// 16 字节头之后的载荷；PING 类消息可为空。
    pub body: Vec<u8>,
}

/// `OP_COMPRESSED` 的 9 字节封套字段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompressionEnvelope {
    /// 压缩器 id（[`COMPRESSOR_SNAPPY`] / [`COMPRESSOR_ZLIB`] / [`COMPRESSOR_ZSTD`]）。
    pub compressor_id: u8,
    /// 封套内原始消息的操作码；本栈只接受 OP_MSG。
    pub original_opcode: i32,
    /// 封套声明的解压后字节数，解压结果必须与之精确一致。
    pub uncompressed_size: i32,
}

/// 一条完成展开的入站帧：头 + 解封套、解压后的载荷。
#[derive(Debug, Clone)]
pub struct InboundFrame {
    /// 从线上解码的头字段。
    pub header: MsgHeader,
    /// 服务端需要应答的操作码；`OP_COMPRESSED` 展开后为 OP_MSG。
    pub opcode: i32,
    /// 解压后的载荷；OP_MSG 时为 flags 加 sections。
    pub payload: Vec<u8>,
    /// 压缩帧的封套字段；未压缩帧为 `None`。
    pub envelope: Option<CompressionEnvelope>,
}

/// 帧解码专用错误。
#[derive(Debug)]
pub enum FrameError {
    /// 完整头到达前 TCP 流干净关闭。
    ConnectionClosed,
    /// 长度前缀超过配置上限。
    FrameTooLarge {
        /// 客户端声明的帧长度。
        announced: u32,
        /// 本栈接受的最大长度。
        limit: u32,
    },
    /// 帧使用了本栈不应答的操作码。
    UnsupportedOpcode {
        /// 从帧头读出的操作码。
        opcode: i32,
    },
    /// Buffer 形态解码时字节数不足 16 字节头。
    HeaderTruncated,
    /// 头里的 `total_length` 与实际收到的字节数不一致。
    LengthMismatch {
        /// 帧头声明的整帧长度。
        announced: u32,
        /// 实际收到的字节数。
        actual: usize,
    },
    /// `OP_COMPRESSED` 载荷不足 9 字节封套。
    EnvelopeTruncated,
    /// `OP_COMPRESSED` 封套包裹了 OP_MSG 之外的操作码。
    CompressedOpcodeNotAllowed {
        /// 封套声明的被包裹操作码。
        opcode: i32,
    },
    /// 封套声明的解压尺寸非正或越出上限。
    InvalidAnnouncedSize {
        /// 封套里的 `uncompressedSize` 原值。
        announced: i32,
    },
    /// 封套使用了未知压缩器 id。
    UnknownCompressor {
        /// 帧里读出的压缩器 id。
        compressor_id: u8,
    },
    /// 解压缩失败或产出与封套声明不一致。
    DecompressionFailed {
        /// 失败时使用的压缩器 id。
        compressor_id: u8,
        /// 底层解码器的可读错误文本。
        message: String,
    },
    /// 读字节时的底层 I/O 失败。
    Io(io::Error),
}

impl FrameError {
    /// 供日志与线回复使用的可读帧错误文本。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ConnectionClosed => "mongo frame: connection closed",
            Self::FrameTooLarge { .. } => "mongo frame: frame too large",
            Self::UnsupportedOpcode { .. } => "mongo frame: unsupported opcode",
            Self::HeaderTruncated => "mongo frame: shorter than its header",
            Self::LengthMismatch { .. } => "mongo frame: length does not match its header",
            Self::EnvelopeTruncated => "mongo frame: OP_COMPRESSED missing its envelope",
            Self::CompressedOpcodeNotAllowed { .. } => "mongo frame: OP_COMPRESSED must wrap OP_MSG",
            Self::InvalidAnnouncedSize { .. } => "mongo frame: OP_COMPRESSED announced an out of range size",
            Self::UnknownCompressor { .. } => "mongo frame: unknown compressor id",
            Self::DecompressionFailed { .. } => "mongo frame: decompression failed",
            Self::Io(_) => "mongo frame: i/o error",
        }
    }
}

impl From<io::Error> for FrameError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

/// Reads one MongoDB frame from the given stream using the provided limits.
///
/// Returns `None` on a clean connection close before any bytes arrive.
pub fn read_frame<R>(stream: &mut R, limits: FrameLimits) -> Result<Option<Frame>, FrameError>
where
    R: Read,
{
    let mut header_buf = [0_u8; 16];
    read_exact(stream, &mut header_buf)?;

    let total_length = u32::from_le_bytes([header_buf[0], header_buf[1], header_buf[2], header_buf[3]]);
    if total_length < 16 {
        return Err(FrameError::FrameTooLarge { announced: total_length, limit: 0 });
    }
    if total_length > limits.max_frame_size {
        return Err(FrameError::FrameTooLarge { announced: total_length, limit: limits.max_frame_size });
    }

    let request_id = i32::from_le_bytes([header_buf[4], header_buf[5], header_buf[6], header_buf[7]]);
    let response_to = i32::from_le_bytes([header_buf[8], header_buf[9], header_buf[10], header_buf[11]]);
    let opcode = i32::from_le_bytes([header_buf[12], header_buf[13], header_buf[14], header_buf[15]]);

    if opcode != OP_MSG && opcode != OP_QUERY {
        return Err(FrameError::UnsupportedOpcode { opcode });
    }

    let body_len = (total_length - 16) as usize;
    let mut body = vec![0_u8; body_len];
    if body_len > 0 {
        read_exact(stream, &mut body)?;
    }

    Ok(Some(Frame { header: MsgHeader { total_length, request_id, response_to, opcode }, body }))
}

/// Writes one OP_MSG response frame to the given stream.
pub fn write_frame<W>(stream: &mut W, request_id: i32, response_to: i32, body: &[u8]) -> Result<(), FrameError>
where
    W: Write,
{
    let total_length: u32 = 16 + body.len() as u32;
    stream.write_all(&total_length.to_le_bytes())?;
    stream.write_all(&request_id.to_le_bytes())?;
    stream.write_all(&response_to.to_le_bytes())?;
    stream.write_all(&OP_MSG.to_le_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}

/// Writes one OP_REPLY frame carrying a single BSON document.
///
/// Legacy replies carry `responseFlags`, `cursorId`, `startingFrom` and
/// `numberReturned` before the document, all zero except the document count.
pub fn write_reply_frame<W>(stream: &mut W, request_id: i32, response_to: i32, document: &[u8]) -> Result<(), FrameError>
where
    W: Write,
{
    let total_length: u32 = 16 + 20 + document.len() as u32;
    stream.write_all(&total_length.to_le_bytes())?;
    stream.write_all(&request_id.to_le_bytes())?;
    stream.write_all(&response_to.to_le_bytes())?;
    stream.write_all(&OP_REPLY.to_le_bytes())?;
    stream.write_all(&0_i32.to_le_bytes())?;
    stream.write_all(&0_i64.to_le_bytes())?;
    stream.write_all(&0_i32.to_le_bytes())?;
    stream.write_all(&1_i32.to_le_bytes())?;
    stream.write_all(document)?;
    stream.flush()?;
    Ok(())
}

fn read_exact<R>(stream: &mut R, buf: &mut [u8]) -> Result<(), FrameError>
where
    R: Read,
{
    let mut total = 0;
    while total < buf.len() {
        match stream.read(&mut buf[total..]) {
            Ok(0) => {
                if total == 0 {
                    return Err(FrameError::ConnectionClosed);
                }
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "mongo frame truncated").into());
            }
            Ok(n) => total += n,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(FrameError::Io(error)),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Buffer 形态组帧 / 解帧（宿主绑定入口）
// ---------------------------------------------------------------------------

/// 从完整帧字节读出 16 字节头；不足 16 字节返回 `None`。
pub fn peek_header(frame: &[u8]) -> Option<MsgHeader> {
    let head = <[u8; 16]>::try_from(frame.get(..16)?).ok()?;
    Some(MsgHeader {
        total_length: u32::from_le_bytes([head[0], head[1], head[2], head[3]]),
        request_id: i32::from_le_bytes([head[4], head[5], head[6], head[7]]),
        response_to: i32::from_le_bytes([head[8], head[9], head[10], head[11]]),
        opcode: i32::from_le_bytes([head[12], head[13], head[14], head[15]]),
    })
}

/// 展开一条完整帧。`OP_COMPRESSED` 在此解封套并按 id 分派解压：
/// Snappy（1）与 Zstd（3）走 `boost.data.compression`，Zlib（2）走宿主回调
/// （如 `node:zlib`）。封套声明的 `uncompressedSize` 在解压前后都会被校验，
/// 谎报尺寸的帧直接拒绝。
pub fn decode_frame(
    frame: &[u8],
    limits: FrameLimits,
    zlib_decompress: &mut dyn FnMut(&[u8], usize) -> Result<Vec<u8>, String>,
) -> Result<InboundFrame, FrameError> {
    let header = peek_header(frame).ok_or(FrameError::HeaderTruncated)?;
    if header.total_length > limits.max_frame_size {
        return Err(FrameError::FrameTooLarge { announced: header.total_length, limit: limits.max_frame_size });
    }
    if u64::from(header.total_length) != frame.len() as u64 {
        return Err(FrameError::LengthMismatch { announced: header.total_length, actual: frame.len() });
    }

    if header.opcode == OP_MSG || header.opcode == OP_QUERY {
        return Ok(InboundFrame { opcode: header.opcode, header, payload: frame[16..].to_vec(), envelope: None });
    }
    if header.opcode != OP_COMPRESSED {
        return Err(FrameError::UnsupportedOpcode { opcode: header.opcode });
    }

    let body = &frame[16..];
    if body.len() < 9 {
        return Err(FrameError::EnvelopeTruncated);
    }
    let original_opcode = i32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    let uncompressed_size = i32::from_le_bytes([body[4], body[5], body[6], body[7]]);
    let compressor_id = body[8];
    if original_opcode != OP_MSG {
        return Err(FrameError::CompressedOpcodeNotAllowed { opcode: original_opcode });
    }
    if uncompressed_size <= 0 || uncompressed_size as u64 > u64::from(limits.max_frame_size) {
        return Err(FrameError::InvalidAnnouncedSize { announced: uncompressed_size });
    }
    let announced = uncompressed_size as usize;
    let compressed = &body[9..];

    let payload = match compressor_id {
        COMPRESSOR_SNAPPY => data_compression::snappy::decode(compressed, announced).map_err(|error| error.to_string()),
        COMPRESSOR_ZSTD => data_compression::zstd::decode(compressed, announced).map_err(|error| error.to_string()),
        COMPRESSOR_ZLIB => zlib_decompress(compressed, announced).and_then(|output| {
            if output.len() != announced {
                return Err(format!("zlib stream decoded {} bytes, frame announced {announced}", output.len()));
            }
            Ok(output)
        }),
        other => return Err(FrameError::UnknownCompressor { compressor_id: other }),
    }
    .map_err(|message| FrameError::DecompressionFailed { compressor_id, message })?;

    Ok(InboundFrame {
        header,
        opcode: OP_MSG,
        payload,
        envelope: Some(CompressionEnvelope { compressor_id, original_opcode, uncompressed_size }),
    })
}

/// 组一条 OP_MSG 帧：16 字节头加原始 body。
pub fn encode_msg_frame(request_id: i32, response_to: i32, body: &[u8]) -> Vec<u8> {
    build_frame(request_id, response_to, OP_MSG, body)
}

/// 组一条 `OP_COMPRESSED` 帧。被封套包裹的消息按规范只能是 OP_MSG；
/// `compressed` 必须是 `compressor_id` 对应算法的合法输出。
pub fn encode_compressed_frame(request_id: i32, response_to: i32, compressor_id: u8, uncompressed_size: i32, compressed: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(9 + compressed.len());
    body.extend_from_slice(&OP_MSG.to_le_bytes());
    body.extend_from_slice(&uncompressed_size.to_le_bytes());
    body.push(compressor_id);
    body.extend_from_slice(compressed);
    build_frame(request_id, response_to, OP_COMPRESSED, &body)
}

/// 组一条遗留 OP_REPLY 帧：头后依次是 responseFlags、cursorID、
/// startingFrom、numberReturned（固定 1），再接单个 BSON 文档。
pub fn encode_reply_frame(request_id: i32, response_to: i32, document: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(20 + document.len());
    body.extend_from_slice(&0_i32.to_le_bytes());
    body.extend_from_slice(&0_i64.to_le_bytes());
    body.extend_from_slice(&0_i32.to_le_bytes());
    body.extend_from_slice(&1_i32.to_le_bytes());
    body.extend_from_slice(document);
    build_frame(request_id, response_to, OP_REPLY, &body)
}

fn build_frame(request_id: i32, response_to: i32, opcode: i32, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(16 + payload.len());
    frame.extend_from_slice(&((16 + payload.len()) as u32).to_le_bytes());
    frame.extend_from_slice(&request_id.to_le_bytes());
    frame.extend_from_slice(&response_to.to_le_bytes());
    frame.extend_from_slice(&opcode.to_le_bytes());
    frame.extend_from_slice(payload);
    frame
}

// ---------------------------------------------------------------------------
// OP_MSG / OP_QUERY 载荷 ↔ BSON 文档
// ---------------------------------------------------------------------------

/// `OP_MSG` 标志位 0：帧尾带 4 字节 CRC-32C 校验和。
const OP_MSG_CHECKSUM_PRESENT: u32 = 0x0000_0001;

/// 把一个命令文档编码为 `OP_MSG` 载荷：flags 为 0，只带一个 body section。
///
/// 需要校验和由调用方自行追加。
pub fn encode_op_msg_body(command: &BsonDocument) -> Vec<u8> {
    let mut body = vec![0_u8; 4];
    body.push(0x00);
    body.extend_from_slice(&encode_bson(command));
    body
}

/// 把 `OP_MSG` 载荷解码为命令文档。
///
/// 文档序列 section（`0x01`）按标识符折叠回命令里同名的数组字段，真实驱动
/// 就是这样发 `insert.documents` 与 `delete.deletes` 的。
pub fn decode_op_msg_command(op_msg_body: &[u8]) -> Result<BsonDocument, BsonError> {
    let flags = op_msg_body
        .get(..4)
        .and_then(|head| <[u8; 4]>::try_from(head).ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| BsonError::Malformed("OP_MSG body too short".into()))?;

    let mut sections = op_msg_body.get(4..).ok_or_else(|| BsonError::Malformed("OP_MSG body too short".into()))?;
    if flags & OP_MSG_CHECKSUM_PRESENT != 0 {
        sections = sections
            .len()
            .checked_sub(4)
            .and_then(|end| sections.get(..end))
            .ok_or_else(|| BsonError::Malformed("OP_MSG checksum section truncated".into()))?;
    }

    let mut body: Option<BsonDocument> = None;
    let mut sequences: Vec<(String, Vec<BsonValue>)> = Vec::new();

    while let Some((kind, rest)) = sections.split_first() {
        match kind {
            0x00 => {
                let length = bson_document_length(rest)?;
                let document = parse_bson(rest)?;
                sections = rest.get(length..).ok_or_else(|| BsonError::Malformed("OP_MSG body section truncated".into()))?;
                if body.replace(document).is_some() {
                    return Err(BsonError::Malformed("OP_MSG carries more than one body section".into()));
                }
            }
            0x01 => {
                let size =
                    rest.get(..4)
                        .and_then(|head| <[u8; 4]>::try_from(head).ok())
                        .map(|head| i32::from_le_bytes(head))
                        .filter(|size| *size > 4)
                        .ok_or_else(|| BsonError::Malformed("document sequence section missing its size".into()))? as usize;
                let section = rest.get(..size).ok_or_else(|| BsonError::Malformed("document sequence section truncated".into()))?;
                let header = &section[4..];
                let name_end = header
                    .iter()
                    .position(|byte| *byte == 0)
                    .ok_or_else(|| BsonError::Malformed("document sequence identifier has no terminator".into()))?;
                let identifier = std::str::from_utf8(&header[..name_end])
                    .map_err(|_| BsonError::Malformed("document sequence identifier is not UTF-8".into()))?
                    .to_string();
                let mut documents = &header[name_end + 1..];
                let mut values = Vec::new();
                while !documents.is_empty() {
                    let length = bson_document_length(documents)?;
                    let document = parse_bson(documents)?;
                    values.push(BsonValue::Document(document));
                    documents = &documents[length..];
                }
                sequences.push((identifier, values));
                sections = &rest[size..];
            }
            other => return Err(BsonError::Malformed(format!("unsupported OP_MSG section kind {other}"))),
        }
    }

    let mut command = body.ok_or_else(|| BsonError::Malformed("OP_MSG carries no body section".into()))?;
    for (identifier, values) in sequences {
        if command.get(&identifier).is_none() {
            command.push(identifier, BsonValue::Array(values));
        }
    }
    Ok(command)
}

/// 把遗留 `OP_QUERY` 载荷解码为命令文档。
///
/// 布局：flags（u32）、完整集合名（cstring）、skip（i32）、期望返回条数
/// （i32），随后是查询文档（可能包在 `$query` 里）。
pub fn decode_op_query_command(op_query_body: &[u8]) -> Result<BsonDocument, BsonError> {
    let rest = op_query_body.get(4..).ok_or_else(|| BsonError::Malformed("OP_QUERY body too short".into()))?;
    let name_end =
        rest.iter().position(|byte| *byte == 0).ok_or_else(|| BsonError::Malformed("OP_QUERY namespace has no terminator".into()))?;
    let query = rest.get(name_end + 1 + 8..).ok_or_else(|| BsonError::Malformed("OP_QUERY carries no query document".into()))?;
    let mut document = parse_bson(query)?;
    if let Some(BsonValue::Document(inner)) = document.get("$query").cloned() {
        document = inner;
    }
    Ok(document)
}

/// 读取 section 起始处的文档长度，并校验它与剩余字节的关系。
fn bson_document_length(bytes: &[u8]) -> Result<usize, BsonError> {
    let length = bytes
        .get(..4)
        .and_then(|head| <[u8; 4]>::try_from(head).ok())
        .map(|head| i32::from_le_bytes(head))
        .filter(|length| *length >= 5)
        .ok_or_else(|| BsonError::Malformed("BSON document length missing".into()))? as usize;
    if length > bytes.len() {
        return Err(BsonError::Malformed("BSON document length exceeds the section".into()));
    }
    Ok(length)
}

#[cfg(test)]
mod frame_tests {
    use super::*;

    fn hex_bytes(text: &str) -> Vec<u8> {
        (0..text.len()).step_by(2).map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("hex")).collect()
    }

    fn unused_zlib(_: &[u8], _: usize) -> Result<Vec<u8>, String> {
        Err("test frame did not expect zlib".into())
    }

    /// 期望值来源：`mongod 8.0.9` 对 `ping` 的真实应答。压缩流与解压结果
    /// 都是线上捕获字节，不由本仓编码器生成。
    fn real_mongod_snappy_capture() -> (Vec<u8>, Vec<u8>) {
        let compressed = hex_bytes("16000001014011000000016f6b00000000000000f03f00");
        let payload = hex_bytes("000000000011000000016f6b00000000000000f03f00");
        (compressed, payload)
    }

    fn compressed_frame(request_id: i32, response_to: i32, compressor_id: u8, uncompressed_size: i32, compressed: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        let total = 16 + 9 + compressed.len();
        frame.extend_from_slice(&(total as u32).to_le_bytes());
        frame.extend_from_slice(&request_id.to_le_bytes());
        frame.extend_from_slice(&response_to.to_le_bytes());
        frame.extend_from_slice(&OP_COMPRESSED.to_le_bytes());
        frame.extend_from_slice(&OP_MSG.to_le_bytes());
        frame.extend_from_slice(&uncompressed_size.to_le_bytes());
        frame.push(compressor_id);
        frame.extend_from_slice(compressed);
        frame
    }

    #[test]
    fn peek_header_reads_the_16_byte_prefix() {
        let frame = encode_msg_frame(7, 0, &[0xAA, 0xBB]);
        let header = peek_header(&frame).expect("header");
        assert_eq!(header, MsgHeader { total_length: 18, request_id: 7, response_to: 0, opcode: OP_MSG });
        assert!(peek_header(&[0u8; 15]).is_none());
    }

    #[test]
    fn decodes_plain_msg_and_query_payloads() {
        let body = vec![0x00, 0x01, 0x02];
        let msg = decode_frame(&encode_msg_frame(3, 0, &body), FrameLimits::default(), &mut unused_zlib).unwrap();
        assert_eq!(msg.opcode, OP_MSG);
        assert_eq!(msg.payload, body);
        assert!(msg.envelope.is_none());

        let query = decode_frame(&build_frame(4, 0, OP_QUERY, &body), FrameLimits::default(), &mut unused_zlib).unwrap();
        assert_eq!(query.opcode, OP_QUERY);
        assert_eq!(query.payload, body);
    }

    #[test]
    fn decodes_a_snappy_compressed_frame_from_a_real_mongod_reply() {
        let (compressed, payload) = real_mongod_snappy_capture();
        let frame = compressed_frame(7, 0, COMPRESSOR_SNAPPY, payload.len() as i32, &compressed);
        let inbound = decode_frame(&frame, FrameLimits::default(), &mut unused_zlib).expect("decode");
        assert_eq!(inbound.opcode, OP_MSG);
        assert_eq!(inbound.payload, payload);
        assert_eq!(
            inbound.envelope,
            Some(CompressionEnvelope { compressor_id: COMPRESSOR_SNAPPY, original_opcode: OP_MSG, uncompressed_size: 22 })
        );
    }

    #[test]
    fn decodes_a_zstd_compressed_frame_produced_by_libzstd() {
        // 压缩帧由 Node `node:zlib`（libzstd C 实现）独立生成，与 `ruzstd`
        // 解码器无共同来源；原始字节即期望解压结果。
        let original = hex_bytes("68656c6c6f207a73746420776f726c64");
        let compressed = hex_bytes("28b52ffd201081000068656c6c6f207a73746420776f726c64");
        let frame = compressed_frame(8, 0, COMPRESSOR_ZSTD, original.len() as i32, &compressed);
        let inbound = decode_frame(&frame, FrameLimits::default(), &mut unused_zlib).expect("decode");
        assert_eq!(inbound.opcode, OP_MSG);
        assert_eq!(inbound.payload, original);
        assert_eq!(
            inbound.envelope,
            Some(CompressionEnvelope { compressor_id: COMPRESSOR_ZSTD, original_opcode: OP_MSG, uncompressed_size: 16 })
        );
    }

    #[test]
    fn zlib_decompression_goes_through_the_host_callback() {
        let compressed = vec![0xAA, 0xBB];
        let frame = compressed_frame(9, 0, COMPRESSOR_ZLIB, 5, &compressed);
        let mut seen: Option<(Vec<u8>, usize)> = None;
        let inbound = decode_frame(&frame, FrameLimits::default(), &mut |input, announced| {
            seen = Some((input.to_vec(), announced));
            Ok(vec![7_u8; announced])
        })
        .expect("decode");
        assert_eq!(seen, Some((compressed, 5)));
        assert_eq!(inbound.payload, vec![7_u8; 5]);
    }

    #[test]
    fn zlib_callback_errors_and_lying_lengths_are_rejected() {
        let frame = compressed_frame(9, 0, COMPRESSOR_ZLIB, 5, &[0xAA]);
        let error =
            decode_frame(&frame, FrameLimits::default(), &mut |_, _| Err("inflate failed".into())).expect_err("callback error must surface");
        assert!(matches!(&error, FrameError::DecompressionFailed { compressor_id: 2, message } if message == "inflate failed"));
        let error =
            decode_frame(&frame, FrameLimits::default(), &mut |_, _| Ok(vec![0_u8; 4])).expect_err("short host output must be rejected");
        assert!(matches!(error, FrameError::DecompressionFailed { compressor_id: 2, .. }));
    }

    #[test]
    fn rejects_malformed_frames() {
        // 不足 16 字节头。
        assert!(matches!(decode_frame(&[0_u8; 8], FrameLimits::default(), &mut unused_zlib), Err(FrameError::HeaderTruncated)));

        // 声明长度与实际字节数不一致。
        let mut frame = encode_msg_frame(1, 0, &[0_u8; 4]);
        frame[0] = 24;
        assert!(matches!(
            decode_frame(&frame, FrameLimits::default(), &mut unused_zlib),
            Err(FrameError::LengthMismatch { announced: 24, actual: 20 })
        ));

        // 越出上限。
        let frame = encode_msg_frame(1, 0, &[0_u8; 8]);
        let limits = FrameLimits { max_frame_size: 16 };
        assert!(matches!(decode_frame(&frame, limits, &mut unused_zlib), Err(FrameError::FrameTooLarge { announced: 24, limit: 16 })));

        // 不支持的操作码。
        let error = decode_frame(&build_frame(1, 0, 2006, &[]), FrameLimits::default(), &mut unused_zlib).expect_err("opcode");
        assert!(matches!(error, FrameError::UnsupportedOpcode { opcode: 2006 }));

        // OP_COMPRESSED 载荷不足 9 字节封套。
        let mut frame = build_frame(1, 0, OP_COMPRESSED, &[0_u8; 8]);
        frame[0] = 24;
        assert!(matches!(decode_frame(&frame, FrameLimits::default(), &mut unused_zlib), Err(FrameError::EnvelopeTruncated)));

        // OP_COMPRESSED 只允许包 OP_MSG。
        let mut body = Vec::new();
        body.extend_from_slice(&OP_QUERY.to_le_bytes());
        body.extend_from_slice(&1_i32.to_le_bytes());
        body.push(COMPRESSOR_SNAPPY);
        let error =
            decode_frame(&build_frame(1, 0, OP_COMPRESSED, &body), FrameLimits::default(), &mut unused_zlib).expect_err("wrapped opcode");
        assert!(matches!(error, FrameError::CompressedOpcodeNotAllowed { opcode: 2004 }));

        // 封套声明尺寸非正或越界。
        let error = decode_frame(&compressed_frame(1, 0, COMPRESSOR_SNAPPY, -1, &[0]), FrameLimits::default(), &mut unused_zlib)
            .expect_err("negative size");
        assert!(matches!(error, FrameError::InvalidAnnouncedSize { announced: -1 }));
        let error =
            decode_frame(&compressed_frame(1, 0, COMPRESSOR_SNAPPY, 48 * 1024 * 1024 + 1, &[0]), FrameLimits::default(), &mut unused_zlib)
                .expect_err("oversized announcement");
        assert!(matches!(error, FrameError::InvalidAnnouncedSize { .. }));

        // 未知压缩器 id。
        let error = decode_frame(&compressed_frame(1, 0, 4, 8, &[0]), FrameLimits::default(), &mut unused_zlib).expect_err("id");
        assert!(matches!(error, FrameError::UnknownCompressor { compressor_id: 4 }));

        // 真实 mongod 流但封套谎报尺寸：snappy 自报长度与封套不一致。
        let (compressed, _) = real_mongod_snappy_capture();
        let error = decode_frame(&compressed_frame(1, 0, COMPRESSOR_SNAPPY, 21, &compressed), FrameLimits::default(), &mut unused_zlib)
            .expect_err("lying envelope");
        assert!(matches!(error, FrameError::DecompressionFailed { compressor_id: 1, .. }));
    }

    #[test]
    fn encode_msg_frame_matches_the_wire_layout() {
        // 期望字节按 MongoDB 线协议规范手工构造，不来自解码器。
        let frame = encode_msg_frame(3, 4, &[0x00, 0x01]);
        assert_eq!(frame, vec![18, 0, 0, 0, 3, 0, 0, 0, 4, 0, 0, 0, 0xDD, 0x07, 0, 0, 0x00, 0x01]);
    }

    #[test]
    fn encode_reply_frame_matches_the_wire_layout() {
        let document = vec![0x05, 0, 0, 0];
        let frame = encode_reply_frame(5, 1, &document);
        let mut expected = vec![40, 0, 0, 0, 5, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0];
        expected.extend_from_slice(&[0, 0, 0, 0]);
        expected.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        expected.extend_from_slice(&[0, 0, 0, 0]);
        expected.extend_from_slice(&[1, 0, 0, 0]);
        expected.extend_from_slice(&document);
        assert_eq!(frame, expected);
    }

    #[test]
    fn encode_compressed_frame_matches_the_wire_layout() {
        let compressed = vec![0x01, 0x02, 0x03];
        let frame = encode_compressed_frame(9, 0, COMPRESSOR_SNAPPY, 22, &compressed);
        let mut expected = vec![28, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0, 0, 0xDC, 0x07, 0, 0];
        expected.extend_from_slice(&[0xDD, 0x07, 0, 0]);
        expected.extend_from_slice(&[0x16, 0, 0, 0]);
        expected.push(1);
        expected.extend_from_slice(&compressed);
        assert_eq!(frame, expected);
    }

    #[test]
    fn encoded_real_mongod_frame_decodes_back_to_the_captured_payload() {
        // 组帧器与 mongod 捕获流的一致性：载荷期望仍是线上真实解压结果。
        let (compressed, payload) = real_mongod_snappy_capture();
        let frame = encode_compressed_frame(7, 0, COMPRESSOR_SNAPPY, payload.len() as i32, &compressed);
        let inbound = decode_frame(&frame, FrameLimits::default(), &mut unused_zlib).expect("decode");
        assert_eq!(inbound.payload, payload);
    }
}
