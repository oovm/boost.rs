#![doc = include_str!("readme.md")]
pub mod highlighter;

use {
    language_server::MemoryVfs,
    language_server::{service::LanguageService, workspace::WorkspaceManager},
};
pub mod formatter;
use data_text_typst::language::TypstLanguage;
/// Typst language service.
pub struct TypstLanguageService {
    vfs: MemoryVfs,
    workspace: WorkspaceManager,
}
impl TypstLanguageService {
    pub fn new(vfs: MemoryVfs) -> Self {
        Self { vfs, workspace: WorkspaceManager::default() }
    }
}
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
