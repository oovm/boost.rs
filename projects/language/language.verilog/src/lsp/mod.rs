#![doc = include_str!("readme.md")]
use data_text_verilog::VerilogLanguage;
use data_text_core::tree::RedNode;
/// Highlighter module.
pub mod highlighter;

use {language_server::Vfs, language_server::service::LanguageService, std::future::Future};
/// MCP module.
/// Formatter module.
pub mod formatter;
/// Language service implementation for Verilog.
pub struct VerilogLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
}
impl<V: Vfs> VerilogLanguageService<V> {
    /// Creates a new `VerilogLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default() }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for VerilogLanguageService<V> {
    type Lang = VerilogLanguage;
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
            let language = VerilogLanguage::default();
            let parser = data_text_verilog::parser::VerilogParser::new(&language);
            let lexer = data_text_verilog::lexer::VerilogLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
}
