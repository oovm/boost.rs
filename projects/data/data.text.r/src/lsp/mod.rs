#![doc = include_str!("readme.md")]
#[cfg(feature = "oak-highlight")]
pub mod highlighter;

#[cfg(feature = "lsp")]
use futures::Future;
#[cfg(feature = "lsp")]
use language_hover::{Hover, HoverProvider};
#[cfg(feature = "lsp")]
use language_server::Vfs;
#[cfg(feature = "lsp")]
use language_server::service::LanguageService;
#[cfg(feature = "oak-pretty-print")]
pub mod formatter;
use crate::{language::RLanguage, parser::element_type::RElementType};
use core::range::Range;
use data_text_core::tree::RedNode;
#[cfg(feature = "lsp")]
pub struct RHoverProvider;
#[cfg(feature = "lsp")]
impl HoverProvider<RLanguage> for RHoverProvider {
    fn hover(&self, node: &RedNode<RLanguage>, _range: Range<usize>) -> Option<Hover> {
        let kind = node.green.kind;
        let contents = match kind {
            RElementType::FunctionDefinition => "### R Function\nDefines a callable block of code.",
            _ => return None,
        };
        Some(Hover { contents: contents.to_string(), range: Some(node.span()) })
    }
}
#[cfg(feature = "lsp")]
pub struct RLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: RHoverProvider,
}
impl<V: Vfs> RLanguageService<V> {
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: RHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for RLanguageService<V> {
    type Lang = RLanguage;
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
            let language = RLanguage::default();
            let parser = crate::parser::RParser::new(&language);
            let lexer = crate::lexer::RLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<RLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
