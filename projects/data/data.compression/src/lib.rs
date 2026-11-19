//! 通用压缩编解码层（`boost.data.compression`）。
//!
//! 本 crate 拥有跨协议共享的压缩字节流算法：Snappy block 格式（MongoDB
//! `OP_COMPRESSED` id 1）与 Zstandard 帧（id 3）。Zlib（id 2）由 Acorn 的
//! `acorn-codec` deflate/zlib 实现提供，消费者直接复用，不在这里复制第二套。
//! 协议层（如 `database.protocol.mongo`）只负责封套与 id 分派，算法归本层；
//! 产品侧禁止再各自内嵌 Snappy/Zstd 编解码副本。

#![deny(missing_docs)]

pub mod snappy;
pub mod zstd;

/// 压缩流解码失败的统一分层。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressionError {
    /// 流在一条完整指令结束前截断。
    Truncated,
    /// 流自报长度与帧封套声明的长度不一致，或解码产出越界。
    LengthMismatch {
        /// 帧封套（或流头部）声明的解压后字节数。
        announced: usize,
        /// 实际解码得到的字节数。
        actual: usize,
    },
    /// 流结构非法：未知 tag、偏移越出已解码前缀、varint 超界等。
    Invalid,
}

impl core::fmt::Display for CompressionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => f.write_str("compressed stream ends inside an instruction"),
            Self::LengthMismatch { announced, actual } => {
                write!(f, "stream decodes to {actual} bytes, frame announced {announced}")
            }
            Self::Invalid => f.write_str("compressed stream is malformed"),
        }
    }
}

impl std::error::Error for CompressionError {}
