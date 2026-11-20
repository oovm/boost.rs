#![doc = include_str!("readme.md")]
pub mod highlighter;

use data_text_handlebars::{HandlebarsElementType, HandlebarsLanguage};
use core::range::Range;
use data_text_core::tree::RedNode;
use {
    futures::Future,
    language_hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
};
/// Hover provider implementation for Handlebars.
pub struct HandlebarsHoverProvider;
impl HoverProvider<HandlebarsLanguage> for HandlebarsHoverProvider {
    fn hover(&self, node: &RedNode<HandlebarsLanguage>, _range: Range<usize>) -> Option<Hover> {
        let kind = node.green.kind;
        // Provide context-aware hover information
        let contents = match kind {
            HandlebarsElementType::Mustache => "### Handlebars Mustache\nA basic template expression.",
            HandlebarsElementType::Block => "### Handlebars Block\nA block helper expression.",
            HandlebarsElementType::Partial => "### Handlebars Partial\nIncludes another template.",
            HandlebarsElementType::CommentNode => "### Handlebars Comment\nA template comment.",
            _ => return None,
        };
        Some(Hover { contents: contents.to_string(), range: Some(node.span()) })
    }
}
/// Language service implementation for Handlebars.
pub struct HandlebarsLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: HandlebarsHoverProvider,
}
impl<V: Vfs> HandlebarsLanguageService<V> {
    /// Creates a new `HandlebarsLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: HandlebarsHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for HandlebarsLanguageService<V> {
    type Lang = HandlebarsLanguage;
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
            let language = HandlebarsLanguage::default();
            let parser = data_text_handlebars::parser::HandlebarsParser::new(&language);
            let lexer = data_text_handlebars::lexer::HandlebarsLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<HandlebarsLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
