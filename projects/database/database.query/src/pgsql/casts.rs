//! Rewrites the PostgreSQL `expr::type` cast operator into `CAST(expr AS type)`.
//!
//! the parser parses `CAST(...)` into a cast expression, so every `::` spelling the
//! gateway accepts is lowered onto that one node. The rewrite works on a small
//! PostgreSQL-only token stream: it never becomes a second general SQL parser,
//! and it refuses operand shapes it cannot attribute instead of dropping the
//! cast.
//!
//! String literals, quoted identifiers and comments are single tokens, so the
//! operator is only recognised in code positions. Whitespace between tokens is
//! normalised; literal text, identifiers and parameter placeholders are copied
//! verbatim.

use std::borrow::Cow;

use crate::sql::{SqlDialect, SqlFrontendError};

/// Multi-word type-name continuations, e.g. `double precision`,
/// `character varying` and `timestamp with time zone`.
const TYPE_CONTINUATIONS: &[&str] = &["PRECISION", "VARYING", "WITH", "WITHOUT", "TIME", "ZONE", "LOCAL"];

/// Literal prefixes that belong to the quoted string that follows them.
const STRING_PREFIXES: &[&str] = &["E", "e", "X", "x", "B", "b", "N", "n"];

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    /// Bare word: identifier, keyword or type name.
    Word(String),
    /// Numeric literal.
    Number(String),
    /// Single-quoted literal, quotes included verbatim.
    String(String),
    /// Double-quoted identifier, quotes included verbatim.
    Quoted(String),
    /// `$n` parameter placeholder.
    Placeholder(String),
    /// `(`, `)`, `[`, `]`, `,`, `.`, `;`.
    Punct(char),
    /// Operator token such as `::`, `=`, `>=` or `||`.
    Op(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Token {
    kind: Kind,
    start: usize,
    end: usize,
}

impl Token {
    fn text(&self) -> &str {
        match &self.kind {
            Kind::Word(text)
            | Kind::Number(text)
            | Kind::String(text)
            | Kind::Quoted(text)
            | Kind::Placeholder(text)
            | Kind::Op(text) => text,
            Kind::Punct('(') => "(",
            Kind::Punct(')') => ")",
            Kind::Punct('[') => "[",
            Kind::Punct(']') => "]",
            Kind::Punct(',') => ",",
            Kind::Punct('.') => ".",
            Kind::Punct(';') => ";",
            Kind::Punct(_) => "?",
        }
    }

    fn is_punct(&self, expected: char) -> bool {
        self.kind == Kind::Punct(expected)
    }

    fn is_word(&self) -> bool {
        matches!(self.kind, Kind::Word(_) | Kind::Quoted(_))
    }

    /// Whether this token is a bare name that may prefix a function call.
    fn is_callable_name(&self) -> bool {
        matches!(self.kind, Kind::Word(_) | Kind::Quoted(_))
    }
}

/// Rewrites every `::` cast in `sql` into the `CAST(...)` form.
///
/// Returns the input unchanged when it carries no cast operator. Non-PostgreSQL
/// dialects reject the spelling: `::` is PostgreSQL syntax, not a shared one.
pub fn rewrite_double_colon_casts(sql: &str, dialect: SqlDialect) -> Result<Cow<'_, str>, SqlFrontendError> {
    if !sql.contains("::") {
        return Ok(Cow::Borrowed(sql));
    }
    if dialect != SqlDialect::Postgres {
        return Err(SqlFrontendError::syntax("the `::` cast operator is PostgreSQL syntax and is not accepted in this dialect"));
    }
    let mut tokens = tokenize(sql)?;
    let mut index = 0;
    let mut rewritten = false;
    while index < tokens.len() {
        if matches!(&tokens[index].kind, Kind::Op(op) if op == "::") {
            let start = left_operand_start(&tokens, index)?;
            let (end, type_tokens) = type_spec(&tokens, index + 1)?;
            let mut replacement = Vec::with_capacity(3 + index - start + type_tokens.len());
            replacement.push(synthetic_word("CAST"));
            replacement.push(synthetic_punct('('));
            replacement.extend_from_slice(&tokens[start..index]);
            replacement.push(synthetic_word("AS"));
            replacement.extend(type_tokens);
            replacement.push(synthetic_punct(')'));
            let next = start + replacement.len();
            tokens.splice(start..end, replacement);
            rewritten = true;
            // A chained cast (`x::int::text`) is rewritten on the next pass.
            index = next;
            continue;
        }
        index += 1;
    }
    if !rewritten {
        return Ok(Cow::Borrowed(sql));
    }
    Ok(Cow::Owned(render(&tokens)))
}

