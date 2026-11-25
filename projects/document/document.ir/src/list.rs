//! 列表的图级变换 —— 嵌套（缩进、提升、跨列表移动）与非嵌套操作
//! （排序、逆序）。
//!
//! 语义对齐下游产品模型的 `list.indentItem` / `list.unindentItem` /
//! `list.moveItemTo`：IR 把嵌套存为 [`crate::ListItem::children`] 里的嵌套列表块，
//! 所以缩进是把条目从当前 run 摘出、挂到前一个条目 children 末尾的同标记样式
//! 嵌套列表里（没有就新建一条只含该条目的嵌套 run，有序时从 1 重启编号）；
//! 提升则把条目末尾的同样式嵌套列表的 items 接回父 run，并物理清扫空掉的
//! 嵌套列表块本体，使其自身嵌套的子块随条目一起保留、不留悬空引用。
//! 有序列表在每次结构变化后从 run 首个条目携带的编号起重新连续编号；
//! 编号会越过 `u32` 上限时在任何改动发生前失败。

use std::collections::HashSet;

use crate::{Block, DocumentGraph, ListItem, NodeId, TransformError};

impl DocumentGraph {
    /// 把一个条目缩进到它前一个兄弟条目之下。
    ///
    /// 前一个条目的 children 末尾若已有同标记样式的嵌套列表，条目并入该
    /// run；否则新建一条只含该条目的嵌套列表挂在其后。有序列表在父 run 与
    /// 嵌套 run 两级都重新连续编号，嵌套 run 从自身首个编号（新建时为 1）
    /// 起算。第一个条目没有前驱兄弟，越界索引同样被拒绝。
    ///
    /// 失败：节点不存在 [`TransformError::NotFound`]、目标非列表
    /// [`TransformError::NotAList`]、首条目 [`TransformError::NoPreviousSibling`]、
    /// 索引越界 [`TransformError::ItemOutOfRange`]、编号溢出
    /// [`TransformError::NumberingOverflow`]，均不产生任何改动。
    pub fn indent_list_item(&mut self, list: NodeId, item: usize) -> Result<(), TransformError> {
        let (ordered, removed, host) = {
            let node = self.blocks.iter_mut().find(|node| node.id == list).ok_or(TransformError::NotFound(list))?;
            let Block::List { ordered, items } = &mut node.block
            else {
                return Err(TransformError::NotAList(list));
            };
            if item == 0 {
                return Err(TransformError::NoPreviousSibling(list));
            }
            let count = items.len();
            if item >= count {
                return Err(TransformError::ItemOutOfRange { list, item, items: count });
            }
            let base = if *ordered { items.first().and_then(|entry| entry.number).unwrap_or(1) } else { 0 };
            Self::renumber_fits(count - 1, base)?;
            let removed = items.remove(item);
            if *ordered {
                Self::renumber_items(items, base)?;
            }
            // 摘除后，前一个条目落在 index `item - 1`。
            let host = items.get(item - 1).and_then(|entry| entry.children.last().copied());
            (*ordered, removed, host)
        };
        let nested = host.filter(|child| {
            self.blocks
                .iter()
                .find(|node| node.id == *child)
                .is_some_and(|node| matches!(&node.block, Block::List { ordered: nested, .. } if *nested == ordered))
        });
        if let Some(child) = nested {
            let node = self.blocks.iter_mut().find(|node| node.id == child).expect("the nested list was resolved");
            let Block::List { items, .. } = &mut node.block
            else {
                unreachable!("the nested list was checked")
            };
            let base = items.first().and_then(|entry| entry.number).unwrap_or(1);
            Self::renumber_fits(items.len() + 1, base)?;
            items.push(removed);
            if ordered {
                Self::renumber_items(items, base)?;
            }
            self.bump_revision();
            return Ok(());
        }
        // 新建的嵌套 run 从 1 重启，有序编号保持连续。
        let mut nested = removed;
        if ordered {
            nested.number = Some(1);
        }
        let child = self.push_block(Block::List { ordered, items: vec![nested] });
        let parent = self.blocks.iter_mut().find(|node| node.id == list).expect("the list is still there");
        let Block::List { items, .. } = &mut parent.block
        else {
            unreachable!("the list is still a list")
        };
        items.get_mut(item - 1).expect("the previous item still exists").children.push(child);
        self.bump_revision();
        Ok(())
    }

