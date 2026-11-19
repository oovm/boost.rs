//! Tree-based syntax highlighting: theme resolution and red-tree traversal.
//!
//! Absorbed from the former `oak-highlight` crate (MPL-2.0) and de-branded.
//! The host-facing dispatch contract lives in [`crate::highlighter`]; this
//! module owns the document-tree pass that turns a parsed red tree into styled
//! segments.

use std::borrow::Cow;
use std::collections::HashMap;

use data_text_core::Range;
use data_text_core::errors::ParseResult;
use data_text_core::language::{ElementRole, Language, LanguageCategory, TokenRole, UniversalElementRole, UniversalTokenRole};
use data_text_core::parser::Parser;
use data_text_core::parser::session::ParseSession;
use data_text_core::source::SourceText;
use data_text_core::tree::{RedLeaf, RedNode, RedTree};
use data_text_core::visitor::Visitor;
use data_text_core::{Lexer, TokenType};

/// Highlight style configuration for a visual text segment.
#[derive(Debug, Clone, PartialEq)]
pub struct HighlightStyle {
    /// Foreground text color in hex format (e.g. `#FF0000`).
    pub color: Option<String>,
    /// Background color in hex format.
    pub background_color: Option<String>,
    /// Whether the text should be bold.
    pub bold: bool,
    /// Whether the text should be italic.
    pub italic: bool,
    /// Whether the text should be underlined.
    pub underline: bool,
}

impl Default for HighlightStyle {
    fn default() -> Self {
        Self { color: None, background_color: None, bold: false, italic: false, underline: false }
    }
}

/// Theme configuration keyed by dot-separated TextMate-like scopes.
#[derive(Debug, Clone)]
pub struct HighlightTheme {
    /// Theme name identifier.
    pub name: String,
    /// Style per scope, for example `keyword.control.rust`.
    pub styles: HashMap<String, HighlightStyle>,
}

impl Default for HighlightTheme {
    fn default() -> Self {
        let mut styles = HashMap::new();

        styles.insert("keyword".to_string(), HighlightStyle { color: Some("#0000FF".to_string()), bold: true, ..Default::default() });
        styles.insert("keyword.operator".to_string(), HighlightStyle { color: Some("#800080".to_string()), ..Default::default() });
        styles.insert("variable.other".to_string(), HighlightStyle { color: Some("#001080".to_string()), ..Default::default() });
        styles.insert("constant".to_string(), HighlightStyle { color: Some("#098658".to_string()), ..Default::default() });
        styles.insert("constant.character.escape".to_string(), HighlightStyle { color: Some("#FF6600".to_string()), ..Default::default() });
        styles.insert("punctuation".to_string(), HighlightStyle { color: Some("#000080".to_string()), ..Default::default() });
        styles.insert("comment".to_string(), HighlightStyle { color: Some("#808080".to_string()), italic: true, ..Default::default() });
        styles.insert("punctuation.whitespace".to_string(), HighlightStyle::default());

        styles.insert("entity.name.function".to_string(), HighlightStyle { color: Some("#795E26".to_string()), bold: true, ..Default::default() });
        styles.insert("entity.name.type".to_string(), HighlightStyle { color: Some("#267F99".to_string()), ..Default::default() });
        styles.insert("variable.other.declaration".to_string(), HighlightStyle { color: Some("#795E26".to_string()), ..Default::default() });
        styles.insert(
            "comment.block.documentation".to_string(),
            HighlightStyle { color: Some("#008000".to_string()), italic: true, ..Default::default() },
        );
        styles.insert("meta.preprocessor".to_string(), HighlightStyle { color: Some("#AF00DB".to_string()), ..Default::default() });
        styles.insert("entity.other.attribute-name".to_string(), HighlightStyle { color: Some("#AF00DB".to_string()), ..Default::default() });
        styles.insert("entity.other.attribute-name.key".to_string(), HighlightStyle { color: Some("#001080".to_string()), ..Default::default() });

        styles.insert(
            "invalid".to_string(),
            HighlightStyle { color: Some("#FF0000".to_string()), background_color: Some("#FFCCCC".to_string()), ..Default::default() },
        );
        styles.insert("none".to_string(), HighlightStyle::default());

        Self { name: "default".to_string(), styles }
    }
}

