//! Minimal RTF reader.
//!
//! RTF is a group/control-word stream, not a document tree. This reader keeps
//! what the semantic IR can represent — paragraphs, hard line breaks and inline
//! bold/italic/underline/strike/superscript/subscript runs — and records a loss
//! marker for constructs it deliberately does not model: tables, lists, images,
//! fields, colors and the font table.
//!
//! Nothing here guesses content. A skipped destination (font table, picture,
//! field instruction) contributes no text, and unsupported table and list
//! structure degrades to paragraphs.

use document_ir::{Block, BreakKind, DocumentGraph, DocumentId, Inline, LossMarker, SemanticStatus};

use crate::FormatError;

const LOSS_TABLE: &str = "import.rtf.table";
const LOSS_LIST: &str = "import.rtf.list";
const LOSS_IMAGE: &str = "import.rtf.image";
const LOSS_FIELD: &str = "import.rtf.field";
const LOSS_COLOR: &str = "import.rtf.color";
const LOSS_FONT: &str = "import.rtf.font";

/// Losses observed while reading one document.
#[derive(Default)]
struct Losses {
    table: bool,
    list: bool,
    image: bool,
    field: bool,
    color: bool,
    font: bool,
}

impl Losses {
    fn push(&self, graph: &mut DocumentGraph) {
        let markers: [(bool, &str, &str); 6] = [
            (self.table, LOSS_TABLE, "RTF tables are imported as tab-separated paragraph text"),
            (self.list, LOSS_LIST, "RTF list markers are dropped and list items become paragraphs"),
            (self.image, LOSS_IMAGE, "RTF pictures are not imported"),
            (self.field, LOSS_FIELD, "RTF field instructions are dropped; field results are kept as text"),
            (self.color, LOSS_COLOR, "RTF color tables are not imported"),
            (self.font, LOSS_FONT, "RTF font tables are not imported"),
        ];
        for (seen, code, message) in markers {
            if seen {
                graph.push_loss(LossMarker { code: code.to_string(), message: message.to_string(), status: SemanticStatus::Partial });
            }
        }
    }
}

/// Character formatting active at one point in the stream.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct StyleSet {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    superscript: bool,
    subscript: bool,
}

impl StyleSet {
    fn names(self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.bold {
            names.push("bold");
        }
        if self.italic {
            names.push("italic");
        }
        if self.underline {
            names.push("underline");
        }
        if self.strike {
            names.push("strike");
        }
        if self.superscript {
            names.push("superscript");
        }
        if self.subscript {
            names.push("subscript");
        }
        names
    }

    fn wrap(self, text: String) -> Vec<Inline> {
        let mut inlines = vec![Inline::Text { text }];
        for style in self.names().into_iter().rev() {
            inlines = vec![Inline::Styled { style: style.to_string(), children: inlines }];
        }
        inlines
    }
}

/// A destination group whose content is skipped.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Destination {
    FontTable,
    ColorTable,
    Stylesheet,
    Info,
    Picture,
    FieldInstruction,
    ListText,
    Skipped,
}

/// Group-scoped parser state.
#[derive(Clone, Copy, Default)]
struct State {
    style: StyleSet,
    destination: Option<Destination>,
    ignorable: bool,
}

impl State {
    fn skipping(self) -> bool {
        self.ignorable || self.destination.is_some()
    }
}

/// Parses one RTF byte stream into a semantic graph.
pub fn import_rtf_bytes(label: &str, bytes: &[u8], id: DocumentId) -> Result<DocumentGraph, FormatError> {
    if !looks_like_rtf(bytes) {
        return Err(FormatError::invalid_input(format!("{label} is not an RTF document")));
    }
    let mut parser = Parser::new(bytes);
    parser.run();
    let mut graph = DocumentGraph::new(id);
    for content in parser.paragraphs {
        graph.push_block(Block::Paragraph { content });
    }
    parser.losses.push(&mut graph);
    Ok(graph)
}

/// Whether the bytes start with an RTF signature, ignoring a byte-order mark.
pub fn looks_like_rtf(bytes: &[u8]) -> bool {
    let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let start = body.iter().position(|byte| !byte.is_ascii_whitespace()).unwrap_or(body.len());
    body.get(start..).is_some_and(|rest| rest.starts_with(b"{\\rtf"))
}

struct Parser<'a> {
    bytes: &'a [u8],
    index: usize,
    state: State,
    stack: Vec<State>,
    current: Vec<Inline>,
    paragraphs: Vec<Vec<Inline>>,
    losses: Losses,
    /// Number of plain characters still to skip after a `\uN` fallback.
    skip_fallback: usize,
    /// Fallback width declared by the most recent `\ucN`, default one.
    fallback_width: usize,
}

