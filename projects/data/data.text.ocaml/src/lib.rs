#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

pub mod ast;
/// Builder module for OCaml.
pub mod builder;
pub mod language;
pub mod lexer;
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// MCP module.
#[cfg(feature = "mcp")]
pub mod mcp;

/// Parser module for OCaml.
pub mod parser;

pub use builder::OCamlBuilder;
pub use language::OCamlLanguage;
pub use lexer::OCamlLexer;
pub use parser::OCamlParser;

/// Highlighter implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::highlighter::{HighlightKind, Highlighter, OCamlHighlighter};

/// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::OCamlLanguageService;

/// MCP service implementation.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_ocaml_mcp;
pub use lexer::token_type::OCamlTokenType;
pub use parser::element_type::OCamlElementType;
