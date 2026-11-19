#![doc = include_str!("readme.md")]
use crate::{
    ast::{
        IntegerFormat, TomlArray, TomlArrayOfTables, TomlArrayOfTablesHeader, TomlBareKey, TomlBoolean, TomlDateTime, TomlFloat,
        TomlInlineTable, TomlInteger, TomlItem, TomlKey, TomlKeySegment, TomlKeyValue, TomlQuotedKey, TomlRoot, TomlString, TomlTable,
        TomlTableHeader, TomlValueNode,
    },
    language::TomlLanguage,
    lexer::{TomlLexer, token_type::TomlTokenKind},
    parser::TomlParser,
};
use data_text_core::{
    Builder, BuilderCache, GreenNode, GreenTree, Parser, SourceText, TextDiagnostics, TextEdit, TextError, builder::BuildOutput, source::Source,
};

/// TOML AST builder
pub struct TomlBuilder<'config> {
    config: &'config TomlLanguage,
}

impl<'config> TomlBuilder<'config> {
    /// Creates a new TOML builder
    pub fn new(config: &'config TomlLanguage) -> Self {
        Self { config }
    }

    fn build_root<'a>(
        &self,
        green_tree: &GreenNode<'a, TomlLanguage>,
        source: &SourceText,
        cache: &mut impl BuilderCache<TomlLanguage>,
    ) -> Result<TomlRoot, TextError> {
        if let Some(cached) = cache.get_typed_node::<TomlRoot>(green_tree) {
            return Ok(cached.clone());
        }

        let span = (0..green_tree.byte_length as usize).into();
        let mut items = Vec::new();
        let mut offset = 0usize;

        for child in green_tree.children {
            match child {
                GreenTree::Node(node) => {
                    match node.kind {
                        TomlTokenKind::KeyValue => items.push(TomlItem::KeyValue(self.build_key_value(node, offset, source)?)),
                        TomlTokenKind::Table => items.push(TomlItem::Table(self.build_table(node, offset, source)?)),
                        TomlTokenKind::ArrayOfTables => items.push(TomlItem::ArrayOfTables(self.build_array_of_tables(node, offset, source)?)),
                        _ => {}
                    }
                    offset += node.byte_length as usize;
                }
                GreenTree::Leaf(leaf) => offset += leaf.length as usize,
            }
        }

        let result = TomlRoot { span, items };
        cache.set_typed_node(green_tree, result.clone());
        Ok(result)
    }

    fn build_key(&self, node: &GreenNode<'_, TomlLanguage>, offset: usize, source: &SourceText) -> TomlKey {
        let mut segments = Vec::new();
        let mut current = offset;

        for child in node.children {
            if let GreenTree::Leaf(leaf) = child {
                let span: data_text_core::Range<usize> = (current..current + leaf.length as usize).into();
                match leaf.kind {
                    TomlTokenKind::BareKey => {
                        let name = source.get_text_in(span.clone()).into_owned();
                        segments.push(TomlKeySegment::Bare(TomlBareKey { span, name }));
                    }
                    TomlTokenKind::BasicString | TomlTokenKind::LiteralString => {
                        let raw = source.get_text_in(span.clone());
                        let value = decode_string(&raw, leaf.kind == TomlTokenKind::LiteralString);
                        segments.push(TomlKeySegment::Quoted(TomlQuotedKey { span, value }));
                    }
                    _ => {}
                }
                current += leaf.length as usize;
            }
            else if let GreenTree::Node(node) = child {
                current += node.byte_length as usize;
            }
        }

        TomlKey { span: (offset..offset + node.byte_length as usize).into(), segments }
    }

    fn build_key_value(&self, node: &GreenNode<'_, TomlLanguage>, offset: usize, source: &SourceText) -> Result<TomlKeyValue, TextError> {
        let span = (offset..offset + node.byte_length as usize).into();
        let mut key = None;
        let mut value = None;
        let mut current = offset;

        for child in node.children {
            match child {
                GreenTree::Node(child_node) => {
                    if child_node.kind == TomlTokenKind::Key && key.is_none() {
                        key = Some(self.build_key(child_node, current, source));
                    }
                    else if value.is_none() {
                        value = Some(self.build_node_value(child_node, current, source)?);
                    }
                    current += child_node.byte_length as usize;
                }
                GreenTree::Leaf(leaf) => {
                    if value.is_none() {
                        if let Some(leaf_value) = self.build_leaf_value(leaf.kind, current, leaf.length as usize, source) {
                            value = Some(leaf_value);
                        }
                    }
                    current += leaf.length as usize;
                }
            }
        }

        Ok(TomlKeyValue {
            key: key.ok_or_else(|| TextError::syntax_error("missing key", offset, source.source_ref()))?,
            value: value.ok_or_else(|| TextError::syntax_error("missing value", offset, source.source_ref()))?,
            span,
        })
    }

    fn build_table(&self, node: &GreenNode<'_, TomlLanguage>, offset: usize, source: &SourceText) -> Result<TomlTable, TextError> {
        let span = (offset..offset + node.byte_length as usize).into();
        let mut header_key = None;
        let mut header_start = offset;
        let mut header_end = offset;
        let mut items = Vec::new();
        let mut current = offset;

        for child in node.children {
            match child {
                GreenTree::Leaf(leaf) => {
                    match leaf.kind {
                        TomlTokenKind::LeftBracket => {
                            header_start = current;
                            header_end = current + leaf.length as usize;
                        }
                        TomlTokenKind::RightBracket => header_end = current + leaf.length as usize,
                        _ => {}
                    }
                    current += leaf.length as usize;
                }
                GreenTree::Node(child_node) => {
                    if child_node.kind == TomlTokenKind::Key && header_key.is_none() {
                        header_key = Some(self.build_key(child_node, current, source));
                        header_end = current + child_node.byte_length as usize;
                    }
                    else if child_node.kind == TomlTokenKind::KeyValue {
                        items.push(self.build_key_value(child_node, current, source)?);
                    }
                    current += child_node.byte_length as usize;
                }
            }
        }

        Ok(TomlTable {
            span,
            header: TomlTableHeader {
                span: (header_start..header_end).into(),
                key: header_key.ok_or_else(|| TextError::syntax_error("missing table header", offset, source.source_ref()))?,
            },
            items,
        })
    }

    fn build_array_of_tables(
        &self,
        node: &GreenNode<'_, TomlLanguage>,
        offset: usize,
        source: &SourceText,
    ) -> Result<TomlArrayOfTables, TextError> {
        let span = (offset..offset + node.byte_length as usize).into();
        let mut header_key = None;
        let mut header_start = offset;
        let mut header_end = offset;
        let mut items = Vec::new();
        let mut current = offset;

        for child in node.children {
            match child {
                GreenTree::Leaf(leaf) => {
                    match leaf.kind {
                        TomlTokenKind::DoubleLeftBracket => {
                            header_start = current;
                            header_end = current + leaf.length as usize;
                        }
                        TomlTokenKind::DoubleRightBracket => header_end = current + leaf.length as usize,
                        _ => {}
                    }
                    current += leaf.length as usize;
                }
                GreenTree::Node(child_node) => {
                    if child_node.kind == TomlTokenKind::Key && header_key.is_none() {
                        header_key = Some(self.build_key(child_node, current, source));
                        header_end = current + child_node.byte_length as usize;
                    }
                    else if child_node.kind == TomlTokenKind::KeyValue {
                        items.push(self.build_key_value(child_node, current, source)?);
                    }
                    current += child_node.byte_length as usize;
                }
            }
        }

        Ok(TomlArrayOfTables {
            span,
            header: TomlArrayOfTablesHeader {
                span: (header_start..header_end).into(),
                key: header_key.ok_or_else(|| TextError::syntax_error("missing array of tables header", offset, source.source_ref()))?,
            },
            items,
        })
    }

    fn build_node_value(&self, node: &GreenNode<'_, TomlLanguage>, offset: usize, source: &SourceText) -> Result<TomlValueNode, TextError> {
        match node.kind {
            TomlTokenKind::Array => self.build_array(node, offset, source),
            TomlTokenKind::InlineTable => self.build_inline_table(node, offset, source),
            _ => Err(TextError::syntax_error(format!("unexpected value node: {:?}", node.kind), offset, source.source_ref())),
        }
    }

    fn build_array(&self, node: &GreenNode<'_, TomlLanguage>, offset: usize, source: &SourceText) -> Result<TomlValueNode, TextError> {
        let span = (offset..offset + node.byte_length as usize).into();
        let mut items = Vec::new();
        let mut current = offset;

        for child in node.children {
            match child {
                GreenTree::Node(child_node) => {
                    items.push(self.build_node_value(child_node, current, source)?);
                    current += child_node.byte_length as usize;
                }
                GreenTree::Leaf(leaf) => {
                    if let Some(value) = self.build_leaf_value(leaf.kind, current, leaf.length as usize, source) {
                        items.push(value);
                    }
                    current += leaf.length as usize;
                }
            }
        }

        Ok(TomlValueNode::Array(TomlArray { span, items }))
    }

    fn build_inline_table(&self, node: &GreenNode<'_, TomlLanguage>, offset: usize, source: &SourceText) -> Result<TomlValueNode, TextError> {
        let span = (offset..offset + node.byte_length as usize).into();
        let mut items = Vec::new();
        let mut current = offset;

        for child in node.children {
            match child {
                GreenTree::Node(child_node) => {
                    if child_node.kind == TomlTokenKind::KeyValue {
                        items.push(self.build_key_value(child_node, current, source)?);
                    }
                    current += child_node.byte_length as usize;
                }
                GreenTree::Leaf(leaf) => current += leaf.length as usize,
            }
        }

        Ok(TomlValueNode::InlineTable(TomlInlineTable { span, items }))
    }

    fn build_leaf_value(&self, kind: TomlTokenKind, offset: usize, length: usize, source: &SourceText) -> Option<TomlValueNode> {
        let span: data_text_core::Range<usize> = (offset..offset + length).into();
        let raw = source.get_text_in(span.clone());

        match kind {
            TomlTokenKind::BasicString => Some(TomlValueNode::String(TomlString {
                span,
                value: decode_string(&raw, false),
                is_multiline: raw.starts_with("\"\"\""),
                is_literal: false,
            })),
            TomlTokenKind::LiteralString => Some(TomlValueNode::String(TomlString {
                span,
                value: decode_string(&raw, true),
                is_multiline: raw.starts_with("'''"),
                is_literal: true,
            })),
            TomlTokenKind::Integer => {
                let (value, format) = parse_integer(&raw);
                Some(TomlValueNode::Integer(TomlInteger { span, value, format }))
            }
            TomlTokenKind::Float => Some(TomlValueNode::Float(TomlFloat { span, value: parse_float(&raw) })),
            TomlTokenKind::Boolean => Some(TomlValueNode::Boolean(TomlBoolean { span, value: raw.as_ref() == "true" })),
            TomlTokenKind::OffsetDateTime | TomlTokenKind::LocalDateTime | TomlTokenKind::LocalDate | TomlTokenKind::LocalTime => {
                Some(TomlValueNode::DateTime(TomlDateTime { span, value: raw.into_owned() }))
            }
            _ => None,
        }
    }
}

