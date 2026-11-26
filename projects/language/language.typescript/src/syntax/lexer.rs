//! 真词法分析器：字符串（单/双引号、转义）、数字、标识符、运算符、
//! 行注释与块注释，token 携带行列位置。

use super::error::SyntaxError;

/// 标点 token 的运算符身份。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Punct {
    /// `{`
    BraceOpen,
    /// `}`
    BraceClose,
    /// `(`
    ParenOpen,
    /// `)`
    ParenClose,
    /// `[`
    BracketOpen,
    /// `]`
    BracketClose,
    /// `;`
    Semicolon,
    /// `,`
    Comma,
    /// `.`
    Dot,
    /// `:`
    Colon,
    /// `?`
    Question,
    /// `=>`
    Arrow,
    /// `===`
    StrictEq,
    /// `!==`
    StrictNeq,
    /// `==`
    EqEq,
    /// `!=`
    NotEq,
    /// `<`
    Lt,
    /// `<=`
    LtEq,
    /// `>`
    Gt,
    /// `>=`
    GtEq,
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `%`
    Percent,
    /// `&&`
    AmpAmp,
    /// `||`
    PipePipe,
    /// `!`
    Bang,
    /// `=`
    Eq,
    /// `|`（仅出现在类型注解的联合分隔）
    Bar,
}

impl Punct {
    /// 该标点的字面拼写，用于错误信息中的“期望”。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BraceOpen => "{",
            Self::BraceClose => "}",
            Self::ParenOpen => "(",
            Self::ParenClose => ")",
            Self::BracketOpen => "[",
            Self::BracketClose => "]",
            Self::Semicolon => ";",
            Self::Comma => ",",
            Self::Dot => ".",
            Self::Colon => ":",
            Self::Question => "?",
            Self::Arrow => "=>",
            Self::StrictEq => "===",
            Self::StrictNeq => "!==",
            Self::EqEq => "==",
            Self::NotEq => "!=",
            Self::Lt => "<",
            Self::LtEq => "<=",
            Self::Gt => ">",
            Self::GtEq => ">=",
            Self::Plus => "+",
            Self::Minus => "-",
            Self::Star => "*",
            Self::Slash => "/",
            Self::Percent => "%",
            Self::AmpAmp => "&&",
            Self::PipePipe => "||",
            Self::Bang => "!",
            Self::Eq => "=",
            Self::Bar => "|",
        }
    }
}

/// token 类别。
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// 标识符或关键字（关键字由语法层按文本识别）。
    Identifier,
    /// 数字字面量（已结算为 `f64`）。
    Number(f64),
    /// 字符串字面量（转义已结算）。
    Str,
    /// 标点。
    Punct(Punct),
    /// 输入结束。
    Eof,
}

/// 一个带位置的 token。
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    /// token 类别。
    pub kind: TokenKind,
    /// 原始文本（标识符名、字符串内容、标点拼写）。
    pub text: String,
    /// 1 起算行号。
    pub line: usize,
    /// 1 起算列号。
    pub column: usize,
}

