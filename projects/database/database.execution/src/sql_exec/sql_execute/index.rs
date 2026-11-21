//! 单列二级索引的增量维护（唯一约束与普通索引共用）。
//!
//! 索引条目由 DML 路径与行写入放进**同一个提交边界**：自动提交走
//! `commit_batch`，事务内走 handle 的写入缓冲。因此回滚、savepoint 与崩溃恢复看到的
//! 都是「行与条目一起在或一起不在」。
//!
//! 索引 namespace 里的 [`database_query::sql_index::READY_KEY`] 是完备标记：
//! - 建表后第一次写入（含旧数据目录）由 `backfill` 从全表快照补齐条目并写标记；
//! - 只要有一行的值无法编码，标记不会写（已存在则删除），读路径退回整表扫描。
//!
//! 标记缺失时读路径绝不使用索引，因此「索引不全」只会让查询变慢，不会漏行。
//!
//! 条目键把 `rowid` 放在键尾（见 [`database_query::sql_index`]），所以非唯一索引的
//! 同一列值可以有多行，不会互相覆盖。
//!
//! 语句内的命令分三段发出：回填 → 删除旧条目 → 写入新条目。唯一键整体平移
//! （`uk = uk + 1`）时，必须先清掉所有旧值再写新值，否则后一行的删除会抹掉前一行
//! 刚写好的条目。

use super::*;

/// 一个需要维护的单列索引。
pub(crate) struct SecondaryIndex {
    namespace: String,
    field_id: u64,
    field_type: database_query::catalog_bind::SqlFieldType,
}

/// 一行在某个索引上的条目状态。
enum EntryState {
    /// 列值是 SQL `NULL`：索引不为 NULL 写条目，等值探测也匹配不到 NULL。
    Null,
    /// 可编码的 payload（尚未拼上 rowid）。
    Payload(Vec<u8>),
    /// 值无法按列类型编码，索引必须整体退回不可用。
    Unusable,
}

/// 一次 DML 语句里共享的索引维护状态。
pub(crate) struct IndexMaintenance {
    indexes: Vec<SecondaryIndex>,
    /// 所有索引都有完备标记。
    ready: bool,
    /// 这张表是否有需要维护的单列索引。
    configured: bool,
    /// 本次语句里出现过无法编码的值，索引必须停用。
    broken: bool,
    /// 回填命令（只在标记缺失时产生）。
    backfill: Vec<KeyValueCommand>,
    /// 要摘除的旧条目。
    deletes: Vec<KeyValueCommand>,
    /// 要写入的新条目。
    puts: Vec<KeyValueCommand>,
}

impl IndexMaintenance {
    /// 登记本次写入涉及的单列索引，并一次性读取它们的完备标记。
    ///
    /// `constraints` 同时包含唯一约束与普通索引：唯一性由写入前的约束校验负责，
    /// 索引维护只关心「哪些列需要条目」。
    pub(crate) fn open(
        executor: &dyn KeyValueExecutor,
        transaction: Option<&dyn TransactionHandle>,
        namespace: &str,
        constraints: &[UniqueConstraintPlan],
        field_types: &BTreeMap<u64, database_query::catalog_bind::SqlFieldType>,
    ) -> Result<Self, SqlFrontendError> {
        let indexes: Vec<SecondaryIndex> = constraints
            .iter()
            .filter_map(|constraint| {
                let [(field_id, _)] = constraint.columns.as_slice()
                else {
                    return None;
                };
                let field_type = field_types.get(field_id)?.clone();
                if !database_query::sql_index::index_supported(&field_type) {
                    return None;
                }
                Some(SecondaryIndex {
                    namespace: database_query::sql_index::index_namespace(namespace, *field_id),
                    field_id: *field_id,
                    field_type,
                })
            })
            .collect();
        if indexes.is_empty() {
            return Ok(Self::idle(indexes));
        }
        let commands: Vec<KeyValueCommand> = indexes
            .iter()
            .map(|index| KeyValueCommand {
                namespace: Namespace(index.namespace.clone()),
                key: database_query::sql_index::READY_KEY.to_vec(),
                action: KeyValueAction::Get,
            })
            .collect();
        let results = read_batch(executor, transaction, &commands)?;
        let ready = results.iter().all(|result| matches!(result, database_query::cluster_kv::KeyValueResult::Get(Some(_))));
        Ok(Self { ready, configured: true, ..Self::idle(indexes) })
    }

    fn idle(indexes: Vec<SecondaryIndex>) -> Self {
        Self { indexes, ready: false, configured: false, broken: false, backfill: Vec::new(), deletes: Vec::new(), puts: Vec::new() }
    }

    /// 索引条目是否可以读写。
    pub(crate) fn is_active(&self) -> bool {
        self.configured && self.ready && !self.broken
    }

    /// 索引已登记但还没有完备标记，调用方应提供全表快照调用 [`Self::backfill`]。
    pub(crate) fn needs_backfill(&self) -> bool {
        self.configured && !self.ready && !self.broken
    }