fn synthetic_word(text: &str) -> Token {
    Token { kind: Kind::Word(text.to_string()), start: 0, end: 0 }
}

fn synthetic_punct(ch: char) -> Token {
    Token { kind: Kind::Punct(ch), start: 0, end: 0 }
}

fn left_operand_start(tokens: &[Token], operator: usize) -> Result<usize, SqlFrontendError> {
    let Some(previous) = operator.checked_sub(1).and_then(|index| tokens.get(index))
    else {
        return Err(SqlFrontendError::syntax("the `::` cast operator has no left operand"));
    };
    match &previous.kind {
        Kind::Punct(')') => {
            let open = matching_open(tokens, operator - 1, '(', ')')?;
            // Only a call name immediately touching the parenthesis is part of
            // the operand: `COUNT(*)::int` includes `COUNT`, while
            // `SELECT (a + b)::int` does not include `SELECT`.
            if open > 0 && tokens[open - 1].is_callable_name() && tokens[open - 1].end == tokens[open].start {
                Ok(open - 1)
            }
            else {
                Ok(open)
            }
        }
        Kind::Punct(']') => {
            let open = matching_open(tokens, operator - 1, '[', ']')?;
            if open > 0 && tokens[open - 1].is_callable_name() && tokens[open - 1].end == tokens[open].start {
                Ok(open - 1)
            }
            else {
                Ok(open)
            }
        }
        Kind::Word(_) | Kind::Number(_) | Kind::String(_) | Kind::Quoted(_) | Kind::Placeholder(_) => {
            let mut start = operator - 1;
            while start >= 2 && tokens[start - 1].is_punct('.') && tokens[start - 2].is_word() {
                start -= 2;
            }
            // A lone keyword is statement syntax, not an operand: `WHERE ::int`
            // must stay a syntax error instead of becoming `CAST(WHERE ...)`.
            if start == operator - 1 {
                if let Kind::Word(word) = &tokens[start].kind {
                    let qualified = start > 0 && tokens[start - 1].is_punct('.');
                    if !qualified && is_statement_keyword(word) {
                        return Err(SqlFrontendError::syntax(format!("`{word}` cannot be the left operand of `::`")));
                    }
                }
            }
            Ok(start)
        }
        other => Err(SqlFrontendError::syntax(format!("`{other:?}` cannot be the left operand of `::`"))),
    }
}

fn matching_open(tokens: &[Token], close: usize, open: char, expected_close: char) -> Result<usize, SqlFrontendError> {
    let mut depth = 0usize;
    let mut index = close;
    loop {
        if tokens[index].is_punct(expected_close) {
            depth += 1;
        }
        else if tokens[index].is_punct(open) {
            depth -= 1;
            if depth == 0 {
                return Ok(index);
            }
        }
        if index == 0 {
            return Err(SqlFrontendError::syntax(format!("unbalanced `{open}` before the `::` cast operator")));
        }
        index -= 1;
    }
}

