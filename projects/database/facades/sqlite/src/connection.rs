//! 连接层:公开对象、选项映射与资源组装,会话状态委托执行层。
//!
//! [`Connection`] 把宿主(N-API、PyO3、WebAssembly)需要的权威信息映射成
//! 各自的 JS/Python 形状。本层不再持有连接状态:
//!
//! - 语句切分与词法来自 [`crate::sql::parse`],宿主不再自己扫分号;
//! - 参数槽位来自 [`crate::sql::parameter_slots`],宿主不再自己数 `?`;
//! - 读写判定来自 [`crate::sql::Statement::writes`],宿主不再用关键词猜;
//! - 脏镜像跟踪、事务代数、提交边界判定(autocommit 写、`COMMIT`、
//!   `ROLLBACK`、最外层 `RELEASE`)与游标表归
//!   `database_execution::sqlite::session`,本连接只转调;
//! - 文件落盘只发生在提交边界,物理发布与恢复归
//!   `database_storage::sqlite::{journal, lock}`,由存储层入口完成。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crate::{
    SqliteWriter,
    sql::{Cell, Engine, ExecResult, SqlError, Statement, parameter_slots, storage_failure},
};

use database_execution::sqlite::session::SqliteSession;
use database_storage::sqlite::{
    OpenWritableError, OpenWritableOptions, PublishMode, WriterLock, journal, lock::LockError, open_writable,
};
use database_udf::{HostMicroDefinition, HostRuntimeAdapter};

pub use database_execution::sqlite::cursor::{CursorBatch, CursorOpen};

/// 把存储层的锁拒绝映射成结构化的 SQL 拒绝。
fn lock_error(error: LockError) -> SqlError {
    match error {
        LockError::Busy { .. } => SqlError::new("busy", error.to_string()),
        LockError::Open { .. } | LockError::Lock { .. } => SqlError::new("storage", error.to_string()),
    }
}

/// 把存储层的打开拒绝映射成结构化的 SQL 拒绝。
fn open_error(error: OpenWritableError, path: &Path) -> SqlError {
    match error {
        OpenWritableError::Lock(inner) => lock_error(inner),
        OpenWritableError::Missing => not_found(path),
        OpenWritableError::Storage(inner) => storage_failure(inner),
    }
}

fn not_found(path: &Path) -> SqlError {
    SqlError::new("not-found", format!("database file `{}` does not exist", path.display()))
}

/// 提交时发布新镜像并处理回滚日志的方式。
///
/// `Delete` 与 `Truncate` 在替换主文件前写出真实格式的 rollback journal:
/// 提交中断后,重新打开(本绑定或外部 SQLite 引擎)会回滚到旧镜像。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum JournalMode {
    /// 只做原子改名发布,不生成回滚日志(本绑定的默认行为)。
    #[default]
    Preserve,
    /// 发布前写 `<db>-journal`,发布成功后删除该文件。
    Delete,
    /// 发布前写 `<db>-journal`,发布成功后把它截断为 0 字节。
    Truncate,
}

impl JournalMode {
    /// 解析 `journalMode` 名称。
    ///
    /// `preserve`、`delete`、`truncate` 之外(含 `wal`)一律以 `unsupported`
    /// 拒绝,不静默降级。
    pub fn parse(name: &str) -> Result<Self, SqlError> {
        match name.trim().to_ascii_lowercase().as_str() {
            "preserve" => Ok(Self::Preserve),
            "delete" => Ok(Self::Delete),
            "truncate" => Ok(Self::Truncate),
            other => Err(SqlError::new("unsupported", format!("journalMode `{other}` is not supported (use preserve|delete|truncate)"))),
        }
    }

    /// 选项名称,与 [`JournalMode::parse`] 互逆。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preserve => "preserve",
            Self::Delete => "delete",
            Self::Truncate => "truncate",
        }
    }

    /// 存储层发布本合同对应的模式。
    fn publish_mode(self) -> PublishMode {
        match self {
            Self::Preserve => PublishMode::Atomic,
            Self::Delete => PublishMode::JournalDelete,
            Self::Truncate => PublishMode::JournalTruncate,
        }
    }
}

