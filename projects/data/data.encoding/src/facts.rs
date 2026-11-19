//! Decode result facts shared across representation transforms.

/// Whether an encoding is in canonical form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Canonicality {
    /// Encoding is canonical.
    Canonical,
    /// Encoding is valid but non-canonical.
    NonCanonical,
    /// Canonicality does not apply to this transform.
    NotApplicable,
}

/// Facts produced by a decode operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeFacts<T> {
    /// Decoded value.
    pub value: T,
    /// Input bytes consumed.
    pub input_consumed: usize,
    /// Output bytes produced (for byte-producing transforms).
    pub output_produced: usize,
    /// Whether the transform completed successfully.
    pub complete: bool,
    /// Trailing input bytes after a complete decode.
    pub trailing_input: usize,
    /// Canonicality of the input encoding.
    pub canonical: Canonicality,
}

impl<T> DecodeFacts<T> {
    /// Whether the entire input slice was consumed with no trailing bytes.
    #[inline]
    pub fn fully_consumed(&self, total_input: usize) -> bool {
        self.complete && self.trailing_input == 0 && self.input_consumed == total_input
    }
}

/// Byte decode result with attached facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ByteDecode {
    /// Decoded bytes.
    bytes: Vec<u8>,
    /// Decode facts.
    facts: DecodeFacts<()>,
}

impl ByteDecode {
    /// Creates a byte decode result.
    pub fn new(bytes: Vec<u8>, facts: DecodeFacts<()>) -> Self {
        Self { bytes, facts }
    }

    /// Returns the decoded bytes.
    #[inline]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns decode facts.
    #[inline]
    pub fn facts(&self) -> &DecodeFacts<()> {
        &self.facts
    }

    /// Consumes the wrapper and returns the decoded bytes.
    #[inline]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}