/// 词法分析：产出带位置的 token 流，以 [`TokenKind::Eof`] 收尾。
///
/// 未闭合的字符串或块注释、非法数字结构都按行列结构化失败。
pub fn lex(source: &str) -> Result<Vec<Token>, SyntaxError> {
    let chars: Vec<char> = source.chars().collect();
    let mut index = 0usize;
    let mut line = 1usize;
    let mut column = 1usize;
    let mut tokens = Vec::new();

    while index < chars.len() {
        let ch = chars[index];
        // 空白与注释。
        if ch == '\n' {
            index += 1;
            line += 1;
            column = 1;
            continue;
        }
        if ch.is_whitespace() {
            index += 1;
            column += 1;
            continue;
        }
        if ch == '/' && index + 1 < chars.len() && chars[index + 1] == '/' {
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
                column += 1;
            }
            continue;
        }
        if ch == '/' && index + 1 < chars.len() && chars[index + 1] == '*' {
            let start_line = line;
            let start_column = column;
            index += 2;
            column += 2;
            loop {
                if index + 1 >= chars.len() {
                    return Err(SyntaxError::new("块注释未闭合", "`*/`", start_line, start_column));
                }
                if chars[index] == '*' && chars[index + 1] == '/' {
                    index += 2;
                    column += 2;
                    break;
                }
                if chars[index] == '\n' {
                    line += 1;
                    column = 1;
                }
                else {
                    column += 1;
                }
                index += 1;
            }
            continue;
        }
        // 字符串字面量。
        if ch == '\'' || ch == '"' {
            let start_line = line;
            let start_column = column;
            let quote = ch;
            index += 1;
            column += 1;
            let mut value = String::new();
            loop {
                if index >= chars.len() {
                    return Err(SyntaxError::new("字符串字面量未闭合", format!("闭合的 `{quote}`"), start_line, start_column));
                }
                let current = chars[index];
                if current == '\n' {
                    return Err(SyntaxError::new("字符串字面量跨行未转义", format!("闭合的 `{quote}`"), line, column));
                }
                if current == quote {
                    index += 1;
                    column += 1;
                    break;
                }
                if current == '\\' {
                    index += 1;
                    column += 1;
                    if index >= chars.len() {
                        return Err(SyntaxError::new("转义序列在输入末尾截断", "转义字符", line, column));
                    }
                    let escaped = chars[index];
                    match escaped {
                        'n' => value.push('\n'),
                        'r' => value.push('\r'),
                        't' => value.push('\t'),
                        'b' => value.push('\u{0008}'),
                        'f' => value.push('\u{000C}'),
                        'v' => value.push('\u{000B}'),
                        '0' => value.push('\0'),
                        '\\' => value.push('\\'),
                        '\'' => value.push('\''),
                        '"' => value.push('"'),
                        other => value.push(other),
                    }
                    index += 1;
                    column += 1;
                    continue;
                }
                value.push(current);
                index += 1;
                column += 1;
            }
            tokens.push(Token { kind: TokenKind::Str, text: value, line: start_line, column: start_column });
            continue;
        }
        // 数字字面量。
        if ch.is_ascii_digit() || (ch == '.' && index + 1 < chars.len() && chars[index + 1].is_ascii_digit()) {
            let start_line = line;
            let start_column = column;
            let start_index = index;
            while index < chars.len() && chars[index].is_ascii_digit() {
                index += 1;
                column += 1;
            }
            if index < chars.len() && chars[index] == '.' {
                index += 1;
                column += 1;
                while index < chars.len() && chars[index].is_ascii_digit() {
                    index += 1;
                    column += 1;
                }
            }
            if index < chars.len() && (chars[index] == 'e' || chars[index] == 'E') {
                let save = index;
                index += 1;
                column += 1;
                if index < chars.len() && (chars[index] == '+' || chars[index] == '-') {
                    index += 1;
                    column += 1;
                }
                if index < chars.len() && chars[index].is_ascii_digit() {
                    while index < chars.len() && chars[index].is_ascii_digit() {
                        index += 1;
                        column += 1;
                    }
                }
                else {
                    index = save;
                }
            }
            let text: String = chars[start_index..index].iter().collect();
            let value: f64 = text.parse().map_err(|_| SyntaxError::new(format!("非法数字字面量 `{text}`"), "十进制数字", start_line, start_column))?;
            tokens.push(Token { kind: TokenKind::Number(value), text, line: start_line, column: start_column });
            continue;
        }
        // 标识符 / 关键字。
        if ch == '_' || ch == '$' || ch.is_alphabetic() {
            let start_line = line;
            let start_column = column;
            let start_index = index;
            while index < chars.len() {
                let current = chars[index];
                if current == '_' || current == '$' || current.is_alphanumeric() {
                    index += 1;
                    column += 1;
                }
                else {
                    break;
                }
            }
            let text: String = chars[start_index..index].iter().collect();
            tokens.push(Token { kind: TokenKind::Identifier, text, line: start_line, column: start_column });
            continue;
        }
        // 标点（最长匹配优先）。
        let (punct, width) = match ch {
            '{' => (Punct::BraceOpen, 1),
            '}' => (Punct::BraceClose, 1),
            '(' => (Punct::ParenOpen, 1),
            ')' => (Punct::ParenClose, 1),
            '[' => (Punct::BracketOpen, 1),
            ']' => (Punct::BracketClose, 1),
            ';' => (Punct::Semicolon, 1),
            ',' => (Punct::Comma, 1),
            '.' => (Punct::Dot, 1),
            ':' => (Punct::Colon, 1),
            '?' => (Punct::Question, 1),
            '%' => (Punct::Percent, 1),
            '=' => {
                if index + 2 < chars.len() && chars[index + 1] == '=' && chars[index + 2] == '=' {
                    (Punct::StrictEq, 3)
                }
                else if index + 1 < chars.len() && chars[index + 1] == '=' {
                    (Punct::EqEq, 2)
                }
                else if index + 1 < chars.len() && chars[index + 1] == '>' {
                    (Punct::Arrow, 2)
                }
                else {
                    (Punct::Eq, 1)
                }
            }
            '!' => {
                if index + 1 < chars.len() && chars[index + 1] == '=' && index + 2 < chars.len() && chars[index + 2] == '=' {
                    (Punct::StrictNeq, 3)
                }
                else if index + 1 < chars.len() && chars[index + 1] == '=' {
                    (Punct::NotEq, 2)
                }
                else {
                    (Punct::Bang, 1)
                }
            }
            '<' => {
                if index + 1 < chars.len() && chars[index + 1] == '=' {
                    (Punct::LtEq, 2)
                }
                else {
                    (Punct::Lt, 1)
                }
            }
            '>' => {
                if index + 1 < chars.len() && chars[index + 1] == '=' {
                    (Punct::GtEq, 2)
                }
                else {
                    (Punct::Gt, 1)
                }
            }
            '+' => (Punct::Plus, 1),
            '-' => (Punct::Minus, 1),
            '*' => (Punct::Star, 1),
            '/' => (Punct::Slash, 1),
            '&' => {
                if index + 1 < chars.len() && chars[index + 1] == '&' {
                    (Punct::AmpAmp, 2)
                }
                else {
                    return Err(SyntaxError::new("单独的 `&` 不在子集内", "`&&`", line, column));
                }
            }
            '|' => {
                if index + 1 < chars.len() && chars[index + 1] == '|' {
                    (Punct::PipePipe, 2)
                }
                else {
                    (Punct::Bar, 1)
                }
            }
            other => return Err(SyntaxError::new(format!("无法识别的字符 `{other}`"), "合法的 token 起始", line, column)),
        };
        tokens.push(Token { kind: TokenKind::Punct(punct), text: punct.as_str().to_string(), line, column });
        index += width;
        column += width;
    }
    tokens.push(Token { kind: TokenKind::Eof, text: String::new(), line, column });
    Ok(tokens)
}
