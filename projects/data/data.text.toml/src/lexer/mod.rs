#![doc = include_str!("readme.md")]
/// Token types for the TOML language.
pub mod token_type;
pub use crate::lexer::token_type::TomlTokenType;

use crate::{language::TomlLanguage, lexer::token_type::TomlTokenKind as TomlSyntaxKind};
use data_text_core::{
    Lexer, LexerState, TextEdit, TextError,
    lexer::{LexOutput, LexerCache},
    source::Source,
};

pub(crate) type State<'a, S> = LexerState<'a, S, TomlLanguage>;

/// TOML lexer implementation.
///
/// This struct implements the `Lexer` trait for the TOML language,
/// converting source text into a stream of tokens.
#[derive(Clone, Debug)]
pub struct TomlLexer<'config> {
    config: &'config TomlLanguage,
}

impl<'config> Lexer<TomlLanguage> for TomlLexer<'config> {
    fn lex<'a, S: Source + ?Sized>(
        &self,
        source: &'a S,
        _edits: &[TextEdit],
        cache: &'a mut impl LexerCache<TomlLanguage>,
    ) -> LexOutput<TomlLanguage> {
        let mut state = State::new(source);
        let result = self.run(&mut state);
        if result.is_ok() {
            state.add_eof();
        }
        state.finish_with_cache(result, cache)
    }
}

impl<'config> TomlLexer<'config> {
    /// Creates a new `TomlLexer` with the given language configuration.
    pub fn new(config: &'config TomlLanguage) -> Self {
        Self { config }
    }

    /// Main lexing loop that iterates through the source text.
    fn run<S: Source + ?Sized>(&self, state: &mut State<S>) -> Result<(), TextError> {
        while state.not_at_end() {
            let Some(ch) = state.current()
            else {
                break;
            };
            match ch {
                ' ' | '\t' => {
                    self.skip_whitespace(state);
                }
                '\n' | '\r' => {
                    self.lex_newline(state);
                }
                '#' => {
                    self.skip_comment(state);
                }
                '"' | '\'' => {
                    self.lex_string(state);
                }
                '+' | '-' | '0'..='9' => {
                    self.lex_number_or_datetime(state);
                }
                '[' | ']' | '{' | '}' | ',' | '.' | '=' => {
                    self.lex_punctuation(state);
                }
                'a'..='z' | 'A'..='Z' | '_' => {
                    self.lex_identifier(state);
                }
                _ => {
                    // Unknown character: emit an error token so the parser can report it.
                    let start = state.get_position();
                    state.advance(ch.len_utf8());
                    state.add_token(TomlSyntaxKind::Error, start, state.get_position());
                }
            }
        }
        Ok(())
    }

    /// Lexes spaces and tabs (newlines are handled separately).
    fn skip_whitespace<S: Source + ?Sized>(&self, state: &mut State<S>) -> bool {
        let start_pos = state.get_position();

        while let Some(ch) = state.current() {
            if ch == ' ' || ch == '\t' {
                state.advance(1);
            }
            else {
                break;
            }
        }

        if state.get_position() > start_pos {
            state.add_token(TomlSyntaxKind::Whitespace, start_pos, state.get_position());
            true
        }
        else {
            false
        }
    }

    /// Lexes a single newline (`\n` or `\r\n`).
    fn lex_newline<S: Source + ?Sized>(&self, state: &mut State<S>) {
        let start = state.get_position();
        if state.current() == Some('\r') {
            state.advance(1);
            if state.current() == Some('\n') {
                state.advance(1);
            }
        }
        else {
            state.advance(1);
        }
        state.add_token(TomlSyntaxKind::Newline, start, state.get_position());
    }

    /// Lexes and skips TOML comments.
    fn skip_comment<S: Source + ?Sized>(&self, state: &mut State<S>) -> bool {
        if state.current() == Some('#') {
            let start_pos = state.get_position();
            state.advance(1);

            // Read until end of line
            while let Some(ch) = state.current() {
                if ch == '\n' || ch == '\r' {
                    break;
                }
                state.advance(ch.len_utf8());
            }

            state.add_token(TomlSyntaxKind::Comment, start_pos, state.get_position());
            true
        }
        else {
            false
        }
    }

