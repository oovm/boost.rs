#![doc = include_str!("readme.md")]
pub mod highlighter;

use data_text_ocaml::{OCamlLanguage, parser::element_type::OCamlElementType};
use data_text_core::{Range, tree::RedNode};
use {
    futures::Future,
    language_server::hover::HoverProvider,
    language_server::Vfs,
    language_server::{
        service::LanguageService,
        types::{Hover, Hover as LspHover},
    },
};
/// Hover provider implementation for OCaml.
pub struct OCamlHoverProvider;
impl HoverProvider<OCamlLanguage> for OCamlHoverProvider {
    fn hover(&self, node: &RedNode<OCamlLanguage>, _range: Range<usize>) -> Option<language_server::hover::Hover> {
        let kind = node.green.kind;
        let contents = match kind {
            OCamlElementType::LetBinding => "### OCaml Let Binding\nDefines a value or function binding.",
            OCamlElementType::ModuleDef => "### OCaml Module\nDefines an OCaml module.",
            OCamlElementType::TypeDefinition => "### OCaml Type\nDefines a new type.",
            OCamlElementType::MatchExpr => "### OCaml Match\nPattern matching expression.",
            _ => return None,
        };
        Some(language_server::hover::Hover { contents: contents.to_string(), range: Some(node.span()) })
    }
}
/// Language service implementation for OCaml.
pub struct OCamlLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: OCamlHoverProvider,
}
impl<V: Vfs> OCamlLanguageService<V> {
    /// Creates a new OCaml language service.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: OCamlHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for OCamlLanguageService<V> {
    type Lang = OCamlLanguage;
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
            let language = OCamlLanguage::default();
            let parser = data_text_ocaml::parser::OCamlParser::new(&language);
            let lexer = data_text_ocaml::lexer::OCamlLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover(&self, uri: &str, range: Range<usize>) -> impl std::future::Future<Output = Option<Hover>> + Send + '_ {
        let uri = uri.to_string();
        async move {
            self.with_root(&uri, |root| self.hover_provider.hover(&root, range).map(|h| Hover { contents: h.contents, range: h.range }))
                .await
                .flatten()
        }
    }
}
