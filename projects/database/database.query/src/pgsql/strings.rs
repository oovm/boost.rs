//! `E'...'` escape-string rewrite applied before the shared parser.
//!
//! The SQL surface only models the standard single-quoted literal, whose
//! backslash is a plain character under `standard_conforming_strings = on`.
//! PostgreSQL's `E'...'` spelling interprets backslash escapes, so this module
//! decodes them and emits a standard literal with quotes doubled. Everything
//! else on the statement — comments, identifiers, dollar-quoted bodies and
//! ordinary literals — is copied byte for byte.

use crate::sql::{SqlErrorKind, SqlFrontendError};

/// Rewrites every `E'...'` literal into the equivalent standard literal.
///
/// Returns a syntax error for an unterminated literal, exactly like PostgreSQL
/// rejects the statement instead of guessing where it ends.
pub fn rewrite_escape_strings(source: &str) -> Result<String, SqlFrontendError> {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        // `$tag$ ... $tag$` bodies may contain anything, including `E'`.
        if byte == b'$' {
            if let Some((body, next)) = take_dollar_quoted(source, index) {
                out.push_str(body);
                index = next;
                continue;
            }
        }
        if byte == b'-' && bytes.get(index + 1) == Some(&b'-') {
            let start = index;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            out.push_str(&source[start..index]);
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            let start = index;
            index += 2;
            while index < bytes.len() && !(bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/')) {
                index += 1;
            }
            index = (index + 2).min(bytes.len());
            out.push_str(&source[start..index]);
            continue;
        }
        if byte == b'"' {
            let (identifier, next) = take_quoted(source, index, b'"');
            out.push_str(identifier);
            index = next;
            continue;
        }
        if byte == b'\'' {
            let (literal, next) = take_quoted(source, index, b'\'');
            out.push_str(literal);
            index = next;
            continue;
        }
        // `E'` only starts an escape literal when the `E` is not the tail of an
        // identifier (`TABLE'x'` is not an escape literal).
        let previous_is_identifier = index > 0 && (bytes[index - 1].is_ascii_alphanumeric() || matches!(bytes[index - 1], b'_' | b'$'));
        if (byte == b'E' || byte == b'e') && bytes.get(index + 1) == Some(&b'\'') && !previous_is_identifier {
            let (body, next) = take_escape_body(source, index + 2)?;
            out.push('\'');
            out.push_str(&decode_escape_body(&body));
            out.push('\'');
            index = next;
            continue;
        }
        let character = source[index..].chars().next().expect("index is inside the string");
        out.push(character);
        index += character.len_utf8();
    }
    Ok(out)
}

/// Copies one single- or double-quoted token starting at `start`.
fn take_quoted(source: &str, start: usize, quote: u8) -> (&str, usize) {
    let bytes = source.as_bytes();
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == quote {
            if bytes.get(index + 1) == Some(&quote) {
                index += 2;
                continue;
            }
            return (&source[start..index + 1], index + 1);
        }
        index += 1;
    }
    (&source[start..], bytes.len())
}

/// Copies one `$tag$ ... $tag$` body, when `start` opens one.
fn take_dollar_quoted(source: &str, start: usize) -> Option<(&str, usize)> {
    let bytes = source.as_bytes();
    let mut end = start + 1;
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    if bytes.get(end) != Some(&b'$') {
        return None;
    }
    let delimiter = &source[start..=end];
    let body_start = end + 1;
    let rest = &source[body_start..];
    let closing = rest.find(delimiter)?;
    let next = body_start + closing + delimiter.len();
    Some((&source[start..next], next))
}

