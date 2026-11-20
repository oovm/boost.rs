//! Protocol Buffers **wire-format** substrate.
//!
//! This crate understands the encoding, not any schema. It walks tags,
//! decodes the primitive types the format defines, opens nested messages and
//! packed repeats, and skips fields the caller does not recognise — all with
//! bounds checks that make malformed input an error rather than an
//! out-of-range read.
//!
//! # Why hand-written parsers sit on top of this
//!
//! Model formats that happen to use the protobuf encoding (ONNX `ModelProto`,
//! TensorFlow `GraphDef`) carry semantics a generated stub cannot express:
//! which operator domains are supported, whether an initializer's declared
//! shape matches its raw bytes, which opset a graph requires. The decision for
//! this stack is therefore to write those parsers by hand and share only the
//! encoding layer, which is exactly the part that must never be rewritten per
//! schema.
//!
//! So a parser here looks like:
//!
//! ```
//! use data_binary_protobuf::{Reader, WireType};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // A one-field message: field 1, varint 150.
//! let bytes = [0x08u8, 0x96, 0x01];
//! let mut reader = Reader::new(&bytes);
//! let tag = reader.next_tag()?.expect("one field");
//! assert_eq!(tag.number, 1);
//! assert_eq!(tag.wire, WireType::Varint);
//! assert_eq!(reader.read_u64()?, 150);
//! assert!(reader.is_empty());
//! # Ok(())
//! # }
//! ```
//!
//! # What is deliberately absent
//!
//! No `.proto` parsing (that is `data.text.protobuf`), no descriptor pool, no
//! reflection, no dynamic message type, no JSON mapping and no gRPC. Those are
//! separate concerns with separate consumers; folding them in here would make
//! the encoding layer depend on all of them.
//!
//! Groups (wire types 3 and 4) are deprecated and absent from proto3, but are
//! decoded and skipped faithfully so a proto2-era artifact cannot be mistaken
//! for corruption.

mod error;
mod reader;
mod wire;
mod writer;

pub use error::ProtoError;
pub use reader::{Field, Packed, PackedValue, Reader, Value};
pub use wire::{
    DEFAULT_MAX_DEPTH, MAX_FIELD_NUMBER, MAX_VARINT_BYTES, RESERVED_FIELD_FROM, RESERVED_FIELD_TO, Tag, WireType,
    zigzag_decode_i32, zigzag_decode_i64, zigzag_encode_i32, zigzag_encode_i64,
};
pub use writer::{PackedWrite, Writer};
