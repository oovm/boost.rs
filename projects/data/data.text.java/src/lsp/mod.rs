#![doc = include_str!("readme.md")]

#[cfg(feature = "oak-highlight")]
pub mod highlighter;

use crate::language::JavaLanguage;
use data_text_core::tree::RedNode;
#[cfg(feature = "lsp")]
use {language_server::LanguageService, language_server::Vfs, std::future::Future};

/// Language service for Java.
#[cfg(feature = "lsp")]
pub struct JavaLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
}
impl<V: Vfs> JavaLanguageService<V> {
    /// Creates a new `JavaLanguageService`.
    pub fn new(vfs: V, _language: JavaLanguage) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default() }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for JavaLanguageService<V> {
    type Lang = JavaLanguage;
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
            let language = JavaLanguage::default();
            let parser = crate::parser::JavaParser::new(&language);
            let lexer = crate::lexer::JavaLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
}