impl<'a> Parser<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes: bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes),
            index: 0,
            state: State::default(),
            stack: Vec::new(),
            current: Vec::new(),
            paragraphs: Vec::new(),
            losses: Losses::default(),
            skip_fallback: 0,
            fallback_width: 1,
        }
    }

    fn run(&mut self) {
        while let Some(byte) = self.bytes.get(self.index).copied() {
            match byte {
                b'{' => {
                    self.index += 1;
                    self.stack.push(self.state);
                }
                b'}' => {
                    self.index += 1;
                    if let Some(previous) = self.stack.pop() {
                        self.state = previous;
                    }
                }
                b'\\' => self.read_control(),
                b'\r' | b'\n' | b'\t' => self.index += 1,
                _ => {
                    self.index += 1;
                    self.consume_text(cp1252(byte));
                }
            }
        }
        self.flush_paragraph();
    }

    fn read_control(&mut self) {
        self.index += 1;
        let Some(first) = self.bytes.get(self.index).copied()
        else {
            return;
        };
        if first.is_ascii_alphabetic() {
            self.read_control_word();
        }
        else {
            self.read_control_symbol(first);
        }
    }

    fn read_control_word(&mut self) {
        let start = self.index;
        while self.bytes.get(self.index).is_some_and(u8::is_ascii_alphabetic) {
            self.index += 1;
        }
        let name = std::str::from_utf8(&self.bytes[start..self.index]).unwrap_or_default().to_ascii_lowercase();
        let parameter = self.read_parameter();
        self.apply_control(&name, parameter);
    }

    fn read_parameter(&mut self) -> Option<i32> {
        let start = self.index;
        if self.bytes.get(self.index) == Some(&b'-') {
            self.index += 1;
        }
        let digits = self.index;
        while self.bytes.get(self.index).is_some_and(u8::is_ascii_digit) {
            self.index += 1;
        }
        let value = if self.index > digits {
            std::str::from_utf8(&self.bytes[start..self.index]).ok().and_then(|raw| raw.parse::<i32>().ok())
        }
        else {
            self.index = start;
            None
        };
        if self.bytes.get(self.index) == Some(&b' ') {
            self.index += 1;
        }
        value
    }

    fn read_control_symbol(&mut self, symbol: u8) {
        self.index += 1;
        match symbol {
            b'\'' => {
                let code = self.read_hex_byte();
                if let Some(byte) = code {
                    self.consume_text(cp1252(byte));
                }
            }
            b'\\' | b'{' | b'}' => self.consume_text(symbol as char),
            b'~' => self.consume_text('\u{00A0}'),
            b'_' => self.consume_text('\u{2011}'),
            b'-' => {}
            b'*' => self.state.ignorable = true,
            _ => {}
        }
    }

    fn read_hex_byte(&mut self) -> Option<u8> {
        let high = self.bytes.get(self.index).copied().and_then(hex_value)?;
        let low = self.bytes.get(self.index + 1).copied().and_then(hex_value)?;
        self.index += 2;
        Some(high << 4 | low)
    }

    fn apply_control(&mut self, name: &str, parameter: Option<i32>) {
        let enabled = parameter != Some(0);
        match name {
            "par" | "row" => self.flush_paragraph(),
            "line" => {
                if !self.state.skipping() {
                    self.current.push(Inline::Break { kind: BreakKind::Hard });
                }
            }
            "b" => self.state.style.bold = enabled,
            "i" => self.state.style.italic = enabled,
            "ul" => self.state.style.underline = enabled,
            "strike" => self.state.style.strike = enabled,
            "super" => {
                self.state.style.superscript = enabled;
                if enabled {
                    self.state.style.subscript = false;
                }
            }
            "sub" => {
                self.state.style.subscript = enabled;
                if enabled {
                    self.state.style.superscript = false;
                }
            }
            "nosupersub" => {
                self.state.style.superscript = false;
                self.state.style.subscript = false;
            }
            "plain" => self.state.style = StyleSet::default(),
            "uc" => self.fallback_width = parameter.unwrap_or(1).clamp(0, 16) as usize,
            "u" => {
                if let Some(value) = parameter {
                    let code = if value < 0 { value + 0x1_0000 } else { value };
                    if let Some(character) = u32::try_from(code).ok().and_then(char::from_u32) {
                        self.consume_text(character);
                    }
                    self.skip_fallback = self.fallback_width;
                }
            }
            "bin" => {
                if let Some(count) = parameter
                    && count > 0
                {
                    self.index = self.index.saturating_add(count as usize).min(self.bytes.len());
                }
            }
            "fonttbl" => {
                self.state.destination = Some(Destination::FontTable);
                self.losses.font = true;
            }
            "colortbl" => {
                self.state.destination = Some(Destination::ColorTable);
                self.losses.color = true;
            }
            "stylesheet" => self.state.destination = Some(Destination::Stylesheet),
            "info" => self.state.destination = Some(Destination::Info),
            "pict" => {
                self.state.destination = Some(Destination::Picture);
                self.losses.image = true;
            }
            "fldinst" => {
                self.state.destination = Some(Destination::FieldInstruction);
                self.losses.field = true;
            }
            "fldrslt" => self.state.destination = None,
            "field" => self.losses.field = true,
            "listtext" | "pntext" => {
                self.state.destination = Some(Destination::ListText);
                self.losses.list = true;
            }
            "ls" | "ilvl" | "pn" => self.losses.list = true,
            "header" | "footer" | "footnote" | "annotation" | "object" | "shpinst" | "datastore" | "themedata" => {
                self.state.destination = Some(Destination::Skipped);
            }
            "intbl" | "trowd" | "cellx" | "clmgf" | "clmrg" => self.losses.table = true,
            "cell" => {
                self.losses.table = true;
                if !self.state.skipping() {
                    self.consume_text('\t');
                }
            }
            "tab" => {
                if !self.state.skipping() {
                    self.consume_text('\t');
                }
            }
            "emdash" => self.consume_text('\u{2014}'),
            "endash" => self.consume_text('\u{2013}'),
            "lquote" => self.consume_text('\u{2018}'),
            "rquote" => self.consume_text('\u{2019}'),
            "ldblquote" => self.consume_text('\u{201C}'),
            "rdblquote" => self.consume_text('\u{201D}'),
            "bullet" => self.consume_text('\u{2022}'),
            _ => {}
        }
    }

    fn consume_text(&mut self, character: char) {
        if self.skip_fallback > 0 {
            self.skip_fallback -= 1;
            return;
        }
        if self.state.skipping() {
            return;
        }
        if self.state.style.names().is_empty() {
            if let Some(Inline::Text { text }) = self.current.last_mut() {
                text.push(character);
                return;
            }
        }
        self.current.extend(self.state.style.wrap(character.to_string()));
    }

    fn flush_paragraph(&mut self) {
        if self.current.is_empty() {
            return;
        }
        if let Some(Inline::Text { text }) = self.current.last_mut() {
            let trimmed = text.trim_end_matches('\t');
            text.truncate(trimmed.len());
        }
        self.current.retain(|inline| !matches!(inline, Inline::Text { text } if text.is_empty()));
        let text = self.current.iter().map(Inline::plain_text).collect::<String>();
        if !text.trim().is_empty() {
            self.paragraphs.push(std::mem::take(&mut self.current));
        }
        else {
            self.current.clear();
        }
    }
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Windows-1252, the RTF default code page for `\'hh` escapes.
fn cp1252(byte: u8) -> char {
    match byte {
        0x80 => '\u{20AC}',
        0x82 => '\u{201A}',
        0x83 => '\u{0192}',
        0x84 => '\u{201E}',
        0x85 => '\u{2026}',
        0x86 => '\u{2020}',
        0x87 => '\u{2021}',
        0x88 => '\u{02C6}',
        0x89 => '\u{2030}',
        0x8A => '\u{0160}',
        0x8B => '\u{2039}',
        0x8C => '\u{0152}',
        0x8E => '\u{017D}',
        0x91 => '\u{2018}',
        0x92 => '\u{2019}',
        0x93 => '\u{201C}',
        0x94 => '\u{201D}',
        0x95 => '\u{2022}',
        0x96 => '\u{2013}',
        0x97 => '\u{2014}',
        0x98 => '\u{02DC}',
        0x99 => '\u{2122}',
        0x9A => '\u{0161}',
        0x9B => '\u{203A}',
        0x9C => '\u{0153}',
        0x9E => '\u{017E}',
        0x9F => '\u{0178}',
        other => other as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_paragraphs_breaks_and_styles() {
        let source = br"{\rtf1\ansi{\fonttbl{\f0 Arial;}}\pard plain text\par {\b bold} and {\i italic}\line next\par}";
        let graph = import_rtf_bytes("sample.rtf", source, DocumentId(1)).expect("graph");
        let paragraphs = graph
            .blocks
            .iter()
            .filter_map(|node| match &node.block {
                Block::Paragraph { content } => Some(content.iter().map(Inline::plain_text).collect::<String>()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(paragraphs, vec!["plain text".to_owned(), "bold and italic\nnext".to_owned()]);
        assert!(graph.coverage.loss.iter().any(|marker| marker.code == LOSS_FONT));
    }

    #[test]
    fn rejects_non_rtf_bytes() {
        assert!(import_rtf_bytes("x.rtf", b"hello", DocumentId(1)).is_err());
        assert!(!looks_like_rtf(b"hello"));
    }
}
