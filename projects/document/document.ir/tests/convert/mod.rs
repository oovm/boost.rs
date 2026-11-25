//! 块类型互转（block.convert）的集成测试。

use document_ir::{Block, BlockKind, DocumentGraph, DocumentId, Inline, ListItem, NodeId, SourceSpan, TransformError};

fn text(value: &str) -> Vec<Inline> {
    vec![Inline::Text { text: value.to_string() }]
}

fn span(start: usize, end: usize) -> Option<SourceSpan> {
    SourceSpan::new(start, end)
}

/// 两个带来源跨度的段落：Lead / Tail。
fn two_paragraphs() -> (DocumentGraph, NodeId, NodeId) {
    let mut g = DocumentGraph::new(DocumentId(1));
    let lead = g.push_block_at(Block::Paragraph { content: text("Lead") }, span(0, 4));
    let tail = g.push_block_at(Block::Paragraph { content: text("Tail") }, span(5, 9));
    (g, lead, tail)
}

// ===========================================================================
// 段落 ↔ 标题
// ===========================================================================

#[test]
fn paragraph_to_heading_keeps_id_position_and_span() {
    let (mut g, lead, tail) = two_paragraphs();
    let from = g.convert_block(lead, BlockKind::Heading, Some(2), None, None).expect("convert to heading");
    assert_eq!(from, BlockKind::Paragraph);
    let node = g.block(lead).expect("the id survives");
    let Block::Section { level, title, children } = &node.block
    else {
        panic!("expected a section")
    };
    assert_eq!(*level, 2);
    assert_eq!(document_ir::plain_text(title), "Lead");
    assert!(children.is_empty());
    assert_eq!(g.blocks[0].id, lead, "the position survives");
    assert_eq!(g.block_span(lead), span(0, 4), "the authored span survives");
    assert_eq!(g.blocks[1].id, tail);
    assert!(g.validate().is_valid());
}

#[test]
fn heading_to_paragraph_promotes_children_right_after_it() {
    let mut g = DocumentGraph::new(DocumentId(1));
    let body = g.push_block(Block::Paragraph { content: text("Body") });
    let deep = g.push_block(Block::Paragraph { content: text("Deep") });
    let inner = g.push_block(Block::Section { level: 3, title: text("Inner"), children: vec![deep] });
    let head = g.push_block_at(Block::Section { level: 1, title: text("Head"), children: vec![body, inner] }, span(0, 4));
    let tail = g.push_block(Block::Paragraph { content: text("Tail") });

    let from = g.convert_block(head, BlockKind::Paragraph, None, None, None).expect("flatten the heading");
    assert_eq!(from, BlockKind::Heading);
    assert_eq!(g.block(head).expect("id survives").block, Block::Paragraph { content: text("Head") });
    // 阅读顺序：Head(段落) → Body → Inner(→Deep) → Tail。
    assert_eq!(document_ir::flat_order(&g), vec![head, body, inner, deep, tail]);
    assert!(g.validate().is_valid());
    assert_eq!(g.block_span(head), span(0, 4));
}

#[test]
fn heading_conversion_requires_a_valid_level() {
    let (mut g, lead, _) = two_paragraphs();
    let before = g.revision;
    assert_eq!(g.convert_block(lead, BlockKind::Heading, None, None, None), Err(TransformError::MissingHeadingLevel(lead)));
    assert_eq!(g.convert_block(lead, BlockKind::Heading, Some(7), None, None), Err(TransformError::MissingHeadingLevel(lead)));
    assert_eq!(g.revision, before);
    assert!(matches!(g.block(lead).expect("still a paragraph").block, Block::Paragraph { .. }));
}

// ===========================================================================
// 段落 ↔ 引用
// ===========================================================================

#[test]
fn paragraph_to_quote_and_back_roundtrips_the_content() {
    let (mut g, lead, tail) = two_paragraphs();
    g.convert_block(lead, BlockKind::Quote, None, None, None).expect("to quote");
    let Block::Quote { children } = &g.block(lead).expect("id survives").block
    else {
        panic!("expected a quote")
    };
    assert_eq!(children.len(), 1);
    let child = children[0];
    assert_eq!(g.block(child).expect("child exists").block, Block::Paragraph { content: text("Lead") });
    assert_eq!(g.blocks[1].id, tail, "the quote stays in position");

    g.convert_block(lead, BlockKind::Paragraph, None, None, None).expect("back to paragraph");
    assert_eq!(g.block(lead).expect("id survives").block, Block::Paragraph { content: text("Lead") });
    assert_eq!(g.block(child), None, "the reused child block is removed");
    assert!(g.validate().is_valid());
}

#[test]
fn quote_with_several_children_flattens_and_sweeps() {
    let mut g = DocumentGraph::new(DocumentId(1));
    let first = g.push_block(Block::Paragraph { content: text("First") });
    let code = g.push_block(Block::Code { language: Some("rust".into()), content: "let x = 1;".into() });
    let quote = g.push_block_at(Block::Quote { children: vec![first, code] }, span(0, 30));

    g.convert_block(quote, BlockKind::Paragraph, None, None, None).expect("flatten the quote");
    assert_eq!(g.block(quote).expect("id survives").block, Block::Paragraph { content: text("First let x = 1;") });
    assert_eq!(g.block(first), None, "the old children are swept with their subtree");
    assert_eq!(g.block(code), None);
    assert!(g.validate().is_valid());
}

