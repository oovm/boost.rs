//! `data-text-core`: source, lexer, parser and incremental tree primitives
//! for the `data.text` capability family.
//!
//! Source identity, revision and byte-range types come from `boost-source`;
//! structured diagnostics come from `boost-harness`. This crate owns the text
//! snapshot domain: chunked sources, green/red trees, edit application and
//! language-service virtual file system adapters.
//!
//! The code originates from the former `oak.rs` project (imported at commit
//! `4dae755e`, MPL-2.0) and has been internalized and de-branded for Boost.

#![allow(incomplete_features)]
#![allow(internal_features)] // Allow using internal features like `core_intrinsics`
#![feature(core_intrinsics)] // Used for `likely`/`unlikely` hints in performance-critical paths
#![feature(checked_type_aliases)] // Enforces bounds on generic type aliases such as `ParseOutput`
#![feature(portable_simd)] // Enables SIMD acceleration for high-performance lexing and parsing
#![warn(missing_docs)]

/// Incremental tree builder and cache management.
pub mod builder;
/// Unified `boost-harness` diagnostic adapters for parser output.
#[cfg(feature = "diagnostic")]
pub mod diagnostic;
/// Error handling and diagnostic reporting for the parsing system.
pub mod errors;
/// Language definition trait for coordinating language-specific components.
pub mod language;
/// Lexical analysis and tokenization functionality.
pub mod lexer;
/// Memory management utilities (Arena, Bump).
pub mod memory;
/// Parsing functionality for converting tokens to kind trees.
pub mod parser;
#[cfg(feature = "serde")]
pub mod serde_arc_str;
#[cfg(feature = "serde")]
pub mod serde_range;
/// Source text management and location tracking.
pub mod source;
/// Tree structures for representing kind trees (green and red trees).
pub mod tree;
/// Virtual file system and document session for language services.
#[cfg(feature = "vfs")]
pub mod vfs;
/// Tree traversal and transformation utilities.
pub mod visitor;

/// Unified AST processing interface.
pub mod ast;

/// Helper utilities for common operations.
pub mod helpers;
/// Syntax-agnostic structured query primitives for document trees.
pub mod query;

pub use core::range::Range;

pub use crate::{
    ast::{AstError, AstProcessor, DefaultAstProcessor, RedTreeExt, VisitResult, Visitor},
    builder::{Builder, BuilderCache},
    errors::{TextDiagnostics, TextError, TextErrorKind},
    language::{ElementRole, ElementType, Language, LanguageCategory, TokenRole, TokenType, UniversalElementRole, UniversalTokenRole},
    lexer::{LexOutput, Lexer, LexerCache, LexerState, NoLexerCache, Token, TokenStream, Tokens},
    memory::arena::SyntaxArena,
    parser::{
        Associativity, CachingParseSession, ContentCache, OperatorInfo, ParseCache, ParseOutput, ParseSession, Parser, ParserState, Pratt,
        PrattParser, binary, parse, parse_one_pass, postfix, state::TreeSink, unary,
    },
    source::{Source, SourceText, TextEdit},
    tree::{GreenNode, GreenTree, RedLeaf, RedNode, RedTree},
};

pub use std::sync::Arc;
