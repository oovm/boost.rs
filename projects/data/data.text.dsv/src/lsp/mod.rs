#![doc = include_str!("readme.md")]
use crate::{Dsv, language::DsvLanguage};
use core::range::Range;
use data_text_core::tree::RedNode;
#[cfg(feature = "lsp")]
use {futures::Future, language_server::Vfs, language_server::service::LanguageService, language_server::types::Hover as LspHover};
#[cfg(feature = "lsp")]
/// Language service implementation for DSV.
pub struct DsvLanguageService<const LANG: DsvLanguage, V: Vfs> {
    /// The virtual file system.
    vfs: V,
    /// The workspace manager.
    workspace: language_server::workspace::WorkspaceManager,
}
impl<const LANG: DsvLanguage, V: Vfs> DsvLanguageService<LANG, V> {
    /// Creates a new `DsvLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::new() }
    }
}
impl<const LANG: DsvLanguage, V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for DsvLanguageService<LANG, V> {
    type Lang = Dsv<LANG>;
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
    fn hover(&self, _uri: &str, _range: Range<usize>) -> impl Future<Output = Option<LspHover>> + Send + '_ {
        async move { None }
    }
}
