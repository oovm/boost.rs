#![doc = include_str!("readme.md")]
use data_text_stylus::language::StylusLanguage;
use core::range::Range;
use data_text_core::tree::RedNode;
/// Highlighter module.
pub mod highlighter;

use {
    futures::Future,
    language_hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
};
/// MCP module.
/// Formatter module.
pub mod formatter;
pub struct StylusHoverProvider;
impl HoverProvider<StylusLanguage> for StylusHoverProvider {
    fn hover(&self, _node: &RedNode<StylusLanguage>, _range: Range<usize>) -> Option<Hover> {
        None
    }
}
pub struct StylusLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: StylusHoverProvider,
}
impl<V: Vfs> StylusLanguageService<V> {
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: StylusHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for StylusLanguageService<V> {
    type Lang = StylusLanguage;
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
            let language = StylusLanguage::default();
            let parser = data_text_stylus::parser::StylusParser::new(&language);
            let lexer = data_text_stylus::lexer::StylusLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<StylusLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
