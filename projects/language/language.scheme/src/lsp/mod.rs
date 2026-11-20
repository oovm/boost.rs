#![doc = include_str!("readme.md")]
pub mod highlighter;

use {
    futures::Future,
    language_hover::{Hover, HoverProvider},
    language_server::{DocumentSession, Vfs},
    language_server::{service::LanguageService, workspace::WorkspaceManager},
};
pub mod formatter;
use data_text_scheme::SchemeLanguage;
use core::range::Range;
use data_text_core::tree::RedNode;
/// Hover provider implementation for Scheme.
pub struct SchemeHoverProvider;
impl HoverProvider<SchemeLanguage> for SchemeHoverProvider {
    fn hover(&self, node: &RedNode<SchemeLanguage>, _range: Range<usize>) -> Option<Hover> {
        let _kind = node.green.kind;
        // Basic Scheme hover implementation
        None
    }
}
/// Language service implementation for Scheme.
pub struct SchemeLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: SchemeHoverProvider,
}
impl<V: Vfs> SchemeLanguageService<V> {
    /// Creates a new `SchemeLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: SchemeHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for SchemeLanguageService<V> {
    type Lang = SchemeLanguage;
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
            let language = SchemeLanguage::default();
            let parser = data_text_scheme::parser::SchemeParser::new(&language);
            let lexer = data_text_scheme::lexer::SchemeLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<SchemeLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
