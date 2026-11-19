//! PostgreSQL utility statements the gateway executes itself.
//!
//! `PREPARE` / `EXECUTE` / `DEALLOCATE` (and later the cursor and notification
//! statements) never reach the binder: they are session commands whose
//! payload is another SQL statement. Parsing them here keeps the protocol
//! session free of SQL text handling while `database.execution::pgsql` owns the
//! state and the execution order.

use crate::sql::{SqlErrorKind, SqlFrontendError};

/// One PostgreSQL utility statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PgUtilityStatement {
    /// `PREPARE name [(type, …)] AS <statement>`.
    Prepare {
        /// Statement name.
        name: String,
        /// Declared parameter types, as written.
        parameter_types: Vec<String>,
        /// Statement text that follows `AS`.
        sql: String,
    },
    /// `EXECUTE name [(value, …)]`.
    Execute {
        /// Statement name.
        name: String,
        /// Argument expressions, as written.
        arguments: Vec<String>,
    },
    /// `DEALLOCATE [PREPARE] { name | ALL }`; `None` drops every statement.
    Deallocate {
        /// Statement name, or `None` for `ALL`.
        name: Option<String>,
    },
    /// `DECLARE name [BINARY] [INSENSITIVE] CURSOR [WITH HOLD] FOR <query>`.
    DeclareCursor {
        /// Cursor name.
        name: String,
        /// Whether `WITH HOLD` was written.
        with_hold: bool,
        /// Query the cursor materializes.
        sql: String,
    },
    /// `FETCH <direction> [FROM|IN] name`.
    Fetch {
        /// Cursor name.
        name: String,
        /// Position and row count to return.
        direction: FetchDirection,
    },
    /// `CLOSE name`.
    CloseCursor {
        /// Cursor name.
        name: String,
    },
    /// `EXPLAIN [(ANALYZE)] <statement>`.
    Explain {
        /// Whether the plan is also executed to report actual rows.
        analyze: bool,
        /// Explained statement.
        sql: String,
    },
    /// `LISTEN channel`.
    Listen {
        /// Channel name.
        channel: String,
    },
    /// `UNLISTEN { channel | * }`; `None` clears every subscription.
    Unlisten {
        /// Channel name, or `None` for `*`.
        channel: Option<String>,
    },
    /// `NOTIFY channel [, 'payload']`.
    Notify {
        /// Channel name.
        channel: String,
        /// Payload, empty when omitted.
        payload: String,
    },
    /// `COPY table [(columns)] FROM STDIN [WITH] [(FORMAT csv|text, …)]`.
    CopyFromStdin {
        /// Target table.
        table: String,
        /// Target columns, empty to use the table order.
        columns: Vec<String>,
        /// Input format.
        format: CopyFormat,
        /// Field delimiter.
        delimiter: char,
        /// Whether the first line is a header to skip.
        header: bool,
    },
}

/// `COPY` input format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyFormat {
    /// Tab-separated fields with backslash escapes (`TEXT`).
    Text,
    /// RFC 4180 quoting.
    Csv,
}

/// How much of a cursor one `FETCH` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchDirection {
    /// `NEXT` or a bare `FETCH name`.
    Next,
    /// `ALL` or `FORWARD ALL`: everything from the current position.
    All,
    /// `FORWARD n`: `n` rows forward.
    Forward(i64),
    /// `ABSOLUTE n`: the `n`-th row, 1-based, negative counting from the end.
    Absolute(i64),
    /// `RELATIVE n`: `n` rows from the current position.
    Relative(i64),
}

