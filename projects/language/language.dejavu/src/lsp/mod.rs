#![doc = include_str!("readme.md")]
pub mod formatter;
pub mod highlighter;

use data_text_dejavu::language::DejavuLanguage;
use core::range::Range;
use data_text_core::tree::RedNode;

use {futures::Future, language_server::Vfs, language_server::service::LanguageService, language_server::types::Hover as LspHover};

/// Language service implementation for Dejavu.
pub struct DejavuLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
}

impl<V: Vfs> DejavuLanguageService<V> {
    /// Creates a new `DejavuLanguageService` with the given VFS.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::new() }
    }
}

impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for DejavuLanguageService<V> {
    type Lang = DejavuLanguage;
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
            let language = DejavuLanguage::default();
            let parser = data_text_dejavu::parser::DejavuParser::new(&language);
            let lexer = data_text_dejavu::lexer::DejavuLexer::new(&language);
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
