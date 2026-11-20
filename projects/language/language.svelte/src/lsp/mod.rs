#![doc = include_str!("readme.md")]
use data_text_svelte::language::SvelteLanguage;
use core::range::Range;
use data_text_core::tree::RedNode;
use {futures::Future, language_server::Vfs, language_server::service::LanguageService, language_server::types::Hover as LspHover};

/// Language service for the Svelte framework.
pub struct SvelteLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
}

impl<V: Vfs> SvelteLanguageService<V> {
    /// Creates a new Svelte language service.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::new() }
    }
}

impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for SvelteLanguageService<V> {
    type Lang = SvelteLanguage;
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
            let language = SvelteLanguage::default();
            let parser = data_text_svelte::parser::SvelteParser::new(&language);
            let lexer = data_text_svelte::lexer::SvelteLexer::new(&language);
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
