#![deny(missing_docs)]
//! S-expression reader and writer.
//!
//! KiCad, SPICE, and several symbol formats are parenthesized trees. This crate
//! keeps one tolerant reader: atoms, quoted strings with backslash escapes,
//! `;` line comments, and nested lists. The writer reproduces a canonical form
//! that the reader accepts back, so a tool can round-trip the parts it does not
//! understand instead of dropping them.

use std::fmt;

/// One S-expression node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// A bare symbol or number, stored verbatim.
    Atom(String),
    /// A quoted string, stored unescaped.
    Str(String),
    /// A parenthesized list.
    List(Vec<Node>),
}

impl Node {
    /// Create an atom.
    pub fn atom(value: impl Into<String>) -> Self {
        Self::Atom(value.into())
    }

    /// Create a quoted string.
    pub fn string(value: impl Into<String>) -> Self {
        Self::Str(value.into())
    }

    /// Create a list.
    pub fn list(items: Vec<Node>) -> Self {
        Self::List(items)
    }

    /// The atom text, when this node is an atom.
    pub fn as_atom(&self) -> Option<&str> {
        match self {
            Self::Atom(value) => Some(value),
            _ => None,
        }
    }

    /// The unescaped text, when this node is a quoted string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(value) => Some(value),
            _ => None,
        }
    }

    /// The items, when this node is a list.
    pub fn as_list(&self) -> Option<&[Node]> {
        match self {
            Self::List(items) => Some(items),
            _ => None,
        }
    }

    /// The head atom of a list, when the first item is an atom.
    pub fn head(&self) -> Option<&str> {
        match self {
            Self::List(items) => items.first().and_then(Self::as_atom),
            _ => None,
        }
    }

    /// The items after the head, or an empty slice for a non-list.
    pub fn children(&self) -> &[Node] {
        match self {
            Self::List(items) if !items.is_empty() => &items[1..],
            _ => &[],
        }
    }

    /// The first child list whose head is `name`.
    pub fn find(&self, name: &str) -> Option<&Node> {
        self.children().iter().find(|child| child.head() == Some(name))
    }

    /// Every child list whose head is `name`, in document order.
    pub fn find_all(&self, name: &str) -> Vec<&Node> {
        self.children().iter().filter(|child| child.head() == Some(name)).collect()
    }

    /// Canonical source text the reader accepts back.
    pub fn to_source(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            Self::Atom(value) => out.push_str(value),
            Self::Str(value) => {
                out.push('"');
                for character in value.chars() {
                    match character {
                        '\\' => out.push_str("\\\\"),
                        '"' => out.push_str("\\\""),
                        '\n' => out.push_str("\\n"),
                        '\t' => out.push_str("\\t"),
                        '\r' => out.push_str("\\r"),
                        other => out.push(other),
                    }
                }
                out.push('"');
            }
            Self::List(items) => {
                out.push('(');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(' ');
                    }
                    item.write(out);
                }
                out.push(')');
            }
        }
    }
}

impl fmt::Display for Node {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_source())
    }
}

/// Parse exactly one top-level form.
pub fn parse(source: &str) -> Result<Node, ParseError> {
    let mut forms = parse_all(source)?;
    match forms.len() {
        1 => Ok(forms.remove(0)),
        0 => Err(ParseError::new(0, "expected one S-expression, found none")),
        count => Err(ParseError::new(0, format!("expected one S-expression, found {count}"))),
    }
}

/// Parse every top-level form in the source.
pub fn parse_all(source: &str) -> Result<Vec<Node>, ParseError> {
    let mut parser = Parser { chars: source.char_indices().collect(), index: 0, length: source.len() };
    let mut forms = Vec::new();
    loop {
        parser.skip_trivia();
        if parser.peek().is_none() {
            return Ok(forms);
        }
        forms.push(parser.parse_node()?);
    }
}

/// A parse failure with the byte offset where it was detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// Byte offset into the source.
    offset: usize,
    /// Human-readable description.
    message: String,
}

impl ParseError {
    /// Create a parse error.
    pub fn new(offset: usize, message: impl Into<String>) -> Self {
        Self { offset, message: message.into() }
    }

    /// Byte offset into the source.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// The failure description.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} at byte {}", self.message, self.offset)
    }
}

impl std::error::Error for ParseError {}

