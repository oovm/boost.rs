//! Encoding errors independent of format adapters and parse budgets.

/// Errors raised by representation transforms in `data-encoding`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodingError {
    /// Input ended before the transform could complete.
    InputExhausted {
        /// Byte offset where more input was needed.
        offset: usize,
        /// Minimum additional bytes required.
        needed: usize,
    },
    /// A byte violated the expected alphabet or encoding.
    InvalidByte {
        /// Byte offset of the violation.
        offset: usize,
        /// The offending byte.
        byte: u8,
    },
    /// Padding rules were violated.
    InvalidPadding {
        /// Byte offset of the padding violation.
        offset: usize,
    },
    /// An integer exceeded the allowed bit width.
    IntegerOverflow {
        /// Byte offset where overflow was detected.
        offset: usize,
        /// Maximum allowed bit width.
        width: u32,
    },
    /// Encoding is valid but not in canonical form.
    NonCanonicalEncoding {
        /// Byte offset of the non-canonical encoding.
        offset: usize,
    },
    /// Internal invariant was violated (should not occur on valid inputs).
    InternalInvariantViolation,
}

impl std::fmt::Display for EncodingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InputExhausted { offset, needed } => {
                write!(f, "encoding input exhausted at offset {offset}, needed {needed} more byte(s)")
            }
            Self::InvalidByte { offset, byte } => {
                write!(f, "encoding invalid byte 0x{byte:02X} at offset {offset}")
            }
            Self::InvalidPadding { offset } => {
                write!(f, "encoding invalid padding at offset {offset}")
            }
            Self::IntegerOverflow { offset, width } => {
                write!(f, "encoding integer overflow at offset {offset} (width {width})")
            }
            Self::NonCanonicalEncoding { offset } => {
                write!(f, "encoding non-canonical encoding at offset {offset}")
            }
            Self::InternalInvariantViolation => f.write_str("encoding internal invariant violation"),
        }
    }
}

impl std::error::Error for EncodingError {}
