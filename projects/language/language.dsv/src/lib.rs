#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![feature(adt_const_params)]
#![allow(incomplete_features)]
#![warn(missing_docs)]

/// lsp module.
pub mod lsp;
pub use crate::lsp::DsvLanguageService;
