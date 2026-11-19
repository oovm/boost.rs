#![doc = include_str!("readme.md")]
#[cfg(feature = "oak-highlight")]
pub mod highlighter;

use crate::{PythonElementType, language::PythonLanguage};
use core::range::Range;
use data_text_core::tree::RedNode;
#[cfg(feature = "lsp")]
use {
    futures::Future,
    language_hover::{Hover as HoverInfo, HoverProvider},
    language_server::Vfs,
    language_server::{service::LanguageService, types::Hover as LspHover},
};
/// Hover provider for Python.
#[cfg(feature = "lsp")]
pub struct PythonHoverProvider;
#[cfg(feature = "lsp")]
impl HoverProvider<PythonLanguage> for PythonHoverProvider {
    fn hover(&self, node: &RedNode<PythonLanguage>, _range: Range<usize>) -> Option<HoverInfo> {
        let kind = node.green.kind;
        let contents = match kind {
            PythonElementType::FunctionDef => "### Function Definition\nA standard Python function defined with `def`.",
            PythonElementType::ClassDef => "### Class Definition\nA standard Python class defined with `class`.",
            PythonElementType::Import | PythonElementType::ImportFrom => "### Import Statement\nImports modules or specific symbols.",
            _ => return None,
        };
        Some(HoverInfo { contents: contents.to_string(), range: Some(node.span()) })
    }
}
/// Language service for Python.
#[cfg(feature = "lsp")]
pub struct PythonLanguageService<V: Vfs> {
    /// Reference to the virtual file system.
    vfs: V,
    /// Reference to the workspace manager.
    workspace: language_server::workspace::WorkspaceManager,
    /// Reference to the hover provider.
    hover_provider: PythonHoverProvider,
}

impl<V: Vfs> PythonLanguageService<V> {
    /// Creates a new Python language service.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::new(), hover_provider: PythonHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for PythonLanguageService<V> {
    type Lang = PythonLanguage;
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
            let language = PythonLanguage::default();
            let parser = crate::parser::PythonParser::new(&language);
            let lexer = crate::lexer::PythonLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<PythonLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
