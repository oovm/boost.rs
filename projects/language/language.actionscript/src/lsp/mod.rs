#![doc = include_str!("readme.md")]
pub mod formatter;
pub mod highlighter;

use data_text_actionscript::{ActionScriptLanguage, parser::ActionScriptElementType};
use core::range::Range;
use data_text_core::tree::RedNode;

use {
    futures::Future,
    language_hover::{Hover, HoverProvider},
    language_server::Vfs,
    language_server::service::LanguageService,
};

/// Hover provider implementation for ActionScript.
pub struct ActionScriptHoverProvider;

impl HoverProvider<ActionScriptLanguage> for ActionScriptHoverProvider {
    fn hover(&self, node: &RedNode<ActionScriptLanguage>, _range: Range<usize>) -> Option<Hover> {
        let kind = node.green.kind;

        let contents = match kind {
            ActionScriptElementType::Class => "### ActionScript Class\nDefines a blueprint for objects.",
            ActionScriptElementType::Interface => "### ActionScript Interface\nDefines a contract for classes.",
            ActionScriptElementType::Function => "### ActionScript Function\nDefines a block of code that performs a task.",
            _ => return None,
        };

        Some(Hover { contents: contents.to_string(), range: Some(node.span()) })
    }
}

/// Language service implementation for ActionScript.
pub struct ActionScriptLanguageService<V: Vfs> {
    vfs: V,
    workspace: language_server::workspace::WorkspaceManager,
    hover_provider: ActionScriptHoverProvider,
}

impl<V: Vfs> ActionScriptLanguageService<V> {
    /// Creates a new `ActionScriptLanguageService`.
    pub fn new(vfs: V) -> Self {
        Self { vfs, workspace: language_server::workspace::WorkspaceManager::default(), hover_provider: ActionScriptHoverProvider }
    }
}

impl<V: Vfs + Send + Sync + 'static + language_server::DocumentSession> LanguageService for ActionScriptLanguageService<V> {
    type Lang = ActionScriptLanguage;
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
            let language = ActionScriptLanguage::default();
            let parser = data_text_actionscript::parser::ActionScriptParser::new(&language);
            let lexer = data_text_actionscript::lexer::ActionScriptLexer::new(&language);
            let mut cache = data_text_core::parser::session::ParseSession::<Self::Lang>::default();
            let parse_out = data_text_core::parser::parse(&parser, &lexer, &source, &[], &mut cache);
            let green = parse_out.result.ok()?;
            Some(f(RedNode::new(green, 0)))
        }
    }

    fn hover_provider(&self) -> Option<&(dyn HoverProvider<ActionScriptLanguage> + Send + Sync)> {
        Some(&self.hover_provider)
    }

    fn completion(&self, _uri: &str, _offset: usize) -> impl Future<Output = Vec<language_server::types::CompletionItem>> + Send + '_ {
        async move { vec![] }
    }
}
