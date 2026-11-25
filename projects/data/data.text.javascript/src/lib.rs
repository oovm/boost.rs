#![doc = include_str!("readme.md")]
#![warn(missing_docs)]
#![feature(portable_simd)]
#![feature(new_range_api)]


pub mod ast;
pub mod erase;
pub mod language;
pub mod lexer;
pub mod parser;
pub mod print;

pub use erase::{erase_types, erase_types_with_deletions};
pub use lexer::token_type::JavaScriptTokenType;
pub use parser::element_type::JavaScriptElementType;
pub use print::{SourceLines, SourceMapSegment, map_erased_offset, minify_source, source_map_json};