/// Parses one utility statement, or `Ok(None)` when the text is not one.
pub fn parse_pg_utility(source: &str) -> Result<Option<PgUtilityStatement>, SqlFrontendError> {
    let trimmed = source.trim().trim_end_matches(';').trim();
    let Some(keyword) = first_word(trimmed)
    else {
        return Ok(None);
    };
    match keyword.to_ascii_uppercase().as_str() {
        "PREPARE" => parse_prepare(&trimmed[keyword.len()..]).map(Some),
        "EXECUTE" => parse_execute(&trimmed[keyword.len()..]).map(Some),
        "DEALLOCATE" => parse_deallocate(&trimmed[keyword.len()..]).map(Some),
        "DECLARE" => parse_declare(&trimmed[keyword.len()..]).map(Some),
        "FETCH" => parse_fetch(&trimmed[keyword.len()..]).map(Some),
        "CLOSE" => parse_close(&trimmed[keyword.len()..]).map(Some),
        "EXPLAIN" => parse_explain(&trimmed[keyword.len()..]).map(Some),
        "LISTEN" => parse_listen(&trimmed[keyword.len()..]).map(Some),
        "UNLISTEN" => parse_unlisten(&trimmed[keyword.len()..]).map(Some),
        "NOTIFY" => parse_notify(&trimmed[keyword.len()..]).map(Some),
        "COPY" => parse_copy(&trimmed[keyword.len()..]).map(Some),
        _ => Ok(None),
    }
}

fn parse_copy(tail: &str) -> Result<PgUtilityStatement, SqlFrontendError> {
    let (table, rest) = take_name(tail.trim_start())?;
    let mut rest = rest.trim_start();
    let mut columns = Vec::new();
    if let Some(inner) = rest.strip_prefix('(') {
        let close = matching_paren(inner)?;
        columns = split_top_level(&inner[..close]);
        rest = inner[close + 1..].trim_start();
    }
    let Some(rest) = strip_keyword(rest, "FROM")
    else {
        return Err(syntax("COPY only supports `FROM STDIN`"));
    };
    let Some(rest) = strip_keyword(rest.trim_start(), "STDIN")
    else {
        return Err(syntax("COPY only supports `FROM STDIN`"));
    };
    let mut rest = rest.trim_start();
    // Legacy `WITH CSV` / `WITH HEADER` precede an optional option list.
    let mut format = CopyFormat::Text;
    let mut delimiter: Option<char> = None;
    let mut header = false;
    let mut saw_with = false;
    if let Some(after) = strip_keyword(rest, "WITH") {
        rest = after.trim_start();
        saw_with = true;
    }
    if let Some(after) = strip_keyword(rest, "CSV") {
        format = CopyFormat::Csv;
        rest = after.trim_start();
    }
    else if let Some(after) = strip_keyword(rest, "TEXT") {
        rest = after.trim_start();
    }
    if let Some(after) = strip_keyword(rest, "HEADER") {
        header = true;
        rest = after.trim_start();
    }
    if let Some(inner) = rest.strip_prefix('(') {
        let close = matching_paren(inner)?;
        for option in split_top_level(&inner[..close]) {
            let (name, value) = split_option(&option);
            match name.as_str() {
                "FORMAT" => match value.to_ascii_lowercase().as_str() {
                    "csv" => format = CopyFormat::Csv,
                    "text" => format = CopyFormat::Text,
                    other => return Err(syntax(format!("unsupported COPY format `{other}`"))),
                },
                "DELIMITER" => {
                    let decoded = decode_copy_literal(value)?;
                    let mut characters = decoded.chars();
                    let Some(character) = characters.next()
                    else {
                        return Err(syntax("COPY DELIMITER must be one character"));
                    };
                    if characters.next().is_some() {
                        return Err(syntax("COPY DELIMITER must be one character"));
                    }
                    delimiter = Some(character);
                }
                "HEADER" => header = true,
                other => return Err(syntax(format!("unsupported COPY option `{other}`"))),
            }
        }
        rest = inner[close + 1..].trim_start();
    }
    if !rest.is_empty() {
        return Err(syntax("COPY only supports `FROM STDIN`"));
    }
    let _ = saw_with;
    let delimiter = delimiter.unwrap_or(if format == CopyFormat::Csv { ',' } else { '\t' });
    Ok(PgUtilityStatement::CopyFromStdin { table, columns, format, delimiter, header })
}

/// Splits `NAME value` into the upper-case option name and its raw value.
fn split_option(option: &str) -> (String, &str) {
    let trimmed = option.trim();
    let end = trimmed.find(|character: char| character.is_whitespace() || character == '(').unwrap_or(trimmed.len());
    (trimmed[..end].to_ascii_uppercase(), trimmed[end..].trim())
}

