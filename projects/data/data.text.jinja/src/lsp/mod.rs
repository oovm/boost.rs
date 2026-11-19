/// LSP module for Jinja
///
/// This module provides LSP (Language Server Protocol) support for Jinja templates.

#[cfg(feature = "language-highlight")]
pub mod highlighter;

#[cfg(feature = "lsp")]
use {data_text_core::tree::RedNode, language_server::LanguageService, language_server::MemoryVfs, std::future::Future};
pub mod formatter;

use crate::language::JinjaLanguage;

/// Jinja language service.
#[cfg(feature = "lsp")]
pub struct JinjaLanguageService {
    vfs: MemoryVfs,
    workspace: language_server::workspace::WorkspaceManager,
}

#[cfg(feature = "lsp")]
impl JinjaLanguageService {
    /// Creates a new language service.
    pub fn new(vfs: MemoryVfs) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default() }
    }
}

#[cfg(feature = "lsp")]
impl LanguageService for JinjaLanguageService {
    type Lang = JinjaLanguage;
    type Vfs = MemoryVfs;

    fn vfs(&self) -> &Self::Vfs {
        &self.vfs
    }

    fn workspace(&self) -> &language_server::workspace::WorkspaceManager {
        &self.workspace
    }

    fn with_root<R, F>(&self, _uri: &str, _f: F) -> impl Future<Output = Option<R>> + Send
    where
        R: Send,
        F: FnOnce(RedNode<'_, Self::Lang>) -> R + Send,
    {
        async { None }
    }
}
