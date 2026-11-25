//! 子树深复制与整棵删除 —— 图级复制变换。
//!
//! 语义对齐下游产品模型的 `duplicate_subtree` / `sweep`：
//! - 深复制给每个被复制节点分配全新 [`NodeId`]，正文 payload 与来源跨度
//!   （[`DocumentGraph::block_span`]）原样保留，父子结构通过 `旧 → 新`
//!   映射表重建；
//! - 复制体内指向缺失子节点的引用被丢弃，而不是悬空指向源子树；
//! - 指向复制体外节点的 inline 引用（如 [`crate::Inline::Reference`]）保持
//!   指向原节点，调用方可用映射表自行重定位。

use std::collections::{BTreeMap, HashSet};

use crate::{Block, BlockNode, DocumentGraph, NodeId, TransformError};

/// 一次成功深复制的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtreeClone {
    /// 每个被复制节点 `旧 → 新` 的映射，覆盖复制体全部节点。
    pub mapping: BTreeMap<NodeId, NodeId>,
    /// 每个请求根对应的新根 id，按请求顺序排列。
    pub roots: Vec<NodeId>,
}

impl DocumentGraph {
    /// 深复制若干根节点及其全部后代，返回新旧节点映射。
    ///
    /// 行为：
    /// 1. 每个根的子树以先序复制，新节点经 [`DocumentGraph::push_block_at`]
    ///    分配全新 id，并携带源节点已登记的来源跨度；
    /// 2. 复制体内部的子引用按映射表重定位；指向缺失节点的引用被丢弃；
    ///    同一子树内被多处共享的子节点只复制一次，多处引用指向同一副本；
    /// 3. 每个新根紧随其源节点接入：源在某个容器（Section / Quote / List
    ///    item / TableCell）的 children 列表中时，副本插入其后；源为顶层节点
    ///    时，副本在 `blocks` 顺序中移到源之后；
    /// 4. 若某个请求根已被先前某个根的子树覆盖，不再重复复制，直接复用
    ///    映射表中的副本 id。
    ///
    /// 任一请求根不存在时返回 [`TransformError::NotFound`] 且不产生任何改动。
    pub fn clone_subtree(&mut self, roots: &[NodeId]) -> Result<SubtreeClone, TransformError> {
        for root in roots {
            if self.block(*root).is_none() {
                return Err(TransformError::NotFound(*root));
            }
        }
        let mut clone = SubtreeClone { mapping: BTreeMap::new(), roots: Vec::with_capacity(roots.len()) };
        for root in roots {
            if let Some(copy) = clone.mapping.get(root).copied() {
                clone.roots.push(copy);
                continue;
            }
            let copy = self.copy_node_detached(*root, &mut clone.mapping);
            self.attach_copy_after_source(*root, copy);
            clone.roots.push(copy);
        }
        self.bump_revision();
        Ok(clone)
    }

    /// 物理删除一个节点及其全部后代，返回被移除的节点数。
    ///
    /// 与 [`DocumentGraph::delete_block`] 只摘除节点本体不同，本方法把整棵
    /// 子树从 `blocks`（及对齐的 `block_spans`）中移除，并清除所有容器对
    /// 被删节点的引用，语义对齐下游的 `remove_block` / `sweep`。
    /// relations 与 sources 中的引用仍需调用方自理（由 `validate` 报告）。
    pub fn delete_subtree(&mut self, id: NodeId) -> Result<usize, TransformError> {
        if self.block(id).is_none() {
            return Err(TransformError::NotFound(id));
        }
        let mut doomed = HashSet::new();
        self.collect_subtree_ids(id, &mut doomed);
        for node in &mut self.blocks {
            Self::prune_children(&mut node.block, &doomed);
        }
        let before = self.blocks.len();
        let mut kept_blocks: Vec<BlockNode> = Vec::with_capacity(before);
        let mut kept_spans = Vec::with_capacity(before);
        for (index, node) in std::mem::take(&mut self.blocks).into_iter().enumerate() {
            if doomed.contains(&node.id) {
                continue;
            }
            kept_blocks.push(node);
            kept_spans.push(self.block_spans.get(index).copied().flatten());
        }
        self.blocks = kept_blocks;
        self.block_spans = kept_spans;
        self.bump_revision();
        Ok(before - self.blocks.len())
    }