/// Decodes `E'…'` / `'…'` COPY option literals, including backslash escapes.
fn decode_copy_literal(value: &str) -> Result<String, SqlFrontendError> {
    let trimmed = value.trim();
    let body = match trimmed.strip_prefix('E').or_else(|| trimmed.strip_prefix('e')) {
        Some(rest) => rest,
        None => trimmed,
    };
    let Some(body) = body.strip_prefix('\'')
    else {
        return Err(syntax("COPY option values must be string literals"));
    };
    let Some(body) = body.strip_suffix('\'')
    else {
        return Err(syntax("unterminated COPY option literal"));
    };
    let mut decoded = String::with_capacity(body.len());
    let mut characters = body.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            decoded.push(character);
            continue;
        }
        match characters.next() {
            Some('t') => decoded.push('\t'),
            Some('n') => decoded.push('\n'),
            Some('r') => decoded.push('\r'),
            Some(other) => decoded.push(other),
            None => return Err(syntax("trailing backslash in COPY option literal")),
        }
    }
    Ok(decoded)
}

fn parse_listen(tail: &str) -> Result<PgUtilityStatement, SqlFrontendError> {
    let (channel, trailing) = take_name(tail.trim_start())?;
    if !trailing.trim().is_empty() {
        return Err(syntax("LISTEN accepts one channel name"));
    }
    validate_channel(&channel)?;
    Ok(PgUtilityStatement::Listen { channel })
}

fn parse_unlisten(tail: &str) -> Result<PgUtilityStatement, SqlFrontendError> {
    let rest = tail.trim();
    if rest == "*" {
        return Ok(PgUtilityStatement::Unlisten { channel: None });
    }
    let (channel, trailing) = take_name(rest)?;
    if !trailing.trim().is_empty() {
        return Err(syntax("UNLISTEN accepts one channel name or `*`"));
    }
    validate_channel(&channel)?;
    Ok(PgUtilityStatement::Unlisten { channel: Some(channel) })
}

fn parse_notify(tail: &str) -> Result<PgUtilityStatement, SqlFrontendError> {
    let (channel, rest) = take_name(tail.trim_start())?;
    validate_channel(&channel)?;
    let rest = rest.trim_start();
    if rest.is_empty() {
        return Ok(PgUtilityStatement::Notify { channel, payload: String::new() });
    }
    let Some(inner) = rest.strip_prefix(',')
    else {
        return Err(syntax("NOTIFY accepts `channel [, 'payload']`"));
    };
    let inner = inner.trim_start();
    let Some(literal) = inner.strip_prefix('\'')
    else {
        return Err(syntax("NOTIFY payload must be a string literal"));
    };
    let Some(end) = literal.rfind('\'')
    else {
        return Err(syntax("unterminated NOTIFY payload"));
    };
    if !literal[end + 1..].trim().is_empty() {
        return Err(syntax("NOTIFY accepts `channel [, 'payload']`"));
    }
    Ok(PgUtilityStatement::Notify { channel, payload: literal[..end].replace("''", "'") })
}

/// Channel names follow PostgreSQL's identifier length (`NAMEDATALEN - 1`).
fn validate_channel(channel: &str) -> Result<(), SqlFrontendError> {
    if channel.is_empty() || channel.len() > 63 {
        return Err(syntax("notification channel must be 1 to 63 bytes"));
    }
    Ok(())
}

fn parse_explain(tail: &str) -> Result<PgUtilityStatement, SqlFrontendError> {
    let mut rest = tail.trim_start();
    let mut analyze = false;
    if let Some(inner) = rest.strip_prefix('(') {
        let close = matching_paren(inner)?;
        for option in split_top_level(&inner[..close]) {
            let name = option.split_whitespace().next().unwrap_or("").to_ascii_uppercase();
            match name.as_str() {
                "ANALYZE" | "ANALYSE" => {
                    analyze = true;
                }
                "VERBOSE" | "COSTS" | "BUFFERS" | "TIMING" | "SUMMARY" | "SETTINGS" | "FORMAT" | "WAL" | "GENERIC_PLAN" => {}
                other => return Err(syntax(format!("unsupported EXPLAIN option `{other}`"))),
            }
        }
        rest = inner[close + 1..].trim_start();
    }
    else if let Some(after) = strip_keyword(rest, "ANALYZE").or_else(|| strip_keyword(rest, "ANALYSE")) {
        analyze = true;
        rest = after.trim_start();
    }
    if rest.trim().is_empty() {
        return Err(syntax("EXPLAIN requires a statement"));
    }
    Ok(PgUtilityStatement::Explain { analyze, sql: rest.trim().to_owned() })
}

