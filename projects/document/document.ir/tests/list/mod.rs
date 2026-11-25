//! 列表变换（indent / unindent / moveItemTo / sort / reverse）的集成测试。

use document_ir::{Block, DocumentGraph, DocumentId, Inline, ListItem, NodeId, SourceSpan, TransformError};

fn text(value: &str) -> Vec<Inline> {
    vec![Inline::Text { text: value.to_string() }]
}

fn span(start: usize, end: usize) -> Option<SourceSpan> {
    SourceSpan::new(start, end)
}

fn item(value: &str, children: Vec<NodeId>) -> ListItem {
    ListItem { content: text(value), children, ..Default::default() }
}

fn item_numbered(value: &str, number: u32) -> ListItem {
    ListItem { content: text(value), number: Some(number), ..Default::default() }
}

/// 顶层只有一个无序列表（Alpha / Beta / Gamma），返回 (graph, list)。
fn bullet_list() -> (DocumentGraph, NodeId) {
    let mut g = DocumentGraph::new(DocumentId(1));
    let list = g.push_block_at(
        Block::List { ordered: false, items: vec![item("Alpha", vec![]), item("Beta", vec![]), item("Gamma", vec![])] },
        span(0, 20),
    );
    (g, list)
}

/// 顶层只有一个有序列表（1 One / 2 Two / 3 Three），返回 (graph, list)。
fn ordered_list() -> (DocumentGraph, NodeId) {
    let mut g = DocumentGraph::new(DocumentId(1));
    let list = g.push_block_at(
        Block::List { ordered: true, items: vec![item_numbered("One", 1), item_numbered("Two", 2), item_numbered("Three", 3)] },
        span(0, 24),
    );
    (g, list)
}

/// 读取无序列表条目的 (文本, children) 形状，便于断言嵌套结构。
fn shape(g: &DocumentGraph, list: NodeId) -> Vec<(String, Vec<NodeId>)> {
    let Block::List { items, .. } = &g.block(list).expect("the list exists").block
    else {
        panic!("expected a list")
    };
    items.iter().map(|entry| (document_ir::plain_text(&entry.content), entry.children.clone())).collect()
}

/// 读取有序列表条目的 (文本, 编号) 序列。
fn numbers(g: &DocumentGraph, list: NodeId) -> Vec<(String, Option<u32>)> {
    let Block::List { items, .. } = &g.block(list).expect("the list exists").block
    else {
        panic!("expected a list")
    };
    items.iter().map(|entry| (document_ir::plain_text(&entry.content), entry.number)).collect()
}

fn nested_list(g: &DocumentGraph, child: NodeId) -> &Vec<ListItem> {
    let Block::List { items, .. } = &g.block(child).expect("the nested list exists").block
    else {
        panic!("expected a nested list")
    };
    items
}

// ===========================================================================
// indent
// ===========================================================================

#[test]
fn indent_creates_a_nested_run_and_a_second_indent_joins_it() {
    let (mut g, list) = bullet_list();
    let before = g.revision;

    g.indent_list_item(list, 1).expect("nest Beta under Alpha");
    let (_, host_children) = shape(&g, list).into_iter().next().expect("Alpha still first");
    assert_eq!(host_children.len(), 1, "one nested run under Alpha");
    let nested = host_children[0];
    assert_eq!(nested_list(&g, nested).len(), 1);

    // 第二次缩进（此时 Gamma 位于 index 1）并入同一条嵌套 run。
    g.indent_list_item(list, 1).expect("nest Gamma under Alpha");
    let (_, host_children) = shape(&g, list).into_iter().next().expect("Alpha still first");
    assert_eq!(host_children, vec![nested], "still exactly one nested run");
    assert_eq!(nested_list(&g, nested).len(), 2);

    assert!(g.validate().is_valid(), "no dangling references");
    assert!(g.revision > before);
}

#[test]
fn indent_renumbers_both_levels_of_an_ordered_list() {
    let (mut g, list) = ordered_list();
    g.indent_list_item(list, 1).expect("nest Two under One");

    // 父 run 从首个编号起连续：One=1, Three=2。
    assert_eq!(numbers(&g, list), vec![("One".into(), Some(1)), ("Three".into(), Some(2))]);
    // 嵌套 run 从 1 重启。
    let nested = shape(&g, list)[0].1[0];
    assert_eq!(numbers(&g, nested), vec![("Two".into(), Some(1))]);
    assert!(g.validate().is_valid());
}

