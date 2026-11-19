#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module.
pub mod ast;
/// Builder module.
pub mod builder;

// pub mod formatter;

/// Type definitions module.
/// Language configuration module.
pub mod language;
/// Lexer module.
pub mod lexer;
/// LSP module.
#[cfg(any(feature = "lsp", feature = "language-highlight"))]
pub mod lsp;
/// MCP module.
#[cfg(feature = "mcp")]
pub mod mcp;

/// Parser module.
pub mod parser;

pub use crate::{ast::LuaRoot, builder::LuaBuilder, language::LuaLanguage, lexer::LuaLexer, parser::LuaParser};

/// Highlighter implementation.
#[cfg(feature = "language-highlight")]
pub use crate::lsp::highlighter::LuaHighlighter;

/// LSP implementation.
#[cfg(feature = "lsp")]
pub use crate::lsp::LuaLanguageService;
// pretty-print is always available through `data-text-pretty`
// pub use crate::lsp::formatter::LuaFormatter;

/// MCP service implementation.
#[cfg(feature = "mcp")]
pub use crate::mcp::serve_lua_mcp;
pub use lexer::token_type::LuaTokenType;
pub use parser::element_type::LuaElementType;
