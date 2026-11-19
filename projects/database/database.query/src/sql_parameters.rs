//! 参数占位符替换。
//!
//! `data.text` 的 SQL AST 没有占位符节点，因此参数必须先按字面量渲染回 SQL 文本，
//! 再交给 `bind_sql_catalog`。替换只识别字符串、标识符、注释之外的位置，
//! 不会改写已有字面量内容。SQL 语义本身仍由 planner 和 executor 决定。

use crate::{
    catalog_bind::{SqlCatalog, SqlFieldType},
    sql::{SqlErrorKind, SqlFrontendError},
    sql_execute::{SqlValue, format_number},
};

/// 参数占位符风格。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlParameterStyle {
    /// MySQL 风格 `?`，按出现顺序绑定。
    QuestionMark,
    /// PostgreSQL 风格 `$1`，按一基序号绑定。
    DollarNumber,
}

impl SqlParameterStyle {
    fn label(self) -> &'static str {
        match self {
            Self::QuestionMark => "`?`",
            Self::DollarNumber => "`$n`",
        }
    }
}

/// 把参数按风格渲染成 SQL 字面量并替换占位符。
///
/// 替换只发生在代码位置：单引号字符串、双引号标识符、反引号标识符、
/// `--` 行注释和 `/* */` 块注释内的 `?` / `$n` 保持原样。
pub fn substitute_sql_parameters(sql: &str, parameters: &[SqlValue], style: SqlParameterStyle) -> Result<String, SqlFrontendError> {
    let (out, referenced) = walk_parameters(sql, style, &mut |index| render_parameter(parameters, index, style))?;
    if referenced < parameters.len() {
        return Err(parameter_error(&format!("{} parameters were supplied but only {referenced} placeholders were referenced", style.label())));
    }
    Ok(out)
}

/// Renders every referenced placeholder as the literal `0` and reports how
/// many positions the statement references. Used to derive statement metadata
/// (parameter count, row description) without bound values. A numeric literal
/// keeps `WHERE pk = $1` plannable, unlike `NULL`.
pub fn neutralized_sql_parameters(sql: &str, style: SqlParameterStyle) -> Result<(String, usize), SqlFrontendError> {
    walk_parameters(sql, style, &mut |_| Ok("0".to_string()))
}

/// Renders every referenced placeholder as `NULL` for metadata-only describes
/// whose target column rejects the `0` stand-in, since every column type
/// accepts `NULL`.
pub fn nulled_sql_parameters(sql: &str, style: SqlParameterStyle) -> Result<(String, usize), SqlFrontendError> {
    walk_parameters(sql, style, &mut |_| Ok("NULL".to_string()))
}

fn walk_parameters(
    sql: &str,
    style: SqlParameterStyle,
    render: &mut dyn FnMut(usize) -> Result<String, SqlFrontendError>,
) -> Result<(String, usize), SqlFrontendError> {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(sql.len() + 16);
    let mut index = 0;
    let mut referenced = 0usize;

    while index < chars.len() {
        let current = chars[index];
        match current {
            '\'' | '"' | '`' => {
                index = copy_quoted(&chars, index, current, &mut out);
            }
            '-' if chars.get(index + 1) == Some(&'-') => {
                while index < chars.len() && chars[index] != '\n' {
                    out.push(chars[index]);
                    index += 1;
                }
            }
            '/' if chars.get(index + 1) == Some(&'*') => {
                out.push('/');
                out.push('*');
                index += 2;
                while index < chars.len() {
                    if chars[index] == '*' && chars.get(index + 1) == Some(&'/') {
                        out.push('*');
                        out.push('/');
                        index += 2;
                        break;
                    }
                    out.push(chars[index]);
                    index += 1;
                }
            }
            '?' if style == SqlParameterStyle::QuestionMark => {
                out.push_str(&render(referenced)?);
                referenced += 1;
                index += 1;
            }
            '$' if style == SqlParameterStyle::DollarNumber && chars.get(index + 1).is_some_and(char::is_ascii_digit) => {
                let start = index + 1;
                let mut end = start;
                while end < chars.len() && chars[end].is_ascii_digit() {
                    end += 1;
                }
                let ordinal = chars[start..end]
                    .iter()
                    .collect::<String>()
                    .parse::<usize>()
                    .map_err(|_| parameter_error("SQL parameter index is not a valid integer"))?;
                if ordinal == 0 {
                    return Err(parameter_error("SQL parameter indexes are one-based"));
                }
                out.push_str(&render(ordinal - 1)?);
                referenced = referenced.max(ordinal);
                index = end;
            }
            _ => {
                out.push(current);
                index += 1;
            }
        }
    }

    Ok((out, referenced))
}

