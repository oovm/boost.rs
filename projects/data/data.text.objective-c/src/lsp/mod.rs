#![doc = include_str!("readme.md")]
#[cfg(feature = "language-highlight")]
pub mod highlighter;

use crate::{ObjectiveCLanguage, parser::element_type::ObjectiveCElementType};
use core::range::Range;
use data_text_core::tree::RedNode;
#[cfg(feature = "lsp")]
use {
    language_hover::HoverProvider,
    language_server::Vfs,
    language_server::{service::LanguageService, types::Hover},
    std::future::Future,
};
/// Hover provider implementation for Objective-C.
#[cfg(feature = "lsp")]
pub struct ObjectiveCHoverProvider;
#[cfg(feature = "lsp")]
impl HoverProvider<ObjectiveCLanguage> for ObjectiveCHoverProvider {
    fn hover(&self, node: &RedNode<'_, ObjectiveCLanguage>, _range: Range<usize>) -> Option<language_hover::Hover> {
        let kind = node.green.kind;
        let contents = match kind {
            ObjectiveCElementType::InterfaceDeclaration => "### Objective-C @interface\nDefines the interface for a class.",
            ObjectiveCElementType::ImplementationDeclaration => "### Objective-C @implementation\nProvides the implementation for a class.",
            ObjectiveCElementType::ProtocolDeclaration => "### Objective-C @protocol\nDefines a set of methods that a class can implement.",
            ObjectiveCElementType::PropertyDeclaration => "### Objective-C @property\nDeclares a property for a class.",
            _ => return None,
        };
        Some(language_hover::Hover { contents: contents.to_string(), range: Some(node.span()) })
    }
}
/// Language service implementation for Objective-C.
#[cfg(feature = "lsp")]
pub struct ObjectiveCLanguageService<V: Vfs> {
    /// Virtual file system.
    vfs: V,
    /// Workspace manager.
    workspace: language_server::workspace::WorkspaceManager,
    /// Hover provider.
    hover_provider: ObjectiveCHoverProvider,
}
impl<V: Vfs> ObjectiveCLanguageService<V> {
    /// Creates a new Objective-C language service.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: ObjectiveCHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for ObjectiveCLanguageService<V> {
    type Lang = ObjectiveCLanguage;
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
            let language = ObjectiveCLanguage::default();
            let parser = crate::parser::ObjectiveCParser::new(&language);
            let lexer = crate::lexer::ObjectiveCLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover(&self, uri: &str, range: Range<usize>) -> impl Future<Output = Option<Hover>> + Send + '_ {
        let uri = uri.to_string();
        async move {
            let hover = self.with_root(&uri, |root| self.hover_provider.hover(&root, range)).await.flatten()?;
            Some(Hover { contents: hover.contents, range: hover.range })
        }
    }
}