impl<'config> Builder<TomlLanguage> for TomlBuilder<'config> {
    fn build<'a, S: Source + ?Sized>(
        &self,
        source: &S,
        edits: &[TextEdit],
        cache: &'a mut impl BuilderCache<TomlLanguage>,
    ) -> BuildOutput<TomlLanguage> {
        let parser = TomlParser::new(self.config);

        let mut session = data_text_core::parser::session::ParseSession::<TomlLanguage>::default();
        let parse_result = parser.parse(source, edits, &mut session);

        match parse_result.result {
            Ok(green_tree) => {
                let source_text = SourceText::new(source.get_text_in((0..source.length()).into()).into_owned());
                match self.build_root(&green_tree, &source_text, cache) {
                    Ok(ast_root) => TextDiagnostics { result: Ok(ast_root), diagnostics: parse_result.diagnostics },
                    Err(build_error) => {
                        let mut diagnostics = parse_result.diagnostics;
                        diagnostics.push(build_error.clone());
                        TextDiagnostics { result: Err(build_error), diagnostics }
                    }
                }
            }
            Err(parse_error) => TextDiagnostics { result: Err(parse_error), diagnostics: parse_result.diagnostics },
        }
    }
}

/// Decodes a TOML string literal, including multi-line and escape handling.
fn decode_string(raw: &str, is_literal: bool) -> String {
    let quote = match raw.chars().next() {
        Some('"') => '"',
        Some('\'') => '\'',
        _ => return raw.to_string(),
    };
    let delimiter = if raw.starts_with("\"\"\"") || raw.starts_with("'''") { 3 } else { 1 };
    let closing = if delimiter == 3 { format!("{}", quote.to_string().repeat(3)) } else { quote.to_string() };

    let mut body = raw;
    if body.len() >= delimiter {
        body = &body[delimiter..];
    }
    if body.ends_with(&closing) {
        body = &body[..body.len() - delimiter];
    }

    if delimiter == 3 {
        body = body.strip_prefix("\r\n").or_else(|| body.strip_prefix('\n')).unwrap_or(body);
    }

    if is_literal { body.to_string() } else { unescape_basic(body) }
}

