//! Wire types, tag encoding and the hard limits this substrate enforces.

use crate::error::ProtoError;

/// Largest number of bytes a base-128 varint may occupy for a 64-bit value.
pub const MAX_VARINT_BYTES: usize = 10;

/// Highest field number protobuf allows (`2^29 - 1`).
pub const MAX_FIELD_NUMBER: u32 = 536_870_911;

/// Start of the range protobuf reserves for internal use.
pub const RESERVED_FIELD_FROM: u32 = 19_000;

/// End of the range protobuf reserves for internal use (inclusive).
pub const RESERVED_FIELD_TO: u32 = 19_999;

/// Default cap on sub-message and group nesting, guarding against
/// recursively self-nesting input driving the stack.
pub const DEFAULT_MAX_DEPTH: usize = 32;

/// The five wire types defined by the protobuf encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireType {
    /// Base-128 varint: `int32`, `int64`, `uint32`, `uint64`, `sint32`,
    /// `sint64`, `bool`, `enum`.
    Varint,
    /// Four fixed bytes: `fixed32`, `sfixed32`, `float`.
    Fixed32,
    /// Eight fixed bytes: `fixed64`, `sfixed64`, `double`.
    Fixed64,
    /// Varint length followed by that many bytes: `string`, `bytes`, embedded
    /// messages, and packed repeated scalars.
    LengthDelimited,
    /// Deprecated group: a `StartGroup` tag, fields, then a matching
    /// `EndGroup` tag.
    Group(u32),
}

impl WireType {
    /// Decode the low three bits of a tag.
    ///
    /// Wire type `4` (`EndGroupTag`) never appears on its own — it is produced
    /// by [`Reader::next_tag`](crate::Reader::next_tag) as
    /// [`WireType::Group`] once the matching start is known — so `4` maps to
    /// [`ProtoError::UnknownWireType`] here.
    pub(crate) fn from_raw(raw: u8, field: u32) -> Result<Self, ProtoError> {
        match raw & 0b111 {
            0 => Ok(WireType::Varint),
            1 => Ok(WireType::Fixed64),
            2 => Ok(WireType::LengthDelimited),
            3 => Ok(WireType::Group(field)),
            5 => Ok(WireType::Fixed32),
            other => Err(ProtoError::UnknownWireType { field, raw: other }),
        }
    }

    /// Encode to the low three bits of a tag.
    pub const fn raw(self) -> u8 {
        match self {
            WireType::Varint => 0,
            WireType::Fixed64 => 1,
            WireType::LengthDelimited => 2,
            WireType::Group(_) => 3,
            WireType::Fixed32 => 5,
        }
    }

    /// Whether this wire type can appear inside a packed repeated field.
    ///
    /// Groups and length-delimited values are excluded: the former is not
    /// packable by definition, the latter is how a *non*-packed repeated
    /// `string`/`bytes`/message field is encoded one element per tag.
    pub const fn packable(self) -> bool {
        matches!(self, WireType::Varint | WireType::Fixed32 | WireType::Fixed64)
    }
}

/// A parsed tag: the field number and the wire type that follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tag {
    pub number: u32,
    pub wire: WireType,
}

impl Tag {
    /// Split a raw varint tag into field number and wire type, validating the
    /// field number against the protobuf range rules.
    pub fn from_raw(raw: u64) -> Result<Self, ProtoError> {
        let number = (raw >> 3) as u32;
        Self::validate(number)?;
        let wire = WireType::from_raw(raw as u8, number)?;
        Ok(Tag { number, wire })
    }

    pub(crate) fn validate(number: u32) -> Result<(), ProtoError> {
        if number == 0 || number > MAX_FIELD_NUMBER || (RESERVED_FIELD_FROM..=RESERVED_FIELD_TO).contains(&number) {
            return Err(ProtoError::InvalidFieldNumber(number));
        }
        Ok(())
    }

    /// Encode this tag as the varint that precedes its value.
    pub const fn to_raw(self) -> u64 {
        ((self.number as u64) << 3) | self.wire.raw() as u64
    }
}

/// Zig-zag encode a signed 32-bit integer.
pub const fn zigzag_encode_i32(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

/// Zig-zag decode a 32-bit varint back to its signed value.
pub const fn zigzag_decode_i32(v: u32) -> i32 {
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

/// Zig-zag encode a signed 64-bit integer.
pub const fn zigzag_encode_i64(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// Zig-zag decode a 64-bit varint back to its signed value.
pub const fn zigzag_decode_i64(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}
