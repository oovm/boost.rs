#![doc = include_str!("readme.md")]
pub mod highlighter;

use {
    futures::Future,
    language_hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
};
pub mod formatter;
use data_text_regex::language::RegexLanguage;
use core::range::Range;
use data_text_core::tree::RedNode;
/// Hover provider implementation for regular expressions.
pub struct RegexHoverProvider;
impl HoverProvider<RegexLanguage> for RegexHoverProvider {
    /// Provides hover information for a regex node.
    fn hover(&self, _node: &RedNode<'_, RegexLanguage>, _range: Range<usize>) -> Option<Hover> {
        None
    }
}
/// Language service implementation for regular expressions.
pub struct RegexLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: RegexHoverProvider,
}
impl<V: Vfs> RegexLanguageService<V> {
    /// Creates a new `RegexLanguageService` with the given VFS.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: RegexHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for RegexLanguageService<V> {
    type Lang = RegexLanguage;
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
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<RegexLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
    fn completion(&self, _uri: &str, _offset: usize) -> impl Future<Output = Vec<language_server::types::CompletionItem>> + Send + '_ {
        async move { vec![] }
    }
}
