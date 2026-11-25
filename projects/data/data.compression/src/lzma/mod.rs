//! LZMA1 and LZMA2 decoding (internalized, pure Rust).
//!
//! [`Lzma1`] decodes the `LZMA_ALONE` envelope (`.lzma` files: five property
//! bytes, an eight-byte unpack size, then the raw range-coded body) and the
//! raw body itself, which is what container formats such as 7z store inside
//! their packed streams. [`Lzma2`] decodes the chunked LZMA2 stream: a control
//! byte per chunk (`0x00` end, `0x01`/`0x02` uncompressed chunks with and
//! without dictionary reset, `0x80..=0xFF` LZMA chunks whose top bits select
//! state/property/dictionary resets), each chunk carrying its own range coder.
//!
//! Both codecs are decompression-only and never spawn external processes.
//! There is no canonical-form concept for arithmetic-coded streams, so every
//! result reports [`Canonicality::NotApplicable`].

mod core;
mod range;

use self::{
    core::{DecodeScope, LzmaCore, LzmaProperties},
    range::RangeDecoder,
};
use crate::{Canonicality, CodecError, CodecFeature, CodecLimits, DecodeFacts};

/// Largest dictionary size the internalized decoder accepts (1 GiB).
pub const MAX_DICTIONARY_SIZE: u32 = 1 << 30;

/// Dictionary size applied when a stream declares zero.
const MINIMUM_DICTIONARY_SIZE: u32 = 1 << 12;

/// `LZMA_ALONE` header: five property bytes plus the eight-byte unpack size.
const ALONE_HEADER_LEN: usize = 13;

/// All-ones marker announcing an unknown unpack size.
const UNKNOWN_UNPACK_SIZE: u64 = u64::MAX;

/// LZMA1 codec (raw range-coded body, optionally inside the `LZMA_ALONE` envelope).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lzma1;

impl Lzma1 {
    /// Decodes an `LZMA_ALONE` stream: the 13-byte header (five property bytes
    /// and a little-endian unpack size, all-ones meaning unknown) followed by
    /// the raw body.
    pub fn decode(input: &[u8], limits: CodecLimits) -> Result<DecodeFacts<Vec<u8>>, CodecError> {
        let header = input
            .get(..ALONE_HEADER_LEN)
            .ok_or(CodecError::InputExhausted { offset: input.len(), needed: ALONE_HEADER_LEN - input.len().min(ALONE_HEADER_LEN) })?;
        let announced = u64::from_le_bytes(header[5..ALONE_HEADER_LEN].try_into().expect("fixed five-byte tail"));
        let unpack_size = (announced != UNKNOWN_UNPACK_SIZE).then_some(announced);
        let mut facts =
            Self::decode_raw(header[..5].try_into().expect("fixed five-byte prefix"), &input[ALONE_HEADER_LEN..], unpack_size, limits)?;
        facts.input_consumed += ALONE_HEADER_LEN;
        Ok(facts)
    }

    /// Decodes a raw LZMA1 body given its five property bytes (`lc`/`lp`/`pb`
    /// byte plus the little-endian dictionary size) and an optional explicit
    /// unpack size. Without a size the body is decoded until its input is
    /// exhausted, which is how unknown-length `LZMA_ALONE` streams terminate.
    pub fn decode_raw(
        properties: &[u8; 5],
        input: &[u8],
        unpack_size: Option<u64>,
        limits: CodecLimits,
    ) -> Result<DecodeFacts<Vec<u8>>, CodecError> {
        let parsed = LzmaProperties::from_byte(properties[0], 0)?;
        let dictionary = dictionary_size(u32::from_le_bytes(properties[1..5].try_into().expect("fixed four-byte field")))?;
        let target = bounded_target(unpack_size, &limits)?;
        let mut output = Vec::new();
        if let Some(target) = target {
            output.reserve(target);
        }
        let mut range = RangeDecoder::new(input, 0)?;
        let mut core = LzmaCore::new(parsed);
        let scope = DecodeScope { pos_base: 0, dict_base: 0, window: dictionary, target };
        core.decode(&mut range, &mut output, scope, &limits)?;
        let produced = output.len();
        if let Some(announced) = unpack_size
            && announced != produced as u64
        {
            return Err(CodecError::InputExhausted { offset: range.consumed(), needed: (announced - produced as u64) as usize });
        }
        Ok(DecodeFacts {
            value: output,
            input_consumed: range.consumed(),
            output_produced: produced,
            complete: true,
            trailing_input: input.len() - range.consumed(),
            canonical: Canonicality::NotApplicable,
        })
    }
}

