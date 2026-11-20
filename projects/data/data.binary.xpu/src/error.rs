//! Error surface for `.xpu` container operations.
//!
//! Categories mirror the contract: unknown version or required feature,
//! directory structure violations, section range violations, digest
//! mismatches and writer input problems stay distinguishable so callers can
//! react per class.

/// All failures produced while parsing, validating, reading or writing an
/// `.xpu` container.
#[derive(Debug, thiserror::Error)]
pub enum XpuError {
    #[error("file shorter than required: need {need} bytes, have {have}")]
    TooShort { need: u64, have: u64 },

    #[error("bad container magic")]
    BadMagic,

    #[error("unsupported format version {major}.{minor}")]
    UnsupportedVersion { major: u16, minor: u16 },

    #[error("fixed header field violation: {0}")]
    HeaderField(&'static str),

    #[error("reserved header or entry bytes are not zero")]
    ReservedNonZero,

    #[error("required flag bits {0:#010x} are not defined by this format revision")]
    UnknownRequiredFlags(u32),

    #[error("file length {actual} does not match the declared {declared} bytes")]
    FileLengthMismatch { declared: u64, actual: u64 },

    #[error("section count {0} is outside the supported range 1..=65536")]
    TooManySections(u32),

    #[error("no section with id {0}")]
    UnknownSection(u32),

    #[error("directory is not sorted by strictly increasing non-zero section id")]
    BadOrder,

    #[error("section flags {0:#010x} use reserved bits")]
    ReservedSectionFlags(u32),

    #[error("section {0} alignment is not a power of two of at least 8 bytes")]
    BadAlignment(u32),

    #[error("section {0} declares stored_bytes different from logical_bytes; v1.0 stores data uncompressed")]
    Compressed(u32),

    #[error("control sections declare {declared} bytes, over the limit of {limit}")]
    ControlBudgetExceeded { declared: u64, limit: u64 },

    #[error("section {id} range [{offset}, {end}) is not fully inside the container after the directory")]
    RangeViolation { id: u32, offset: u64, end: u64 },

    #[error("section {id} starts at {offset}, not a multiple of its alignment {alignment}")]
    Misaligned { id: u32, offset: u64, alignment: u64 },

    #[error("sections {a} and {b} overlap")]
    Overlap { a: u32, b: u32 },

    #[error("directory digest mismatch")]
    DirectoryDigestMismatch,

    #[error("digest mismatch for section {0}")]
    SectionDigestMismatch(u32),

    #[error("required singleton section kind {0} is missing")]
    MissingSingleton(u16),

    #[error("singleton section kind {kind} appears {count} times")]
    DuplicateSingleton { kind: u16, count: u32 },

    #[error("section {0} has an unknown kind and is marked globally required")]
    RequiredUnknownKind(u32),

    #[error("manifest id {0} does not reference the manifest section")]
    ManifestRefMismatch(u32),

    #[error("writer input invalid: {0}")]
    WriterInput(String),
}
