//! Canonical Huffman code construction and decoding.

use super::{BrotliError, bit::BitReader};

/// Maximum code length used by Brotli prefix codes.
const MAX_CODE_LENGTH: usize = 15;

/// A canonical prefix code over an alphabet of at most 704 symbols.
pub(crate) struct Huffman {
    /// Number of codes for each length, indexed by length (0 is unused).
    counts: [u32; MAX_CODE_LENGTH + 1],
    /// Symbols ordered by code length and then by symbol value.
    symbols: Vec<u16>,
    /// Symbol of a zero-bit code, produced only by "simple" trees with one symbol.
    single: Option<u16>,
}

impl Huffman {
    /// Builds a code from per-symbol code lengths.
    ///
    /// A tree with a single symbol must be built through [`Self::single`]
    /// because a zero-length code cannot be represented canonically.
    pub(crate) fn from_lengths(lengths: &[u8], alphabet_size: usize) -> Result<Self, BrotliError> {
        let mut counts = [0u32; MAX_CODE_LENGTH + 1];
        for &length in lengths.iter().take(alphabet_size) {
            if length as usize > MAX_CODE_LENGTH {
                return Err(BrotliError::Corrupt("code length exceeds 15"));
            }
            counts[length as usize] += 1;
        }
        let mut symbols = Vec::new();
        for length in 1..=MAX_CODE_LENGTH {
            for (symbol, &symbol_length) in lengths.iter().take(alphabet_size).enumerate() {
                if symbol_length as usize == length {
                    symbols.push(symbol as u16);
                }
            }
        }
        if symbols.is_empty() {
            return Err(BrotliError::Corrupt("empty prefix code"));
        }
        // Reject over-subscribed codes; incomplete codes are accepted here and
        // rejected by the callers that require a complete code.
        let mut space: i64 = 1 << MAX_CODE_LENGTH;
        for length in 1..=MAX_CODE_LENGTH {
            space -= i64::from(counts[length]) << (MAX_CODE_LENGTH - length);
        }
        if space < 0 {
            return Err(BrotliError::Corrupt("over-subscribed prefix code"));
        }
        Ok(Self { counts, symbols, single: None })
    }

    /// Builds the zero-bit code used for single-symbol "simple" trees.
    pub(crate) fn single(symbol: u16) -> Self {
        Self { counts: [0; MAX_CODE_LENGTH + 1], symbols: Vec::new(), single: Some(symbol) }
    }

    /// Decodes one symbol, consuming as many bits as the code requires.
    pub(crate) fn decode(&self, reader: &mut BitReader<'_>) -> Result<u16, BrotliError> {
        if let Some(symbol) = self.single {
            return Ok(symbol);
        }
        let mut code: u32 = 0;
        let mut first: u32 = 0;
        let mut index: u32 = 0;
        for length in 1..=MAX_CODE_LENGTH {
            code = (code << 1) | reader.read_bits(1)?;
            let count = self.counts[length];
            if code < first + count {
                let position = (index + (code - first)) as usize;
                return Ok(self.symbols[position]);
            }
            index += count;
            first = (first + count) << 1;
        }
        Err(BrotliError::Corrupt("invalid prefix code"))
    }
}