    /// 用全表快照补齐条目并写完备标记。
    ///
    /// `stored` 是整张表的 `(rowid, row bytes)` 快照。任一值无法编码时索引保持不可用
    /// ——宁可不走索引，也不能让索引成为结果集的过滤条件。
    pub(crate) fn backfill(&mut self, stored: &[(Vec<u8>, Vec<u8>)]) -> Result<(), SqlFrontendError> {
        let field_ids: Vec<u64> = self.indexes.iter().map(|index| index.field_id).collect();
        let mut commands = Vec::new();
        for (rowid, bytes) in stored {
            let row = decode_row_fields(bytes, &field_ids)?;
            for index in &self.indexes {
                match entry_state(index, &row) {
                    EntryState::Payload(payload) => commands.push(index_put(&index.namespace, &payload, rowid)),
                    EntryState::Null => {}
                    EntryState::Unusable => {
                        self.deactivate();
                        return Ok(());
                    }
                }
            }
        }
        for index in &self.indexes {
            commands.push(index_put_ready(&index.namespace));
        }
        self.ready = true;
        self.backfill = commands;
        Ok(())
    }

    /// 登记一行的索引条目（新写入或改写后的行）。
    pub(crate) fn remember_insert(&mut self, row: &RowFieldValues, rowid: &[u8]) {
        if !self.is_active() {
            return;
        }
        for index in &self.indexes {
            match entry_state(index, row) {
                EntryState::Payload(payload) => self.puts.push(index_put(&index.namespace, &payload, rowid)),
                EntryState::Null => {}
                EntryState::Unusable => {
                    self.deactivate();
                    return;
                }
            }
        }
    }

    /// 登记一行的索引条目摘除（删除或改写前的旧行）。
    ///
    /// `rowid` 是旧行的主键：非唯一索引的条目键含 `rowid`，缺了它就删不到正确条目。
    pub(crate) fn remember_remove(&mut self, row: Option<&RowFieldValues>, rowid: &[u8]) {
        if !self.is_active() {
            return;
        }
        let Some(row) = row
        else {
            return;
        };
        for index in &self.indexes {
            match entry_state(index, row) {
                EntryState::Payload(payload) => self.deletes.push(index_delete(&index.namespace, &payload, rowid)),
                EntryState::Null => {}
                EntryState::Unusable => {
                    self.deactivate();
                    return;
                }
            }
        }
    }

    /// 改写一行：旧条目进删除段，新条目进写入段。
    pub(crate) fn remember_replace(&mut self, old: Option<&RowFieldValues>, new: &RowFieldValues, rowid: &[u8]) {
        self.remember_remove(old, rowid);
        self.remember_insert(new, rowid);
    }

    /// 取出本次语句要提交的全部索引命令，按「回填 → 删除 → 写入」排好序。
    ///
    /// 回填段保留在自身缓存里：提交边界因并发冲突被拒绝并重排时，同一条语句会再次
    /// 取命令，回填不能只发一次。
    pub(crate) fn take_commands(&mut self) -> Vec<KeyValueCommand> {
        let mut commands = self.backfill.clone();
        commands.append(&mut self.deletes);
        commands.append(&mut self.puts);
        commands
    }

    /// 值无法编码时摘掉完备标记，让所有读路径退回整表扫描。
    fn deactivate(&mut self) {
        self.broken = true;
        self.deletes.extend(self.indexes.iter().map(|index| KeyValueCommand {
            namespace: Namespace(index.namespace.clone()),
            key: database_query::sql_index::READY_KEY.to_vec(),
            action: KeyValueAction::Delete,
        }));
    }
}

fn entry_state(index: &SecondaryIndex, row: &RowFieldValues) -> EntryState {
    match row.value_of(index.field_id, &index.field_type) {
        None => EntryState::Null,
        Some(value) => match database_query::sql_index::encode_value(&value, &index.field_type) {
            Ok(Some(payload)) => EntryState::Payload(payload),
            Ok(None) => EntryState::Null,
            Err(()) => EntryState::Unusable,
        },
    }
}

fn index_put(namespace: &str, payload: &[u8], rowid: &[u8]) -> KeyValueCommand {
    KeyValueCommand {
        namespace: Namespace(namespace.to_string()),
        key: database_query::sql_index::entry_key(payload, rowid),
        action: KeyValueAction::Put(Vec::new()),
    }
}

fn index_put_ready(namespace: &str) -> KeyValueCommand {
    KeyValueCommand {
        namespace: Namespace(namespace.to_string()),
        key: database_query::sql_index::READY_KEY.to_vec(),
        action: KeyValueAction::Put(Vec::new()),
    }
}

fn index_delete(namespace: &str, payload: &[u8], rowid: &[u8]) -> KeyValueCommand {
    KeyValueCommand {
        namespace: Namespace(namespace.to_string()),
        key: database_query::sql_index::entry_key(payload, rowid),
        action: KeyValueAction::Delete,
    }
}

/// 在事务里按顺序写入索引命令；写入缓冲保证 ROLLBACK / savepoint 一起丢弃。
///
/// 自动提交路径不能逐条 `execute`——那会把一条语句拆成多个提交点，所以调用方必须
/// 在 `None` 时把命令并入 `commit_batch`，这里不做静默降级。
pub(crate) fn apply_index_commands_in_transaction(
    transaction: &dyn TransactionHandle,
    commands: &[KeyValueCommand],
) -> Result<(), SqlFrontendError> {
    for command in commands {
        transaction.execute(command.clone()).map_err(|e| internal_error(&format!("TransactionHandle.index_write failed: {e}")))?;
    }
    Ok(())
}

/// 全表快照 → `(rowid, bytes)`，供索引回填使用。
pub(crate) fn index_snapshot(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    namespace: &Namespace,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, SqlFrontendError> {
    scan_rows(executor, transaction, namespace, &[], None, None, 0, "index_backfill")
}