/// Resolves escape sequences in a basic string.
fn unescape_basic(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }

        match chars.next() {
            Some('b') => out.push('\u{8}'),
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('f') => out.push('\u{c}'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('u') => {
                if let Some(ch) = read_hex_escape(&mut chars, 4) {
                    out.push(ch);
                }
            }
            Some('U') => {
                if let Some(ch) = read_hex_escape(&mut chars, 8) {
                    out.push(ch);
                }
            }
            Some('\r') => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                skip_leading_whitespace(&mut chars);
            }
            Some('\n') => skip_leading_whitespace(&mut chars),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }

    out
}

/// Reads `digits` hexadecimal characters and converts them to a `char`.
fn read_hex_escape(chars: &mut std::iter::Peekable<std::str::Chars<'_>>, digits: usize) -> Option<char> {
    let mut code = 0u32;
    for _ in 0..digits {
        code = code * 16 + chars.next()?.to_digit(16)?;
    }
    char::from_u32(code)
}

/// Skips whitespace following a line-ending backslash in a multi-line basic string.
fn skip_leading_whitespace(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(&ch) = chars.peek() {
        if ch.is_whitespace() {
            chars.next();
        }
        else {
            break;
        }
    }
}

/// Parses an integer literal with its radix and format.
fn parse_integer(text: &str) -> (i64, IntegerFormat) {
    let cleaned: String = text.chars().filter(|ch| *ch != '_').collect();
    let (sign, rest) = match cleaned.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None => (1i64, cleaned.strip_prefix('+').unwrap_or(&cleaned)),
    };

    let (digits, format, radix) = if let Some(rest) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        (rest, IntegerFormat::Hex, 16)
    }
    else if let Some(rest) = rest.strip_prefix("0o").or_else(|| rest.strip_prefix("0O")) {
        (rest, IntegerFormat::Octal, 8)
    }
    else if let Some(rest) = rest.strip_prefix("0b").or_else(|| rest.strip_prefix("0B")) {
        (rest, IntegerFormat::Binary, 2)
    }
    else {
        (rest, IntegerFormat::Decimal, 10)
    };

    (i64::from_str_radix(digits, radix).map(|value| value * sign).unwrap_or(0), format)
}

/// Parses a floating-point literal, including `inf` and `nan`.
fn parse_float(text: &str) -> f64 {
    let cleaned: String = text.chars().filter(|ch| *ch != '_').collect();
    match cleaned.as_str() {
        "inf" | "+inf" | "Infinity" | "+Infinity" => f64::INFINITY,
        "-inf" | "-Infinity" => f64::NEG_INFINITY,
        "nan" | "+nan" | "-nan" => f64::NAN,
        _ => cleaned.parse::<f64>().unwrap_or(0.0),
    }
}
