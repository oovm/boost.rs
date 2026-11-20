#![doc = include_str!("readme.md")]
pub mod highlighter;

use {
    futures::Future,
    language_hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
};
pub mod formatter;
use data_text_ada::{AdaLanguage, parser::AdaElementType};
use core::range::Range;
use data_text_core::tree::RedNode;
/// Hover provider implementation for Ada.
pub struct AdaHoverProvider;
impl HoverProvider<AdaLanguage> for AdaHoverProvider {
    fn hover(&self, node: &RedNode<AdaLanguage>, _range: Range<usize>) -> Option<Hover> {
        let kind = node.green.kind;
        // Provide context-aware hover information
        let contents = match kind {
            AdaElementType::SubprogramDeclaration => "### Ada Subprogram\nDefines a procedure or function.",
            AdaElementType::PackageDeclaration => "### Ada Package\nDefines a module of code.",
            _ => return None,
        };
        Some(Hover { contents: contents.to_string(), range: Some(node.span()) })
    }
}
/// Language service implementation for Ada.
pub struct AdaLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: AdaHoverProvider,
}
impl<V: Vfs> AdaLanguageService<V> {
    /// Creates a new `AdaLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: AdaHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for AdaLanguageService<V> {
    type Lang = AdaLanguage;
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
            let language = AdaLanguage::default();
            let parser = data_text_ada::parser::AdaParser::new(&language);
            let lexer = data_text_ada::lexer::AdaLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<AdaLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
