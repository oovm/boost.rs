#![doc = include_str!("readme.md")]
pub mod highlighter;

use data_text_koka::language::KokaLanguage;
use {
    language_server::MemoryVfs,
    language_server::{LanguageService, WorkspaceManager},
};
pub struct KokaLanguageService {
    _language: KokaLanguage,
    vfs: MemoryVfs,
    workspace: WorkspaceManager,
}
impl KokaLanguageService {
    pub fn new(language: KokaLanguage) -> Self {
        Self { _language: language, vfs: MemoryVfs::default(), workspace: WorkspaceManager::default() }
    }
}
impl LanguageService for KokaLanguageService {
    type Lang = KokaLanguage;
    type Vfs = MemoryVfs;
    fn vfs(&self) -> &Self::Vfs {
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
