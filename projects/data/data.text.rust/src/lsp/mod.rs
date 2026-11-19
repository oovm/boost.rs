#![doc = include_str!("readme.md")]
#[cfg(feature = "oak-highlight")]
pub mod highlighter;

#[cfg(feature = "lsp")]
use {
    futures::{Future, FutureExt},
    language_hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
};
#[cfg(feature = "oak-pretty-print")]
pub mod formatter;
use crate::{RustLanguage, parser::RustElementType};
use core::range::Range;
use dashmap::DashMap;
use data_text_core::{
    GreenNode, Source,
    language::{ElementType, TokenType},
    parser::{ParseCache, Parser, session::ParseSession},
    tree::RedNode,
};
/// Hover provider implementation for Rust.
#[cfg(feature = "lsp")]
pub struct RustHoverProvider;
#[cfg(feature = "lsp")]
impl HoverProvider<RustLanguage> for RustHoverProvider {
    fn hover(&self, node: &RedNode<RustLanguage>, _range: Range<usize>) -> Option<Hover> {
        let kind = node.green.kind;
        // Provide context-aware hover information
        let contents = match kind {
            RustElementType::Function => "### Rust Function\nDefines a callable block of code.",
            RustElementType::StructItem => "### Rust Struct\nDefines a custom data type.",
            RustElementType::ModuleItem => "### Rust Module\nOrganizes code into namespaces.",
            RustElementType::Trait => "### Rust Trait\nDefines a shared behavior.",
            _ => return None,
        };
        Some(Hover { contents: contents.to_string(), range: Some(node.span()) })
    }
}
/// Language service implementation for Rust.
#[cfg(feature = "lsp")]
pub struct RustLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: RustHoverProvider,
    sessions: DashMap<String, Box<ParseSession<RustLanguage>>>,
}
impl<V: Vfs> RustLanguageService<V> {
    /// Creates a new `RustLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: RustHoverProvider, sessions: DashMap::new() }
    }
    /// Collects definitions of a name starting from the given node.
    ///
    /// This method performs a recursive search through the syntax tree to find nodes
    /// that represent definitions (e.g., functions, structs, traits) and checks if
    /// their identifier matches the provided name.
    ///
    /// # Arguments
    ///
    /// * `node` - The root node to start the search from.
    /// * `name` - The name of the symbol to look for.
    /// * `source` - The source text of the file.
    /// * `uri` - The URI of the file being searched.
    /// * `definitions` - A mutable vector to store the found locations.
    fn collect_definitions<S: Source + ?Sized>(&self, node: &RedNode<RustLanguage>, name: &str, source: &S, uri: &str, definitions: &mut Vec<language_server::LocationRange>) {
        use data_text_core::{
            language::{ElementRole, UniversalElementRole},
            tree::RedTree,
        };
        let role = node.green.kind.role();
        if role.universal() == UniversalElementRole::Definition {
            for child in node.children() {
                if let RedTree::Leaf(leaf) = child {
                    if leaf.kind.is_universal(data_text_core::language::UniversalTokenRole::Name) {
                        let leaf_name = source.get_text_in(leaf.span.clone());
                        if leaf_name.as_ref() == name {
                            definitions.push(language_server::LocationRange { uri: uri.to_string().into(), range: leaf.span });
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
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for RustLanguageService<V> {
    type Lang = RustLanguage;
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
            let language = RustLanguage::default();
            let parser = crate::parser::RustParser::new(&language);
            let lexer = crate::lexer::RustLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn definition<'a>(&'a self, uri: &'a str, range: Range<usize>) -> impl Future<Output = Vec<language_server::LocationRange>> + Send + 'a {
        let uri_clone = uri.to_string();
        async move {
            let uri_clone_for_root = uri_clone.clone();
            self.with_root(&uri_clone_for_root, move |root| {
                let uri_clone = uri_clone.clone();
                let source = match self.vfs().request_source(&uri_clone) {
                    Some(s) => s,
                    None => return vec![],
                };

                // In a real implementation this would collect definitions across files using with_roots
                vec![]
            })
            .await
            .unwrap_or_default()
        }
    }
    fn references<'a>(&'a self, uri: &'a str, range: Range<usize>) -> impl Future<Output = Vec<language_server::LocationRange>> + Send + 'a {
        let uri_clone = uri.to_string();
        async move {
            let uri_clone_for_root = uri_clone.clone();
            self.with_root(&uri_clone_for_root, move |root| {
                let uri_clone = uri_clone.clone();
                let source = match self.vfs().request_source(&uri_clone) {
                    Some(s) => s,
                    None => return vec![],
                };
                vec![]
            })
            .await
            .unwrap_or_default()
        }
    }
    fn rename<'a>(&'a self, uri: &'a str, range: Range<usize>, new_name: String) -> impl Future<Output = Option<language_server::WorkspaceEdit>> + Send + 'a {
        let uri = uri.to_string();
        async move {
            let refs = self.references(&uri, range).await;
            if refs.is_empty() {
                return None;
            }
            let mut changes = std::collections::HashMap::new();
            for r in refs {
                changes.entry(r.uri.to_string()).or_insert_with(Vec::new).push(language_server::TextEdit { range: r.range, new_text: new_name.clone() });
            }
            Some(language_server::WorkspaceEdit { changes })
        }
    }
    fn hover(&self, uri: &str, range: Range<usize>) -> impl Future<Output = Option<language_server::Hover>> + Send + '_ {
        let uri = uri.to_string();
        async move {
            self.with_root(&uri, |root| {
                // In a real implementation, you would find the specific node at offset
                // For this example, we just check the root or simple children
                self.hover_provider.hover(&root, range).map(|h| language_server::Hover { contents: h.contents, range: h.range })
            })
            .await
            .flatten()
        }
    }
}
