#![doc = include_str!("readme.md")]

use core::range::Range;
use data_text_core::{
    Builder, Source,
    parser::session::ParseSession,
    tree::{RedNode, RedTree},
};
use data_text_typescript::{
    TypeScriptBuilder, TypeScriptLanguage, TypeScriptLexer, TypeScriptParser,
    parser::element_type::TypeScriptElementType,
    scope::{ModuleBindingFacts, module_scope_facts},
};
use futures::Future;
use language_server::{
    Vfs,
    hover::{Hover as HoverInfo, HoverProvider},
    service::LanguageService,
    types::Hover as LspHover,
};

/// Hover provider for TypeScript declarations.
pub struct TypescriptHoverProvider;

impl TypescriptHoverProvider {
    /// Returns the documentation markdown for a declaration kind, if any.
    pub fn describe(kind: TypeScriptElementType) -> Option<&'static str> {
        let contents = match kind {
            TypeScriptElementType::FunctionDeclaration => "### TypeScript Function\nDeclares a callable function.",
            TypeScriptElementType::ClassDeclaration => "### TypeScript Class\nDeclares a class with fields and methods.",
            TypeScriptElementType::InterfaceDeclaration => "### TypeScript Interface\nDeclares a structural contract for values.",
            TypeScriptElementType::TypeAliasDeclaration => "### TypeScript Type Alias\nIntroduces a name for another type.",
            TypeScriptElementType::EnumDeclaration => "### TypeScript Enum\nDeclares a named set of constants.",
            TypeScriptElementType::NamespaceDeclaration => "### TypeScript Namespace\nGroups declarations under a qualified name.",
            TypeScriptElementType::VariableDeclaration => "### TypeScript Variable\nDeclares a `let`, `const` or `var` binding.",
            TypeScriptElementType::MethodDeclaration => "### TypeScript Method\nDeclares a method on a class or object.",
            TypeScriptElementType::PropertyDeclaration => "### TypeScript Property\nDeclares a field on a class.",
            TypeScriptElementType::ConstructorDeclaration => "### TypeScript Constructor\nInitializes a class instance.",
            TypeScriptElementType::ImportDeclaration => "### TypeScript Import\nImports bindings from another module.",
            TypeScriptElementType::ExportDeclaration => "### TypeScript Export\nRe-exports or exports a module binding.",
            _ => return None,
        };
        Some(contents)
    }
}

impl HoverProvider<TypeScriptLanguage> for TypescriptHoverProvider {
    fn hover(&self, node: &RedNode<'_, TypeScriptLanguage>, _range: Range<usize>) -> Option<HoverInfo> {
        let contents = Self::describe(node.green.kind)?;
        Some(HoverInfo { contents: contents.to_string(), range: Some(node.span()) })
    }
}

fn contains(span: &Range<usize>, offset: usize) -> bool {
    span.start <= offset && (offset < span.end || (span.start == span.end && offset == span.start))
}

/// Finds the deepest described declaration that contains `offset`.
fn described_declaration(
    node: &RedNode<'_, TypeScriptLanguage>,
    offset: usize,
    found: Option<(TypeScriptElementType, Range<usize>)>,
) -> Option<(TypeScriptElementType, Range<usize>)> {
    if !contains(&node.span(), offset) {
        return None;
    }
    let found = if TypescriptHoverProvider::describe(node.green.kind).is_some() { Some((node.green.kind, node.span())) } else { found };
    for child in node.children() {
        if let RedTree::Node(child_node) = child {
            if let Some(deep) = described_declaration(&child_node, offset, found) {
                return Some(deep);
            }
        }
    }
    found
}

/// Language service for TypeScript providing hover, navigation and formatting.
pub struct TypescriptLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: TypescriptHoverProvider,
}

impl<V: Vfs + language_server::DocumentSession> TypescriptLanguageService<V> {
    /// Creates a new `TypescriptLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::new(), hover_provider: TypescriptHoverProvider }
    }

    /// Builds module-scope facts and locates the binding under `offset`.
    fn scope_binding(&self, uri: &str, offset: usize) -> Option<ModuleBindingFacts> {
        let source = self.vfs.request_source(uri)?;
        let language = TypeScriptLanguage::standard();
        let builder = TypeScriptBuilder::new(&language);
        let mut session = ParseSession::<TypeScriptLanguage>::default();
        let output = builder.build(&source, &[], &mut session);
        let root = output.result.ok()?;
        module_scope_facts(&root)
            .bindings
            .into_iter()
            .find(|binding| contains(&binding.declaration, offset) || binding.references.iter().any(|span| contains(span, offset)))
    }
}

impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for TypescriptLanguageService<V> {
    type Lang = TypeScriptLanguage;
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
            let language = TypeScriptLanguage::standard();
            let parser = TypeScriptParser::new(&language);
            let lexer = TypeScriptLexer::new(&language);
            let mut cache = ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }

    fn hover(&self, uri: &str, range: Range<usize>) -> impl Future<Output = Option<LspHover>> + Send + '_ {
        let uri = uri.to_string();
        async move {
            self.with_root(&uri, move |root| {
                let (kind, span) = described_declaration(&root, range.start, None)?;
                let contents = TypescriptHoverProvider::describe(kind)?;
                Some(LspHover { contents: contents.to_string(), range: Some(span) })
            })
            .await
            .flatten()
        }
    }

    fn definition<'a>(&'a self, uri: &'a str, range: Range<usize>) -> impl Future<Output = Vec<language_server::LocationRange>> + Send + 'a {
        let uri = uri.to_string();
        async move {
            let Some(binding) = self.scope_binding(&uri, range.start)
            else {
                return vec![];
            };
            vec![language_server::LocationRange { uri: uri.into(), range: binding.declaration }]
        }
    }

    fn references<'a>(&'a self, uri: &'a str, range: Range<usize>) -> impl Future<Output = Vec<language_server::LocationRange>> + Send + 'a {
        let uri = uri.to_string();
        async move {
            let Some(binding) = self.scope_binding(&uri, range.start)
            else {
                return vec![];
            };
            let mut spans = Vec::with_capacity(binding.references.len() + 1);
            spans.push(binding.declaration);
            spans.extend(binding.references);
            spans.sort_by_key(|span| span.start);
            spans.dedup();
            spans.into_iter().map(|range| language_server::LocationRange { uri: uri.clone().into(), range }).collect()
        }
    }

    fn formatting<'a>(&'a self, uri: &'a str) -> impl Future<Output = Vec<language_server::TextEdit>> + Send + 'a {
        let uri = uri.to_string();
        async move {
            let Some(source) = self.source_for_uri(&uri)
            else {
                return vec![];
            };
            let full = Range { start: 0, end: source.length() };
            let text = source.get_text_in(full.clone());
            let text = text.as_ref();
            let Ok(formatted) = crate::formatter::format_source(text, &crate::formatter::FormatOptions::default())
            else {
                return vec![];
            };
            if formatted == text {
                return vec![];
            }
            vec![language_server::TextEdit { range: full, new_text: formatted }]
        }
    }

    fn hover_provider(&self) -> Option<&(dyn HoverProvider<TypeScriptLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }
}
