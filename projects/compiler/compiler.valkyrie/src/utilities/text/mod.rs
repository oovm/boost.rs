//! 文本格式化能力（`data.text` `formatter` / `printer` 负责 lexer/parser，本模块负责 Valkyrie 侧契约）。

pub mod format_syntax;
pub mod to_document;
pub mod valkyrie;

pub use data_text_valkyrie::printer::Document as PrettyDocument;
pub use format_syntax::FormatSyntax;
pub use to_document::ToDocument;
