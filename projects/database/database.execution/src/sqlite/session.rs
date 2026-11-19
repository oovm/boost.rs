//! SQLite 连接级会话状态机:脏镜像跟踪、事务代数、发布边界判定与游标表。
//!
//! 本模块是 SQLite 会话状态的 owner(22 号盘点 A4 第二批):`dirty` 跟踪、
//! 事务 epoch 前进、`after_statement` 的发布边界判定(autocommit 写、
//! `COMMIT`、`ROLLBACK`、最外层 `RELEASE`)、游标表与游标执行方式选择
//! (惰性扫描或打开时物化)都在这里。产品门面只保留公开对象、选项映射、
//! 写锁与镜像恢复的资源组装和转调,不再持有连接状态字段。
//!
//! 不新建第二套状态机:事务与 savepoint 状态仍由 [`crate::sqlite::txn`]
//! 机器维护在 [`Engine`] 的连接级字段里,代数与游标失效直接复用
//! [`crate::sqlite::cursor::CursorTable`] 的既有 epoch。`engine::connection`
//! 的既有状态机面向 VOS 记录型引擎(按记录键的 `TxnJournal` 与快照发布),
//! 与 SQLite 整镜像 dirty/发布边界模型不是同一适用模型,不合并、不借用。
//! 物理发布与恢复归 `database.storage::sqlite::{journal, lock}`,本模块只
//! 决定何时调用,不重实现落盘机制。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use database_query::sqlite::{
    SqlError,
    frontend::{Statement, TransactionAction, parameter_slots},
    value::Cell,
};
use database_storage::sqlite::PublishMode;
use database_udf::{HostMicroDefinition, HostRuntimeAdapter};

use crate::sqlite::{
    cursor::{CursorBatch, CursorOpen, CursorTable},
    engine::{Engine, ExecResult},
};

/// 一个 SQLite 连接的会话状态:引擎句柄、文件归属、脏镜像标记、
/// 关闭标记、发布模式与游标表。
pub struct SqliteSession {
    /// 底层执行引擎,持有写者与目录;事务状态也在引擎的连接级字段里。
    engine: Engine,
    /// 落盘目标;`None` 为纯内存或字节镜像连接。
    filename: Option<PathBuf>,
    /// 只读标志:任何会改变镜像的语句在执行前即被拒绝。
    read_only: bool,
    /// 镜像自上次落盘后是否发生过变化。
    dirty: bool,
    /// [`SqliteSession::close`] 成功之后不再接受语句。
    closed: bool,
    /// 提交时存储层的发布模式;公开的日志模式选项映射由门面持有。
    publish_mode: PublishMode,
    /// 本会话打开的结果游标表,以句柄为键,代数标记随事务结束前进。
    cursors: CursorTable,
}

impl SqliteSession {
    /// 以装配完成的引擎与会话归属建立连接级会话。
    ///
    /// 引擎的构造(读文件、回放热日志、取写锁)是资源组装,留在门面或
    /// 存储层入口;本构造器只接管之后的会话状态。
    pub fn new(engine: Engine, filename: Option<PathBuf>, read_only: bool, publish_mode: PublishMode) -> Self {
        Self { engine, filename, read_only, dirty: false, closed: false, publish_mode, cursors: CursorTable::new() }
    }

    /// 按本会话的宿主函数表解析一段脚本。
    pub fn parse(&self, sql: &str) -> Result<Vec<Statement>, SqlError> {
        self.engine.parse(sql)
    }

    /// prepare 时刻由引擎目录推导出的结果列标签。
    ///
    /// 目录不足以在不执行的前提下确定表头(缺表、缺列等)时如实返回
    /// `None`,不提前拒绝;错误留给执行期按既有语义抛出。
    pub fn describe(&self, statement: &Statement) -> Option<Vec<String>> {
        self.engine.describe(statement).ok().flatten()
    }

