//! `data-binary-sevenz`: 7z container facts, header indexing, and entry decoding.
//!
//! The adapter parses the 7z signature header and the (possibly encoded) main header,
//! exposes container facts and the entry table, and decodes entries through the
//! internalized pure-Rust codecs of `data-compression`: `Copy`, `LZMA`, `LZMA2`, and
//! the `BCJ x86` branch filter. Coders outside that set fail with an explicit
//! unsupported-coder error; no external process is ever spawned.

#![warn(missing_docs)]

mod decode;
mod format;
mod model;
mod read;

pub use data_compression::CodecLimits;
pub use decode::{SevenZDecodeError, bcj_x86_decode, decode_entry};
pub use model::{SevenZArchiveIndex, SevenZCoder, SevenZEntry, SevenZFolder};
pub use read::{SevenZIndexError, index_7z};

use boost_source::binary::MemorySource;

/// Indexes an in-memory 7z container.
pub fn index_7z_slice(bytes: &[u8]) -> Result<SevenZArchiveIndex, SevenZIndexError> {
    index_7z(&MemorySource::from_bytes("sevenz", bytes.to_vec()))
}

/// Decodes one entry of an in-memory 7z container under `limits`.
pub fn decode_entry_slice(
    bytes: &[u8],
    index: &SevenZArchiveIndex,
    entry: usize,
    limits: data_compression::CodecLimits,
) -> Result<Vec<u8>, SevenZDecodeError> {
    decode_entry(&MemorySource::from_bytes("sevenz", bytes.to_vec()), index, entry, limits)
}
