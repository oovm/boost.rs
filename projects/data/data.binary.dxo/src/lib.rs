//! `.dxo` portable asset container (design contract 43, format v1.0).
//!
//! A `.dxo` asset is one main file (`<name>.dxo`) plus optional numbered
//! volumes (`<name>.dxo.0000` …). The main file carries a fixed 96-byte
//! shell, a canonical JSON index and — for single-file assets — the data
//! chunks; each volume carries a fixed 64-byte shell and data chunks.
//! Chunks are the unit of random access and digest verification; volumes
//! are the unit of shipping. Re-chunking an asset never changes its
//! semantic digest.
//!
//! This crate performs no network I/O and owns no cache policy: callers
//! supply paths, the download layer and the asset-level descriptions
//! (`dataset` / `weights`). Structural validation lives here; domain
//! semantics (splits, parameters) are checked only for cross-reference
//! integrity.

mod error;
mod index;
mod json;
mod reader;
mod shell;
mod writer;

pub use error::DxoError;
pub use index::{
    CHUNK_ALIGN, ChunkDesc, ChunkRef, DatasetDesc, Dtype, FORMAT_MAJOR, FORMAT_MINOR, FieldDesc, Index, Kind, Provenance, SplitDesc,
    TensorDesc, VolumeDesc, WeightEntry, WeightsDesc, sha256_bytes, sha256_hex,
};
pub use reader::Asset;
pub use shell::{MAIN_HEADER_BYTES, MAIN_MAGIC, MainHeader, VOLUME_HEADER_BYTES, VOLUME_MAGIC, VolumeHeader, volume_path};
pub use writer::{Artifact, AssetSpec, TensorInput, build, sample_boundaries};
