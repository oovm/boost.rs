#![doc = include_str!("readme.md")]
pub mod highlighter;

use data_text_markdown::{language::MarkdownLanguage, parser::element_type::MarkdownElementType};
use core::range::Range;
use data_text_core::{ElementRole, ElementType, Source, TokenType, tree::RedNode};
use {
    futures::Future,
    language_hover::{Hover as HoverInfo, HoverProvider},
    language_server::Vfs,
    language_server::{service::LanguageService, types::Hover as LspHover},
};
/// Hover provider for Markdown.
pub struct MarkdownHoverProvider;
impl HoverProvider<MarkdownLanguage> for MarkdownHoverProvider {
    fn hover(&self, node: &RedNode<'_, MarkdownLanguage>, _range: Range<usize>) -> Option<HoverInfo> {
        let kind = node.green.kind;
        let contents = match kind {
            MarkdownElementType::Heading1 => "### Heading 1\nTop-level heading.",
            MarkdownElementType::Heading2 => "### Heading 2\nSecond-level heading.",
            MarkdownElementType::Link => "### Markdown Link\nLink to an external resource or internal anchor.",
            MarkdownElementType::CodeBlock => "### Code Block\nA block of source code.",
            MarkdownElementType::Strong => "### Strong Emphasis\nBold text.",
            MarkdownElementType::Emphasis => "### Emphasis\nItalic text.",
            _ => return None,
        };
        Some(HoverInfo { contents: contents.to_string(), range: Some(node.span()) })
    }
}
/// Language service for Markdown providing LSP features.
pub struct MarkdownLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: MarkdownHoverProvider,
}
impl<V: Vfs> MarkdownLanguageService<V> {
    /// Creates a new MarkdownLanguageService with the given VFS.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::new(), hover_provider: MarkdownHoverProvider }
    }
    fn collect_definitions(
        &self,
        node: &RedNode<MarkdownLanguage>,
        name: &str,
        source: &str,
        uri: &str,
        definitions: &mut Vec<language_server::LocationRange>,
    ) {
        use data_text_core::{language::UniversalElementRole, tree::RedTree};
        let role = ElementType::role(&node.green.kind);
        if ElementRole::universal(&role) == UniversalElementRole::Definition {
            for child in node.children() {
                if let RedTree::Leaf(leaf) = child {
                    if TokenType::is_universal(&leaf.kind, data_text_core::language::UniversalTokenRole::Name) {
                        if &source[leaf.span.clone()] == name {
                            definitions.push(language_server::LocationRange { uri: uri.into(), range: leaf.span });
                            return;
                        }
                    }
                }
            }
        }
        for child in node.children() {
            if let RedTree::Node(child_node) = child {
                self.collect_definitions(&child_node, name, source, uri, definitions);
            }
        }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for MarkdownLanguageService<V> {
    type Lang = MarkdownLanguage;
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
            let language = MarkdownLanguage::github_flavored();
            let parser = data_text_markdown::parser::MarkdownParser::new(&language);
            let lexer = data_text_markdown::lexer::MarkdownLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn definition<'a>(&'a self, uri: &'a str, range: Range<usize>) -> impl Future<Output = Vec<language_server::LocationRange>> + Send + 'a {
        let uri = uri.to_string();
        async move {
            self.with_root(&uri, |root| {
                let source = self.vfs().request_source(&uri)?;
                let leaf = root.leaf_at_offset(range.start)?;
                if !TokenType::is_universal(&leaf.kind, data_text_core::language::UniversalTokenRole::Name) {
                    return None;
                }
                let name = source.get_text_in(leaf.span.clone());
                let mut definitions = Vec::new();
                let full_text = source.get_text_in(Range { start: 0, end: source.length() });
                self.collect_definitions(&root, &name, &full_text, &uri, &mut definitions);
                Some(definitions)
            })
            .await
            .flatten()
            .unwrap_or_default()
        }
    }
    fn references<'a>(&'a self, uri: &'a str, range: Range<usize>) -> impl Future<Output = Vec<language_server::LocationRange>> + Send + 'a {
        let uri = uri.to_string();
        async move {
            let name = self
                .with_root(&uri, |root| {
                    let source = self.vfs().request_source(&uri)?;
                    let leaf = root.leaf_at_offset(range.start)?;
                    if !TokenType::is_universal(&leaf.kind, data_text_core::language::UniversalTokenRole::Name) {
                        return None;
                    }
                    Some(source.get_text_in(leaf.span.clone()).to_string())
                })
                .await
                .flatten();
            let Some(name) = name
            else {
                return vec![];
            };
            let mut all_refs = Vec::new();
            let files = self.list_all_files(&uri).await;
            for file_uri in files {
                if let Some(refs) = self
                    .with_root(&file_uri, |file_root| {
                        let mut refs = Vec::new();
                        if let Some(file_source) = self.vfs().request_source(&file_uri) {
                            let full_text = file_source.get_text_in(Range { start: 0, end: file_source.length() });
                            let found = language_navigation::SimpleReferenceFinder::find(&file_root, &name, &full_text, file_uri.clone());
                            refs.extend(found.into_iter().map(|l| language_server::LocationRange { uri: l.uri, range: l.range }));
                        }
                        refs
                    })
                    .await
                {
                    all_refs.extend(refs);
                }
            }
            all_refs
        }
    }
    fn rename<'a>(
        &'a self,
        uri: &'a str,
        range: Range<usize>,
        new_name: String,
    ) -> impl Future<Output = Option<language_server::WorkspaceEdit>> + Send + 'a {
        let uri = uri.to_string();
        async move {
            let refs = self.references(&uri, range).await;
            if refs.is_empty() {
                return None;
            }
            let mut changes = std::collections::HashMap::new();
            for r in refs {
                changes
                    .entry(r.uri.to_string())
                    .or_insert_with(Vec::new)
                    .push(language_server::TextEdit { range: r.range, new_text: new_name.clone() });
            }
            Some(language_server::WorkspaceEdit { changes })
        }
    }
    fn hover_provider(&self) -> Option<&(dyn HoverProvider<MarkdownLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
