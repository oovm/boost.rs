//! 事务式变换会话 —— 复用 [`DocumentGraph`] 的 [`Clone`] 做快照回滚。
//!
//! 图中原本没有事务机制；本模块提供最小的暂存会话：[`DocumentGraph::begin_transaction`]
//! 先快照整图，会话期间通过 `Deref`/`DerefMut` 直接调用任意 mutation API，
//! 只有显式 [`GraphTransaction::commit`] 才接受改动。未提交即离开作用域
//! （正常结束、提前 `return`、panic 展开）时按快照整体回滚，包括 `revision`
//! 与 `block_spans` 对齐状态。

use std::ops::{Deref, DerefMut};

use crate::DocumentGraph;

/// 一个暂存变换会话。
///
/// 见 [`DocumentGraph::begin_transaction`]。
#[derive(Debug)]
pub struct GraphTransaction<'g> {
    target: &'g mut DocumentGraph,
    snapshot: Option<DocumentGraph>,
}

impl DocumentGraph {
    /// 开启一个事务会话：复制当前整图作为回滚快照。
    ///
    /// 返回的 [`GraphTransaction`] 以 `&mut DocumentGraph` 方式解引用，
    /// 会话内所有改动在 [`GraphTransaction::commit`] 前都是暂存的。
    pub fn begin_transaction(&mut self) -> GraphTransaction<'_> {
        let snapshot = self.clone();
        GraphTransaction { target: self, snapshot: Some(snapshot) }
    }
}

impl GraphTransaction<'_> {
    /// 接受会话期间的全部改动。
    pub fn commit(mut self) {
        self.snapshot = None;
    }

    /// 显式丢弃会话期间的全部改动，恢复快照。
    pub fn rollback(mut self) {
        self.restore();
    }

    /// 当前会话是否仍未提交。
    pub fn is_active(&self) -> bool {
        self.snapshot.is_some()
    }

    fn restore(&mut self) {
        if let Some(snapshot) = self.snapshot.take() {
            *self.target = snapshot;
        }
    }
}

impl Deref for GraphTransaction<'_> {
    type Target = DocumentGraph;

    fn deref(&self) -> &Self::Target {
        self.target
    }
}

impl DerefMut for GraphTransaction<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.target
    }
}

impl Drop for GraphTransaction<'_> {
    fn drop(&mut self) {
        self.restore();
    }
}
