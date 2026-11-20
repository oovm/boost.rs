#![doc = include_str!("readme.md")]
use data_text_less::LessLanguage;
use data_text_core::tree::RedNode;
/// Highlighter module.
pub mod highlighter;

use {language_server::Vfs, language_server::service::LanguageService, std::future::Future};
/// MCP module.
/// Formatter module.
pub mod formatter;
/// Language service implementation for Less.
pub struct LessLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
}
impl<V: Vfs> LessLanguageService<V> {
    /// Creates a new `LessLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default() }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for LessLanguageService<V> {
    type Lang = LessLanguage;
    type Vfs = V;
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
