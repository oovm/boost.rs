#![doc = include_str!("readme.md")]
#[cfg(feature = "oak-highlight")]
pub mod highlighter;

use crate::{NixLanguage, parser::element_type::NixElementType};
use core::range::Range;
use data_text_core::tree::RedNode;
#[cfg(feature = "lsp")]
use {language_hover::HoverProvider, language_server::Vfs, language_server::service::LanguageService, std::future::Future};
/// Hover provider implementation for Nix.
#[cfg(feature = "lsp")]
pub struct NixHoverProvider;
#[cfg(feature = "lsp")]
impl HoverProvider<NixLanguage> for NixHoverProvider {
    fn hover(&self, node: &RedNode<NixLanguage>, _range: Range<usize>) -> Option<language_hover::Hover> {
        let kind = node.green.kind;
        let contents = match kind {
            NixElementType::Let => "### Nix Let expression\nStarts a let-binding block.",
            NixElementType::With => "### Nix With expression\nBrings a set's attributes into scope.",
            NixElementType::Import => "### Nix Import\nImports another Nix expression.",
            NixElementType::Root => "### Nix Expression\nThe root of a Nix configuration.",
            _ => return None,
        };
        Some(language_hover::Hover { contents: contents.to_string(), range: Some(node.span()) })
    }
}
/// Language service implementation for Nix.
#[cfg(feature = "lsp")]
pub struct NixLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: NixHoverProvider,
}
impl<V: Vfs> NixLanguageService<V> {
    /// Creates a new Nix language service with the given VFS.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: NixHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for NixLanguageService<V> {
    type Lang = NixLanguage;
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
            let language = NixLanguage::default();
            let parser = crate::parser::NixParser::new(&language);
            let lexer = crate::lexer::NixLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover(&self, uri: &str, range: Range<usize>) -> impl Future<Output = Option<language_server::types::Hover>> + Send + '_ {
        let uri = uri.to_string();
        async move {
            let res = self.with_root(&uri, |root| self.hover_provider.hover(&root, range)).await.flatten();
            res.map(|h| language_server::types::Hover { contents: h.contents, range: h.range })
        }
    }
}
