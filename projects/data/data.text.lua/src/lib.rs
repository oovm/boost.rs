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

/// Parser module.
pub mod parser;

pub use crate::{ast::LuaRoot, builder::LuaBuilder, language::LuaLanguage, lexer::LuaLexer, parser::LuaParser};


// pretty-print is always available through `data-text-pretty`
// pub use crate::lsp::formatter::LuaFormatter;

pub use lexer::token_type::LuaTokenType;
pub use parser::element_type::LuaElementType;
