#![doc = include_str!("readme.md")]
#[cfg(feature = "oak-highlight")]
pub mod highlighter;

#[cfg(feature = "lsp")]
use {
    futures::Future,
    language_hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::{service::LanguageService, workspace::WorkspaceManager},
};
#[cfg(feature = "oak-pretty-print")]
pub mod formatter;
use crate::SmalltalkLanguage;
use core::range::Range;
use data_text_core::tree::RedNode;
#[cfg(feature = "lsp")]
pub struct SmalltalkHoverProvider;
#[cfg(feature = "lsp")]
impl HoverProvider<SmalltalkLanguage> for SmalltalkHoverProvider {
    fn hover(&self, node: &RedNode<SmalltalkLanguage>, _range: Range<usize>) -> Option<Hover> {
        let contents = format!("### Smalltalk Node\nKind: {:?}", node.green.kind);
        Some(Hover { contents, range: Some(node.span()) })
    }
}
#[cfg(feature = "lsp")]
pub struct SmalltalkLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: SmalltalkHoverProvider,
}
impl<V: Vfs> SmalltalkLanguageService<V> {
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: SmalltalkHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for SmalltalkLanguageService<V> {
    type Lang = SmalltalkLanguage;
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
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<SmalltalkLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
