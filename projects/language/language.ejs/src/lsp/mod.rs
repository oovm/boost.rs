#![doc = include_str!("readme.md")]
pub mod highlighter;

use {
    futures::Future,
    language_server::service::LanguageService,
    language_server::types::Hover as LspHover,
    language_server::{MemoryVfs, Vfs},
};
pub mod formatter;
use data_text_ejs::language::EjsLanguage;
use core::range::Range;
use data_text_core::tree::RedNode;

/// EJS language service.
pub struct EjsLanguageService<V: Vfs = MemoryVfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
}

impl<V: Vfs> EjsLanguageService<V> {
    /// Creates a new `EjsLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default() }
    }
}

impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for EjsLanguageService<V> {
    type Lang = EjsLanguage;
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
            let language = EjsLanguage::default();
            let parser = data_text_ejs::parser::EjsParser::new(&language);
            let lexer = data_text_ejs::lexer::EjsLexer::new(&language);
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
