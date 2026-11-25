//! 块类型互转的图级变换 —— 段落 ↔ 标题 ↔ 引用 ↔ 代码 ↔ 列表。
//!
//! 语义对齐下游产品模型的 `block.convert`：块保留自身 [`NodeId`] 与在文档
//! 中的位置，内容按转换对搬运。标题转段落时子块整体提升到它之后；引用转
//! 段落时单个段落子块原样复用，其余情况把子块纯文本拼接并物理清扫旧子块；
//! 列表转段落时首条目占据块本体，其余条目连同各自嵌套子块紧随其后。
//! 源与目标同类、表格等不可转的块、以及不支持的转换对被拒绝。

use crate::{Block, DocumentGraph, Inline, ListItem, NodeId, TransformError, plain_text};

/// [`DocumentGraph::convert_block`] 可互转的块类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// 段落。
    Paragraph,
    /// 标题（Section）。
    Heading,
    /// 引用块。
    Quote,
    /// 代码块。
    Code,
    /// 列表。
    List,
}

impl BlockKind {
    /// 稳定的类型名。
    pub fn label(self) -> &'static str {
        match self {
            Self::Paragraph => "paragraph",
            Self::Heading => "heading",
            Self::Quote => "quote",
            Self::Code => "code",
            Self::List => "list",
        }
    }
}

