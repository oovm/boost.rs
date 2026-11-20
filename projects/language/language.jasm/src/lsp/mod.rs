#![doc = include_str!("readme.md")]

pub mod highlighter;

use data_text_jasm::JasmLanguage;
use core::range::Range;
use data_text_core::tree::RedNode;
use {
    futures::Future,
    language_hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
};
/// Hover provider for Jasm.
pub struct JasmHoverProvider;
impl HoverProvider<JasmLanguage> for JasmHoverProvider {
    fn hover(&self, node: &RedNode<JasmLanguage>, _range: Range<usize>) -> Option<Hover> {
        let kind = node.green.kind;
        let contents = format!("### JASM Node\nKind: {:?}", kind);
        Some(Hover { contents, range: Some(node.span()) })
    }
}
/// Language service for Jasm.
pub struct JasmLanguageService<V: Vfs> {
    /// The virtual file system.
    pub vfs: V,
    /// The workspace manager.
    pub workspace: language_server::workspace::WorkspaceManager,
    /// The hover provider.
    pub hover_provider: JasmHoverProvider,
}
impl<V: Vfs> JasmLanguageService<V> {
    /// Creates a new instance of `JasmLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: JasmHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for JasmLanguageService<V> {
    type Lang = JasmLanguage;
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
            let language = JasmLanguage::default();
            let parser = data_text_jasm::parser::JasmParser::new(&language);
            let lexer = data_text_jasm::lexer::JasmLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<JasmLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
