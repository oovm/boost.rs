//! Minimal RTF writer.
//!
//! Paragraphs, hard breaks and the character styles the reader understands are
//! written directly. Tables degrade to tab-separated paragraph rows; pictures
//! are skipped and hyperlink destinations are dropped while their text stays.
//! Every degradation is reported as a [`LossMarker`] instead of being silent.

use std::collections::HashSet;

use document_ir::{Block, BreakKind, DocumentGraph, Inline, LossMarker, NodeId, SemanticStatus, TableCell};

use crate::FormatError;

const LOSS_EXPORT_TABLE: &str = "export.rtf.table";
const LOSS_EXPORT_IMAGE: &str = "export.rtf.image";
const LOSS_EXPORT_LINK: &str = "export.rtf.link";

/// RTF text plus the semantics the export explicitly declares as lost.
pub struct RtfExport {
    /// The serialized RTF stream.
    pub text: String,
    /// Declared losses, one marker per known degradation.
    pub losses: Vec<LossMarker>,
}

/// Writes one graph as an RTF 1.0 stream.
pub fn export_rtf(graph: &DocumentGraph) -> Result<String, FormatError> {
    Ok(export_rtf_report(graph)?.text)
}

/// Writes one graph as an RTF 1.0 stream and reports declared losses.
pub fn export_rtf_report(graph: &DocumentGraph) -> Result<RtfExport, FormatError> {
    let validation = graph.validate();
    if !validation.is_valid() {
        return Err(FormatError::invalid_input(format!("invalid document graph: {:?}", validation.issues)));
    }
    let mut renderer = Renderer::new();
    let text = renderer.render(graph);
    Ok(RtfExport { text, losses: renderer.losses() })
}

struct Renderer {
    out: String,
    seen: HashSet<NodeId>,
    table: bool,
    image: bool,
    link: bool,
}

impl Renderer {
    fn new() -> Self {
        let out = String::from("{\\rtf1\\ansi\\ansicpg1252\\deff0{\\fonttbl{\\f0\\fnil\\fcharset0 Times New Roman;}}\\uc1 ");
        Self { out, seen: HashSet::new(), table: false, image: false, link: false }
    }

    fn render(&mut self, graph: &DocumentGraph) -> String {
        let nested = nested_block_ids(graph);
        for node in &graph.blocks {
            if !nested.contains(&node.id) {
                self.block(graph, node.id);
            }
        }
        self.out.push_str("}\n");
        std::mem::take(&mut self.out)
    }

    fn losses(&self) -> Vec<LossMarker> {
        let markers = [
            (self.table, LOSS_EXPORT_TABLE, "RTF tables are exported as tab-separated paragraph rows"),
            (self.image, LOSS_EXPORT_IMAGE, "RTF pictures are not exported; image references are skipped"),
            (self.link, LOSS_EXPORT_LINK, "RTF hyperlink destinations are not exported; link text is kept"),
        ];
        markers
            .into_iter()
            .filter(|(seen, _, _)| *seen)
            .map(|(_, code, message)| LossMarker {
                code: code.to_string(),
                message: message.to_string(),
                status: SemanticStatus::Partial,
            })
            .collect()
    }

    fn block(&mut self, graph: &DocumentGraph, id: NodeId) {
        if !self.seen.insert(id) {
            return;
        }
        let Some(node) = graph.block(id)
        else {
            return;
        };
        match &node.block {
            Block::Section { title, children, .. } => {
                self.out.push_str("\\pard\\plain\\b ");
                self.inlines(title);
                self.out.push_str("\\b0 \\par\n");
                for child in children {
                    self.block(graph, *child);
                }
            }
            Block::Paragraph { content } => {
                self.out.push_str("\\pard\\plain ");
                self.inlines(content);
                self.out.push_str("\\par\n");
            }
            Block::List { ordered, items, .. } => {
                for (index, item) in items.iter().enumerate() {
                    self.out.push_str("\\pard\\plain ");
                    if *ordered {
                        self.out.push_str(&format!("{}. ", item.number.unwrap_or(index as u32 + 1)));
                    }
                    else {
                        self.out.push_str("\\u8226? ");
                    }
                    self.inlines(&item.content);
                    self.out.push_str("\\par\n");
                    for child in &item.children {
                        self.block(graph, *child);
                    }
                }
            }
            Block::Table { head, body, foot, .. } => {
                self.table = true;
                for row in head.iter().chain(body.iter()).chain(foot.iter()) {
                    self.out.push_str("\\pard\\plain ");
                    let cells = row.cells.iter().map(|cell| cell_text(graph, cell)).collect::<Vec<_>>();
                    self.out.push_str(&cells.join("\\tab "));
                    self.out.push_str("\\par\n");
                }
            }
            Block::Quote { children } => {
                for child in children {
                    self.block(graph, *child);
                }
            }
            Block::Code { content, .. } => self.paragraph(content),
            Block::Math { formula } => self.paragraph(&formula.source),
            Block::Diagram { source, .. } | Block::Chart { source, .. } | Block::Engraving { source, .. } | Block::Depiction { source, .. } => {
                self.paragraph(source)
            }
            Block::RawHtml { html } => self.paragraph(html),
            Block::Opaque { payload_hint, .. } if !payload_hint.is_empty() => self.paragraph(payload_hint),
            _ => {}
        }
    }

    fn paragraph(&mut self, text: &str) {
        self.out.push_str("\\pard\\plain ");
        self.text(text);
        self.out.push_str("\\par\n");
    }

    fn inlines(&mut self, inlines: &[Inline]) {
        for inline in inlines {
            self.inline(inline);
        }
    }

