#![warn(missing_docs)]
//! Tar container indexing, writing and tar-gzip composition.
//!
//! The code originates from the former `acorn.rs` project (imported at commit
//! `6deb56a4`, MPL-2.0) and has been internalized and de-branded for Boost.

mod format;
mod model;
mod read;
mod targz;
mod write;

pub use data_compression::StreamChunk;
pub use model::{TarArchive, TarEntry, TarEntryDraft, TarEntryKind};
pub use read::{TarEntryStream, TarIndexError, TarReadError, index_tar, index_tar_bytes, read_member_payload};
pub use targz::{DEFAULT_STREAM_CHUNK, TarGz, TarGzArchive, TarGzEntry, TarGzError, TarGzStream};
pub use write::{TarWriteError, TarWriter, write_tar};
