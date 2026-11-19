#![doc = include_str!("readme.md")]
use crate::CssLanguage;
use data_text_core::tree::RedNode;
/// Highlighter module.
#[cfg(feature = "language-highlight")]
pub mod highlighter;

#[cfg(feature = "lsp")]
use {language_server::Vfs, language_server::service::LanguageService, std::future::Future};
/// MCP module.
/// Formatter module.
#[cfg(feature = "lsp")]
pub mod formatter;
/// Language service implementation for CSS.
#[cfg(feature = "lsp")]
pub struct CssLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
}
#[cfg(feature = "lsp")]
impl<V: Vfs> CssLanguageService<V> {
    /// Creates a new `CssLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default() }
    }
}
#[cfg(feature = "lsp")]
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for CssLanguageService<V> {
    type Lang = CssLanguage;
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
            let language = CssLanguage::default();
            let parser = crate::parser::CssParser::new(&language);
            let lexer = crate::lexer::CssLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
}
