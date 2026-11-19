//! 跨语言语法高亮服务。
//!
//! # 归属
//!
//! 本 crate 拥有宿主 dispatch 与结果适配；词法/语法分类消费 `data.text` 与具体语言事实，语义高亮消费
//! 具体语言的编译分析。颜色主题、ANSI、HTML 与编辑器 token scope 属于展示适配。
//!
//! 与 `language.format` 一样，高亮一个文档不需要启动 language server。

#![allow(missing_docs)]

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