/// LZMA2 chunked-stream codec, as used by xz and 7z containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lzma2;

impl Lzma2 {
    /// Decodes an LZMA2 stream: a sequence of control-byte-led chunks ending
    /// with the `0x00` terminator. Uncompressed chunks copy their payload
    /// verbatim; LZMA chunks carry a one-byte property reset and a bounded
    /// range-coded body.
    pub fn decode(input: &[u8], limits: CodecLimits) -> Result<DecodeFacts<Vec<u8>>, CodecError> {
        let mut output = Vec::new();
        let mut core = LzmaCore::new(LzmaProperties { lc: 3, lp: 0, pb: 2 });
        let mut position = 0usize;
        let mut pos_base = 0usize;
        let mut dict_base = 0usize;
        loop {
            let control = *input.get(position).ok_or(CodecError::InputExhausted { offset: position, needed: 1 })?;
            position += 1;
            match control {
                0x00 => break,
                0x01 | 0x02 => {
                    if control == 0x01 {
                        dict_base = output.len();
                    }
                    let size = read_u16(input, &mut position)? as usize + 1;
                    let body = input.get(position..position + size).ok_or(CodecError::InputExhausted { offset: position, needed: size })?;
                    if (output.len() + size) as u64 > limits.output_limit() {
                        return Err(CodecError::OutputLimitExceeded { produced: output.len() as u64, limit: limits.output_limit() });
                    }
                    output.extend_from_slice(body);
                    position += size;
                }
                0x03..=0x7F => return Err(CodecError::InvalidByte { offset: position - 1, byte: control }),
                _ => {
                    let reset = (control >> 5) & 0x3;
                    let unpacked = (((u32::from(control) & 0x1F) << 16) | u32::from(read_u16(input, &mut position)?)) as usize + 1;
                    let packed = read_u16(input, &mut position)? as usize + 1;
                    if reset == 3 {
                        dict_base = output.len();
                    }
                    if reset >= 1 {
                        let properties = if reset >= 2 {
                            let byte = *input.get(position).ok_or(CodecError::InputExhausted { offset: position, needed: 1 })?;
                            position += 1;
                            LzmaProperties::from_byte(byte, position - 1)?
                        }
                        else {
                            core.properties()
                        };
                        core.reset(properties);
                        pos_base = output.len();
                    }
                    let body = input.get(position..position + packed).ok_or(CodecError::InputExhausted { offset: position, needed: packed })?;
                    let mut range = RangeDecoder::new(body, position)?;
                    let target = bounded_target(Some(unpacked as u64), &limits)?.map(|target| output.len() + target);
                    let scope = DecodeScope { pos_base, dict_base, window: MAX_DICTIONARY_SIZE as usize, target };
                    core.decode(&mut range, &mut output, scope, &limits)?;
                    position += packed;
                }
            }
        }
        let produced = output.len();
        Ok(DecodeFacts {
            value: output,
            input_consumed: position,
            output_produced: produced,
            complete: true,
            trailing_input: input.len() - position,
            canonical: Canonicality::NotApplicable,
        })
    }
}

/// Validates a declared dictionary size against the decoder cap.
fn dictionary_size(declared: u32) -> Result<usize, CodecError> {
    if declared > MAX_DICTIONARY_SIZE {
        return Err(CodecError::UnsupportedFeature { feature: CodecFeature::DictionarySize(declared) });
    }
    Ok(declared.max(MINIMUM_DICTIONARY_SIZE) as usize)
}

/// Converts an announced unpack size into a byte target, enforcing limits.
fn bounded_target(unpack_size: Option<u64>, limits: &CodecLimits) -> Result<Option<usize>, CodecError> {
    match unpack_size {
        Some(size) if size > limits.output_limit() => Err(CodecError::OutputLimitExceeded { produced: 0, limit: limits.output_limit() }),
        Some(size) => Ok(Some(size as usize)),
        None => Ok(None),
    }
}

/// Reads a big-endian `u16` and advances the cursor.
fn read_u16(input: &[u8], position: &mut usize) -> Result<u16, CodecError> {
    let field = input
        .get(*position..*position + 2)
        .ok_or(CodecError::InputExhausted { offset: *position, needed: 2usize.saturating_sub(input.len().saturating_sub(*position)) })?;
    *position += 2;
    Ok(u16::from_be_bytes([field[0], field[1]]))
}
