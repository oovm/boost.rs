#![doc = include_str!("readme.md")]
pub mod formatter;
pub mod highlighter;

use data_text_tailwind::language::TailwindLanguage;

use {
    data_text_core::tree::RedNode,
    language_server::LanguageService,
    language_server::{DocumentSession, MemoryVfs, Vfs},
    std::future::Future,
};

/// Language service implementation for Tailwind CSS.
pub struct TailwindLanguageService {
    vfs: MemoryVfs,
    workspace: language_server::workspace::WorkspaceManager,
}

impl TailwindLanguageService {
    /// Creates a new `TailwindLanguageService` with the given VFS.
    pub fn new(vfs: MemoryVfs) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default() }
    }
}

impl LanguageService for TailwindLanguageService {
    type Lang = TailwindLanguage;
    type Vfs = MemoryVfs;

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
            let language = TailwindLanguage::default();
            let parser = data_text_tailwind::parser::TailwindParser::new(language);
            let lexer = data_text_tailwind::lexer::TailwindLexer::new(language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }

    fn completion(
        &self,
        _uri: &str,
        _offset: usize,
    ) -> impl std::future::Future<Output = Vec<language_server::types::CompletionItem>> + Send + '_ {
        async move { vec![] }
    }
}
