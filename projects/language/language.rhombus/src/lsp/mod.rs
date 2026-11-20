#![doc = include_str!("readme.md")]
pub mod highlighter;

use {
    futures::Future,
    language_hover::{Hover, HoverProvider},
    language_server::{DocumentSession, Vfs},
    language_server::{service::LanguageService, workspace::WorkspaceManager},
};
pub mod formatter;
use data_text_rhombus::RhombusLanguage;
use core::range::Range;
use data_text_core::tree::RedNode;
/// Hover provider implementation for Rhombus.
pub struct RhombusHoverProvider;
impl HoverProvider<RhombusLanguage> for RhombusHoverProvider {
    fn hover(&self, node: &RedNode<RhombusLanguage>, _range: Range<usize>) -> Option<Hover> {
        let _kind = node.green.kind;
        // Basic Rhombus hover implementation
        None
    }
}
/// Language service implementation for Rhombus.
pub struct RhombusLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: RhombusHoverProvider,
}
impl<V: Vfs> RhombusLanguageService<V> {
    /// Creates a new `RhombusLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: RhombusHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for RhombusLanguageService<V> {
    type Lang = RhombusLanguage;
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
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<RhombusLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