fn copy_quoted(chars: &[char], mut index: usize, quote: char, out: &mut String) -> usize {
    out.push(quote);
    index += 1;
    while index < chars.len() {
        let current = chars[index];
        out.push(current);
        index += 1;
        if current == '\\' && quote == '\'' {
            // MySQL 风格反斜杠转义：吞掉下一个字符，避免把 `\'` 当成字符串结束。
            if index < chars.len() {
                out.push(chars[index]);
                index += 1;
            }
            continue;
        }
        if current == quote {
            if chars.get(index) == Some(&quote) {
                out.push(quote);
                index += 1;
                continue;
            }
            break;
        }
    }
    index
}

fn render_parameter(parameters: &[SqlValue], index: usize, style: SqlParameterStyle) -> Result<String, SqlFrontendError> {
    let Some(value) = parameters.get(index)
    else {
        return Err(parameter_error(&format!("{} placeholder {} has no bound parameter", style.label(), index + 1)));
    };
    Ok(match value {
        SqlValue::Integer(value) => value.to_string(),
        SqlValue::Number(value) if value.is_finite() => format_number(*value),
        SqlValue::Number(_) => return Err(parameter_error("non-finite floating parameters are not supported")),
        SqlValue::Boolean(value) => (if *value { "TRUE" } else { "FALSE" }).to_string(),
        SqlValue::String(value) => {
            let mut literal = String::with_capacity(value.len() + 2);
            literal.push('\'');
            for character in value.chars() {
                if character == '\'' {
                    literal.push('\'');
                }
                literal.push(character);
            }
            literal.push('\'');
            literal
        }
        SqlValue::Bytes(value) => {
            // 原始 octets 无法放进 SQL 文本的字符串字面量，只有十六进制 CAST 能承载它们。
            // PostgreSQL 用 `bytea` 的 `\x` 拼写，MySQL 用裸十六进制 `BLOB` 拼写。
            let hex = crate::sql::encode_hex(value);
            match style {
                SqlParameterStyle::DollarNumber => format!("CAST('\\x{hex}' AS BYTEA)"),
                SqlParameterStyle::QuestionMark => format!("CAST('{hex}' AS BLOB)"),
            }
        }
        SqlValue::Null => "NULL".to_string(),
    })
}

fn parameter_error(message: &str) -> SqlFrontendError {
    SqlFrontendError::new(SqlErrorKind::Parameter, message)
}

