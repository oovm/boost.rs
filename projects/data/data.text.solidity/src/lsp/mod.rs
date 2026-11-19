#![doc = include_str!("readme.md")]
#[cfg(feature = "language-highlight")]
pub mod highlighter;

#[cfg(feature = "lsp")]
use {
    futures::Future,
    language_hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
};
pub mod formatter;
use crate::language::SolidityLanguage;
use core::range::Range;
use data_text_core::tree::RedNode;
#[cfg(feature = "lsp")]
pub struct SolidityHoverProvider;
#[cfg(feature = "lsp")]
impl HoverProvider<SolidityLanguage> for SolidityHoverProvider {
    fn hover(&self, node: &RedNode<SolidityLanguage>, _range: Range<usize>) -> Option<Hover> {
        let _kind = node.green.kind;
        // Solidity specific kinds would go here
        None
    }
}
#[cfg(feature = "lsp")]
pub struct SolidityLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: SolidityHoverProvider,
}
impl<V: Vfs> SolidityLanguageService<V> {
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: SolidityHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for SolidityLanguageService<V> {
    type Lang = SolidityLanguage;
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
        // Solidity lexer/parser traits are not wired yet.
        async { None }
    }
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<SolidityLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
