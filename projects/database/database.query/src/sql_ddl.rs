//! DDL subset parser for the SQL compatibility gateways.
//!
//! The gateways only accept the DDL the Iris ORM adapter emits so a local schema
//! registry can stay in sync with the client's expectations. Every construct
//! outside that subset fails with a structured diagnostic naming it — this
//! parser never decides that an unknown clause is safe to ignore.

use crate::sql::{SqlDialect, SqlFrontendError};

/// One DDL statement the gateway can apply to a local schema registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlDdlStatement {
    /// `CREATE [TEMP|TEMPORARY] TABLE ...`
    CreateTable {
        /// Table name after the accepted `public.` / `pg_temp.` schema prefix.
        name: String,
        /// Whether `TEMP` / `TEMPORARY` was written, or the name was
        /// qualified with `pg_temp`. Temporary tables live in a session
        /// registry and never reach the persisted schema.
        temporary: bool,
        /// `ON COMMIT` behaviour; PostgreSQL only accepts it for temporary
        /// tables.
        on_commit: OnCommit,
        /// Whether `IF NOT EXISTS` was written.
        if_not_exists: bool,
        /// Column definitions in declaration order.
        columns: Vec<DdlColumn>,
        /// Primary key columns collected from inline and table-level declarations.
        primary_key: Vec<String>,
        /// Unique constraints collected from table-level declarations.
        unique_constraints: Vec<Vec<String>>,
        /// Table-level `CHECK` expressions, stored as the text written inside
        /// the parentheses and re-parsed by the SQL frontend at write time.
        check_constraints: Vec<String>,
        /// Table-level `FOREIGN KEY` constraints.
        foreign_keys: Vec<DdlForeignKey>,
        /// `CHARSET` table option, as written.
        charset: Option<String>,
        /// `COLLATE` table option, as written.
        collation: Option<String>,
    },
    /// `ALTER TABLE ... ADD COLUMN ...`
    AlterTableAddColumn {
        /// Target table name.
        table: String,
        /// Column definition to append.
        column: DdlColumn,
        /// Whether `IF NOT EXISTS` was written.
        if_not_exists: bool,
    },
    /// `ALTER TABLE ... ADD CONSTRAINT ...`
    AlterTableAddConstraint {
        /// Target table name.
        table: String,
        /// Constraint to register.
        constraint: DdlTableConstraint,
    },
    /// `DROP TABLE ...`
    DropTable {
        /// Tables to drop.
        names: Vec<String>,
        /// Whether `IF EXISTS` was written.
        if_exists: bool,
    },
    /// `CREATE [UNIQUE] INDEX ...`
    CreateIndex {
        /// Index name.
        name: String,
        /// Indexed table.
        table: String,
        /// Whether `IF NOT EXISTS` was written.
        if_not_exists: bool,
        /// Whether `UNIQUE` was written.
        unique: bool,
        /// Indexed columns in declaration order.
        columns: Vec<String>,
        /// Partial-index `WHERE` predicate text, as written after `WHERE`.
        predicate: Option<String>,
    },
    /// `DROP INDEX ...`
    DropIndex {
        /// Index name.
        name: String,
        /// Table name (MySQL requires it, PostgreSQL does not).
        table: Option<String>,
        /// Whether `IF EXISTS` was written.
        if_exists: bool,
    },
}

/// `ON COMMIT` behaviour of a temporary table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnCommit {
    /// `ON COMMIT PRESERVE ROWS`, the default: rows survive a commit.
    PreserveRows,
    /// `ON COMMIT DELETE ROWS`: the gateway rejects this explicitly, because
    /// deleting every row at the commit boundary would run a write statement
    /// from inside the transaction epilogue.
    DeleteRows,
    /// `ON COMMIT DROP`: the temporary table disappears at the commit point.
    Drop,
}

/// One column definition in a `CREATE TABLE` / `ALTER TABLE ADD COLUMN`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdlColumn {
    /// Column name.
    pub name: String,
    /// Raw SQL type text as written, normalised to uppercase words with no spaces
    /// inside parameter lists, e.g. `TINYINT(1)`, `DOUBLE PRECISION`, `VARCHAR(191)`.
    pub sql_type: String,
    /// Whether `NOT NULL` was written.
    pub not_null: bool,
    /// Whether `PRIMARY KEY` was written inline.
    pub inline_primary_key: bool,
    /// Whether `UNIQUE` was written inline. The column-level spelling is
    /// collected into the same table-level constraint list as
    /// `UNIQUE KEY (column)`.
    pub inline_unique: bool,
    /// Whether the column allocates its own values: MySQL `AUTO_INCREMENT` or
    /// PostgreSQL `SERIAL` / `BIGSERIAL` / `SMALLSERIAL`.
    pub auto_increment: bool,
    /// Raw `DEFAULT` literal text, when written.
    pub default: Option<String>,
    /// Inline `CHECK` expression, stored as the text written inside the
    /// parentheses and re-parsed by the SQL frontend at write time.
    pub check: Option<String>,
    /// Inline `REFERENCES` clause, when written.
    pub reference: Option<DdlReference>,
    /// Stored generated-column expression text, when the column declared
    /// `GENERATED ALWAYS AS (expression) STORED`. The expression is re-parsed by
    /// the SQL frontend on every write, so the registry only stores the text.
    pub generated: Option<String>,
}

/// One `REFERENCES table [(column)] [ON DELETE action] [ON UPDATE action]`
/// clause, from a column-level inline reference or a table-level foreign key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdlReference {
    /// Referenced table name after the accepted `public.` prefix.
    pub table: String,
    /// Referenced column, when written. A table-level foreign key without a
    /// column list references the parent table's primary key.
    pub column: Option<String>,
    /// `ON DELETE` action text, as written.
    pub on_delete: Option<String>,
    /// `ON UPDATE` action text, as written.
    pub on_update: Option<String>,
}

/// One table-level `FOREIGN KEY (columns) REFERENCES ...` constraint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdlForeignKey {
    /// Child columns in declaration order.
    pub columns: Vec<String>,
    /// Referenced parent table and column.
    pub reference: DdlReference,
}

