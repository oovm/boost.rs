#![doc = include_str!("readme.md")]
/// Element types for the TOML parser.
pub mod element_type;
use crate::{TomlBuilder, ast::TomlRoot, language::TomlLanguage, lexer::token_type::TomlTokenKind as TomlSyntaxKind};
use data_text_core::{Builder, Parser, ParserState, Source, TextError, parser::session::ParseSession, source::SourceText};

pub(crate) type State<'a, S> = ParserState<'a, TomlLanguage, S>;

/// TOML language parser.
///
/// Implements the `Parser` trait for the TOML language, building a structured
/// green tree out of the token stream produced by [`TomlLexer`](crate::lexer::TomlLexer).
pub struct TomlParser<'config> {
    /// Language configuration.
    pub(crate) config: &'config TomlLanguage,
}

impl<'config> TomlParser<'config> {
    /// Creates a new `TomlParser` with the given configuration.
    pub fn new(config: &'config TomlLanguage) -> Self {
        Self { config }
    }

    /// Parses a document root: a sequence of key-value pairs, tables, and arrays of tables.
    fn parse_root<'a, S: Source + ?Sized>(&self, state: &mut State<'a, S>) -> Result<(), TextError> {
        while state.not_at_end() && !state.at(TomlSyntaxKind::Eof) {
            match state.peek_kind() {
                Some(TomlSyntaxKind::LeftBracket) => {
                    let _ = self.parse_table(state);
                }
                Some(TomlSyntaxKind::DoubleLeftBracket) => {
                    let _ = self.parse_array_of_tables(state);
                }
                Some(TomlSyntaxKind::BareKey | TomlSyntaxKind::BasicString | TomlSyntaxKind::LiteralString) => {
                    let checkpoint = state.checkpoint();
                    if self.parse_key_value(state).is_err() && state.checkpoint() == checkpoint {
                        state.bump();
                    }
                }
                _ => {
                    state.record_unexpected_token(format!("{:?}", state.peek_kind()));
                    state.bump();
                }
            }
        }
        Ok(())
    }

    /// Parses a dotted key such as `a.b."c d"`.
    fn parse_key<'a, S: Source + ?Sized>(&self, state: &mut State<'a, S>) -> Result<(), TextError> {
        state.incremental_node(TomlSyntaxKind::Key, |state| {
            self.parse_key_segment(state)?;
            while state.at(TomlSyntaxKind::Dot) {
                state.bump();
                self.parse_key_segment(state)?;
            }
            Ok(())
        })
    }

    /// Parses a single key segment (bare key or quoted key).
    fn parse_key_segment<'a, S: Source + ?Sized>(&self, state: &mut State<'a, S>) -> Result<(), TextError> {
        match state.peek_kind() {
            Some(TomlSyntaxKind::BareKey | TomlSyntaxKind::BasicString | TomlSyntaxKind::LiteralString) => {
                state.bump();
                Ok(())
            }
            _ => {
                state.record_expected("key");
                Err(state.errors.last().cloned().expect("Error should have been recorded"))
            }
        }
    }

    /// Parses `key = value`.
    fn parse_key_value<'a, S: Source + ?Sized>(&self, state: &mut State<'a, S>) -> Result<(), TextError> {
        state.incremental_node(TomlSyntaxKind::KeyValue, |state| {
            self.parse_key(state)?;
            if !state.eat(TomlSyntaxKind::Equal) {
                state.record_expected("=");
            }
            self.parse_value(state)
        })
    }

    /// Parses a value: scalar, array, or inline table.
    fn parse_value<'a, S: Source + ?Sized>(&self, state: &mut State<'a, S>) -> Result<(), TextError> {
        match state.peek_kind() {
            Some(
                TomlSyntaxKind::BasicString
                | TomlSyntaxKind::LiteralString
                | TomlSyntaxKind::Integer
                | TomlSyntaxKind::Float
                | TomlSyntaxKind::Boolean
                | TomlSyntaxKind::OffsetDateTime
                | TomlSyntaxKind::LocalDateTime
                | TomlSyntaxKind::LocalDate
                | TomlSyntaxKind::LocalTime,
            ) => {
                state.bump();
                Ok(())
            }
            Some(TomlSyntaxKind::LeftBracket) => self.parse_array(state),
            Some(TomlSyntaxKind::LeftBrace) => self.parse_inline_table(state),
            other => {
                state.record_unexpected_token(format!("{other:?}"));
                Err(state.errors.last().cloned().expect("Error should have been recorded"))
            }
        }
    }

    /// Parses an array value `[1, 2, 3]`.
    fn parse_array<'a, S: Source + ?Sized>(&self, state: &mut State<'a, S>) -> Result<(), TextError> {
        state.incremental_node(TomlSyntaxKind::Array, |state| {
            if !state.eat(TomlSyntaxKind::LeftBracket) {
                state.record_expected("[");
                return Ok(());
            }

            while state.not_at_end() && !state.at(TomlSyntaxKind::Eof) {
                if state.at(TomlSyntaxKind::RightBracket) {
                    break;
                }

                self.parse_value(state)?;

                if state.eat(TomlSyntaxKind::Comma) {
                    continue;
                }
                break;
            }

            if !state.eat(TomlSyntaxKind::RightBracket) {
                state.record_expected("]");
            }
            Ok(())
        })
    }

    /// Parses an inline table `{ a = 1, b = 2 }`.
    fn parse_inline_table<'a, S: Source + ?Sized>(&self, state: &mut State<'a, S>) -> Result<(), TextError> {
        state.incremental_node(TomlSyntaxKind::InlineTable, |state| {
            if !state.eat(TomlSyntaxKind::LeftBrace) {
                state.record_expected("{");
                return Ok(());
            }

            while state.not_at_end() && !state.at(TomlSyntaxKind::Eof) {
                if state.at(TomlSyntaxKind::RightBrace) {
                    break;
                }

                self.parse_key_value(state)?;

                if state.eat(TomlSyntaxKind::Comma) {
                    continue;
                }
                break;
            }

            if !state.eat(TomlSyntaxKind::RightBrace) {
                state.record_expected("}");
            }
            Ok(())
        })
    }

    /// Parses `[table]` and the key-value pairs that belong to it.
    fn parse_table<'a, S: Source + ?Sized>(&self, state: &mut State<'a, S>) -> Result<(), TextError> {
        state.incremental_node(TomlSyntaxKind::Table, |state| {
            if !state.eat(TomlSyntaxKind::LeftBracket) {
                state.record_expected("[");
                return Ok(());
            }
            self.parse_key(state)?;
            if !state.eat(TomlSyntaxKind::RightBracket) {
                state.record_expected("]");
            }
            self.parse_table_items(state);
            Ok(())
        })
    }

    /// Parses `[[table]]` and the key-value pairs that belong to it.
    fn parse_array_of_tables<'a, S: Source + ?Sized>(&self, state: &mut State<'a, S>) -> Result<(), TextError> {
        state.incremental_node(TomlSyntaxKind::ArrayOfTables, |state| {
            if !state.eat(TomlSyntaxKind::DoubleLeftBracket) {
                state.record_expected("[[");
                return Ok(());
            }
            self.parse_key(state)?;
            if !state.eat(TomlSyntaxKind::DoubleRightBracket) {
                state.record_expected("]]");
            }
            self.parse_table_items(state);
            Ok(())
        })
    }

    /// Parses key-value pairs until the next table header or the end of the document.
    fn parse_table_items<'a, S: Source + ?Sized>(&self, state: &mut State<'a, S>) {
        while state.not_at_end() && !state.at(TomlSyntaxKind::Eof) {
            match state.peek_kind() {
                Some(TomlSyntaxKind::LeftBracket | TomlSyntaxKind::DoubleLeftBracket) => break,
                Some(TomlSyntaxKind::BareKey | TomlSyntaxKind::BasicString | TomlSyntaxKind::LiteralString) => {
                    if self.parse_key_value(state).is_err() {
                        // Avoid an infinite loop when the item cannot be parsed.
                        state.bump();
                    }
                }
                _ => {
                    state.record_unexpected_token(format!("{:?}", state.peek_kind()));
                    state.bump();
                }
            }
        }
    }
}

impl<'config> Parser<TomlLanguage> for TomlParser<'config> {
    fn parse<'a, S: Source + ?Sized>(
        &self,
        text: &'a S,
        edits: &[data_text_core::TextEdit],
        cache: &'a mut impl data_text_core::ParseCache<TomlLanguage>,
    ) -> data_text_core::ParseOutput<'a, TomlLanguage> {
        let lexer = crate::lexer::TomlLexer::new(&self.config);
        data_text_core::parser::parse_with_lexer(&lexer, text, edits, cache, |state| {
            let checkpoint = state.checkpoint();
            self.parse_root(state)?;
            Ok(state.finish_at(checkpoint, TomlSyntaxKind::Root.into()))
        })
    }
}

/// Parses a TOML string.
pub fn parse(toml: &str) -> Result<TomlRoot, TextError> {
    let language = TomlLanguage::default();
    parse_with_config(toml, &language)
}

/// Parses a TOML string.
pub fn parse_with_config(toml: &str, config: &TomlLanguage) -> Result<TomlRoot, TextError> {
    let builder = TomlBuilder::new(&config);
    let source = SourceText::new(toml);
    let mut cache = ParseSession::default();
    let result = builder.build(&source, &[], &mut cache);
    result.result
}
