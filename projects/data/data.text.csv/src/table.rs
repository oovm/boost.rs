//! Delimited record parsing and escaping for tabular consumers.
//!
//! The language pipeline above produces a syntax tree; spreadsheet products
//! instead need two plain answers: how a byte stream splits into quoted fields,
//! and how one field is written back with the same quoting rules. Both live here
//! so CSV and TSV importers share one implementation instead of copying the
//! quote state machine per product.

/// One parsed field with its quoting state.
///
/// `quoted` records whether the author wrote the field inside quotes. Callers
/// use it to keep a quoted numeric string as text instead of inferring a number.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Field {
    /// Decoded field text.
    pub text: String,
    /// Whether the field was written inside quotes.
    pub quoted: bool,
}

impl Field {
    /// Builds a decoded field.
    pub fn new(text: impl Into<String>, quoted: bool) -> Self {
        Self { text: text.into(), quoted }
    }
}

/// Splits delimited text into records.
///
/// A leading byte-order mark is ignored, `""` inside a quoted field becomes one
/// quote, quoted fields keep embedded delimiters and line breaks, `CRLF`, `CR`
/// and `LF` all end a record, and blank lines are skipped.
pub fn parse_records(text: &str, delimiter: char) -> Vec<Vec<Field>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut records = Vec::new();
    let mut record: Vec<Field> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut in_quotes = false;
    let mut characters = text.chars().peekable();

    while let Some(character) = characters.next() {
        if in_quotes {
            if character == '"' {
                if characters.peek() == Some(&'"') {
                    characters.next();
                    field.push('"');
                }
                else {
                    in_quotes = false;
                }
            }
            else {
                field.push(character);
            }
            continue;
        }
        if character == '"' && field.is_empty() {
            in_quotes = true;
            quoted = true;
        }
        else if character == delimiter {
            record.push(Field { text: std::mem::take(&mut field), quoted });
            quoted = false;
        }
        else if character == '\r' || character == '\n' {
            if character == '\r' && characters.peek() == Some(&'\n') {
                characters.next();
            }
            record.push(Field { text: std::mem::take(&mut field), quoted });
            quoted = false;
            push_record(&mut records, std::mem::take(&mut record));
        }
        else {
            field.push(character);
        }
    }

    if !field.is_empty() || quoted || !record.is_empty() {
        record.push(Field { text: field, quoted });
        push_record(&mut records, record);
    }
    records
}

fn push_record(records: &mut Vec<Vec<Field>>, record: Vec<Field>) {
    let blank = record.len() == 1 && record[0].text.is_empty() && !record[0].quoted;
    if !blank {
        records.push(record);
    }
}

/// Quotes a field when it contains the delimiter, a quote or a line break.
pub fn escape_field(text: &str, delimiter: char) -> String {
    if text.contains(delimiter) || text.contains('"') || text.contains('\n') || text.contains('\r') {
        format!("\"{}\"", text.replace('"', "\"\""))
    }
    else {
        text.to_owned()
    }
}

/// Serializes rows as delimited text with a trailing newline per row.
///
/// Rows are written verbatim: an empty trailing column is preserved instead of
/// being trimmed, so a rectangle round-trips through [`parse_records`].
pub fn write_records(rows: &[Vec<String>], delimiter: char) -> String {
    let mut output = String::new();
    for row in rows {
        let fields = row.iter().map(|field| escape_field(field, delimiter)).collect::<Vec<_>>();
        output.push_str(&fields.join(&delimiter.to_string()));
        output.push('\n');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_quotes_escapes_and_embedded_newlines() {
        let records = parse_records("name,note\r\n\"Doe, Jane\",\"line 1\nline 2\"\nplain,\"say \"\"hi\"\"\"\n", ',');
        assert_eq!(records.len(), 3);
        assert_eq!(records[0][0].text, "name");
        assert_eq!(records[1][0].text, "Doe, Jane");
        assert_eq!(records[1][1].text, "line 1\nline 2");
        assert_eq!(records[2][1].text, "say \"hi\"");
        assert!(records[2][1].quoted);
    }

    #[test]
    fn keeps_quoted_state_for_number_columns() {
        let records = parse_records("id,amount\n1,\"2.5\"\n2,3.5\n", ',');
        assert!(records[1][1].quoted);
        assert!(!records[2][1].quoted);
    }

    #[test]
    fn writes_escaped_fields_and_round_trips() {
        let rows = vec![vec!["a".to_owned()], vec!["x,y".to_owned()], vec!["say \"hi\"".to_owned()]];
        let text = write_records(&rows, ',');
        assert_eq!(text, "a\n\"x,y\"\n\"say \"\"hi\"\"\"\n");
        let parsed = parse_records(&text, ',');
        assert_eq!(parsed[1][0].text, "x,y");
        assert_eq!(parsed[2][0].text, "say \"hi\"");
    }

    #[test]
    fn skips_blank_lines_and_honours_the_bom() {
        let records = parse_records("\u{feff}a,b\n\n\nc,d\n", ',');
        assert_eq!(records.len(), 2);
        assert_eq!(records[0][0].text, "a");
    }

    #[test]
    fn round_trips_empty_and_quoted_empty_fields() {
        let rows = vec![vec!["a".to_owned(), String::new()], vec![String::new(), String::new()]];
        let text = write_records(&rows, ',');
        assert_eq!(text, "a,\n,\n");
        let parsed = parse_records(&text, ',');
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].len(), 2);
        assert_eq!(parsed[1][0].text, "");
        assert!(!parsed[1][0].quoted);
        let quoted = parse_records("\"\",x\n", ',');
        assert_eq!(quoted.len(), 1);
        assert!(quoted[0][0].quoted);
        assert_eq!(quoted[0][0].text, "");
    }
}