/// One constraint added by `ALTER TABLE ... ADD CONSTRAINT [...]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DdlTableConstraint {
    /// `UNIQUE (columns)`.
    Unique {
        /// Columns carrying the unique key.
        columns: Vec<String>,
    },
    /// `CHECK (expression)`.
    Check {
        /// Expression text inside the parentheses.
        expression: String,
    },
    /// `FOREIGN KEY (columns) REFERENCES ...`.
    ForeignKey {
        /// Foreign key definition.
        key: DdlForeignKey,
    },
}

/// Collations the gateway accepts as metadata.
///
/// Text is always compared by exact code points, so the declared collation
/// never changes query semantics. Names that drivers emit by default (for
/// example `utf8mb4_0900_ai_ci` from mysql-connector-python) are accepted so a
/// stock client can connect, and the registry stores them as metadata only.
pub const ACCEPTED_COLLATIONS: &[&str] = &[
    "utf8mb4_bin",
    "utf8mb4_0900_bin",
    "utf8mb4_0900_ai_ci",
    "utf8mb4_0900_as_ci",
    "utf8mb4_0900_as_cs",
    "utf8mb4_general_ci",
    "utf8mb4_unicode_ci",
    "utf8mb4_unicode_520_ci",
    "utf8_bin",
    "utf8_general_ci",
    "utf8_unicode_ci",
];

/// Whether `name` is an accepted UTF-8 collation name.
///
/// Used by both DDL (`COLLATE = ...`) and the MySQL session `SET NAMES`
/// command so a connection and a table cannot disagree about accepted names.
pub fn is_accepted_collation(name: &str) -> bool {
    ACCEPTED_COLLATIONS.iter().any(|candidate| candidate.eq_ignore_ascii_case(name))
}

/// Whether `name` is the session's temporary schema: `pg_temp` or
/// `pg_temp_<digits>`, the two spellings PostgreSQL resolves to it.
pub fn is_pg_temp_schema(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower == "pg_temp" {
        return true;
    }
    match lower.strip_prefix("pg_temp_") {
        Some(suffix) => !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()),
        None => false,
    }
}

/// Constraint keywords that terminate a column type.
const CONSTRAINT_KEYWORDS: &[&str] =
    &["NOT", "NULL", "PRIMARY", "UNIQUE", "DEFAULT", "CHECK", "REFERENCES", "COLLATE", "AUTO_INCREMENT", "COMMENT", "GENERATED"];

/// Returns true when the first SQL keyword is `CREATE`, `ALTER`, or `DROP`.
///
/// Whitespace, `--` line comments, and `/* */` block comments are skipped. Any
/// other leading keyword means the statement is not DDL and belongs to the
/// regular SQL path.
pub fn is_ddl_statement(source: &str) -> bool {
    matches!(leading_keyword(source).as_deref(), Some("CREATE") | Some("ALTER") | Some("DROP"))
}

/// Parses one DDL statement, or returns `Ok(None)` when the source is not DDL.
pub fn parse_ddl(source: &str, dialect: SqlDialect) -> Result<Option<SqlDdlStatement>, SqlFrontendError> {
    if !is_ddl_statement(source) {
        return Ok(None);
    }
    let tokens = tokenize(source, dialect)?;
    if tokens.is_empty() {
        return Ok(None);
    }
    let mut parser = Parser { tokens, cursor: 0, dialect };
    let statement = parser.statement()?;
    parser.finish()?;
    Ok(Some(statement))
}

