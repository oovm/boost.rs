use std::string::{String, ToString};

/// Describes what a [`ByteSink`] implementation can provide.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SinkCapabilities {
    /// Supports appending bytes at the current end.
    pub sequential_append: bool,
    /// Supports writing at arbitrary offsets.
    pub seek_write: bool,
    /// Supports patching previously written bytes in place.
    pub patch_in_place: bool,
    /// Reports the current written length.
    pub known_length: bool,
}

impl SinkCapabilities {
    /// Capabilities of an in-memory growable sink.
    pub const MEMORY: Self = Self { sequential_append: true, seek_write: true, patch_in_place: true, known_length: true };

    /// Capabilities of a strictly sequential sink.
    pub const SEQUENTIAL: Self = Self { sequential_append: true, seek_write: false, patch_in_place: false, known_length: true };
}

/// Outcome of a bounded write attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// All requested bytes were written.
    Complete,
    /// The write would exceed the configured budget.
    WouldExceedBudget {
        /// Bytes that would have been written.
        attempted: u64,
        /// Active byte budget.
        limit: u64,
    },
}

/// Bounded byte sink contract, symmetric to [`ByteSource`](super::ByteSource) but for generation.
pub trait ByteSink {
    /// Returns sink capabilities.
    fn capabilities(&self) -> SinkCapabilities;

    /// Returns the number of bytes currently written.
    fn len(&self) -> u64;

    /// Returns whether nothing has been written yet.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends bytes at the current end.
    fn append(&mut self, bytes: &[u8]) -> Result<WriteOutcome, WriteError>;

    /// Writes bytes at `offset`, growing the sink when supported.
    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<WriteOutcome, WriteError> {
        if !self.capabilities().seek_write {
            return Err(WriteError::Unsupported { operation: "write_at" });
        }
        self.write_at_impl(offset, bytes)
    }

    /// Patches a little-endian `u32` at `offset`.
    fn patch_u32_le(&mut self, offset: u64, value: u32) -> Result<(), WriteError> {
        if !self.capabilities().patch_in_place {
            return Err(WriteError::Unsupported { operation: "patch_u32_le" });
        }
        self.write_at(offset, &value.to_le_bytes())?;
        Ok(())
    }

    /// Implementation hook for sinks that support seek writes.
    fn write_at_impl(&mut self, offset: u64, bytes: &[u8]) -> Result<WriteOutcome, WriteError>;
}

/// Errors from sink writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteError {
    /// Underlying I/O failure.
    Io(String),
    /// The sink does not support the requested operation.
    Unsupported {
        /// Operation name.
        operation: &'static str,
    },
    /// Offset is out of range for a non-growable sink.
    OutOfRange {
        /// Requested offset.
        offset: u64,
        /// Current sink length.
        len: u64,
    },
    /// Write would exceed the configured byte budget.
    BudgetExceeded {
        /// Bytes that would be written.
        attempted: u64,
        /// Active byte budget.
        limit: u64,
    },
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(f, "io error: {message}"),
            Self::Unsupported { operation } => {
                write!(f, "unsupported sink operation: {operation}")
            }
            Self::OutOfRange { offset, len } => {
                write!(f, "write offset {offset} is out of range for length {len}")
            }
            Self::BudgetExceeded { attempted, limit } => {
                write!(f, "write would exceed byte budget: attempted {attempted}, limit {limit}")
            }
        }
    }
}

impl std::error::Error for WriteError {}

impl From<std::io::Error> for WriteError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}
