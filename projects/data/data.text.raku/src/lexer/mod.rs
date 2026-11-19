/// Token types and definitions for Raku.
pub mod token_type;

use crate::language::RakuLanguage;
use data_text_core::{
    lexer::{LexOutput, Lexer, LexerCache, LexerState},
    source::Source,
};

/// Lexer for Raku.
pub struct RakuLexer {}

impl RakuLexer {
    /// Creates a new `RakuLexer`.
    pub fn new() -> Self {
        Self {}
    }
}

impl Lexer<RakuLanguage> for RakuLexer {
    fn lex<'a, S: Source + ?Sized>(
        &self,
        source: &'a S,
        _edits: &[data_text_core::source::TextEdit],
        cache: &'a mut impl LexerCache<RakuLanguage>,
    ) -> LexOutput<RakuLanguage> {
        let mut state = LexerState::new(source);
        // Minimal implementation for now
        state.add_eof();
        state.finish_with_cache(Ok(()), cache)
    }
}
