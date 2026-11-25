//! 子树深复制 + 表格跨度占位/插入 + 事务回滚的集成测试。

use document_ir::{
    Block, DocumentGraph, DocumentId, GraphTransaction, Inline, ListItem, NodeId, SourceSpan, SubtreeClone, TableCell, TableRow,
    TransformError,
};

fn text(value: &str) -> Vec<Inline> {
    vec![Inline::Text { text: value.to_string() }]
}

fn span(start: usize, end: usize) -> Option<SourceSpan> {
    SourceSpan::new(start, end)
}

fn cell(children: Vec<NodeId>) -> TableCell {
    TableCell { children, colspan: 1, rowspan: 1 }
}

fn cell_span(children: Vec<NodeId>, colspan: u32, rowspan: u32) -> TableCell {
    TableCell { children, colspan, rowspan }
}

/// 典型树：Section 根 → 段落 + 嵌套 Section → 段落 + 列表 → 条目 → 段落。
fn build_tree() -> (DocumentGraph, Vec<NodeId>) {
    let mut g = DocumentGraph::new(DocumentId(1));
    let p_hello = g.push_block_at(Block::Paragraph { content: text("Hello") }, span(0, 5));
    let p_nested = g.push_block_at(Block::Paragraph { content: text("Nested body") }, span(6, 17));
    let sec_nested = g.push_block_at(Block::Section { level: 2, title: text("Nested"), children: vec![p_nested] }, span(18, 30));
    let p_item = g.push_block_at(Block::Paragraph { content: text("Item body") }, span(31, 40));
    let list = g.push_block_at(
        Block::List { ordered: false, items: vec![ListItem { content: text("item"), children: vec![p_item], ..Default::default() }] },
        span(41, 52),
    );
    let sec_root = g.push_block_at(Block::Section { level: 1, title: text("Root"), children: vec![p_hello, sec_nested, list] }, span(53, 60));
    (g, vec![sec_root, p_hello, sec_nested, p_nested, list, p_item])
}

fn all_ids(clone: &SubtreeClone) -> Vec<NodeId> {
    clone.mapping.values().copied().collect()
}

// ===========================================================================
// clone_subtree
// ===========================================================================

#[test]
fn clone_subtree_maps_every_node_and_keeps_structure() {
    let (mut g, ids) = build_tree();
    let (sec_root, p_hello, sec_nested, p_nested, list, p_item) = (ids[0], ids[1], ids[2], ids[3], ids[4], ids[5]);

    let clone = g.clone_subtree(&[sec_root]).unwrap();

    // 映射表覆盖全部 6 个源节点，且每个副本都是全新 id 并真实存在。
    assert_eq!(clone.mapping.len(), 6);
    assert_eq!(clone.roots, vec![clone.mapping[&sec_root]]);
    for (&source, &copy) in &clone.mapping {
        assert_ne!(source, copy);
        assert!(g.block(copy).is_some(), "copy of {source:?} must exist");
    }

    // 父子结构通过映射表重建。
    let Block::Section { children: copy_children, .. } = &g.block(clone.roots[0]).unwrap().block else { panic!("expected section") };
    assert_eq!(copy_children, &vec![clone.mapping[&p_hello], clone.mapping[&sec_nested], clone.mapping[&list]]);
    let Block::Section { children: nested_children, .. } = &g.block(clone.mapping[&sec_nested]).unwrap().block else { panic!("expected section") };
    assert_eq!(nested_children, &vec![clone.mapping[&p_nested]]);
    let Block::List { items, .. } = &g.block(clone.mapping[&list]).unwrap().block else { panic!("expected list") };
    assert_eq!(items[0].children, vec![clone.mapping[&p_item]]);

    // 复制后无悬空引用。
    assert!(g.validate().is_valid());
}

/// 把源 block 的容器引用按映射表重定位，用于与副本 payload 比较。
fn mapped(block: &Block, mapping: &std::collections::BTreeMap<NodeId, NodeId>) -> Block {
    let map = |ids: &[NodeId]| ids.iter().filter_map(|id| mapping.get(id).copied()).collect::<Vec<_>>();
    match block {
        Block::Section { level, title, children } => Block::Section { level: *level, title: title.clone(), children: map(children) },
        Block::Quote { children } => Block::Quote { children: map(children) },
        Block::List { ordered, items } => Block::List {
            ordered: *ordered,
            items: items.iter().map(|item| ListItem { children: map(&item.children), ..item.clone() }).collect(),
        },
        Block::Table { caption, head, body, foot } => Block::Table {
            caption: caption.clone(),
            head: map_row(head, &map),
            body: map_row(body, &map),
            foot: map_row(foot, &map),
        },
        other => other.clone(),
    }
}

