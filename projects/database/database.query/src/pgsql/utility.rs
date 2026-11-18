//! PostgreSQL utility statements the gateway executes itself.
//!
//! `PREPARE` / `EXECUTE` / `DEALLOCATE` (and later the cursor and notification
//! statements) never reach the Oak binder: they are session commands whose
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
        _ => Ok(None),
    }
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
        let Some(end) = inner.find('"') else {
            return Err(syntax("unterminated quoted name"));
        };
        return Ok((inner[..end].to_owned(), &inner[end + 1..]));
    }
    let end = source.find(|character: char| character.is_whitespace() || character == '(').unwrap_or(source.len());
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
        assert_eq!(
            parse_pg_utility("EXECUTE p").unwrap(),
            Some(PgUtilityStatement::Execute { name: "p".to_owned(), arguments: Vec::new() })
        );
    }

    #[test]
    fn deallocate_accepts_one_name_or_all() {
        assert_eq!(
            parse_pg_utility("DEALLOCATE PREPARE p").unwrap(),
            Some(PgUtilityStatement::Deallocate { name: Some("p".to_owned()) })
        );
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
