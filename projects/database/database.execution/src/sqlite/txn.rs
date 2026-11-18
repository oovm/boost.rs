//! SQLite 门面的事务与 savepoint 机器。
//!
//! 事务状态类型与 `BEGIN` / `COMMIT` / `ROLLBACK` / `SAVEPOINT` / `RELEASE` /
//! `ROLLBACK TO` 的变更语义 owner 在本模块(C9b):`from_begin` 与 savepoint 栈
//! 决定提交门与回滚范围,deferred 外键校验在 `COMMIT` 与「释放最后一个
//! savepoint」两处落点先行执行,快照层级协调直接借用
//! `database.storage::sqlite::writer` 的 `begin` / `commit_all` / `rollback_all` /
//! `savepoint` / `release` / `rollback_to` 原语,不在这里重实现。状态实例仍存放在
//! 门面 `Engine` 的连接级字段里,机器经 [`StmtContext`] 的事务借用运行;门面只保留
//! `ATTACH` / `DETACH` 与 `VACUUM` 的引擎集合编排。本模块不得反向依赖任何产品门面。

use database_query::sqlite::{SqlError, catalog::*, frontend::TransactionAction, storage_failure};
use database_storage::sqlite::SqliteWriter;

use super::{
    ddl,
    stmt::{ExecResult, StmtContext},
};

/// Explicit transaction bookkeeping, mirroring the writer's snapshot levels.
pub struct TransactionState {
    /// `BEGIN` opened the transaction rather than a bare `SAVEPOINT`.
    pub from_begin: bool,
    /// Open savepoint names, outermost first.
    pub savepoints: Vec<String>,
}

/// Rebuilds the in-memory catalog after storage rolled back, so schema
/// changes undone by the rollback also disappear from later statements.
pub fn reload_catalog(writer: &SqliteWriter, catalog: &mut SqliteCatalog) -> Result<(), SqlError> {
    catalog.tables.clear();
    catalog.table_lookup.clear();
    catalog.indexes.clear();
    catalog.views.clear();
    catalog.triggers.clear();
    ddl::load_catalog(writer, catalog)
}