/// 打开连接的方式。
#[derive(Clone, Debug)]
pub struct ConnectionOptions {
    /// 数据库文件路径;`None` 表示纯内存连接。
    pub filename: Option<PathBuf>,
    /// 以字节镜像打开,优先于读取 `filename`。
    pub data: Option<Vec<u8>>,
    /// 只读:任何会改变镜像的语句在执行前即被拒绝。
    pub read_only: bool,
    /// 文件不存在时是否创建;只读连接隐含不创建。
    pub create: bool,
    /// 新建数据库的页大小,`None` 用 4096。
    pub page_size: Option<u32>,
    /// `PRAGMA synchronous`;`false` 时落盘跳过持久化刷新。
    pub synchronous: bool,
    /// 提交时如何发布新镜像与处理回滚日志。
    pub journal_mode: JournalMode,
    /// 写连接在锁被另一个写连接占用时的重试时长(毫秒),`0` 表示只尝试一次。
    ///
    /// 只读连接、纯内存连接与字节镜像连接不参与加锁,该值对它们无意义。
    pub busy_timeout_ms: u64,
}

impl Default for ConnectionOptions {
    fn default() -> Self {
        Self {
            filename: None,
            data: None,
            read_only: false,
            create: true,
            page_size: None,
            synchronous: true,
            journal_mode: JournalMode::Preserve,
            busy_timeout_ms: 0,
        }
    }
}

/// 一条已经解析好的语句及其权威元数据。
///
/// 解析只发生一次:`run` 反复执行同一 AST,不再重新词法。槽位、读写分类都
/// 来自 AST,与执行时绑定的槽位永远一致。
#[derive(Debug)]
pub struct Prepared {
    /// 提交的 SQL 原文。
    sql: String,
    /// 解析出的唯一语句。
    statement: Statement,
    /// 零基槽位索引与书写原文,按索引升序。
    slots: Vec<(usize, String)>,
    /// 语句是否会改变存储的数据库镜像。
    is_write: bool,
    /// prepare 时刻由引擎目录推导出的结果列标签;`None` 表示语句没有行集
    /// 结果,或目录不足以在不执行的前提下确定表头。
    columns: Option<Vec<String>>,
}

impl Prepared {
    /// 提交的 SQL 原文。
    pub fn sql(&self) -> &str {
        &self.sql
    }

    /// 语句种类名(`insert`、`select` 等),与执行结果报告同源。
    pub fn statement_name(&self) -> &'static str {
        self.statement.name()
    }

    /// 权威参数槽位:索引与书写原文。
    pub fn slots(&self) -> &[(usize, String)] {
        &self.slots
    }

    /// 绑定所需的最小参数个数。
    pub fn parameter_count(&self) -> usize {
        self.slots.last().map(|(index, _)| index + 1).unwrap_or(0)
    }

    /// 语句是否会改变存储的数据库镜像。
    pub fn is_write(&self) -> bool {
        self.is_write
    }

    /// 结果列标签,prepare 时刻由引擎目录推导,不执行语句。
    ///
    /// 行集语句给出与成功执行后 `ExecResult.columns` 逐字节一致的表头:
    /// `SELECT`(含视图、派生表、CTE、复合查询与 `*` 展开)、带 `RETURNING`
    /// 的 `INSERT`/`UPDATE`/`DELETE`、`EXPLAIN` 与读形 `PRAGMA`。
    /// `None` 如实表示两种情况:语句本就没有行集结果(无 `RETURNING` 的
    /// DML、DDL、事务控制、写形 `PRAGMA`),或目录不足以确定表头(缺表、
    /// 缺列等——这类语句执行时本就会报错,错误语义与执行期一致)。
    /// 标签在 prepare 时刻的目录上定格;此后 schema 再变化,以执行期结果
    /// 为准,本方法不重新推导。
    pub fn columns(&self) -> Option<Vec<String>> {
        self.columns.clone()
    }
}

/// 一个真实连接:公开对象、选项映射与资源组装,会话状态归执行层。
///
/// 脏镜像跟踪、事务代数、提交边界判定与游标表由
/// [`SqliteSession`](`database_execution::sqlite::session`)拥有;本结构
/// 只持有会话句柄、跨进程写锁与公开的日志模式选项。
///
/// 文件写连接在打开时取得跨进程写锁并持有到 [`Connection::close`] 或句柄
/// 释放;只读连接不参与加锁,打开时把文件一次性读进内存,因此不会被写连接
/// 阻塞。写连接发布镜像时先写同目录临时文件再改名覆盖,读连接要么读到旧的
/// 完整镜像,要么读到新的完整镜像。`delete`/`truncate` 模式在改名前进写出
/// 回滚日志,提交中断后重新打开会回滚到旧镜像。
pub struct Connection {
    /// 连接级会话状态机:引擎句柄、文件归属、脏标记、代数与游标表。
    session: SqliteSession,
    /// 文件写连接的跨进程写锁;只读、内存与字节镜像连接为 `None`。
    lock: Option<WriterLock>,
    /// 提交时如何发布新镜像并处理回滚日志(公开的选项映射)。
    journal_mode: JournalMode,
}

