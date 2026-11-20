#![doc = include_str!("readme.md")]
pub mod highlighter;

use data_text_lua::language::LuaLanguage;
use core::range::Range;
use data_text_core::tree::RedNode;
use {
    futures::Future,
    language_hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
};
/// Hover provider implementation for Lua.
pub struct LuaHoverProvider;
impl HoverProvider<LuaLanguage> for LuaHoverProvider {
    fn hover(&self, node: &RedNode<LuaLanguage>, _range: Range<usize>) -> Option<Hover> {
        let kind = node.green.kind;
        // Provide context-aware hover information
        // This is a simplified example
        let contents = match format!("{:?}", kind).as_str() {
            "Function" => "### Lua Function\nA block of code that performs a specific task.",
            "LocalDeclaration" => "### Local Variable\nA variable with local scope.",
            _ => return None,
        };
        Some(Hover { contents: contents.to_string(), range: Some(node.span()) })
    }
}
/// Language service implementation for Lua.
pub struct LuaLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: LuaHoverProvider,
}
impl<V: Vfs> LuaLanguageService<V> {
    /// Creates a new `LuaLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: LuaHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for LuaLanguageService<V> {
    type Lang = LuaLanguage;
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
            let language = LuaLanguage::default();
            let parser = data_text_lua::parser::LuaParser::new(&language);
            let lexer = data_text_lua::lexer::LuaLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<LuaLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
