//! PostgreSQL `COPY ... FROM STDIN` decoding.
//!
//! The protocol session owns the `CopyIn` frame loop; this module owns the data
//! semantics: turning the accumulated `TEXT` or `CSV` stream into rows and
//! inserting them through the same bind → plan → execute path every other
//! statement uses. A malformed row is reported as `22P04` instead of being
//! skipped.

use std::sync::Arc;

use database_query::{CopyFormat, PgUtilityStatement};

use super::session::{PgsqlSqlRuntime, TransactionSession};
use super::statement::{StatementError, run_statement};

/// One `COPY ... FROM STDIN` target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyInPlan {
    /// Target table.
    pub table: String,
    /// Target columns; empty means the table's catalog order.
    pub columns: Vec<String>,
    /// Input format.
    pub format: CopyFormat,
    /// Field delimiter.
    pub delimiter: char,
    /// Whether the first line is a header to skip.
    pub header: bool,
}

impl CopyInPlan {
    /// Extracts the plan from a parsed utility statement.
    pub fn from_utility(utility: &PgUtilityStatement) -> Option<Self> {
        match utility {
            PgUtilityStatement::CopyFromStdin { table, columns, format, delimiter, header } => {
                Some(Self { table: table.clone(), columns: columns.clone(), format: *format, delimiter: *delimiter, header: *header })
            }
            _ => None,
        }
    }
}

/// Accumulates `CopyData` bytes and inserts the rows when the client sends
/// `CopyDone`.
#[derive(Debug)]
pub struct CopyInState {
    plan: CopyInPlan,
    buffer: Vec<u8>,
    header_pending: bool,
}

impl CopyInState {
    /// Starts one copy-in stream.
    pub fn new(plan: CopyInPlan) -> Self {
        let header_pending = plan.header;
        Self { plan, buffer: Vec::new(), header_pending }
    }

    /// Appends one `CopyData` payload.
    pub fn push(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    /// Number of bytes received so far.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Whether nothing has been received yet.
    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Parses the stream and inserts every row, returning the affected count.
    pub fn finish(mut self, runtime: &Arc<PgsqlSqlRuntime>, session: &mut TransactionSession) -> Result<u64, StatementError> {
        let text = String::from_utf8(std::mem::take(&mut self.buffer))
            .map_err(|_| StatementError::new("22P04", "COPY data is not valid UTF-8"))?;
        let columns = self.resolve_columns(runtime)?;
        let mut rows = Vec::new();
        for line in text.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if line.is_empty() {
                continue;
            }
            if self.header_pending {
                self.header_pending = false;
                continue;
            }
            let fields = match self.plan.format {
                CopyFormat::Csv => parse_csv_line(line, self.plan.delimiter)?,
                CopyFormat::Text => parse_text_line(line, self.plan.delimiter)?,
            };
            if fields.len() != columns.len() {
                return Err(StatementError::new(
                    "22P04",
                    format!(
                        "missing data for column \"{}\" in COPY line: expected {} fields, found {}",
                        columns.get(fields.len()).map(String::as_str).unwrap_or("?"),
                        columns.len(),
                        fields.len()
                    ),
                ));
            }
            rows.push(fields);
        }
        let mut inserted = 0u64;
        for row in rows {
            let values: Vec<String> = row.iter().map(|field| render_literal(field)).collect();
            let sql = format!(
                "INSERT INTO \"{}\" ({}) VALUES ({})",
                self.plan.table,
                columns.iter().map(|column| format!("\"{column}\"")).collect::<Vec<_>>().join(", "),
                values.join(", ")
            );
            let outcome = run_statement(&sql, runtime, session).map_err(|(sqlstate, message)| StatementError::new(sqlstate, message))?;
            inserted += outcome.command_tag.rsplit(' ').next().and_then(|tail| tail.parse::<u64>().ok()).unwrap_or(0);
        }
        Ok(inserted)
    }

