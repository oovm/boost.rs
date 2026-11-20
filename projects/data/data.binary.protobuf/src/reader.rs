//! Bounds-checked reader for the protobuf wire format.
//!
//! [`Reader`] walks a byte slice without copying it. It knows nothing about
//! schemas: it produces [`Tag`]s and primitive values, skips anything a caller
//! does not recognise, and refuses input that cannot be a well-formed message.
//! Hand-written model parsers build their schema knowledge on top of this.

use core::marker::PhantomData;

use crate::error::ProtoError;
use crate::wire::{DEFAULT_MAX_DEPTH, MAX_VARINT_BYTES, Tag, WireType, zigzag_decode_i32, zigzag_decode_i64};

/// A borrowed, forward-only cursor over one protobuf message.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    depth: usize,
    max_depth: usize,
}

/// One decoded field: its number and the value bytes that followed it.
///
/// Used for generic traversal and unknown-field retention, where the caller has
/// no schema to select a typed accessor with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field<'a> {
    pub number: u32,
    pub value: Value<'a>,
}

/// The payload of a [`Field`], still in wire terms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value<'a> {
    Varint(u64),
    Fixed32(u32),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Group(Vec<Field<'a>>),
}

/// What a tag scan produced: a field start, or the end of the enclosing group.
enum Scan {
    Start(Tag),
    End(u32),
}

impl<'a> Reader<'a> {
    /// Wrap a complete message.
    pub fn new(buf: &'a [u8]) -> Self {
        Self::with_max_depth(buf, DEFAULT_MAX_DEPTH)
    }

    /// Wrap a complete message with an explicit nesting cap.
    pub fn with_max_depth(buf: &'a [u8], max_depth: usize) -> Self {
        Reader { buf, pos: 0, depth: 0, max_depth }
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Whether the message has been fully consumed.
    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// Absolute offset of the next byte to read.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Current nesting level: `0` for a top-level message.
    pub fn depth(&self) -> usize {
        self.depth
    }

    // ---------------------------------------------------------------- tags ---

    /// Read the next field tag, or `None` once the message is exhausted.
    ///
    /// An end-group tag outside a group is a structural error: the caller's
    /// message is not self-delimiting.
    pub fn next_tag(&mut self) -> Result<Option<Tag>, ProtoError> {
        match self.scan_tag()? {
            Some(Scan::Start(tag)) => Ok(Some(tag)),
            Some(Scan::End(field)) => Err(ProtoError::StrayEndGroup(field)),
            None => Ok(None),
        }
    }

    /// Read a tag without treating an end-group marker as an error. Group
    /// bodies use this so they can stop on their own terminator.
    fn scan_tag(&mut self) -> Result<Option<Scan>, ProtoError> {
        if self.is_empty() {
            return Ok(None);
        }
        let raw = self.read_varint()?;
        let number = (raw >> 3) as u32;
        let low = (raw & 0b111) as u8;
        if low == 4 {
            Tag::validate(number)?;
            return Ok(Some(Scan::End(number)));
        }
        let tag = Tag::from_raw(raw)?;
        Ok(Some(Scan::Start(tag)))
    }

    // ------------------------------------------------------------ primitives ---

    /// Read a single byte.
    fn read_byte(&mut self) -> Result<u8, ProtoError> {
        let Some(&byte) = self.buf.get(self.pos) else {
            return Err(ProtoError::Truncated { offset: self.pos, need: 1, have: self.remaining() });
        };
        self.pos += 1;
        Ok(byte)
    }

    /// Borrow the next `n` bytes.
    fn read_take(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        let offset = self.pos;
        let Some(end) = offset.checked_add(n) else {
            return Err(ProtoError::Truncated { offset, need: n, have: self.remaining() });
        };
        if end > self.buf.len() {
            return Err(ProtoError::Truncated { offset, need: n, have: self.remaining() });
        }
        self.pos = end;
        Ok(&self.buf[offset..end])
    }

    /// Read a base-128 varint as `u64`.
    ///
    /// Rejects encodings longer than [`MAX_VARINT_BYTES`] and those whose
    /// surplus high bits would silently truncate a 64-bit value.
    pub fn read_varint(&mut self) -> Result<u64, ProtoError> {
        let start = self.pos;
        let mut acc: u64 = 0;
        let mut shift: u32 = 0;
        loop {
            let byte = self.read_byte()?;
            if shift == 63 {
                // Tenth and final byte: one value bit at most, no continuation.
                if byte > 1 {
                    return Err(if byte & 0x80 != 0 {
                        ProtoError::VarintTooLong { offset: start, max: MAX_VARINT_BYTES }
                    } else {
                        ProtoError::VarintOverflow { offset: start, bits: 64 }
                    });
                }
                acc |= (byte as u64) << shift;
                return Ok(acc);
            }
            acc |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(acc);
            }
            shift += 7;
        }
    }

    /// Read a `uint32`. Values that do not fit in 32 bits are rejected rather
    /// than truncated.
    pub fn read_u32(&mut self) -> Result<u32, ProtoError> {
        let raw = self.read_varint()?;
        u32::try_from(raw).map_err(|_| ProtoError::VarintOverflow { offset: self.pos, bits: 32 })
    }

