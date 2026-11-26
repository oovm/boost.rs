//! Least-significant-bit-first reader used by the Brotli bit stream.
//!
//! Brotli packs values starting at the least significant bit of each byte, so
//! the reader keeps a little-endian accumulator. Huffman codes are read one bit
//! at a time in most-significant-bit order, which the decoder in
//! [`super::huffman`] performs on top of this reader.

use super::BrotliError;

/// A bounded bit reader over an immutable input slice.
pub(crate) struct BitReader<'a> {
    data: &'a [u8],
    next_byte: usize,
    buffer: u64,
    buffered_bits: u32,
}

impl<'a> BitReader<'a> {
    /// Creates a reader positioned at the first bit of `data`.
    pub(crate) fn new(data: &'a [u8]) -> Self {
        Self { data, next_byte: 0, buffer: 0, buffered_bits: 0 }
    }

    /// Refills the accumulator with as many whole bytes as fit.
    fn fill(&mut self) {
        while self.buffered_bits <= 56 && self.next_byte < self.data.len() {
            self.buffer |= u64::from(self.data[self.next_byte]) << self.buffered_bits;
            self.next_byte += 1;
            self.buffered_bits += 8;
        }
    }

    /// Reads up to 32 bits, returning `Truncated` if the input is exhausted.
    pub(crate) fn read_bits(&mut self, count: u32) -> Result<u32, BrotliError> {
        debug_assert!(count <= 32);
        if count == 0 {
            return Ok(0);
        }
        if self.buffered_bits < count {
            self.fill();
            if self.buffered_bits < count {
                return Err(BrotliError::Truncated);
            }
        }
        let mask = (1u64 << count) - 1;
        let value = (self.buffer & mask) as u32;
        self.buffer >>= count;
        self.buffered_bits -= count;
        Ok(value)
    }

    /// Drops bits until the next read starts on a byte boundary.
    pub(crate) fn align_to_byte(&mut self) {
        let drop = self.buffered_bits % 8;
        self.buffer >>= drop;
        self.buffered_bits -= drop;
    }

    /// Number of input bytes the decoder has consumed.
    ///
    /// Whole bytes still sitting in the accumulator are not counted, but the
    /// byte holding partially consumed bits is: the result is the end offset of
    /// the bitstream read so far, so `input.len() - consumed_bytes()` is the
    /// trailing input after a complete stream.
    pub(crate) fn consumed_bytes(&self) -> usize {
        self.next_byte - (self.buffered_bits / 8) as usize
    }

    /// Reads one byte after [`Self::align_to_byte`].
    pub(crate) fn read_aligned_byte(&mut self) -> Result<u8, BrotliError> {
        if self.buffered_bits >= 8 {
            let byte = (self.buffer & 0xff) as u8;
            self.buffer >>= 8;
            self.buffered_bits -= 8;
            Ok(byte)
        }
        else if self.next_byte < self.data.len() {
            let byte = self.data[self.next_byte];
            self.next_byte += 1;
            Ok(byte)
        }
        else {
            Err(BrotliError::Truncated)
        }
    }
}