fn parse_declare(tail: &str) -> Result<PgUtilityStatement, SqlFrontendError> {
    let (name, rest) = take_name(tail.trim_start())?;
    let mut rest = rest.trim_start();
    // Optional cursor attributes before `CURSOR`.
    loop {
        if let Some(after) = strip_keyword(rest, "BINARY") {
            rest = after.trim_start();
        }
        else if let Some(after) = strip_keyword(rest, "INSENSITIVE") {
            rest = after.trim_start();
        }
        else if let Some(after) = strip_keyword(rest, "ASENSITIVE") {
            rest = after.trim_start();
        }
        else if let Some(after) = strip_keyword(rest, "SCROLL") {
            rest = after.trim_start();
        }
        else if let Some(after) = strip_keyword(rest, "NO") {
            // `NO SCROLL`
            rest = strip_keyword(after, "SCROLL").ok_or_else(|| syntax("DECLARE only accepts `NO SCROLL` after `NO`"))?.trim_start();
        }
        else {
            break;
        }
    }
    let Some(rest) = strip_keyword(rest, "CURSOR")
    else {
        return Err(syntax("DECLARE requires the `CURSOR` keyword"));
    };
    let mut rest = rest.trim_start();
    let with_hold = if let Some(after) = strip_keyword(rest, "WITH") {
        let after = strip_keyword(after, "HOLD").ok_or_else(|| syntax("DECLARE only accepts `WITH HOLD` after `WITH`"))?;
        rest = after.trim_start();
        true
    }
    else if let Some(after) = strip_keyword(rest, "WITHOUT") {
        rest = strip_keyword(after, "HOLD").ok_or_else(|| syntax("DECLARE only accepts `WITHOUT HOLD` after `WITHOUT`"))?.trim_start();
        false
    }
    else {
        false
    };
    let Some(sql) = strip_keyword(rest, "FOR")
    else {
        return Err(syntax("DECLARE CURSOR requires `FOR <query>`"));
    };
    if sql.trim().is_empty() {
        return Err(syntax("DECLARE CURSOR requires a query after `FOR`"));
    }
    Ok(PgUtilityStatement::DeclareCursor { name, with_hold, sql: sql.trim().to_owned() })
}

fn parse_fetch(tail: &str) -> Result<PgUtilityStatement, SqlFrontendError> {
    let mut rest = tail.trim();
    let direction = if let Some(after) = strip_keyword(rest, "NEXT") {
        rest = after.trim_start();
        FetchDirection::Next
    }
    else if let Some(after) = strip_keyword(rest, "ALL") {
        rest = after.trim_start();
        FetchDirection::All
    }
    else if let Some(after) = strip_keyword(rest, "FORWARD") {
        let after = after.trim_start();
        if let Some(after_all) = strip_keyword(after, "ALL") {
            rest = after_all.trim_start();
            FetchDirection::All
        }
        else {
            let (count, after_count) = take_signed_integer(after)?;
            rest = after_count.trim_start();
            FetchDirection::Forward(count)
        }
    }
    else if let Some(after) = strip_keyword(rest, "ABSOLUTE") {
        let (count, after_count) = take_signed_integer(after)?;
        rest = after_count.trim_start();
        FetchDirection::Absolute(count)
    }
    else if let Some(after) = strip_keyword(rest, "RELATIVE") {
        let (count, after_count) = take_signed_integer(after)?;
        rest = after_count.trim_start();
        FetchDirection::Relative(count)
    }
    else if rest.starts_with(|character: char| character.is_ascii_digit() || character == '-' || character == '+') {
        let (count, after_count) = take_signed_integer(rest)?;
        rest = after_count.trim_start();
        FetchDirection::Forward(count)
    }
    else {
        FetchDirection::Next
    };
    // Optional `FROM` / `IN`.
    let rest = strip_keyword(rest, "FROM").or_else(|| strip_keyword(rest, "IN")).unwrap_or(rest).trim();
    let (name, trailing) = take_name(rest)?;
    if !trailing.trim().is_empty() {
        return Err(syntax("FETCH accepts one cursor name"));
    }
    Ok(PgUtilityStatement::Fetch { name, direction })
}

