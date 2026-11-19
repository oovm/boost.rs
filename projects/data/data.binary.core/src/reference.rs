use std::fmt;

use crate::address::{AddressSpaceId, ByteExtent, ByteExtentError};

/// Semantic category of a format reference field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ReferenceKind {
    /// Offset from the start of a source space.
    AbsoluteOffset,
    /// Offset relative to a basis range.
    RelativeOffset,
    /// Offset inside a parent segment.
    SegmentOffset,
    /// Virtual address resolved through a mapping table.
    VirtualAddress,
    /// Index into a directory or page table.
    DirectoryIndex,
    /// Cross-reference object identifier.
    ObjectId,
    /// Next range determined by a length prefix.
    LengthPrefixed,
    /// Footer or directory back-reference into body bytes.
    FooterBackref,
}

/// Evidence recorded for one resolved or attempted jump.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ReferenceEvidence {
    /// Reference category declared by the adapter.
    pub kind: ReferenceKind,
    /// Human-readable source field label.
    pub source_field: String,
    /// Basis range for relative or segment offsets.
    pub basis: Option<ByteExtent>,
    /// Resolved target range in `space`.
    pub target: ByteExtent,
    /// Address space containing `target`.
    pub space: AddressSpaceId,
}

/// Errors from checked jump arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JumpError {
    /// Integer overflow while computing a target range.
    Overflow,
    /// Target range exceeds the address space bounds.
    OutOfBounds,
    /// Basis and target are not in the same address space.
    SpaceMismatch,
    /// Underlying extent arithmetic failed.
    Extent(ByteExtentError),
}

impl fmt::Display for JumpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow => write!(f, "jump target overflow"),
            Self::OutOfBounds => write!(f, "jump target out of bounds"),
            Self::SpaceMismatch => write!(f, "address space mismatch"),
            Self::Extent(error) => write!(f, "extent error: {error}"),
        }
    }
}

impl std::error::Error for JumpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Extent(error) => Some(error),
            _ => None,
        }
    }
}

impl From<ByteExtentError> for JumpError {
    fn from(error: ByteExtentError) -> Self {
        Self::Extent(error)
    }
}

/// Identity used to detect revisits during bounded graph traversal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct VisitKey {
    /// Address space being visited.
    pub space: AddressSpaceId,
    /// Byte offset within the space.
    pub offset: u64,
}

impl VisitKey {
    /// Creates a visit key for one address.
    pub const fn new(space: AddressSpaceId, offset: u64) -> Self {
        Self { space, offset }
    }
}

/// Resolves an absolute offset into a bounded byte extent.
pub fn checked_absolute_jump(_space: AddressSpaceId, space_len: u64, offset: u64, len: u64) -> Result<ByteExtent, JumpError> {
    let target = ByteExtent::new(offset, len)?;
    target.fits_in(space_len)?;
    Ok(target)
}

/// Resolves a relative offset from a basis extent start.
pub fn checked_relative_jump(
    _space: AddressSpaceId,
    space_len: u64,
    basis: ByteExtent,
    relative: u64,
    len: u64,
) -> Result<ByteExtent, JumpError> {
    let start = basis.start.checked_add(relative).ok_or(JumpError::Overflow)?;
    let target = ByteExtent::new(start, len)?;
    target.fits_in(space_len)?;
    Ok(target)
}
