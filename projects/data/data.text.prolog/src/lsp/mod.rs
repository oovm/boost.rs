#![doc = include_str!("readme.md")]
#[cfg(feature = "language-highlight")]
pub mod highlighter;

use crate::{language::PrologLanguage, parser::element_type::PrologElementType};
use core::range::Range;
use data_text_core::tree::RedNode;
#[cfg(feature = "lsp")]
use {
    language_hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
    std::future::Future,
};
/// Hover provider implementation for Prolog.
#[cfg(feature = "lsp")]
pub struct PrologHoverProvider;
#[cfg(feature = "lsp")]
impl HoverProvider<PrologLanguage> for PrologHoverProvider {
    fn hover(&self, node: &RedNode<'_, PrologLanguage>, _range: Range<usize>) -> Option<Hover> {
        let kind = node.green.kind;
        let contents = match kind {
            PrologElementType::Clause => "### Prolog Clause\nA statement that is either a fact or a rule.",
            PrologElementType::Directive => "### Prolog Directive\nA command to the Prolog system.",
            PrologElementType::Query => "### Prolog Query\nA goal to be proven by the Prolog system.",
            _ => return None,
        };
        Some(Hover { contents: contents.to_string(), range: Some(node.span()) })
    }
}
/// Language service implementation for Prolog.
#[cfg(feature = "lsp")]
pub struct PrologLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: PrologHoverProvider,
}
impl<V: Vfs> PrologLanguageService<V> {
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: PrologHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for PrologLanguageService<V> {
    type Lang = PrologLanguage;
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
            let language = PrologLanguage::default();
            let parser = crate::parser::PrologParser::new(&language);
            let lexer = crate::lexer::PrologLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover(&self, uri: &str, range: Range<usize>) -> impl Future<Output = Option<language_server::Hover>> + Send + '_ {
        let uri = uri.to_string();
        async move {
            let hover = self.with_root(&uri, |root| self.hover_provider.hover(&root, range)).await.flatten()?;
            Some(language_server::Hover { contents: hover.contents, range: hover.range })
        }
    }
}
