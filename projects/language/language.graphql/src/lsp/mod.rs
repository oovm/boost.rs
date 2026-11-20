#![doc = include_str!("readme.md")]
use data_text_graphql::language::GraphQLLanguage;
use core::range::Range;
use data_text_core::tree::RedNode;
use {futures::Future, language_server::Vfs, language_server::service::LanguageService, language_server::types::Hover as LspHover};

/// LSP language service for GraphQL.
pub struct GraphqlLanguageService<V: Vfs> {
    /// The virtual file system.
    pub vfs: V,
    /// The workspace manager.
    pub workspace: language_server::workspace::WorkspaceManager,
}
impl<V: Vfs> GraphqlLanguageService<V> {
    /// Creates a new GraphQL language service.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::new() }
    }
}
impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for GraphqlLanguageService<V> {
    type Lang = GraphQLLanguage;
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
            let language = GraphQLLanguage::default();
            let parser = data_text_graphql::parser::GraphQLParser::new(&language);
            let lexer = data_text_graphql::lexer::GraphQLLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }
    fn hover(&self, _uri: &str, _range: Range<usize>) -> impl Future<Output = Option<LspHover>> + Send + '_ {
        async move { None }
    }
}
