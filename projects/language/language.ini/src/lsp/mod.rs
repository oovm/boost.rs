#![doc = include_str!("readme.md")]
pub mod highlighter;

use data_text_ini::{language::IniLanguage, parser::element_type::IniElementType};
use core::range::Range;
use data_text_core::tree::RedNode;
use {
    futures::Future,
    language_server::hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
};
/// INI hover provider.
pub struct IniHoverProvider;

impl HoverProvider<IniLanguage> for IniHoverProvider {
    fn hover(&self, node: &RedNode<IniLanguage>, _range: Range<usize>) -> Option<Hover> {
        let kind = node.green.kind;
        let contents = match kind {
            IniElementType::Table => "### INI Section\nDefines a group of properties.",
            IniElementType::KeyValue => "### INI Property\nA key-value pair.",
            _ => return None,
        };
        Some(Hover { contents: contents.to_string(), range: Some(node.span()) })
    }
}
/// INI language service.
pub struct IniLanguageService<V: Vfs> {
    /// Virtual file system.
    vfs: V,
    /// Workspace manager.
    workspace: language_server::workspace::WorkspaceManager,
    /// Hover provider.
    hover_provider: IniHoverProvider,
}
impl<V: Vfs> IniLanguageService<V> {
    /// Creates a new `IniLanguageService` with the given VFS.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: IniHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for IniLanguageService<V> {
    type Lang = IniLanguage;
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
            let language = IniLanguage::default();
            let parser = data_text_ini::parser::IniParser::new(&language);
            let lexer = data_text_ini::lexer::IniLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<IniLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
