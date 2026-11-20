#![doc = include_str!("readme.md")]
pub mod highlighter;

use data_text_lean::language::LeanLanguage;
use language_server::{LanguageService, MemoryVfs, WorkspaceManager};
pub struct LeanLanguageService {
    config: LeanLanguage,
    vfs: MemoryVfs,
    workspace: WorkspaceManager,
}
impl LeanLanguageService {
    pub fn new(config: LeanLanguage) -> Self {
        Self { config, vfs: MemoryVfs::default(), workspace: WorkspaceManager::default() }
    }
}
impl LanguageService for LeanLanguageService {
    type Lang = LeanLanguage;
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
