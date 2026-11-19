#![warn(missing_docs)]
//! Magic-byte probing and format candidate collection for binary containers.
//!
//! Signatures produce ranked [`ProbeCandidate`] values with evidence ranges.
//! A magic hit alone is never treated as fully identified structure.
//!
//! The code originates from the former `acorn.rs` project (imported at commit
//! `6deb56a4`, MPL-2.0) and has been internalized and de-branded for Boost.

mod candidate;
mod probe;
mod signature;

pub use candidate::{Confidence, ProbeCandidate};
pub use probe::{ProbeBudget, ProbeSession, document_container_signatures, probe_memory};
pub use signature::{BytePattern, ProbeAnchor, Signature};
