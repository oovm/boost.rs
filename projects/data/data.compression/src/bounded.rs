//! Bounded one-shot decoding with a partial prefix and a typed terminal status.
//!
//! Format adapters that carve a compressed stream out of a larger file cannot
//! assume the stream is complete: the caller's output cap may cut it short and
//! the file may end in the middle of the bitstream. [`BoundedDecode`] therefore
//! reports the decoded prefix together with the input bytes consumed and a
//! [`DecodeStatus`], instead of returning only an error and dropping the work.
//!
//! With the `deflate` feature every envelope is selected explicitly through
//! `Envelope`, and `Envelope::detect` offers the same leading-byte heuristic the
//! products used locally before this module existed. The `brotli` feature only
//! uses [`BoundedDecode`] and [`DecodeStatus`].

#[cfg(feature = "deflate")]
use crate::{
    deflate::{Deflate, RawOutcome},
    gzip::Gzip,
    limits::CodecLimits,
    stream::EnvelopeOutcome,
    zlib::Zlib,
};
use crate::{
    error::{ChecksumAlgorithm, CodecError},
    facts::ChecksumFacts,
};

/// Envelope wrapped around a DEFLATE bitstream.
#[cfg(feature = "deflate")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Envelope {
    /// Raw DEFLATE (RFC 1951): no container header or trailer.
    Raw,
    /// Zlib container (RFC 1950): two-byte header and Adler-32 trailer.
    Zlib,
    /// Gzip member (RFC 1952): member header and CRC-32 plus `ISIZE` trailer.
    Gzip,
}

#[cfg(feature = "deflate")]
impl Envelope {
    /// Guesses the envelope from the leading bytes.
    ///
    /// The gzip member magic wins first, then a header that satisfies the RFC
    /// 1950 check rules, otherwise the bytes are treated as raw DEFLATE. The
    /// guess is a convenience only: [`Envelope::decode`] still validates the
    /// header it selected.
    pub fn detect(input: &[u8]) -> Self {
        if input.starts_with(&[0x1F, 0x8B]) {
            Self::Gzip
        }
        else if input.len() >= 2 && input[0] & 0x0F == 8 && input[0] >> 4 <= 7 && u16::from_be_bytes([input[0], input[1]]) % 31 == 0 {
            Self::Zlib
        }
        else {
            Self::Raw
        }
    }

    /// Stable lowercase name of the envelope.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Zlib => "zlib",
            Self::Gzip => "gzip",
        }
    }

    /// Decodes one stream under this envelope.
    pub fn decode(self, input: &[u8], limits: CodecLimits) -> BoundedDecode {
        match self {
            Self::Raw => Deflate::decode_bounded(input, limits),
            Self::Zlib => Zlib::decode_bounded(input, limits),
            Self::Gzip => Gzip::decode_bounded(input, limits),
        }
    }
}

/// Terminal state of a bounded decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeStatus {
    /// The stream decoded completely and every envelope trailer validated.
    Complete,
    /// A caller limit stopped decoding; [`BoundedDecode::output`] is a prefix.
    ///
    /// `produced` is the number of bytes emitted and `limit` is the output cap
    /// that stopped the decode.
    OutputLimit {
        /// Bytes emitted before decoding stopped.
        produced: u64,
        /// Output cap that stopped the decode.
        limit: u64,
    },
    /// The input ended before the stream completed; [`BoundedDecode::output`] is a prefix.
    InputTruncated {
        /// Byte offset where more input was needed.
        offset: usize,
        /// Minimum additional bytes required.
        needed: usize,
    },
    /// An envelope checksum did not match; [`BoundedDecode::output`] holds the payload.
    ChecksumMismatch {
        /// Checksum algorithm that failed.
        algorithm: ChecksumAlgorithm,
        /// Value the trailer carried.
        expected: u64,
        /// Value computed over the decoded bytes.
        actual: u64,
    },
    /// An envelope length trailer disagreed with the decoded length.
    LengthMismatch {
        /// Length the trailer announced.
        announced: u64,
        /// Length actually decoded.
        actual: u64,
    },
    /// The stream is structurally invalid and cannot be decoded further.
    Invalid {
        /// Underlying error with its typed parameters and stable code.
        error: CodecError,
    },
}

impl DecodeStatus {
    /// Whether the whole stream decoded and validated.
    #[inline]
    pub const fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }

    /// Stable machine-readable code of this status.
    ///
    /// Codes never change between releases and carry no message text, so a
    /// caller can dispatch on them without parsing a [`Display`](core::fmt::Display)
    /// string. A structural failure reports the code of its underlying
    /// [`CodecError`].
    pub fn code(&self) -> &'static str {
        match self {
            Self::Complete => "compression/complete",
            Self::OutputLimit { .. } => "compression/output-limit",
            Self::InputTruncated { .. } => "compression/input-truncated",
            Self::ChecksumMismatch { .. } => "compression/checksum-mismatch",
            Self::LengthMismatch { .. } => "compression/length-mismatch",
            Self::Invalid { error } => error.code(),
        }
    }
}