/// Reads the body of an `E'...'` literal, returning it without the quotes.
fn take_escape_body(source: &str, body_start: usize) -> Result<(String, usize), SqlFrontendError> {
    let bytes = source.as_bytes();
    let mut body = String::new();
    let mut index = body_start;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => {
                // Keep the escape sequence verbatim for the decoder; `\'` keeps
                // the quote escaped instead of ending the literal.
                let next = (index + 2).min(bytes.len());
                body.push_str(&source[index..next]);
                index = next;
            }
            b'\'' => {
                if bytes.get(index + 1) == Some(&b'\'') {
                    // A doubled quote is one quote in the decoded value.
                    body.push('\'');
                    index += 2;
                    continue;
                }
                return Ok((body, index + 1));
            }
            _ => {
                let character = source[index..].chars().next().expect("index is inside the string");
                body.push(character);
                index += character.len_utf8();
            }
        }
    }
    Err(SqlFrontendError::new(SqlErrorKind::Syntax, "unterminated escape string literal"))
}

/// Decodes PostgreSQL's `E'...'` backslash escapes.
fn decode_escape_body(body: &str) -> String {
    let characters: Vec<char> = body.chars().collect();
    let mut decoded = String::with_capacity(body.len());
    let mut index = 0usize;
    while index < characters.len() {
        let character = characters[index];
        if character != '\\' || index + 1 >= characters.len() {
            decoded.push(character);
            index += 1;
            continue;
        }
        let escaped = characters[index + 1];
        match escaped {
            'b' => {
                decoded.push('\u{8}');
                index += 2;
            }
            'f' => {
                decoded.push('\u{c}');
                index += 2;
            }
            'n' => {
                decoded.push('\n');
                index += 2;
            }
            'r' => {
                decoded.push('\r');
                index += 2;
            }
            't' => {
                decoded.push('\t');
                index += 2;
            }
            'x' => {
                // `\xHH` consumes up to two hex digits.
                let mut digits = String::new();
                let mut cursor = index + 2;
                while cursor < characters.len() && digits.len() < 2 && characters[cursor].is_ascii_hexdigit() {
                    digits.push(characters[cursor]);
                    cursor += 1;
                }
                if digits.is_empty() {
                    decoded.push('x');
                    index += 2;
                }
                else {
                    let value = u8::from_str_radix(&digits, 16).unwrap_or_default();
                    decoded.push(char::from(value));
                    index = cursor;
                }
            }
            '0'..='7' => {
                // `\ooo` octal, up to three digits.
                let mut digits = String::new();
                let mut cursor = index + 1;
                while cursor < characters.len() && digits.len() < 3 && matches!(characters[cursor], '0'..='7') {
                    digits.push(characters[cursor]);
                    cursor += 1;
                }
                let value = u32::from_str_radix(&digits, 8).unwrap_or_default();
                if let Some(character) = char::from_u32(value) {
                    decoded.push(character);
                }
                index = cursor;
            }
            other => {
                // PostgreSQL treats an unknown escape as the escaped character.
                decoded.push(other);
                index += 2;
            }
        }
    }
    // Emit a standard literal: a quote inside the value is doubled.
    decoded.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_are_decoded_and_quotes_doubled() {
        assert_eq!(rewrite_escape_strings("SELECT E'a\\nb'").unwrap(), "SELECT 'a\nb'");
        assert_eq!(rewrite_escape_strings("SELECT E'\\x41\\x42'").unwrap(), "SELECT 'AB'");
        assert_eq!(rewrite_escape_strings("SELECT E'it\\'s'").unwrap(), "SELECT 'it''s'");
        assert_eq!(rewrite_escape_strings("SELECT E'a\\\\b'").unwrap(), "SELECT 'a\\b'");
    }

    #[test]
    fn ordinary_text_is_untouched() {
        assert_eq!(rewrite_escape_strings("SELECT 'E\\nx'").unwrap(), "SELECT 'E\\nx'");
        assert_eq!(rewrite_escape_strings("SELECT ENAME FROM t").unwrap(), "SELECT ENAME FROM t");
        assert_eq!(rewrite_escape_strings("SELECT $$E'x'$$").unwrap(), "SELECT $$E'x'$$");
        assert_eq!(rewrite_escape_strings("-- E'x'\nSELECT 1").unwrap(), "-- E'x'\nSELECT 1");
    }

    #[test]
    fn an_unterminated_escape_literal_is_a_syntax_error() {
        assert_eq!(rewrite_escape_strings("SELECT E'abc").unwrap_err().kind, SqlErrorKind::Syntax);
    }
}
