#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

/// The initial declaration AST emitted by the VOS builder.
pub mod ast;
/// The builder for VOS declarations.
pub mod builder;
/// The language configuration for VOS.
pub mod language;
/// The lexer for VOS.
pub mod lexer;
/// The parser for VOS.
pub mod parser;

pub use crate::{
    ast::{
        VosBinaryOp, VosDeclaration, VosDeclarationKind, VosExpr, VosField, VosFieldAttribute, VosFieldInit, VosFnDecl, VosFnKind, VosLambda,
        VosLet, VosLiteral, VosParameter, VosPathSep, VosProgram, VosProjItem, VosRoot, VosServiceMethod, VosServiceMethodReturn, VosStmt,
        VosSyntaxElement, VosSyntaxNode, VosSyntaxSlice, VosSyntaxToken, VosTypeArgument, VosTypeSyntax, VosUnaryOp,
    },
    builder::VosBuilder,
    language::VosLanguage,
    lexer::{VosLexer, VosToken, VosTokenType},
    parser::{VosElementType, VosParser},
};

/// Builds a structural VOS CST through the `data.text` parser's lexer and parser pipeline.
///
/// This entry rejects lexical errors and malformed delimiters. It does not
/// establish full VOS grammar validity or produce a resolved VOS contract.
pub fn parse(source: &str) -> Result<VosRoot, String> {
    use data_text_core::{Builder, SourceText, parser::session::ParseSession};

    let language = VosLanguage::default();
    let builder = VosBuilder::new(&language);
    let source_text = SourceText::new(source.to_owned());
    let mut cache = ParseSession::<VosLanguage>::default();
    let result = builder.build(&source_text, &[], &mut cache);
    if let Some(error) = result.diagnostics.first() {
        return Err(format!("{error:?}"));
    }
    result.result.map_err(|error| format!("{error:?}"))
}

/// Parses a VOS expression program (statements, `micro` declarations and an
/// optional trailing result expression) from parser tokens.
///
/// This is the structured program projection consumed by hosts that execute
/// VOS operations. It performs no name resolution or type checking.
pub fn parse_program(source: &str) -> Result<VosProgram, String> {
    let root = parse(source)?;
    VosBuilder::default().project_program(&root).map_err(|error| format!("{error:?}"))
}
