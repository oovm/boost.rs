#![doc = include_str!("readme.md")]
pub mod highlighter;

use {
    language_server::hover::{Hover as ProviderHover, HoverProvider},
    language_server::Vfs,
    language_server::{service::LanguageService, types::Hover as LspHover},
};
pub mod formatter;
use data_text_toml::{language::TomlLanguage, lexer::token_type::TomlTokenKind};
use core::range::Range;
use data_text_core::tree::RedNode;
/// Hover provider for TOML.
pub struct TomlHoverProvider;
impl HoverProvider<TomlLanguage> for TomlHoverProvider {
    fn hover(&self, node: &RedNode<'_, TomlLanguage>, _range: Range<usize>) -> Option<ProviderHover> {
        let kind = node.green.kind;
        let contents = match kind {
            TomlTokenKind::BareKey | TomlTokenKind::Identifier => "### TOML Key\nConfiguration key identifier.",
            TomlTokenKind::LeftBracket => "### TOML Table\nDefines a section of configuration.",
            TomlTokenKind::DoubleLeftBracket => "### TOML Array of Tables\nDefines a collection of table sections.",
            _ => return None,
        };
        Some(ProviderHover { contents: contents.to_string(), range: Some(node.span()) })
    }
}
/// Language service implementation for TOML.
///
/// Provides IDE features such as hover and workspace management
/// by integrating with the `language-server` framework and `Vfs`.
pub struct TomlLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: TomlHoverProvider,
}
impl<V: Vfs> TomlLanguageService<V> {
    /// Creates a new `TomlLanguageService` with the given virtual file system.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: TomlHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for TomlLanguageService<V> {
    type Lang = TomlLanguage;
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
            let language = TomlLanguage::default();
            let parser = data_text_toml::parser::TomlParser::new(&language);
            let lexer = data_text_toml::lexer::TomlLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover(&self, uri: &str, range: Range<usize>) -> impl futures::Future<Output = Option<LspHover>> + Send + '_ {
        let uri = uri.to_string();
        async move {
            self.with_root(&uri, |root| {
                use data_text_core::tree::RedTree;
                let node = match root.child_at_offset(range.start) {
                    Some(RedTree::Node(n)) => n,
                    _ => root,
                };
                self.hover_provider.hover(&node, range).map(|h| LspHover { contents: h.contents, range: h.range })
            })
            .await
            .flatten()
        }
    }
}