/// Infers one catalog column type per placeholder slot of a statement.
///
/// Slot `i` is zero-based: it belongs to `$ (i + 1)` for
/// [`SqlParameterStyle::DollarNumber`] and to the `i`-th `?` for
/// [`SqlParameterStyle::QuestionMark`]. The text scan covers the shapes a
/// client can bind against a catalog: `INSERT ... (columns) VALUES (placeholders)`
/// maps placeholders positionally and `column = placeholder` (in either
/// order) maps a placeholder to the column on either side. Slots outside
/// those shapes, unknown columns, and placeholders inside string literals,
/// `--` comments or `/* */` comments stay `None`.
pub fn infer_sql_parameter_types(sql: &str, style: SqlParameterStyle, catalog: &SqlCatalog) -> Vec<Option<SqlFieldType>> {
    let tokens = scan_context(sql, style);
    let mut inferred = Vec::new();

    if let Some((table, columns, values_at)) = insert_shape(&tokens) {
        let mut value_index = 0usize;
        // `values_at` points past the tuple's opening parenthesis.
        let mut depth = 1usize;
        for token in &tokens[values_at..] {
            match token {
                ContextToken::Open => depth += 1,
                ContextToken::Close => {
                    if depth <= 1 {
                        break;
                    }
                    depth -= 1;
                }
                ContextToken::Comma if depth == 1 => value_index += 1,
                ContextToken::Placeholder(slot) if depth == 1 => {
                    if let Some(column) = columns.get(value_index) {
                        assign_inferred(&mut inferred, *slot, column_type(catalog, Some(&table), column));
                    }
                }
                _ => {}
            }
        }
    }

    for (position, token) in tokens.iter().enumerate() {
        let Some(column) = identifier(token)
        else {
            continue;
        };
        let pair = match tokens.get(position + 1) {
            Some(ContextToken::Equals) => match tokens.get(position + 2) {
                Some(ContextToken::Placeholder(slot)) => Some(*slot),
                _ => None,
            },
            Some(ContextToken::Placeholder(slot)) => Some(*slot),
            _ => None,
        };
        if let Some(slot) = pair {
            assign_inferred(&mut inferred, slot, column_type(catalog, None, column));
        }
    }

    for (position, token) in tokens.iter().enumerate() {
        let ContextToken::Placeholder(slot) = token
        else {
            continue;
        };
        if !matches!(tokens.get(position + 1), Some(ContextToken::Equals)) {
            continue;
        }
        let Some(column) = tokens.get(position + 2).and_then(identifier)
        else {
            continue;
        };
        assign_inferred(&mut inferred, *slot, column_type(catalog, None, column));
    }

    inferred
}

/// First inference wins: a slot already attributed to a column is never
/// overwritten by a later, looser scan.
fn assign_inferred(inferred: &mut Vec<Option<SqlFieldType>>, slot: usize, field_type: Option<SqlFieldType>) {
    if inferred.len() <= slot {
        inferred.resize(slot + 1, None);
    }
    if inferred[slot].is_none() {
        inferred[slot] = field_type;
    }
}

/// Column type for a named catalog column. `table` restricts the lookup to
/// one catalog table when the statement already named it.
fn column_type(catalog: &SqlCatalog, table: Option<&str>, column: &str) -> Option<SqlFieldType> {
    for candidate in catalog.tables() {
        if table.is_some_and(|name| name != candidate.name) {
            continue;
        }
        if let Some(field) = candidate.fields.iter().find(|field| field.name == column) {
            return Some(field.field_type.clone());
        }
    }
    None
}

/// A column or table name, quoted or bare. Bare identifiers reach the scanner
/// as [`ContextToken::Word`] because the same token class carries SQL keywords.
fn identifier(token: &ContextToken) -> Option<&str> {
    match token {
        ContextToken::Ident(name) | ContextToken::Word(name) => Some(name),
        _ => None,
    }
}

/// Token context for parameter type inference: quoted identifiers,
/// placeholders, and the punctuation that separates them. String literals and
/// comments produce no tokens at all, so nothing inside them is inferred.
#[derive(Debug, PartialEq, Eq)]
enum ContextToken {
    Ident(String),
    Word(String),
    Placeholder(usize),
    Equals,
    Comma,
    Open,
    Close,
    Other,
}

