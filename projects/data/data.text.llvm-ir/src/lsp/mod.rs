#![doc = include_str!("readme.md")]
#[cfg(feature = "language-highlight")]
pub mod highlighter;

use crate::language::LLvmLanguage;
use data_text_core::{Range, tree::RedNode};
#[cfg(feature = "lsp")]
use {
    futures::Future,
    language_hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
    language_server::types::Hover as LspHover,
};
/// Hover provider implementation for LLVM IR.
#[cfg(feature = "lsp")]
pub struct LLirHoverProvider;
#[cfg(feature = "lsp")]
impl HoverProvider<LLvmLanguage> for LLirHoverProvider {
    fn hover(&self, node: &RedNode<LLvmLanguage>, _range: Range<usize>) -> Option<Hover> {
        let _kind = node.green.kind;
        // TODO: Provide context-aware hover information
        None
    }
}
/// Language service implementation for LLVM IR.
#[cfg(feature = "lsp")]
pub struct LLirLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: LLirHoverProvider,
}
#[cfg(feature = "lsp")]
impl<V: Vfs> LLirLanguageService<V> {
    /// Creates a new `LLirLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: LLirHoverProvider }
    }
}
#[cfg(feature = "lsp")]
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
            let parser = crate::parser::LLirParser::new(&language);
            let lexer = crate::lexer::LLvmLexer::new(&language);
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
#[cfg(feature = "lsp")]
pub struct LLvmLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
}
#[cfg(feature = "lsp")]
impl<V: Vfs> LLvmLanguageService<V> {
    /// Creates a new `LLvmLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::new() }
    }
}
#[cfg(feature = "lsp")]
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
            let parser = crate::parser::LLirParser::new(&language);
            let lexer = crate::lexer::LLvmLexer::new(&language);
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