fn map_row(rows: &[TableRow], map: &dyn Fn(&[NodeId]) -> Vec<NodeId>) -> Vec<TableRow> {
    rows.iter().map(|row| TableRow { cells: row.cells.iter().map(|c| TableCell { children: map(&c.children), ..c.clone() }).collect() }).collect()
}

#[test]
fn clone_subtree_preserves_source_spans_and_bodies() {
    let (mut g, ids) = build_tree();
    let clone = g.clone_subtree(&[ids[0]]).unwrap();

    for (&source, &copy) in &clone.mapping {
        assert_eq!(g.block_span(copy), g.block_span(source), "span of copy {copy:?} must match source {source:?}");
        assert!(g.block_span(copy).is_some(), "authored spans must survive the copy");
        let expected = mapped(&g.block(source).unwrap().block, &clone.mapping);
        assert_eq!(g.block(copy).unwrap().block, expected, "payload must survive except for remapped ids");
    }
}

#[test]
fn clone_subtree_copies_nested_table_paragraph_and_list() {
    let mut g = DocumentGraph::new(DocumentId(2));
    let cell_para = g.push_block_at(Block::Paragraph { content: text("cell text") }, span(0, 9));
    let list_body = g.push_block_at(Block::Paragraph { content: text("list body") }, span(10, 19));
    let cell_list = g.push_block_at(
        Block::List { ordered: true, items: vec![ListItem { content: text("one"), children: vec![list_body], number: Some(1), ..Default::default() }] },
        span(20, 29),
    );
    let cell_tail = g.push_block_at(Block::Paragraph { content: text("tail") }, span(25, 29));
    let table = g.push_block_at(
        Block::Table {
            caption: Some("表 1".into()),
            head: vec![TableRow { cells: vec![cell_span(vec![cell_para], 2, 1)] }],
            body: vec![TableRow { cells: vec![cell(vec![cell_list]), cell(vec![cell_tail])] }],
            foot: Vec::new(),
        },
        span(30, 99),
    );
    let sec = g.push_block_at(Block::Section { level: 1, title: text("T"), children: vec![table] }, span(100, 120));

    let clone = g.clone_subtree(&[sec]).unwrap();
    assert_eq!(clone.mapping.len(), 6);

    let copy_table = clone.mapping[&table];
    let Block::Table { caption, head, body, .. } = &g.block(copy_table).unwrap().block else { panic!("expected table") };
    assert_eq!(caption.as_deref(), Some("表 1"));
    assert_eq!(head[0].cells[0].children, vec![clone.mapping[&cell_para]]);
    assert_eq!(head[0].cells[0].colspan, 2, "table spans must survive the copy");
    assert_eq!(body[0].cells[0].children, vec![clone.mapping[&cell_list]]);
    assert_eq!(body[0].cells[1].children, vec![clone.mapping[&cell_tail]]);
    let Block::List { items, .. } = &g.block(clone.mapping[&cell_list]).unwrap().block else { panic!("expected list") };
    assert_eq!(items[0].children, vec![clone.mapping[&list_body]]);
    assert_eq!(g.block_span(copy_table), span(30, 99));
    assert!(g.validate().is_valid());
}

#[test]
fn clone_subtree_attaches_copy_after_source_in_container() {
    let mut g = DocumentGraph::new(DocumentId(3));
    let a = g.push_block(Block::Paragraph { content: text("a") });
    let b = g.push_block(Block::Paragraph { content: text("b") });
    let sec = g.push_block(Block::Section { level: 1, title: text("s"), children: vec![a, b] });

    let clone = g.clone_subtree(&[a]).unwrap();
    let copy_a = clone.roots[0];
    let Block::Section { children, .. } = &g.block(sec).unwrap().block else { panic!("expected section") };
    assert_eq!(children, &vec![a, copy_a, b]);
}

