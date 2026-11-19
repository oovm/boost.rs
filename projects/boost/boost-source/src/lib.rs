#![doc = include_str!("readme.md")]
#![deny(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

mod access;
mod identity;
mod range;
mod snapshot;
mod text;

#[cfg(feature = "std")]
mod memory;

#[cfg(feature = "std")]
pub mod binary;
mod wire;

pub use access::{AccessError, ByteAccess, PartialByteAccess};
pub use identity::{IdentityError, SourceIdentity, SourceLabel, SourceRef, SourceRevision, SourceSpan};
pub use range::{ByteRange, RangeError};
pub use snapshot::{SnapshotHandle, SnapshotRef};
pub use text::{LineIndex, TextCoords, TextView};

#[cfg(feature = "std")]
pub use memory::{MemorySnapshot, MemoryStore};

#[cfg(feature = "std")]
pub use binary::{
    ByteSink, ByteSource, ByteWindow, ContentIdentity, MemorySink, MemorySource, RangeMapSource, ReadError, ReadOutcome, ReadResult,
    SinkCapabilities, SourceCapabilities, WriteError, WriteOutcome, content_digest_of,
};
pub use wire::{WireError, decode_source_ref, encode_source_ref};