    /// Target column names, using the catalog order when `COPY` names none.
    fn resolve_columns(&self, runtime: &Arc<PgsqlSqlRuntime>) -> Result<Vec<String>, StatementError> {
        if !self.plan.columns.is_empty() {
            return Ok(self.plan.columns.clone());
        }
        let catalog = runtime.schema.catalog();
        catalog
            .tables()
            .iter()
            .find(|table| table.name == self.plan.table)
            .map(|table| table.fields.iter().map(|field| field.name.clone()).collect())
            .ok_or_else(|| StatementError::new("42P01", format!("relation \"{}\" does not exist", self.plan.table)))
    }
}

/// Renders one COPY field as a SQL literal.
fn render_literal(field: &Option<String>) -> String {
    match field {
        None => "NULL".to_owned(),
        Some(text) => format!("'{}'", text.replace('\'', "''")),
    }
}

/// Parses one `TEXT` line: `\N` is NULL and backslash escapes are decoded.
fn parse_text_line(line: &str, delimiter: char) -> Result<Vec<Option<String>>, StatementError> {
    let mut fields = Vec::new();
    let mut current = String::new();
    // A field is NULL only when `\N` is its entire content.
    let mut null_field = false;
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\\' {
            match characters.next() {
                Some('N') if current.is_empty() && !null_field => {
                    let at_field_end = characters.peek().is_none() || characters.peek() == Some(&delimiter);
                    if at_field_end {
                        null_field = true;
                    }
                    else {
                        current.push('N');
                    }
                }
                Some('t') => current.push('\t'),
                Some('n') => current.push('\n'),
                Some('r') => current.push('\r'),
                Some('b') => current.push('\u{8}'),
                Some('f') => current.push('\u{c}'),
                Some('\\') => current.push('\\'),
                Some(other) => current.push(other),
                None => {}
            }
            continue;
        }
        if character == delimiter {
            fields.push(if null_field { None } else { Some(std::mem::take(&mut current)) });
            null_field = false;
            continue;
        }
        current.push(character);
    }
    fields.push(if null_field { None } else { Some(current) });
    Ok(fields)
}

/// Parses one RFC 4180 CSV line.
fn parse_csv_line(line: &str, delimiter: char) -> Result<Vec<Option<String>>, StatementError> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut was_quoted = false;
    let mut characters = line.chars().peekable();
    while let Some(character) = characters.next() {
        if quoted {
            if character == '"' {
                if characters.peek() == Some(&'"') {
                    characters.next();
                    current.push('"');
                }
                else {
                    quoted = false;
                }
            }
            else {
                current.push(character);
            }
            continue;
        }
        match character {
            '"' if current.is_empty() && !was_quoted => {
                quoted = true;
                was_quoted = true;
            }
            character if character == delimiter => {
                fields.push(csv_field(std::mem::take(&mut current), was_quoted));
                was_quoted = false;
            }
            other => current.push(other),
        }
    }
    if quoted {
        return Err(StatementError::new("22P04", "unterminated quoted field in COPY CSV data"));
    }
    fields.push(csv_field(current, was_quoted));
    Ok(fields)
}

fn csv_field(text: String, quoted: bool) -> Option<String> {
    // PostgreSQL treats an unquoted `\N` as NULL and a quoted `"\N"` as text.
    if !quoted && text == "\\N" { None } else { Some(text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_lines_decode_escapes_and_null() {
        assert_eq!(
            parse_text_line("1\tab\\tc\t\\N", '\t').unwrap(),
            vec![Some("1".to_owned()), Some("ab\tc".to_owned()), None]
        );
    }

    #[test]
    fn csv_lines_honour_quotes_and_escaped_quotes() {
        assert_eq!(
            parse_csv_line("1,\"a,b\",\"say \"\"hi\"\"\",\\N", ',').unwrap(),
            vec![Some("1".to_owned()), Some("a,b".to_owned()), Some("say \"hi\"".to_owned()), None]
        );
    }

    #[test]
    fn an_unterminated_csv_quote_is_a_bad_copy_format() {
        let error = parse_csv_line("\"a", ',').unwrap_err();
        assert_eq!(error.sqlstate, "22P04");
    }
}