    /// 执行一条已解析语句:只读拒绝、参数检查、执行、代数前进与提交边界。
    pub fn run(&mut self, statement: &Statement, parameters: &[Cell]) -> Result<ExecResult, SqlError> {
        self.guard_open()?;
        let is_write = statement.writes();
        self.guard_write(is_write)?;
        let required = parameter_slots(statement).last().map(|(index, _)| index + 1).unwrap_or(0);
        if parameters.len() < required {
            return Err(SqlError::new("bind-parameter", format!("statement expects {required} parameters, got {}", parameters.len()))
                .in_statement(statement.name()));
        }
        let was_in_transaction = self.engine.in_transaction();
        let result = self.engine.execute_statement(statement, parameters)?;
        self.advance_epoch_if_transaction_ended(was_in_transaction);
        self.after_statement(is_write, statement)?;
        Ok(result)
    }

    /// 执行一段分号分隔的脚本,逐条语句应用只读拒绝与提交边界。
    pub fn execute(&mut self, sql: &str, parameters: &[Cell]) -> Result<Vec<ExecResult>, SqlError> {
        self.guard_open()?;
        let statements = self.engine.parse(sql)?;
        let mut results = Vec::with_capacity(statements.len());
        for statement in statements {
            results.push(self.run(&statement, parameters)?);
        }
        Ok(results)
    }

    /// 打开一条结果游标并选择执行方式。
    ///
    /// 简单单表扫描在打开时只固定一份整页快照,后续 [`SqliteSession::cursor_next`]
    /// 按 b-tree 页顺序分批解码;其余语句与 [`SqliteSession::run`] 共用同一条
    /// 执行路径与提交边界,结果集在打开时一次性物化。事务内打开的游标记录
    /// 当前事务代数,事务结束后读取以 `cursor-expired` 拒绝。
    pub fn open_cursor(&mut self, statement: &Statement, parameters: &[Cell]) -> Result<CursorOpen, SqlError> {
        self.guard_open()?;
        let is_write = statement.writes();
        self.guard_write(is_write)?;
        let required = parameter_slots(statement).last().map(|(index, _)| index + 1).unwrap_or(0);
        if parameters.len() < required {
            return Err(SqlError::new("bind-parameter", format!("statement expects {required} parameters, got {}", parameters.len()))
                .in_statement(statement.name()));
        }
        let transaction_epoch = self.engine.in_transaction().then_some(self.cursors.epoch());
        if let Some(lazy) = self.engine.open_lazy_cursor(statement, parameters)? {
            let columns = lazy.columns().to_vec();
            let handle = self.cursors.open_lazy(lazy, transaction_epoch);
            return Ok(CursorOpen { handle, columns, lazy: true });
        }
        let was_in_transaction = self.engine.in_transaction();
        let ExecResult { columns, rows, .. } = self.engine.execute_statement(statement, parameters)?;
        self.advance_epoch_if_transaction_ended(was_in_transaction);
        self.after_statement(is_write, statement)?;
        let transaction_epoch = self.engine.in_transaction().then_some(self.cursors.epoch());
        let handle = self.cursors.open_materialized(rows, transaction_epoch);
        Ok(CursorOpen { handle, columns, lazy: false })
    }

    /// 从游标读取至多 `limit` 行;惰性来源经引擎的页扫描推进。
    pub fn cursor_next(&mut self, handle: u64, limit: usize) -> Result<CursorBatch, SqlError> {
        self.guard_open()?;
        let Self { cursors, engine, .. } = self;
        cursors.next(handle, limit, |lazy, limit| engine.lazy_cursor_next(lazy, limit))
    }

    /// 释放一个游标。未知句柄视为已释放,便于宿主在 `finally` 里安全清理。
    pub fn cursor_close(&mut self, handle: u64) -> Result<(), SqlError> {
        self.guard_open()?;
        self.cursors.close(handle);
        Ok(())
    }

    /// 当前会话仍持有的游标数量(测试与诊断用)。
    pub fn cursor_count(&self) -> usize {
        self.cursors.len()
    }

    /// 当前数据库镜像,可直接写成 `.sqlite3` 文件。
    pub fn snapshot(&mut self) -> Result<&[u8], SqlError> {
        self.engine.snapshot()
    }

    /// 镜像头里的 format-3 库版本。
    pub fn sqlite_version(&self) -> Result<String, SqlError> {
        self.engine.sqlite_version()
    }

    /// 注册一个宿主自定义标量函数,语义见 [`Engine::register_host_function`]。
    pub fn register_host_function(&mut self, definition: HostMicroDefinition, adapter: Arc<dyn HostRuntimeAdapter>) -> Result<(), SqlError> {
        self.engine.register_host_function(definition, adapter)
    }

