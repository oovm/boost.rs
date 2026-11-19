#![doc = include_str!("readme.md")]
#[cfg(feature = "language-highlight")]
pub mod highlighter;

#[cfg(feature = "lsp")]
use {
    language_server::MemoryVfs,
    language_server::{service::LanguageService, workspace::WorkspaceManager},
};
pub mod formatter;
use crate::language::TypstLanguage;
/// Typst language service.
#[cfg(feature = "lsp")]
pub struct TypstLanguageService {
    vfs: MemoryVfs,
    workspace: WorkspaceManager,
}
#[cfg(feature = "lsp")]
impl TypstLanguageService {
    pub fn new(vfs: MemoryVfs) -> Self {
        Self { vfs, workspace: WorkspaceManager::default() }
    }
}
#[cfg(feature = "lsp")]
impl LanguageService for TypstLanguageService {
    type Lang = TypstLanguage;
    type Vfs = MemoryVfs;
    fn vfs(&self) -> &MemoryVfs {
        &self.vfs
    }
    fn workspace(&self) -> &WorkspaceManager {
        &self.workspace
    }
    fn with_root<R, F>(&self, _uri: &str, _f: F) -> impl std::future::Future<Output = Option<R>> + Send
    where
        R: Send,
        F: FnOnce(data_text_core::tree::RedNode<'_, Self::Lang>) -> R + Send,
    {
        async { None }
    }
}
