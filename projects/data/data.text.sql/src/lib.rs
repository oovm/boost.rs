#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module for SQL.
pub mod ast;
/// Builder module for SQL.
pub mod builder;

/// Language configuration module for SQL.
pub mod language;
/// Lexer module for SQL.
pub mod lexer;

/// Parser module for SQL.
pub mod parser;

pub use crate::{
    ast::SqlRoot,
    builder::SqlBuilder,
    language::SqlLanguage,
    lexer::{
        SqlLexer,
        token_type::{SqlTokenType, SqlTokenType as SqlSyntaxKind},
    },
    parser::{SqlParser, element_type::SqlElementType},
};

/// Parses a SQL string.
pub fn parse(sql: &str) -> Result<crate::ast::SqlRoot, String> {
    use data_text_core::{Builder, parser::session::ParseSession, source::SourceText};
    let language = SqlLanguage::default();
    let builder = SqlBuilder::new(&language);
    let source = SourceText::new(sql.to_string());
    let mut cache = ParseSession::default();
    let result = builder.build(&source, &[], &mut cache);
    result.result.map_err(|e| format!("{:?}", e))
}

/// Re-export SqlSyntaxKind in a kind module for backward compatibility
pub mod kind {
    /// Re-export SqlTokenType as SqlSyntaxKind.
    pub use crate::lexer::token_type::SqlTokenType as SqlSyntaxKind;
}


