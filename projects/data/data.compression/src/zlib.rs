//! Zlib envelope (RFC 1950).
//!
//! `Zlib` owns everything specific to the wrapper: the `CMF` / `FLG` header and
//! its check value, the `CINFO` window declaration, the `FDICT` dictionary bit,
//! the Adler-32 trailer, and the trailing-input rule. The Deflate bitstream
//! itself belongs to [`Deflate`], so this module never re-implements it.

use crate::{
    deflate::{Deflate, DeflateLevel},
    error::{ChecksumAlgorithm, CodecError, CodecFeature, EnvelopeKind},
    facts::{ChecksumFacts, DecodeFacts},
    limits::CodecLimits,
    stream::{CompressionProtocol, EnvelopeFacts, EnvelopeOutcome, StreamDecoder},
};

use data_checksum::adler32;

/// Zlib stream codec.
#[derive(Debug, Clone, Copy, Default)]
pub struct Zlib;

impl Zlib {
    /// Decodes a zlib stream with the given limits.
    pub fn decode(input: &[u8], limits: CodecLimits) -> Result<DecodeFacts<Vec<u8>>, CodecError> {
        decode_zlib(input, limits).map(EnvelopeFacts::into_decode)
    }

    /// Encodes `input` as a zlib stream with a fixed, reproducible header.
    ///
    /// The header declares method 8 (Deflate) with a 32 KiB window, `FDICT`
    /// clear, and `FLEVEL` 0, so repeated encodes of the same input produce
    /// identical bytes.
    pub fn encode(input: &[u8], level: DeflateLevel) -> Vec<u8> {
        let mut output = Vec::with_capacity(input.len() + 11);
        output.push(CMF_DEFLATE_32K);
        output.push(FLG_DEFAULT);
        output.extend_from_slice(&Deflate::encode(input, level));
        output.extend_from_slice(&adler32(input).to_be_bytes());
        output
    }

    /// Creates a buffered stream decoder for zlib.
    pub fn decoder(limits: CodecLimits) -> StreamDecoder {
        StreamDecoder::new(CompressionProtocol::Zlib, limits)
    }
}

/// `CMF` byte for method 8 with a 32 KiB window.
pub const CMF_DEFLATE_32K: u8 = 0x78;
/// `FLG` byte with `FLEVEL` 0, `FDICT` clear, and a valid header check value.
pub const FLG_DEFAULT: u8 = 0x01;

/// Widest window an RFC 1950 `CINFO` field may declare (`2^(7+8)` = 32 KiB).
const MAX_CINFO: u8 = 7;
/// `FDICT` bit in the `FLG` byte.
const FLG_PRESET_DICTIONARY: u8 = 0x20;

/// Decodes a zlib stream and reports its envelope facts.
pub(crate) fn decode_zlib(input: &[u8], limits: CodecLimits) -> Result<EnvelopeFacts, CodecError> {
    decode_zlib_outcome(input, limits).into_facts()
}

/// Decodes a zlib stream and keeps whatever it produced before a stop.
pub(crate) fn decode_zlib_outcome(input: &[u8], limits: CodecLimits) -> EnvelopeOutcome {
    if let Err(error) = check_header(input) {
        return EnvelopeOutcome::rejected(error);
    }

    let raw = crate::deflate::decode_outcome(&input[2..], limits);
    let trailer_start = 2 + raw.consumed;
    if let Some(error) = raw.error {
        return EnvelopeOutcome { output: raw.output, consumed: trailer_start, trailing: 0, checksum: None, error: Some(error) };
    }

    if input.len() < trailer_start + 4 {
        return EnvelopeOutcome {
            output: raw.output,
            consumed: input.len(),
            trailing: 0,
            checksum: None,
            error: Some(CodecError::InputExhausted { offset: input.len(), needed: trailer_start + 4 - input.len() }),
        };
    }

    let expected = u32::from_be_bytes([input[trailer_start], input[trailer_start + 1], input[trailer_start + 2], input[trailer_start + 3]]);
    let actual = adler32(&raw.output);
    let consumed = trailer_start + 4;
    let verified = expected == actual;
    let checksum = ChecksumFacts { algorithm: ChecksumAlgorithm::Adler32, value: actual as u64, verified };
    let error = if verified {
        None
    }
    else {
        Some(CodecError::ChecksumMismatch { algorithm: ChecksumAlgorithm::Adler32, expected: expected as u64, actual: actual as u64 })
    };
    EnvelopeOutcome { output: raw.output, consumed, trailing: input.len() - consumed, checksum: Some(checksum), error }
}

/// Validates the two-byte zlib header without touching the bitstream.
fn check_header(input: &[u8]) -> Result<(), CodecError> {
    if input.len() < 6 {
        return Err(CodecError::InputExhausted { offset: input.len(), needed: 6 - input.len() });
    }

    let cmf = input[0];
    let flg = input[1];
    if (cmf as u16 * 256 + flg as u16) % 31 != 0 {
        return Err(CodecError::InvalidEnvelope { offset: 0, kind: EnvelopeKind::Zlib });
    }
    if cmf & 0x0F != 8 {
        return Err(CodecError::UnsupportedFeature { feature: CodecFeature::CompressionMethod(cmf & 0x0F) });
    }
    // `CINFO` declares the window as `2^(CINFO + 8)`; anything above 7 exceeds
    // the 32 KiB window Deflate itself defines.
    if cmf >> 4 > MAX_CINFO {
        return Err(CodecError::InvalidEnvelope { offset: 0, kind: EnvelopeKind::Zlib });
    }
    // A preset dictionary would need a 4-byte `DICTID` and an initialized
    // window. Decoding it as if the window were empty would silently produce
    // wrong bytes, so the envelope refuses it instead of guessing.
    if flg & FLG_PRESET_DICTIONARY != 0 {
        return Err(CodecError::UnsupportedFeature { feature: CodecFeature::PresetDictionary });
    }
    Ok(())
}
