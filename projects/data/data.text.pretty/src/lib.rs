//! Document algebra and pretty-print renderer shared by `data.text` printers.
//!
//! The code originates from the former `oak-pretty-print` project (MPL-2.0) and
//! has been internalized and de-branded for Boost.

#![allow(incomplete_features)]
#![feature(associated_type_defaults)]
#![warn(missing_docs)]

extern crate self as data_text_pretty;

/// Comment collection and comment rendering helpers.
pub mod comment;
/// Document abstraction and its renderer.
pub mod document;
/// Error types for formatting rules.
pub mod errors;
/// Incremental-tree formatting rules.
pub mod rules;
/// Formatting state shared by rules.
pub mod state;
/// Conversion traits from values to documents.
pub mod to_doc;
/// Whitespace post-processing helpers.
pub mod whitespace;

pub use crate::{
    comment::{Comment, CommentCollector, CommentKind, CommentProcessor},
    document::{Document, IndentStyle, LineEnding, Printer, PrinterConfig},
    errors::FormatResult,
    rules::{FormatRule, RuleSet},
    state::{DefaultFormatState, FormatContext, FormatState},
    to_doc::{AsDocument, ToDocument},
    whitespace::WhitespaceProcessor,
};
pub use data_text_core::language::Language;
pub use language_macro::{AsDocument, FormatRule, define_rules, doc};

/// Type alias for [`Document`].
pub type Doc<'a> = Document<'a>;

/// Empty document.
pub const NIL: Document<'static> = Document::Nil;
/// Forced line break.
pub const LINE: Document<'static> = Document::Line;
/// Soft line break that renders empty while the surrounding group fits.
pub const SOFT_LINE: Document<'static> = Document::SoftLine;
/// Soft line break that renders a space while the surrounding group fits.
pub const SOFT_LINE_SPACE: Document<'static> = Document::SoftLineSpace;
/// Forced line break that propagates to the parent groups.
pub const HARD_LINE: Document<'static> = Document::HardLine;

/// Increase the indentation level of `doc`.
pub fn indent<'a>(doc: Document<'a>) -> Document<'a> {
    Document::indent(doc)
}

/// Join `docs` with `separator`.
pub fn join<'a>(docs: impl IntoIterator<Item = Document<'a>>, separator: Document<'a>) -> Document<'a> {
    Document::join(docs, separator)
}
