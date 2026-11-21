//! Read-only projections over a [`DocumentGraph`].
//!
//! Every authoring surface needs the same three views: a structural summary,
//! document statistics and full-text search. All of them walk the graph in the
//! same depth-first order, so a block index reported by one view addresses the
//! same block in the others.
//!
//! Projections return typed values rather than a serialized shape: a product
//! keeps its own JSON contract and only borrows the traversal and counting.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::{block::Block, graph::DocumentGraph, id::NodeId, inline::Inline, status::SemanticStatus};

/// Reading speed assumed by [`DocumentStatistics::reading_minutes`].
pub const WORDS_PER_MINUTE: usize = 200;

/// Direct child block ids of one block, in document order.
pub fn child_ids(block: &Block) -> Vec<NodeId> {
    DocumentGraph::immediate_children(block)
}

/// Top-level blocks no container references, in graph order.
pub fn roots(graph: &DocumentGraph) -> Vec<NodeId> {
    let mut referenced = HashSet::new();
    for node in &graph.blocks {
        referenced.extend(child_ids(&node.block));
    }
    graph.blocks.iter().map(|node| node.id).filter(|id| !referenced.contains(id)).collect()
}

/// Depth-first document order, parents before their children.
pub fn flat_order(graph: &DocumentGraph) -> Vec<NodeId> {
    let mut order = Vec::with_capacity(graph.blocks.len());
    let mut seen = HashSet::new();
    for root in roots(graph) {
        push_pre_order(graph, root, &mut seen, &mut order);
    }
    order
}

fn push_pre_order(graph: &DocumentGraph, id: NodeId, seen: &mut HashSet<NodeId>, order: &mut Vec<NodeId>) {
    if !seen.insert(id) {
        return;
    }
    order.push(id);
    if let Some(node) = graph.block(id) {
        for child in child_ids(&node.block) {
            push_pre_order(graph, child, seen, order);
        }
    }
}

/// Stable address of one block: `section-N` for sections, `node-N` otherwise.
pub fn block_address(block: &Block, id: NodeId) -> String {
    match block {
        Block::Section { .. } => format!("section-{}", id.0),
        _ => format!("node-{}", id.0),
    }
}

/// Parses `node-N`, `section-N` or a bare `N` into a node id.
pub fn parse_block_address(raw: &str) -> Option<NodeId> {
    let trimmed = raw.trim();
    let digits = trimmed.strip_prefix("node-").or_else(|| trimmed.strip_prefix("section-")).unwrap_or(trimmed);
    digits.parse().ok().map(NodeId)
}

/// Flattens inline content into plain text.
pub fn plain_text(inlines: &[Inline]) -> String {
    inlines.iter().map(Inline::plain_text).collect()
}

/// Section entry of a [`DocumentSummary`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionSummary {
    /// Position among sections, in reading order.
    pub index: usize,
    /// Stable address such as `section-4`.
    pub id: String,
    /// Title text, `None` when the heading is empty.
    pub title: Option<String>,
}

/// Paragraph entry of a [`DocumentSummary`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParagraphSummary {
    /// Position among paragraphs, in reading order.
    pub index: usize,
    /// Index of the nearest enclosing section, or `-1` outside any section.
    pub section_index: i64,
    /// Flattened paragraph text.
    pub text: String,
}

/// Table entry of a [`DocumentSummary`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TableSummary {
    /// Position among tables, in reading order.
    pub index: usize,
    /// Index of the nearest enclosing section, or `-1` outside any section.
    pub section_index: i64,
    /// Row count across the head, body and foot bands.
    pub rows: usize,
    /// Widest row measured in spanned columns.
    pub columns: u32,
}

/// Structural summary of one committed graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentSummary {
    /// Document address, `doc-<id>`.
    pub id: String,
    /// Graph revision the summary describes.
    pub revision: u64,
    /// Document title, `None` when unset.
    pub title: Option<String>,
    /// Sections in reading order.
    pub sections: Vec<SectionSummary>,
    /// Paragraphs in reading order.
    pub paragraphs: Vec<ParagraphSummary>,
    /// Tables in reading order.
    pub tables: Vec<TableSummary>,
}

