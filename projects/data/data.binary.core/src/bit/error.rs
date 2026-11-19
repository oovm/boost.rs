//! Errors raised by bit-level cursors.

use std::fmt;

/// Failure of one bit-level read or write.
///
/// Codec crates map these into their own umbrella errors; the cursors
/// themselves stay independent of any format or algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitError {
    /// Input ended before the requested bits could be read.
    InputExhausted {
        /// Byte offset where more input was needed.
        offset: usize,
        /// Minimum additional bytes required.
        needed: usize,
    },
    /// A bit pattern was invalid for the current context.
    InvalidBitPattern {
        /// Byte offset at the time of the violation.
        offset: usize,
        /// Width of the invalid bit pattern.
        width: u8,
    },
}

impl fmt::Display for BitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputExhausted { offset, needed } => {
                write!(f, "bit input exhausted at offset {offset}, needed {needed} more byte(s)")
            }
            Self::InvalidBitPattern { offset, width } => {
                write!(f, "invalid {width}-bit pattern at offset {offset}")
            }
        }
    }
}

impl std::error::Error for BitError {}
