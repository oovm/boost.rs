//! Capability entry for the `data.*` families.
//!
//! Each module re-exports the owning crate; this facade defines no types of its
//! own. Enable only the families an application needs, or depend on the
//! matching `data-*` crate directly when one capability is enough.

#![warn(missing_docs)]

#[cfg(feature = "text")]
pub use data_text_core as text;

#[cfg(feature = "binary")]
pub use data_binary_core as binary;

#[cfg(feature = "encoding")]
pub use data_encoding as encoding;

#[cfg(feature = "checksum")]
pub use data_checksum as checksum;

#[cfg(feature = "compression")]
pub use data_compression as compression;
