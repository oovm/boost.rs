//! 连接内结果集游标的状态机。
//!
//! 一条已执行语句的结果集有两种来源:打开时一次性物化的行集,以及固定页
//! 快照上的惰性 b-tree 扫描。句柄分配、读取位置、事务代数失效与分窗读取
//! 全部归本模块;产品门面只保留公开入口、只读拒绝与提交边界之类的连接策略,
//! 不再持有游标状态。

use std::collections::BTreeMap;

use database_query::sqlite::{SqlError, value::Cell};

use crate::sqlite::select::{LazyBatch, LazyCursor};

/// 打开游标时返回的句柄与结果列标签。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorOpen {
    /// 连接内的游标句柄;后续 [`CursorTable::next`] 与 [`CursorTable::close`]
    /// 都以它定位游标。
    pub handle: u64,
    /// 本次执行产生的结果列标签。
    pub columns: Vec<String>,
    /// `true` 表示游标按表 b-tree 页顺序惰性扫描,`false` 表示结果集在
    /// 打开时已经全部物化。
    pub lazy: bool,
}

/// 一次游标读取返回的有界行窗口。
#[derive(Clone, Debug, PartialEq)]
pub struct CursorBatch {
    /// 本窗口的行,保持结果顺序。
    pub rows: Vec<Vec<Cell>>,
    /// 游标是否已经到达结果集末尾。
    pub done: bool,
}

/// 游标的结果来源。
enum CursorRows {
    /// 完整结果集,按执行顺序。
    Materialized { rows: Vec<Vec<Cell>>, position: usize },
    /// 打开时固定的页快照上的惰性表扫描。
    Lazy(LazyCursor),
}

/// 一个已执行结果集及其读取位置。
struct Cursor {
    /// 行集来源:惰性扫描或打开时物化。
    rows: CursorRows,
    /// 打开时所属事务的代数;`None` 表示在 autocommit 下打开。
    ///
    /// 事务结束时代数前进,旧代数的游标随之以 `cursor-expired` 拒绝,与
    /// SQLite 在事务结束后让语句失效一致。
    transaction_epoch: Option<u64>,
}

/// 一个连接持有的全部游标及其代数标记。
///
/// 句柄单调递增(回绕),不因关闭游标而回收;`epoch` 表示当前事务代数,
/// 只有事务结束(提交或回滚)才前进。
#[derive(Default)]
pub struct CursorTable {
    cursors: BTreeMap<u64, Cursor>,
    next: u64,
    epoch: u64,
}

impl CursorTable {
    /// 建一张空游标表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前事务代数。
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// 事务结束时让代数前进,使事务内打开的游标失效。
    pub fn advance_if_transaction_ended(&mut self, was_in_transaction: bool, in_transaction: bool) {
        if was_in_transaction && !in_transaction {
            self.epoch = self.epoch.wrapping_add(1);
        }
    }

    /// 登记一个打开时已物化的结果集,返回句柄。
    pub fn open_materialized(&mut self, rows: Vec<Vec<Cell>>, transaction_epoch: Option<u64>) -> u64 {
        let handle = self.next;
        self.next = self.next.wrapping_add(1);
        self.cursors.insert(handle, Cursor { rows: CursorRows::Materialized { rows, position: 0 }, transaction_epoch });
        handle
    }

    /// 登记一个惰性表扫描游标,返回句柄。
    pub fn open_lazy(&mut self, cursor: LazyCursor, transaction_epoch: Option<u64>) -> u64 {
        let handle = self.next;
        self.next = self.next.wrapping_add(1);
        self.cursors.insert(handle, Cursor { rows: CursorRows::Lazy(cursor), transaction_epoch });
        handle
    }

    /// 从游标读取至多 `limit` 行,返回本窗口与是否已到末尾。
    ///
    /// 未知句柄以 `cursor-not-found` 拒绝,事务已结束的句柄以
    /// `cursor-expired` 拒绝。惰性来源经 `lazy_next` 回调推进,回调由持有
    /// 执行引擎的调用方提供,游标状态机本身不拥有引擎。
    pub fn next<F>(&mut self, handle: u64, limit: usize, lazy_next: F) -> Result<CursorBatch, SqlError>
    where
        F: FnOnce(&mut LazyCursor, usize) -> Result<LazyBatch, SqlError>,
    {
        let epoch = self.epoch;
        let cursor = self.cursors.get_mut(&handle).ok_or_else(|| SqlError::new("cursor-not-found", format!("cursor {handle} is not open")))?;
        if let Some(opened) = cursor.transaction_epoch
            && opened != epoch
        {
            return Err(SqlError::new("cursor-expired", "cursor was opened inside a transaction that has already ended"));
        }
        match &mut cursor.rows {
            CursorRows::Materialized { rows, position } => {
                let remaining = rows.len() - *position;
                let end = *position + limit.min(remaining);
                let window = rows[*position..end].to_vec();
                *position = end;
                Ok(CursorBatch { rows: window, done: *position == rows.len() })
            }
            CursorRows::Lazy(lazy) => {
                let batch = lazy_next(lazy, limit)?;
                Ok(CursorBatch { rows: batch.rows, done: batch.done })
            }
        }
    }

    /// 释放一个游标。未知句柄视为已释放,便于宿主在 `finally` 里安全清理。
    pub fn close(&mut self, handle: u64) {
        self.cursors.remove(&handle);
    }

    /// 当前仍持有的游标数量(测试与诊断用)。
    pub fn len(&self) -> usize {
        self.cursors.len()
    }

    /// 是否没有打开的游标。
    pub fn is_empty(&self) -> bool {
        self.cursors.is_empty()
    }

    /// 丢弃全部游标,用于关闭连接。
    pub fn clear(&mut self) {
        self.cursors.clear();
    }
}