fn parse_close(tail: &str) -> Result<PgUtilityStatement, SqlFrontendError> {
    let (name, trailing) = take_name(tail.trim())?;
    if !trailing.trim().is_empty() {
        return Err(syntax("CLOSE accepts one cursor name"));
    }
    Ok(PgUtilityStatement::CloseCursor { name })
}

/// Reads `[+|-]digits`, leaving the rest untouched.
fn take_signed_integer(source: &str) -> Result<(i64, &str), SqlFrontendError> {
    let trimmed = source.trim_start();
    let bytes = trimmed.as_bytes();
    let mut end = usize::from(bytes.first().is_some_and(|byte| *byte == b'+' || *byte == b'-'));
    let digits_start = end;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == digits_start {
        return Err(syntax("expected an integer after the fetch direction"));
    }
    let count = trimmed[..end].parse::<i64>().map_err(|_| syntax("fetch direction is out of range"))?;
    Ok((count, &trimmed[end..]))
}

fn parse_prepare(tail: &str) -> Result<PgUtilityStatement, SqlFrontendError> {
    let (name, rest) = take_name(tail.trim_start())?;
    let rest = rest.trim_start();
    let (parameter_types, rest) = if let Some(inner) = rest.strip_prefix('(') {
        let close = matching_paren(inner)?;
        (split_top_level(&inner[..close]), inner[close + 1..].trim_start())
    }
    else {
        (Vec::new(), rest)
    };
    let Some(sql) = strip_keyword(rest, "AS")
    else {
        return Err(syntax("PREPARE requires `AS <statement>`"));
    };
    if sql.trim().is_empty() {
        return Err(syntax("PREPARE requires a statement after `AS`"));
    }
    Ok(PgUtilityStatement::Prepare { name, parameter_types, sql: sql.trim().to_owned() })
}

fn parse_execute(tail: &str) -> Result<PgUtilityStatement, SqlFrontendError> {
    let (name, rest) = take_name(tail.trim_start())?;
    let rest = rest.trim_start();
    let arguments = if let Some(inner) = rest.strip_prefix('(') {
        let close = matching_paren(inner)?;
        if !inner[close + 1..].trim().is_empty() {
            return Err(syntax("EXECUTE does not accept a clause after its argument list"));
        }
        split_top_level(&inner[..close])
    }
    else if rest.is_empty() {
        Vec::new()
    }
    else {
        return Err(syntax("EXECUTE only accepts an optional parenthesised argument list"));
    };
    Ok(PgUtilityStatement::Execute { name, arguments })
}

fn parse_deallocate(tail: &str) -> Result<PgUtilityStatement, SqlFrontendError> {
    let rest = tail.trim();
    let rest = strip_keyword(rest, "PREPARE").unwrap_or(rest).trim();
    if rest.eq_ignore_ascii_case("ALL") {
        return Ok(PgUtilityStatement::Deallocate { name: None });
    }
    let (name, trailing) = take_name(rest)?;
    if !trailing.trim().is_empty() {
        return Err(syntax("DEALLOCATE accepts one statement name or `ALL`"));
    }
    Ok(PgUtilityStatement::Deallocate { name: Some(name) })
}

/// Reads a bare or double-quoted identifier.
fn take_name(source: &str) -> Result<(String, &str), SqlFrontendError> {
    if let Some(inner) = source.strip_prefix('"') {
        let Some(end) = inner.find('"')
        else {
            return Err(syntax("unterminated quoted name"));
        };
        return Ok((inner[..end].to_owned(), &inner[end + 1..]));
    }
    let end = source
        .find(|character: char| character.is_whitespace() || character == '(' || character == ',' || character == ')')
        .unwrap_or(source.len());
    if end == 0 {
        return Err(syntax("expected a statement name"));
    }
    Ok((source[..end].to_owned(), &source[end..]))
}

