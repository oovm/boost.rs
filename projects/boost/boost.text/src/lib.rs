//! Capability entry for the former `oak` umbrella: text facts and language services.
//!
//! Each module re-exports the owning crate; this facade defines no types of its
//! own. Enable only the capabilities an application needs.

#[cfg(feature = "core")]
pub use data_text_core as core;

#[cfg(feature = "symbols")]
pub use data_text_symbols as symbols;

#[cfg(feature = "source-map")]
pub use data_text_source_map as source_map;

#[cfg(feature = "vos")]
pub use data_text_vos as vos;
