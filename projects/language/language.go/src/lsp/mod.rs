#![doc = include_str!("readme.md")]
pub mod highlighter;

use {language_server::LanguageService, language_server::MemoryVfs, std::future::Future};
pub mod formatter;
/// Go language LSP service.
use data_text_go::language::GoLanguage;
use data_text_core::tree::RedNode;
/// Go language service.
pub struct GoLanguageService {
    vfs: MemoryVfs,
    workspace: language_server::workspace::WorkspaceManager,
}
impl GoLanguageService {
    /// Creates a new `GoLanguageService`.
    pub fn new(vfs: MemoryVfs) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default() }
    }
}
impl LanguageService for GoLanguageService {
    type Lang = GoLanguage;
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