fn matching_paren(rest: &str) -> Result<usize, SqlFrontendError> {
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    for (index, character) in rest.char_indices() {
        if let Some(active) = quote {
            if character == active {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '(' => depth += 1,
            ')' => {
                if depth == 0 {
                    return Ok(index);
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    Err(syntax("unbalanced `(` in a utility statement"))
}

/// Splits on top-level commas, ignoring commas inside parentheses or quotes.
fn split_top_level(source: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    for character in source.chars() {
        if let Some(active) = quote {
            current.push(character);
            if character == active {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' => {
                quote = Some(character);
                current.push(character);
            }
            '(' => {
                depth += 1;
                current.push(character);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(character);
            }
            ',' if depth == 0 => {
                parts.push(current.trim().to_owned());
                current.clear();
            }
            _ => current.push(character),
        }
    }
    let tail = current.trim();
    if !tail.is_empty() || !parts.is_empty() {
        parts.push(tail.to_owned());
    }
    parts
}

fn first_word(source: &str) -> Option<&str> {
    let end = source.find(|character: char| !character.is_ascii_alphabetic()).unwrap_or(source.len());
    if end == 0 {
        return None;
    }
    Some(&source[..end])
}

/// Consumes `keyword` (case-insensitive) and returns the rest.
fn strip_keyword<'a>(source: &'a str, keyword: &str) -> Option<&'a str> {
    let trimmed = source.trim_start();
    let head = &trimmed[..trimmed.len().min(keyword.len())];
    if !head.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = &trimmed[keyword.len()..];
    if rest.starts_with(|character: char| character.is_alphanumeric() || character == '_') {
        return None;
    }
    Some(rest)
}

fn syntax(message: impl Into<String>) -> SqlFrontendError {
    SqlFrontendError::new(SqlErrorKind::Syntax, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_carries_its_types_and_statement() {
        assert_eq!(
            parse_pg_utility("PREPARE p (int, text) AS SELECT $1, $2").unwrap(),
            Some(PgUtilityStatement::Prepare {
                name: "p".to_owned(),
                parameter_types: vec!["int".to_owned(), "text".to_owned()],
                sql: "SELECT $1, $2".to_owned(),
            })
        );
        assert_eq!(
            parse_pg_utility("prepare p as select 1").unwrap(),
            Some(PgUtilityStatement::Prepare { name: "p".to_owned(), parameter_types: Vec::new(), sql: "select 1".to_owned() })
        );
    }

    #[test]
    fn execute_keeps_nested_commas_inside_one_argument() {
        assert_eq!(
            parse_pg_utility("EXECUTE p (1, 'a,b', (2 + 3))").unwrap(),
            Some(PgUtilityStatement::Execute {
                name: "p".to_owned(),
                arguments: vec!["1".to_owned(), "'a,b'".to_owned(), "(2 + 3)".to_owned()],
            })
        );
        assert_eq!(parse_pg_utility("EXECUTE p").unwrap(), Some(PgUtilityStatement::Execute { name: "p".to_owned(), arguments: Vec::new() }));
    }

    #[test]
    fn deallocate_accepts_one_name_or_all() {
        assert_eq!(parse_pg_utility("DEALLOCATE PREPARE p").unwrap(), Some(PgUtilityStatement::Deallocate { name: Some("p".to_owned()) }));
        assert_eq!(parse_pg_utility("DEALLOCATE ALL").unwrap(), Some(PgUtilityStatement::Deallocate { name: None }));
    }

    #[test]
    fn non_utility_statements_are_not_claimed() {
        assert_eq!(parse_pg_utility("SELECT 1").unwrap(), None);
        assert_eq!(parse_pg_utility("PREPARE").is_err(), true);
        assert_eq!(parse_pg_utility("PREPARE p SELECT 1").is_err(), true);
        assert_eq!(parse_pg_utility("EXECUTE p (1) junk").is_err(), true);
    }
}