    /// Read a `uint64`.
    pub fn read_u64(&mut self) -> Result<u64, ProtoError> {
        self.read_varint()
    }

    /// Read an `int32`.
    ///
    /// Protobuf writes negative `int32` as the sign-extended 64-bit varint, so
    /// the low 32 bits are the value and the high bits carry only the sign.
    pub fn read_i32(&mut self) -> Result<i32, ProtoError> {
        Ok(self.read_varint()? as i32)
    }

    /// Read an `int64`.
    pub fn read_i64(&mut self) -> Result<i64, ProtoError> {
        Ok(self.read_varint()? as i64)
    }

    /// Read a `sint32` (zig-zag).
    pub fn read_sint32(&mut self) -> Result<i32, ProtoError> {
        Ok(zigzag_decode_i32(self.read_u32()?))
    }

    /// Read a `sint64` (zig-zag).
    pub fn read_sint64(&mut self) -> Result<i64, ProtoError> {
        Ok(zigzag_decode_i64(self.read_varint()?))
    }

    /// Read a `bool`. Protobuf permits any non-zero varint; zero is `false`.
    pub fn read_bool(&mut self) -> Result<bool, ProtoError> {
        Ok(self.read_varint()? != 0)
    }

    /// Read an `enum` as its raw `i32`, leaving symbol mapping to the caller.
    pub fn read_enum(&mut self) -> Result<i32, ProtoError> {
        Ok(self.read_varint()? as i32)
    }

    /// Read a `fixed32`.
    pub fn read_fixed32(&mut self) -> Result<u32, ProtoError> {
        let b = self.read_take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read an `sfixed32`.
    pub fn read_sfixed32(&mut self) -> Result<i32, ProtoError> {
        Ok(self.read_fixed32()? as i32)
    }

    /// Read a `float`.
    pub fn read_float(&mut self) -> Result<f32, ProtoError> {
        Ok(f32::from_bits(self.read_fixed32()?))
    }

    /// Read a `fixed64`.
    pub fn read_fixed64(&mut self) -> Result<u64, ProtoError> {
        let b = self.read_take(8)?;
        Ok(u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }

    /// Read an `sfixed64`.
    pub fn read_sfixed64(&mut self) -> Result<i64, ProtoError> {
        Ok(self.read_fixed64()? as i64)
    }

    /// Read a `double`.
    pub fn read_double(&mut self) -> Result<f64, ProtoError> {
        Ok(f64::from_bits(self.read_fixed64()?))
    }

    // ------------------------------------------------------ length-delimited ---

    /// Read a `bytes` field.
    pub fn read_bytes(&mut self, field: u32) -> Result<&'a [u8], ProtoError> {
        let declared = self.read_varint()?;
        let available = self.remaining();
        let Ok(len) = usize::try_from(declared) else {
            return Err(ProtoError::LengthBeyondEnd { field, declared, available });
        };
        if len > available {
            return Err(ProtoError::LengthBeyondEnd { field, declared, available });
        }
        self.read_take(len)
    }

    /// Read a `string` field, validating UTF-8.
    pub fn read_string(&mut self, field: u32) -> Result<&'a str, ProtoError> {
        let raw = self.read_bytes(field)?;
        core::str::from_utf8(raw).map_err(|_| ProtoError::InvalidUtf8(field))
    }