#[test]
fn indent_keeps_the_item_task_marker_and_children() {
    let mut g = DocumentGraph::new(DocumentId(1));
    let body = g.push_block(Block::Paragraph { content: text("nested body") });
    let task = ListItem { content: text("Beta"), children: vec![body], is_task: true, is_checked: Some(true), ..Default::default() };
    let list = g.push_block(Block::List { ordered: false, items: vec![item("Alpha", vec![]), task] });

    g.indent_list_item(list, 1).expect("nest the task item");
    let nested = shape(&g, list)[0].1[0];
    let entries = nested_list(&g, nested);
    assert!(entries[0].is_task && entries[0].is_checked == Some(true), "task marker travels with the item");
    assert_eq!(entries[0].children, vec![body], "the item keeps its nested block");
    assert!(g.validate().is_valid());
}

#[test]
fn indent_refuses_the_first_item_and_out_of_range_without_mutating() {
    let (mut g, list) = bullet_list();
    let paragraph = g.push_block(Block::Paragraph { content: text("not a list") });
    let before = g.revision;
    let blocks = g.blocks.clone();

    assert_eq!(g.indent_list_item(list, 0), Err(TransformError::NoPreviousSibling(list)));
    assert_eq!(g.indent_list_item(list, 9), Err(TransformError::ItemOutOfRange { list, item: 9, items: 3 }));
    let missing = NodeId(999);
    assert_eq!(g.indent_list_item(missing, 1), Err(TransformError::NotFound(missing)));
    assert_eq!(g.indent_list_item(paragraph, 1), Err(TransformError::NotAList(paragraph)));

    assert_eq!(g.revision, before, "failed indents consume no revision");
    assert_eq!(g.blocks, blocks, "failed indents change nothing");
}

// ===========================================================================
// unindent
// ===========================================================================

#[test]
fn unindent_lifts_items_and_sweeps_the_empty_nested_block() {
    let (mut g, list) = bullet_list();
    g.indent_list_item(list, 1).expect("nest Beta");
    g.indent_list_item(list, 1).expect("nest Gamma");
    let nested = shape(&g, list)[0].1[0];
    let before = g.blocks.len();

    g.unindent_list_item(list, 0).expect("lift the nested run back");

    assert_eq!(shape(&g, list).into_iter().map(|(value, _)| value).collect::<Vec<_>>(), vec!["Alpha", "Beta", "Gamma"]);
    assert_eq!(g.blocks.len(), before - 1, "the emptied nested list block is physically removed");
    assert!(g.block(nested).is_none());
    assert!(g.validate().is_valid(), "no dangling references after the sweep");
}

#[test]
fn unindent_renumbers_an_ordered_run_and_keeps_lifted_children() {
    let (mut g, list) = ordered_list();
    g.indent_list_item(list, 1).expect("nest Two under One");
    let nested = shape(&g, list)[0].1[0];
    // 给被嵌套条目再挂一层子块，提升后必须随条目保留。
    let body = g.push_block(Block::Paragraph { content: text("Two body") });
    if let Block::List { items, .. } = &mut g.blocks.iter_mut().find(|node| node.id == nested).expect("nested list").block {
        items[0].children.push(body);
    }

    g.unindent_list_item(list, 0).expect("lift Two back");

    assert_eq!(numbers(&g, list), vec![("One".into(), Some(1)), ("Two".into(), Some(2)), ("Three".into(), Some(3))]);
    let lifted_children = shape(&g, list)[1].1.clone();
    assert_eq!(lifted_children, vec![body], "the lifted item keeps its own nested block");
    assert!(g.block(body).is_some(), "the nested block survives the sweep of the old list block");
    assert!(g.validate().is_valid());
}

#[test]
fn unindent_refuses_items_without_a_matching_nested_list() {
    let (mut g, list) = bullet_list();
    let before = g.revision;
    let blocks = g.blocks.clone();

    assert_eq!(g.unindent_list_item(list, 0), Err(TransformError::NothingToLift { list, item: 0 }));
    assert_eq!(g.unindent_list_item(list, 7), Err(TransformError::ItemOutOfRange { list, item: 7, items: 3 }));
    assert_eq!(g.revision, before, "failed lifts consume no revision");
    assert_eq!(g.blocks, blocks, "failed lifts change nothing");

    // 嵌套列表标记样式不同时同样拒绝。
    g.indent_list_item(list, 1).expect("nest Beta");
    let nested = shape(&g, list)[0].1[0];
    if let Block::List { ordered, .. } = &mut g.blocks.iter_mut().find(|node| node.id == nested).expect("nested list").block {
        *ordered = true;
    }
    let before = g.revision;
    let blocks = g.blocks.clone();
    assert_eq!(g.unindent_list_item(list, 0), Err(TransformError::NothingToLift { list, item: 0 }));
    // 嵌套 Beta 后父 run 只剩 Alpha / Gamma 两个条目。
    assert_eq!(g.unindent_list_item(list, 7), Err(TransformError::ItemOutOfRange { list, item: 7, items: 2 }));

    assert_eq!(g.revision, before, "failed lifts consume no revision");
    assert_eq!(g.blocks, blocks, "failed lifts change nothing");
}