impl Connection {
    /// 按选项打开连接:读已有文件、从字节镜像打开、创建新文件或纯内存。
    ///
    /// 新建的文件库在打开成功后立即落一份一致镜像,与 SQLite 创建即有文件的
    /// 行为一致;缺失文件以 `not-found` 拒绝。
    ///
    /// 文件写连接先取得跨进程写锁再读文件,因此两个写连接不可能同时以同一份
    /// 旧镜像起步;锁被占用时按 `busy_timeout_ms` 重试,超时以 `busy` 拒绝。
    /// 只读连接与字节镜像连接不加锁。
    ///
    /// 打开已有文件时先回放热 `-journal`:写连接把回滚结果写回主文件后再清理
    /// 日志,只读连接只在内存里使用回滚结果、不改动磁盘。
    pub fn open(options: ConnectionOptions) -> Result<Self, SqlError> {
        let mut lock = None;
        let writer = if let Some(pages) = &options.data {
            SqliteWriter::from_pages(pages.clone()).map_err(storage_failure)?
        }
        else if let Some(path) = &options.filename {
            match options.read_only {
                true => {
                    if !path.exists() {
                        return Err(not_found(path));
                    }
                    let pages =
                        journal::recover(path, true, options.journal_mode == JournalMode::Truncate, options.synchronous)
                            .map_err(storage_failure)?;
                    SqliteWriter::from_pages(pages).map_err(storage_failure)?
                }
                false => {
                    let opened = open_writable(
                        path,
                        OpenWritableOptions {
                            busy_timeout: Duration::from_millis(options.busy_timeout_ms),
                            create: options.create,
                            page_size: options.page_size.unwrap_or(4096) as usize,
                            publish: options.journal_mode.publish_mode(),
                            synchronous: options.synchronous,
                        },
                    )
                    .map_err(|error| open_error(error, path))?;
                    lock = Some(opened.lock);
                    opened.writer
                }
            }
        }
        else {
            SqliteWriter::create(options.page_size.unwrap_or(4096) as usize).map_err(storage_failure)?
        };
        let mut engine = Engine::from_writer(writer)?;
        engine.set_synchronous(options.synchronous);
        let journal_mode = options.journal_mode;
        let session = SqliteSession::new(engine, options.filename, options.read_only, journal_mode.publish_mode());
        Ok(Self { session, lock, journal_mode })
    }

    /// 当前数据库镜像,可直接写成 `.sqlite3` 文件。
    ///
    /// 显式事务内推迟的索引重建会在这里先落下,所以镜像永远与目录一致;重建
    /// 失败按 `storage` 家庭错误返回,不交出半成品的字节。
    pub fn snapshot(&mut self) -> Result<&[u8], SqlError> {
        self.session.snapshot()
    }

    /// 镜像头里的 format-3 库版本。
    pub fn sqlite_version(&self) -> Result<String, SqlError> {
        self.session.sqlite_version()
    }

    /// 注册一个宿主自定义标量函数;解析与求值都经本连接的引擎,注册语义
    /// 见 [`Engine::register_host_function`]。
    pub fn register_host_function(&mut self, definition: HostMicroDefinition, adapter: Arc<dyn HostRuntimeAdapter>) -> Result<(), SqlError> {
        self.session.register_host_function(definition, adapter)
    }

    /// 解析一次 SQL,得到可反复执行的预编译语句。
    ///
    /// 恰好一条语句:空输入与多语句分别以 `empty-statement` 和
    /// `multiple-statements` 拒绝,与 [`Engine::execute_one`] 的合同一致。
    /// 结果列标签在本时刻的目录上推导;目录解析失败(缺表、缺列)不改变
    /// 拒绝口径——`prepare` 仍然成功,标签如实为 `None`,错误留给执行期按
    /// 既有语义抛出。
    pub fn prepare(&self, sql: &str) -> Result<Prepared, SqlError> {
        let statements = self.session.parse(sql)?;
        let statement = match statements.len() {
            1 => statements.into_iter().next().expect("length checked"),
            0 => return Err(SqlError::new("empty-statement", "expected exactly one statement but found 0")),
            count => return Err(SqlError::new("multiple-statements", format!("expected exactly one statement but found {count}"))),
        };
        let slots = parameter_slots(&statement);
        let is_write = statement.writes();
        let columns = self.session.describe(&statement);
        Ok(Prepared { sql: sql.to_string(), statement, slots, is_write, columns })
    }

    /// 执行一条预编译语句,绑定零基位置参数。
    ///
    /// 只读拒绝、参数检查、执行与提交边界全部由会话状态机给出,本连接
    /// 只做转调。
    pub fn run(&mut self, prepared: &Prepared, parameters: &[Cell]) -> Result<ExecResult, SqlError> {
        self.session.run(&prepared.statement, parameters)
    }