    /// 把当前镜像完整写入 `path`,即真正的备份。
    ///
    /// 只读会话允许备份:它读取镜像、写出到另一个文件,不改变本会话。
    pub fn backup(&mut self, path: &Path) -> Result<(), SqlError> {
        self.engine.save(path)
    }

    /// 关闭会话:先完成发布,再改关闭状态并丢弃游标。
    ///
    /// 有已提交未落盘的改动时,发布在关闭状态改变之前执行;发布失败则
    /// 原样传播错误,会话保持未关闭、`dirty` 保留,资源状态可解释
    /// (未发布、脏镜像仍在),调用方修复环境后可以重试。事务未结束时
    /// 不落盘,与 SQLite 在关闭时回滚未提交事务的行为一致。再次调用
    /// [`SqliteSession::close`] 为空操作。
    pub fn close(&mut self) -> Result<(), SqlError> {
        if self.closed {
            return Ok(());
        }
        if self.dirty && !self.engine.in_transaction() {
            self.publish()?;
            self.dirty = false;
        }
        self.closed = true;
        self.cursors.clear();
        Ok(())
    }

    /// 是否有显式事务或保存点作用域打开。
    pub fn in_transaction(&self) -> bool {
        self.engine.in_transaction()
    }

    /// 本会话最近一次成功插入的 rowid。
    pub fn last_insert_rowid(&self) -> i64 {
        self.engine.last_insert_rowid()
    }

    /// 配置的页大小。
    pub fn page_size(&self) -> u32 {
        self.engine.page_size()
    }

    /// 会话是否为只读。
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// 落盘目标路径。
    pub fn filename(&self) -> Option<&Path> {
        self.filename.as_deref()
    }

    /// `PRAGMA synchronous` 透传给写者。
    pub fn set_synchronous(&mut self, on: bool) {
        self.engine.set_synchronous(on);
    }

    /// 镜像是否存在已提交但尚未落盘(或落盘失败)的改动。
    ///
    /// 诊断入口:发布失败后资源状态必须可解释,调用方据此区分
    /// 「已发布」与「未发布、脏镜像保留」。
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// 会话是否已关闭。
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// 只读会话拒绝任何会写镜像的语句,在执行前拒绝。
    fn guard_write(&self, is_write: bool) -> Result<(), SqlError> {
        if self.read_only && is_write {
            return Err(SqlError::new("read-only", "cannot modify a read-only database"));
        }
        Ok(())
    }

    /// 已关闭的会话不再接受语句。
    fn guard_open(&self) -> Result<(), SqlError> {
        if self.closed {
            return Err(SqlError::new("db-closed", "database connection is closed"));
        }
        Ok(())
    }

    /// 成功执行一条语句后应用提交边界。
    ///
    /// 写语句弄脏镜像;只要显式事务没有打开(autocommit、`COMMIT` 之后、
    /// 最外层 `RELEASE` 之后),脏镜像立即由存储层落盘。发布失败时错误
    /// 原样传播、脏标志保留,下一条语句边界会重试落盘,提交故障不会先
    /// 宣告成功。`ROLLBACK` 把镜像恢复到上次落盘的状态,脏标志随之清除。
    fn after_statement(&mut self, is_write: bool, statement: &Statement) -> Result<(), SqlError> {
        if is_write {
            self.dirty = true;
        }
        if matches!(statement, Statement::Transaction { action: TransactionAction::Rollback }) {
            self.dirty = false;
        }
        if self.dirty && !self.engine.in_transaction() {
            self.publish()?;
            self.dirty = false;
        }
        Ok(())
    }

    /// 事务结束时让代数前进,使事务内打开的游标失效。
    fn advance_epoch_if_transaction_ended(&mut self, was_in_transaction: bool) {
        self.cursors.advance_if_transaction_ended(was_in_transaction, self.engine.in_transaction());
    }

    /// 把当前镜像写到本会话的文件。纯内存会话为空操作。
    ///
    /// 提交发行策略归 `database.storage::sqlite`;会话只按持有的
    /// [`PublishMode`] 转调。
    pub fn publish(&mut self) -> Result<(), SqlError> {
        let Some(path) = self.filename.clone()
        else {
            return Ok(());
        };
        self.engine.publish(&path, self.publish_mode)
    }
}
