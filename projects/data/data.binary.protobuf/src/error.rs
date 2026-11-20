//! Error surface for protobuf wire decoding and encoding.

/// All failures produced while reading or writing the protobuf wire format.
///
/// Every variant names a structural property of the stream rather than a
/// schema, so a hand-written model parser can decide whether a given failure
/// is "this file is not protobuf" or "this file violates my schema" without
/// guessing from a generic message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtoError {
    #[error("field {field} declares wire type {raw}, which is not defined")]
    UnknownWireType { field: u32, raw: u8 },

    #[error("field number {0} is not valid (must be 1..=536870911 and outside 19000..=19999)")]
    InvalidFieldNumber(u32),

    #[error("varint at offset {offset} does not terminate within {max} bytes")]
    VarintTooLong { offset: usize, max: usize },

    #[error("varint at offset {offset} overflows {bits} bits")]
    VarintOverflow { offset: usize, bits: u32 },

    #[error("truncated input: need {need} more bytes at offset {offset}, have {have}")]
    Truncated { offset: usize, need: usize, have: usize },

    #[error("length-delimited field {field} declares {declared} bytes but only {available} remain")]
    LengthBeyondEnd { field: u32, declared: u64, available: usize },

    #[error("sub-message nesting exceeds the limit of {0} levels")]
    TooDeep(usize),

    #[error("group started at field {open} ended at field {close}, which does not match")]
    GroupMismatch { open: u32, close: u32 },

    #[error("group opened at field {0} is not closed before the end of the input")]
    GroupUnterminated(u32),

    #[error("unexpected end-group for field {0} outside any group")]
    StrayEndGroup(u32),

    #[error("string field {0} is not valid UTF-8")]
    InvalidUtf8(u32),

    #[error("packed field {field} carries an element of wire type {raw}, which cannot be packed")]
    UnpackableWireType { field: u32, raw: u8 },

    #[error("packed field {field} has {left} trailing bytes that do not form a whole element")]
    PackedRemainder { field: u32, left: usize },

    #[error("writer cannot encode: {0}")]
    Writer(&'static str),
}

impl ProtoError {
    /// Number of bytes still missing from a fixed-width read.
    pub(crate) fn truncated(offset: usize, need: usize, have: usize) -> Self {
        Self::Truncated { offset, need, have }
    }
}
