//! Protobuf wire encoder.
//!
//! [`Writer`] appends tags and values to a growable buffer. It validates field
//! numbers the same way [`Reader`](crate::Reader) does, so a schema mistake
//! surfaces as an error instead of a silently unreadable message.

use crate::error::ProtoError;
use crate::wire::{Tag, WireType, zigzag_encode_i32, zigzag_encode_i64};

/// A scalar type this writer can place inside a packed repeated field.
pub trait PackedWrite {
    /// Wire type its elements use.
    const WIRE: WireType;

    /// Append one element, without a tag.
    fn write(&self, out: &mut Writer);

    /// Bytes this element contributes to a packed body.
    fn encoded_len(&self) -> usize;
}

/// Growable protobuf message buffer.
#[derive(Debug, Clone, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

/// Number of bytes a base-128 varint occupies.
const fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

impl Writer {
    /// Create an empty writer.
    pub fn new() -> Self {
        Writer { buf: Vec::new() }
    }

    /// Create a writer with room for `capacity` bytes.
    pub fn with_capacity(capacity: usize) -> Self {
        Writer { buf: Vec::with_capacity(capacity) }
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been written yet.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Borrow the encoded message.
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    /// Consume the writer and return the encoded message.
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    // ------------------------------------------------------------- untagged ---

    /// Append a tag for `number`/`wire`.
    pub fn write_tag(&mut self, number: u32, wire: WireType) -> Result<(), ProtoError> {
        Tag::validate(number)?;
        self.write_varint(((number as u64) << 3) | wire.raw() as u64);
        Ok(())
    }

    /// Append a base-128 varint.
    pub fn write_varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push((v as u8 & 0x7f) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }

    /// Append a `fixed32`.
    pub fn write_fixed32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Append a `fixed64`.
    pub fn write_fixed64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    // --------------------------------------------------------------- fields ---

    /// Write a `uint32` field.
    pub fn write_u32(&mut self, number: u32, v: u32) -> Result<(), ProtoError> {
        self.write_tag(number, WireType::Varint)?;
        self.write_varint(v as u64);
        Ok(())
    }

    /// Write a `uint64` field.
    pub fn write_u64(&mut self, number: u32, v: u64) -> Result<(), ProtoError> {
        self.write_tag(number, WireType::Varint)?;
        self.write_varint(v);
        Ok(())
    }

    /// Write an `int32` field, sign-extending negatives to 64 bits as the
    /// format requires.
    pub fn write_i32(&mut self, number: u32, v: i32) -> Result<(), ProtoError> {
        self.write_u64(number, v as u64)
    }

    /// Write an `int64` field.
    pub fn write_i64(&mut self, number: u32, v: i64) -> Result<(), ProtoError> {
        self.write_u64(number, v as u64)
    }

    /// Write a `sint32` field.
    pub fn write_sint32(&mut self, number: u32, v: i32) -> Result<(), ProtoError> {
        self.write_u32(number, zigzag_encode_i32(v))
    }

    /// Write a `sint64` field.
    pub fn write_sint64(&mut self, number: u32, v: i64) -> Result<(), ProtoError> {
        self.write_u64(number, zigzag_encode_i64(v))
    }

    /// Write a `bool` field.
    pub fn write_bool(&mut self, number: u32, v: bool) -> Result<(), ProtoError> {
        self.write_u64(number, u64::from(v))
    }

    /// Write an `enum` field from its raw number.
    pub fn write_enum(&mut self, number: u32, v: i32) -> Result<(), ProtoError> {
        self.write_i32(number, v)
    }

    /// Write a `fixed32` field.
    pub fn write_fixed32_field(&mut self, number: u32, v: u32) -> Result<(), ProtoError> {
        self.write_tag(number, WireType::Fixed32)?;
        self.write_fixed32(v);
        Ok(())
    }

    /// Write a `fixed64` field.
    pub fn write_fixed64_field(&mut self, number: u32, v: u64) -> Result<(), ProtoError> {
        self.write_tag(number, WireType::Fixed64)?;
        self.write_fixed64(v);
        Ok(())
    }

    /// Write a `float` field.
    pub fn write_float(&mut self, number: u32, v: f32) -> Result<(), ProtoError> {
        self.write_fixed32_field(number, v.to_bits())
    }

    /// Write a `double` field.
    pub fn write_double(&mut self, number: u32, v: f64) -> Result<(), ProtoError> {
        self.write_fixed64_field(number, v.to_bits())
    }

    /// Write a `bytes` field.
    pub fn write_bytes(&mut self, number: u32, v: &[u8]) -> Result<(), ProtoError> {
        self.write_tag(number, WireType::LengthDelimited)?;
        self.write_varint(v.len() as u64);
        self.buf.extend_from_slice(v);
        Ok(())
    }

    /// Write a `string` field.
    pub fn write_string(&mut self, number: u32, v: &str) -> Result<(), ProtoError> {
        self.write_bytes(number, v.as_bytes())
    }

    /// Write an embedded message from an already-encoded body.
    pub fn write_message(&mut self, number: u32, body: &[u8]) -> Result<(), ProtoError> {
        self.write_bytes(number, body)
    }

    /// Write a packed repeated field.
    pub fn write_packed<T: PackedWrite>(&mut self, number: u32, values: &[T]) -> Result<(), ProtoError> {
        self.write_tag(number, WireType::LengthDelimited)?;
        let body_len = values.iter().map(PackedWrite::encoded_len).sum::<usize>();
        self.write_varint(body_len as u64);
        for v in values {
            v.write(self);
        }
        Ok(())
    }

    /// Append a raw end-group marker for `number`.
    pub fn write_end_group(&mut self, number: u32) -> Result<(), ProtoError> {
        Tag::validate(number)?;
        self.write_varint(((number as u64) << 3) | 4);
        Ok(())
    }
}

macro_rules! impl_packed_varint_write {
    ($($ty:ty),* $(,)?) => {
        $(
            impl PackedWrite for $ty {
                const WIRE: WireType = WireType::Varint;

                fn write(&self, out: &mut Writer) {
                    out.write_varint(*self as u64);
                }

                fn encoded_len(&self) -> usize {
                    varint_len(*self as u64)
                }
            }
        )*
    };
}

impl_packed_varint_write!(u32, u64, i32, i64);

impl PackedWrite for bool {
    const WIRE: WireType = WireType::Varint;

    fn write(&self, out: &mut Writer) {
        out.write_varint(u64::from(*self));
    }

    fn encoded_len(&self) -> usize {
        1
    }
}

impl PackedWrite for f32 {
    const WIRE: WireType = WireType::Fixed32;

    fn write(&self, out: &mut Writer) {
        out.write_fixed32(self.to_bits());
    }

    fn encoded_len(&self) -> usize {
        4
    }
}

impl PackedWrite for f64 {
    const WIRE: WireType = WireType::Fixed64;

    fn write(&self, out: &mut Writer) {
        out.write_fixed64(self.to_bits());
    }

    fn encoded_len(&self) -> usize {
        8
    }
}
