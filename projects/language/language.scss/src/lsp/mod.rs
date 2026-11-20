#![doc = include_str!("readme.md")]
pub mod highlighter;

use {
    futures::Future,
    language_server::hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
};
pub mod formatter;
use data_text_scss::{ScssLanguage, lexer::token_type::ScssTokenType, parser::element_type::ScssElementType};
use core::range::Range;
use data_text_core::tree::RedNode;
/// Hover provider implementation for SCSS.
pub struct ScssHoverProvider;
impl HoverProvider<ScssLanguage> for ScssHoverProvider {
    fn hover(&self, node: &RedNode<'_, ScssLanguage>, _range: Range<usize>) -> Option<Hover> {
        let kind = node.green.kind;
        // Provide context-aware hover information
        let contents = match kind {
            ScssElementType::Selector => "### SCSS Selector\nDefines the target elements for styling.",
            ScssElementType::Property => "### SCSS Property\nDefines a style attribute.",
            ScssElementType::VariableDeclaration => "### SCSS Variable\nDefines a reusable value.",
            _ => return None,
        };
        Some(Hover { contents: contents.to_string(), range: Some(node.span()) })
    }
}
/// Language service implementation for SCSS.
pub struct ScssLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: ScssHoverProvider,
}
impl<V: Vfs> ScssLanguageService<V> {
    /// Creates a new `ScssLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: ScssHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for ScssLanguageService<V> {
    type Lang = ScssLanguage;
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
            let language = ScssLanguage::default();
            let parser = data_text_scss::parser::ScssParser::new(&language);
            let lexer = data_text_scss::lexer::ScssLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<ScssLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