impl HighlightTheme {
    /// Resolves the style for a scope, falling back through parent scopes.
    pub fn resolve_style(&self, scope: &str) -> HighlightStyle {
        let mut current_scope = scope;
        while !current_scope.is_empty() {
            if let Some(style) = self.styles.get(current_scope) {
                return style.clone();
            }
            if let Some(position) = current_scope.rfind('.') {
                current_scope = &current_scope[..position];
            }
            else {
                break;
            }
        }
        self.styles.get("none").cloned().unwrap_or_default()
    }

    /// Resolves the most specific style across several scopes.
    pub fn resolve_styles(&self, scopes: &[String]) -> HighlightStyle {
        let mut best_style = None;
        let mut best_depth = -1;

        for scope in scopes {
            let mut current_scope = scope.as_str();
            let mut depth = current_scope.split('.').count() as i32;

            while !current_scope.is_empty() {
                if let Some(style) = self.styles.get(current_scope) {
                    if depth > best_depth {
                        best_depth = depth;
                        best_style = Some(style.clone());
                    }
                    break;
                }
                if let Some(position) = current_scope.rfind('.') {
                    current_scope = &current_scope[..position];
                    depth -= 1;
                }
                else {
                    break;
                }
            }
        }

        best_style.unwrap_or_else(|| self.styles.get("none").cloned().unwrap_or_default())
    }

    /// Gets the style for a universal token role.
    pub fn get_token_style(&self, role: UniversalTokenRole) -> HighlightStyle {
        self.resolve_style(role.name())
    }

    /// Gets the style for a universal element role.
    pub fn get_element_style(&self, role: UniversalElementRole) -> HighlightStyle {
        self.resolve_style(role.name())
    }
}

fn category_prefix(category: LanguageCategory) -> &'static str {
    match category {
        LanguageCategory::Markup => "markup",
        LanguageCategory::Config => "config",
        LanguageCategory::Programming => "source",
        LanguageCategory::Dsl => "dsl",
        _ => "source",
    }
}

/// Builds the candidate scopes for a token role.
fn token_scopes<R: TokenRole>(role: R, language: &str, category: LanguageCategory) -> Vec<String> {
    let specific_name = role.name();
    let universal = role.universal();
    let universal_name = universal.name();
    let prefix = category_prefix(category);

    let mut scopes = Vec::with_capacity(5);
    scopes.push(format!("{}.{}", specific_name, language));
    if specific_name != universal_name {
        scopes.push(specific_name.to_string());
    }
    scopes.push(format!("{}.{}", prefix, universal_name));
    scopes.push(universal_name.to_string());
    scopes.push(format!("{}.{}", prefix, language));
    scopes
}

/// Builds the candidate scopes for an element role.
fn element_scopes<R: ElementRole>(role: R, language: &str, category: LanguageCategory) -> Vec<String> {
    let specific_name = role.name();
    let universal = role.universal();
    let universal_name = universal.name();
    let prefix = category_prefix(category);

    let mut scopes = Vec::with_capacity(5);
    scopes.push(format!("{}.{}", specific_name, language));
    if specific_name != universal_name {
        scopes.push(specific_name.to_string());
    }
    scopes.push(format!("{}.{}", prefix, universal_name));
    scopes.push(universal_name.to_string());
    scopes.push(format!("{}.{}", prefix, language));
    scopes
}

/// Provides the scopes associated with a token or element role.
pub trait ScopeProvider {
    /// Returns the scopes for a language and category.
    fn scopes(&self, language: &str, category: LanguageCategory) -> Vec<String>;
}

impl ScopeProvider for UniversalTokenRole {
    fn scopes(&self, language: &str, category: LanguageCategory) -> Vec<String> {
        token_scopes(*self, language, category)
    }
}

impl ScopeProvider for UniversalElementRole {
    fn scopes(&self, language: &str, category: LanguageCategory) -> Vec<String> {
        element_scopes(*self, language, category)
    }
}

/// A byte range in the source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightSpan {
    /// Starting byte offset.
    pub start: usize,
    /// Ending byte offset.
    pub end: usize,
}

impl From<Range<usize>> for HighlightSpan {
    fn from(range: Range<usize>) -> Self {
        Self { start: range.start, end: range.end }
    }
}

/// A contiguous source segment with one resolved style.
#[derive(Debug, Clone)]
pub struct HighlightSegment<'a> {
    /// Byte range covered by this segment.
    pub span: HighlightSpan,
    /// Resolved visual style.
    pub style: HighlightStyle,
    /// Source text of the segment.
    pub text: Cow<'a, str>,
}

