#![doc = include_str!("readme.md")]

pub mod formatter;
pub mod highlighter;

use data_text_raku::{RakuLanguage, parser::element_type::RakuElementType};
use core::range::Range;
use data_text_core::tree::RedNode;
use {
    futures::Future,
    language_hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
};

/// Raku hover provider implementation.
pub struct RakuHoverProvider;

impl HoverProvider<RakuLanguage> for RakuHoverProvider {
    fn hover(&self, node: &RedNode<RakuLanguage>, _range: Range<usize>) -> Option<Hover> {
        let kind = node.green.kind;
        let contents = match kind {
            RakuElementType::Statement => "### Raku Statement\nA single execution unit in Raku.",
            RakuElementType::Expression => "### Raku Expression\nA piece of code that evaluates to a value.",
            _ => return None,
        };

        Some(Hover { contents: contents.to_string(), range: Some(node.span()) })
    }
}

/// Raku language service implementation.
pub struct RakuLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: RakuHoverProvider,
}

impl<V: Vfs> RakuLanguageService<V> {
    /// Creates a new `RakuLanguageService` with the given VFS.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: RakuHoverProvider }
    }
}

impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for RakuLanguageService<V> {
    type Lang = RakuLanguage;
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

    fn hover_provider(&self) -> Option<&(dyn HoverProvider<RakuLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
