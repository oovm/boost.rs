//! `.xpu` prepared-binary container shell (design contract 42, shell v1.0).
//!
//! An `.xpu` file is a 96-byte fixed header, an 80-byte-per-entry section
//! directory and section payloads placed after the directory at their
//! declared alignment. The directory digest covers the raw directory bytes;
//! each section digest covers its raw stored bytes.
//!
//! This crate owns the shell only: structure, bounds, ordering, alignment,
//! budgets and digest verification. It deliberately does not interpret
//! section payloads — the control-segment field tables are frozen
//! separately (contract 42 §8 X0) and model semantics live in `dl.model`.
//! Unknown optional sections are carried through; a globally required
//! section of an unknown kind makes the file unusable.
//!
//! v1.0 stores every section uncompressed (`stored_bytes ==
//! logical_bytes`); a section claiming otherwise is rejected, not skipped.

#![warn(missing_docs)]

mod error;
mod index;
mod reader;
mod shell;
mod writer;

pub use error::XpuError;
pub use index::{
    DEFAULT_ALIGNMENT, ENTRY_BYTES, FLAG_GLOBAL_REQUIRED, KNOWN_REQUIRED_FLAGS, KNOWN_SECTION_FLAGS, MAX_CONTROL_BYTES,
    MAX_SECTIONS, MIN_ALIGNMENT, WEIGHT_ALIGNMENT, Section, SectionKind, sha256_bytes,
};
pub use reader::Container;
pub use shell::{Header, MAGIC, HEADER_BYTES, FORMAT_MAJOR, FORMAT_MINOR};
pub use writer::{ContainerSpec, SectionInput, build};