impl DocumentGraph {
    /// 就地转换一个块的类型，返回转换前的类型。
    ///
    /// 支持的转换对（其余被拒绝）：
    /// - 段落 → 标题：正文成为标题，`level` 必填（1..=6）；
    /// - 标题 → 段落：标题成为正文，子块提升到块之后（容器内或顶层顺序）；
    /// - 段落 → 引用：正文移入一个新的段落子块；
    /// - 引用 → 段落：单个段落子块原样复用，否则子块纯文本以空格拼接，旧
    ///   子块连同其子树被物理删除；
    /// - 段落 → 代码：正文展平为纯文本，`language` 作为围栏标签保留；
    /// - 代码 → 段落：源码成为一段纯文本；
    /// - 段落 → 列表：正文成为唯一条目，`ordered` 决定样式，有序时编号从 1；
    /// - 列表 → 段落：首条目占据块本体，其余条目成为新段落紧随其后，各条目
    ///   的嵌套子块一并随行。
    ///
    /// 块 id 与位置保持不变；新产生的块（引用子段落、列表拆出的段落）经
    /// [`DocumentGraph::push_block`] 追加，来源跨度不虚构。
    ///
    /// 失败：[`TransformError::NotFound`]、[`TransformError::NotConvertible`]、
    /// [`TransformError::AlreadyKind`]、[`TransformError::MissingHeadingLevel`]、
    /// [`TransformError::InvalidConversion`]，均不产生任何改动。
    pub fn convert_block(
        &mut self,
        id: NodeId,
        to: BlockKind,
        level: Option<u8>,
        language: Option<&str>,
        ordered: Option<bool>,
    ) -> Result<BlockKind, TransformError> {
        let from = {
            let node = self.block(id).ok_or(TransformError::NotFound(id))?;
            match &node.block {
                Block::Paragraph { .. } => BlockKind::Paragraph,
                Block::Section { .. } => BlockKind::Heading,
                Block::Quote { .. } => BlockKind::Quote,
                Block::Code { .. } => BlockKind::Code,
                Block::List { .. } => BlockKind::List,
                _ => return Err(TransformError::NotConvertible(id)),
            }
        };
        if from == to {
            return Err(TransformError::AlreadyKind { node: id, kind: to });
        }
        match (from, to) {
            (BlockKind::Paragraph, BlockKind::Heading) => {
                let Some(level @ 1..=6) = level
                else {
                    return Err(TransformError::MissingHeadingLevel(id));
                };
                let node = self.blocks.iter_mut().find(|node| node.id == id).expect("the block was just resolved");
                let Block::Paragraph { content } = &mut node.block
                else {
                    unreachable!("the source was a paragraph")
                };
                let title = std::mem::take(content);
                node.block = Block::Section { level, title, children: Vec::new() };
            }
            (BlockKind::Heading, BlockKind::Paragraph) => {
                let (title, children) = {
                    let node = self.blocks.iter_mut().find(|node| node.id == id).expect("the block was just resolved");
                    let Block::Section { title, children, .. } = &mut node.block
                    else {
                        unreachable!("the source was a heading")
                    };
                    (std::mem::take(title), std::mem::take(children))
                };
                let node = self.blocks.iter_mut().find(|node| node.id == id).expect("the block is still there");
                node.block = Block::Paragraph { content: title };
                let mut anchor = id;
                for child in children {
                    if self.block(child).is_some() {
                        self.place_after(anchor, child);
                        anchor = child;
                    }
                }
            }
            (BlockKind::Paragraph, BlockKind::Quote) => {
                let content = {
                    let node = self.blocks.iter_mut().find(|node| node.id == id).expect("the block was just resolved");
                    let Block::Paragraph { content } = &mut node.block
                    else {
                        unreachable!("the source was a paragraph")
                    };
                    std::mem::take(content)
                };
                let child = self.push_block(Block::Paragraph { content });
                let node = self.blocks.iter_mut().find(|node| node.id == id).expect("the block is still there");
                node.block = Block::Quote { children: vec![child] };
            }
            (BlockKind::Quote, BlockKind::Paragraph) => {
                let old_children = {
                    let node = self.blocks.iter_mut().find(|node| node.id == id).expect("the block was just resolved");
                    let Block::Quote { children } = &mut node.block
                    else {
                        unreachable!("the source was a quote")
                    };
                    std::mem::take(children)
                };
                let content = match old_children.as_slice() {
                    [child] => self.blocks.iter().find(|node| node.id == *child).and_then(|node| match &node.block {
                        Block::Paragraph { content } => Some(content.clone()),
                        _ => None,
                    }),
                    _ => None,
                };
                let content = content.unwrap_or_else(|| {
                    vec![Inline::Text { text: old_children.iter().map(|child| self.block_text(*child)).collect::<Vec<_>>().join(" ") }]
                });
                for old in old_children {
                    self.remove_subtree_quiet(old);
                }
                let node = self.blocks.iter_mut().find(|node| node.id == id).expect("the block is still there");
                node.block = Block::Paragraph { content };
            }
            (BlockKind::Paragraph, BlockKind::Code) => {
                let content = {
                    let node = self.blocks.iter_mut().find(|node| node.id == id).expect("the block was just resolved");
                    let Block::Paragraph { content } = &mut node.block
                    else {
                        unreachable!("the source was a paragraph")
                    };
                    std::mem::take(content)
                };
                let text = plain_text(&content);
                let node = self.blocks.iter_mut().find(|node| node.id == id).expect("the block is still there");
                node.block = Block::Code { language: language.map(str::to_owned), content: text };
            }
            (BlockKind::Code, BlockKind::Paragraph) => {
                let node = self.blocks.iter_mut().find(|node| node.id == id).expect("the block was just resolved");
                let Block::Code { content, .. } = &mut node.block
                else {
                    unreachable!("the source was code")
                };
                let text = std::mem::take(content);
                node.block = Block::Paragraph { content: vec![Inline::Text { text }] };
            }
            (BlockKind::Paragraph, BlockKind::List) => {
                let content = {
                    let node = self.blocks.iter_mut().find(|node| node.id == id).expect("the block was just resolved");
                    let Block::Paragraph { content } = &mut node.block
                    else {
                        unreachable!("the source was a paragraph")
                    };
                    std::mem::take(content)
                };
                let ordered = ordered.unwrap_or(false);
                let node = self.blocks.iter_mut().find(|node| node.id == id).expect("the block is still there");
                node.block = Block::List {
                    ordered,
                    items: vec![ListItem { content, children: Vec::new(), number: ordered.then_some(1), is_task: false, is_checked: None }],
                };
            }
            (BlockKind::List, BlockKind::Paragraph) => {
                let items = {
                    let node = self.blocks.iter_mut().find(|node| node.id == id).expect("the block was just resolved");
                    let Block::List { items, .. } = &mut node.block
                    else {
                        unreachable!("the source was a list")
                    };
                    std::mem::take(items)
                };
                let mut items = items.into_iter();
                let mut anchor = id;
                match items.next() {
                    Some(first) => {
                        let node = self.blocks.iter_mut().find(|node| node.id == id).expect("the block is still there");
                        node.block = Block::Paragraph { content: first.content };
                        for child in first.children {
                            if self.block(child).is_some() {
                                self.place_after(anchor, child);
                                anchor = child;
                            }
                        }
                    }
                    None => {
                        let node = self.blocks.iter_mut().find(|node| node.id == id).expect("the block is still there");
                        node.block = Block::Paragraph { content: Vec::new() };
                    }
                }
                for item in items {
                    let paragraph = self.push_block(Block::Paragraph { content: item.content });
                    self.place_after(anchor, paragraph);
                    anchor = paragraph;
                    for child in item.children {
                        if self.block(child).is_some() {
                            self.place_after(anchor, child);
                            anchor = child;
                        }
                    }
                }
            }
            (source, target) => return Err(TransformError::InvalidConversion { from: source, to: target }),
        }
        self.bump_revision();
        Ok(from)
    }

    /// 一个块的可见纯文本，递归引用块；不可见文本的块返回空串。
    fn block_text(&self, id: NodeId) -> String {
        let Some(node) = self.block(id)
        else {
            return String::new();
        };
        match &node.block {
            Block::Paragraph { content } => plain_text(content),
            Block::Section { title, .. } => plain_text(title),
            Block::Code { content, .. } => content.clone(),
            Block::Quote { children } => children.iter().map(|child| self.block_text(*child)).collect::<Vec<_>>().join(" "),
            _ => String::new(),
        }
    }
}