/// Reads the first keyword without tokenizing the rest of the statement.
fn leading_keyword(source: &str) -> Option<String> {
    let bytes = source.as_bytes();
    let mut index = 0;
    loop {
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if index >= bytes.len() {
            return None;
        }
        if bytes[index] == b'-' && bytes.get(index + 1) == Some(&b'-') {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            while index < bytes.len() && !(bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/')) {
                index += 1;
            }
            // An unterminated block comment cannot expose a keyword.
            if index >= bytes.len() {
                return None;
            }
            index += 2;
            continue;
        }
        let start = index;
        while index < bytes.len() && is_word_byte(bytes[index]) {
            index += 1;
        }
        if start == index {
            return None;
        }
        return Some(source[start..index].to_ascii_uppercase());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// Bare word: letters, digits, `_`, `$`.
    Word(String),
    /// Backtick-quoted identifier (MySQL only).
    Backtick(String),
    /// Double-quoted identifier (PostgreSQL only).
    Quoted(String),
    /// Single-quoted string literal, unescaped.
    String(String),
    /// Numeric literal, including a leading `-`.
    Number(String),
    /// `(`, `)`, `,`, `;`, `=`, or `.`.
    Punct(char),
    /// Operator token used inside `CHECK` expressions, function-call defaults
    /// and partial-index predicates: `>=`, `<=`, `<>`, `!=`, `<`, `>`, `+`,
    /// `-`, `*` or `||`.
    Op(&'static str),
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

fn token_text(token: &Token) -> String {
    match token {
        Token::Word(text) | Token::Number(text) => text.clone(),
        Token::Backtick(text) => format!("`{text}`"),
        Token::Quoted(text) => format!("\"{text}\""),
        Token::String(text) => format!("'{}'", text.replace('\'', "''")),
        Token::Punct(ch) => ch.to_string(),
        Token::Op(text) => text.to_string(),
    }
}

fn tokenize(source: &str, dialect: SqlDialect) -> Result<Vec<Token>, SqlFrontendError> {
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
                return Err(SqlFrontendError::syntax("unclosed `/* */` comment in DDL statement"));
            }
            continue;
        }
        match byte {
            b'`' => {
                if dialect != SqlDialect::Mysql {
                    return Err(SqlFrontendError::syntax(
                        "backtick-quoted identifiers are MySQL syntax and are not accepted in PostgreSQL DDL",
                    ));
                }
                index += 1;
                let start = index;
                while index < bytes.len() && bytes[index] != b'`' {
                    index += 1;
                }
                if index >= bytes.len() {
                    return Err(SqlFrontendError::syntax("unclosed backtick-quoted identifier in DDL statement"));
                }
                tokens.push(Token::Backtick(source[start..index].to_string()));
                index += 1;
            }
            b'"' => {
                if dialect != SqlDialect::Postgres {
                    return Err(SqlFrontendError::syntax("double-quoted identifiers are PostgreSQL syntax and are not accepted in MySQL DDL"));
                }
                index += 1;
                let start = index;
                while index < bytes.len() && bytes[index] != b'"' {
                    index += 1;
                }
                if index >= bytes.len() {
                    return Err(SqlFrontendError::syntax("unclosed double-quoted identifier in DDL statement"));
                }
                tokens.push(Token::Quoted(source[start..index].to_string()));
                index += 1;
            }
            b'\'' => {
                index += 1;
                let mut value = String::new();
                loop {
                    if index >= bytes.len() {
                        return Err(SqlFrontendError::syntax("unclosed string literal in DDL statement"));
                    }
                    match bytes[index] {
                        b'\'' if bytes.get(index + 1) == Some(&b'\'') => {
                            value.push('\'');
                            index += 2;
                        }
                        b'\'' => {
                            index += 1;
                            break;
                        }
                        b'\\' if dialect == SqlDialect::Mysql && index + 1 < bytes.len() => {
                            value.push(bytes[index + 1] as char);
                            index += 2;
                        }
                        byte => {
                            value.push(byte as char);
                            index += 1;
                        }
                    }
                }
                tokens.push(Token::String(value));
            }
            b'(' | b')' | b',' | b';' | b'=' | b'.' => {
                tokens.push(Token::Punct(char::from(byte)));
                index += 1;
            }
            b'|' if bytes.get(index + 1) == Some(&b'|') => {
                tokens.push(Token::Op("||"));
                index += 2;
            }
            b'>' if bytes.get(index + 1) == Some(&b'=') => {
                tokens.push(Token::Op(">="));
                index += 2;
            }
            b'>' => {
                tokens.push(Token::Op(">"));
                index += 1;
            }
            b'<' if bytes.get(index + 1) == Some(&b'=') => {
                tokens.push(Token::Op("<="));
                index += 2;
            }
            b'<' if bytes.get(index + 1) == Some(&b'>') => {
                tokens.push(Token::Op("<>"));
                index += 2;
            }
            b'<' => {
                tokens.push(Token::Op("<"));
                index += 1;
            }
            b'!' if bytes.get(index + 1) == Some(&b'=') => {
                tokens.push(Token::Op("!="));
                index += 2;
            }
            b'+' | b'*' => {
                tokens.push(Token::Op(if byte == b'+' { "+" } else { "*" }));
                index += 1;
            }
            b'-' if bytes.get(index + 1).is_some_and(u8::is_ascii_digit) => {
                let start = index;
                index += 1;
                index = take_number(bytes, index);
                tokens.push(Token::Number(source[start..index].to_string()));
            }
            b'-' => {
                tokens.push(Token::Op("-"));
                index += 1;
            }
            byte if byte.is_ascii_digit() => {
                let start = index;
                index = take_number(bytes, index);
                tokens.push(Token::Number(source[start..index].to_string()));
            }
            byte if is_word_byte(byte) => {
                let start = index;
                while index < bytes.len() && is_word_byte(bytes[index]) {
                    index += 1;
                }
                tokens.push(Token::Word(source[start..index].to_string()));
            }
            byte => {
                return Err(SqlFrontendError::syntax(format!("unexpected character `{}` in DDL statement", char::from(byte))));
            }
        }
    }
    Ok(tokens)
}

/// Consumes digits, an optional fractional part, and an optional exponent.
fn take_number(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        index += 1;
    }
    if index < bytes.len() && bytes[index] == b'.' && bytes.get(index + 1).is_some_and(u8::is_ascii_digit) {
        index += 1;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
    }
    if index < bytes.len() && matches!(bytes[index], b'e' | b'E') {
        let mut exponent = index + 1;
        if exponent < bytes.len() && matches!(bytes[exponent], b'+' | b'-') {
            exponent += 1;
        }
        if exponent < bytes.len() && bytes[exponent].is_ascii_digit() {
            index = exponent;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
            }
        }
    }
    index
}

