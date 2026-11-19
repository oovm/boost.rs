//! Rewrites of PostgreSQL's temporary-schema qualifier.
//!
//! `pg_temp` (and its real spelling `pg_temp_<n>`) names the session's
//! temporary schema. The shared binder resolves a bare table name, so a
//! reference the session's temporary registry owns is lowered onto that bare
//! name here. A qualifier naming a table the session does not own is left
//! untouched, which keeps the binder's `42P01` answer for an unknown relation
//! instead of silently resolving it against the persistent schema.

/// Rewrites `pg_temp.<table>` / `pg_temp_<n>.<table>` into `<table>` for every
/// table `is_temp_table` reports as owned by the session.
///
/// String literals, quoted identifiers and SQL comments are copied verbatim, so
/// a channel name or a payload that happens to contain the qualifier is never
/// rewritten.
pub fn rewrite_pg_temp_references(source: &str, is_temp_table: impl Fn(&str) -> bool) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        // Single-quoted string literal, including doubled quotes.
        if byte == b'\'' {
            let start = index;
            index += 1;
            while index < bytes.len() {
                if bytes[index] == b'\'' {
                    if bytes.get(index + 1) == Some(&b'\'') {
                        index += 2;
                        continue;
                    }
                    index += 1;
                    break;
                }
                index += 1;
            }
            out.push_str(&source[start..index]);
            continue;
        }
        // Double-quoted identifier.
        if byte == b'"' {
            let start = index;
            index += 1;
            while index < bytes.len() {
                if bytes[index] == b'"' {
                    if bytes.get(index + 1) == Some(&b'"') {
                        index += 2;
                        continue;
                    }
                    index += 1;
                    break;
                }
                index += 1;
            }
            out.push_str(&source[start..index]);
            continue;
        }
        // Line and block comments.
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
        // Candidate identifier.
        if byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$' {
            let (identifier, next) = take_identifier(source, index);
            if is_pg_temp_schema(identifier) && bytes.get(next) == Some(&b'.') {
                let (table, after) = take_identifier(source, next + 1);
                if !table.is_empty() && is_temp_table(table) {
                    out.push_str(table);
                    index = after;
                    continue;
                }
            }
            out.push_str(identifier);
            index = next;
            continue;
        }
        // Copy one full character so multi-byte text stays intact.
        let character = source[index..].chars().next().expect("index is inside the string");
        out.push(character);
        index += character.len_utf8();
    }
    out
}

/// Copies the next bare or double-quoted identifier starting at `start`.
fn take_identifier(source: &str, start: usize) -> (&str, usize) {
    let bytes = source.as_bytes();
    if bytes.get(start) == Some(&b'"') {
        let mut index = start + 1;
        while index < bytes.len() {
            if bytes[index] == b'"' {
                if bytes.get(index + 1) == Some(&b'"') {
                    index += 2;
                    continue;
                }
                return (&source[start + 1..index], index + 1);
            }
            index += 1;
        }
        return (&source[start + 1..], bytes.len());
    }
    let mut index = start;
    while index < bytes.len() && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_' || bytes[index] == b'$') {
        index += 1;
    }
    (&source[start..index], index)
}

/// Whether `name` is the session's temporary schema: `pg_temp` or
/// `pg_temp_<digits>`.
fn is_pg_temp_schema(name: &str) -> bool {
    super::super::sql_ddl::is_pg_temp_schema(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_qualified_temp_reference_is_reduced_to_the_bare_name() {
        let sql = rewrite_pg_temp_references("SELECT * FROM pg_temp.staged WHERE pg_temp.staged.id > 1", |name| name == "staged");
        assert_eq!(sql, "SELECT * FROM staged WHERE staged.id > 1");
        assert_eq!(
            rewrite_pg_temp_references("INSERT INTO pg_temp_12.staged (id) VALUES (1)", |_| true),
            "INSERT INTO staged (id) VALUES (1)"
        );
    }

    #[test]
    fn unknown_tables_and_non_sql_text_keep_their_qualifier() {
        assert_eq!(rewrite_pg_temp_references("SELECT * FROM pg_temp.absent", |_| false), "SELECT * FROM pg_temp.absent");
        assert_eq!(
            rewrite_pg_temp_references("SELECT 'pg_temp.staged' FROM pg_temp.staged -- pg_temp.staged", |name| name == "staged"),
            "SELECT 'pg_temp.staged' FROM staged -- pg_temp.staged"
        );
        assert_eq!(
            rewrite_pg_temp_references("SELECT \"pg_temp\".\"staged\" FROM pg_temp.\"staged\"", |name| name == "staged"),
            "SELECT \"pg_temp\".\"staged\" FROM staged"
        );
    }
}