    /// Lexes basic (`"..."`) and literal (`'...'`) strings, including their multi-line forms.
    fn lex_string<S: Source + ?Sized>(&self, state: &mut State<S>) -> bool {
        let quote = match state.current() {
            Some('"') => '"',
            Some('\'') => '\'',
            _ => return false,
        };
        let is_basic = quote == '"';
        let start = state.get_position();

        let multiline = state.peek_next() == Some(quote) && state.peek_next_n(2) == Some(quote);
        state.advance(1);
        if multiline {
            state.advance(2);
        }

        if multiline {
            loop {
                match state.current() {
                    None => break,
                    Some(ch) if ch == quote => {
                        // Closing delimiter is three quotes; up to two extra quotes belong to the content.
                        if state.peek_next() == Some(quote) && state.peek_next_n(2) == Some(quote) {
                            state.advance(3);
                            let mut extra = 0;
                            while extra < 2 && state.current() == Some(quote) {
                                state.advance(1);
                                extra += 1;
                            }
                            break;
                        }
                        state.advance(1);
                    }
                    Some('\\') if is_basic => {
                        state.advance(1);
                        if let Some(ch) = state.current() {
                            state.advance(ch.len_utf8());
                        }
                    }
                    Some(ch) => state.advance(ch.len_utf8()),
                }
            }
        }
        else {
            loop {
                match state.current() {
                    None | Some('\n') | Some('\r') => break,
                    Some(ch) if ch == quote => {
                        state.advance(1);
                        break;
                    }
                    Some('\\') if is_basic => {
                        state.advance(1);
                        if let Some(ch) = state.current() {
                            state.advance(ch.len_utf8());
                        }
                    }
                    Some(ch) => state.advance(ch.len_utf8()),
                }
            }
        }

        let kind = if is_basic { TomlSyntaxKind::BasicString } else { TomlSyntaxKind::LiteralString };
        state.add_token(kind, start, state.get_position());
        true
    }

    /// Lexes numbers, `inf`/`nan`, and date-time literals.
    fn lex_number_or_datetime<S: Source + ?Sized>(&self, state: &mut State<S>) -> bool {
        if self.lex_datetime(state) {
            return true;
        }
        self.lex_number(state)
    }

    /// Attempts to lex an RFC 3339 date-time, local date-time, local date, or local time.
    fn lex_datetime<S: Source + ?Sized>(&self, state: &mut State<S>) -> bool {
        let start = state.get_position();
        let Some((len, kind)) = ({
            let rest = state.rest();
            scan_datetime(rest)
        })
        else {
            return false;
        };

        state.advance(len);
        state.add_token(kind, start, state.get_position());
        true
    }

    /// Lexes numeric literals.
    fn lex_number<S: Source + ?Sized>(&self, state: &mut State<S>) -> bool {
        if !state.current().map_or(false, |c| c.is_ascii_digit() || c == '-' || c == '+') {
            return false;
        }

        let start = state.get_position();

        // Skip sign
        if matches!(state.current(), Some('-') | Some('+')) {
            state.advance(1);
        }

        // Special floats: inf / nan
        if state.starts_with("inf") || state.starts_with("nan") {
            state.advance(3);
            state.add_token(TomlSyntaxKind::Float, start, state.get_position());
            return true;
        }

        // Base-prefixed integers
        let base = if state.starts_with("0x") || state.starts_with("0X") {
            Some(16)
        }
        else if state.starts_with("0o") || state.starts_with("0O") {
            Some(8)
        }
        else if state.starts_with("0b") || state.starts_with("0B") {
            Some(2)
        }
        else {
            None
        };
        if base.is_some() {
            state.advance(2);
            while state.current().map_or(false, |c| c.is_ascii_alphanumeric() || c == '_') {
                state.advance(1);
            }
            state.add_token(TomlSyntaxKind::Integer, start, state.get_position());
            return true;
        }

        let mut is_float = false;

        // Integer part
        self.consume_digits(state);

        // Fractional part: only when a digit follows the dot (`1.5`), so `1.` stays an integer.
        if state.current() == Some('.') && state.peek_next().map_or(false, |c| c.is_ascii_digit()) {
            is_float = true;
            state.advance(1);
            self.consume_digits(state);
        }

        // Exponent part
        if matches!(state.current(), Some('e') | Some('E')) {
            let before_exponent = state.get_position();
            state.advance(1);
            if matches!(state.current(), Some('+') | Some('-')) {
                state.advance(1);
            }
            if state.current().map_or(false, |c| c.is_ascii_digit()) {
                is_float = true;
                self.consume_digits(state);
            }
            else {
                // Not an exponent after all; leave the `e` for the next token.
                state.set_position(before_exponent);
            }
        }

        let kind = if is_float { TomlSyntaxKind::Float } else { TomlSyntaxKind::Integer };
        state.add_token(kind, start, state.get_position());
        true
    }

    /// Consumes digits and `_` separators.
    fn consume_digits<S: Source + ?Sized>(&self, state: &mut State<S>) {
        while state.current().map_or(false, |c| c.is_ascii_digit() || c == '_') {
            state.advance(1);
        }
    }

