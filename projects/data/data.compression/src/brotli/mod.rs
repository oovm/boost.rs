//! Brotli (RFC 7932) stream decoding, internalized with `std` only.
//!
//! The decoder reads one complete stream from the start of the input and stops
//! after the final meta-block, so trailing bytes are reported rather than
//! rejected. It supports window sizes 10 to 24, uncompressed and compressed
//! meta-blocks, block switching for literals, commands and distances, all four
//! context modes, distance postfix/direct codes, the full 704-symbol command
//! table, and static dictionary references with their word transforms.
//!
//! The public entry points follow the crate's bounded-decode contract:
//! [`Brotli::decode`] returns the payload or a [`CodecError`], and
//! [`Brotli::decode_bounded`] keeps the prefix produced before a stop and
//! reports a [`DecodeStatus`]. Only `max_output_bytes` from [`CodecLimits`] is
//! consulted; the other limit fields are Deflate/LZMA-specific.
//!
//! Consumers should note one limitation: the decoder does not expose a
//! streaming interface, and it decodes the whole stream into memory, so the
//! output cap is the only defence against a decompression bomb.

mod bit;
mod decoder;
mod dictionary;
mod huffman;
mod tables;

use crate::{BoundedDecode, CodecError, CodecLimits, DecodeStatus, bit::BitPosition};

/// Error raised while decoding a Brotli stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrotliError {
    /// The stream ended before the final meta-block.
    Truncated,
    /// The stream is structurally invalid.
    Corrupt(&'static str),
    /// Decoding would exceed the caller's output limit.
    LimitExceeded,
}

/// Brotli stream codec (RFC 7932).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Brotli;

impl Brotli {
    /// Decodes one complete Brotli stream.
    ///
    /// Decoding starts at the first byte of `input` and stops after the final
    /// meta-block, so trailing bytes are ignored. When the decoded output would
    /// exceed `limits.max_output_bytes`, [`CodecError::OutputLimitExceeded`] is
    /// returned before the limit is crossed.
    pub fn decode(input: &[u8], limits: CodecLimits) -> Result<Vec<u8>, CodecError> {
        let outcome = decoder::decompress(input, output_cap(&limits));
        match outcome.error {
            None => Ok(outcome.output),
            Some(error) => Err(codec_error(error, outcome.output.len(), outcome.consumed, &limits)),
        }
    }

    /// Decodes one Brotli stream, keeping the prefix produced before a stop.
    ///
    /// A complete stream reports [`DecodeStatus::Complete`] with the trailing
    /// bytes in [`BoundedDecode::trailing_input`]; an output cap reports
    /// [`DecodeStatus::OutputLimit`] with a valid prefix inside the cap.
    pub fn decode_bounded(input: &[u8], limits: CodecLimits) -> BoundedDecode {
        let outcome = decoder::decompress(input, output_cap(&limits));
        let consumed = outcome.consumed;
        let status = match outcome.error {
            None => DecodeStatus::Complete,
            Some(BrotliError::Truncated) => DecodeStatus::InputTruncated { offset: consumed, needed: 1 },
            Some(BrotliError::LimitExceeded) => {
                DecodeStatus::OutputLimit { produced: outcome.output.len() as u64, limit: limits.output_limit() }
            }
            Some(BrotliError::Corrupt(reason)) => DecodeStatus::Invalid { error: corrupt_error(reason, consumed) },
        };
        let trailing_input = if status.is_complete() { input.len() - consumed } else { 0 };
        BoundedDecode { output: outcome.output, consumed, status, checksum: None, trailing_input }
    }
}

/// Looks up and transforms one RFC 7932 static-dictionary word.
///
/// `word_id` encodes both the word index and the transform, with the transform
/// identifier in the high bits above the per-length index width. Returns
/// `CodecError::InvalidBitPattern` at offset 0 when the length, index or
/// transform is outside the dictionary tables.
pub fn word(length: usize, word_id: u32) -> Result<Vec<u8>, CodecError> {
    dictionary::word(length, word_id).map_err(|_| CodecError::InvalidBitPattern { offset: 0, width: 8 })
}

/// Applies one of the 121 RFC 7932 dictionary word transforms to `word`.
///
/// Returns `CodecError::InvalidBitPattern` at offset 0 when `transform_id` is
/// not one of the transforms defined by the specification.
pub fn apply_transform(transform_id: usize, word: &[u8]) -> Result<Vec<u8>, CodecError> {
    dictionary::apply_transform(transform_id, word).map_err(|_| CodecError::InvalidBitPattern { offset: 0, width: 8 })
}

/// Output cap as a `usize`, saturating at the platform maximum.
fn output_cap(limits: &CodecLimits) -> usize {
    usize::try_from(limits.output_limit()).unwrap_or(usize::MAX)
}

/// Converts an internal terminal error into the crate's codec error.
fn codec_error(error: BrotliError, produced: usize, consumed: usize, limits: &CodecLimits) -> CodecError {
    match error {
        BrotliError::Truncated => CodecError::InputExhausted { offset: consumed, needed: 1 },
        BrotliError::LimitExceeded => CodecError::OutputLimitExceeded { produced: produced as u64, limit: limits.output_limit() },
        BrotliError::Corrupt(reason) => corrupt_error(reason, consumed),
    }
}

/// Classifies a corrupt-stream reason into a structured codec error.
fn corrupt_error(reason: &'static str, offset: usize) -> CodecError {
    if reason.contains("prefix code") || reason.contains("code length") {
        CodecError::InvalidHuffmanTree { position: BitPosition::new(offset as u64, 0) }
    }
    else {
        CodecError::InvalidBitPattern { offset, width: 8 }
    }
}
