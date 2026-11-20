#![doc = include_str!("readme.md")]
#![feature(new_range_api)]
#![warn(missing_docs)]

/// Root node of the Dockerfile AST.
pub mod ast;
/// Builder for the Dockerfile AST.
pub mod builder;
/// Language configuration for Dockerfile.
pub mod language;
pub mod lexer;
pub mod parser;

pub use crate::{
    ast::DockerfileRoot,
    builder::DockerfileBuilder,
    language::DockerfileLanguage,
    lexer::{DockerfileLexer, token_type::DockerfileTokenType},
    parser::{DockerfileParser, element_type::DockerfileElementType},
};