    /// 把一个条目末尾嵌套列表里的 items 提升回父 run。
    ///
    /// 条目的最后一个 child 必须是同标记样式的嵌套列表，其 items 接回该条目
    /// 之后，父 run 有序时从首个条目编号起重排；空掉的嵌套列表块本体被物理
    /// 删除，但它先与已提升 items 的 child 引用脱钩，嵌套子块随条目保留。
    /// 没有可提升嵌套列表的条目被拒绝。
    ///
    /// 失败：[`TransformError::NotFound`]、[`TransformError::NotAList`]、
    /// [`TransformError::ItemOutOfRange`]、[`TransformError::NothingToLift`]、
    /// [`TransformError::NumberingOverflow`]，均不产生任何改动。
    pub fn unindent_list_item(&mut self, list: NodeId, item: usize) -> Result<(), TransformError> {
        let (child, lifted) = {
            let node = self.blocks.iter().find(|node| node.id == list).ok_or(TransformError::NotFound(list))?;
            let Block::List { ordered, items } = &node.block
            else {
                return Err(TransformError::NotAList(list));
            };
            let count = items.len();
            if item >= count {
                return Err(TransformError::ItemOutOfRange { list, item, items: count });
            }
            let Some(child) = items[item].children.last().copied()
            else {
                return Err(TransformError::NothingToLift { list, item });
            };
            let lifted = self.blocks.iter().find(|node| node.id == child).and_then(|node| match &node.block {
                Block::List { ordered: nested, items } if *nested == *ordered => Some(items.clone()),
                _ => None,
            });
            match lifted {
                Some(lifted) => (child, lifted),
                None => return Err(TransformError::NothingToLift { list, item }),
            }
        };
        let node = self.blocks.iter_mut().find(|node| node.id == list).expect("the list is still there");
        let Block::List { ordered, items } = &mut node.block
        else {
            unreachable!("the list is still a list")
        };
        let base = items.first().and_then(|entry| entry.number).unwrap_or(1);
        Self::renumber_fits(items.len() + lifted.len(), base)?;
        items[item].children.retain(|candidate| *candidate != child);
        items.splice(item + 1..item + 1, lifted);
        if *ordered {
            Self::renumber_items(items, base)?;
        }
        // 旧嵌套列表已把 items 交给父 run；先清掉它对提升条目 children 的
        // 引用，再删除块本体，提升条目的嵌套子块随之保留而非被连带扫掉。
        if let Some(node) = self.blocks.iter_mut().find(|node| node.id == child) {
            if let Block::List { items, .. } = &mut node.block {
                items.clear();
            }
        }
        self.remove_subtree_quiet(child);
        self.bump_revision();
        Ok(())
    }

