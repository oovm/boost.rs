//! Self-contained formula engine: lexer, parser, dependency graph and evaluator.
//!
//! The engine covers the arithmetic, comparison and text operators of the
//! SpreadsheetML grammar plus the function set the product exposes. Formulas are
//! parsed into an expression tree, evaluated in dependency order with a
//! depth-first walk and cached per cell in [`Cell::computed`]. Cycles are
//! detected across sheets and reported as the `#CYCLE!` error value.
//!
//! Numeric cells stay `f64`; integers lose their `.0` through
//! [`crate::model::format_number`] at every projection.

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    EvalReport, Workbook,
    model::{CellRange, CellValue, format_number, parse_reference},
};

/// Error code of a division by zero.
const DIV0: &str = "#DIV/0!";
/// Error code of an operand or result that cannot be coerced.
const VALUE: &str = "#VALUE!";
/// Error code of a reference that does not resolve.
const REF: &str = "#REF!";
/// Error code of an unknown function or name.
const NAME: &str = "#NAME?";
/// Error code of a lookup that found no match.
const NA: &str = "#N/A";
/// Error code of an argument outside the domain a function accepts.
const NUM: &str = "#NUM!";
/// Error code of a formula that depends on itself, directly or through others.
const CYCLE: &str = "#CYCLE!";

/// Functions this engine evaluates, sorted for a stable capability list.
pub const FUNCTIONS: &[&str] = &[
    "ABS",
    "AND",
    "AVERAGE",
    "AVERAGEIF",
    "AVERAGEIFS",
    "CEILING",
    "CHOOSE",
    "COLUMN",
    "COLUMNS",
    "CONCAT",
    "COUNT",
    "COUNTA",
    "COUNTIF",
    "COUNTIFS",
    "DATE",
    "DAY",
    "EDATE",
    "EOMONTH",
    "EXACT",
    "FIND",
    "FLOOR",
    "FV",
    "IF",
    "IFERROR",
    "IFS",
    "INDEX",
    "INT",
    "LARGE",
    "LEFT",
    "LEN",
    "LOWER",
    "MATCH",
    "MAX",
    "MAXIFS",
    "MEDIAN",
    "MID",
    "MIN",
    "MINIFS",
    "MOD",
    "MODE",
    "MONTH",
    "NOT",
    "NOW",
    "NPV",
    "OR",
    "PMT",
    "POWER",
    "PROPER",
    "PV",
    "RANK",
    "REPLACE",
    "RIGHT",
    "ROUND",
    "ROUNDDOWN",
    "ROUNDUP",
    "ROW",
    "ROWS",
    "SEARCH",
    "SIGN",
    "SMALL",
    "SQRT",
    "STDEV",
    "STDEVP",
    "SUBSTITUTE",
    "SUM",
    "SUMIF",
    "SUMIFS",
    "SUMPRODUCT",
    "SWITCH",
    "TEXTJOIN",
    "TODAY",
    "TRIM",
    "UPPER",
    "VAR",
    "VARP",
    "VLOOKUP",
    "WEEKDAY",
    "XLOOKUP",
    "YEAR",
];

/// Legacy and dotted Excel spellings folded onto a canonical [`FUNCTIONS`]
/// entry at dispatch, so `CONCATENATE` computes as `CONCAT`, `STDEV.S` as
/// `STDEV` and so on. Aliases share the single registry and evaluation path;
/// no alias carries its own implementation.
pub const FUNCTION_ALIASES: &[(&str, &str)] = &[
    ("CONCATENATE", "CONCAT"),
    ("MODE.SNGL", "MODE"),
    ("RANK.EQ", "RANK"),
    ("STDEV.P", "STDEVP"),
    ("STDEV.S", "STDEV"),
    ("VAR.P", "VARP"),
    ("VAR.S", "VAR"),
];

/// Resolves a called name to the canonical function that evaluates it,
/// ignoring ASCII case. Names that are not registered aliases pass through
/// unchanged, so near misses such as `CONCATENATEX` never fold onto `CONCAT`.
pub fn canonical_function(name: &str) -> &str {
    FUNCTION_ALIASES.iter().find(|(alias, _)| alias.eq_ignore_ascii_case(name)).map_or(name, |(_, target)| *target)
}

const MAX_ROW: u32 = 1_048_576;
const MAX_COLUMN: u32 = 16_384;
/// Upper bound on the cells one range argument may expand into.
const MAX_RANGE_CELLS: u64 = 200_000;

/// One formula cell, identified by sheet index, row and column.
pub type CellKey = (usize, u32, u32);

// ---------------------------------------------------------------------------
// Expression tree
// ---------------------------------------------------------------------------

/// Binary operators of the supported grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BinOp {
    Add,
    Subtract,
    Multiply,
    Divide,
    Power,
    Concat,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

/// One parsed formula body.
#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Number(f64),
    Text(String),
    Bool(bool),
    Error(String),
    Empty,
    /// A bare identifier that is neither a reference nor a boolean.
    Name(String),
    Cell {
        sheet: Option<String>,
        row: u32,
        column: u32,
    },
    Range {
        sheet: Option<String>,
        range: CellRange,
    },
    Negate(Box<Expr>),
    Percent(Box<Expr>),
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Call {
        name: String,
        args: Vec<Expr>,
    },
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(f64),
    Text(String),
    Ident(String),
    ErrorCode(String),
    Sheet(String),
    Plus,
    Minus,
    Star,
    Slash,
    Caret,
    Amp,
    Percent,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    LParen,
    RParen,
    Comma,
    Colon,
    Bang,
}

fn lex(source: &str) -> Result<Vec<Token>, String> {
    let characters: Vec<char> = source.chars().collect();
    let mut tokens = Vec::new();
    let mut index = 0usize;
    while index < characters.len() {
        let character = characters[index];
        if character.is_whitespace() {
            index += 1;
            continue;
        }
        match character {
            '+' => {
                tokens.push(Token::Plus);
                index += 1;
            }
            '-' => {
                tokens.push(Token::Minus);
                index += 1;
            }
            '*' => {
                tokens.push(Token::Star);
                index += 1;
            }
            '/' => {
                tokens.push(Token::Slash);
                index += 1;
            }
            '^' => {
                tokens.push(Token::Caret);
                index += 1;
            }
            '&' => {
                tokens.push(Token::Amp);
                index += 1;
            }
            '%' => {
                tokens.push(Token::Percent);
                index += 1;
            }
            '(' => {
                tokens.push(Token::LParen);
                index += 1;
            }
            ')' => {
                tokens.push(Token::RParen);
                index += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                index += 1;
            }
            ':' => {
                tokens.push(Token::Colon);
                index += 1;
            }
            '!' => {
                tokens.push(Token::Bang);
                index += 1;
            }
            '=' => {
                tokens.push(Token::Equal);
                index += 1;
            }
            '<' => match characters.get(index + 1) {
                Some('=') => {
                    tokens.push(Token::LessEqual);
                    index += 2;
                }
                Some('>') => {
                    tokens.push(Token::NotEqual);
                    index += 2;
                }
                _ => {
                    tokens.push(Token::Less);
                    index += 1;
                }
            },
            '>' => match characters.get(index + 1) {
                Some('=') => {
                    tokens.push(Token::GreaterEqual);
                    index += 2;
                }
                _ => {
                    tokens.push(Token::Greater);
                    index += 1;
                }
            },
            '"' => {
                let (text, next) = read_quoted(&characters, index + 1, '"', "string literal")?;
                tokens.push(Token::Text(text));
                index = next;
            }
            '\'' => {
                let (name, next) = read_quoted(&characters, index + 1, '\'', "sheet name")?;
                tokens.push(Token::Sheet(name));
                index = next;
            }
            '#' => {
                let start = index;
                index += 1;
                while index < characters.len()
                    && (characters[index].is_ascii_alphanumeric() || matches!(characters[index], '/' | '!' | '?' | '_' | '&'))
                {
                    index += 1;
                }
                tokens.push(Token::ErrorCode(characters[start..index].iter().collect()));
            }
            _ if character.is_ascii_digit() || (character == '.' && characters.get(index + 1).is_some_and(char::is_ascii_digit)) => {
                let start = index;
                let mut seen_dot = false;
                while index < characters.len() {
                    let current = characters[index];
                    if current.is_ascii_digit() {
                        index += 1;
                    }
                    else if current == '.' && !seen_dot {
                        seen_dot = true;
                        index += 1;
                    }
                    else if (current == 'e' || current == 'E') && exponent_length(&characters, index + 1).is_some() {
                        index += 1 + exponent_length(&characters, index + 1).unwrap_or(0);
                    }
                    else {
                        break;
                    }
                }
                let literal: String = characters[start..index].iter().collect();
                let number = literal.parse::<f64>().map_err(|_| format!("`{literal}` is not a number"))?;
                tokens.push(Token::Number(number));
            }
            _ if character.is_alphabetic() || character == '_' || character == '$' => {
                let start = index;
                while index < characters.len() && (characters[index].is_alphanumeric() || matches!(characters[index], '_' | '.' | '$')) {
                    index += 1;
                }
                tokens.push(Token::Ident(characters[start..index].iter().collect()));
            }
            other => return Err(format!("unexpected character `{other}`")),
        }
    }
    Ok(tokens)
}

/// Length of an exponent suffix (`3`, `+3`, `-3`) starting at `start`.
fn exponent_length(characters: &[char], start: usize) -> Option<usize> {
    let first = *characters.get(start)?;
    if first.is_ascii_digit() {
        return Some(1);
    }
    if (first == '+' || first == '-') && characters.get(start + 1).is_some_and(char::is_ascii_digit) {
        return Some(2);
    }
    None
}

