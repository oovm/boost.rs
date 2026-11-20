#![doc = include_str!("readme.md")]
mod bridge;
mod engine;
mod error;
mod options;
mod provider;
mod source;

pub use bridge::VonCstFormatter;
pub use engine::format_source;
pub use error::FormatError;
pub use options::FormatOptions;
pub use provider::{VonSourceFormatterProvider, register_source_formatter};
