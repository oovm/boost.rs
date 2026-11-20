#![doc = include_str!("readme.md")]
pub mod highlighter;

use data_text_llvm_ir::language::LLvmLanguage;
use data_text_core::{Range, tree::RedNode};
use {
    futures::Future,
    language_hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
    language_server::types::Hover as LspHover,
};
/// Hover provider implementation for LLVM IR.
pub struct LLirHoverProvider;
impl HoverProvider<LLvmLanguage> for LLirHoverProvider {
    fn hover(&self, node: &RedNode<LLvmLanguage>, _range: Range<usize>) -> Option<Hover> {
        let _kind = node.green.kind;
        // TODO: Provide context-aware hover information
        None
    }
}
/// Language service implementation for LLVM IR.
pub struct LLirLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: LLirHoverProvider,
}
impl<V: Vfs> LLirLanguageService<V> {
    /// Creates a new `LLirLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: LLirHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for LLirLanguageService<V> {
    type Lang = LLvmLanguage;
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
            let language = LLvmLanguage::default();
            let parser = data_text_llvm_ir::parser::LLirParser::new(&language);
            let lexer = data_text_llvm_ir::lexer::LLvmLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<LLvmLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
/// Language service implementation for LLVM IR (Legacy name).
pub struct LLvmLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
}
impl<V: Vfs> LLvmLanguageService<V> {
    /// Creates a new `LLvmLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::new() }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for LLvmLanguageService<V> {
    type Lang = LLvmLanguage;
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
            let language = LLvmLanguage::default();
            let parser = data_text_llvm_ir::parser::LLirParser::new(&language);
            let lexer = data_text_llvm_ir::lexer::LLvmLexer::new(&language);
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