#[test]
fn clone_subtree_attaches_top_level_copy_right_after_source() {
    let mut g = DocumentGraph::new(DocumentId(4));
    let p1 = g.push_block(Block::Paragraph { content: text("1") });
    let p2 = g.push_block(Block::Paragraph { content: text("2") });
    let p3 = g.push_block(Block::Paragraph { content: text("3") });

    let clone = g.clone_subtree(&[p2]).unwrap();
    let copy_p2 = clone.roots[0];
    let order: Vec<NodeId> = g.blocks.iter().map(|node| node.id).collect();
    assert_eq!(order, vec![p1, p2, copy_p2, p3]);
}

#[test]
fn clone_subtree_drops_dangling_child_reference() {
    let mut g = DocumentGraph::new(DocumentId(5));
    let p = g.push_block(Block::Paragraph { content: text("p") });
    let missing = NodeId(9_999);
    let sec = g.push_block(Block::Section { level: 1, title: text("s"), children: vec![p, missing] });

    let clone = g.clone_subtree(&[sec]).unwrap();
    let copy_sec = clone.roots[0];
    let Block::Section { children, .. } = &g.block(copy_sec).unwrap().block else { panic!("expected section") };
    assert_eq!(children, &vec![clone.mapping[&p]], "dangling source references must not alias the source subtree");
    assert!(!all_ids(&clone).contains(&missing));
}

#[test]
fn clone_subtree_copies_shared_child_once() {
    let mut g = DocumentGraph::new(DocumentId(6));
    let p = g.push_block(Block::Paragraph { content: text("shared") });
    let q1 = g.push_block(Block::Quote { children: vec![p] });
    let q2 = g.push_block(Block::Quote { children: vec![p] });

    let clone = g.clone_subtree(&[q1, q2]).unwrap();
    assert_eq!(clone.mapping.len(), 3, "the shared paragraph is copied exactly once");
    let copy_p = clone.mapping[&p];
    let Block::Quote { children: c1 } = &g.block(clone.roots[0]).unwrap().block else { panic!("expected quote") };
    let Block::Quote { children: c2 } = &g.block(clone.roots[1]).unwrap().block else { panic!("expected quote") };
    assert_eq!(c1, &vec![copy_p]);
    assert_eq!(c2, &vec![copy_p]);
}

#[test]
fn clone_subtree_reuses_copy_for_nested_requested_root() {
    let (mut g, ids) = build_tree();
    let (sec_root, sec_nested) = (ids[0], ids[2]);

    let clone = g.clone_subtree(&[sec_root, sec_nested]).unwrap();
    assert_eq!(clone.roots.len(), 2);
    assert_eq!(clone.roots[1], clone.mapping[&sec_nested], "a requested root inside an earlier subtree reuses its copy");
    assert_eq!(clone.mapping.len(), 6, "no node is copied twice");
    assert!(g.validate().is_valid());
}

#[test]
fn clone_subtree_missing_root_is_atomic_error() {
    let (mut g, ids) = build_tree();
    let baseline = g.clone();
    let err = g.clone_subtree(&[ids[0], NodeId(9_999)]).unwrap_err();
    assert_eq!(err, TransformError::NotFound(NodeId(9_999)));
    assert_eq!(g, baseline, "a failed clone must not touch the graph");
}

#[test]
fn delete_subtree_removes_whole_subtree() {
    let (mut g, ids) = build_tree();
    let (sec_root, p_hello) = (ids[0], ids[1]);
    let removed = g.delete_subtree(sec_root).unwrap();
    assert_eq!(removed, 6);
    assert!(g.block(sec_root).is_none());
    assert!(g.block(p_hello).is_none());
    assert!(g.blocks.is_empty());
}

#[test]
fn delete_block_keeps_span_alignment() {
    let mut g = DocumentGraph::new(DocumentId(7));
    let p1 = g.push_block_at(Block::Paragraph { content: text("1") }, span(0, 1));
    let p2 = g.push_block_at(Block::Paragraph { content: text("2") }, span(2, 3));
    g.delete_block(p1).unwrap();
    assert_eq!(g.block_span(p2), span(2, 3), "block_spans must stay aligned with blocks after a delete");
}

// ===========================================================================
// 表格跨度占位 / 插入
// ===========================================================================

