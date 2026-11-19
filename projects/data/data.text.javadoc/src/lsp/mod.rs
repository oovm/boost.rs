#![doc = include_str!("readme.md")]
/// Highlighter module.
#[cfg(feature = "oak-highlight")]
pub mod highlighter;

/// MCP module.
use crate::language::JavadocLanguage;
use data_text_core::tree::RedNode;
#[cfg(feature = "lsp")]
use {language_server::Vfs, language_server::service::LanguageService, std::future::Future};
/// Javadoc language service implementation.
#[cfg(feature = "lsp")]
pub struct JavadocLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
}
#[cfg(feature = "lsp")]
impl<V: Vfs> JavadocLanguageService<V> {
    /// Creates a new Javadoc language service.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default() }
    }
}
#[cfg(feature = "lsp")]
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for JavadocLanguageService<V> {
    type Lang = JavadocLanguage;
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
            let language = JavadocLanguage::default();
            let parser = crate::parser::JavadocParser::new(&language);
            let lexer = crate::lexer::JavadocLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
}