fn scan_context(sql: &str, style: SqlParameterStyle) -> Vec<ContextToken> {
    let chars: Vec<char> = sql.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    let mut question_marks = 0usize;
    while index < chars.len() {
        let current = chars[index];
        match current {
            '\'' => index = skip_string(&chars, index),
            '"' | '`' => {
                let (name, next) = read_quoted_identifier(&chars, index);
                tokens.push(ContextToken::Ident(name));
                index = next;
            }
            '-' if chars.get(index + 1) == Some(&'-') => {
                while index < chars.len() && chars[index] != '\n' {
                    index += 1;
                }
            }
            '/' if chars.get(index + 1) == Some(&'*') => index = skip_block_comment(&chars, index + 2),
            '$' if style == SqlParameterStyle::DollarNumber => {
                let start = index + 1;
                let mut end = start;
                while end < chars.len() && chars[end].is_ascii_digit() {
                    end += 1;
                }
                let ordinal = chars[start..end].iter().collect::<String>().parse::<usize>().ok().filter(|value| *value > 0);
                if let Some(ordinal) = ordinal {
                    tokens.push(ContextToken::Placeholder(ordinal - 1));
                    index = end;
                }
                else {
                    tokens.push(ContextToken::Other);
                    index += 1;
                }
            }
            '?' if style == SqlParameterStyle::QuestionMark => {
                tokens.push(ContextToken::Placeholder(question_marks));
                question_marks += 1;
                index += 1;
            }
            _ if current.is_alphanumeric() || current == '_' => {
                let mut word = String::new();
                while index < chars.len() && (chars[index].is_alphanumeric() || chars[index] == '_') {
                    word.push(chars[index]);
                    index += 1;
                }
                tokens.push(ContextToken::Word(word));
            }
            _ if current.is_whitespace() => index += 1,
            '=' => {
                tokens.push(ContextToken::Equals);
                index += 1;
            }
            ',' => {
                tokens.push(ContextToken::Comma);
                index += 1;
            }
            '(' => {
                tokens.push(ContextToken::Open);
                index += 1;
            }
            ')' => {
                tokens.push(ContextToken::Close);
                index += 1;
            }
            _ => {
                tokens.push(ContextToken::Other);
                index += 1;
            }
        }
    }
    tokens
}

fn skip_string(chars: &[char], mut index: usize) -> usize {
    index += 1;
    while index < chars.len() {
        let current = chars[index];
        index += 1;
        if current == '\\' {
            // MySQL 风格反斜杠转义：吞掉下一个字符，避免把 `\'` 当成字符串结束。
            if index < chars.len() {
                index += 1;
            }
            continue;
        }
        if current == '\'' {
            if chars.get(index) == Some(&'\'') {
                index += 1;
                continue;
            }
            break;
        }
    }
    index
}

fn read_quoted_identifier(chars: &[char], mut index: usize) -> (String, usize) {
    let quote = chars[index];
    index += 1;
    let mut name = String::new();
    while index < chars.len() {
        let current = chars[index];
        index += 1;
        if current == quote {
            if chars.get(index) == Some(&quote) {
                name.push(quote);
                index += 1;
                continue;
            }
            break;
        }
        name.push(current);
    }
    (name, index)
}

fn skip_block_comment(chars: &[char], mut index: usize) -> usize {
    while index < chars.len() {
        if chars[index] == '*' && chars.get(index + 1) == Some(&'/') {
            return index + 2;
        }
        index += 1;
    }
    index
}