/// 2x2 表体，每格一个段落。
fn build_grid(g: &mut DocumentGraph) -> (NodeId, Vec<NodeId>) {
    let a = g.push_block(Block::Paragraph { content: text("a") });
    let b = g.push_block(Block::Paragraph { content: text("b") });
    let c = g.push_block(Block::Paragraph { content: text("c") });
    let d = g.push_block(Block::Paragraph { content: text("d") });
    let table = g.push_block(Block::Table {
        caption: None,
        head: Vec::new(),
        body: vec![
            TableRow { cells: vec![cell(vec![a]), cell(vec![b])] },
            TableRow { cells: vec![cell(vec![c]), cell(vec![d])] },
        ],
        foot: Vec::new(),
    });
    (table, vec![a, b, c, d])
}

#[test]
fn set_cell_span_merges_and_consumes_covered_cells() {
    let mut g = DocumentGraph::new(DocumentId(8));
    let (table, ids) = build_grid(&mut g);
    let (a, b, _c, _d) = (ids[0], ids[1], ids[2], ids[3]);

    g.set_cell_span(table, 0, 0, 2, 1).unwrap();

    let Block::Table { body, .. } = &g.block(table).unwrap().block else { panic!("expected table") };
    assert_eq!(body[0].cells.len(), 1, "the covered cell leaves the row");
    assert_eq!(body[0].cells[0].colspan, 2);
    assert_eq!(body[0].cells[0].rowspan, 1);
    assert_eq!(body[0].cells[0].children, vec![a], "the anchor keeps its content");
    assert!(g.block(b).is_none(), "the consumed cell's subtree is physically removed");
    assert!(g.validate().is_valid());

    // 设回 1x1 即拆分（锚点恢复 1x1，被消耗的单元格不复活）。
    g.set_cell_span(table, 0, 0, 1, 1).unwrap();
    let Block::Table { body, .. } = &g.block(table).unwrap().block else { panic!("expected table") };
    assert_eq!((body[0].cells[0].colspan, body[0].cells[0].rowspan), (1, 1));
}

#[test]
fn set_cell_span_addresses_anchor_by_grid_column() {
    let mut g = DocumentGraph::new(DocumentId(9));
    let a = g.push_block(Block::Paragraph { content: text("a") });
    let b = g.push_block(Block::Paragraph { content: text("b") });
    let table = g.push_block(Block::Table {
        caption: None,
        head: Vec::new(),
        body: vec![TableRow { cells: vec![cell_span(vec![a], 2, 1), cell(vec![b])] }],
        foot: Vec::new(),
    });

    // 列 1 落在锚点 (colspan 2) 的跨度内 → 目标是锚点；消耗其后的单元格。
    g.set_cell_span(table, 0, 1, 3, 1).unwrap();
    let Block::Table { body, .. } = &g.block(table).unwrap().block else { panic!("expected table") };
    assert_eq!(body[0].cells.len(), 1);
    assert_eq!(body[0].cells[0].colspan, 3);
    assert!(g.block(b).is_none());
}

#[test]
fn set_cell_span_rejects_bad_bounds() {
    let mut g = DocumentGraph::new(DocumentId(10));
    let (table, _ids) = build_grid(&mut g);
    let p = g.push_block(Block::Paragraph { content: text("p") });

    assert_eq!(g.set_cell_span(table, 0, 0, 0, 1).unwrap_err(), TransformError::InvalidSpan { colspan: 0, rowspan: 1 });
    assert_eq!(g.set_cell_span(table, 0, 0, 65_536, 1).unwrap_err(), TransformError::InvalidSpan { colspan: 65_536, rowspan: 1 });
    assert_eq!(g.set_cell_span(table, 5, 0, 2, 2).unwrap_err(), TransformError::RowOutOfRange { table, row: 5, rows: 2 });
    assert_eq!(g.set_cell_span(table, 0, 5, 2, 1).unwrap_err(), TransformError::ColumnOutOfRange { table, column: 5, columns: 2 });
    assert_eq!(g.set_cell_span(p, 0, 0, 2, 1).unwrap_err(), TransformError::NotATable(p));
    assert_eq!(g.set_cell_span(NodeId(9_999), 0, 0, 2, 1).unwrap_err(), TransformError::NotFound(NodeId(9_999)));
}

