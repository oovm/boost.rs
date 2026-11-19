#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;

/// Builder module.
pub mod builder;

/// Type definitions module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;
// pub mod lsp;

/// Parser module.
pub mod parser;

pub use crate::{ast::VhdlRoot, builder::VhdlBuilder, language::VhdlLanguage, lexer::VhdlLexer, parser::VhdlParser};
pub use data_text_core::{ElementType, TokenType};

// #[cfg(feature = "language-highlight")]
// // pub use crate::lsp::highlighter::VhdlHighlighter;
// #[cfg(feature = "lsp")]
// pub use crate::lsp::formatter::VhdlFormatter;
// #[cfg(feature = "lsp")]
// pub use crate::{ lsp::VhdlLanguageService};
// #[cfg(feature = "mcp")]
// pub use crate::mcp::serve_vhdl_mcp;
pub use lexer::token_type::VhdlTokenType;
pub use parser::element_type::VhdlElementType;
