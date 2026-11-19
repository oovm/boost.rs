#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

pub mod ansi;
pub mod highlighter;
pub mod html;
pub mod kind;
pub mod span;
pub mod syntax;
pub mod themes;

pub use ansi::{AnsiExporter, Exporter, HighlightResult};
pub use highlighter::{
    AnalysisContext, HighlightRequest, Highlighter, HighlighterKind, HighlighterProvider, HighlighterRegistry, KindClassifier, SemanticKindMap,
    SyntaxHighlighter,
};
pub use html::{escape_html, render_spans_html};
pub use kind::HighlightKind;
pub use span::{HighlightSpan, merge_spans};