// ===========================================================================
// 段落 ↔ 代码
// ===========================================================================

#[test]
fn paragraph_to_code_flattens_text_and_code_back_keeps_it() {
    let (mut g, lead, _) = two_paragraphs();
    let from = g.convert_block(lead, BlockKind::Code, None, Some("rust"), None).expect("to code");
    assert_eq!(from, BlockKind::Paragraph);
    assert_eq!(g.block(lead).expect("id survives").block, Block::Code { language: Some("rust".into()), content: "Lead".into() });

    g.convert_block(lead, BlockKind::Paragraph, None, None, None).expect("back to paragraph");
    assert_eq!(g.block(lead).expect("id survives").block, Block::Paragraph { content: text("Lead") });
    assert!(g.validate().is_valid());
}

// ===========================================================================
// 段落 ↔ 列表
// ===========================================================================

#[test]
fn paragraph_to_list_makes_one_item_and_ordered_starts_at_one() {
    let (mut g, lead, _) = two_paragraphs();
    g.convert_block(lead, BlockKind::List, None, None, Some(true)).expect("to an ordered list");
    let Block::List { ordered, items } = &g.block(lead).expect("id survives").block
    else {
        panic!("expected a list")
    };
    assert!(ordered);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].number, Some(1));
    assert_eq!(document_ir::plain_text(&items[0].content), "Lead");
}

#[test]
fn list_to_paragraph_splits_items_and_moves_nested_children() {
    let mut g = DocumentGraph::new(DocumentId(1));
    let body = g.push_block(Block::Paragraph { content: text("Alpha body") });
    let list = g.push_block_at(
        Block::List {
            ordered: false,
            items: vec![
                ListItem { content: text("First"), children: vec![body], ..Default::default() },
                ListItem { content: text("Second"), children: Vec::new(), ..Default::default() },
            ],
        },
        span(0, 20),
    );
    let tail = g.push_block(Block::Paragraph { content: text("Tail") });

    let from = g.convert_block(list, BlockKind::Paragraph, None, None, None).expect("split the list");
    assert_eq!(from, BlockKind::List);
    assert_eq!(g.block(list).expect("id survives").block, Block::Paragraph { content: text("First") });
    // 阅读顺序：First → Alpha body → Second → Tail。
    let order = document_ir::flat_order(&g);
    assert_eq!(order.len(), 4);
    assert_eq!(order[0], list);
    assert_eq!(order[1], body);
    assert_eq!(para_text(&g, order[2]), "Second");
    assert_eq!(order[3], tail);
    assert_eq!(g.block_span(list), span(0, 20), "the list keeps its authored span");
    assert!(g.validate().is_valid());
}

fn para_text(g: &DocumentGraph, id: NodeId) -> String {
    let Block::Paragraph { content } = &g.block(id).expect("the paragraph exists").block
    else {
        panic!("expected a paragraph")
    };
    document_ir::plain_text(content)
}

#[test]
fn empty_list_to_paragraph_becomes_an_empty_paragraph() {
    let mut g = DocumentGraph::new(DocumentId(1));
    let list = g.push_block(Block::List { ordered: false, items: Vec::new() });
    g.convert_block(list, BlockKind::Paragraph, None, None, None).expect("empty list flattens");
    assert_eq!(g.block(list).expect("id survives").block, Block::Paragraph { content: Vec::new() });
}

// ===========================================================================
// 拒绝路径
// ===========================================================================

#[test]
fn convert_refuses_same_kind_tables_and_unsupported_pairs() {
    let (mut g, lead, _) = two_paragraphs();
    let before = g.revision;
    let blocks = g.blocks.clone();

    assert_eq!(
        g.convert_block(lead, BlockKind::Paragraph, None, None, None),
        Err(TransformError::AlreadyKind { node: lead, kind: BlockKind::Paragraph })
    );
    let table = g.push_block(Block::Table { caption: None, head: Vec::new(), body: Vec::new(), foot: Vec::new() });
    assert_eq!(g.convert_block(table, BlockKind::Paragraph, None, None, None), Err(TransformError::NotConvertible(table)));
    // 标题 → 代码不是受支持的转换对。
    let head = g.push_block(Block::Section { level: 1, title: text("Head"), children: Vec::new() });
    assert_eq!(
        g.convert_block(head, BlockKind::Code, None, None, None),
        Err(TransformError::InvalidConversion { from: BlockKind::Heading, to: BlockKind::Code })
    );
    let missing = NodeId(999);
    assert_eq!(g.convert_block(missing, BlockKind::Heading, Some(1), None, None), Err(TransformError::NotFound(missing)));

    assert_eq!(g.revision, before, "failed conversions consume no revision");
    // 只比较失败时涉及的原始段落，后面的新块是测试自己加的。
    assert_eq!(g.block(lead).expect("lead untouched").block, blocks[0].block);
}
