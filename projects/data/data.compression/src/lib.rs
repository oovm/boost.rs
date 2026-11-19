//! 通用压缩编解码层（`boost.data.compression`）。
//!
//! 本 crate 拥有跨协议共享的压缩字节流算法：Snappy block 格式（MongoDB
//! `OP_COMPRESSED` id 1）、Zstandard 帧（id 3），以及 `deflate` feature 下的
//! 原始 Deflate（RFC 1951）与其 zlib（RFC 1950）、gzip（RFC 1952）封套。
//! Deflate 位流算法只有一份：`Deflate` 是算法本身，`Zlib` 与 `Gzip` 是它的
//! 封套，各自持有 header、trailer、checksum 与 trailing-input 规则。
//! 协议层（如 `database.protocol.mongo`）只负责封套与 id 分派，算法归本层；
//! 产品侧禁止再各自内嵌 Snappy/Zstd/Deflate 编解码副本。
//!
//! The Deflate family originates from the former `acorn.rs` project (imported
//! at commit `6deb56a4`, MPL-2.0) and has been internalized and de-branded
//! for Boost.

#![deny(missing_docs)]

pub mod snappy;
pub mod zstd;

#[cfg(feature = "deflate")]
pub(crate) mod bit;
#[cfg(feature = "deflate")]
mod error;
#[cfg(feature = "deflate")]
mod facts;
#[cfg(feature = "deflate")]
mod limits;
#[cfg(feature = "deflate")]
pub mod stream;

#[cfg(feature = "deflate")]
pub mod deflate;
#[cfg(feature = "deflate")]
pub mod gzip;
#[cfg(all(feature = "deflate", not(target_arch = "wasm32")))]
mod gzip_stream;
#[cfg(feature = "deflate")]
pub(crate) mod input;
#[cfg(feature = "deflate")]
pub mod zlib;

#[cfg(feature = "deflate")]
pub use error::{ChecksumAlgorithm, CodecError, CodecFeature, EnvelopeKind, Ratio};
#[cfg(feature = "deflate")]
pub use facts::{Canonicality, ChecksumFacts, DecodeFacts, StreamFacts, StreamState};
#[cfg(feature = "deflate")]
pub use limits::CodecLimits;
#[cfg(feature = "deflate")]
pub use stream::{CompressionProtocol, StreamChunk, StreamCodec, StreamDecoder};

#[cfg(feature = "deflate")]
pub use crate::deflate::{Deflate, DeflateEncoder, DeflateLevel, DeflateStreamDecoder, DeflateStreamEncoder};
#[cfg(feature = "deflate")]
pub use crate::gzip::Gzip;
#[cfg(all(feature = "deflate", not(target_arch = "wasm32")))]
pub use crate::gzip_stream::{GzipStreamDecoder, GzipStreamEncoder};
#[cfg(feature = "deflate")]
pub use crate::zlib::Zlib;
#[cfg(feature = "deflate")]
pub use data_checksum::Crc32;

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
