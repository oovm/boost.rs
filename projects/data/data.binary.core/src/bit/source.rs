//! Bit-source contract shared by finite and resumable readers.

use crate::bit::{BitError, BitPosition, BitReader};

/// Reads bits from a bounded or incrementally supplied input.
///
/// [`BitReader`] implements this for a finite slice. Streaming decoders
/// implement it over an input queue that grows between reads, and report a
/// shortage as [`BitError::InputExhausted`] so the caller can resume after
/// supplying more bytes.
pub trait BitSource {
    /// Reads `count` bits in stream order.
    fn read_bits(&mut self, count: u8) -> Result<u32, BitError>;

    /// Returns the position of the next bit to read.
    fn position(&self) -> BitPosition;
}

impl BitSource for BitReader<'_> {
    #[inline]
    fn read_bits(&mut self, count: u8) -> Result<u32, BitError> {
        BitReader::read_bits(self, count)
    }

    #[inline]
    fn position(&self) -> BitPosition {
        BitReader::position(self)
    }
}
