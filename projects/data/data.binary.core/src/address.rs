use std::fmt;

/// Identifier for an address space within one parse session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AddressSpaceId(pub u32);

/// Well-known address space kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AddressSpaceKind {
    /// Stable byte offsets of the original input.
    File,
    /// Bounded sub-view, such as an archive member or partition.
    View,
    /// Decompressed, decoded, or transformed logical bytes.
    Decoded,
    /// Format-declared virtual addresses used only for reference resolution.
    Virtual,
    /// Object numbering assigned by a format directory or index.
    Object,
}

/// A location inside a specific address space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Address {
    /// Address space containing the offset.
    pub space: AddressSpaceId,
    /// Byte offset from the start of the space.
    pub offset: u64,
}

/// A half-open byte interval `[start, start + len)` inside one address space.
///
/// This is a layout-domain extent relative to its address space, not a
/// source-bound byte range; see the crate-level type identity boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ByteExtent {
    /// Inclusive start offset.
    pub start: u64,
    /// Length in bytes.
    pub len: u64,
}

/// Errors produced by checked extent arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteExtentError {
    /// `start + len` overflowed `u64`.
    Overflow,
    /// The extent end exceeds the declared space length.
    OutOfBounds,
    /// `len` is zero where a non-empty extent is required.
    Empty,
}

impl fmt::Display for ByteExtentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow => write!(f, "byte extent overflow"),
            Self::OutOfBounds => write!(f, "byte extent exceeds space length"),
            Self::Empty => write!(f, "empty byte extent"),
        }
    }
}

impl std::error::Error for ByteExtentError {}

impl ByteExtent {
    /// Creates a new extent after validating `start + len`.
    pub fn new(start: u64, len: u64) -> Result<Self, ByteExtentError> {
        if len == 0 {
            return Err(ByteExtentError::Empty);
        }
        let end = start.checked_add(len).ok_or(ByteExtentError::Overflow)?;
        if end < start {
            return Err(ByteExtentError::Overflow);
        }
        Ok(Self { start, len })
    }

    /// Creates an extent that may be empty (for zero-length container payloads).
    pub fn new_allowing_empty(start: u64, len: u64) -> Result<Self, ByteExtentError> {
        if len == 0 {
            return Ok(Self { start, len: 0 });
        }
        Self::new(start, len)
    }

    /// Returns the exclusive end offset, if it can be computed.
    pub fn end(self) -> Result<u64, ByteExtentError> {
        self.start.checked_add(self.len).ok_or(ByteExtentError::Overflow)
    }

    /// Returns whether `self` is fully contained in `[0, space_len)`.
    pub fn fits_in(self, space_len: u64) -> Result<(), ByteExtentError> {
        let end = self.end()?;
        if end > space_len {
            return Err(ByteExtentError::OutOfBounds);
        }
        Ok(())
    }

    /// Returns whether two extents overlap in the same address space.
    pub fn overlaps(self, other: Self) -> Result<bool, ByteExtentError> {
        let self_end = self.end()?;
        let other_end = other.end()?;
        Ok(self.start < other_end && other.start < self_end)
    }
}

impl Address {
    /// Creates a new address.
    pub const fn new(space: AddressSpaceId, offset: u64) -> Self {
        Self { space, offset }
    }
}
