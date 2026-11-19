//! `remove-blocks` 清扫冒烟：删除容器块必须连带删除其不可达后代。
//!
//! 后代集合来自 `boost.math::graph_theory::reachable_from`（共享核 BFS），
//! 本测试锁定迁移后的行为合同：嵌套后代被扫掉、无关兄弟保留。

use document_convert::{TransformPassStatus, apply_transform_passes};
use document_ir::{Block, DocumentGraph, DocumentId};

#[test]
fn remove_blocks_sweeps_unreachable_descendants() {
    let mut graph = DocumentGraph::new(DocumentId(1));
    let leaf = graph.push_block(Block::Section { level: 3, title: Vec::new(), children: Vec::new() });
    let middle = graph.push_block(Block::Quote { children: vec![leaf] });
    graph.push_block(Block::Quote { children: vec![middle] });
    let keeper = graph.push_block(Block::Section { level: 1, title: Vec::new(), children: Vec::new() });

    let outcome = apply_transform_passes(&mut graph, Some(r#"{"transforms":[{"name":"remove-blocks","targetKinds":["quote"]}]}"#));

    assert_eq!(outcome.passes.len(), 1);
    let report = &outcome.passes[0];
    assert_eq!(report.status, TransformPassStatus::Applied);
    assert_eq!(report.changed_nodes, 2, "both quote blocks are removed by the walk");
    assert_eq!(report.swept_nodes, Some(1), "the section reachable only from the quotes is swept");
    assert!(graph.block(leaf).is_none(), "the unreachable descendant must be deleted");
    assert!(graph.block(keeper).is_some(), "the unrelated section survives");
}