/// `INSERT INTO "table" ("c1", "c2", ...) VALUES (...)` positions, returning
/// the target table, its column names in VALUES order, and the token index of
/// the first VALUES tuple's contents.
fn insert_shape(tokens: &[ContextToken]) -> Option<(String, Vec<String>, usize)> {
    let insert_at = tokens.iter().position(|token| matches!(token, ContextToken::Word(word) if word.eq_ignore_ascii_case("INSERT")))?;
    if !matches!(tokens.get(insert_at + 1), Some(ContextToken::Word(word)) if word.eq_ignore_ascii_case("INTO")) {
        return None;
    }
    let Some(table) = tokens.get(insert_at + 2).and_then(identifier)
    else {
        return None;
    };
    let mut columns = Vec::new();
    if matches!(tokens.get(insert_at + 3), Some(ContextToken::Open)) {
        for token in &tokens[insert_at + 4..] {
            match token {
                ContextToken::Close => break,
                other => {
                    if let Some(name) = identifier(other) {
                        columns.push(name.to_string());
                    }
                }
            }
        }
    }
    let values_at = tokens.iter().enumerate().skip(insert_at).find_map(|(position, token)| match token {
        ContextToken::Word(word) if word.eq_ignore_ascii_case("VALUES") => Some(position),
        _ => None,
    })?;
    if !matches!(tokens.get(values_at + 1), Some(ContextToken::Open)) {
        return None;
    }
    Some((table.to_string(), columns, values_at + 2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_bind::{SqlCatalogField, SqlCatalogTable};

    fn catalog() -> SqlCatalog {
        SqlCatalog::from_tables(
            1,
            vec![SqlCatalogTable {
                type_id: 1,
                name: "users".to_owned(),
                fields: vec![
                    SqlCatalogField {
                        field_id: 1,
                        name: "id".to_owned(),
                        field_type: SqlFieldType::Int64,
                        vos_type_name: None,
                        capacity: None,
                        default: None,
                        check: None,
                        generated: None,
                    },
                    SqlCatalogField {
                        field_id: 2,
                        name: "name".to_owned(),
                        field_type: SqlFieldType::String,
                        vos_type_name: None,
                        capacity: None,
                        default: None,
                        check: None,
                        generated: None,
                    },
                ],
                unique_constraints: Vec::new(),
                indexes: Vec::new(),
                check_constraints: Vec::new(),
                foreign_keys: Vec::new(),
            }],
        )
        .expect("test catalog validates")
    }

    #[test]
    fn insert_placeholders_map_to_column_types() {
        let inferred = infer_sql_parameter_types("INSERT INTO users (id, name) VALUES ($1, $2)", SqlParameterStyle::DollarNumber, &catalog());
        assert_eq!(inferred, vec![Some(SqlFieldType::Int64), Some(SqlFieldType::String)]);
    }

    #[test]
    fn comparison_pairs_map_in_either_order() {
        let inferred =
            infer_sql_parameter_types("UPDATE users SET name = $1 WHERE id = $2", SqlParameterStyle::DollarNumber, &catalog());
        assert_eq!(inferred, vec![Some(SqlFieldType::String), Some(SqlFieldType::Int64)]);
        let inferred = infer_sql_parameter_types("SELECT 1 FROM users WHERE $1 = id", SqlParameterStyle::DollarNumber, &catalog());
        assert_eq!(inferred, vec![Some(SqlFieldType::Int64)]);
    }

    #[test]
    fn quoted_identifiers_and_unknown_columns_stay_uninferred() {
        let inferred =
            infer_sql_parameter_types(r#"INSERT INTO "users" ("id", "missing") VALUES ($1, $2)"#, SqlParameterStyle::DollarNumber, &catalog());
        assert_eq!(inferred, vec![Some(SqlFieldType::Int64), None]);
    }

    #[test]
    fn placeholders_in_comments_and_strings_are_not_inferred() {
        let inferred = infer_sql_parameter_types("UPDATE users SET id = $1 WHERE name = 'x $2 y' -- $3\n", SqlParameterStyle::DollarNumber, &catalog());
        assert_eq!(inferred, vec![Some(SqlFieldType::Int64)]);
        let inferred = infer_sql_parameter_types("UPDATE users SET id = $1 /* $2 */", SqlParameterStyle::DollarNumber, &catalog());
        assert_eq!(inferred, vec![Some(SqlFieldType::Int64)]);
    }

    #[test]
    fn question_mark_style_maps_by_appearance() {
        let inferred = infer_sql_parameter_types("INSERT INTO users (id, name) VALUES (?, ?)", SqlParameterStyle::QuestionMark, &catalog());
        assert_eq!(inferred, vec![Some(SqlFieldType::Int64), Some(SqlFieldType::String)]);
    }

    #[test]
    fn unattributed_placeholders_stay_none() {
        let inferred = infer_sql_parameter_types("SELECT strpos(name, $1) FROM users WHERE id = $2", SqlParameterStyle::DollarNumber, &catalog());
        assert_eq!(inferred, vec![None, Some(SqlFieldType::Int64)]);
    }
}