fn type_spec(tokens: &[Token], at: usize) -> Result<(usize, Vec<Token>), SqlFrontendError> {
    let mut index = at;
    let mut spec = Vec::new();
    match tokens.get(index) {
        Some(Token { kind: Kind::Word(text), .. }) | Some(Token { kind: Kind::Quoted(text), .. }) => {
            spec.push(synthetic_word(text));
            index += 1;
        }
        _ => return Err(SqlFrontendError::syntax("the `::` cast operator has no target type")),
    }
    while let Some(Token { kind: Kind::Word(word), .. }) = tokens.get(index) {
        if TYPE_CONTINUATIONS.contains(&word.to_ascii_uppercase().as_str()) {
            spec.push(synthetic_word(word));
            index += 1;
        }
        else {
            break;
        }
    }
    // Type modifiers (`numeric(10,2)`, `varchar(64)`) keep their parentheses.
    if tokens.get(index).is_some_and(|token| token.is_punct('(')) {
        let close = matching_close_forward(tokens, index)?;
        spec.extend_from_slice(&tokens[index..=close]);
        index = close + 1;
    }
    // Array suffixes (`::text[]`, `::int[][]`) stay attached to the type.
    while tokens.get(index).is_some_and(|token| token.is_punct('[')) && tokens.get(index + 1).is_some_and(|token| token.is_punct(']')) {
        spec.push(synthetic_punct('['));
        spec.push(synthetic_punct(']'));
        index += 2;
    }
    Ok((index, spec))
}

fn matching_close_forward(tokens: &[Token], open: usize) -> Result<usize, SqlFrontendError> {
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate().skip(open) {
        if token.is_punct('(') {
            depth += 1;
        }
        else if token.is_punct(')') {
            depth -= 1;
            if depth == 0 {
                return Ok(index);
            }
        }
    }
    Err(SqlFrontendError::syntax("unbalanced `(` in a cast target type"))
}

fn render(tokens: &[Token]) -> String {
    let mut out = String::new();
    let mut previous: Option<&Token> = None;
    for token in tokens {
        if let Some(previous) = previous {
            if needs_space(previous, token) {
                out.push(' ');
            }
        }
        out.push_str(token.text());
        previous = Some(token);
    }
    out
}

fn needs_space(previous: &Token, next: &Token) -> bool {
    if previous.is_punct('(') || previous.is_punct('[') || previous.is_punct(',') || previous.is_punct('.') {
        return false;
    }
    if next.is_punct('(') || next.is_punct(')') || next.is_punct('[') || next.is_punct(']') || next.is_punct(',') || next.is_punct('.')
        || next.is_punct(';')
    {
        return false;
    }
    true
}

/// Keywords that can never be the operand of a cast. A bare identifier may
/// share the spelling (`case` as a column name is quoted in real schemas), so
/// only unqualified occurrences are refused.
fn is_statement_keyword(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "SELECT"
            | "FROM"
            | "WHERE"
            | "GROUP"
            | "ORDER"
            | "HAVING"
            | "LIMIT"
            | "OFFSET"
            | "AND"
            | "OR"
            | "NOT"
            | "ON"
            | "AS"
            | "IN"
            | "IS"
            | "NULL"
            | "TRUE"
            | "FALSE"
            | "WHEN"
            | "THEN"
            | "ELSE"
            | "CASE"
            | "SET"
            | "VALUES"
            | "RETURNING"
            | "BY"
            | "JOIN"
            | "USING"
            | "WITH"
            | "DISTINCT"
            | "UNION"
            | "INSERT"
            | "UPDATE"
            | "DELETE"
    )
}

