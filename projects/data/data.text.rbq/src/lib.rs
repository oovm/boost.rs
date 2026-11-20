#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// AST module containing node definitions for the RBQ language.
pub mod ast;
/// Builder module for constructing RBQ trees.
pub mod builder;

/// Language configuration and syntax kind definitions.
pub mod language;
/// Lexer implementation for RBQ.
pub mod lexer;

/// Parser implementation for RBQ.
pub mod parser;

pub use crate::{
    ast::RbqRoot,
    builder::RbqBuilder,
    language::RbqLanguage,
    lexer::{RbqLexer, token_type::RbqTokenType},
    parser::{RbqParser, element_type::RbqElementType},
};

/// Alias for RbqTokenType to support tests and common usage
pub type RbqSyntaxKind = RbqTokenType;





/// Parses a string into an RBQ AST.
pub fn parse(input: &str) -> Result<RbqRoot, data_text_core::TextError> {
    use data_text_core::{ParseSession, Parser, tree::RedTree};

    // Create language configuration
    let language = RbqLanguage::new();

    // Create parser
    let parser = RbqParser::new(&language);

    // Create parse session
    let mut session = ParseSession::new(16);

    // Parse the input
    let output = parser.parse(input, &[], &mut session);

    // Check for errors
    if let Err(err) = output.result {
        return Err(err);
    }

    // Get the parse tree
    let tree = output.result.unwrap();

    // Convert the green tree to red tree
    let red_tree = RedTree::new(&tree);

    // Get the red node from the red tree
    let red_node = match red_tree.as_node() {
        Some(node) => node,
        None => return Err(data_text_core::TextError::custom_error("Root node not found")),
    };

    // Convert the red tree to AST
    let ast = RbqRoot::lower(red_node, input);

    Ok(ast)
}