/// Projects one graph into its structural summary.
pub fn summarize(graph: &DocumentGraph) -> DocumentSummary {
    let mut walker = Walker { graph, sections: Vec::new(), paragraphs: Vec::new(), tables: Vec::new(), visited: HashSet::new() };
    for root in roots(graph) {
        walker.visit(root, None);
    }
    DocumentSummary {
        id: format!("doc-{}", graph.id.0),
        revision: graph.revision,
        title: graph.metadata.title.clone(),
        sections: walker.sections,
        paragraphs: walker.paragraphs,
        tables: walker.tables,
    }
}

struct Walker<'a> {
    graph: &'a DocumentGraph,
    sections: Vec<SectionSummary>,
    paragraphs: Vec<ParagraphSummary>,
    tables: Vec<TableSummary>,
    visited: HashSet<NodeId>,
}

impl Walker<'_> {
    fn visit(&mut self, id: NodeId, section: Option<usize>) {
        if !self.visited.insert(id) {
            return;
        }
        let Some(node) = self.graph.block(id)
        else {
            return;
        };
        match &node.block {
            Block::Section { title, children, .. } => {
                let index = self.sections.len();
                let text = plain_text(title);
                self.sections.push(SectionSummary {
                    index,
                    id: format!("section-{}", id.0),
                    title: if text.trim().is_empty() { None } else { Some(text) },
                });
                for child in children {
                    self.visit(*child, Some(index));
                }
            }
            Block::Paragraph { content } => {
                let index = self.paragraphs.len();
                self.paragraphs.push(ParagraphSummary { index, section_index: section.map_or(-1, |value| value as i64), text: plain_text(content) });
            }
            Block::Table { head, body, foot, .. } => {
                let rows = head.len() + body.len() + foot.len();
                let mut columns = 0u32;
                for row in head.iter().chain(body.iter()).chain(foot.iter()) {
                    columns = columns.max(row.cells.iter().fold(0u32, |total, cell| total.saturating_add(cell.colspan)));
                }
                let index = self.tables.len();
                self.tables.push(TableSummary { index, section_index: section.map_or(-1, |value| value as i64), rows, columns });
                for row in head.iter().chain(body.iter()).chain(foot.iter()) {
                    for cell in &row.cells {
                        for child in &cell.children {
                            self.visit(*child, section);
                        }
                    }
                }
            }
            Block::List { items, .. } => {
                for item in items {
                    for child in &item.children {
                        self.visit(*child, section);
                    }
                }
            }
            Block::Quote { children } => {
                for child in children {
                    self.visit(*child, section);
                }
            }
            _ => {}
        }
    }
}

/// Document statistics over one committed graph.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentStatistics {
    /// Graph revision the counts describe.
    pub revision: u64,
    /// Visited block count.
    pub blocks: usize,
    /// Paragraph blocks.
    pub paragraphs: usize,
    /// Heading count per section level.
    pub headings: BTreeMap<u8, usize>,
    /// Table blocks.
    pub tables: usize,
    /// Rows across every table.
    pub table_rows: usize,
    /// Whitespace-separated word count.
    pub words: usize,
    /// Character count including whitespace.
    pub characters: usize,
    /// Character count excluding whitespace.
    pub characters_without_whitespace: usize,
    /// Estimated reading time from [`WORDS_PER_MINUTE`].
    pub reading_minutes: usize,
}

