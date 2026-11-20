#![doc = include_str!("readme.md")]
pub mod highlighter;

use {language_server::LanguageService, language_server::MemoryVfs};
pub mod formatter;
use data_text_vala::language::ValaLanguage;
/// Vala language service.
pub struct ValaLanguageService {
    vfs: MemoryVfs,
    workspace: language_server::workspace::WorkspaceManager,
}
impl ValaLanguageService {
    /// Creates a new Vala language service.
    pub fn new(vfs: MemoryVfs) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default() }
    }
}
impl LanguageService for ValaLanguageService {
    type Lang = ValaLanguage;
    type Vfs = MemoryVfs;
    fn vfs(&self) -> &Self::Vfs {
        &self.vfs
    }
    fn workspace(&self) -> &language_server::workspace::WorkspaceManager {
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
