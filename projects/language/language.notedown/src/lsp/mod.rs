#![doc = include_str!("readme.md")]
pub mod highlighter;

use data_text_notedown::{language::NotedownLanguage, lexer::token_type::NoteTokenType, parser::element_type::NoteElementType};
use core::range::Range;
use data_text_core::tree::RedNode;
use {
    futures::Future, language_hover::HoverProvider, language_server::Vfs, language_server::service::LanguageService,
    language_server::types::Hover as LspHover,
};
/// Hover provider implementation for Notedown.
pub struct NoteHoverProvider;
impl HoverProvider<NotedownLanguage> for NoteHoverProvider {
    fn hover(&self, node: &RedNode<NotedownLanguage>, _range: Range<usize>) -> Option<language_hover::Hover> {
        let kind = node.green.kind;
        let contents = match kind {
            NoteElementType::Token(t) if t == NoteTokenType::Heading1 => "### Heading 1\nTop-level heading.",
            NoteElementType::Token(t) if t == NoteTokenType::Heading2 => "### Heading 2\nSecond-level heading.",
            NoteElementType::Token(t) if t == NoteTokenType::CodeBlock => "### Code Block\nSyntax-highlighted code block.",
            NoteElementType::Token(t) if t == NoteTokenType::Table => "### Table\nMarkdown table structure.",
            _ => return None,
        };
        Some(language_hover::Hover { contents: contents.to_string(), range: Some(node.span()) })
    }
}
/// Language service implementation for Notedown.
pub struct NoteLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: NoteHoverProvider,
}
impl<V: Vfs> NoteLanguageService<V> {
    /// Create a new language service instance
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: NoteHoverProvider }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for NoteLanguageService<V> {
    type Lang = NotedownLanguage;
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
            let language = NotedownLanguage::default();
            let parser = data_text_notedown::parser::NoteParser::new(&language);
            let lexer = data_text_notedown::lexer::NotedownLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover(&self, uri: &str, range: Range<usize>) -> impl futures::Future<Output = Option<language_server::types::Hover>> + Send + '_ {
        let uri = uri.to_string();
        async move {
            self.with_root(&uri, |root| {
                self.hover_provider.hover(&root, range).map(|h| language_server::types::Hover { contents: h.contents, range: h.range })
            })
            .await
            .flatten()
        }
    }
}