/// Projects one graph into document statistics.
pub fn statistics(graph: &DocumentGraph) -> DocumentStatistics {
    let mut report = DocumentStatistics { revision: graph.revision, ..DocumentStatistics::default() };
    for id in flat_order(graph) {
        let Some(node) = graph.block(id)
        else {
            continue;
        };
        report.blocks += 1;
        match &node.block {
            Block::Section { level, title, .. } => {
                *report.headings.entry(*level).or_insert(0) += 1;
                report.push(&plain_text(title));
            }
            Block::Paragraph { content } => {
                report.paragraphs += 1;
                report.push(&plain_text(content));
            }
            Block::List { items, .. } => {
                for item in items {
                    report.push(&plain_text(&item.content));
                }
            }
            Block::Code { content, .. } => report.push(content),
            Block::Table { head, body, foot, .. } => {
                report.tables += 1;
                report.table_rows += head.len() + body.len() + foot.len();
            }
            _ => {}
        }
    }
    report.reading_minutes = report.words.div_ceil(WORDS_PER_MINUTE);
    report
}

impl DocumentStatistics {
    fn push(&mut self, text: &str) {
        self.words += text.split_whitespace().count();
        self.characters += text.chars().count();
        self.characters_without_whitespace += text.chars().filter(|character| !character.is_whitespace()).count();
    }
}

/// One full-text search hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    /// Stable address of the containing block.
    pub block_id: String,
    /// Depth-first index of the containing block.
    pub block_index: usize,
    /// Enclosing section address, `None` outside any section.
    pub path: Option<String>,
    /// Match start as a character offset inside the block text.
    pub start: usize,
    /// Match end as a character offset inside the block text.
    pub end: usize,
    /// Matched text.
    pub text: String,
}

/// Finds every occurrence of `query` in block text, up to `limit` hits.
///
/// Offsets are Unicode scalar counts inside one block's plain text, never byte
/// offsets. Block indices match [`summarize`] order.
pub fn find(graph: &DocumentGraph, query: &str, limit: usize) -> Vec<SearchHit> {
    if query.is_empty() || limit == 0 {
        return Vec::new();
    }
    let indices: HashMap<NodeId, usize> = flat_order(graph).into_iter().enumerate().map(|(index, id)| (id, index)).collect();
    let mut hits = Vec::new();
    let mut seen = HashSet::new();
    for root in roots(graph) {
        collect(graph, root, None, &indices, query, limit, &mut seen, &mut hits);
    }
    hits
}

#[allow(clippy::too_many_arguments)]
fn collect(
    graph: &DocumentGraph,
    id: NodeId,
    section: Option<NodeId>,
    indices: &HashMap<NodeId, usize>,
    query: &str,
    limit: usize,
    seen: &mut HashSet<NodeId>,
    hits: &mut Vec<SearchHit>,
) {
    if hits.len() >= limit || !seen.insert(id) {
        return;
    }
    let Some(node) = graph.block(id)
    else {
        return;
    };
    let Some(index) = indices.get(&id).copied()
    else {
        return;
    };
    let block = block_address(&node.block, id);
    let path = section.map(|value| format!("section-{}", value.0));
    match &node.block {
        Block::Section { title, children, .. } => {
            push_hits(&block, index, path.as_deref(), &plain_text(title), query, limit, hits);
            for child in children {
                collect(graph, *child, Some(id), indices, query, limit, seen, hits);
            }
        }
        Block::Paragraph { content } => {
            push_hits(&block, index, path.as_deref(), &plain_text(content), query, limit, hits);
        }
        Block::List { items, .. } => {
            for item in items {
                push_hits(&block, index, path.as_deref(), &plain_text(&item.content), query, limit, hits);
                for child in &item.children {
                    collect(graph, *child, section, indices, query, limit, seen, hits);
                }
            }
        }
        Block::Table { head, body, foot, .. } => {
            for row in head.iter().chain(body.iter()).chain(foot.iter()) {
                for cell in &row.cells {
                    for child in &cell.children {
                        collect(graph, *child, section, indices, query, limit, seen, hits);
                    }
                }
            }
        }
        Block::Quote { children } => {
            for child in children {
                collect(graph, *child, section, indices, query, limit, seen, hits);
            }
        }
        _ => {}
    }
}

fn push_hits(block: &str, index: usize, path: Option<&str>, text: &str, query: &str, limit: usize, hits: &mut Vec<SearchHit>) {
    for (byte, matched) in text.match_indices(query) {
        if hits.len() >= limit {
            break;
        }
        let start = text[..byte].chars().count();
        let end = start + matched.chars().count();
        hits.push(SearchHit {
            block_id: block.to_string(),
            block_index: index,
            path: path.map(str::to_string),
            start,
            end,
            text: matched.to_string(),
        });
    }
}

