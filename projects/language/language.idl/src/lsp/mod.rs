#![doc = include_str!("readme.md")]
pub mod highlighter;

use data_text_idl::IdlLanguage;
use core::range::Range;
use data_text_core::tree::RedNode;
use {
    futures::Future,
    language_server::hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
};
pub struct IdlHoverProvider;
impl HoverProvider<IdlLanguage> for IdlHoverProvider {
    fn hover(&self, node: &RedNode<IdlLanguage>, _range: Range<usize>) -> Option<Hover> {
        let _kind = node.green.kind;
        None
    }
}
pub struct IdlLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: IdlHoverProvider,
}
impl<V: Vfs> IdlLanguageService<V> {
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: IdlHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for IdlLanguageService<V> {
    type Lang = IdlLanguage;
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
            let language = IdlLanguage::default();
            let parser = data_text_idl::parser::IdlParser::new(&language);
            let lexer = data_text_idl::lexer::IdlLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<IdlLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
