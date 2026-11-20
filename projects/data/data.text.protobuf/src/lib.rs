#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module for Protobuf.
pub mod ast;
/// Builder module for Protobuf.
pub mod builder;

/// Language configuration for Protobuf.
pub mod language;
/// Lexer for Protobuf.
pub mod lexer;

/// Parser for Protobuf.
pub mod parser;

pub use crate::{
    ast::ProtobufRoot,
    builder::ProtobufBuilder,
    language::ProtobufLanguage,
    // crate::lsp::highlighter::{HighlightKindHighlighterProtobufHighlighter},
    lexer::token_type::ProtobufTokenType,
    parser::ProtobufParser,
};


// #[cfg(feature = "mcp")]
// pub use crate::mcp::serve_protobuf_mcp;
pub use parser::element_type::ProtobufElementType;

/// Parses a Protobuf string.
pub fn parse(protobuf: &str) -> Result<crate::ast::ProtobufRoot, String> {
    use data_text_core::{Builder, parser::session::ParseSession, source::SourceText};
    let language = ProtobufLanguage::default();
    let builder = ProtobufBuilder::new(&language);
    let source = SourceText::new(protobuf.to_string());
    let mut cache = ParseSession::default();
    let result = builder.build(&source, &[], &mut cache);
    result.result.map_err(|e| format!("{:?}", e))
}
