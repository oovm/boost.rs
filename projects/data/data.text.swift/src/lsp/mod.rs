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
use crate::language::SwiftLanguage;
use core::range::Range;
use data_text_core::tree::RedNode;
#[cfg(feature = "lsp")]
pub struct SwiftHoverProvider;
#[cfg(feature = "lsp")]
impl HoverProvider<SwiftLanguage> for SwiftHoverProvider {
    fn hover(&self, node: &RedNode<SwiftLanguage>, _range: Range<usize>) -> Option<Hover> {
        let _kind = node.green.kind;
        None
    }
}
#[cfg(feature = "lsp")]
pub struct SwiftLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: SwiftHoverProvider,
}
impl<V: Vfs> SwiftLanguageService<V> {
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: SwiftHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for SwiftLanguageService<V> {
    type Lang = SwiftLanguage;
    type Vfs = V;
    fn vfs(&self) -> &Self::Vfs {
        &self.vfs
    }
    fn workspace(&self) -> &language_server::workspace::WorkspaceManager {
        &self.workspace
    }
    fn with_root<R, F>(&self, uri: &str, f: F) -> impl Future<Output = Option<R>> + Send
    where
        R: Send,
        F: FnOnce(RedNode<'_, Self::Lang>) -> R + Send,
    {
        let source = self.vfs().request_source(uri);
        async move {
            let source = source?;
            let language = SwiftLanguage::default();
            let parser = crate::parser::SwiftParser::new(&language);
            let lexer = crate::lexer::SwiftLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<SwiftLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