struct Parser {
    tokens: Vec<Token>,
    cursor: usize,
    dialect: SqlDialect,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.cursor)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.cursor).cloned();
        if token.is_some() {
            self.cursor += 1;
        }
        token
    }

    fn peek_word(&self) -> Option<String> {
        match self.peek() {
            Some(Token::Word(word)) => Some(word.clone()),
            _ => None,
        }
    }

    fn peek_keyword(&self) -> Option<String> {
        self.peek_word().map(|word| word.to_ascii_uppercase())
    }

    fn eat_keyword(&mut self, keyword: &str) -> bool {
        if self.peek_keyword().as_deref() == Some(keyword) {
            self.cursor += 1;
            true
        }
        else {
            false
        }
    }

    fn expect_keyword(&mut self, keyword: &str) -> Result<(), SqlFrontendError> {
        if self.eat_keyword(keyword) { Ok(()) } else { Err(self.expected(keyword)) }
    }

    fn expected(&self, keyword: &str) -> SqlFrontendError {
        match self.peek() {
            Some(token) => SqlFrontendError::syntax(format!("expected `{keyword}`, found `{}`", token_text(token))),
            None => SqlFrontendError::syntax(format!("expected `{keyword}`, found end of statement")),
        }
    }

    fn peek_punct(&self, expected: char) -> bool {
        matches!(self.peek(), Some(Token::Punct(punct)) if *punct == expected)
    }

    fn eat_punct(&mut self, expected: char) -> bool {
        if self.peek_punct(expected) {
            self.cursor += 1;
            true
        }
        else {
            false
        }
    }

    fn expect_punct(&mut self, expected: char) -> Result<(), SqlFrontendError> {
        if self.eat_punct(expected) { Ok(()) } else { Err(self.expected(&expected.to_string())) }
    }

    fn statement(&mut self) -> Result<SqlDdlStatement, SqlFrontendError> {
        if self.eat_keyword("CREATE") {
            self.create()
        }
        else if self.eat_keyword("ALTER") {
            self.alter()
        }
        else if self.eat_keyword("DROP") {
            self.drop_statement()
        }
        else {
            Err(SqlFrontendError::syntax("expected a `CREATE`, `ALTER`, or `DROP` statement"))
        }
    }

    /// Consumes a trailing `;` and rejects anything after it.
    fn finish(&mut self) -> Result<(), SqlFrontendError> {
        let terminated = self.eat_punct(';');
        match self.peek() {
            None => Ok(()),
            Some(_) if terminated => Err(SqlFrontendError::unsupported("more than one SQL statement per gateway message is not supported")),
            Some(_) => Err(SqlFrontendError::syntax("unexpected token after the DDL statement")),
        }
    }

    fn create(&mut self) -> Result<SqlDdlStatement, SqlFrontendError> {
        if self.eat_keyword("TABLE") {
            return self.create_table(false);
        }
        if self.eat_keyword("TEMP") || self.eat_keyword("TEMPORARY") {
            self.expect_keyword("TABLE")?;
            return self.create_table(true);
        }
        if self.peek_keyword().as_deref() == Some("UNIQUE") || self.peek_keyword().as_deref() == Some("INDEX") {
            return self.create_index();
        }
        Err(self.unsupported_prefix("CREATE"))
    }

    fn create_table(&mut self, temporary: bool) -> Result<SqlDdlStatement, SqlFrontendError> {
        let if_not_exists = self.if_not_exists()?;
        let (name, temp_schema) = self.qualified_table_name()?;
        let temporary = temporary || temp_schema;
        self.expect_punct('(')?;
        let mut columns = Vec::new();
        let mut primary_key = Vec::new();
        let mut unique_constraints = Vec::new();
        let mut check_constraints = Vec::new();
        let mut foreign_keys = Vec::new();
        while !self.peek_punct(')') {
            self.table_item(&mut columns, &mut primary_key, &mut unique_constraints, &mut check_constraints, &mut foreign_keys)?;
            if !self.eat_punct(',') {
                break;
            }
        }
        self.expect_punct(')')?;
        let on_commit = self.on_commit_clause(temporary)?;
        let (charset, collation) = self.table_options()?;
        Ok(SqlDdlStatement::CreateTable {
            name,
            temporary,
            on_commit,
            if_not_exists,
            columns,
            primary_key,
            unique_constraints,
            check_constraints,
            foreign_keys,
            charset,
            collation,
        })
    }

    fn table_item(
        &mut self,
        columns: &mut Vec<DdlColumn>,
        primary_key: &mut Vec<String>,
        unique_constraints: &mut Vec<Vec<String>>,
        check_constraints: &mut Vec<String>,
        foreign_keys: &mut Vec<DdlForeignKey>,
    ) -> Result<(), SqlFrontendError> {
        match self.peek_keyword().as_deref() {
            Some("CONSTRAINT") => {
                self.cursor += 1;
                let _constraint_name = self.identifier()?;
                if self.eat_keyword("PRIMARY") {
                    self.expect_keyword("KEY")?;
                    let names = self.column_list()?;
                    if !primary_key.is_empty() {
                        return Err(SqlFrontendError::syntax("table has more than one PRIMARY KEY definition"));
                    }
                    *primary_key = names;
                    Ok(())
                }
                else if self.eat_keyword("UNIQUE") {
                    self.eat_keyword("KEY");
                    if !self.peek_punct('(') {
                        let _index_name = self.identifier()?;
                    }
                    unique_constraints.push(self.column_list()?);
                    Ok(())
                }
                else if self.eat_keyword("FOREIGN") {
                    foreign_keys.push(self.foreign_key_item()?);
                    Ok(())
                }
                else if self.eat_keyword("CHECK") {
                    check_constraints.push(self.capture_paren_group()?);
                    Ok(())
                }
                else {
                    Err(self.unsupported_prefix("CONSTRAINT"))
                }
            }
            Some("PRIMARY") => {
                self.cursor += 1;
                self.expect_keyword("KEY")?;
                let names = self.column_list()?;
                if !primary_key.is_empty() {
                    return Err(SqlFrontendError::syntax("table has more than one PRIMARY KEY definition"));
                }
                *primary_key = names;
                Ok(())
            }
            Some("UNIQUE") => {
                self.cursor += 1;
                self.eat_keyword("KEY");
                if !self.peek_punct('(') {
                    let _index_name = self.identifier()?;
                }
                unique_constraints.push(self.column_list()?);
                Ok(())
            }
            Some("FOREIGN") => {
                self.cursor += 1;
                foreign_keys.push(self.foreign_key_item()?);
                Ok(())
            }
            Some("CHECK") => {
                self.cursor += 1;
                check_constraints.push(self.capture_paren_group()?);
                Ok(())
            }
            Some("KEY") | Some("INDEX") => Err(SqlFrontendError::unsupported(
                "`KEY` / `INDEX` table constraints are not supported by the cluster SQL gateway, use `CREATE INDEX`",
            )),
            Some("FULLTEXT") => Err(SqlFrontendError::unsupported("`FULLTEXT` indexes are not supported by the cluster SQL gateway")),
            Some("SPATIAL") => Err(SqlFrontendError::unsupported("`SPATIAL` indexes are not supported by the cluster SQL gateway")),
            _ => {
                let column = self.column_definition()?;
                if column.inline_unique {
                    unique_constraints.push(vec![column.name.clone()]);
                }
                columns.push(column);
                Ok(())
            }
        }
    }

    fn column_definition(&mut self) -> Result<DdlColumn, SqlFrontendError> {
        let name = self.identifier()?;
        let mut sql_type = String::new();
        loop {
            match self.peek() {
                Some(Token::Word(word)) if !is_constraint_keyword(word) => {
                    let word = word.to_ascii_uppercase();
                    self.cursor += 1;
                    if !sql_type.is_empty() {
                        sql_type.push(' ');
                    }
                    sql_type.push_str(&word);
                }
                Some(Token::Punct('(')) => {
                    let parameters = self.type_parameters()?;
                    sql_type.push('(');
                    sql_type.push_str(&parameters);
                    sql_type.push(')');
                }
                _ => break,
            }
        }
        if sql_type.is_empty() {
            return Err(SqlFrontendError::syntax(format!("column `{name}` declares no type")));
        }
        let mut not_null = false;
        let mut inline_primary_key = false;
        let mut inline_unique = false;
        // PostgreSQL serial pseudo-types imply an owned sequence.
        let mut auto_increment = matches!(sql_type.as_str(), "SERIAL" | "BIGSERIAL" | "SMALLSERIAL");
        let mut default = None;
        let mut check = None;
        let mut reference = None;
        let mut generated = None;
        loop {
            match self.peek_keyword().as_deref() {
                Some("NOT") => {
                    self.cursor += 1;
                    self.expect_keyword("NULL")?;
                    not_null = true;
                }
                Some("NULL") => {
                    self.cursor += 1;
                }
                Some("PRIMARY") => {
                    self.cursor += 1;
                    self.expect_keyword("KEY")?;
                    inline_primary_key = true;
                    not_null = true;
                }
                Some("UNIQUE") => {
                    self.cursor += 1;
                    inline_unique = true;
                }
                Some("DEFAULT") => {
                    self.cursor += 1;
                    default = Some(self.default_literal()?);
                }
                Some("COLLATE") => {
                    self.cursor += 1;
                    let _collation = self.identifier()?;
                }
                Some("AUTO_INCREMENT") => {
                    self.cursor += 1;
                    auto_increment = true;
                }
                Some("COMMENT") => {
                    self.cursor += 1;
                    self.string_literal("COMMENT")?;
                }
                Some("CHECK") => {
                    self.cursor += 1;
                    check = Some(self.capture_paren_group()?);
                }
                Some("REFERENCES") => {
                    reference = Some(self.reference_clause()?);
                }
                Some("GENERATED") => {
                    self.cursor += 1;
                    if !self.eat_keyword("ALWAYS") {
                        self.expect_keyword("BY")?;
                        self.expect_keyword("DEFAULT")?;
                    }
                    self.expect_keyword("AS")?;
                    if self.eat_keyword("IDENTITY") {
                        auto_increment = true;
                        // `( START WITH n INCREMENT BY m … )` is accepted as
                        // metadata; the gateway always allocates from 1.
                        if self.peek_punct('(') {
                            let _ = self.capture_paren_group()?;
                        }
                    }
                    else if self.peek_punct('(') {
                        generated = Some(self.capture_paren_group()?);
                        // PostgreSQL only supports `STORED` for a generated
                        // column, so a missing or `VIRTUAL` storage keyword is a
                        // syntax error rather than a silent downgrade.
                        if !self.eat_keyword("STORED") {
                            return Err(SqlFrontendError::syntax("`GENERATED ... AS (expression)` requires `STORED`"));
                        }
                    }
                    else {
                        return Err(SqlFrontendError::syntax("`GENERATED` requires `AS IDENTITY` or `AS (expression) STORED`"));
                    }
                }
                _ => break,
            }
        }
        Ok(DdlColumn {
            name,
            sql_type,
            not_null,
            inline_primary_key,
            inline_unique,
            auto_increment,
            default,
            check,
            reference,
            generated,
        })
    }

    /// Captures one balanced parenthesised group as re-parseable SQL text.
    ///
    /// The opening `(` must be the next token. The returned text is the group
    /// content with tokens rejoined by single spaces, enough for the SQL
    /// frontend to re-parse the expression when a write validates it; the DDL
    /// parser itself never evaluates this text.
    fn capture_paren_group(&mut self) -> Result<String, SqlFrontendError> {
        self.expect_punct('(')?;
        let mut depth = 1usize;
        let mut parts: Vec<String> = Vec::new();
        while let Some(token) = self.next() {
            match token {
                Token::Punct('(') => depth += 1,
                Token::Punct(')') => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(parts.join(" "));
                    }
                }
                other => parts.push(token_text(&other)),
            }
        }
        Err(SqlFrontendError::syntax("unclosed parenthesised expression in DDL statement"))
    }

    /// Parses the body of a `FOREIGN KEY (columns) REFERENCES ...` item after
    /// the `FOREIGN` keyword was consumed.
    fn foreign_key_item(&mut self) -> Result<DdlForeignKey, SqlFrontendError> {
        self.expect_keyword("KEY")?;
        let columns = self.column_list()?;
        let reference = self.reference_clause()?;
        Ok(DdlForeignKey { columns, reference })
    }

    /// Parses `REFERENCES table [(column)] [ON DELETE action] [ON UPDATE action]`.
    fn reference_clause(&mut self) -> Result<DdlReference, SqlFrontendError> {
        self.expect_keyword("REFERENCES")?;
        let table = self.table_name()?;
        let column = if self.peek_punct('(') {
            let columns = self.column_list()?;
            let column = columns
                .into_iter()
                .next()
                .ok_or_else(|| SqlFrontendError::syntax("`REFERENCES` requires at least one column"))?;
            Some(column)
        }
        else {
            None
        };
        let mut on_delete = None;
        let mut on_update = None;
        while self.eat_keyword("ON") {
            if self.eat_keyword("DELETE") {
                on_delete = Some(self.reference_action()?);
            }
            else if self.eat_keyword("UPDATE") {
                on_update = Some(self.reference_action()?);
            }
            else {
                return Err(SqlFrontendError::syntax("`REFERENCES ... ON` requires `DELETE` or `UPDATE`"));
            }
        }
        Ok(DdlReference { table, column, on_delete, on_update })
    }

    /// Parses one referential action: `NO ACTION`, `RESTRICT`, `CASCADE`,
    /// `SET NULL` or `SET DEFAULT`.
    fn reference_action(&mut self) -> Result<String, SqlFrontendError> {
        match self.peek_keyword().as_deref() {
            Some("NO") => {
                self.cursor += 1;
                self.expect_keyword("ACTION")?;
                Ok("NO ACTION".to_string())
            }
            Some("RESTRICT") => {
                self.cursor += 1;
                Ok("RESTRICT".to_string())
            }
            Some("CASCADE") => {
                self.cursor += 1;
                Ok("CASCADE".to_string())
            }
            Some("SET") => {
                self.cursor += 1;
                if self.eat_keyword("NULL") {
                    Ok("SET NULL".to_string())
                }
                else {
                    self.expect_keyword("DEFAULT")?;
                    Ok("SET DEFAULT".to_string())
                }
            }
            _ => Err(self.expected("a referential action")),
        }
    }

    fn type_parameters(&mut self) -> Result<String, SqlFrontendError> {
        self.expect_punct('(')?;
        let mut text = String::new();
        loop {
            match self.next() {
                Some(Token::Word(word)) | Some(Token::Number(word)) => text.push_str(&word),
                Some(Token::String(value)) => {
                    text.push('\'');
                    text.push_str(&value);
                    text.push('\'');
                }
                Some(Token::Punct(',')) => text.push(','),
                Some(Token::Punct(')')) => return Ok(text),
                Some(token) => {
                    return Err(SqlFrontendError::syntax(format!("invalid column type parameter `{}`", token_text(&token))));
                }
                None => return Err(SqlFrontendError::syntax("unclosed column type parameter list")),
            }
        }
    }

    fn default_literal(&mut self) -> Result<String, SqlFrontendError> {
        match self.next() {
            Some(Token::Number(text)) => Ok(text),
            Some(Token::String(value)) => Ok(format!("'{value}'")),
            Some(Token::Word(word)) => {
                let keyword = word.to_ascii_uppercase();
                match keyword.as_str() {
                    "NULL" | "TRUE" | "FALSE" => Ok(word),
                    _ if self.peek_punct('(') => {
                        // Function-call default such as `now()` or
                        // `CURRENT_TIMESTAMP(3)`: capture the balanced group
                        // and store the call text for the write path.
                        let inner = self.capture_paren_group()?;
                        Ok(format!("{word}({inner})"))
                    }
                    "CURRENT_TIMESTAMP" => Ok(word),
                    _ => Err(SqlFrontendError::unsupported(format!("`DEFAULT {word}` is not supported by the cluster SQL gateway"))),
                }
            }
            Some(token) => Err(SqlFrontendError::syntax(format!("invalid `DEFAULT` literal `{}`", token_text(&token)))),
            None => Err(SqlFrontendError::syntax("`DEFAULT` requires a literal")),
        }
    }

    fn column_list(&mut self) -> Result<Vec<String>, SqlFrontendError> {
        self.expect_punct('(')?;
        let mut names = Vec::new();
        loop {
            if self.peek_punct(')') {
                break;
            }
            names.push(self.identifier()?);
            if self.eat_punct(',') {
                continue;
            }
            if self.peek_punct(')') {
                break;
            }
            return Err(SqlFrontendError::unsupported("expressions inside a column list are not supported by the cluster SQL gateway"));
        }
        self.expect_punct(')')?;
        Ok(names)
    }

    fn table_options(&mut self) -> Result<(Option<String>, Option<String>), SqlFrontendError> {
        if self.dialect == SqlDialect::Postgres {
            if self.peek().is_some() && !self.peek_punct(';') {
                return Err(SqlFrontendError::syntax("PostgreSQL `CREATE TABLE` does not accept trailing table options"));
            }
            return Ok((None, None));
        }
        let mut charset = None;
        let mut collation = None;
        loop {
            while self.eat_punct(',') {}
            if self.peek().is_none() || self.peek_punct(';') {
                break;
            }
            if self.eat_keyword("ENGINE") {
                self.eat_punct('=');
                self.option_word("ENGINE")?;
                continue;
            }
            if self.eat_keyword("DEFAULT") {
                if !self.eat_keyword("CHARSET") {
                    return Err(self.unsupported_prefix("DEFAULT"));
                }
                self.eat_punct('=');
                charset = Some(self.option_word("CHARSET")?);
                continue;
            }
            if self.eat_keyword("CHARSET") {
                self.eat_punct('=');
                charset = Some(self.option_word("CHARSET")?);
                continue;
            }
            if self.eat_keyword("COLLATE") {
                self.eat_punct('=');
                collation = Some(self.option_word("COLLATE")?);
                continue;
            }
            if self.eat_keyword("AUTO_INCREMENT") {
                self.eat_punct('=');
                self.option_number("AUTO_INCREMENT")?;
                continue;
            }
            if self.eat_keyword("COMMENT") {
                self.eat_punct('=');
                self.string_literal("COMMENT")?;
                continue;
            }
            return Err(self.unsupported_prefix(""));
        }
        if let Some(character_set) = &charset {
            let accepted = character_set.eq_ignore_ascii_case("utf8mb4") || character_set.eq_ignore_ascii_case("utf8");
            if !accepted {
                return Err(SqlFrontendError::unsupported(format!(
                    "table character set `{character_set}` is not supported: the gateway compares text by exact code points, use \
                     `utf8mb4` or `utf8`"
                )));
            }
        }
        if let Some(name) = &collation {
            if !is_accepted_collation(name) {
                return Err(SqlFrontendError::unsupported(format!(
                    "table collation `{name}` is not supported: accepted collations are {}",
                    ACCEPTED_COLLATIONS.join(", ")
                )));
            }
        }
        Ok((charset, collation))
    }

    fn option_word(&mut self, option: &str) -> Result<String, SqlFrontendError> {
        match self.next() {
            Some(Token::Word(word)) => Ok(word),
            Some(token) => {
                Err(SqlFrontendError::unsupported(format!("table option `{option}` requires a bare word, found `{}`", token_text(&token))))
            }
            None => Err(SqlFrontendError::syntax(format!("table option `{option}` requires a value"))),
        }
    }

    fn option_number(&mut self, option: &str) -> Result<(), SqlFrontendError> {
        match self.next() {
            Some(Token::Number(_)) => Ok(()),
            Some(token) => {
                Err(SqlFrontendError::unsupported(format!("table option `{option}` requires a number, found `{}`", token_text(&token))))
            }
            None => Err(SqlFrontendError::syntax(format!("table option `{option}` requires a value"))),
        }
    }

    fn string_literal(&mut self, option: &str) -> Result<(), SqlFrontendError> {
        match self.next() {
            Some(Token::String(_)) => Ok(()),
            Some(token) => {
                Err(SqlFrontendError::unsupported(format!("table option `{option}` requires a string literal, found `{}`", token_text(&token))))
            }
            None => Err(SqlFrontendError::syntax(format!("table option `{option}` requires a value"))),
        }
    }

    fn alter(&mut self) -> Result<SqlDdlStatement, SqlFrontendError> {
        if !self.eat_keyword("TABLE") {
            return Err(self.unsupported_prefix("ALTER"));
        }
        let table = self.table_name()?;
        if !self.eat_keyword("ADD") {
            return Err(self.unsupported_action("ALTER TABLE"));
        }
        if self.eat_keyword("CONSTRAINT") {
            let _name = self.identifier()?;
            let constraint = self.table_constraint()?;
            return Ok(SqlDdlStatement::AlterTableAddConstraint { table, constraint });
        }
        if matches!(self.peek_keyword().as_deref(), Some("UNIQUE") | Some("CHECK") | Some("FOREIGN")) {
            let constraint = self.table_constraint()?;
            return Ok(SqlDdlStatement::AlterTableAddConstraint { table, constraint });
        }
        if !self.eat_keyword("COLUMN") {
            return Err(SqlFrontendError::unsupported(
                "`ALTER TABLE ... ADD` without `COLUMN` is not supported by the cluster SQL gateway, write `ADD COLUMN`",
            ));
        }
        let if_not_exists = self.if_not_exists()?;
        let column = self.column_definition()?;
        if self.peek_punct(',') {
            return Err(SqlFrontendError::unsupported("`ALTER TABLE` supports exactly one action per gateway message"));
        }
        Ok(SqlDdlStatement::AlterTableAddColumn { table, column, if_not_exists })
    }

    /// Parses one `UNIQUE` / `CHECK` / `FOREIGN KEY` constraint body.
    fn table_constraint(&mut self) -> Result<DdlTableConstraint, SqlFrontendError> {
        if self.eat_keyword("UNIQUE") {
            self.eat_keyword("KEY");
            if !self.peek_punct('(') {
                let _index_name = self.identifier()?;
            }
            return Ok(DdlTableConstraint::Unique { columns: self.column_list()? });
        }
        if self.eat_keyword("CHECK") {
            return Ok(DdlTableConstraint::Check { expression: self.capture_paren_group()? });
        }
        if self.eat_keyword("FOREIGN") {
            return Ok(DdlTableConstraint::ForeignKey { key: self.foreign_key_item()? });
        }
        Err(self.unsupported_action("ALTER TABLE ADD"))
    }

    fn drop_statement(&mut self) -> Result<SqlDdlStatement, SqlFrontendError> {
        if self.eat_keyword("TABLE") {
            return self.drop_table();
        }
        if self.eat_keyword("INDEX") {
            return self.drop_index();
        }
        Err(self.unsupported_prefix("DROP"))
    }

    fn drop_table(&mut self) -> Result<SqlDdlStatement, SqlFrontendError> {
        let if_exists = self.if_exists()?;
        let mut names = Vec::new();
        loop {
            names.push(self.table_name()?);
            if !self.eat_punct(',') {
                break;
            }
        }
        while self.eat_keyword("CASCADE") || self.eat_keyword("RESTRICT") {}
        if let Some(keyword) = self.peek_keyword() {
            return Err(SqlFrontendError::unsupported(format!("`DROP TABLE ... {keyword}` is not supported by the cluster SQL gateway")));
        }
        Ok(SqlDdlStatement::DropTable { names, if_exists })
    }

    fn drop_index(&mut self) -> Result<SqlDdlStatement, SqlFrontendError> {
        let if_exists = if self.dialect == SqlDialect::Mysql {
            if self.peek_keyword().as_deref() == Some("IF") {
                return Err(SqlFrontendError::unsupported("`DROP INDEX IF EXISTS` is PostgreSQL syntax"));
            }
            false
        }
        else {
            self.if_exists()?
        };
        let missing_name = self.peek().is_none()
            || self.peek_punct(';')
            || matches!(self.peek_keyword().as_deref(), Some("ON") | Some("CASCADE") | Some("RESTRICT"));
        if missing_name {
            return Err(SqlFrontendError::syntax("`DROP INDEX` requires an index name"));
        }
        let name = self.table_name()?;
        let table = if self.dialect == SqlDialect::Mysql {
            self.expect_keyword("ON")?;
            Some(self.table_name()?)
        }
        else {
            while self.eat_keyword("CASCADE") || self.eat_keyword("RESTRICT") {}
            None
        };
        Ok(SqlDdlStatement::DropIndex { name, table, if_exists })
    }

    fn create_index(&mut self) -> Result<SqlDdlStatement, SqlFrontendError> {
        let unique = self.eat_keyword("UNIQUE");
        self.expect_keyword("INDEX")?;
        let if_not_exists = self.if_not_exists()?;
        let name = self.table_name()?;
        if self.eat_keyword("USING") {
            self.index_method()?;
        }
        self.expect_keyword("ON")?;
        let table = self.table_name()?;
        if self.eat_keyword("USING") {
            self.index_method()?;
        }
        self.expect_punct('(')?;
        let mut columns = Vec::new();
        loop {
            if self.peek_punct(')') {
                break;
            }
            columns.push(self.identifier()?);
            if self.eat_keyword("ASC") || self.eat_keyword("DESC") {}
            if self.eat_punct(',') {
                continue;
            }
            if self.peek_punct(')') {
                break;
            }
            return Err(SqlFrontendError::unsupported(
                "expressions inside a `CREATE INDEX` column list are not supported by the cluster SQL gateway",
            ));
        }
        self.expect_punct(')')?;
        if self.eat_keyword("USING") {
            self.index_method()?;
        }
        let predicate = if self.eat_keyword("WHERE") {
            let mut parts: Vec<String> = Vec::new();
            while self.peek().is_some_and(|token| !matches!(token, Token::Punct(';'))) {
                let token = self.next().expect("token was just peeked");
                parts.push(token_text(&token));
            }
            if parts.is_empty() {
                return Err(SqlFrontendError::syntax("`CREATE INDEX ... WHERE` requires a predicate expression"));
            }
            Some(parts.join(" "))
        }
        else {
            None
        };
        if let Some(keyword) = self.peek_keyword() {
            return Err(SqlFrontendError::unsupported(format!("`CREATE INDEX ... {keyword}` is not supported by the cluster SQL gateway")));
        }
        Ok(SqlDdlStatement::CreateIndex { name, table, if_not_exists, unique, columns, predicate })
    }

    fn index_method(&mut self) -> Result<(), SqlFrontendError> {
        match self.peek_keyword().as_deref() {
            Some("BTREE") | Some("HASH") => {
                self.cursor += 1;
                Ok(())
            }
            Some(method) => Err(SqlFrontendError::unsupported(format!(
                "index method `{method}` is not supported by the cluster SQL gateway, use `BTREE` or `HASH`"
            ))),
            None => Err(self.expected("BTREE")),
        }
    }

    fn if_not_exists(&mut self) -> Result<bool, SqlFrontendError> {
        if !self.eat_keyword("IF") {
            return Ok(false);
        }
        self.expect_keyword("NOT")?;
        self.expect_keyword("EXISTS")?;
        Ok(true)
    }

    fn if_exists(&mut self) -> Result<bool, SqlFrontendError> {
        if !self.eat_keyword("IF") {
            return Ok(false);
        }
        self.expect_keyword("EXISTS")?;
        Ok(true)
    }

    fn identifier(&mut self) -> Result<String, SqlFrontendError> {
        match self.next() {
            Some(Token::Word(word)) | Some(Token::Backtick(word)) | Some(Token::Quoted(word)) => Ok(word),
            Some(token) => Err(SqlFrontendError::syntax(format!("expected an identifier, found `{}`", token_text(&token)))),
            None => Err(SqlFrontendError::syntax("expected an identifier, found end of statement")),
        }
    }

    /// Parses one table name, accepting PostgreSQL's `public.` prefix.
    fn table_name(&mut self) -> Result<String, SqlFrontendError> {
        Ok(self.qualified_table_name()?.0)
    }

    /// Parses one table name plus whether it named the session's temporary
    /// schema (`pg_temp` or `pg_temp_<n>`).
    fn qualified_table_name(&mut self) -> Result<(String, bool), SqlFrontendError> {
        let first = self.identifier()?;
        if !self.eat_punct('.') {
            return Ok((first, false));
        }
        let second = self.identifier()?;
        if self.dialect == SqlDialect::Postgres {
            if first.eq_ignore_ascii_case("public") {
                return Ok((second, false));
            }
            if is_pg_temp_schema(&first) {
                return Ok((second, true));
            }
        }
        Err(SqlFrontendError::unsupported(format!("qualified table name `{first}.{second}` is not supported by the cluster SQL gateway")))
    }

    /// Parses `[ON COMMIT { PRESERVE ROWS | DELETE ROWS | DROP }]`.
    fn on_commit_clause(&mut self, temporary: bool) -> Result<OnCommit, SqlFrontendError> {
        if self.dialect != SqlDialect::Postgres || self.peek_keyword().as_deref() != Some("ON") {
            return Ok(OnCommit::PreserveRows);
        }
        if !temporary {
            return Err(SqlFrontendError::syntax("`ON COMMIT` can only be used on temporary tables"));
        }
        self.cursor += 1;
        self.expect_keyword("COMMIT")?;
        match self.peek_keyword().as_deref() {
            Some("DROP") => {
                self.cursor += 1;
                Ok(OnCommit::Drop)
            }
            Some("DELETE") => {
                self.cursor += 1;
                self.expect_keyword("ROWS")?;
                Ok(OnCommit::DeleteRows)
            }
            Some("PRESERVE") => {
                self.cursor += 1;
                self.expect_keyword("ROWS")?;
                Ok(OnCommit::PreserveRows)
            }
            _ => Err(self.expected("`DROP`, `DELETE ROWS` or `PRESERVE ROWS`")),
        }
    }

    /// Builds a message for an unsupported statement prefix, naming the next keyword.
    fn unsupported_prefix(&self, prefix: &str) -> SqlFrontendError {
        match self.peek_keyword() {
            Some(keyword) if prefix.is_empty() => {
                SqlFrontendError::unsupported(format!("`{keyword}` table option is not supported by the cluster SQL gateway"))
            }
            Some(keyword) => SqlFrontendError::unsupported(format!("`{prefix} {keyword}` is not supported by the cluster SQL gateway")),
            None if prefix.is_empty() => SqlFrontendError::syntax("expected a table option, found end of statement"),
            None => SqlFrontendError::syntax(format!("`{prefix}` requires a statement object")),
        }
    }

    /// Builds a message for an unsupported `ALTER TABLE` action.
    fn unsupported_action(&self, prefix: &str) -> SqlFrontendError {
        match self.peek_keyword() {
            Some(keyword) => SqlFrontendError::unsupported(format!(
                "`{prefix} ... {keyword}` is not supported by the cluster SQL gateway, only `ADD COLUMN` is"
            )),
            None => SqlFrontendError::syntax(format!("`{prefix}` requires an action")),
        }
    }
}

fn is_constraint_keyword(word: &str) -> bool {
    let keyword = word.to_ascii_uppercase();
    CONSTRAINT_KEYWORDS.contains(&keyword.as_str())
}
