#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// formatter module.
pub mod formatter;
pub use formatter::{FormatError, FormatOptions, format_source};
