#![doc = include_str!("readme.md")]
pub mod highlighter;

use {
    futures::Future,
    language_server::hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
};
pub mod formatter;
use data_text_j::{JLanguage, parser::JElementType};
use core::range::Range;
use data_text_core::tree::RedNode;
/// Hover provider implementation for J.
pub struct JHoverProvider;
impl HoverProvider<JLanguage> for JHoverProvider {
    fn hover(&self, node: &RedNode<JLanguage>, _range: Range<usize>) -> Option<Hover> {
        let kind = node.green.kind;
        // Provide context-aware hover information
        let contents = match kind {
            JElementType::Sentence => "### J Sentence\nA basic unit of execution in J.",
            JElementType::Assignment => "### J Assignment\nAssigns a value to a name locally (=.) or globally (=:).",
            JElementType::Verb => "### J Verb\nA function that operates on nouns.",
            _ => return None,
        };
        Some(Hover { contents: contents.to_string(), range: Some(node.span()) })
    }
}
/// Language service implementation for J.
pub struct JLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: JHoverProvider,
}
impl<V: Vfs> JLanguageService<V> {
    /// Creates a new `JLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: JHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for JLanguageService<V> {
    type Lang = JLanguage;
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
            let language = JLanguage::default();
            let parser = data_text_j::parser::JParser::new(&language);
            let lexer = data_text_j::lexer::JLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<JLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
