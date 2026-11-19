//! `data-binary-dxbc` — `DXBC` container and `SHEX` instruction-stream layout.
//!
//! Decodes the container header (magic, digest, version, declared length, chunk
//! offset table) and classifies chunks by FourCC; `SHEX` walks SM5 instruction
//! words reporting raw opcode, declared word count and byte range. Structural
//! validation returns a summary or a classified `DxbcLayoutError` carrying the
//! failing chunk index and byte range. Layout and checks only: opcodes, operand
//! meaning and signatures are never interpreted, the digest is recorded not
//! recomputed, and the write side is out of scope. No third-party DXBC or
//! Direct3D toolchain is used.
//!
//! The code originates from the former `acorn.rs` project (imported at commit
//! `6deb56a4`, MPL-2.0) and has been internalized and de-branded for Boost.

#![warn(missing_docs)]

mod bytes;
mod chunk;
mod container;
mod error;
mod header;
mod shex;

/// Byte-slice validation entry and its error surface.
pub use bytes::{DxbcByteError, validate_dxbc_bytes};
/// Chunk records and the recognized FourCC classes.
pub use chunk::{DxbcChunk, DxbcChunkKind};
/// Bounded container view, validation summary and the validation entry.
pub use container::{DxbcContainer, DxbcShaderSummary, DxbcSummary, validate_dxbc};
/// Structural failure classification and its byte evidence.
pub use error::{DxbcEvidence, DxbcLayoutError};
/// Header constants and the recorded container header.
pub use header::{
    DXBC_CHUNK_HEADER_BYTES, DXBC_DIGEST_BYTES, DXBC_HEADER_BYTES, DXBC_MAGIC, DXBC_OFFSET_TABLE_ENTRY_BYTES, DXBC_OPCODE_TOKEN_LENGTH_MASK,
    DXBC_OPCODE_TOKEN_LENGTH_SHIFT, DXBC_OPCODE_TOKEN_OPCODE_MASK, DXBC_WORD_BYTES, DxbcHeader, SHEX_HEADER_BYTES,
};
/// `SHEX` tokens, instruction layout and the lazy instruction walk.
pub use shex::{DxbcInstruction, ShexInstructions, ShexProgram};
