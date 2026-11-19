//! `data-encoding`: stateless representation transforms for the `data` family.
//!
//! This crate owns the byte-representation and integer-encoding algorithms
//! that turn bytes into typed values and back: Base64 and Hex (`representation`),
//! LEB128 and ZigZag (`integer`), plus the shared scalar/slice transform traits.
//! It has no dependency on compression, checksums, source snapshots or format
//! adapters. Callers map [`EncodingError`] and decode facts into their own
//! budgets, provenance and format-specific diagnostics.
//!
//! The code originates from the former `acorn.rs` project (imported at commit
//! `6deb56a4`, MPL-2.0) and has been internalized and de-branded for Boost.

#![deny(missing_docs)]

mod error;
mod facts;
mod scalar;
mod slice;

pub use error::EncodingError;
pub use facts::{ByteDecode, Canonicality, DecodeFacts};
pub use scalar::ScalarTransform;
pub use slice::SliceCodec;

/// Integer byte representations (`ZigZag`, `Leb128`).
pub mod integer;
/// Byte-to-text representation transforms (`Base64`, `Hex`).
pub mod representation;