    fn inline(&mut self, inline: &Inline) {
        match inline {
            Inline::Text { text } | Inline::InlineCode { text } => self.text(text),
            Inline::Styled { style, children } => match style_controls(style) {
                Some((open, close)) => {
                    self.out.push_str(open);
                    self.inlines(children);
                    self.out.push_str(close);
                }
                None => self.inlines(children),
            },
            Inline::Attributed { children, .. } => self.inlines(children),
            Inline::Link { display, .. } => {
                self.link = true;
                self.inlines(display);
            }
            Inline::Image { .. } => self.image = true,
            Inline::Reference { display, .. } => self.text(display),
            Inline::Note { label, display } => match display {
                Some(display) => self.inlines(display),
                None => self.text(label),
            },
            Inline::Citation { key, display } => match display {
                Some(display) => self.inlines(display),
                None => self.text(key),
            },
            Inline::InlineMath { formula } => self.text(&formula.source),
            Inline::Break { kind } => match kind {
                BreakKind::Hard => self.out.push_str("\\line "),
                BreakKind::Soft => self.out.push(' '),
            },
            Inline::RawHtml { .. } => {}
        }
    }

    /// Escapes control characters and writes every code point outside Latin-1
    /// as a `\uN?` escape with its UTF-16 code unit.
    fn text(&mut self, text: &str) {
        for character in text.chars() {
            match character {
                '\\' => self.out.push_str("\\\\"),
                '{' => self.out.push_str("\\{"),
                '}' => self.out.push_str("\\}"),
                '\n' => self.out.push_str("\\line "),
                '\r' => {}
                '\t' => self.out.push_str("\\tab "),
                value if (value as u32) < 0x80 => self.out.push(value),
                value if (value as u32) <= 0xFF => self.out.push_str(&format!("\\'{:02x}", value as u32)),
                value => {
                    let mut units = [0u16; 2];
                    for unit in value.encode_utf16(&mut units) {
                        self.out.push_str(&format!("\\u{}?", *unit as i16 as i32));
                    }
                }
            }
        }
    }
}

fn style_controls(style: &str) -> Option<(&'static str, &'static str)> {
    match style.to_ascii_lowercase().as_str() {
        "bold" | "strong" => Some(("\\b ", "\\b0 ")),
        "italic" | "emph" | "emphasis" => Some(("\\i ", "\\i0 ")),
        "underline" => Some(("\\ul ", "\\ul0 ")),
        "strike" | "strikethrough" | "strikeout" => Some(("\\strike ", "\\strike0 ")),
        "superscript" | "sup" => Some(("\\super ", "\\nosupersub ")),
        "subscript" | "sub" => Some(("\\sub ", "\\nosupersub ")),
        _ => None,
    }
}

fn cell_text(graph: &DocumentGraph, cell: &TableCell) -> String {
    cell.children
        .iter()
        .filter_map(|id| graph.block(*id))
        .filter_map(|node| match &node.block {
            Block::Paragraph { content } => Some(content.iter().map(Inline::plain_text).collect::<String>()),
            _ => None,
        })
        .map(|text| text.replace('\n', " "))
        .collect::<Vec<_>>()
        .join(" ")
}

fn nested_block_ids(graph: &DocumentGraph) -> HashSet<NodeId> {
    let mut ids = HashSet::new();
    for node in &graph.blocks {
        match &node.block {
            Block::Section { children, .. } | Block::Quote { children, .. } => ids.extend(children.iter().copied()),
            Block::List { items, .. } => {
                for item in items {
                    ids.extend(item.children.iter().copied());
                }
            }
            Block::Table { head, body, foot, .. } => {
                for cell in head.iter().chain(body).chain(foot).flat_map(|row| row.cells.iter()) {
                    ids.extend(cell.children.iter().copied());
                }
            }
            _ => {}
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use document_ir::{DocumentId, BreakKind};

    #[test]
    fn writes_paragraphs_and_round_trips_styles() {
        let mut graph = DocumentGraph::new(DocumentId(7));
        graph.push_block(Block::Paragraph { content: vec![Inline::Text { text: "plain".to_owned() }] });
        graph.push_block(Block::Paragraph {
            content: vec![
                Inline::Styled { style: "bold".to_owned(), children: vec![Inline::Text { text: "bold".to_owned() }] },
                Inline::Break { kind: BreakKind::Hard },
                Inline::Text { text: "\u{4E2D}\u{6587}".to_owned() },
            ],
        });
        let report = export_rtf_report(&graph).expect("rtf");
        assert!(report.text.starts_with("{\\rtf1"));
        assert!(report.text.contains("\\b bold\\b0 "));
        assert!(report.text.contains("\\u20013?\\u25991?"));
        assert!(report.losses.is_empty());

        let imported = crate::import::rtf::import_rtf_bytes("roundtrip.rtf", report.text.as_bytes(), DocumentId(8)).expect("import");
        assert_eq!(imported.blocks.len(), 2);
    }

    #[test]
    fn reports_table_and_image_losses() {
        let mut graph = DocumentGraph::new(DocumentId(9));
        let cell = graph.push_block(Block::Paragraph { content: vec![Inline::Text { text: "cell".to_owned() }] });
        graph.push_block(Block::Table {
            caption: None,
            head: vec![],
            body: vec![document_ir::TableRow {
                cells: vec![TableCell { children: vec![cell], colspan: 1, rowspan: 1 }],
            }],
            foot: vec![],
        });
        graph.push_block(Block::Paragraph {
            content: vec![Inline::Image { source: "a.png".to_owned(), alt: vec![], title: None, asset: None }],
        });
        let report = export_rtf_report(&graph).expect("rtf");
        assert!(report.losses.iter().any(|marker| marker.code == LOSS_EXPORT_TABLE));
        assert!(report.losses.iter().any(|marker| marker.code == LOSS_EXPORT_IMAGE));
    }
}
