#![doc = include_str!("readme.md")]
pub mod highlighter;

use data_text_haskell::{HaskellLanguage, lexer::token_type::HaskellTokenType, parser::element_type::HaskellElementType};
use core::range::Range;
use data_text_core::tree::RedNode;
use {
    futures::Future,
    language_server::hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
};
/// Hover provider implementation for Haskell.
pub struct HaskellHoverProvider;
impl HoverProvider<HaskellLanguage> for HaskellHoverProvider {
    fn hover(&self, node: &RedNode<HaskellLanguage>, _range: Range<usize>) -> Option<Hover> {
        let kind = node.green.kind;
        // Provide context-aware hover information
        let contents = match kind {
            HaskellElementType::Function => "### Haskell Function\nDefines a transformation from inputs to outputs.",
            HaskellElementType::DataDeclaration => "### Haskell Data Type\nDefines a new algebraic data type.",
            HaskellElementType::ModuleDeclaration => "### Haskell Module\nOrganizes Haskell code into namespaces.",
            _ => return None,
        };
        Some(Hover { contents: contents.to_string(), range: Some(node.span()) })
    }
}
/// Language service implementation for Haskell.
pub struct HaskellLanguageService<V: Vfs> {
    /// The virtual file system.
    vfs: V,
    /// The workspace manager.
    workspace: language_server::workspace::WorkspaceManager,
    /// The hover provider.
    hover_provider: HaskellHoverProvider,
}
impl<V: Vfs> HaskellLanguageService<V> {
    /// Creates a new `HaskellLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: HaskellHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for HaskellLanguageService<V> {
    type Lang = HaskellLanguage;
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
            let language = HaskellLanguage::default();
            let parser = data_text_haskell::parser::HaskellParser::new(&language);
            let lexer = data_text_haskell::lexer::HaskellLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<HaskellLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
