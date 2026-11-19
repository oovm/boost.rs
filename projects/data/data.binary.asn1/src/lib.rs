#![warn(missing_docs)]
//! ASN.1 DER codec for binary layouts.
//!
//! This crate encodes and decodes strict DER only: definite minimal lengths,
//! minimal INTEGER forms, and the high-tag-number form are all enforced. BER
//! constructs such as indefinite lengths are rejected with a stable
//! [`DerErrorKind`].
//!
//! X.509 structures, CSR building, and TLS certificate messages are layered on
//! top of these primitives in their own crates; nothing here knows about
//! certificates or trust. The [`oids`] namespace only pins the wire identifiers
//! those upper layers match against.
//!
//! The code originates from the former `acorn.rs` project (imported at commit
//! `6deb56a4`, MPL-2.0) and has been internalized and de-branded for Boost.

mod error;
mod oid;
mod reader;
mod tag;
mod time;
mod writer;

pub use error::{DerError, DerErrorKind, DerResult};
pub use oid::Oid;
pub use reader::{BitString, Reader, Tlv};
pub use tag::{Class, Tag};
pub use time::DateTime;
pub use writer::{Writer, encode_element, encode_header};

/// The object identifiers X.509 and PKI documents encode or match.
pub mod oids;
