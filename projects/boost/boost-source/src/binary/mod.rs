//! Random-access byte sources and bounded byte sinks over the shared substrate.
//!
//! [`ByteSource`] reports short reads and unavailable intervals explicitly and
//! never pads past EOF or fabricates missing bytes. [`ByteSink`] is the
//! symmetric generation contract with budgets and in-place patching.
//!
//! Boundary with the rest of this crate: reads here use the shared
//! [`ByteRange`](crate::ByteRange) and no second range type is introduced.
//! [`ContentIdentity`] is a content provenance and cache key for one byte
//! stream, distinct from the logical [`SourceRef`](crate::SourceRef) identity
//! of a snapshot.
//!
//! The code originates from the former `acorn.rs` project (imported at commit
//! `6deb56a4`, MPL-2.0) and has been internalized and de-branded for Boost.

mod capabilities;
mod identity;
mod memory;
mod memory_sink;
mod partial;
mod read;
mod sink;
mod source;
mod window;

pub use capabilities::SourceCapabilities;
pub use identity::{ContentIdentity, content_digest_of};
pub use memory::MemorySource;
pub use memory_sink::MemorySink;
pub use partial::RangeMapSource;
pub use read::{ReadOutcome, ReadResult};
pub use sink::{ByteSink, SinkCapabilities, WriteError, WriteOutcome};
pub use source::{ByteSource, ReadError};
pub use window::ByteWindow;
