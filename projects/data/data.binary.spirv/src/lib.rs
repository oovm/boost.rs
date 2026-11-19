//! `data-binary-spirv` — `SPIR-V` binary module layout and structural validation.
//!
//! Decodes the five little-endian `u32` header words (magic `0x07230203`,
//! version, generator, id `bound`, reserved schema) as recorded bytes, walks the
//! instruction stream reporting raw opcode, declared word count, word offset and
//! byte range, and classifies structural failures as `SpirvLayoutError` carrying
//! the failing word offset and byte range. Layout only: opcode names, operand
//! meaning and id semantics are never interpreted, and the write side is out of
//! scope. No third-party SPIR-V library is used.
//!
//! The code originates from the former `acorn.rs` project (imported at commit
//! `6deb56a4`, MPL-2.0) and has been internalized and de-branded for Boost.

#![warn(missing_docs)]

mod bytes;
mod error;
mod header;
mod instruction;
mod module;

/// Byte-slice validation entry and its error surface.
pub use bytes::{SpirvByteError, validate_spirv_bytes};
/// Structural failure classification and its byte evidence.
pub use error::{SpirvEvidence, SpirvLayoutError};
/// Header constants and the recorded id bound.
pub use header::{
    SPIRV_HEADER_BYTES, SPIRV_HEADER_WORDS, SPIRV_ID_BOUND_CEILING, SPIRV_MAGIC, SPIRV_WORD_BYTES, SpirvBoundSuspicion,
    SpirvHeader,
};
/// Instruction layout and the lazy instruction walk.
pub use instruction::{SpirvInstruction, SpirvInstructions};
/// Bounded module view, validation summary and the validation entry.
pub use module::{SpirvModule, SpirvSummary, validate_spirv};
