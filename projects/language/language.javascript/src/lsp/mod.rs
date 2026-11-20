#![doc = include_str!("readme.md")]

pub mod highlighter;

use {futures::Future, language_server::Vfs, language_server::service::LanguageService, language_server::types::Hover as LspHover};
pub mod formatter;
use data_text_javascript::language::JavaScriptLanguage;
use core::range::Range;
use data_text_core::tree::RedNode;

/// JavaScript language service.
pub struct JavaScriptLanguageService<V: Vfs> {
    /// The virtual file system.
    vfs: V,
    /// The workspace manager.
    workspace: language_server::workspace::WorkspaceManager,
}
impl<V: Vfs> JavaScriptLanguageService<V> {
    /// Creates a new JavaScript language service.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default() }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for JavaScriptLanguageService<V> {
    type Lang = JavaScriptLanguage;
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
            let language = JavaScriptLanguage::default();
            let parser = data_text_javascript::parser::JavaScriptParser::new(&language);
            let lexer = data_text_javascript::lexer::JavaScriptLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover(&self, _uri: &str, _range: Range<usize>) -> impl Future<Output = Option<LspHover>> + Send + '_ {
        async move { None }
    }
}