/// Reads a delimited literal, honoring the doubled delimiter escape.
fn read_quoted(characters: &[char], start: usize, delimiter: char, label: &str) -> Result<(String, usize), String> {
    let mut index = start;
    let mut text = String::new();
    while index < characters.len() {
        if characters[index] == delimiter {
            if characters.get(index + 1) == Some(&delimiter) {
                text.push(delimiter);
                index += 2;
            }
            else {
                return Ok((text, index + 1));
            }
        }
        else {
            text.push(characters[index]);
            index += 1;
        }
    }
    Err(format!("unterminated {label}"))
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    tokens: Vec<Token>,
    index: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.index)
    }

    fn advance(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.index).cloned();
        if token.is_some() {
            self.index += 1;
        }
        token
    }

    fn eat(&mut self, token: &Token) -> bool {
        if self.peek() == Some(token) {
            self.index += 1;
            true
        }
        else {
            false
        }
    }

    fn expression(&mut self) -> Result<Expr, String> {
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Expr, String> {
        let mut left = self.concat()?;
        loop {
            let op = match self.peek() {
                Some(Token::Equal) => BinOp::Equal,
                Some(Token::NotEqual) => BinOp::NotEqual,
                Some(Token::Less) => BinOp::Less,
                Some(Token::LessEqual) => BinOp::LessEqual,
                Some(Token::Greater) => BinOp::Greater,
                Some(Token::GreaterEqual) => BinOp::GreaterEqual,
                _ => break,
            };
            self.index += 1;
            let right = self.concat()?;
            left = Expr::Binary { op, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn concat(&mut self) -> Result<Expr, String> {
        let mut left = self.additive()?;
        while self.eat(&Token::Amp) {
            let right = self.additive()?;
            left = Expr::Binary { op: BinOp::Concat, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn additive(&mut self) -> Result<Expr, String> {
        let mut left = self.term()?;
        loop {
            let op = match self.peek() {
                Some(Token::Plus) => BinOp::Add,
                Some(Token::Minus) => BinOp::Subtract,
                _ => break,
            };
            self.index += 1;
            let right = self.term()?;
            left = Expr::Binary { op, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn term(&mut self) -> Result<Expr, String> {
        let mut left = self.power()?;
        loop {
            let op = match self.peek() {
                Some(Token::Star) => BinOp::Multiply,
                Some(Token::Slash) => BinOp::Divide,
                _ => break,
            };
            self.index += 1;
            let right = self.power()?;
            left = Expr::Binary { op, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn power(&mut self) -> Result<Expr, String> {
        let mut left = self.unary()?;
        while self.eat(&Token::Caret) {
            let right = self.unary()?;
            left = Expr::Binary { op: BinOp::Power, left: Box::new(left), right: Box::new(right) };
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Some(Token::Minus) => {
                self.index += 1;
                Ok(Expr::Negate(Box::new(self.unary()?)))
            }
            Some(Token::Plus) => {
                self.index += 1;
                self.unary()
            }
            _ => self.postfix(),
        }
    }

    fn postfix(&mut self) -> Result<Expr, String> {
        let mut expression = self.primary()?;
        while self.eat(&Token::Percent) {
            expression = Expr::Percent(Box::new(expression));
        }
        Ok(expression)
    }

    fn primary(&mut self) -> Result<Expr, String> {
        let token = self.advance().ok_or_else(|| "the formula ends unexpectedly".to_owned())?;
        match token {
            Token::Number(number) => Ok(Expr::Number(number)),
            Token::Text(text) => Ok(Expr::Text(text)),
            Token::ErrorCode(code) => Ok(Expr::Error(code)),
            Token::LParen => {
                let inner = self.expression()?;
                if !self.eat(&Token::RParen) {
                    return Err("a `(` is not closed".to_owned());
                }
                Ok(inner)
            }
            Token::Sheet(name) => {
                if !self.eat(&Token::Bang) {
                    return Err("a quoted sheet name needs `!`".to_owned());
                }
                self.reference(Some(name))
            }
            Token::Ident(name) => {
                if self.eat(&Token::LParen) {
                    return self.call(name);
                }
                if self.eat(&Token::Bang) {
                    return self.reference(Some(name));
                }
                if name.eq_ignore_ascii_case("TRUE") {
                    return Ok(Expr::Bool(true));
                }
                if name.eq_ignore_ascii_case("FALSE") {
                    return Ok(Expr::Bool(false));
                }
                self.reference_from(None, &name)
            }
            other => Err(format!("unexpected token `{}`", label(&other))),
        }
    }

    /// Reads a cell or range reference after an optional sheet qualifier.
    fn reference(&mut self, sheet: Option<String>) -> Result<Expr, String> {
        let token = self.advance().ok_or_else(|| "a sheet qualifier needs a cell reference".to_owned())?;
        match token {
            Token::Ident(name) => self.reference_from(sheet, &name),
            Token::Sheet(other) => {
                if !self.eat(&Token::Bang) {
                    return Err("a quoted sheet name needs `!`".to_owned());
                }
                let next = self.advance().ok_or_else(|| "a sheet qualifier needs a cell reference".to_owned())?;
                match next {
                    Token::Ident(name) => self.reference_from(sheet.or(Some(other)), &name),
                    other => Err(format!("unexpected token `{}` after a sheet qualifier", label(&other))),
                }
            }
            other => Err(format!("unexpected token `{}` after `!`", label(&other))),
        }
    }

    fn reference_from(&mut self, sheet: Option<String>, name: &str) -> Result<Expr, String> {
        let Some((row, column)) = parse_reference(name)
        else {
            // A bare identifier is a name, which evaluates to `#NAME?`.
            return Ok(Expr::Name(name.to_owned()));
        };
        if !self.eat(&Token::Colon) {
            return Ok(Expr::Cell { sheet, row, column });
        }
        let token = self.advance().ok_or_else(|| "a range needs an end reference".to_owned())?;
        let end = match token {
            Token::Ident(name) => name,
            Token::Sheet(_) => {
                if !self.eat(&Token::Bang) {
                    return Err("a quoted sheet name needs `!`".to_owned());
                }
                match self.advance() {
                    Some(Token::Ident(name)) => name,
                    other => {
                        return Err(format!(
                            "unexpected token `{}` at the end of a range",
                            other.as_ref().map_or_else(|| "end of formula".to_owned(), label)
                        ));
                    }
                }
            }
            other => return Err(format!("unexpected token `{}` at the end of a range", label(&other))),
        };
        let (end_row, end_column) = parse_reference(&end).ok_or_else(|| format!("`{end}` is not a cell reference"))?;
        Ok(Expr::Range { sheet, range: CellRange::between(row, column, end_row, end_column) })
    }

    fn call(&mut self, name: String) -> Result<Expr, String> {
        let mut args = Vec::new();
        if self.eat(&Token::RParen) {
            return Ok(Expr::Call { name: name.to_ascii_uppercase(), args });
        }
        loop {
            if self.eat(&Token::Comma) {
                args.push(Expr::Empty);
                continue;
            }
            if self.eat(&Token::RParen) {
                args.push(Expr::Empty);
                break;
            }
            args.push(self.expression()?);
            if self.eat(&Token::Comma) {
                continue;
            }
            if !self.eat(&Token::RParen) {
                return Err("a function call is not closed with `)`".to_owned());
            }
            break;
        }
        Ok(Expr::Call { name: name.to_ascii_uppercase(), args })
    }
}

fn label(token: &Token) -> String {
    match token {
        Token::Number(number) => number.to_string(),
        Token::Text(text) => format!("\"{text}\""),
        Token::Ident(name) => name.clone(),
        Token::ErrorCode(code) => code.clone(),
        Token::Sheet(name) => format!("'{name}'"),
        Token::Plus => "+".to_owned(),
        Token::Minus => "-".to_owned(),
        Token::Star => "*".to_owned(),
        Token::Slash => "/".to_owned(),
        Token::Caret => "^".to_owned(),
        Token::Amp => "&".to_owned(),
        Token::Percent => "%".to_owned(),
        Token::Equal => "=".to_owned(),
        Token::NotEqual => "<>".to_owned(),
        Token::Less => "<".to_owned(),
        Token::LessEqual => "<=".to_owned(),
        Token::Greater => ">".to_owned(),
        Token::GreaterEqual => ">=".to_owned(),
        Token::LParen => "(".to_owned(),
        Token::RParen => ")".to_owned(),
        Token::Comma => ",".to_owned(),
        Token::Colon => ":".to_owned(),
        Token::Bang => "!".to_owned(),
    }
}

/// Parses a formula body, with or without the leading `=`.
fn parse_formula(source: &str) -> Result<Expr, String> {
    let trimmed = source.trim();
    let body = trimmed.strip_prefix('=').unwrap_or(trimmed).trim();
    if body.is_empty() {
        return Err("the formula body is empty".to_owned());
    }
    let tokens = lex(body)?;
    let mut parser = Parser { tokens, index: 0 };
    let expression = parser.expression()?;
    if parser.peek().is_some() {
        return Err(format!("unexpected trailing input in `{body}`"));
    }
    Ok(expression)
}

// ---------------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------------

fn error(code: &str) -> CellValue {
    CellValue::error(code)
}

fn normalize_zero(number: f64) -> f64 {
    if number == 0.0 { 0.0 } else { number }
}

/// Coerces one value for arithmetic: empty is `0`, text is `#VALUE!`.
fn numeric(value: &CellValue) -> Result<f64, CellValue> {
    match value {
        CellValue::Number { number } => Ok(*number),
        CellValue::Bool { value } => Ok(if *value { 1.0 } else { 0.0 }),
        CellValue::Empty => Ok(0.0),
        CellValue::Error { .. } => Err(value.clone()),
        CellValue::Text { .. } => Err(error(VALUE)),
    }
}

/// Coerces one value for text concatenation.
fn as_text(value: &CellValue) -> Result<String, CellValue> {
    match value {
        CellValue::Text { text } => Ok(text.clone()),
        CellValue::Number { number } => Ok(format_number(*number)),
        CellValue::Bool { value } => Ok(if *value { "TRUE".to_owned() } else { "FALSE".to_owned() }),
        CellValue::Empty => Ok(String::new()),
        CellValue::Error { .. } => Err(value.clone()),
    }
}

fn truthy(value: &CellValue) -> Result<bool, CellValue> {
    match value {
        CellValue::Bool { value } => Ok(*value),
        CellValue::Number { number } => Ok(*number != 0.0),
        CellValue::Empty => Ok(false),
        CellValue::Error { .. } => Err(value.clone()),
        CellValue::Text { .. } => Err(error(VALUE)),
    }
}

fn rank(value: &CellValue) -> u8 {
    match value {
        CellValue::Empty | CellValue::Number { .. } => 0,
        CellValue::Text { .. } => 1,
        CellValue::Bool { .. } => 2,
        CellValue::Error { .. } => 3,
    }
}

/// Orders two values the way a comparison operator does.
fn compare(left: &CellValue, right: &CellValue) -> Result<Ordering, CellValue> {
    match (left, right) {
        (CellValue::Error { .. }, _) => Err(left.clone()),
        (_, CellValue::Error { .. }) => Err(right.clone()),
        (CellValue::Empty, CellValue::Empty) => Ok(Ordering::Equal),
        (CellValue::Text { text: left }, CellValue::Text { text: right }) => Ok(left.to_lowercase().cmp(&right.to_lowercase())),
        (CellValue::Text { text }, CellValue::Empty) => Ok(text.to_lowercase().cmp(&String::new())),
        (CellValue::Empty, CellValue::Text { text }) => Ok(String::new().cmp(&text.to_lowercase())),
        (
            CellValue::Empty | CellValue::Number { .. } | CellValue::Bool { .. },
            CellValue::Empty | CellValue::Number { .. } | CellValue::Bool { .. },
        ) => {
            let left = numeric(left).unwrap_or(0.0);
            let right = numeric(right).unwrap_or(0.0);
            Ok(left.partial_cmp(&right).unwrap_or(Ordering::Equal))
        }
        _ => Ok(rank(left).cmp(&rank(right))),
    }
}

/// One evaluated function argument.
enum Argument {
    Value(CellValue),
    Grid(Grid),
    /// Resolution failed before any cell could be read.
    Invalid(CellValue),
}

/// A rectangular range flattened row by row, together with its shape.
struct Grid {
    values: Vec<CellValue>,
    rows: u32,
    columns: u32,
}

impl Grid {
    fn value(&self, row: u32, column: u32) -> Option<&CellValue> {
        if row == 0 || column == 0 || row > self.rows || column > self.columns {
            return None;
        }
        self.values.get(((row - 1) * self.columns + (column - 1)) as usize)
    }
}

struct Evaluator<'a> {
    workbook: &'a dyn Workbook,
    forms: HashMap<CellKey, Option<Expr>>,
    values: HashMap<CellKey, CellValue>,
    states: HashMap<CellKey, u8>,
    stack: Vec<CellKey>,
    /// Cells whose formula used a function or construct the engine does not
    /// evaluate. Their imported cached value stays the effective value.
    unsupported: HashSet<CellKey>,
}

impl Evaluator<'_> {
    /// Marks the formula cell currently being evaluated as unsupported.
    fn mark_unsupported(&mut self) {
        if let Some(key) = self.stack.last().copied() {
            self.unsupported.insert(key);
        }
    }

    fn resolve_sheet(&self, sheet: Option<&str>, current: usize) -> Option<usize> {
        match sheet {
            None => Some(current),
            Some(name) => self.workbook.sheet_index(name),
        }
    }

    fn cell_value(&mut self, sheet: usize, row: u32, column: u32) -> CellValue {
        if row == 0 || row > MAX_ROW || column == 0 || column > MAX_COLUMN {
            return error(REF);
        }
        let Some(cell) = self.workbook.cell(sheet, row, column)
        else {
            return CellValue::Empty;
        };
        if cell.has_formula { self.eval_cell(sheet, row, column) } else { cell.value }
    }

    fn eval_cell(&mut self, sheet: usize, row: u32, column: u32) -> CellValue {
        let key = (sheet, row, column);
        match self.states.get(&key).copied() {
            Some(2) => return self.values.get(&key).cloned().unwrap_or(CellValue::Empty),
            Some(1) => {
                if let Some(position) = self.stack.iter().position(|entry| *entry == key) {
                    let members: Vec<CellKey> = self.stack[position..].to_vec();
                    for member in members {
                        self.states.insert(member, 2);
                        self.values.insert(member, error(CYCLE));
                    }
                }
                return error(CYCLE);
            }
            _ => {}
        }
        self.states.insert(key, 1);
        self.stack.push(key);
        let form = self.forms.get(&key).cloned().flatten();
        let evaluated = match form {
            Some(expression) => self.eval(&expression, sheet),
            None => error(VALUE),
        };
        // An unsupported formula keeps the value the source writer cached, so
        // opening and re-exporting a valid workbook never replaces a real
        // result with `#NAME?`.
        let value = if self.unsupported.contains(&key) {
            self.workbook.cell(sheet, row, column).and_then(|cell| cell.cached).unwrap_or_else(|| error(NAME))
        }
        else {
            evaluated
        };
        self.stack.pop();
        if self.states.get(&key).copied() == Some(1) {
            self.states.insert(key, 2);
            self.values.insert(key, value.clone());
        }
        value
    }

    fn grid(&mut self, sheet: Option<&str>, range: &CellRange, current: usize) -> Result<Grid, CellValue> {
        let Some(index) = self.resolve_sheet(sheet, current)
        else {
            return Err(error(REF));
        };
        self.grid_at(index, range)
    }

    /// Flattens one validated range of one resolved sheet into a grid.
    fn grid_at(&mut self, index: usize, range: &CellRange) -> Result<Grid, CellValue> {
        if !range.is_valid() || range.end_row > MAX_ROW || range.end_column > MAX_COLUMN {
            return Err(error(REF));
        }
        let area = u64::from(range.rows()) * u64::from(range.columns());
        if area > MAX_RANGE_CELLS {
            return Err(error(REF));
        }
        let mut values = Vec::with_capacity(area as usize);
        for row in range.start_row..=range.end_row {
            for column in range.start_column..=range.end_column {
                values.push(self.cell_value(index, row, column));
            }
        }
        Ok(Grid { values, rows: range.rows(), columns: range.columns() })
    }

    /// Resolves one name used as a function argument: a defined range becomes a
    /// grid, a single-cell name becomes its value and an unknown name keeps the
    /// `#NAME?` fallback.
    fn name_argument(&mut self, name: &str) -> Argument {
        let Some(named) = self.workbook.named_range(name)
        else {
            self.mark_unsupported();
            return Argument::Value(error(NAME));
        };
        let Some(index) = self.workbook.sheet_index(&named.sheet)
        else {
            return Argument::Invalid(error(REF));
        };
        if !named.range.is_valid() {
            return Argument::Invalid(error(REF));
        }
        if named.range.start_row == named.range.end_row && named.range.start_column == named.range.end_column {
            Argument::Value(self.cell_value(index, named.range.start_row, named.range.start_column))
        }
        else {
            match self.grid_at(index, &named.range) {
                Ok(grid) => Argument::Grid(grid),
                Err(error) => Argument::Invalid(error),
            }
        }
    }

    fn eval(&mut self, expression: &Expr, current: usize) -> CellValue {
        match expression {
            Expr::Number(number) => CellValue::number(*number),
            Expr::Text(text) => CellValue::text(text),
            Expr::Bool(value) => CellValue::boolean(*value),
            Expr::Error(code) => error(code),
            Expr::Empty => CellValue::Empty,
            Expr::Name(name) => {
                let Some(named) = self.workbook.named_range(name)
                else {
                    self.mark_unsupported();
                    return error(NAME);
                };
                let Some(index) = self.workbook.sheet_index(&named.sheet)
                else {
                    return error(REF);
                };
                if !named.range.is_valid() {
                    return error(REF);
                }
                if named.range.start_row == named.range.end_row && named.range.start_column == named.range.end_column {
                    self.cell_value(index, named.range.start_row, named.range.start_column)
                }
                else {
                    // A multi-cell name used as one scalar is the same
                    // construct as `Expr::Range`: keep the imported cache
                    // instead of replacing it with `#VALUE!`.
                    self.mark_unsupported();
                    error(VALUE)
                }
            }
            Expr::Cell { sheet, row, column } => {
                let Some(index) = self.resolve_sheet(sheet.as_deref(), current)
                else {
                    return error(REF);
                };
                self.cell_value(index, *row, *column)
            }
            Expr::Range { .. } => {
                // A range used as one scalar is an array formula or an
                // unimplemented construct. Keep the imported cache instead of
                // replacing it with `#VALUE!`.
                self.mark_unsupported();
                error(VALUE)
            }
            Expr::Negate(inner) => match numeric(&self.eval(inner, current)) {
                Ok(number) => CellValue::number(normalize_zero(-number)),
                Err(error) => error,
            },
            Expr::Percent(inner) => match numeric(&self.eval(inner, current)) {
                Ok(number) => CellValue::number(normalize_zero(number / 100.0)),
                Err(error) => error,
            },
            Expr::Binary { op, left, right } => self.binary(*op, left, right, current),
            Expr::Call { name, args } => self.call(name, args, current),
        }
    }

    fn binary(&mut self, op: BinOp, left: &Expr, right: &Expr, current: usize) -> CellValue {
        let left = self.eval(left, current);
        if let CellValue::Error { .. } = left {
            return left;
        }
        let right = self.eval(right, current);
        if let CellValue::Error { .. } = right {
            return right;
        }
        match op {
            BinOp::Concat => match (as_text(&left), as_text(&right)) {
                (Ok(left), Ok(right)) => CellValue::text(format!("{left}{right}")),
                (Err(error), _) | (_, Err(error)) => error,
            },
            BinOp::Equal | BinOp::NotEqual | BinOp::Less | BinOp::LessEqual | BinOp::Greater | BinOp::GreaterEqual => {
                let ordering = match compare(&left, &right) {
                    Ok(ordering) => ordering,
                    Err(error) => return error,
                };
                let result = match op {
                    BinOp::Equal => ordering == Ordering::Equal,
                    BinOp::NotEqual => ordering != Ordering::Equal,
                    BinOp::Less => ordering == Ordering::Less,
                    BinOp::LessEqual => ordering != Ordering::Greater,
                    BinOp::Greater => ordering == Ordering::Greater,
                    _ => ordering != Ordering::Less,
                };
                CellValue::boolean(result)
            }
            _ => {
                let left = match numeric(&left) {
                    Ok(number) => number,
                    Err(error) => return error,
                };
                let right = match numeric(&right) {
                    Ok(number) => number,
                    Err(error) => return error,
                };
                let result = match op {
                    BinOp::Add => left + right,
                    BinOp::Subtract => left - right,
                    BinOp::Multiply => left * right,
                    BinOp::Divide => {
                        if right == 0.0 {
                            return error(DIV0);
                        }
                        left / right
                    }
                    BinOp::Power => left.powf(right),
                    _ => return error(VALUE),
                };
                if !result.is_finite() {
                    return error(VALUE);
                }
                CellValue::number(normalize_zero(result))
            }
        }
    }

    fn call(&mut self, name: &str, args: &[Expr], current: usize) -> CellValue {
        let name = canonical_function(name);
        if !FUNCTIONS.contains(&name) {
            // The engine does not know this function. Mark the cell so the
            // imported cached value survives instead of `#NAME?`.
            self.mark_unsupported();
            return error(NAME);
        }
        match name {
            "IF" => return self.function_if(args, current),
            "IFERROR" => return self.function_iferror(args, current),
            "IFS" => return self.function_ifs(args, current),
            "SWITCH" => return self.function_switch(args, current),
            "CHOOSE" => return self.function_choose(args, current),
            "AND" | "OR" => return self.function_logical(name == "AND", args, current),
            // XLOOKUP evaluates its `if_not_found` argument only when the
            // lookup fails, so a fallback that would raise never runs on a hit.
            "XLOOKUP" => return self.function_xlookup(args, current),
            // ROW/COLUMN read the address of their reference or of the cell
            // that holds the formula, so they need the evaluation stack.
            "ROW" | "COLUMN" => return self.function_position(name == "ROW", args),
            _ => {}
        }
        let mut arguments = Vec::with_capacity(args.len());
        for argument in args {
            arguments.push(match argument {
                Expr::Range { sheet, range } => match self.grid(sheet.as_deref(), range, current) {
                    Ok(grid) => Argument::Grid(grid),
                    Err(error) => Argument::Invalid(error),
                },
                Expr::Name(name) => self.name_argument(name),
                _ => Argument::Value(self.eval(argument, current)),
            });
        }
        apply_function(name, &arguments)
    }

    fn function_if(&mut self, args: &[Expr], current: usize) -> CellValue {
        let Some(condition) = args.first()
        else {
            return error(VALUE);
        };
        if args.len() > 3 {
            return error(VALUE);
        }
        let condition = self.eval(condition, current);
        let truth = match truthy(&condition) {
            Ok(truth) => truth,
            Err(error) => return error,
        };
        let chosen = if truth { args.get(1) } else { args.get(2) };
        match chosen {
            Some(expression) if !matches!(expression, Expr::Empty) => self.eval(expression, current),
            _ => CellValue::boolean(false),
        }
    }

    /// `IFERROR(value, fallback)` evaluates the fallback only for an error.
    fn function_iferror(&mut self, args: &[Expr], current: usize) -> CellValue {
        if args.len() != 2 {
            return error(VALUE);
        }
        let value = self.eval(&args[0], current);
        if matches!(value, CellValue::Error { .. }) { self.eval(&args[1], current) } else { value }
    }

    /// `AND`/`OR` over scalars and range members, short-circuiting on the
    /// deciding value. Text inside a range is ignored, matching a spreadsheet.
    fn function_logical(&mut self, and: bool, args: &[Expr], current: usize) -> CellValue {
        if args.is_empty() {
            return error(VALUE);
        }
        for argument in args {
            match argument {
                Expr::Range { sheet, range } => match self.grid(sheet.as_deref(), range, current) {
                    Ok(grid) => {
                        for value in &grid.values {
                            let truth = match value {
                                CellValue::Bool { value } => Some(*value),
                                CellValue::Number { number } => Some(*number != 0.0),
                                CellValue::Error { .. } => return value.clone(),
                                _ => None,
                            };
                            if let Some(truth) = truth
                                && truth != and
                            {
                                return CellValue::boolean(!and);
                            }
                        }
                    }
                    Err(error) => return error,
                },
                _ => {
                    let value = self.eval(argument, current);
                    match truthy(&value) {
                        Ok(truth) => {
                            if truth != and {
                                return CellValue::boolean(!and);
                            }
                        }
                        Err(error) => return error,
                    }
                }
            }
        }
        CellValue::boolean(and)
    }

    /// `XLOOKUP(lookup, lookup_array, return_array, [if_not_found], [match_mode], [search_mode])`.
    ///
    /// Supports exact (`0`), exact-or-next-smaller (`-1`), exact-or-next-larger
    /// (`1`) and wildcard (`2`) matching. Binary search modes fall back to a
    /// linear walk, which yields the same matches.
    fn function_xlookup(&mut self, args: &[Expr], current: usize) -> CellValue {
        if !(3..=6).contains(&args.len()) {
            return error(VALUE);
        }
        let lookup = self.eval(&args[0], current);
        if let CellValue::Error { .. } = lookup {
            return lookup;
        }
        let lookup_grid = match &args[1] {
            Expr::Range { sheet, range } => match self.grid(sheet.as_deref(), range, current) {
                Ok(grid) => grid,
                Err(error) => return error,
            },
            _ => return error(VALUE),
        };
        let return_grid = match &args[2] {
            Expr::Range { sheet, range } => match self.grid(sheet.as_deref(), range, current) {
                Ok(grid) => grid,
                Err(error) => return error,
            },
            _ => return error(VALUE),
        };
        let match_mode = match self.optional_integer(args.get(4), current) {
            Ok(mode) => mode.unwrap_or(0),
            Err(error) => return error,
        };
        if !matches!(match_mode, -1 | 0 | 1 | 2) {
            return error(VALUE);
        }
        let search_mode = match self.optional_integer(args.get(5), current) {
            Ok(mode) => mode.unwrap_or(1),
            Err(error) => return error,
        };
        if !matches!(search_mode, 1 | -1 | 2 | -2) {
            return error(VALUE);
        }
        let vertical = if lookup_grid.columns == 1 {
            true
        }
        else if lookup_grid.rows == 1 {
            false
        }
        else {
            return error(VALUE);
        };
        let length = if vertical { lookup_grid.rows } else { lookup_grid.columns };
        let key_at = |index: u32| -> CellValue {
            if vertical {
                lookup_grid.value(index, 1).cloned().unwrap_or(CellValue::Empty)
            }
            else {
                lookup_grid.value(1, index).cloned().unwrap_or(CellValue::Empty)
            }
        };

        let reverse = search_mode == -1;
        let mut found: Option<u32> = None;
        if match_mode == 2 {
            let pattern = match as_text(&lookup) {
                Ok(text) => text,
                Err(error) => return error,
            };
            for index in 1..=length {
                let position = if reverse { length - index + 1 } else { index };
                if let CellValue::Text { text } = key_at(position)
                    && wildcard_match(&pattern, &text)
                {
                    found = Some(position);
                    break;
                }
            }
        }
        else if match_mode == 0 {
            for index in 1..=length {
                let position = if reverse { length - index + 1 } else { index };
                match compare(&key_at(position), &lookup) {
                    Ok(Ordering::Equal) => {
                        found = Some(position);
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => return error,
                }
            }
        }
        else {
            let mut best = None;
            for index in 1..=length {
                let key = key_at(index);
                let ordering = match compare(&key, &lookup) {
                    Ok(ordering) => ordering,
                    Err(error) => return error,
                };
                let eligible = if match_mode == -1 { ordering != Ordering::Greater } else { ordering != Ordering::Less };
                if !eligible {
                    continue;
                }
                let better = match best {
                    None => true,
                    Some(existing) => match compare(&key, &key_at(existing)) {
                        Ok(ordering) => {
                            if match_mode == -1 {
                                ordering == Ordering::Greater
                            }
                            else {
                                ordering == Ordering::Less
                            }
                        }
                        Err(error) => return error,
                    },
                };
                if better {
                    best = Some(index);
                }
            }
            found = best;
        }

        let Some(position) = found
        else {
            if let Some(fallback) = args.get(3)
                && !matches!(fallback, Expr::Empty)
            {
                return self.eval(fallback, current);
            }
            return error(NA);
        };
        if vertical {
            if position > return_grid.rows {
                return error(REF);
            }
            return_grid.value(position, 1).cloned().unwrap_or(CellValue::Empty)
        }
        else {
            if position > return_grid.columns {
                return error(REF);
            }
            return_grid.value(1, position).cloned().unwrap_or(CellValue::Empty)
        }
    }

    /// `ROW([reference])` and `COLUMN([reference])`: the row or column of the
    /// top-left corner of the reference, or of the formula cell when omitted.
    fn function_position(&mut self, row: bool, args: &[Expr]) -> CellValue {
        if args.len() > 1 {
            return error(VALUE);
        }
        let vertical = row;
        let coordinate = |target_row: u32, target_column: u32| f64::from(if vertical { target_row } else { target_column });
        match args.first() {
            None | Some(Expr::Empty) => match self.stack.last() {
                Some((_, current_row, current_column)) => CellValue::number(coordinate(*current_row, *current_column)),
                None => error(VALUE),
            },
            Some(Expr::Cell { row: target_row, column: target_column, .. }) => CellValue::number(coordinate(*target_row, *target_column)),
            Some(Expr::Range { range, .. }) => CellValue::number(coordinate(range.start_row, range.start_column)),
            Some(_) => error(VALUE),
        }
    }

    /// Reads an optional numeric argument, treating a gap as absent.
    fn optional_integer(&mut self, argument: Option<&Expr>, current: usize) -> Result<Option<i64>, CellValue> {
        match argument {
            None | Some(Expr::Empty) => Ok(None),
            Some(expression) => {
                let value = self.eval(expression, current);
                numeric(&value).map(|number| Some(number.trunc() as i64))
            }
        }
    }

    /// `IFS(condition1, value1, ...)`: the value of the first true condition.
    /// Conditions and values are evaluated only as far as needed, so a raising
    /// branch after the first match never runs.
    fn function_ifs(&mut self, args: &[Expr], current: usize) -> CellValue {
        if args.len() < 2 || args.len() % 2 != 0 {
            return error(VALUE);
        }
        let mut index = 0;
        while index < args.len() {
            let condition = self.eval(&args[index], current);
            match truthy(&condition) {
                Ok(true) => return self.eval(&args[index + 1], current),
                Ok(false) => {}
                Err(error) => return error,
            }
            index += 2;
        }
        error(NA)
    }

    /// `SWITCH(expression, value1, result1, ..., [default])`. The subject is
    /// evaluated once; a trailing odd argument is the default.
    fn function_switch(&mut self, args: &[Expr], current: usize) -> CellValue {
        if args.len() < 3 {
            return error(VALUE);
        }
        let subject = self.eval(&args[0], current);
        if let CellValue::Error { .. } = subject {
            return subject;
        }
        let has_default = args.len() % 2 == 0;
        let pair_limit = if has_default { args.len() - 1 } else { args.len() };
        let mut index = 1;
        while index + 1 < pair_limit {
            let candidate = self.eval(&args[index], current);
            if let CellValue::Error { .. } = candidate {
                return candidate;
            }
            match compare(&subject, &candidate) {
                Ok(Ordering::Equal) => return self.eval(&args[index + 1], current),
                Ok(_) => {}
                Err(error) => return error,
            }
            index += 2;
        }
        if has_default { self.eval(&args[args.len() - 1], current) } else { error(NA) }
    }

    /// `CHOOSE(index, value1, ...)`: evaluates only the selected value.
    fn function_choose(&mut self, args: &[Expr], current: usize) -> CellValue {
        if args.len() < 2 {
            return error(VALUE);
        }
        let index = self.eval(&args[0], current);
        let index = match numeric(&index) {
            Ok(number) if number.is_finite() => number.trunc(),
            Ok(_) => return error(VALUE),
            Err(error) => return error,
        };
        if index < 1.0 || index >= args.len() as f64 {
            return error(VALUE);
        }
        self.eval(&args[index as usize], current)
    }
}

fn scalar(args: &[Argument], index: usize) -> Option<CellValue> {
    match args.get(index)? {
        Argument::Value(value) => Some(value.clone()),
        Argument::Grid(grid) => grid.values.first().cloned(),
        Argument::Invalid(value) => Some(value.clone()),
    }
}

fn numbers(args: &[Argument]) -> Result<Vec<f64>, CellValue> {
    let mut values = Vec::new();
    for argument in args {
        match argument {
            Argument::Grid(grid) => {
                for item in &grid.values {
                    match item {
                        CellValue::Number { number } => values.push(*number),
                        CellValue::Error { .. } => return Err(item.clone()),
                        _ => {}
                    }
                }
            }
            Argument::Value(value) => match value {
                CellValue::Number { number } => values.push(*number),
                CellValue::Bool { value } => values.push(if *value { 1.0 } else { 0.0 }),
                CellValue::Empty => {}
                CellValue::Error { .. } => return Err(value.clone()),
                CellValue::Text { .. } => return Err(error(VALUE)),
            },
            Argument::Invalid(value) => return Err(value.clone()),
        }
    }
    Ok(values)
}

fn count_numbers(args: &[Argument]) -> usize {
    let mut count = 0;
    for argument in args {
        match argument {
            Argument::Value(CellValue::Number { .. }) => count += 1,
            Argument::Grid(grid) => count += grid.values.iter().filter(|item| matches!(item, CellValue::Number { .. })).count(),
            _ => {}
        }
    }
    count
}

fn count_non_empty(args: &[Argument]) -> usize {
    let mut count = 0;
    for argument in args {
        match argument {
            Argument::Value(value) => {
                if !value.is_empty() {
                    count += 1;
                }
            }
            Argument::Grid(grid) => count += grid.values.iter().filter(|item| !item.is_empty()).count(),
            Argument::Invalid(_) => {}
        }
    }
    count
}

fn single_text(args: &[Argument]) -> Result<String, CellValue> {
    if args.len() != 1 {
        return Err(error(VALUE));
    }
    match scalar(args, 0) {
        Some(value) => as_text(&value),
        None => Err(error(VALUE)),
    }
}

fn single_number(args: &[Argument]) -> Result<f64, CellValue> {
    if args.len() != 1 {
        return Err(error(VALUE));
    }
    match scalar(args, 0) {
        Some(value) => numeric(&value),
        None => Err(error(VALUE)),
    }
}

/// Rounds half away from zero, like a spreadsheet does.
fn round_half_away(number: f64, digits: f64) -> f64 {
    let digits = digits.clamp(-15.0, 15.0) as i32;
    let factor = 10f64.powi(digits);
    let scaled = number * factor;
    let rounded = if scaled >= 0.0 { (scaled + 0.5).floor() } else { (scaled - 0.5).ceil() };
    normalize_zero(rounded / factor)
}

/// Collapses inner whitespace runs to one space and trims both ends.
fn trim_text(text: &str) -> String {
    let mut result = String::new();
    let mut pending_space = false;
    for character in text.chars() {
        if character == ' ' {
            pending_space = !result.is_empty();
        }
        else {
            if pending_space {
                result.push(' ');
                pending_space = false;
            }
            result.push(character);
        }
    }
    result
}

fn text_argument(args: &[Argument], index: usize) -> Result<String, CellValue> {
    match scalar(args, index) {
        Some(value) => as_text(&value),
        None => Err(error(VALUE)),
    }
}

fn number_argument(args: &[Argument], index: usize) -> Result<f64, CellValue> {
    match scalar(args, index) {
        Some(value) => numeric(&value),
        None => Err(error(VALUE)),
    }
}

fn integer_argument(args: &[Argument], index: usize) -> Result<i64, CellValue> {
    number_argument(args, index).map(|number| number.trunc() as i64)
}

fn grid_argument(args: &[Argument], index: usize) -> Option<&Grid> {
    match args.get(index)? {
        Argument::Grid(grid) => Some(grid),
        _ => None,
    }
}

/// Counts one `num_chars` argument, rejecting negative widths.
fn character_count(number: f64) -> Result<usize, CellValue> {
    if number < 0.0 {
        return Err(error(VALUE));
    }
    Ok(number.trunc() as usize)
}

fn round_toward(number: f64, digits: f64, up: bool) -> f64 {
    let digits = digits.clamp(-15.0, 15.0) as i32;
    let factor = 10f64.powi(digits);
    let scaled = number * factor;
    let magnitude = scaled.abs();
    let rounded = if up { magnitude.ceil() } else { magnitude.floor() };
    normalize_zero(if scaled < 0.0 { -rounded / factor } else { rounded / factor })
}

/// Replaces the nth occurrence of a needle, leaving every other one intact.
fn replace_nth(text: &str, needle: &str, replacement: &str, instance: usize) -> String {
    let mut output = String::new();
    let mut last = 0usize;
    let mut count = 0usize;
    for (index, _) in text.match_indices(needle) {
        count += 1;
        if count == instance {
            output.push_str(&text[last..index]);
            output.push_str(replacement);
            last = index + needle.len();
        }
    }
    output.push_str(&text[last..]);
    output
}

fn function_vlookup(args: &[Argument]) -> CellValue {
    if !(3..=4).contains(&args.len()) {
        return error(VALUE);
    }
    let Some(grid) = grid_argument(args, 1)
    else {
        return error(VALUE);
    };
    let Some(lookup) = scalar(args, 0)
    else {
        return error(VALUE);
    };
    let column = match integer_argument(args, 2) {
        Ok(column) => column,
        Err(error) => return error,
    };
    if column < 1 {
        return error(VALUE);
    }
    if column as u32 > grid.columns {
        return error(REF);
    }
    let approximate = if args.len() == 4 {
        match scalar(args, 3).map(|value| truthy(&value)) {
            Some(Ok(approximate)) => approximate,
            Some(Err(error)) => return error,
            None => true,
        }
    }
    else {
        true
    };
    let mut found: Option<u32> = None;
    for row in 1..=grid.rows {
        let key = grid.value(row, 1).cloned().unwrap_or(CellValue::Empty);
        let ordering = match compare(&key, &lookup) {
            Ok(ordering) => ordering,
            Err(error) => return error,
        };
        if ordering == Ordering::Equal {
            found = Some(row);
            break;
        }
        if approximate {
            if ordering == Ordering::Less {
                found = Some(row);
            }
            else {
                break;
            }
        }
    }
    let Some(row) = found
    else {
        return error(NA);
    };
    grid.value(row, column as u32).cloned().unwrap_or(CellValue::Empty)
}

fn function_index(args: &[Argument]) -> CellValue {
    if !(2..=3).contains(&args.len()) {
        return error(VALUE);
    }
    let Some(grid) = grid_argument(args, 0)
    else {
        return error(VALUE);
    };
    let row = match integer_argument(args, 1) {
        Ok(row) => row,
        Err(error) => return error,
    };
    let column = if args.len() == 3 {
        match integer_argument(args, 2) {
            Ok(column) => column,
            Err(error) => return error,
        }
    }
    else {
        1
    };
    if row < 1 || column < 1 {
        return error(VALUE);
    }
    let (row, column) = (row as u32, column as u32);
    if row > grid.rows || column > grid.columns {
        return error(REF);
    }
    grid.value(row, column).cloned().unwrap_or(CellValue::Empty)
}

fn function_match(args: &[Argument]) -> CellValue {
    if !(2..=3).contains(&args.len()) {
        return error(VALUE);
    }
    let Some(lookup) = scalar(args, 0)
    else {
        return error(VALUE);
    };
    let Some(grid) = grid_argument(args, 1)
    else {
        return error(VALUE);
    };
    let match_type = if args.len() == 3 {
        match integer_argument(args, 2) {
            Ok(match_type) => match_type,
            Err(error) => return error,
        }
    }
    else {
        1
    };
    if !matches!(match_type, -1 | 0 | 1) {
        return error(VALUE);
    }
    let mut found: Option<u32> = None;
    for (index, value) in grid.values.iter().enumerate() {
        let ordering = match compare(value, &lookup) {
            Ok(ordering) => ordering,
            Err(error) => return error,
        };
        if match_type == 0 {
            if ordering == Ordering::Equal {
                return CellValue::number(index as f64 + 1.0);
            }
            continue;
        }
        if match_type == 1 {
            if ordering == Ordering::Greater {
                break;
            }
            found = Some(index as u32 + 1);
        }
        else {
            if ordering == Ordering::Less {
                break;
            }
            found = Some(index as u32 + 1);
        }
    }
    match found {
        Some(position) => CellValue::number(f64::from(position)),
        None => error(NA),
    }
}

// ---------------------------------------------------------------------------
// Statistics and arithmetic
// ---------------------------------------------------------------------------

/// Two-pass variance; `sample` divides by `n - 1`, the population by `n`.
fn variance(values: &[f64], sample: bool) -> Option<f64> {
    let count = values.len();
    if count == 0 || (sample && count < 2) {
        return None;
    }
    let mean = values.iter().sum::<f64>() / count as f64;
    let squares = values.iter().map(|value| (value - mean).powi(2)).sum::<f64>();
    Some(squares / if sample { (count - 1) as f64 } else { count as f64 })
}

fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let middle = sorted.len() / 2;
    Some(if sorted.len() % 2 == 0 { (sorted[middle - 1] + sorted[middle]) / 2.0 } else { sorted[middle] })
}

/// The `k`th largest (`k` is one-based) or smallest value, or `None` when out of range.
fn nth_value(values: &[f64], k: usize, largest: bool) -> Option<f64> {
    if k == 0 || k > values.len() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    Some(if largest { sorted[sorted.len() - k] } else { sorted[k - 1] })
}

fn function_large_small(args: &[Argument], largest: bool) -> CellValue {
    if args.len() != 2 {
        return error(VALUE);
    }
    let values = match numbers(&args[..1]) {
        Ok(values) => values,
        Err(error) => return error,
    };
    let k = match integer_argument(args, 1) {
        Ok(k) => k,
        Err(error) => return error,
    };
    let Some(k) = usize::try_from(k).ok().filter(|k| *k >= 1)
    else {
        return error(NUM);
    };
    match nth_value(&values, k, largest) {
        Some(value) => CellValue::number(normalize_zero(value)),
        None => error(NUM),
    }
}

/// Multiplies equal-length arrays item by item and sums the products.
fn function_sumproduct(args: &[Argument]) -> CellValue {
    if args.is_empty() {
        return error(VALUE);
    }
    let mut grids = Vec::with_capacity(args.len());
    for argument in args {
        match argument {
            Argument::Grid(grid) => grids.push(grid),
            _ => return error(VALUE),
        }
    }
    let first = grids[0];
    if grids.iter().any(|grid| !same_shape(grid, first)) {
        return error(VALUE);
    }
    let mut total = 0.0;
    for index in 0..first.values.len() {
        let mut product = 1.0;
        for grid in &grids {
            match grid.values.get(index) {
                Some(CellValue::Number { number }) => product *= number,
                Some(value @ CellValue::Error { .. }) => return value.clone(),
                _ => product = 0.0,
            }
        }
        total += product;
    }
    CellValue::number(normalize_zero(total))
}

fn function_floor_ceiling(args: &[Argument], up: bool) -> CellValue {
    if args.len() != 2 {
        return error(VALUE);
    }
    let number = match number_argument(args, 0) {
        Ok(number) => number,
        Err(error) => return error,
    };
    let significance = match number_argument(args, 1) {
        Ok(significance) => significance,
        Err(error) => return error,
    };
    if significance == 0.0 {
        return error(DIV0);
    }
    let scaled = number / significance;
    let rounded = if up { scaled.ceil() } else { scaled.floor() };
    CellValue::number(normalize_zero(rounded * significance))
}

fn function_rank(args: &[Argument]) -> CellValue {
    if !(2..=3).contains(&args.len()) {
        return error(VALUE);
    }
    let number = match number_argument(args, 0) {
        Ok(number) => number,
        Err(error) => return error,
    };
    let values = match numbers(&args[1..2]) {
        Ok(values) => values,
        Err(error) => return error,
    };
    let ascending = if args.len() == 3 {
        match scalar(args, 2).map(|value| numeric(&value)) {
            Some(Ok(order)) => order != 0.0,
            Some(Err(error)) => return error,
            None => return error(VALUE),
        }
    }
    else {
        false
    };
    if !values.contains(&number) {
        return error(NA);
    }
    let ahead = values.iter().filter(|value| if ascending { **value < number } else { **value > number }).count();
    CellValue::number((ahead + 1) as f64)
}

// ---------------------------------------------------------------------------
// Financial functions
// ---------------------------------------------------------------------------

/// Coerces a computed number into a cell, rejecting overflow to infinity.
fn finite_number(number: f64) -> CellValue {
    if number.is_finite() { CellValue::number(normalize_zero(number)) } else { error(NUM) }
}

/// Reads the three required and two optional annuity arguments shared by
/// `PMT`, `FV` and `PV`. The timing flag collapses to 0 or 1, as Excel does.
fn annuity_arguments(args: &[Argument]) -> Result<(f64, f64, f64, f64, f64), CellValue> {
    if !(3..=5).contains(&args.len()) {
        return Err(error(VALUE));
    }
    let rate = number_argument(args, 0)?;
    let periods = number_argument(args, 1)?;
    let amount = number_argument(args, 2)?;
    let extra = if args.len() > 3 { number_argument(args, 3)? } else { 0.0 };
    let timing = if args.len() > 4 { if number_argument(args, 4)? == 0.0 { 0.0 } else { 1.0 } } else { 0.0 };
    Ok((rate, periods, amount, extra, timing))
}

/// `PMT(rate, nper, pv, [fv], [type])`.
fn function_pmt(args: &[Argument]) -> CellValue {
    let (rate, periods, present, future, timing) = match annuity_arguments(args) {
        Ok(values) => values,
        Err(error) => return error,
    };
    if rate == 0.0 {
        if periods == 0.0 {
            return error(DIV0);
        }
        return finite_number(-(present + future) / periods);
    }
    let factor = (1.0 + rate).powf(periods);
    let denominator = (factor - 1.0) * (1.0 + rate * timing);
    if denominator == 0.0 {
        return error(DIV0);
    }
    finite_number(-(rate * (present * factor + future)) / denominator)
}

/// `FV(rate, nper, pmt, [pv], [type])`.
fn function_fv(args: &[Argument]) -> CellValue {
    let (rate, periods, payment, present, timing) = match annuity_arguments(args) {
        Ok(values) => values,
        Err(error) => return error,
    };
    if rate == 0.0 {
        return finite_number(-(present + payment * periods));
    }
    let factor = (1.0 + rate).powf(periods);
    finite_number(-(present * factor + payment * (1.0 + rate * timing) * (factor - 1.0) / rate))
}

/// `PV(rate, nper, pmt, [fv], [type])`.
fn function_pv(args: &[Argument]) -> CellValue {
    let (rate, periods, payment, future, timing) = match annuity_arguments(args) {
        Ok(values) => values,
        Err(error) => return error,
    };
    if rate == 0.0 {
        return finite_number(-(future + payment * periods));
    }
    let factor = (1.0 + rate).powf(periods);
    finite_number(-(future + payment * (1.0 + rate * timing) * (factor - 1.0) / rate) / factor)
}

/// `NPV(rate, value1, ...)`: each value is discounted one period later than
/// the previous one, so the first value is divided by `1 + rate`.
fn function_npv(args: &[Argument]) -> CellValue {
    if args.len() < 2 {
        return error(VALUE);
    }
    let rate = match number_argument(args, 0) {
        Ok(rate) => rate,
        Err(error) => return error,
    };
    if rate == -1.0 {
        return error(DIV0);
    }
    let values = match numbers(&args[1..]) {
        Ok(values) => values,
        Err(error) => return error,
    };
    let step = 1.0 + rate;
    let mut divisor = step;
    let mut total = 0.0;
    for value in values {
        total += value / divisor;
        divisor *= step;
    }
    finite_number(total)
}

/// `MODE(number1, ...)`: the most frequent number, the smallest one on a tie.
fn function_mode(args: &[Argument]) -> CellValue {
    if args.is_empty() {
        return error(VALUE);
    }
    let values = match numbers(args) {
        Ok(values) => values,
        Err(error) => return error,
    };
    let mut counts: HashMap<u64, (usize, f64)> = HashMap::new();
    for value in values {
        let entry = counts.entry(value.to_bits()).or_insert((0, value));
        entry.0 += 1;
    }
    let mut best: Option<(usize, f64)> = None;
    for &(count, value) in counts.values() {
        let better = match best {
            None => true,
            Some((best_count, best_value)) => count > best_count || (count == best_count && value < best_value),
        };
        if better {
            best = Some((count, value));
        }
    }
    match best {
        Some((count, value)) if count > 1 => CellValue::number(normalize_zero(value)),
        _ => error(NA),
    }
}

// ---------------------------------------------------------------------------
// Dates and times
// ---------------------------------------------------------------------------

/// Last supported serial: 9999-12-31 in the Excel 1900 date system.
const MAX_SERIAL: f64 = 2_958_465.0;

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`). A date library is not needed for the small calendar
/// surface the engine exposes.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

/// Rolls a possibly out-of-range month into its year.
fn normalize_year_month(year: i64, month: i64) -> (i64, i64) {
    let total = year * 12 + month - 1;
    (total.div_euclid(12), total.rem_euclid(12) + 1)
}

/// Days in a one-based month of a Gregorian year.
fn days_in_month(year: i64, month: i64) -> i64 {
    let (next_year, next_month) = normalize_year_month(year, month + 1);
    days_from_civil(next_year, next_month, 1) - days_from_civil(year, month, 1)
}

/// Excel serial for a civil date.
///
/// Serials before 1900-03-01 count from 1899-12-31 and later ones from
/// 1899-12-30, which leaves room for the fictitious 1900-02-29 at serial 60.
/// The engine reproduces that serial when reading dates, but `DATE(1900,2,29)`
/// normalizes to 1900-03-01 (serial 61) because a normalized civil date cannot
/// carry the fictitious day. This is the documented simplification of the
/// 1900 leap-year compatibility rule.
fn serial_from_parts(year: i64, month: i64, day: i64) -> Option<i64> {
    let (year, month) = normalize_year_month(year, month);
    if !(1900..=9999).contains(&year) {
        return None;
    }
    let days = days_from_civil(year, month, 1) + day - 1;
    let serial = if days < days_from_civil(1900, 3, 1) { days - days_from_civil(1899, 12, 31) } else { days - days_from_civil(1899, 12, 30) };
    (1..=2_958_465).contains(&serial).then_some(serial)
}

/// Splits a positive serial into its calendar date, including serial 60.
fn parts_from_serial(serial: f64) -> Option<(i64, i64, i64)> {
    let serial = serial.trunc();
    if !(1.0..=MAX_SERIAL).contains(&serial) {
        return None;
    }
    let serial = serial as i64;
    if serial == 60 {
        return Some((1900, 2, 29));
    }
    let days = if serial < 61 { days_from_civil(1899, 12, 31) + serial } else { days_from_civil(1899, 12, 30) + serial };
    Some(civil_from_days(days))
}

/// Reads one serial argument, rejecting non-numeric, fractional-before-1900
/// and out-of-range values with an Excel error instead of panicking.
fn serial_argument(args: &[Argument], index: usize) -> Result<f64, CellValue> {
    let serial = number_argument(args, index)?;
    if !(1.0..=MAX_SERIAL).contains(&serial) {
        return Err(error(NUM));
    }
    Ok(serial)
}

/// `DATE(year, month, day)`; years 0-1899 are offset by 1900 like Excel.
fn function_date(args: &[Argument]) -> CellValue {
    if args.len() != 3 {
        return error(VALUE);
    }
    let mut fields = [0i64; 3];
    for (index, field) in fields.iter_mut().enumerate() {
        let number = match number_argument(args, index) {
            Ok(number) => number,
            Err(error) => return error,
        };
        if !number.is_finite() || number.abs() > 1_000_000.0 {
            return error(NUM);
        }
        *field = number.trunc() as i64;
    }
    let [year, month, day] = fields;
    let year = if (0..1900).contains(&year) { year + 1900 } else { year };
    match serial_from_parts(year, month, day) {
        Some(serial) => CellValue::number(serial as f64),
        None => error(NUM),
    }
}

/// `YEAR`, `MONTH` and `DAY` read one field of a date serial.
fn function_date_part(args: &[Argument], name: &str) -> CellValue {
    if args.len() != 1 {
        return error(VALUE);
    }
    let serial = match serial_argument(args, 0) {
        Ok(serial) => serial,
        Err(error) => return error,
    };
    let Some((year, month, day)) = parts_from_serial(serial)
    else {
        return error(NUM);
    };
    CellValue::number(match name {
        "YEAR" => year as f64,
        "MONTH" => month as f64,
        _ => day as f64,
    })
}

/// `WEEKDAY(serial, [return_type])`. Serial 1 (1900-01-01) was a Monday.
fn function_weekday(args: &[Argument]) -> CellValue {
    if !(1..=2).contains(&args.len()) {
        return error(VALUE);
    }
    let serial = match serial_argument(args, 0) {
        Ok(serial) => serial.trunc() as i64,
        Err(error) => return error,
    };
    let return_type = if args.len() == 2 {
        match integer_argument(args, 1) {
            Ok(return_type) => return_type,
            Err(error) => return error,
        }
    }
    else {
        1
    };
    // Excel counts back from serial 1 as Sunday and carries the fictitious
    // 1900-02-29, which matches real weekdays from 1900-03-01 onwards.
    let sunday_based = (serial - 1).rem_euclid(7);
    let start = match return_type {
        1 => 0,
        2 | 3 => 1,
        11..=17 => (return_type - 10) % 7,
        _ => return error(NUM),
    };
    let offset = (sunday_based - start).rem_euclid(7);
    CellValue::number(if return_type == 3 { offset as f64 } else { (offset + 1) as f64 })
}

/// `EDATE(start, months)` and `EOMONTH(start, months)` share the month shift;
/// the latter always lands on the last day of the target month.
fn function_month_shift(args: &[Argument], end_of_month: bool) -> CellValue {
    if args.len() != 2 {
        return error(VALUE);
    }
    let serial = match serial_argument(args, 0) {
        Ok(serial) => serial,
        Err(error) => return error,
    };
    let months = match number_argument(args, 1) {
        Ok(months) if months.is_finite() && months.abs() <= 1_000_000.0 => months.trunc() as i64,
        Ok(_) => return error(NUM),
        Err(error) => return error,
    };
    let Some((year, month, day)) = parts_from_serial(serial)
    else {
        return error(NUM);
    };
    let (year, month) = normalize_year_month(year, month + months);
    let day = if end_of_month { days_in_month(year, month) } else { day.min(days_in_month(year, month)) };
    match serial_from_parts(year, month, day) {
        Some(serial) => CellValue::number(serial as f64),
        None => error(NUM),
    }
}

/// `TODAY()` and `NOW()`. These are volatile: they read the wall clock on
/// every recalculation. The engine has no timezone database, so the value is
/// the UTC date rather than the host's local date.
fn function_now(args: &[Argument], with_time: bool) -> CellValue {
    if !args.is_empty() {
        return error(VALUE);
    }
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs_f64()).unwrap_or(0.0);
    let serial = 25_569.0 + seconds / 86_400.0;
    CellValue::number(if with_time { serial } else { serial.floor() })
}

/// Uppercases the first letter of every word and lowercases the rest.
fn proper_case(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut start_of_word = true;
    for character in text.chars() {
        if character.is_alphabetic() {
            if start_of_word {
                result.extend(character.to_uppercase());
            }
            else {
                result.extend(character.to_lowercase());
            }
            start_of_word = false;
        }
        else {
            result.push(character);
            start_of_word = true;
        }
    }
    result
}

/// One-based character offset of `needle` in `text`, or `None` when absent.
fn text_position(text: &str, needle: &str, start: usize, case_sensitive: bool) -> Option<usize> {
    let text: Vec<char> = if case_sensitive { text.chars().collect() } else { text.chars().flat_map(char::to_lowercase).collect() };
    let needle: Vec<char> = if case_sensitive { needle.chars().collect() } else { needle.chars().flat_map(char::to_lowercase).collect() };
    if start > text.len() || needle.len() > text.len() {
        return None;
    }
    if needle.is_empty() {
        return Some(start);
    }
    (start..=text.len() - needle.len()).find(|index| text[*index..*index + needle.len()] == needle[..]).map(|index| index + 1)
}

/// `FIND` (case-sensitive) and `SEARCH` (case-insensitive).
fn function_text_search(args: &[Argument], case_sensitive: bool) -> CellValue {
    if !(2..=3).contains(&args.len()) {
        return error(VALUE);
    }
    let needle = match text_argument(args, 0) {
        Ok(needle) => needle,
        Err(error) => return error,
    };
    let text = match text_argument(args, 1) {
        Ok(text) => text,
        Err(error) => return error,
    };
    let start = if args.len() == 3 {
        match number_argument(args, 2) {
            Ok(start) => start,
            Err(error) => return error,
        }
    }
    else {
        1.0
    };
    if start < 1.0 {
        return error(VALUE);
    }
    let length = text.chars().count();
    let start = start.trunc() as usize - 1;
    if start > length {
        return error(VALUE);
    }
    match text_position(&text, &needle, start, case_sensitive) {
        Some(position) => CellValue::number(position as f64),
        None => error(VALUE),
    }
}

// ---------------------------------------------------------------------------
// Conditional aggregation
// ---------------------------------------------------------------------------

/// Comparison an `IF`-suffixed criterion applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComparisonOp {
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

/// One parsed `COUNTIF`-style criterion.
struct Criterion {
    op: ComparisonOp,
    operand: CellValue,
}

/// Reads a criterion argument: a literal, `">10"`, `"<>x"` or `"*x*"`.
fn criterion_from(value: &CellValue) -> Result<Option<Criterion>, CellValue> {
    match value {
        CellValue::Error { .. } => Err(value.clone()),
        CellValue::Number { .. } | CellValue::Bool { .. } => Ok(Some(Criterion { op: ComparisonOp::Equal, operand: value.clone() })),
        CellValue::Empty => Ok(None),
        CellValue::Text { text } => {
            for (prefix, op) in [
                (">=", ComparisonOp::GreaterEqual),
                ("<=", ComparisonOp::LessEqual),
                ("<>", ComparisonOp::NotEqual),
                (">", ComparisonOp::Greater),
                ("<", ComparisonOp::Less),
                ("=", ComparisonOp::Equal),
            ] {
                if let Some(rest) = text.strip_prefix(prefix) {
                    return Ok(Some(Criterion { op, operand: criterion_operand(rest) }));
                }
            }
            Ok(Some(Criterion { op: ComparisonOp::Equal, operand: criterion_operand(text) }))
        }
    }
}

/// Converts the operand of a criterion, keeping wildcard text as text.
fn criterion_operand(text: &str) -> CellValue {
    let trimmed = text.trim();
    if let Ok(number) = trimmed.parse::<f64>() {
        return CellValue::number(number);
    }
    if trimmed.eq_ignore_ascii_case("TRUE") {
        return CellValue::boolean(true);
    }
    if trimmed.eq_ignore_ascii_case("FALSE") {
        return CellValue::boolean(false);
    }
    CellValue::text(text)
}

/// Matches one value against a criterion; errors propagate to the caller.
fn criterion_matches(criterion: &Criterion, value: &CellValue) -> bool {
    if value.is_empty() {
        let blank_operand = match &criterion.operand {
            CellValue::Empty => true,
            CellValue::Text { text } => text.is_empty(),
            _ => false,
        };
        return criterion.op == ComparisonOp::Equal && blank_operand;
    }
    match criterion.op {
        ComparisonOp::Equal => criterion_equal(&criterion.operand, value),
        ComparisonOp::NotEqual => !criterion_equal(&criterion.operand, value),
        _ => compare(value, &criterion.operand)
            .map(|ordering| match criterion.op {
                ComparisonOp::Less => ordering == Ordering::Less,
                ComparisonOp::LessEqual => ordering != Ordering::Greater,
                ComparisonOp::Greater => ordering == Ordering::Greater,
                _ => ordering != Ordering::Less,
            })
            .unwrap_or(false),
    }
}

/// Matches one value against an optional criterion; `None` means "blank".
fn criterion_matches_optional(criterion: &Option<Criterion>, value: &CellValue) -> bool {
    match criterion {
        Some(criterion) => criterion_matches(criterion, value),
        None => value.is_empty(),
    }
}

fn criterion_equal(operand: &CellValue, value: &CellValue) -> bool {
    match (operand, value) {
        (CellValue::Text { text: pattern }, CellValue::Text { text }) => {
            if pattern.contains(['*', '?']) {
                wildcard_match(pattern, text)
            }
            else {
                pattern.eq_ignore_ascii_case(text)
            }
        }
        (CellValue::Text { .. }, _) => false,
        (CellValue::Number { number: left }, CellValue::Number { number: right }) => left == right,
        (CellValue::Number { .. }, _) => false,
        (CellValue::Bool { value: left }, CellValue::Bool { value: right }) => left == right,
        (CellValue::Bool { .. }, _) => false,
        (CellValue::Empty, other) => other.is_empty(),
        _ => false,
    }
}

/// Case-insensitive `*` (any run) and `?` (one character) pattern match.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().flat_map(char::to_lowercase).collect();
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let (mut p, mut t) = (0usize, 0usize);
    let mut star = None;
    let mut mark = 0usize;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        }
        else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            p += 1;
            mark = t;
        }
        else if let Some(position) = star {
            p = position + 1;
            mark += 1;
            t = mark;
        }
        else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}

/// Reads `(range, criterion)` pairs from `start`, rejecting odd tails.
fn criteria_pairs<'a>(args: &'a [Argument], start: usize) -> Result<Vec<(&'a Grid, Option<Criterion>)>, CellValue> {
    let mut pairs = Vec::new();
    let mut index = start;
    while index < args.len() {
        if index + 1 >= args.len() {
            return Err(error(VALUE));
        }
        let grid = match &args[index] {
            Argument::Grid(grid) => grid,
            _ => return Err(error(VALUE)),
        };
        let raw = scalar(args, index + 1).ok_or_else(|| error(VALUE))?;
        let criterion = criterion_from(&raw)?;
        pairs.push((grid, criterion));
        index += 2;
    }
    if pairs.is_empty() { Err(error(VALUE)) } else { Ok(pairs) }
}

fn same_shape(left: &Grid, right: &Grid) -> bool {
    left.rows == right.rows && left.columns == right.columns
}

/// Whether every criterion pair matches the cell at `index`.
fn criteria_match_at(pairs: &[(&Grid, Option<Criterion>)], index: usize) -> Result<bool, CellValue> {
    for (grid, criterion) in pairs {
        let Some(value) = grid.values.get(index)
        else {
            return Err(error(VALUE));
        };
        if let CellValue::Error { .. } = value {
            return Err(value.clone());
        }
        if !criterion_matches_optional(criterion, value) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn function_sumif(args: &[Argument]) -> CellValue {
    if !(2..=3).contains(&args.len()) {
        return error(VALUE);
    }
    let Some(criteria_grid) = grid_argument(args, 0)
    else {
        return error(VALUE);
    };
    let Some(raw) = scalar(args, 1)
    else {
        return error(VALUE);
    };
    let criterion = match criterion_from(&raw) {
        Ok(criterion) => criterion,
        Err(error) => return error,
    };
    let sum_grid = if args.len() == 3 {
        match grid_argument(args, 2) {
            Some(grid) => grid,
            None => return error(VALUE),
        }
    }
    else {
        criteria_grid
    };
    let mut total = 0.0;
    for (index, value) in criteria_grid.values.iter().enumerate() {
        if let CellValue::Error { .. } = value {
            return value.clone();
        }
        if !criterion_matches_optional(&criterion, value) {
            continue;
        }
        match sum_grid.values.get(index) {
            Some(CellValue::Number { number }) => total += number,
            Some(value @ CellValue::Error { .. }) => return value.clone(),
            _ => {}
        }
    }
    CellValue::number(normalize_zero(total))
}

fn function_countif(args: &[Argument]) -> CellValue {
    if args.len() != 2 {
        return error(VALUE);
    }
    let Some(criteria_grid) = grid_argument(args, 0)
    else {
        return error(VALUE);
    };
    let Some(raw) = scalar(args, 1)
    else {
        return error(VALUE);
    };
    let criterion = match criterion_from(&raw) {
        Ok(criterion) => criterion,
        Err(error) => return error,
    };
    let mut count = 0usize;
    for value in &criteria_grid.values {
        if let CellValue::Error { .. } = value {
            return value.clone();
        }
        if criterion_matches_optional(&criterion, value) {
            count += 1;
        }
    }
    CellValue::number(count as f64)
}

fn function_averageif(args: &[Argument]) -> CellValue {
    if !(2..=3).contains(&args.len()) {
        return error(VALUE);
    }
    let Some(criteria_grid) = grid_argument(args, 0)
    else {
        return error(VALUE);
    };
    let Some(raw) = scalar(args, 1)
    else {
        return error(VALUE);
    };
    let criterion = match criterion_from(&raw) {
        Ok(criterion) => criterion,
        Err(error) => return error,
    };
    let average_grid = if args.len() == 3 {
        match grid_argument(args, 2) {
            Some(grid) => grid,
            None => return error(VALUE),
        }
    }
    else {
        criteria_grid
    };
    let mut total = 0.0;
    let mut count = 0usize;
    for (index, value) in criteria_grid.values.iter().enumerate() {
        if let CellValue::Error { .. } = value {
            return value.clone();
        }
        if !criterion_matches_optional(&criterion, value) {
            continue;
        }
        match average_grid.values.get(index) {
            Some(CellValue::Number { number }) => {
                total += number;
                count += 1;
            }
            Some(value @ CellValue::Error { .. }) => return value.clone(),
            _ => {}
        }
    }
    if count == 0 {
        return error(DIV0);
    }
    CellValue::number(normalize_zero(total / count as f64))
}

fn function_sumifs(args: &[Argument]) -> CellValue {
    if args.len() < 3 || (args.len() - 1) % 2 != 0 {
        return error(VALUE);
    }
    let Some(sum_grid) = grid_argument(args, 0)
    else {
        return error(VALUE);
    };
    let pairs = match criteria_pairs(args, 1) {
        Ok(pairs) => pairs,
        Err(error) => return error,
    };
    if pairs.iter().any(|(grid, _)| !same_shape(grid, sum_grid)) {
        return error(VALUE);
    }
    let mut total = 0.0;
    for (index, value) in sum_grid.values.iter().enumerate() {
        match criteria_match_at(&pairs, index) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => return error,
        }
        match value {
            CellValue::Number { number } => total += number,
            CellValue::Error { .. } => return value.clone(),
            _ => {}
        }
    }
    CellValue::number(normalize_zero(total))
}

fn function_countifs(args: &[Argument]) -> CellValue {
    if args.len() < 2 {
        return error(VALUE);
    }
    let pairs = match criteria_pairs(args, 0) {
        Ok(pairs) => pairs,
        Err(error) => return error,
    };
    let first = pairs[0].0;
    if pairs.iter().any(|(grid, _)| !same_shape(grid, first)) {
        return error(VALUE);
    }
    let mut count = 0usize;
    for index in 0..first.values.len() {
        match criteria_match_at(&pairs, index) {
            Ok(true) => count += 1,
            Ok(false) => {}
            Err(error) => return error,
        }
    }
    CellValue::number(count as f64)
}

fn function_averageifs(args: &[Argument]) -> CellValue {
    if args.len() < 3 || (args.len() - 1) % 2 != 0 {
        return error(VALUE);
    }
    let Some(average_grid) = grid_argument(args, 0)
    else {
        return error(VALUE);
    };
    let pairs = match criteria_pairs(args, 1) {
        Ok(pairs) => pairs,
        Err(error) => return error,
    };
    if pairs.iter().any(|(grid, _)| !same_shape(grid, average_grid)) {
        return error(VALUE);
    }
    let mut total = 0.0;
    let mut count = 0usize;
    for (index, value) in average_grid.values.iter().enumerate() {
        match criteria_match_at(&pairs, index) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => return error,
        }
        match value {
            CellValue::Number { number } => {
                total += number;
                count += 1;
            }
            CellValue::Error { .. } => return value.clone(),
            _ => {}
        }
    }
    if count == 0 {
        return error(DIV0);
    }
    CellValue::number(normalize_zero(total / count as f64))
}

fn function_maxifs(args: &[Argument], maximum: bool) -> CellValue {
    if args.len() < 3 || (args.len() - 1) % 2 != 0 {
        return error(VALUE);
    }
    let Some(value_grid) = grid_argument(args, 0)
    else {
        return error(VALUE);
    };
    let pairs = match criteria_pairs(args, 1) {
        Ok(pairs) => pairs,
        Err(error) => return error,
    };
    if pairs.iter().any(|(grid, _)| !same_shape(grid, value_grid)) {
        return error(VALUE);
    }
    let mut best: Option<f64> = None;
    for (index, value) in value_grid.values.iter().enumerate() {
        match criteria_match_at(&pairs, index) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => return error,
        }
        match value {
            CellValue::Number { number } => {
                best = Some(match best {
                    None => *number,
                    Some(current) => {
                        if maximum {
                            current.max(*number)
                        }
                        else {
                            current.min(*number)
                        }
                    }
                });
            }
            CellValue::Error { .. } => return value.clone(),
            _ => {}
        }
    }
    CellValue::number(normalize_zero(best.unwrap_or(0.0)))
}

fn function_textjoin(args: &[Argument]) -> CellValue {
    if args.len() < 3 {
        return error(VALUE);
    }
    let delimiter = match text_argument(args, 0) {
        Ok(delimiter) => delimiter,
        Err(error) => return error,
    };
    let ignore_empty = match scalar(args, 1).map(|value| truthy(&value)) {
        Some(Ok(ignore_empty)) => ignore_empty,
        Some(Err(error)) => return error,
        None => return error(VALUE),
    };
    let mut parts = Vec::new();
    for argument in &args[2..] {
        match argument {
            Argument::Value(value) => {
                let text = match as_text(value) {
                    Ok(text) => text,
                    Err(error) => return error,
                };
                if !(ignore_empty && text.is_empty()) {
                    parts.push(text);
                }
            }
            Argument::Grid(grid) => {
                for value in &grid.values {
                    let text = match as_text(value) {
                        Ok(text) => text,
                        Err(error) => return error,
                    };
                    if !(ignore_empty && text.is_empty()) {
                        parts.push(text);
                    }
                }
            }
            Argument::Invalid(value) => return value.clone(),
        }
    }
    CellValue::text(parts.join(&delimiter))
}

fn function_substitute(args: &[Argument]) -> CellValue {
    if !(3..=4).contains(&args.len()) {
        return error(VALUE);
    }
    let text = match text_argument(args, 0) {
        Ok(text) => text,
        Err(error) => return error,
    };
    let old = match text_argument(args, 1) {
        Ok(old) => old,
        Err(error) => return error,
    };
    let new = match text_argument(args, 2) {
        Ok(new) => new,
        Err(error) => return error,
    };
    if old.is_empty() {
        return CellValue::text(text);
    }
    if args.len() == 4 {
        let instance = match integer_argument(args, 3) {
            Ok(instance) => instance,
            Err(error) => return error,
        };
        if instance < 1 {
            return error(VALUE);
        }
        CellValue::text(replace_nth(&text, &old, &new, instance as usize))
    }
    else {
        CellValue::text(text.replace(&old, &new))
    }
}

fn function_left(args: &[Argument]) -> CellValue {
    if !(1..=2).contains(&args.len()) {
        return error(VALUE);
    }
    let text = match text_argument(args, 0) {
        Ok(text) => text,
        Err(error) => return error,
    };
    let count = if args.len() == 2 {
        match number_argument(args, 1).and_then(character_count) {
            Ok(count) => count,
            Err(error) => return error,
        }
    }
    else {
        1
    };
    CellValue::text(text.chars().take(count).collect::<String>())
}

fn function_right(args: &[Argument]) -> CellValue {
    if !(1..=2).contains(&args.len()) {
        return error(VALUE);
    }
    let text = match text_argument(args, 0) {
        Ok(text) => text,
        Err(error) => return error,
    };
    let count = if args.len() == 2 {
        match number_argument(args, 1).and_then(character_count) {
            Ok(count) => count,
            Err(error) => return error,
        }
    }
    else {
        1
    };
    let characters: Vec<char> = text.chars().collect();
    let start = characters.len().saturating_sub(count);
    CellValue::text(characters[start..].iter().collect::<String>())
}

fn function_mid(args: &[Argument]) -> CellValue {
    if args.len() != 3 {
        return error(VALUE);
    }
    let text = match text_argument(args, 0) {
        Ok(text) => text,
        Err(error) => return error,
    };
    let start = match number_argument(args, 1) {
        Ok(start) => start,
        Err(error) => return error,
    };
    if start < 1.0 {
        return error(VALUE);
    }
    let count = match number_argument(args, 2).and_then(character_count) {
        Ok(count) => count,
        Err(error) => return error,
    };
    let start = start.trunc() as usize - 1;
    CellValue::text(text.chars().skip(start).take(count).collect::<String>())
}

/// `REPLACE(old_text, start_num, num_chars, new_text)` replaces a character
/// span, numbering characters from one. Only `start_num == length + 1`
/// appends; a farther position is `#VALUE!`.
fn function_replace(args: &[Argument]) -> CellValue {
    if args.len() != 4 {
        return error(VALUE);
    }
    let text = match text_argument(args, 0) {
        Ok(text) => text,
        Err(error) => return error,
    };
    let start = match number_argument(args, 1) {
        Ok(start) => start,
        Err(error) => return error,
    };
    if !start.is_finite() || start < 1.0 {
        return error(VALUE);
    }
    let count = match number_argument(args, 2).and_then(character_count) {
        Ok(count) => count,
        Err(error) => return error,
    };
    let replacement = match text_argument(args, 3) {
        Ok(replacement) => replacement,
        Err(error) => return error,
    };
    let characters: Vec<char> = text.chars().collect();
    let start = start.trunc() as usize - 1;
    if start > characters.len() {
        return error(VALUE);
    }
    let end = start.saturating_add(count).min(characters.len());
    let mut output = characters[..start].iter().collect::<String>();
    output.push_str(&replacement);
    output.extend(characters[end..].iter());
    CellValue::text(output)
}

fn apply_function(name: &str, args: &[Argument]) -> CellValue {
    if let Some(error) = args.iter().find_map(|argument| match argument {
        Argument::Invalid(value) => Some(value.clone()),
        _ => None,
    }) {
        return error;
    }
    match name {
        "SUM" => match numbers(args) {
            Ok(values) => CellValue::number(normalize_zero(values.iter().sum())),
            Err(error) => error,
        },
        "AVERAGE" => match numbers(args) {
            Ok(values) if values.is_empty() => error(DIV0),
            Ok(values) => CellValue::number(normalize_zero(values.iter().sum::<f64>() / values.len() as f64)),
            Err(error) => error,
        },
        "MIN" => match numbers(args) {
            Ok(values) if values.is_empty() => CellValue::number(0.0),
            Ok(values) => CellValue::number(values.iter().copied().fold(f64::INFINITY, f64::min)),
            Err(error) => error,
        },
        "MAX" => match numbers(args) {
            Ok(values) if values.is_empty() => CellValue::number(0.0),
            Ok(values) => CellValue::number(values.iter().copied().fold(f64::NEG_INFINITY, f64::max)),
            Err(error) => error,
        },
        "COUNT" => CellValue::number(count_numbers(args) as f64),
        "COUNTA" => CellValue::number(count_non_empty(args) as f64),
        "SUMIF" => function_sumif(args),
        "COUNTIF" => function_countif(args),
        "AVERAGEIF" => function_averageif(args),
        "SUMIFS" => function_sumifs(args),
        "COUNTIFS" => function_countifs(args),
        "AVERAGEIFS" => function_averageifs(args),
        "MAXIFS" => function_maxifs(args, true),
        "MINIFS" => function_maxifs(args, false),
        "DATE" => function_date(args),
        "YEAR" | "MONTH" | "DAY" => function_date_part(args, name),
        "WEEKDAY" => function_weekday(args),
        "EDATE" => function_month_shift(args, false),
        "EOMONTH" => function_month_shift(args, true),
        "TODAY" | "NOW" => function_now(args, name == "NOW"),
        "MEDIAN" => match numbers(args) {
            Ok(values) => CellValue::number(normalize_zero(median(&values).unwrap_or(0.0))),
            Err(error) => error,
        },
        "MODE" => function_mode(args),
        "PMT" => function_pmt(args),
        "FV" => function_fv(args),
        "PV" => function_pv(args),
        "NPV" => function_npv(args),
        "STDEV" | "STDEVP" | "VAR" | "VARP" => match numbers(args) {
            Ok(values) => {
                let sample = matches!(name, "STDEV" | "VAR");
                match variance(&values, sample) {
                    Some(variance) => {
                        let result = if matches!(name, "STDEV" | "STDEVP") { variance.sqrt() } else { variance };
                        CellValue::number(normalize_zero(result))
                    }
                    None => error(DIV0),
                }
            }
            Err(error) => error,
        },
        "LARGE" => function_large_small(args, true),
        "SMALL" => function_large_small(args, false),
        "SUMPRODUCT" => function_sumproduct(args),
        "FLOOR" => function_floor_ceiling(args, false),
        "CEILING" => function_floor_ceiling(args, true),
        "MOD" => {
            if args.len() != 2 {
                return error(VALUE);
            }
            let number = match number_argument(args, 0) {
                Ok(number) => number,
                Err(error) => return error,
            };
            let divisor = match number_argument(args, 1) {
                Ok(divisor) => divisor,
                Err(error) => return error,
            };
            if divisor == 0.0 {
                return error(DIV0);
            }
            CellValue::number(normalize_zero(number - divisor * (number / divisor).floor()))
        }
        "POWER" => {
            if args.len() != 2 {
                return error(VALUE);
            }
            let number = match number_argument(args, 0) {
                Ok(number) => number,
                Err(error) => return error,
            };
            let exponent = match number_argument(args, 1) {
                Ok(exponent) => exponent,
                Err(error) => return error,
            };
            let result = number.powf(exponent);
            if !result.is_finite() {
                return error(NUM);
            }
            CellValue::number(normalize_zero(result))
        }
        "SIGN" => match single_number(args) {
            Ok(number) => CellValue::number(if number > 0.0 {
                1.0
            }
            else if number < 0.0 {
                -1.0
            }
            else {
                0.0
            }),
            Err(error) => error,
        },
        "SQRT" => match single_number(args) {
            Ok(number) if number < 0.0 => error(NUM),
            Ok(number) => CellValue::number(number.sqrt()),
            Err(error) => error,
        },
        "EXACT" => {
            if args.len() != 2 {
                return error(VALUE);
            }
            let left = match text_argument(args, 0) {
                Ok(text) => text,
                Err(error) => return error,
            };
            let right = match text_argument(args, 1) {
                Ok(text) => text,
                Err(error) => return error,
            };
            CellValue::boolean(left == right)
        }
        "PROPER" => match single_text(args) {
            Ok(text) => CellValue::text(proper_case(&text)),
            Err(error) => error,
        },
        "FIND" => function_text_search(args, true),
        "SEARCH" => function_text_search(args, false),
        "ROWS" | "COLUMNS" => {
            if args.len() != 1 {
                return error(VALUE);
            }
            match grid_argument(args, 0) {
                Some(grid) => CellValue::number(f64::from(if name == "ROWS" { grid.rows } else { grid.columns })),
                None => error(VALUE),
            }
        }
        "RANK" => function_rank(args),
        "IF" => error(VALUE),
        "ROUND" => {
            if args.len() != 2 {
                return error(VALUE);
            }
            let number = match scalar(args, 0).map(|value| numeric(&value)) {
                Some(Ok(number)) => number,
                Some(Err(error)) => return error,
                None => return error(VALUE),
            };
            let digits = match scalar(args, 1).map(|value| numeric(&value)) {
                Some(Ok(digits)) => digits,
                Some(Err(error)) => return error,
                None => return error(VALUE),
            };
            CellValue::number(round_half_away(number, digits))
        }
        "ABS" => match single_number(args) {
            Ok(number) => CellValue::number(number.abs()),
            Err(error) => error,
        },
        "CONCAT" => {
            let mut output = String::new();
            for argument in args {
                match argument {
                    Argument::Value(value) => match as_text(value) {
                        Ok(text) => output.push_str(&text),
                        Err(error) => return error,
                    },
                    Argument::Grid(grid) => {
                        for item in &grid.values {
                            match as_text(item) {
                                Ok(text) => output.push_str(&text),
                                Err(error) => return error,
                            }
                        }
                    }
                    Argument::Invalid(value) => return value.clone(),
                }
            }
            CellValue::text(output)
        }
        "LEN" => match single_text(args) {
            Ok(text) => CellValue::number(text.chars().count() as f64),
            Err(error) => error,
        },
        "UPPER" => match single_text(args) {
            Ok(text) => CellValue::text(text.to_uppercase()),
            Err(error) => error,
        },
        "LOWER" => match single_text(args) {
            Ok(text) => CellValue::text(text.to_lowercase()),
            Err(error) => error,
        },
        "TRIM" => match single_text(args) {
            Ok(text) => CellValue::text(trim_text(&text)),
            Err(error) => error,
        },
        "NOT" => {
            if args.len() != 1 {
                return error(VALUE);
            }
            match scalar(args, 0) {
                Some(value) => match truthy(&value) {
                    Ok(truth) => CellValue::boolean(!truth),
                    Err(error) => error,
                },
                None => error(VALUE),
            }
        }
        "VLOOKUP" => function_vlookup(args),
        "INDEX" => function_index(args),
        "MATCH" => function_match(args),
        "LEFT" => function_left(args),
        "RIGHT" => function_right(args),
        "MID" => function_mid(args),
        "REPLACE" => function_replace(args),
        "SUBSTITUTE" => function_substitute(args),
        "TEXTJOIN" => function_textjoin(args),
        "INT" => match single_number(args) {
            Ok(number) => CellValue::number(normalize_zero(number.floor())),
            Err(error) => error,
        },
        "ROUNDUP" | "ROUNDDOWN" => {
            if args.len() != 2 {
                return error(VALUE);
            }
            let number = match number_argument(args, 0) {
                Ok(number) => number,
                Err(error) => return error,
            };
            let digits = match number_argument(args, 1) {
                Ok(digits) => digits,
                Err(error) => return error,
            };
            CellValue::number(round_toward(number, digits, name == "ROUNDUP"))
        }
        "IFERROR" | "AND" | "OR" | "ROW" | "COLUMN" | "IFS" | "SWITCH" | "CHOOSE" => error(VALUE),
        _ => error(NAME),
    }
}

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// Recomputes every formula cell and returns the resolved values.
pub fn evaluate(workbook: &dyn Workbook) -> EvalReport {
    let mut forms = HashMap::new();
    for cell in workbook.formula_cells() {
        forms.insert((cell.sheet, cell.row, cell.column), parse_formula(&cell.source).ok());
    }
    let mut keys: Vec<CellKey> = forms.keys().copied().collect();
    keys.sort_unstable();
    let mut evaluator =
        Evaluator { workbook, forms, values: HashMap::new(), states: HashMap::new(), stack: Vec::new(), unsupported: HashSet::new() };
    for (sheet, row, column) in keys {
        evaluator.eval_cell(sheet, row, column);
    }
    EvalReport { values: std::mem::take(&mut evaluator.values), unsupported: std::mem::take(&mut evaluator.unsupported) }
}

/// Reports whether a formula body parses, without exposing the expression tree.
pub fn validate(source: &str) -> Result<(), String> {
    parse_formula(source).map(|_| ())
}

/// Formula cells and the cells they read, including every range member.
fn dependencies(workbook: &dyn Workbook) -> HashMap<CellKey, Vec<CellKey>> {
    let mut graph = HashMap::new();
    for cell in workbook.formula_cells() {
        let mut targets = Vec::new();
        if let Ok(expression) = parse_formula(&cell.source) {
            collect_dependencies(&expression, workbook, cell.sheet, &mut targets);
        }
        graph.insert((cell.sheet, cell.row, cell.column), targets);
    }
    graph
}

fn collect_dependencies(expression: &Expr, workbook: &dyn Workbook, current: usize, out: &mut Vec<CellKey>) {
    match expression {
        Expr::Cell { sheet, row, column } => {
            let index = match sheet {
                None => Some(current),
                Some(name) => workbook.sheet_index(name),
            };
            if let Some(index) = index
                && *row >= 1
                && *row <= MAX_ROW
                && *column >= 1
                && *column <= MAX_COLUMN
            {
                out.push((index, *row, *column));
            }
        }
        Expr::Range { sheet, range } => {
            let index = match sheet {
                None => Some(current),
                Some(name) => workbook.sheet_index(name),
            };
            if let Some(index) = index
                && range.is_valid()
                && u64::from(range.rows()) * u64::from(range.columns()) <= MAX_RANGE_CELLS
            {
                for row in range.start_row..=range.end_row {
                    for column in range.start_column..=range.end_column {
                        out.push((index, row, column));
                    }
                }
            }
        }
        Expr::Name(name) => {
            // A defined name depends on every cell its rectangle denotes.
            if let Some(named) = workbook.named_range(name)
                && let Some(index) = workbook.sheet_index(&named.sheet)
                && named.range.is_valid()
                && named.range.end_row <= MAX_ROW
                && named.range.end_column <= MAX_COLUMN
                && u64::from(named.range.rows()) * u64::from(named.range.columns()) <= MAX_RANGE_CELLS
            {
                for row in named.range.start_row..=named.range.end_row {
                    for column in named.range.start_column..=named.range.end_column {
                        out.push((index, row, column));
                    }
                }
            }
        }
        Expr::Negate(inner) | Expr::Percent(inner) => collect_dependencies(inner, workbook, current, out),
        Expr::Binary { left, right, .. } => {
            collect_dependencies(left, workbook, current, out);
            collect_dependencies(right, workbook, current, out);
        }
        Expr::Call { args, .. } => {
            for argument in args {
                collect_dependencies(argument, workbook, current, out);
            }
        }
        _ => {}
    }
}

struct CycleScan<'a> {
    graph: &'a HashMap<CellKey, Vec<CellKey>>,
    colors: HashMap<CellKey, u8>,
    stack: Vec<CellKey>,
    cycles: Vec<Vec<CellKey>>,
    reported: HashSet<Vec<CellKey>>,
}

impl CycleScan<'_> {
    fn visit(&mut self, node: CellKey) {
        self.colors.insert(node, 1);
        self.stack.push(node);
        if let Some(targets) = self.graph.get(&node).cloned() {
            for target in targets {
                match self.colors.get(&target).copied().unwrap_or(0) {
                    0 => {
                        if self.graph.contains_key(&target) {
                            self.visit(target);
                        }
                    }
                    1 => {
                        if let Some(position) = self.stack.iter().position(|entry| *entry == target) {
                            let cycle = self.stack[position..].to_vec();
                            let mut key = cycle.clone();
                            key.sort_unstable();
                            if self.reported.insert(key) {
                                self.cycles.push(cycle);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        self.stack.pop();
        self.colors.insert(node, 2);
    }
}

/// Finds every formula cycle, including cycles that cross sheets.
pub fn find_cycles(workbook: &dyn Workbook) -> Vec<Vec<CellKey>> {
    let graph = dependencies(workbook);
    let mut scan = CycleScan { graph: &graph, colors: HashMap::new(), stack: Vec::new(), cycles: Vec::new(), reported: HashSet::new() };
    let mut keys: Vec<CellKey> = graph.keys().copied().collect();
    keys.sort_unstable();
    for key in keys {
        if scan.colors.get(&key).copied().unwrap_or(0) == 0 {
            scan.visit(key);
        }
    }
    scan.cycles
}