/// Severity of one reported loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LossSeverity {
    /// The construct was degraded but the rest survived.
    Warning,
    /// The construct could not be carried at all.
    Error,
}

/// One loss marker in report form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LossEntry {
    /// Stable loss code.
    pub code: String,
    /// Severity derived from the marker status.
    pub severity: LossSeverity,
    /// Human-readable reason.
    pub message: String,
}

/// Projects the recorded loss markers into report form.
pub fn losses(graph: &DocumentGraph) -> Vec<LossEntry> {
    graph
        .coverage
        .loss
        .iter()
        .map(|marker| LossEntry {
            code: marker.code.clone(),
            severity: if matches!(marker.status, SemanticStatus::Unsupported) { LossSeverity::Error } else { LossSeverity::Warning },
            message: marker.message.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::{Block, DocumentGraph, DocumentId, Inline};

    use super::{find, losses, statistics, summarize};

    fn text(value: &str) -> Vec<Inline> {
        vec![Inline::Text { text: value.to_string() }]
    }

    fn sample() -> DocumentGraph {
        let mut graph = DocumentGraph::new(DocumentId(7));
        let paragraph = graph.push_block(Block::Paragraph { content: text("alpha beta gamma") });
        let table_cell = graph.push_block(Block::Paragraph { content: text("cell text") });
        let table = graph.push_block(Block::Table {
            caption: None,
            head: Vec::new(),
            body: vec![crate::TableRow {
                cells: vec![crate::TableCell { children: vec![table_cell], colspan: 1, rowspan: 1 }],
            }],
            foot: Vec::new(),
        });
        graph.push_block(Block::Section { level: 2, title: text("Heading"), children: vec![paragraph, table] });
        graph
    }

    #[test]
    fn summary_walks_sections_before_children() {
        let summary = summarize(&sample());
        assert_eq!(summary.id, "doc-7");
        assert_eq!(summary.sections.len(), 1);
        assert_eq!(summary.sections[0].title.as_deref(), Some("Heading"));
        assert_eq!(summary.paragraphs.len(), 2);
        assert_eq!(summary.paragraphs[0].section_index, 0);
        assert_eq!(summary.tables.len(), 1);
        assert_eq!(summary.tables[0].columns, 1);
    }

    #[test]
    fn statistics_count_text_once() {
        let report = statistics(&sample());
        assert_eq!(report.blocks, 4);
        assert_eq!(report.paragraphs, 2);
        assert_eq!(report.tables, 1);
        assert_eq!(report.headings.get(&2), Some(&1));
        assert_eq!(report.words, 6);
        assert_eq!(report.characters_without_whitespace, 29);
        assert_eq!(report.reading_minutes, 1);
    }

    #[test]
    fn search_reports_character_offsets() {
        let hits = find(&sample(), "text", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].block_id, "node-2");
        assert_eq!(hits[0].start, 5);
        assert_eq!(hits[0].end, 9);
        assert_eq!(hits[0].path.as_deref(), Some("section-4"));
        assert_eq!(find(&sample(), "a", 2).len(), 2);
        assert_eq!(find(&sample(), "text", 0).len(), 0);
    }

    #[test]
    fn losses_map_status_to_severity() {
        let mut graph = sample();
        graph.push_loss(crate::LossMarker {
            code: "a".to_string(),
            message: "degraded".to_string(),
            status: crate::SemanticStatus::Partial,
        });
        graph.push_loss(crate::LossMarker {
            code: "b".to_string(),
            message: "refused".to_string(),
            status: crate::SemanticStatus::Unsupported,
        });
        let entries = losses(&graph);
        assert_eq!(entries[0].severity, super::LossSeverity::Warning);
        assert_eq!(entries[1].severity, super::LossSeverity::Error);
    }
}
