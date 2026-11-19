#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
pub mod language;
pub mod lexer;
pub mod parser;

mod builder;
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// MCP module.
#[cfg(feature = "mcp")]
pub mod mcp;

pub use crate::{ast::*, builder::RhombusBuilder, language::RhombusLanguage, lexer::RhombusLexer, parser::RhombusParser};

#[cfg(feature = "lsp")]
pub use crate::lsp::RhombusLanguageService;
#[cfg(feature = "lsp")]
pub use crate::lsp::formatter::RhombusFormatter;
#[cfg(all(feature = "lsp", feature = "language-highlight"))]
pub use crate::lsp::highlighter::RhombusHighlighter;

#[cfg(feature = "mcp")]
pub use crate::mcp::serve_rhombus_mcp;
pub use lexer::token_type::RhombusTokenType;
pub use parser::element_type::RhombusElementType;