struct Parser {
    chars: Vec<(usize, char)>,
    index: usize,
    length: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.index).map(|(_, character)| *character)
    }

    fn offset(&self) -> usize {
        self.chars.get(self.index).map(|(offset, _)| *offset).unwrap_or(self.length)
    }

    fn bump(&mut self) -> Option<char> {
        let character = self.peek()?;
        self.index += 1;
        Some(character)
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(character) if character.is_whitespace() => {
                    self.bump();
                }
                Some(';') => {
                    while let Some(character) = self.bump() {
                        if character == '\n' {
                            break;
                        }
                    }
                }
                _ => return,
            }
        }
    }

    fn parse_node(&mut self) -> Result<Node, ParseError> {
        self.skip_trivia();
        match self.peek() {
            Some('(') => self.parse_list(),
            Some('"') => self.parse_string(),
            Some(')') => Err(ParseError::new(self.offset(), "unexpected `)`")),
            Some(_) => Ok(Node::Atom(self.parse_atom())),
            None => Err(ParseError::new(self.offset(), "unexpected end of input")),
        }
    }

    fn parse_list(&mut self) -> Result<Node, ParseError> {
        let start = self.offset();
        self.bump();
        let mut items = Vec::new();
        loop {
            self.skip_trivia();
            match self.peek() {
                Some(')') => {
                    self.bump();
                    return Ok(Node::List(items));
                }
                Some(_) => items.push(self.parse_node()?),
                None => return Err(ParseError::new(start, "unterminated list")),
            }
        }
    }

    fn parse_string(&mut self) -> Result<Node, ParseError> {
        let start = self.offset();
        self.bump();
        let mut value = String::new();
        loop {
            match self.bump() {
                Some('"') => return Ok(Node::Str(value)),
                Some('\\') => match self.bump() {
                    Some('n') => value.push('\n'),
                    Some('t') => value.push('\t'),
                    Some('r') => value.push('\r'),
                    Some(other) => value.push(other),
                    None => return Err(ParseError::new(start, "unterminated string escape")),
                },
                Some(character) => value.push(character),
                None => return Err(ParseError::new(start, "unterminated string")),
            }
        }
    }

    fn parse_atom(&mut self) -> String {
        let mut value = String::new();
        while let Some(character) = self.peek() {
            if character.is_whitespace() || matches!(character, '(' | ')' | '"' | ';') {
                break;
            }
            value.push(character);
            self.bump();
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_lists_and_atoms() {
        let node = parse("(module demo (pin 1 GND) (pin 2 \"VCC\"))").expect("parse");
        assert_eq!(node.head(), Some("module"));
        assert_eq!(node.children().len(), 3);
        let pins = node.find_all("pin");
        assert_eq!(pins.len(), 2);
        assert_eq!(pins[0].children()[0].as_atom(), Some("1"));
        assert_eq!(pins[1].children()[1].as_str(), Some("VCC"));
    }

    #[test]
    fn skips_line_comments_between_forms() {
        let forms = parse_all("; header\n(a 1) ; trailing\n(b 2)\n").expect("parse");
        assert_eq!(forms.len(), 2);
        assert_eq!(forms[0].to_source(), "(a 1)");
        assert_eq!(forms[1].to_source(), "(b 2)");
    }

    #[test]
    fn round_trips_escaped_strings() {
        let node = Node::list(vec![Node::atom("write"), Node::string("a \"quoted\" \\ value\n")]);
        let text = node.to_source();
        let restored = parse(&text).expect("parse");
        assert_eq!(restored, node);
    }

    #[test]
    fn errors_report_offsets() {
        let error = parse("(a (b)").expect_err("unterminated");
        assert_eq!(error.offset(), 0);
        let error = parse("(a))").expect_err("extra close");
        assert_eq!(error.offset(), 3);
        let error = parse("").expect_err("empty");
        assert!(error.message().contains("none"));
    }

    #[test]
    fn children_and_find_ignore_non_lists() {
        let atom = Node::atom("x");
        assert!(atom.children().is_empty());
        assert!(atom.find("y").is_none());
        let list = Node::list(vec![Node::atom("root"), Node::list(vec![Node::atom("child")])]);
        assert!(list.find("child").is_some());
        assert!(list.find("root").is_none());
    }
}
