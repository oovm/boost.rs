//! Byte-addressed range decoder for LZMA streams.
//!
//! The LZMA range coder is big-endian: the stream opens with one reserved
//! byte and a four-byte initial code, and every renormalization pulls one
//! byte off the tail. The decoder keeps an explicit cursor so exhausted
//! input can be reported with the byte offset that was missing.

use crate::CodecError;

/// Range threshold below which one input byte is renormalized in.
const TOP_VALUE: u32 = 1 << 24;

/// Range decoder over a bounded slice of an LZMA stream.
pub(crate) struct RangeDecoder<'a> {
    /// Backing bytes of the encoded body.
    bytes: &'a [u8],
    /// Cursor into `bytes`; the next byte a renormalization would pull.
    cursor: usize,
    /// Absolute offset of `bytes[0]`, used to report exhausted input.
    base: usize,
    /// Current interval width.
    range: u32,
    /// Current interval offset inside `range`.
    code: u32,
}

impl<'a> RangeDecoder<'a> {
    /// Reads the five-byte initialization (reserved byte, then the big-endian code).
    pub(crate) fn new(bytes: &'a [u8], base: usize) -> Result<Self, CodecError> {
        let mut decoder = Self { bytes, cursor: 0, base, range: u32::MAX, code: 0 };
        decoder.read_byte()?;
        let mut code = 0u32;
        for _ in 0..4 {
            code = (code << 8) | u32::from(decoder.read_byte()?);
        }
        decoder.code = code;
        Ok(decoder)
    }

    /// Number of body bytes the decoder has pulled into its state.
    pub(crate) fn consumed(&self) -> usize {
        self.cursor
    }

    /// Pulls one input byte, reporting exhaustion with an absolute offset.
    fn read_byte(&mut self) -> Result<u8, CodecError> {
        let byte = *self.bytes.get(self.cursor).ok_or(CodecError::InputExhausted { offset: self.base + self.cursor, needed: 1 })?;
        self.cursor += 1;
        Ok(byte)
    }

    /// Renormalizes the interval by shifting one byte in when it got narrow.
    fn normalize(&mut self) -> Result<(), CodecError> {
        if self.range < TOP_VALUE {
            self.range <<= 8;
            self.code = (self.code << 8) | u32::from(self.read_byte()?);
        }
        Ok(())
    }

    /// Decodes one bit against an adaptive probability and updates it.
    pub(crate) fn decode_bit(&mut self, prob: &mut u16) -> Result<u32, CodecError> {
        self.normalize()?;
        let bound = (self.range >> 11) * u32::from(*prob);
        if self.code < bound {
            *prob += (0x800 - *prob) >> 5;
            self.range = bound;
            Ok(0)
        }
        else {
            *prob -= *prob >> 5;
            self.code -= bound;
            self.range -= bound;
            Ok(1)
        }
    }

    /// Decodes `count` uniformly distributed bits (direct distance bits).
    pub(crate) fn decode_direct(&mut self, count: u32) -> Result<u32, CodecError> {
        let mut value = 0u32;
        for _ in 0..count {
            self.normalize()?;
            self.range >>= 1;
            let bit = u32::from(self.code >= self.range);
            self.code -= self.range * bit;
            value = (value << 1) | bit;
        }
        Ok(value)
    }

    /// Decodes a full bit tree, most significant bit first.
    pub(crate) fn decode_tree(&mut self, num_bits: u32, probs: &mut [u16]) -> Result<u32, CodecError> {
        let mut node = 1usize;
        for _ in 0..num_bits {
            let bit = self.decode_bit(&mut probs[node])?;
            node = (node << 1) | bit as usize;
        }
        Ok((node - (1 << num_bits)) as u32)
    }

    /// Decodes a reverse bit tree, least significant bit first, at `offset`.
    pub(crate) fn decode_tree_reverse(&mut self, num_bits: u32, probs: &mut [u16], offset: usize) -> Result<u32, CodecError> {
        let mut node = 1usize;
        let mut value = 0u32;
        for index in 0..num_bits {
            let bit = self.decode_bit(&mut probs[offset + node])?;
            node = (node << 1) | bit as usize;
            value |= bit << index;
        }
        Ok(value)
    }
}
