#![doc = include_str!("readme.md")]
#[cfg(feature = "language-highlight")]
pub mod highlighter;

use crate::{OrgModeElementType, OrgModeLanguage};
use core::range::Range;
use data_text_core::tree::RedNode;
#[cfg(feature = "lsp")]
use language_hover::{Hover as ProviderHover, HoverProvider};
#[cfg(feature = "lsp")]
use language_server::Vfs;
#[cfg(feature = "lsp")]
use language_server::{service::LanguageService, types::Hover as LspHover};
/// Hover provider implementation for Org-mode.
#[cfg(feature = "lsp")]
pub struct OrgModeHoverProvider;
#[cfg(feature = "lsp")]
impl HoverProvider<OrgModeLanguage> for OrgModeHoverProvider {
    fn hover(&self, node: &RedNode<OrgModeLanguage>, _range: Range<usize>) -> Option<ProviderHover> {
        let kind = node.green.kind;
        let contents = match kind {
            OrgModeElementType::Heading => "### Org Heading\nA hierarchical heading in the document.",
            OrgModeElementType::TodoKeyword => "### TODO Keyword\nAn actionable item.",
            OrgModeElementType::Block => "### Org Block\nA special block (e.g., code, quote).",
            _ => return None,
        };
        Some(ProviderHover { contents: contents.to_string(), range: Some(node.span()) })
    }
}
/// Language service implementation for Org-mode.
#[cfg(feature = "lsp")]
pub struct OrgModeLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: OrgModeHoverProvider,
}
impl<V: Vfs> OrgModeLanguageService<V> {
    /// Creates a new `OrgModeLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: OrgModeHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for OrgModeLanguageService<V> {
    type Lang = OrgModeLanguage;
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
            let language = OrgModeLanguage::default();
            let parser = crate::parser::OrgModeParser::new(&language);
            let lexer = crate::lexer::OrgModeLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover(&self, uri: &str, range: Range<usize>) -> impl std::future::Future<Output = Option<LspHover>> + Send + '_ {
        let uri = uri.to_string();
        async move {
            self.with_root(&uri, |root| self.hover_provider.hover(&root, range).map(|h| LspHover { contents: h.contents, range: h.range }))
                .await
                .flatten()
        }
    }
}