/// Styled segments produced by a highlighting pass.
#[derive(Debug, Clone)]
pub struct HighlightResult<'a> {
    /// Styled segments in source order.
    pub segments: Vec<HighlightSegment<'a>>,
    /// Original source text.
    pub source: Cow<'a, str>,
}

/// Visitor that turns red-tree tokens into styled segments.
pub struct HighlightVisitor<'a, 'theme> {
    /// Theme used for style resolution.
    pub theme: &'theme HighlightTheme,
    /// Segments collected during traversal.
    pub segments: Vec<HighlightSegment<'a>>,
    /// Source text.
    pub source: &'a str,
}

impl<'a, 'theme, 'tree, L: Language> Visitor<'tree, L> for HighlightVisitor<'a, 'theme> {
    fn visit_node(&mut self, node: RedNode<'tree, L>) {
        for child in node.children() {
            match child {
                RedTree::Node(node) => self.visit_node(node),
                RedTree::Leaf(token) => self.visit_token(token),
            }
        }
    }

    fn visit_token(&mut self, token: RedLeaf<L>) {
        let scopes = token_scopes(token.kind.role(), L::NAME, L::CATEGORY);
        let style = self.theme.resolve_styles(&scopes);
        let text = &self.source[token.span.start..token.span.end];
        self.segments.push(HighlightSegment {
            span: HighlightSpan { start: token.span.start, end: token.span.end },
            style,
            text: Cow::Borrowed(text),
        });
    }
}

/// Document-tree highlighting contract.
pub trait HighlightSource {
    /// Highlights source text for a language and theme.
    fn highlight<'a>(&self, source: &'a str, language: &str, theme: crate::themes::Theme) -> ParseResult<HighlightResult<'a>>;
}

/// Coordinates theme resolution and document-tree highlighting.
pub struct TreeHighlighter {
    /// Theme used when no per-call theme is supplied.
    pub theme: HighlightTheme,
}

impl Default for TreeHighlighter {
    fn default() -> Self {
        Self { theme: HighlightTheme::default() }
    }
}

impl TreeHighlighter {
    /// Creates a highlighter with the default theme.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces the stored theme.
    pub fn with_theme(mut self, theme: HighlightTheme) -> Self {
        self.theme = theme;
        self
    }

    /// Selects one of the predefined themes.
    pub fn theme(mut self, theme: crate::themes::Theme) -> Self {
        self.theme = theme.get_theme();
        self
    }

    /// Highlights the whole source as one unstyled segment.
    pub fn highlight<'a>(&self, source: &'a str, _language: &str, theme: crate::themes::Theme) -> ParseResult<HighlightResult<'a>> {
        let theme_config = theme.get_theme();
        let segments = vec![HighlightSegment {
            span: Range { start: 0, end: source.len() }.into(),
            style: theme_config.resolve_style("none"),
            text: Cow::Borrowed(source),
        }];
        Ok(HighlightResult { segments, source: Cow::Borrowed(source) })
    }

    /// Highlights by parsing the source with a language parser and lexer.
    pub fn highlight_with_language<'a, L, P, LX>(
        &self,
        source: &'a str,
        theme: crate::themes::Theme,
        parser: &P,
        lexer: &LX,
    ) -> ParseResult<HighlightResult<'a>>
    where
        L: Language + Send + Sync + 'static,
        P: Parser<L>,
        LX: Lexer<L>,
        L::ElementType: From<L::TokenType>,
    {
        let theme_config = theme.get_theme();
        let source_text = SourceText::new(source);
        let mut cache = ParseSession::<L>::default();
        let parse_result = data_text_core::parser::parse(parser, lexer, &source_text, &[], &mut cache);
        let mut visitor = HighlightVisitor { theme: &theme_config, segments: Vec::new(), source };
        let root_node = parse_result.result?;
        visitor.visit_node(RedNode::new(root_node, 0));
        Ok(HighlightResult { segments: visitor.segments, source: Cow::Borrowed(source) })
    }
}

impl HighlightSource for TreeHighlighter {
    fn highlight<'a>(&self, source: &'a str, language: &str, theme: crate::themes::Theme) -> ParseResult<HighlightResult<'a>> {
        TreeHighlighter::highlight(self, source, language, theme)
    }
}