#[test]
fn insert_table_row_places_placeholder_cells_within_bands() {
    let mut g = DocumentGraph::new(DocumentId(11));
    let h = g.push_block(Block::Paragraph { content: text("h") });
    let b1 = g.push_block(Block::Paragraph { content: text("b1") });
    let b2 = g.push_block(Block::Paragraph { content: text("b2") });
    let f = g.push_block(Block::Paragraph { content: text("f") });
    let table = g.push_block(Block::Table {
        caption: None,
        head: vec![TableRow { cells: vec![cell(vec![h]), cell(vec![h])] }],
        body: vec![TableRow { cells: vec![cell(vec![b1]), cell(vec![b2])] }],
        foot: vec![TableRow { cells: vec![cell(vec![f]), cell(vec![f])] }],
    });

    // 行 0 → 表头带；行 2 → 表体带；行 4 → 表脚带。
    g.insert_table_row(table, 0, Some(vec![text("x"), text("y")])).unwrap();
    g.insert_table_row(table, 2, None).unwrap();
    g.insert_table_row(table, 4, None).unwrap();

    let Block::Table { head, body, foot, .. } = &g.block(table).unwrap().block else { panic!("expected table") };
    assert_eq!(head.len(), 2);
    assert_eq!(body.len(), 2);
    assert_eq!(foot.len(), 2);
    let first_cell = &head[0].cells[0];
    assert_eq!((first_cell.colspan, first_cell.rowspan), (1, 1));
    assert_eq!(first_cell.children.len(), 1);
    let Block::Paragraph { content } = &g.block(first_cell.children[0]).unwrap().block else { panic!("expected placeholder paragraph") };
    assert_eq!(content, &text("x"));
    assert!(g.validate().is_valid());

    assert_eq!(g.insert_table_row(table, 9, None).unwrap_err(), TransformError::RowOutOfRange { table, row: 9, rows: 6 });
    assert_eq!(
        g.insert_table_row(table, 1, Some(vec![text("only")])).unwrap_err(),
        TransformError::CellCountMismatch { table, expected: 2, actual: 1 }
    );
    let empty = g.push_block(Block::Table { caption: None, head: Vec::new(), body: Vec::new(), foot: Vec::new() });
    assert_eq!(g.insert_table_row(empty, 0, None).unwrap_err(), TransformError::EmptyTable(empty));
}

#[test]
fn insert_table_column_addresses_grid_columns() {
    let mut g = DocumentGraph::new(DocumentId(12));
    let a = g.push_block(Block::Paragraph { content: text("a") });
    let b = g.push_block(Block::Paragraph { content: text("b") });
    let c = g.push_block(Block::Paragraph { content: text("c") });
    let d = g.push_block(Block::Paragraph { content: text("d") });
    let table = g.push_block(Block::Table {
        caption: None,
        head: Vec::new(),
        body: vec![
            TableRow { cells: vec![cell_span(vec![a], 2, 1), cell(vec![b])] },
            TableRow { cells: vec![cell(vec![c]), cell(vec![d])] },
        ],
        foot: Vec::new(),
    });

    // 网格列 1：行 0 落在锚点跨度内 → 插到锚点前；行 1 在 c 之后。
    g.insert_table_column(table, 1, Some(vec![text("n0"), text("n1")])).unwrap();
    let Block::Table { body, .. } = &g.block(table).unwrap().block else { panic!("expected table") };
    assert_eq!(body[0].cells.len(), 3);
    assert_eq!(body[1].cells.len(), 3);
    let Block::Paragraph { content } = &g.block(body[0].cells[0].children[0]).unwrap().block else { panic!("expected paragraph") };
    assert_eq!(content, &text("n0"));
    let Block::Paragraph { content } = &g.block(body[1].cells[1].children[0]).unwrap().block else { panic!("expected paragraph") };
    assert_eq!(content, &text("n1"));
    assert!(g.validate().is_valid());

    assert_eq!(g.insert_table_column(table, 9, None).unwrap_err(), TransformError::ColumnOutOfRange { table, column: 9, columns: 4 });
    assert_eq!(
        g.insert_table_column(table, 0, Some(vec![text("one")])).unwrap_err(),
        TransformError::CellCountMismatch { table, expected: 2, actual: 1 }
    );
}

