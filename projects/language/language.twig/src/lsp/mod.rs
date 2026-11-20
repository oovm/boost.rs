#![doc = include_str!("readme.md")]
pub mod highlighter;

use {
    data_text_core::tree::RedNode,
    futures::Future,
    language_server::LanguageService,
    language_server::{DocumentSession, MemoryVfs, Vfs},
};
pub mod formatter;
use data_text_twig::language::TwigLanguage;
/// Twig language service.
pub struct TwigLanguageService {
    vfs: MemoryVfs,
    workspace: language_server::workspace::WorkspaceManager,
}
impl TwigLanguageService {
    /// Creates a new language service.
    pub fn new(vfs: MemoryVfs) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default() }
    }
}
impl LanguageService for TwigLanguageService {
    type Lang = TwigLanguage;
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
            let language = TwigLanguage::default();
            let parser = data_text_twig::parser::TwigParser::new(&language);
            let lexer = data_text_twig::lexer::TwigLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
}