    /// Read an embedded message as a sub-reader bounded to exactly its bytes.
    ///
    /// The sub-reader stops at the end of this field, so a malformed inner
    /// length cannot make the caller read past the outer boundary.
    pub fn read_message(&mut self, field: u32) -> Result<Reader<'a>, ProtoError> {
        let raw = self.read_bytes(field)?;
        let depth = self.depth + 1;
        if depth > self.max_depth {
            return Err(ProtoError::TooDeep(self.max_depth));
        }
        Ok(Reader { buf: raw, pos: 0, depth, max_depth: self.max_depth })
    }

    // ---------------------------------------------------------------- packed ---

    /// Open a packed repeated field as a streaming iterator.
    ///
    /// A packed field always arrives as one length-delimited run of same-typed
    /// elements, so the tag must be `LengthDelimited` and `T` must have a
    /// packable wire type.
    pub fn read_packed<T: PackedValue>(&mut self, tag: Tag) -> Result<Packed<'a, T>, ProtoError> {
        if tag.wire != WireType::LengthDelimited {
            return Err(ProtoError::UnpackableWireType { field: tag.number, raw: tag.wire.raw() });
        }
        if !T::WIRE.packable() {
            return Err(ProtoError::UnpackableWireType { field: tag.number, raw: T::WIRE.raw() });
        }
        let inner = self.read_message(tag.number)?;
        Ok(Packed { inner, _value: PhantomData })
    }

    // ----------------------------------------------------------------- skip ---

    /// Consume the value belonging to `tag` without interpreting it.
    ///
    /// This is what makes unknown fields safe to ignore: a group is skipped by
    /// matching its terminator, not by guessing a length.
    pub fn skip(&mut self, tag: Tag) -> Result<(), ProtoError> {
        match tag.wire {
            WireType::Varint => {
                self.read_varint()?;
            }
            WireType::Fixed32 => {
                self.read_take(4)?;
            }
            WireType::Fixed64 => {
                self.read_take(8)?;
            }
            WireType::LengthDelimited => {
                self.read_bytes(tag.number)?;
            }
            WireType::Group(open) => self.skip_group(open)?,
        }
        Ok(())
    }

    /// Skip fields until the end-group marker for `open` is consumed.
    fn skip_group(&mut self, open: u32) -> Result<(), ProtoError> {
        if self.depth + 1 > self.max_depth {
            return Err(ProtoError::TooDeep(self.max_depth));
        }
        let saved_depth = self.depth;
        self.depth += 1;
        let result = self.skip_group_inner(open);
        self.depth = saved_depth;
        result
    }

    fn skip_group_inner(&mut self, open: u32) -> Result<(), ProtoError> {
        loop {
            match self.scan_tag()? {
                None => return Err(ProtoError::GroupUnterminated(open)),
                Some(Scan::End(close)) => {
                    if close != open {
                        return Err(ProtoError::GroupMismatch { open, close });
                    }
                    return Ok(());
                }
                Some(Scan::Start(tag)) => self.skip(tag)?,
            }
        }
    }

    // ------------------------------------------------------------ traversal ---

    /// Decode the value of `tag` into wire terms, without a schema.
    pub fn read_value(&mut self, tag: Tag) -> Result<Value<'a>, ProtoError> {
        let value = match tag.wire {
            WireType::Varint => Value::Varint(self.read_varint()?),
            WireType::Fixed32 => Value::Fixed32(self.read_fixed32()?),
            WireType::Fixed64 => Value::Fixed64(self.read_fixed64()?),
            WireType::LengthDelimited => Value::Bytes(self.read_bytes(tag.number)?),
            WireType::Group(open) => Value::Group(self.read_group(open)?),
        };
        Ok(value)
    }

    /// Read every remaining field of this message into a [`Field`] list.
    pub fn read_all(&mut self) -> Result<Vec<Field<'a>>, ProtoError> {
        let mut out = Vec::new();
        while let Some(tag) = self.next_tag()? {
            out.push(Field { number: tag.number, value: self.read_value(tag)? });
        }
        Ok(out)
    }

    fn read_group(&mut self, open: u32) -> Result<Vec<Field<'a>>, ProtoError> {
        if self.depth + 1 > self.max_depth {
            return Err(ProtoError::TooDeep(self.max_depth));
        }
        let saved = self.depth;
        self.depth += 1;
        let mut fields = Vec::new();
        let result = loop {
            match self.scan_tag()? {
                None => break Err(ProtoError::GroupUnterminated(open)),
                Some(Scan::End(close)) => {
                    if close != open {
                        break Err(ProtoError::GroupMismatch { open, close });
                    }
                    break Ok(());
                }
                Some(Scan::Start(tag)) => fields.push(Field {
                    number: tag.number,
                    value: self.read_value(tag)?,
                }),
            }
        };
        self.depth = saved;
        result?;
        Ok(fields)
    }
}

/// Streaming iterator over the elements of a packed repeated field.
///
/// The iterator yields `Result`s and stops permanently at the first malformed
/// element, so a caller can `?` through a long tensor of packed dims without
/// buffering them first.
#[derive(Debug)]
pub struct Packed<'a, T> {
    inner: Reader<'a>,
    _value: PhantomData<T>,
}

impl<'a, T: PackedValue> Iterator for Packed<'a, T> {
    type Item = Result<T, ProtoError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.inner.is_empty() {
            return None;
        }
        Some(T::read(&mut self.inner))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

/// A scalar type that can appear inside a packed repeated field.
pub trait PackedValue: Sized {
    /// Wire type its elements use.
    const WIRE: WireType;

    /// Decode one element from a packed run.
    fn read(reader: &mut Reader<'_>) -> Result<Self, ProtoError>;
}

macro_rules! impl_packed_varint {
    ($($ty:ty => $method:ident),* $(,)?) => {
        $(
            impl PackedValue for $ty {
                const WIRE: WireType = WireType::Varint;
                fn read(reader: &mut Reader<'_>) -> Result<Self, ProtoError> {
                    reader.$method()
                }
            }
        )*
    };
}

impl_packed_varint!(
    u32 => read_u32,
    u64 => read_u64,
    i32 => read_i32,
    i64 => read_i64,
    bool => read_bool,
);

macro_rules! impl_packed_fixed {
    ($($ty:ty => ($method:ident, $wire:expr)),* $(,)?) => {
        $(
            impl PackedValue for $ty {
                const WIRE: WireType = $wire;
                fn read(reader: &mut Reader<'_>) -> Result<Self, ProtoError> {
                    reader.$method()
                }
            }
        )*
    };
}

impl_packed_fixed!(
    f32 => (read_float, WireType::Fixed32),
    f64 => (read_double, WireType::Fixed64),
);