fn tokenize(source: &str) -> Result<Vec<Token>, SqlFrontendError> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if byte == b'-' && bytes.get(index + 1) == Some(&b'-') {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            let mut closed = false;
            while index + 1 < bytes.len() {
                if bytes[index] == b'*' && bytes[index + 1] == b'/' {
                    index += 2;
                    closed = true;
                    break;
                }
                index += 1;
            }
            if !closed {
                return Err(SqlFrontendError::syntax("unclosed `/* */` comment in SQL"));
            }
            continue;
        }
        let start = index;
        let kind = match byte {
            b'\'' => {
                let end = skip_single_quoted(bytes, index)?;
                index = end;
                Kind::String(source[start..end].to_string())
            }
            b'"' => {
                let end = skip_double_quoted(bytes, index)?;
                index = end;
                Kind::Quoted(source[start..end].to_string())
            }
            b'`' => {
                return Err(SqlFrontendError::syntax("backtick-quoted identifiers are MySQL syntax and are not accepted in PostgreSQL SQL"));
            }
            b'$' => {
                if let Some(end) = skip_dollar_quoted(bytes, index)? {
                    index = end;
                    Kind::String(source[start..end].to_string())
                }
                else {
                    index += 1;
                    while index < bytes.len() && bytes[index].is_ascii_digit() {
                        index += 1;
                    }
                    Kind::Placeholder(source[start..index].to_string())
                }
            }
            b'(' | b')' | b'[' | b']' | b',' | b'.' | b';' => {
                index += 1;
                Kind::Punct(byte as char)
            }
            byte if byte.is_ascii_digit() => {
                index += 1;
                while index < bytes.len() && (bytes[index].is_ascii_digit() || bytes[index] == b'.') {
                    index += 1;
                }
                if index < bytes.len() && (bytes[index] == b'e' || bytes[index] == b'E') {
                    index += 1;
                    if index < bytes.len() && (bytes[index] == b'+' || bytes[index] == b'-') {
                        index += 1;
                    }
                    while index < bytes.len() && bytes[index].is_ascii_digit() {
                        index += 1;
                    }
                }
                Kind::Number(source[start..index].to_string())
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                index += 1;
                while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_' || bytes[index] == b'$') {
                    index += 1;
                }
                let word = &source[start..index];
                // `E'...'` / `X'...'` / `B'...'` prefixes belong to the literal.
                if STRING_PREFIXES.contains(&word) && bytes.get(index) == Some(&b'\'') {
                    let end = skip_single_quoted(bytes, index)?;
                    index = end;
                    Kind::String(source[start..end].to_string())
                }
                else {
                    Kind::Word(word.to_string())
                }
            }
            b':' if bytes.get(index + 1) == Some(&b':') => {
                index += 2;
                Kind::Op("::".to_string())
            }
            _ => {
                index += 1;
                if index < bytes.len() && is_operator_byte(bytes[start]) && is_operator_byte(bytes[index]) {
                    index += 1;
                }
                Kind::Op(source[start..index].to_string())
            }
        };
        tokens.push(Token { kind, start, end: index });
    }
    Ok(tokens)
}

fn is_operator_byte(byte: u8) -> bool {
    matches!(byte, b'=' | b'<' | b'>' | b'!' | b'|' | b'&' | b'+' | b'-' | b'*' | b'/' | b'%' | b'#' | b'~' | b'^' | b'@' | b'?')
}

fn skip_single_quoted(bytes: &[u8], start: usize) -> Result<usize, SqlFrontendError> {
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 1 < bytes.len() {
            index += 2;
            continue;
        }
        if bytes[index] == b'\'' {
            if bytes.get(index + 1) == Some(&b'\'') {
                index += 2;
                continue;
            }
            return Ok(index + 1);
        }
        index += 1;
    }
    Err(SqlFrontendError::syntax("unclosed string literal in SQL"))
}

fn skip_double_quoted(bytes: &[u8], start: usize) -> Result<usize, SqlFrontendError> {
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            if bytes.get(index + 1) == Some(&b'"') {
                index += 2;
                continue;
            }
            return Ok(index + 1);
        }
        index += 1;
    }
    Err(SqlFrontendError::syntax("unclosed quoted identifier in SQL"))
}