impl StmtContext<'_> {
    /// Applies one explicit transaction control statement. Every scope keeps a
    /// writer snapshot level, so statements stay atomic inside a wider
    /// transaction and a rollback only undoes the scope it names.
    pub fn apply_transaction(&mut self, action: TransactionAction) -> Result<ExecResult, SqlError> {
        let statement = action_name(&action);
        match action {
            TransactionAction::Begin => {
                if self.transaction.is_some() {
                    return Err(SqlError::new("transaction-active", "cannot start a transaction within a transaction").in_statement("begin"));
                }
                self.writer.begin().map_err(|error| storage_failure(error).in_statement("begin"))?;
                for attached in &mut self.attached {
                    attached.ctx.writer.begin().map_err(|error| storage_failure(error).in_statement("begin"))?;
                }
                *self.transaction = Some(TransactionState { from_begin: true, savepoints: Vec::new() });
            }
            TransactionAction::Commit => {
                if self.transaction.is_none() {
                    return Err(SqlError::new("transaction-missing", "cannot commit - no transaction is active").in_statement("commit"));
                }
                self.exec().check_deferred_foreign_keys(self.foreign_keys)?;
                for attached in &self.attached {
                    attached.ctx.exec().check_deferred_foreign_keys(attached.ctx.foreign_keys)?;
                }
                // 推迟的索引重建留到读取镜像的入口再落下:提交本身不读镜像,
                // 在这里重建会让每个写事务都付一次整树重建成本。
                self.transaction.take();
                self.writer.commit_all().map_err(|error| storage_failure(error).in_statement("commit"))?;
                for attached in &mut self.attached {
                    attached.ctx.writer.commit_all().map_err(|error| storage_failure(error).in_statement("commit"))?;
                }
                self.persist_attached_databases()?;
            }
            TransactionAction::Rollback => {
                if self.transaction.take().is_none() {
                    return Err(SqlError::new("transaction-missing", "cannot rollback - no transaction is active").in_statement("rollback"));
                }
                self.writer.rollback_all().map_err(|error| storage_failure(error).in_statement("rollback"))?;
                // 回滚把内存键集退回事务前,但推迟标记可能来自事务开始之前
                // 的 autocommit 写入,这里只保留标记,不因回滚丢弃。
                reload_catalog(self.writer, self.catalog)?;
                for index in 0..self.attached.len() {
                    let attached = &mut self.attached[index];
                    attached.ctx.writer.rollback_all().map_err(|error| storage_failure(error).in_statement("rollback"))?;
                    *attached.dirty = false;
                    reload_catalog(&*attached.ctx.writer, &mut attached.ctx.catalog)?;
                }
            }
            TransactionAction::Savepoint(name) => {
                if self.transaction.is_none() {
                    // A savepoint outside `BEGIN` opens its own transaction.
                    self.writer.begin().map_err(|error| storage_failure(error).in_statement("savepoint"))?;
                    for attached in &mut self.attached {
                        attached.ctx.writer.begin().map_err(|error| storage_failure(error).in_statement("savepoint"))?;
                    }
                    *self.transaction = Some(TransactionState { from_begin: false, savepoints: Vec::new() });
                }
                self.writer.savepoint(&name).map_err(|error| storage_failure(error).in_statement("savepoint"))?;
                for attached in &mut self.attached {
                    attached.ctx.writer.savepoint(&name).map_err(|error| storage_failure(error).in_statement("savepoint"))?;
                }
                self.transaction.as_mut().expect("savepoint opened a transaction").savepoints.push(name);
            }
            TransactionAction::Release(name) => {
                let index = self.savepoint_position(&name, "release")?;
                let closing = {
                    let state = self.transaction.as_ref().expect("an open savepoint implies a transaction");
                    index == 0 && !state.from_begin
                };
                if closing {
                    // Releasing the last savepoint of a transaction that no
                    // `BEGIN` opened commits it, so the commit gate runs
                    // first: deferred foreign keys must pass before any
                    // savepoint state is destroyed, and a failed check leaves
                    // the transaction open for a repair or a rollback.
                    self.exec().check_deferred_foreign_keys(self.foreign_keys)?;
                    for attached in &self.attached {
                        attached.ctx.exec().check_deferred_foreign_keys(attached.ctx.foreign_keys)?;
                    }
                }
                self.writer.release(&name).map_err(|error| storage_failure(error).in_statement("release"))?;
                for attached in &mut self.attached {
                    attached.ctx.writer.release(&name).map_err(|error| storage_failure(error).in_statement("release"))?;
                }
                let closing = {
                    let state = self.transaction.as_mut().expect("an open savepoint implies a transaction");
                    state.savepoints.truncate(index);
                    state.savepoints.is_empty() && !state.from_begin
                };
                if closing {
                    // Releasing the last savepoint of a transaction that no
                    // `BEGIN` opened commits it, as in SQLite. 索引重建同样
                    // 推迟到读取镜像的入口。
                    self.writer.commit_all().map_err(|error| storage_failure(error).in_statement("release"))?;
                    *self.transaction = None;
                    self.persist_attached_databases()?;
                }
            }
            TransactionAction::RollbackTo(name) => {
                let index = self.savepoint_position(&name, "rollback-to")?;
                self.writer.rollback_to(&name).map_err(|error| storage_failure(error).in_statement("rollback-to"))?;
                reload_catalog(self.writer, self.catalog)?;
                for index in 0..self.attached.len() {
                    let attached = &mut self.attached[index];
                    attached.ctx.writer.rollback_to(&name).map_err(|error| storage_failure(error).in_statement("rollback-to"))?;
                    reload_catalog(&*attached.ctx.writer, &mut attached.ctx.catalog)?;
                }
                self.transaction.as_mut().expect("an open savepoint implies a transaction").savepoints.truncate(index + 1);
            }
        }
        Ok(ExecResult { statement, columns: Vec::new(), rows: Vec::new(), affected: 0 })
    }

    /// Finds an open savepoint by name and reports SQLite's missing-savepoint
    /// error when it is absent. Names are case insensitive.
    fn savepoint_position(&self, name: &str, statement: &'static str) -> Result<usize, SqlError> {
        let missing = || SqlError::new("savepoint-missing", format!("no such savepoint: {name}")).in_statement(statement);
        let state = self.transaction.as_ref().ok_or_else(missing)?;
        state.savepoints.iter().rposition(|saved| saved.eq_ignore_ascii_case(name)).ok_or_else(missing)
    }

    /// Flushes every attached database that changed inside a transaction.
    fn persist_attached_databases(&mut self) -> Result<(), SqlError> {
        for index in 0..self.attached.len() {
            self.persist_attached(index)?;
        }
        Ok(())
    }
}

/// Statement name reported for one transaction action, matching
/// `Statement::name`.
fn action_name(action: &TransactionAction) -> &'static str {
    match action {
        TransactionAction::Begin => "begin",
        TransactionAction::Commit => "commit",
        TransactionAction::Rollback => "rollback",
        TransactionAction::Savepoint(_) => "savepoint",
        TransactionAction::Release(_) => "release",
        TransactionAction::RollbackTo(_) => "rollback-to",
    }
}
