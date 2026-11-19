//! `data-binary-hsaco` — AMD `HSACO` ELF code object layout and validation.
//!
//! Decodes the ELF64 identification bytes and fixed header fields, bounds the
//! program-header and section-header tables against the object length, records
//! segments and sections as names and intervals, and classifies key sections
//! (`.text`, `.rodata`, `.symtab`, `.strtab`/`.shstrtab`, `.note`,
//! `.amdgpu_metadata`) from the section name table. Layout and structural checks
//! only: instruction encoding, relocations, YAML metadata and note payloads are
//! never interpreted. No third-party ELF, HIP or AMDGPU library is used.
//!
//! The code originates from the former `acorn.rs` project (imported at commit
//! `6deb56a4`, MPL-2.0) and has been internalized and de-branded for Boost.

#![warn(missing_docs)]

mod bytes;
mod error;
mod header;
mod object;

/// Byte-slice validation entry and its error surface.
pub use bytes::{HsacoByteError, validate_hsaco_bytes};
/// Structural failure classification and its byte evidence.
pub use error::{HsacoEvidence, HsacoHeaderTable, HsacoLayoutError};
/// ELF64 constants and the recorded code object header.
pub use header::{
    HSACO_ELF_HEADER_BYTES, HSACO_ELF_MAGIC, HSACO_ELFCLASS64, HSACO_ELFDATA2LSB, HSACO_EM_AMDGPU, HSACO_PROGRAM_HEADER_BYTES,
    HSACO_SECTION_HEADER_BYTES, HSACO_SHT_NOBITS, HSACO_SYMBOL_BYTES, HsacoHeader,
};
/// Bounded object view, section and segment records, summary and the validation entry.
pub use object::{HsacoObject, HsacoSection, HsacoSectionKind, HsacoSegment, HsacoSummary, validate_hsaco};
