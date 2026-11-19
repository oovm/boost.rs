#![doc = include_str!("readme.md")]
/// Highlighter module.
#[cfg(feature = "language-highlight")]
pub mod highlighter;
/// MCP module.
/// LSP service implementation.
#[cfg(feature = "lsp")]
pub struct DotLanguageService;