    /// 深复制单个节点及其后代（不接入任何容器），返回新根 id。
    /// 供 [`DocumentGraph::clone_subtree`] 与表格行/列复制共享。
    pub(crate) fn copy_node_detached(&mut self, id: NodeId, mapping: &mut BTreeMap<NodeId, NodeId>) -> NodeId {
        if let Some(copy) = mapping.get(&id).copied() {
            return copy;
        }
        let source = self.block(id).expect("the subtree node was resolved").block.clone();
        let children = DocumentGraph::immediate_children(&source);
        let span = self.block_span(id);
        let new_id = self.push_block_at(source, span);
        mapping.insert(id, new_id);
        for child in children {
            if self.block(child).is_some() {
                self.copy_node_detached(child, mapping);
            }
        }
        let node = self.blocks.iter_mut().find(|node| node.id == new_id).expect("the copy was just pushed");
        Self::remap_copy_children(&mut node.block, mapping);
        new_id
    }

    /// 把副本根接入源根之后：优先在源的容器列表中紧随源插入；源为顶层节点
    /// 时在 `blocks` 顺序上移到源之后（`block_spans` 同步移动保持对齐）。
    fn attach_copy_after_source(&mut self, source: NodeId, copy: NodeId) {
        if self.insert_after_in_containers(source, copy) {
            return;
        }
        let Some(source_pos) = self.blocks.iter().position(|node| node.id == source) else { return };
        let Some(copy_pos) = self.blocks.iter().position(|node| node.id == copy) else { return };
        let node = self.blocks.remove(copy_pos);
        let span = if copy_pos < self.block_spans.len() { self.block_spans.remove(copy_pos) } else { None };
        let insert = if copy_pos < source_pos { source_pos } else { source_pos + 1 };
        self.blocks.insert(insert, node);
        self.block_spans.insert(insert.min(self.block_spans.len()), span);
    }

    /// 在引用 `target` 的第一个容器列表中把 `new_id` 插入其后；
    /// `target` 无容器（顶层节点）时返回 `false`。
    fn insert_after_in_containers(&mut self, target: NodeId, new_id: NodeId) -> bool {
        for node in &mut self.blocks {
            match &mut node.block {
                Block::Section { children, .. } | Block::Quote { children } => {
                    if let Some(position) = children.iter().position(|child| *child == target) {
                        children.insert(position + 1, new_id);
                        return true;
                    }
                }
                Block::List { items, .. } => {
                    for item in items {
                        if let Some(position) = item.children.iter().position(|child| *child == target) {
                            item.children.insert(position + 1, new_id);
                            return true;
                        }
                    }
                }
                Block::Table { head, body, foot, .. } => {
                    for row in head.iter_mut().chain(body.iter_mut()).chain(foot.iter_mut()) {
                        for cell in &mut row.cells {
                            if let Some(position) = cell.children.iter().position(|child| *child == target) {
                                cell.children.insert(position + 1, new_id);
                                return true;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        false
    }

    /// 收集一个节点及其全部后代 id（含环保护）。
    pub(crate) fn collect_subtree_ids(&self, id: NodeId, doomed: &mut HashSet<NodeId>) {
        if !doomed.insert(id) {
            return;
        }
        if let Some(node) = self.block(id) {
            for child in DocumentGraph::immediate_children(&node.block) {
                self.collect_subtree_ids(child, doomed);
            }
        }
    }

    /// 从 block 的容器列表中剔除 `doomed` 集合里的引用。
    pub(crate) fn prune_children(block: &mut Block, doomed: &HashSet<NodeId>) {
        match block {
            Block::Section { children, .. } | Block::Quote { children } => children.retain(|child| !doomed.contains(child)),
            Block::List { items, .. } => {
                for item in items {
                    item.children.retain(|child| !doomed.contains(child));
                }
            }
            Block::Table { head, body, foot, .. } => {
                for row in head.iter_mut().chain(body.iter_mut()).chain(foot.iter_mut()) {
                    for cell in &mut row.cells {
                        cell.children.retain(|child| !doomed.contains(child));
                    }
                }
            }
            _ => {}
        }
    }

    /// 把复制体内部的子引用重映射到新节点；无副本的引用被丢弃。
    fn remap_copy_children(block: &mut Block, mapping: &BTreeMap<NodeId, NodeId>) {
        match block {
            Block::Section { children, .. } | Block::Quote { children } => retain_copies(children, mapping),
            Block::List { items, .. } => {
                for item in items {
                    retain_copies(&mut item.children, mapping);
                }
            }
            Block::Table { head, body, foot, .. } => {
                for row in head.iter_mut().chain(body.iter_mut()).chain(foot.iter_mut()) {
                    for cell in &mut row.cells {
                        retain_copies(&mut cell.children, mapping);
                    }
                }
            }
            _ => {}
        }
    }
}

fn retain_copies(children: &mut Vec<NodeId>, mapping: &BTreeMap<NodeId, NodeId>) {
    children.retain_mut(|child| match mapping.get(child) {
        Some(mapped) => {
            *child = *mapped;
            true
        }
        None => false,
    });
}