#[test]
fn duplicate_table_row_keeps_band_and_spans() {
    let mut g = DocumentGraph::new(DocumentId(13));
    let h1 = g.push_block_at(Block::Paragraph { content: text("h1") }, span(0, 2));
    let h2 = g.push_block_at(Block::Paragraph { content: text("h2") }, span(3, 5));
    let b1 = g.push_block(Block::Paragraph { content: text("b1") });
    let table = g.push_block(Block::Table {
        caption: Some("表".into()),
        head: vec![TableRow { cells: vec![cell_span(vec![h1], 2, 1), cell(vec![h2])] }],
        body: vec![TableRow { cells: vec![cell(vec![b1]), cell(vec![b1])] }],
        foot: Vec::new(),
    });

    let position = g.duplicate_table_row(table, 0).unwrap();
    assert_eq!(position, 1);

    let Block::Table { head, body, .. } = &g.block(table).unwrap().block else { panic!("expected table") };
    assert_eq!(head.len(), 2, "the copy stays inside the header band");
    assert_eq!(body.len(), 1);
    assert_eq!(head[1].cells[0].colspan, 2, "row and column spans survive the copy");
    assert_eq!(head[1].cells[1].rowspan, 1);
    let copy_h1 = head[1].cells[0].children[0];
    let copy_h2 = head[1].cells[1].children[0];
    assert_ne!(copy_h1, h1);
    assert_eq!(g.block(copy_h1).unwrap().block, g.block(h1).unwrap().block);
    assert_eq!(g.block_span(copy_h1), span(0, 2), "authored spans survive the row copy");
    assert_eq!(g.block_span(copy_h2), span(3, 5));
    assert!(g.validate().is_valid());

    assert_eq!(g.duplicate_table_row(table, 5).unwrap_err(), TransformError::RowOutOfRange { table, row: 5, rows: 3 });
}

// ===========================================================================
// 事务组合与回滚
// ===========================================================================

#[test]
fn transaction_commit_keeps_combined_edits() {
    let (mut g, ids) = build_tree();
    let (p_hello, list) = (ids[1], ids[4]);
    let baseline = g.clone();
    let copy_id;

    {
        let mut tx: GraphTransaction = g.begin_transaction();
        let clone: SubtreeClone = tx.clone_subtree(&[p_hello]).unwrap();
        copy_id = clone.roots[0];
        tx.delete_block(list).unwrap();
        tx.replace_block(p_hello, Block::Code { language: None, content: "swapped".into() }).unwrap();
        assert!(tx.is_active());
        tx.commit();
    }

    assert_ne!(g, baseline);
    assert!(g.block(copy_id).is_some());
    assert!(g.block(list).is_none());
    let Block::Code { content, .. } = &g.block(p_hello).unwrap().block else { panic!("expected replaced code block") };
    assert_eq!(content, "swapped");
    assert!(g.validate().is_valid());
}

#[test]
fn transaction_failure_rolls_back_clone_delete_and_replace() {
    let (mut g, ids) = build_tree();
    let (sec_root, p_hello, list) = (ids[0], ids[1], ids[4]);
    let baseline = g.clone();

    {
        let mut tx = g.begin_transaction();
        tx.clone_subtree(&[sec_root]).unwrap();
        tx.delete_block(p_hello).unwrap();
        tx.set_cell_span(NodeId(9_999), 0, 0, 2, 2).unwrap_err();
        // 未 commit 即离开作用域 —— Drop 回滚。
    }
    assert_eq!(g, baseline, "an uncommitted session must restore blocks, spans and revision");

    // 显式 rollback 同样恢复。
    {
        let mut tx = g.begin_transaction();
        tx.clone_subtree(&[p_hello]).unwrap();
        tx.delete_block(list).unwrap();
        tx.rollback();
    }
    assert_eq!(g, baseline);
}

#[test]
fn transaction_rolls_back_table_span_operations() {
    let mut g = DocumentGraph::new(DocumentId(14));
    let (table, ids) = build_grid(&mut g);
    let baseline = g.clone();

    {
        let mut tx = g.begin_transaction();
        tx.set_cell_span(table, 0, 0, 2, 1).unwrap();
        tx.insert_table_row(table, 0, None).unwrap();
        tx.duplicate_table_row(table, 1).unwrap();
        tx.delete_block(NodeId(9_999)).unwrap_err();
        tx.rollback();
    }
    assert_eq!(g, baseline);
    assert!(g.block(ids[1]).is_some(), "cells consumed by the merge come back with the rollback");
}