// ===========================================================================
// moveItemTo
// ===========================================================================

/// 两个有序列表：One/Two/Three 与 Four/Five，Two 已嵌套在 One 下。
fn two_ordered_lists() -> (DocumentGraph, NodeId, NodeId) {
    let mut g = DocumentGraph::new(DocumentId(1));
    let first =
        g.push_block(Block::List { ordered: true, items: vec![item_numbered("One", 1), item_numbered("Two", 2), item_numbered("Three", 3)] });
    let second = g.push_block(Block::List { ordered: true, items: vec![item_numbered("Four", 1), item_numbered("Five", 2)] });
    g.indent_list_item(first, 1).expect("nest Two under One");
    (g, first, second)
}

#[test]
fn move_item_to_another_list_renumbers_both_runs() {
    let (mut g, first, second) = two_ordered_lists();

    // 源 run 现在是 One(1, 嵌套 Two) / Three(2)；把 Three 移到第二个列表 index 1。
    g.move_list_item_to(first, 1, second, 1).expect("move Three into the second list");

    assert_eq!(numbers(&g, first), vec![("One".into(), Some(1))], "the source keeps its nested run");
    let nested = shape(&g, first)[0].1[0];
    assert_eq!(numbers(&g, nested), vec![("Two".into(), Some(1))]);
    assert_eq!(
        numbers(&g, second),
        vec![("Four".into(), Some(1)), ("Three".into(), Some(2)), ("Five".into(), Some(3))],
        "the target renumbers around the moved item"
    );
    assert!(g.validate().is_valid());
}

#[test]
fn move_item_to_the_same_list_reorders_it() {
    let (mut g, list) = ordered_list();
    g.move_list_item_to(list, 2, list, 0).expect("move Three to the front");
    assert_eq!(numbers(&g, list), vec![("Three".into(), Some(1)), ("One".into(), Some(2)), ("Two".into(), Some(3))]);
    assert!(g.validate().is_valid());
}

#[test]
fn move_item_to_refuses_cycles_and_out_of_range() {
    let (mut g, first, second) = two_ordered_lists();
    let nested = shape(&g, first)[0].1[0];
    let before = g.revision;
    let blocks = g.blocks.clone();

    // 把 One（其子树包含嵌套列表）移进它自己的嵌套列表 —— 环，拒绝。
    assert_eq!(g.move_list_item_to(first, 0, nested, 0), Err(TransformError::MoveIntoDescendant { list: first, item: 0, target: nested }));
    assert_eq!(g.move_list_item_to(first, 9, second, 0), Err(TransformError::ItemOutOfRange { list: first, item: 9, items: 2 }));
    assert_eq!(g.move_list_item_to(first, 0, second, 9), Err(TransformError::IndexOutOfRange { list: second, index: 9, positions: 3 }));
    let missing = NodeId(999);
    assert_eq!(g.move_list_item_to(first, 0, missing, 0), Err(TransformError::NotFound(missing)));

    assert_eq!(g.revision, before, "failed moves consume no revision");
    assert_eq!(g.blocks, blocks, "failed moves change nothing");
}

#[test]
fn list_ops_keep_block_spans_aligned() {
    let (mut g, list) = bullet_list();
    assert_eq!(g.block_span(list), span(0, 20), "the authored span survives");
    g.indent_list_item(list, 1).expect("nest Beta");
    assert_eq!(g.block_span(list), span(0, 20), "indenting keeps the list span");
    let nested = shape(&g, list)[0].1[0];
    assert_eq!(g.block_span(nested), None, "a fresh nested run invents no span");
    g.unindent_list_item(list, 0).expect("lift back");
    assert_eq!(g.block_span(list), span(0, 20));
    assert!(g.validate().is_valid());
}

// ===========================================================================
// sortItems / reverseItems
// ===========================================================================

/// 无序列表（Gamma / alpha / 2 / 10 / 空），返回 (graph, list)。
fn mixed_list() -> (DocumentGraph, NodeId) {
    let mut g = DocumentGraph::new(DocumentId(1));
    let list = g.push_block_at(
        Block::List {
            ordered: false,
            items: vec![item("Gamma", vec![]), item("alpha", vec![]), item("2", vec![]), item("10", vec![]), item("  ", vec![])],
        },
        span(0, 30),
    );
    (g, list)
}

fn texts(g: &DocumentGraph, list: NodeId) -> Vec<String> {
    shape(g, list).into_iter().map(|(value, _)| value).collect()
}

