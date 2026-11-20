#![doc = include_str!("readme.md")]
pub mod highlighter;

use data_text_kotlin::language::KotlinLanguage;
use {
    language_server::MemoryVfs,
    language_server::{LanguageService, WorkspaceManager},
};
pub struct KotlinLanguageService {
    _language: KotlinLanguage,
    vfs: MemoryVfs,
    workspace: WorkspaceManager,
}
impl KotlinLanguageService {
    pub fn new(language: KotlinLanguage) -> Self {
        Self { _language: language, vfs: MemoryVfs::default(), workspace: WorkspaceManager::default() }
    }
}
impl LanguageService for KotlinLanguageService {
    type Lang = KotlinLanguage;
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