/// One bounded decode: the decoded prefix, bytes consumed, and why it stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedDecode {
    /// Bytes decoded before the stream completed or stopped.
    ///
    /// A prefix of the full payload unless [`DecodeStatus::Complete`].
    pub output: Vec<u8>,
    /// Input bytes consumed.
    ///
    /// On success this covers the envelope header, the bitstream, and the
    /// trailer, so it is the exact end offset of the member. On a stop it is the
    /// furthest point the decoder fully read: the bit cursor for a bitstream
    /// stop, the trailer end for a checksum or length mismatch, and the input
    /// length for a truncated trailer.
    pub consumed: usize,
    /// Terminal state of the decode.
    pub status: DecodeStatus,
    /// Checksum an envelope computed over the decoded bytes, when it got that far.
    pub checksum: Option<ChecksumFacts>,
    /// Input bytes remaining after a complete member; `0` when decoding stopped early.
    pub trailing_input: usize,
}

impl BoundedDecode {
    /// Whether the whole stream decoded and validated.
    #[inline]
    pub fn is_complete(&self) -> bool {
        self.status.is_complete()
    }

    /// Whether a caller limit stopped the decode while keeping a partial prefix.
    #[inline]
    pub fn is_output_limit(&self) -> bool {
        matches!(self.status, DecodeStatus::OutputLimit { .. })
    }

    #[cfg(feature = "deflate")]
    fn from_raw(outcome: RawOutcome, limits: &CodecLimits) -> Self {
        match outcome.error {
            None => Self {
                output: outcome.output,
                consumed: outcome.consumed,
                status: DecodeStatus::Complete,
                checksum: None,
                trailing_input: outcome.trailing,
            },
            Some(error) => {
                let output_len = outcome.output.len() as u64;
                let status = match status_of(error, limits) {
                    DecodeStatus::OutputLimit { limit, .. } => DecodeStatus::OutputLimit { produced: output_len, limit },
                    other => other,
                };
                Self { output: outcome.output, consumed: outcome.consumed, status, checksum: None, trailing_input: 0 }
            }
        }
    }

    #[cfg(feature = "deflate")]
    fn from_envelope(outcome: EnvelopeOutcome, limits: &CodecLimits) -> Self {
        match outcome.error {
            None => Self {
                output: outcome.output,
                consumed: outcome.consumed,
                status: DecodeStatus::Complete,
                checksum: outcome.checksum,
                trailing_input: outcome.trailing,
            },
            Some(error) => {
                let output_len = outcome.output.len() as u64;
                let status = match status_of(error, limits) {
                    DecodeStatus::OutputLimit { limit, .. } => DecodeStatus::OutputLimit { produced: output_len, limit },
                    other => other,
                };
                Self { output: outcome.output, consumed: outcome.consumed, status, checksum: outcome.checksum, trailing_input: 0 }
            }
        }
    }
}

#[cfg(feature = "deflate")]
impl Deflate {
    /// Decodes a raw DEFLATE stream, keeping the prefix produced before a stop.
    pub fn decode_bounded(input: &[u8], limits: CodecLimits) -> BoundedDecode {
        BoundedDecode::from_raw(crate::deflate::decode_outcome(input, limits), &limits)
    }
}

#[cfg(feature = "deflate")]
impl Zlib {
    /// Decodes a zlib stream, keeping the prefix produced before a stop.
    pub fn decode_bounded(input: &[u8], limits: CodecLimits) -> BoundedDecode {
        BoundedDecode::from_envelope(decode_zlib_bounded(input, limits), &limits)
    }
}

#[cfg(feature = "deflate")]
impl Gzip {
    /// Decodes the first gzip member, keeping the prefix produced before a stop.
    ///
    /// Further members are never followed: they are reported through
    /// [`BoundedDecode::trailing_input`] and left to the caller.
    pub fn decode_bounded(input: &[u8], limits: CodecLimits) -> BoundedDecode {
        BoundedDecode::from_envelope(decode_gzip_bounded(input, limits), &limits)
    }
}

/// Wraps the zlib outcome entry so the bounded path shares one implementation.
#[cfg(feature = "deflate")]
fn decode_zlib_bounded(input: &[u8], limits: CodecLimits) -> EnvelopeOutcome {
    crate::zlib::decode_zlib_outcome(input, limits)
}

/// Wraps the gzip outcome entry so the bounded path shares one implementation.
#[cfg(feature = "deflate")]
fn decode_gzip_bounded(input: &[u8], limits: CodecLimits) -> EnvelopeOutcome {
    crate::gzip::decode_gzip_outcome(input, limits)
}

/// Classifies a terminal error into the status a bounded caller expects.
///
/// Both the output cap and the expansion-ratio cap are caller limits, so they
/// map to [`DecodeStatus::OutputLimit`]; every other resource limit is a
/// structural failure for a one-shot decode.
#[cfg(feature = "deflate")]
fn status_of(error: CodecError, limits: &CodecLimits) -> DecodeStatus {
    match error {
        CodecError::InputExhausted { offset, needed } => DecodeStatus::InputTruncated { offset, needed },
        CodecError::OutputLimitExceeded { produced, limit } => DecodeStatus::OutputLimit { produced, limit },
        CodecError::ExpansionRatioExceeded { output, .. } => DecodeStatus::OutputLimit { produced: output, limit: limits.output_limit() },
        CodecError::ChecksumMismatch { algorithm, expected, actual } => DecodeStatus::ChecksumMismatch { algorithm, expected, actual },
        CodecError::LengthMismatch { announced, actual } => DecodeStatus::LengthMismatch { announced, actual },
        other => DecodeStatus::Invalid { error: other },
    }
}