#[test]
fn sort_orders_keys_numerically_then_case_insensitively_with_blanks_last() {
    let (mut g, list) = mixed_list();
    let before = g.revision;

    let count = g.sort_list_items(list, true).expect("sort ascending");
    assert_eq!(count, 5);
    assert_eq!(texts(&g, list), vec!["2", "10", "alpha", "Gamma", "  "], "numbers compare numerically, blanks trail");

    g.sort_list_items(list, false).expect("sort descending");
    assert_eq!(texts(&g, list), vec!["Gamma", "alpha", "10", "2", "  "], "descending keeps blanks last");
    assert!(g.revision > before);
    assert!(g.validate().is_valid());
}

#[test]
fn sort_travels_with_nested_children_and_renumbers_ordered_runs() {
    let (mut g, list) = ordered_list();
    g.indent_list_item(list, 1).expect("nest Two under One");
    let nested = shape(&g, list)[0].1[0];

    // 降序把带嵌套 run 的 One 翻到末尾，编号从首个条目（Three 的 1）起连续。
    g.sort_list_items(list, false).expect("sort the ordered run");
    assert_eq!(numbers(&g, list), vec![("Three".into(), Some(1)), ("One".into(), Some(2))]);
    assert_eq!(shape(&g, list)[1].1, vec![nested], "the nested run travels with its item");
    assert_eq!(g.block_span(list), span(0, 24), "sorting keeps the list span");
    assert!(g.validate().is_valid());
}

#[test]
fn sort_refuses_unknown_and_non_list_without_mutating() {
    let (mut g, list) = mixed_list();
    let paragraph = g.push_block(Block::Paragraph { content: text("not a list") });
    let before = g.revision;
    let blocks = g.blocks.clone();

    let missing = NodeId(999);
    assert_eq!(g.sort_list_items(missing, true), Err(TransformError::NotFound(missing)));
    assert_eq!(g.sort_list_items(paragraph, true), Err(TransformError::NotAList(paragraph)));

    assert_eq!(g.revision, before, "failed sorts consume no revision");
    assert_eq!(g.blocks, blocks, "failed sorts change nothing");
    assert_eq!(texts(&g, list), vec!["Gamma", "alpha", "2", "10", "  "]);
}

#[test]
fn reverse_flips_items_and_renumbers_from_the_leading_number() {
    let mut g = DocumentGraph::new(DocumentId(1));
    let list = g.push_block_at(
        Block::List {
            ordered: true,
            items: vec![item_numbered("a", 3), item_numbered("b", 4), item_numbered("c", 5)],
        },
        span(0, 20),
    );
    let before = g.revision;

    let count = g.reverse_list_items(list).expect("reverse");
    assert_eq!(count, 3);
    assert_eq!(numbers(&g, list), vec![("c".into(), Some(3)), ("b".into(), Some(4)), ("a".into(), Some(5))]);
    assert!(g.revision > before);
    assert!(g.validate().is_valid());
}

#[test]
fn reverse_keeps_nested_runs_and_task_markers() {
    let (mut g, list) = bullet_list();
    g.indent_list_item(list, 1).expect("nest Beta under Alpha");
    let nested = shape(&g, list)[0].1[0];
    let task = ListItem { content: text("Gamma"), is_task: true, is_checked: Some(true), ..Default::default() };
    if let Block::List { items, .. } = &mut g.blocks.iter_mut().find(|node| node.id == list).expect("the list").block {
        items[1] = task;
    }

    g.reverse_list_items(list).expect("reverse");
    let flipped = shape(&g, list);
    assert_eq!(flipped.iter().map(|(value, _)| value.as_str()).collect::<Vec<_>>(), vec!["Gamma", "Alpha"]);
    assert_eq!(flipped[1].1, vec![nested], "the nested run travels with Alpha");
    let Block::List { items, .. } = &g.block(list).expect("the list").block
    else {
        panic!("expected a list")
    };
    assert!(items[0].is_task && items[0].is_checked == Some(true), "the task marker survives the flip");
    assert!(g.validate().is_valid());
}

#[test]
fn reverse_refuses_unknown_and_non_list_without_mutating() {
    let (mut g, _list) = bullet_list();
    let paragraph = g.push_block(Block::Paragraph { content: text("not a list") });
    let before = g.revision;
    let blocks = g.blocks.clone();

    let missing = NodeId(999);
    assert_eq!(g.reverse_list_items(missing), Err(TransformError::NotFound(missing)));
    assert_eq!(g.reverse_list_items(paragraph), Err(TransformError::NotAList(paragraph)));

    assert_eq!(g.revision, before, "failed reversals consume no revision");
    assert_eq!(g.blocks, blocks, "failed reversals change nothing");
}