    /// Lexes punctuation.
    fn lex_punctuation<S: Source + ?Sized>(&self, state: &mut State<S>) -> bool {
        let start = state.get_position();

        match state.current() {
            Some('[') => {
                state.advance(1);
                if state.current() == Some('[') {
                    state.advance(1);
                    let end = state.get_position();
                    state.add_token(TomlSyntaxKind::DoubleLeftBracket, start, end);
                }
                else {
                    let end = state.get_position();
                    state.add_token(TomlSyntaxKind::LeftBracket, start, end);
                }
                true
            }
            Some(']') => {
                state.advance(1);
                if state.current() == Some(']') {
                    state.advance(1);
                    let end = state.get_position();
                    state.add_token(TomlSyntaxKind::DoubleRightBracket, start, end);
                }
                else {
                    let end = state.get_position();
                    state.add_token(TomlSyntaxKind::RightBracket, start, end);
                }
                true
            }
            Some('{') => {
                state.advance(1);
                let end = state.get_position();
                state.add_token(TomlSyntaxKind::LeftBrace, start, end);
                true
            }
            Some('}') => {
                state.advance(1);
                let end = state.get_position();
                state.add_token(TomlSyntaxKind::RightBrace, start, end);
                true
            }
            Some(',') => {
                state.advance(1);
                let end = state.get_position();
                state.add_token(TomlSyntaxKind::Comma, start, end);
                true
            }
            Some('.') => {
                state.advance(1);
                let end = state.get_position();
                state.add_token(TomlSyntaxKind::Dot, start, end);
                true
            }
            Some('=') => {
                state.advance(1);
                let end = state.get_position();
                state.add_token(TomlSyntaxKind::Equal, start, end);
                true
            }
            _ => false,
        }
    }

    /// Lexes identifiers (bare keys and the `true`/`false` keywords).
    fn lex_identifier<S: Source + ?Sized>(&self, state: &mut State<S>) -> bool {
        if !state.current().map_or(false, |c| c.is_ascii_alphabetic() || c == '_') {
            return false;
        }

        let start = state.get_position();

        while state.current().map_or(false, |c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            state.advance(1);
        }

        let end = state.get_position();

        // Check for keywords
        let text = state.get_text_in((start..end).into());
        let kind = match text.as_ref() {
            "true" | "false" => TomlSyntaxKind::Boolean,
            _ => TomlSyntaxKind::BareKey,
        };

        state.add_token(kind, start, end);
        true
    }
}

/// Scans a TOML date-time literal at the start of `rest`.
///
/// Returns the byte length and token kind when `rest` begins with a valid
/// local time, local date, local date-time, or offset date-time.
fn scan_datetime(rest: &str) -> Option<(usize, TomlSyntaxKind)> {
    let bytes = rest.as_bytes();

    // Local time: HH:MM:SS[.fraction]
    if bytes.len() >= 8 && bytes.get(2) == Some(&b':') {
        let len = scan_time(bytes)?;
        return Some((len, TomlSyntaxKind::LocalTime));
    }

    // Local date: YYYY-MM-DD
    if !is_date_prefix(bytes) {
        return None;
    }

    let mut pos = 10;
    match bytes.get(pos) {
        Some(b'T') | Some(b't') => {
            pos += 1;
            let time_len = scan_time(&bytes[pos..])?;
            pos += time_len;

            let (offset_len, _) = scan_offset(&bytes[pos..]).unwrap_or((0, false));
            let has_offset = offset_len > 0;
            pos += offset_len;

            let kind = if has_offset { TomlSyntaxKind::OffsetDateTime } else { TomlSyntaxKind::LocalDateTime };
            Some((pos, kind))
        }
        _ => Some((pos, TomlSyntaxKind::LocalDate)),
    }
}

/// Checks for `YYYY-MM-DD`.
fn is_date_prefix(bytes: &[u8]) -> bool {
    bytes.len() >= 10
        && bytes[0..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..10].iter().all(u8::is_ascii_digit)
}

/// Scans `HH:MM:SS[.fraction]`, returning the consumed length.
fn scan_time(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 8 {
        return None;
    }
    let shape = bytes[0].is_ascii_digit()
        && bytes[1].is_ascii_digit()
        && bytes[2] == b':'
        && bytes[3].is_ascii_digit()
        && bytes[4].is_ascii_digit()
        && bytes[5] == b':'
        && bytes[6].is_ascii_digit()
        && bytes[7].is_ascii_digit();
    if !shape {
        return None;
    }

    let mut pos = 8;
    if bytes.get(pos) == Some(&b'.') {
        pos += 1;
        let digits_start = pos;
        while bytes.get(pos).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        }
        if pos == digits_start {
            return None;
        }
    }
    Some(pos)
}

/// Scans `Z` or `±HH:MM`, returning the consumed length.
fn scan_offset(bytes: &[u8]) -> Option<(usize, bool)> {
    match bytes.first() {
        Some(b'Z') | Some(b'z') => Some((1, true)),
        Some(b'+') | Some(b'-') => {
            if bytes.len() >= 6
                && bytes[1].is_ascii_digit()
                && bytes[2].is_ascii_digit()
                && bytes[3] == b':'
                && bytes[4].is_ascii_digit()
                && bytes[5].is_ascii_digit()
            {
                Some((6, true))
            }
            else {
                None
            }
        }
        _ => None,
    }
}
