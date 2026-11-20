#![doc = include_str!("readme.md")]
pub mod highlighter;

use {
    futures::Future,
    language_server::{DocumentSession, MemoryVfs, Vfs},
    language_server::{service::LanguageService, workspace::WorkspaceManager},
};
pub mod formatter;
use data_text_tex::language::TexLanguage;
use data_text_core::tree::RedNode;
/// TeX language service.
pub struct TexLanguageService {
    vfs: MemoryVfs,
    workspace: WorkspaceManager,
}
impl TexLanguageService {
    /// Creates a new TeX language service.
    pub fn new(vfs: MemoryVfs) -> Self {
        Self { vfs, workspace: WorkspaceManager::default() }
    }
}
impl LanguageService for TexLanguageService {
    type Lang = TexLanguage;
    type Vfs = MemoryVfs;
    fn vfs(&self) -> &Self::Vfs {
        &self.vfs
    }
    fn workspace(&self) -> &WorkspaceManager {
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
            let language = TexLanguage::default();
            let parser = data_text_tex::parser::TexParser::new(&language);
            let lexer = data_text_tex::lexer::TexLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
}