    /// 把一个条目移动到另一个列表（或同一列表）的指定位置。
    ///
    /// 条目保留文本、任务标记、勾选状态与全部嵌套子块；源与目标各自从自身
    /// 首个编号重新连续编号，移入目标的条目编号由目标 run 接管。目标是被
    /// 移动条目的后代时拒绝，避免形成环。同一列表内调用即重排位置，索引须
    /// 落在现有条目位次内。
    ///
    /// 失败：[`TransformError::NotFound`]、[`TransformError::NotAList`]、
    /// [`TransformError::ItemOutOfRange`]、[`TransformError::IndexOutOfRange`]、
    /// [`TransformError::MoveIntoDescendant`]、[`TransformError::NumberingOverflow`]，
    /// 均不产生任何改动。
    pub fn move_list_item_to(&mut self, list: NodeId, item: usize, target: NodeId, index: usize) -> Result<(), TransformError> {
        let source_ordered = self.list_ordered(list)?;
        let target_ordered = self.list_ordered(target)?;
        if list == target {
            let node = self.blocks.iter_mut().find(|node| node.id == list).expect("the list was just resolved");
            let Block::List { ordered, items } = &mut node.block
            else {
                unreachable!("the list was just checked")
            };
            let count = items.len();
            if item >= count {
                return Err(TransformError::ItemOutOfRange { list, item, items: count });
            }
            if index >= count {
                return Err(TransformError::IndexOutOfRange { list, index, positions: count });
            }
            let base = if *ordered { items.first().and_then(|entry| entry.number).unwrap_or(1) } else { 0 };
            Self::renumber_fits(count, base)?;
            let moved = items.remove(item);
            items.insert(index, moved);
            if *ordered {
                Self::renumber_items(items, base)?;
            }
            self.bump_revision();
            return Ok(());
        }
        let (source_base, moved) = {
            let node = self.blocks.iter().find(|node| node.id == list).expect("the list was just resolved");
            let Block::List { ordered: _, items } = &node.block
            else {
                unreachable!("the list was just checked")
            };
            let count = items.len();
            if item >= count {
                return Err(TransformError::ItemOutOfRange { list, item, items: count });
            }
            let base = if source_ordered { items.first().and_then(|entry| entry.number).unwrap_or(1) } else { 0 };
            (base, items[item].clone())
        };
        let mut descendants = HashSet::new();
        for child in &moved.children {
            self.collect_subtree_ids(*child, &mut descendants);
        }
        if descendants.contains(&target) {
            return Err(TransformError::MoveIntoDescendant { list, item, target });
        }
        let target_base = {
            let node = self.blocks.iter().find(|node| node.id == target).expect("the target list was just resolved");
            let Block::List { ordered: _, items } = &node.block
            else {
                unreachable!("the target list was just checked")
            };
            if index > items.len() {
                return Err(TransformError::IndexOutOfRange { list: target, index, positions: items.len() + 1 });
            }
            if target_ordered { items.first().and_then(|entry| entry.number).unwrap_or(1) } else { 0 }
        };
        let source_count = {
            let node = self.blocks.iter().find(|node| node.id == list).expect("the list was just resolved");
            let Block::List { items, .. } = &node.block
            else {
                unreachable!("the list was just checked")
            };
            items.len()
        };
        let target_count = {
            let node = self.blocks.iter().find(|node| node.id == target).expect("the target list was just resolved");
            let Block::List { items, .. } = &node.block
            else {
                unreachable!("the target list was just checked")
            };
            items.len()
        };
        Self::renumber_fits(source_count - 1, source_base)?;
        Self::renumber_fits(target_count + 1, target_base)?;
        let node = self.blocks.iter_mut().find(|node| node.id == list).expect("the list was just resolved");
        let Block::List { items, .. } = &mut node.block
        else {
            unreachable!("the list was just checked")
        };
        items.remove(item);
        if source_ordered {
            Self::renumber_items(items, source_base)?;
        }
        let node = self.blocks.iter_mut().find(|node| node.id == target).expect("the target list was just resolved");
        let Block::List { items, .. } = &mut node.block
        else {
            unreachable!("the target list was just checked")
        };
        let mut moved = moved;
        moved.number = None;
        items.insert(index, moved);
        if target_ordered {
            Self::renumber_items(items, target_base)?;
        }
        self.bump_revision();
        Ok(())
    }

    /// 按条目文本排序一个列表的顶层条目，返回参与排序的条目数。
    ///
    /// 排序键是条目的纯文本（[`crate::plain_text`]）：先去除首尾空白，空条目
    /// 无论升降序都排在末尾；两个键都能解析为数字时按数值比较，否则按
    /// 忽略大小写的字典序比较。条目携带自身的任务标记、勾选状态与全部嵌套
    /// 子块一起移动；有序列表从排序前首个条目携带的编号起重新连续编号。
    ///
    /// 失败：[`TransformError::NotFound`]、[`TransformError::NotAList`]、
    /// [`TransformError::NumberingOverflow`]，均不产生任何改动。
    pub fn sort_list_items(&mut self, list: NodeId, ascending: bool) -> Result<usize, TransformError> {
        let count = {
            let node = self.blocks.iter_mut().find(|node| node.id == list).ok_or(TransformError::NotFound(list))?;
            let Block::List { ordered, items } = &mut node.block
            else {
                return Err(TransformError::NotAList(list));
            };
            let base = if *ordered { items.first().and_then(|entry| entry.number).unwrap_or(1) } else { 0 };
            Self::renumber_fits(items.len(), base)?;
            items.sort_by(|left, right| {
                compare_item_keys(&crate::plain_text(&left.content), &crate::plain_text(&right.content), ascending)
            });
            if *ordered {
                Self::renumber_items(items, base)?;
            }
            items.len()
        };
        self.bump_revision();
        Ok(count)
    }

