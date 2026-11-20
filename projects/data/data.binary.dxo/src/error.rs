//! Error surface for `.dxo` container operations.

use std::{io, path::PathBuf};

/// All failures produced while parsing, validating, reading or writing a
/// `.dxo` asset. Categories mirror the contract: unknown version/feature,
/// bad index, missing volume, volume misattribution and chunk corruption
/// stay distinguishable so callers can react per class.
#[derive(Debug, thiserror::Error)]
pub enum DxoError {
    #[error("I/O failure on {path}: {source}")]
    Io { path: PathBuf, source: io::Error },

    #[error("file shorter than declared: need {need} bytes, have {have}")]
    TooShort { need: u64, have: u64 },

    #[error("bad container magic")]
    BadMagic,

    #[error("unsupported format version {major}.{minor}")]
    UnsupportedVersion { major: u16, minor: u16 },

    #[error("reserved header bytes are not zero")]
    ReservedNonZero,

    #[error("malformed index JSON: {0}")]
    BadJson(&'static str),

    #[error("duplicate JSON key `{0}`")]
    DuplicateKey(String),

    #[error("JSON nesting exceeds the depth limit")]
    TooDeep,

    #[error("invalid index field: {0}")]
    BadIndex(&'static str),

    #[error("range violation: {0}")]
    Range(&'static str),

    #[error("physical ranges of chunks {a} and {b} overlap in file {file}")]
    Overlap { file: u64, a: u64, b: u64 },

    #[error("chunk {0} is not 64-byte aligned")]
    Unaligned(u64),

    #[error("chunk coverage of tensor `{0}` does not match its logical bytes")]
    Coverage(&'static str),

    #[error("chunk {0} is referenced by more than one tensor")]
    MultiOwnedChunk(u64),

    #[error("chunk {0} is not referenced by any tensor")]
    UnreferencedChunk(u64),

    #[error("duplicate chunk id {0}")]
    DuplicateChunkId(u64),

    #[error("duplicate tensor key `{0}`")]
    DuplicateTensorKey(String),

    #[error("digest mismatch for {what}")]
    DigestMismatch { what: String },

    #[error("semantic digest mismatch: index declares {declared}")]
    SemanticMismatch { declared: String },

    #[error("missing volume file {path}")]
    VolumeMissing { path: PathBuf },

    #[error("volume file {path} does not describe volume {expected}")]
    VolumeMismatch { path: PathBuf, expected: u64 },

    #[error("file {path} is {actual} bytes but {expected} were declared")]
    FileLengthMismatch { path: PathBuf, expected: u64, actual: u64 },

    #[error("required feature `{0}` is not supported")]
    UnknownFeature(String),

    #[error("asset kind `{0}` is not supported")]
    UnsupportedKind(String),

    #[error("codec `{0}` is not supported")]
    UnsupportedCodec(String),

    #[error("dtype `{0}` is not supported")]
    UnsupportedDtype(String),

    #[error("layout `{0}` is not supported")]
    UnsupportedLayout(String),

    #[error("writer input invalid: {0}")]
    WriterInput(String),

    #[error("index layout did not converge while placing chunks")]
    NotConverged,
}

impl DxoError {
    /// Wrap an I/O failure with the path it happened on.
    pub(crate) fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io { path: path.into(), source }
    }
}