/// Skips a `$$…$$` or `$tag$…$tag$` literal, returning `None` when the `$`
/// starts a `$n` parameter instead.
fn skip_dollar_quoted(bytes: &[u8], start: usize) -> Result<Option<usize>, SqlFrontendError> {
    let mut index = start + 1;
    while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_') {
        index += 1;
    }
    if bytes.get(index) != Some(&b'$') {
        return Ok(None);
    }
    let tag = &bytes[start..=index];
    let mut cursor = index + 1;
    while cursor + tag.len() <= bytes.len() {
        if &bytes[cursor..cursor + tag.len()] == tag {
            return Ok(Some(cursor + tag.len()));
        }
        cursor += 1;
    }
    Err(SqlFrontendError::syntax("unclosed dollar-quoted string in SQL"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewrite(sql: &str) -> String {
        rewrite_double_colon_casts(sql, SqlDialect::Postgres).expect("rewrite").into_owned()
    }

    #[test]
    fn literal_casts_keep_the_literal_text() {
        assert_eq!(rewrite("SELECT '1'::int"), "SELECT CAST('1' AS int)");
        assert_eq!(rewrite("SELECT 1::BIGINT + 2"), "SELECT CAST(1 AS BIGINT) + 2");
    }

    #[test]
    fn qualified_columns_function_calls_and_parenthesised_operands() {
        assert_eq!(rewrite("SELECT t.v::text FROM t"), "SELECT CAST(t.v AS text) FROM t");
        assert_eq!(rewrite("SELECT COUNT(*)::int FROM t"), "SELECT CAST(COUNT(*) AS int) FROM t");
        assert_eq!(rewrite("SELECT (a + b)::numeric(10,2) FROM t"), "SELECT CAST((a + b) AS numeric(10,2)) FROM t");
    }

    #[test]
    fn chained_casts_nest() {
        assert_eq!(rewrite("SELECT '1'::int::text"), "SELECT CAST(CAST('1' AS int) AS text)");
    }

    #[test]
    fn multi_word_types_and_arrays_stay_attached() {
        assert_eq!(rewrite("SELECT ts::timestamp with time zone"), "SELECT CAST(ts AS timestamp with time zone)");
        assert_eq!(rewrite("SELECT body::text[]"), "SELECT CAST(body AS text[])");
    }

    #[test]
    fn placeholders_and_quoted_identifiers_are_preserved() {
        assert_eq!(rewrite("SELECT $1::uuid"), "SELECT CAST($1 AS uuid)");
        assert_eq!(rewrite("SELECT \"Weird Col\"::text FROM t"), "SELECT CAST(\"Weird Col\" AS text) FROM t");
    }

    #[test]
    fn operators_inside_literals_and_comments_are_not_rewritten() {
        assert_eq!(rewrite("SELECT 'a::int'"), "SELECT 'a::int'");
        assert_eq!(rewrite("SELECT 1 -- x::int\n"), "SELECT 1 -- x::int\n");
        assert_eq!(rewrite("SELECT 1 /* x::int */"), "SELECT 1 /* x::int */");
    }

    #[test]
    fn a_comment_only_operator_leaves_the_input_untouched() {
        assert_eq!(rewrite("SELECT 1 -- x::int"), "SELECT 1 -- x::int");
    }

    #[test]
    fn non_postgres_dialects_reject_the_operator() {
        assert!(rewrite_double_colon_casts("SELECT '1'::int", SqlDialect::Mysql).is_err());
        assert!(matches!(rewrite_double_colon_casts("SELECT 1", SqlDialect::Mysql), Ok(Cow::Borrowed(_))));
    }

    #[test]
    fn a_cast_without_a_target_type_is_a_syntax_error() {
        assert!(rewrite_double_colon_casts("SELECT 1::", SqlDialect::Postgres).is_err());
        assert!(rewrite_double_colon_casts("SELECT ::int", SqlDialect::Postgres).is_err());
    }
}