    /// 逆序一个列表的顶层条目，返回条目数。
    ///
    /// 条目保留文本、任务标记、勾选状态与全部嵌套子块；有序列表从逆序前
    /// 首个条目携带的编号起重新连续编号，可见编号段保持连续。
    ///
    /// 失败：[`TransformError::NotFound`]、[`TransformError::NotAList`]、
    /// [`TransformError::NumberingOverflow`]，均不产生任何改动。
    pub fn reverse_list_items(&mut self, list: NodeId) -> Result<usize, TransformError> {
        let count = {
            let node = self.blocks.iter_mut().find(|node| node.id == list).ok_or(TransformError::NotFound(list))?;
            let Block::List { ordered, items } = &mut node.block
            else {
                return Err(TransformError::NotAList(list));
            };
            let base = if *ordered { items.first().and_then(|entry| entry.number).unwrap_or(1) } else { 0 };
            Self::renumber_fits(items.len(), base)?;
            items.reverse();
            if *ordered {
                Self::renumber_items(items, base)?;
            }
            items.len()
        };
        self.bump_revision();
        Ok(count)
    }

    /// 读取一个列表节点的 marker 样式，非列表返回 [`TransformError::NotAList`]。
    fn list_ordered(&self, id: NodeId) -> Result<bool, TransformError> {
        let node = self.block(id).ok_or(TransformError::NotFound(id))?;
        match &node.block {
            Block::List { ordered, .. } => Ok(*ordered),
            _ => Err(TransformError::NotAList(id)),
        }
    }

    /// 把一个有序 run 的编号重写为从 `base` 起的连续段。
    pub(crate) fn renumber_items(items: &mut [ListItem], base: u32) -> Result<(), TransformError> {
        for (index, entry) in items.iter_mut().enumerate() {
            let offset = u32::try_from(index).unwrap_or(u32::MAX);
            let Some(number) = base.checked_add(offset)
            else {
                return Err(TransformError::NumberingOverflow);
            };
            entry.number = Some(number);
        }
        Ok(())
    }

    /// 预检：`items` 个条目从 `base` 起连续编号是否会越过 `u32` 上限。
    fn renumber_fits(items: usize, base: u32) -> Result<(), TransformError> {
        if items == 0 {
            return Ok(());
        }
        let last = u32::try_from(items - 1).unwrap_or(u32::MAX);
        base.checked_add(last).map(|_| ()).ok_or(TransformError::NumberingOverflow)
    }
}

/// 排序键比较：空白先裁剪，空条目恒排末尾，数字按数值比较，
/// 其余按忽略大小写的字典序比较；降序只翻转非空键的相对次序。
fn compare_item_keys(left: &str, right: &str, ascending: bool) -> std::cmp::Ordering {
    let (left, right) = (left.trim(), right.trim());
    let left_blank = left.is_empty();
    let right_blank = right.is_empty();
    if left_blank != right_blank {
        return if left_blank { std::cmp::Ordering::Greater } else { std::cmp::Ordering::Less };
    }
    let ordering = match (left.parse::<f64>(), right.parse::<f64>()) {
        (Ok(left), Ok(right)) => left.partial_cmp(&right).unwrap_or(std::cmp::Ordering::Equal),
        _ => left.to_lowercase().cmp(&right.to_lowercase()),
    };
    if ascending { ordering } else { ordering.reverse() }
}