    /// 执行一段分号分隔的脚本,逐条语句应用只读拒绝与提交边界。
    pub fn execute(&mut self, sql: &str, parameters: &[Cell]) -> Result<Vec<ExecResult>, SqlError> {
        self.session.execute(sql, parameters)
    }

    /// 执行一条预编译语句,把结果集放进会话的游标表,返回句柄与表头。
    ///
    /// 简单单表扫描在打开时只固定一份整页快照,后续 [`Connection::cursor_next`]
    /// 按 b-tree 页顺序分批解码,`CursorOpen::lazy` 为 `true`。其余语句与
    /// [`Connection::run`] 共用同一条执行路径、只读拒绝、参数检查与提交
    /// 边界,结果集在打开时一次性物化,`CursorOpen::lazy` 为 `false`。
    ///
    /// 事务内打开的游标记录当前事务代数;事务提交或回滚后调用
    /// [`Connection::cursor_next`] 会以 `cursor-expired` 拒绝。
    pub fn open_cursor(&mut self, prepared: &Prepared, parameters: &[Cell]) -> Result<CursorOpen, SqlError> {
        self.session.open_cursor(&prepared.statement, parameters)
    }

    /// 从游标读取至多 `limit` 行,返回本窗口与是否已到末尾。
    ///
    /// `done` 为真表示结果集已经取完,游标仍可调用 [`Connection::cursor_close`]
    /// 释放。未知句柄以 `cursor-not-found` 拒绝,事务已结束的句柄以
    /// `cursor-expired` 拒绝,已关闭的连接以 `db-closed` 拒绝。
    pub fn cursor_next(&mut self, handle: u64, limit: usize) -> Result<CursorBatch, SqlError> {
        self.session.cursor_next(handle, limit)
    }

    /// 释放一个游标。未知句柄视为已释放,便于宿主在 `finally` 里安全清理。
    pub fn cursor_close(&mut self, handle: u64) -> Result<(), SqlError> {
        self.session.cursor_close(handle)
    }

    /// 当前连接仍持有的游标数量(测试与诊断用,不是作者面 API)。
    #[doc(hidden)]
    pub fn cursor_count(&self) -> usize {
        self.session.cursor_count()
    }

    /// 把当前镜像完整写入 `path`,即真正的备份。
    ///
    /// 只读连接允许备份:它读取镜像、写出到另一个文件,不改变本连接。
    pub fn backup(&mut self, path: &Path) -> Result<(), SqlError> {
        self.session.backup(path)
    }

    /// 关闭连接:先完成发布,再改关闭状态并释放跨进程写锁。
    ///
    /// 有已提交未落盘的改动时,发布在关闭状态改变之前执行;发布失败则
    /// 原样传播错误,连接保持未关闭、锁保持持有、未发布状态可查询,
    /// 修复环境后可重试,发布错误不会被丢弃。事务未结束时不落盘,与
    /// SQLite 在关闭时回滚未提交事务的行为一致。再次调用 `close` 为空
    /// 操作,关闭后的连接以 `db-closed` 拒绝语句。
    pub fn close(&mut self) -> Result<(), SqlError> {
        self.session.close()?;
        self.lock = None;
        Ok(())
    }

    /// 镜像是否存在已提交但尚未落盘(或落盘失败待重试)的改动
    /// (测试与诊断用,不是作者面 API)。
    #[doc(hidden)]
    pub fn has_unpublished_changes(&self) -> bool {
        self.session.is_dirty()
    }

    /// 是否有显式事务或保存点作用域打开。
    pub fn in_transaction(&self) -> bool {
        self.session.in_transaction()
    }

    /// 本连接最近一次成功插入的 rowid。
    pub fn last_insert_rowid(&self) -> i64 {
        self.session.last_insert_rowid()
    }

    /// 配置的页大小。
    pub fn page_size(&self) -> u32 {
        self.session.page_size()
    }

    /// 连接是否为只读。
    pub fn is_read_only(&self) -> bool {
        self.session.is_read_only()
    }

    /// 落盘目标路径。
    pub fn filename(&self) -> Option<&Path> {
        self.session.filename()
    }

    /// `PRAGMA synchronous` 透传。
    pub fn set_synchronous(&mut self, on: bool) {
        self.session.set_synchronous(on);
    }

    /// 本连接提交时使用的日志模式。
    pub fn journal_mode(&self) -> JournalMode {
        self.journal_mode
    }
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Connection")
            .field("filename", &self.session.filename())
            .field("read_only", &self.session.is_read_only())
            .field("dirty", &self.session.is_dirty())
            .finish()
    }
}
