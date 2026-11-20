#![doc = include_str!("readme.md")]
pub mod highlighter;

use data_text_coq::{CoqLanguage, parser::element_type::CoqElementType};
use core::range::Range;
use data_text_core::tree::RedNode;
use {
    futures::Future,
    language_hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
};
/// Hover provider implementation for Coq.
pub struct CoqHoverProvider;
impl HoverProvider<CoqLanguage> for CoqHoverProvider {
    fn hover(&self, node: &RedNode<CoqLanguage>, _range: Range<usize>) -> Option<Hover> {
        let kind = node.green.kind;
        // Provide context-aware hover information
        let contents = match kind {
            CoqElementType::Declaration => "### Coq Declaration\nDefines a new term, theorem, or inductive type.",
            CoqElementType::Statement => "### Coq Statement\nA vernacular command or tactic.",
            CoqElementType::Expression => "### Coq Expression\nA Coq term or type.",
            _ => return None,
        };
        Some(Hover { contents: contents.to_string(), range: Some(node.span()) })
    }
}
/// Language service implementation for Coq.
pub struct CoqLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: CoqHoverProvider,
}
impl<V: Vfs> CoqLanguageService<V> {
    /// Creates a new `CoqLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: CoqHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for CoqLanguageService<V> {
    type Lang = CoqLanguage;
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
            let language = CoqLanguage::default();
            let parser = data_text_coq::parser::CoqParser::new(&language);
            let lexer = data_text_coq::lexer::CoqLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<CoqLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
pub mod formatter;
