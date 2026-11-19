use std::string::String;

use crate::range::ByteRange;

/// Outcome of a bounded read attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOutcome {
    /// All requested bytes were read.
    Complete,
    /// Fewer bytes than requested were available before EOF.
    Partial {
        /// Number of bytes written into the destination buffer.
        bytes_read: u64,
    },
    /// The read starts at or past EOF.
    Eof,
    /// The requested interval is not currently available.
    Unavailable {
        /// Missing interval.
        range: ByteRange,
        /// Why the interval is unavailable.
        reason: String,
    },
}

/// Result of reading into a caller-provided buffer.
pub type ReadResult = Result<ReadOutcome, std::io::Error>;
