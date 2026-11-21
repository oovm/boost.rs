//! In-memory execution layer: SQL subset over the pure-Rust format-3 writer.
//!
//! The engine keeps the whole database image in `SqliteWriter::pages()`. It
//! never imports an upstream SQLite engine; [`Engine::from_writer`] wraps an
//! image read by `SqliteWriter::open`, and [`Engine::save`] flushes the current
//! image back to a file.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use database_formats::sqlite::{RecordLimits, RecordValue, WriteValue, decode_library_version};
use database_query::sqlite::{
    SqlError,
    catalog::*,
    frontend::{Expr, Statement, TransactionAction, parse_with_host_functions},
    storage_failure,
    value::*,
};
use database_storage::sqlite::{ColumnSpec, SchemaObjectKind, SqliteWriter, TableLimits, TextEncoding, decode_record};
use database_udf::{HostMicroDefinition, HostRuntimeAdapter};

use crate::sqlite::{
    ddl, describe, dml, explain,
    host::HostFunctionTable,
    pragma,
    select::*,
    stmt::{self, AttachedSlot, StmtContext},
    trigger::TriggerState,
    txn::{self, TransactionState},
};

pub use crate::sqlite::stmt::ExecResult;

/// One database attached with `ATTACH DATABASE`.
struct AttachedDb {
    /// Schema name as written.
    name: String,
    /// File the database lives in; `None` for an in-memory or temporary one.
    file: Option<PathBuf>,
    /// Text `PRAGMA database_list` reports in the `file` column.
    display: String,
    /// The attached database runs through the same engine as `main`, so every
    /// statement shape keeps working inside it without a second parser.
    engine: Box<Engine>,
    /// The in-memory image differs from the file on disk.
    dirty: bool,
}

/// In-memory SQLite format-3 database with a minimal SQL surface.
pub struct Engine {
    writer: SqliteWriter,
    /// The catalog: schema objects statements resolve names against, plus
    /// the temp-schema allocator state.
    catalog: SqliteCatalog,
    /// 触发器事件队列与 `NEW` / `OLD` 帧栈,机器与状态 owner 在
    /// `database.execution::sqlite::trigger`(C8);门面持有实例,语句上下文借用。
    trigger: TriggerState,
    /// Databases attached with `ATTACH DATABASE`, in attach order.
    attached: Vec<AttachedDb>,
    /// Lowercase attached schema name to its position in [`Engine::attached`].
    attached_lookup: BTreeMap<String, usize>,
    /// Schema name this database was attached under. A reference to that name
    /// inside the database itself resolves like an unqualified one.
    alias: Option<String>,
    /// `main.` / `temp.` qualifier of the statement being executed.
    scope: TableScope,
    /// Values bound to `?` / `:name` placeholders for the statement currently
    /// executing, in declaration order.
    parameters: Vec<Cell>,
    /// Supplied row of the `ON CONFLICT DO UPDATE` clause being evaluated.
    /// `excluded.<column>` reads this row instead of the table's stored row.
    upsert_excluded: Vec<Cell>,
    /// `PRAGMA foreign_keys`, enabled at session start like better-sqlite3.
    foreign_keys: bool,
    /// `PRAGMA recursive_triggers`, off at session start like SQLite.
    recursive_triggers: bool,
    /// Rowid of the most recent row a statement wrote successfully, like
    /// `sqlite3_last_insert_rowid`. It starts at 0 for every connection and
    /// only changes when an insert writes at least one row.
    last_insert_rowid: i64,
    /// 打开的显式事务,`None` 表示 autocommit;savepoint 名按外到内存放。
    /// 状态类型与变更语义 owner 在 `database.execution::sqlite::txn`(C9b),
    /// 门面只持有实例作连接级资源存放处,事务机器与语句上下文借用它。
    transaction: Option<TransactionState>,
    /// SELECT 机器共享的语句级缓存状态(写世代、索引探针、子查询记忆等)。
    exec_state: SqliteExecState,
    /// 会话宿主函数表,按折叠小写名索引;解析期与求值期共用同一份。
    host_functions: HostFunctionTable,
    /// 显式事务内被推迟的索引 b-tree 重建。
    ///
    /// 索引页是整树重建出来的,单条语句重建一次会让事务内的第 n 条写入花掉
    /// O(n) 的重建成本。事务内只置位,提交或读镜像前统一重建一次。
    pending_indexes: bool,
    /// 同 [`Engine::pending_indexes`],针对 WITHOUT ROWID 表的聚簇树。
    pending_without_rowid: bool,
}

impl Engine {
    /// Creates a blank in-memory database image.
    pub fn new(page_size: Option<u32>) -> Result<Self, SqlError> {
        let page_size = page_size.unwrap_or(4096) as usize;
        let writer = SqliteWriter::create(page_size).map_err(|error| storage_failure(error))?;
        Self::from_writer(writer)
    }

    /// Wraps an already opened writer and rebuilds the catalog from the
    /// `sqlite_master` rows of its image, so `SELECT` works on a file opened
    /// with [`SqliteWriter::open`].
    pub fn from_writer(writer: SqliteWriter) -> Result<Self, SqlError> {
        let mut engine = Self {
            writer,
            catalog: SqliteCatalog::default(),
            trigger: TriggerState::default(),
            attached: Vec::new(),
            attached_lookup: BTreeMap::new(),
            alias: None,
            scope: TableScope::Default,
            parameters: Vec::new(),
            upsert_excluded: Vec::new(),
            foreign_keys: true,
            recursive_triggers: false,
            last_insert_rowid: 0,
            transaction: None,
            exec_state: SqliteExecState::default(),

            host_functions: HostFunctionTable::default(),
            pending_indexes: false,
            pending_without_rowid: false,
        };
        ddl::load_catalog(&engine.writer, &mut engine.catalog)?;
        Ok(engine)
    }

    /// 用一份新的已提交镜像替换底层页集合,保留连接级会话状态。
    ///
    /// 只供只读文件连接在别的事务提交后刷新读视图使用:宿主函数、`PRAGMA`
    /// 与外键开关、`last_insert_rowid` 和附加库都属于连接,不随文件内容变化;
    /// 目录与语句级索引缓存属于镜像,必须按新镜像重建,否则旧探针会返回
    /// 已提交镜像里不存在的 rowid。
    pub fn reload_image(&mut self, writer: SqliteWriter) -> Result<(), SqlError> {
        self.writer = writer;
        txn::reload_catalog(&self.writer, &mut self.catalog)?;
        self.exec_state = SqliteExecState::default();
        self.pending_indexes = false;
        self.pending_without_rowid = false;
        Ok(())
    }

    /// Executes exactly one statement.
    pub fn execute_one(&mut self, statement: &str) -> Result<ExecResult, SqlError> {
        let statements = self.parse(statement)?;
        match statements.len() {
            1 => {
                let only = statements.into_iter().next().expect("length checked");
                self.run_triggered(only)
            }
            0 => Err(SqlError::new("empty-statement", "expected exactly one statement but found 0")),
            count => Err(SqlError::new("multiple-statements", format!("expected exactly one statement but found {count}"))),
        }
    }

    /// Executes a list of statements in order and stops at the first rejection.
    pub fn execute_batch(&mut self, statements: &[String]) -> Result<Vec<ExecResult>, SqlError> {
        let mut results = Vec::with_capacity(statements.len());
        for statement in statements {
            results.push(self.execute_one(statement)?);
        }
        Ok(results)
    }

    /// Executes a semicolon-separated script and returns the results in order.
    pub fn execute(&mut self, sql: &str) -> Result<Vec<ExecResult>, SqlError> {
        let statements = self.parse(sql)?;
        let mut results = Vec::with_capacity(statements.len());
        for statement in statements {
            results.push(self.run_triggered(statement)?);
        }
        Ok(results)
    }

    /// Executes a semicolon-separated script with positional parameter values
    /// bound to `?`, `?NNN` and `:name` placeholders.
    pub fn execute_with_parameters(&mut self, sql: &str, parameters: &[Cell]) -> Result<Vec<ExecResult>, SqlError> {
        self.parameters = parameters.to_vec();
        let result = self.execute(sql);
        self.parameters.clear();
        result
    }

    /// 执行一条已经解析好的语句,并绑定零基位置参数。
    ///
    /// 已解析的 AST 就是本引擎的"预编译"形态:宿主解析一次、反复执行同一
    /// 语句时不再重新词法或重新解析 SQL 文本。触发器体照常经过
    /// [`Engine::run_triggered`],与文本入口的执行路径一致。
    pub fn execute_statement(&mut self, statement: &Statement, parameters: &[Cell]) -> Result<ExecResult, SqlError> {
        // 复用参数缓冲:预编译语句高频执行时不再每次重新分配一份参数向量。
        self.parameters.clear();
        self.parameters.extend_from_slice(parameters);
        let result = self.run_triggered(statement.clone());
        self.parameters.clear();
        result
    }

    /// 按本引擎的宿主函数表解析一段分号分隔的脚本。
    ///
    /// 与 [`crate::sql::parse`] 的唯一区别:折叠名命中宿主注册表且元数相符
    /// 的函数调用成为 [`Expr::HostFunction`] 节点,同名同元数的内建派发被
    /// 替换。宿主在 prepare 之前注册即可让语句通过解析;注册发生在某条
    /// 已解析语句之后时,该语句里的宿主调用在执行期按当时的注册表求值,
    /// 未注册的名字以 `unknown-function` 如实拒绝。
    pub fn parse(&self, sql: &str) -> Result<Vec<Statement>, SqlError> {
        parse_with_host_functions(sql, Some(&self.host_functions))
    }

    /// 注册一个宿主自定义标量函数,形态取自 `database-udf` 合同。
    ///
    /// `definition` 是合同的 [`HostMicroDefinition`] 元数据(身份名、签名、
    /// 策略、不透明句柄、指纹),`adapter` 是宿主运行时中真实函数注册表的
    /// 适配器;注册前按合同校验元数据,不合合同的定义以
    /// `udf-invalid-definition` 拒绝。派发名按 ASCII 小写折叠,与内建函数
    /// 同一口径;折叠同名再次注册替换旧实现(JS API 的注册语义),折叠名
    /// 与内建函数同名同元数时宿主实现优先,与 SQLite
    /// `sqlite3_create_function` 替换内建的真实行为一致。
    ///
    /// 注册同时传播到已附加库的引擎,并对之后 `ATTACH` 的库继续生效,
    /// 使宿主函数在跨库语句与附加库视图展开里同样可求值。
    ///
    /// 合同是 Phase-1 标量形态,没有 step / final 聚合语义:宿主聚合与
    /// 宿主窗口函数不接入,`OVER` / `FILTER` / `DISTINCT` 形态在解析期以
    /// 明确错误拒绝。
    pub fn register_host_function(&mut self, definition: HostMicroDefinition, adapter: Arc<dyn HostRuntimeAdapter>) -> Result<(), SqlError> {
        let implementation = HostFunctionTable::build(definition, adapter)?;
        self.host_functions.install(implementation.clone());
        for attached in &mut self.attached {
            attached.engine.host_functions.install(implementation.clone());
        }
        Ok(())
    }

    /// 当前会话注册的宿主函数名(折叠小写,字典序)。
    pub fn host_function_names(&self) -> Vec<String> {
        self.host_functions.names()
    }

    /// `PRAGMA synchronous` 的透传:`false` 时落盘跳过持久化刷新。
    pub fn set_synchronous(&mut self, on: bool) {
        self.writer.set_synchronous(on);
    }

    /// Current database image, suitable for writing to a `.sqlite3` file.
    ///
    /// Index b-trees are rebuilt before the image is handed out, so the exported
    /// bytes stay consistent with their `sqlite_master` entries without calling
    /// [`SqliteWriter::save`]. Inside an explicit transaction the rebuild of the
    /// writes made so far happens here, which is why the receiver is mutable and
    /// the rebuild can fail.
    pub fn snapshot(&mut self) -> Result<&[u8], SqlError> {
        self.flush_pending_materialization("snapshot")?;
        Ok(self.writer.pages())
    }

    /// 组装本引擎的 SELECT 执行上下文:按引用共享目录、写入器、缓存与参数。
    fn exec(&self) -> ExecContext<'_> {
        ExecContext {
            writer: &self.writer,
            catalog: &self.catalog,
            state: &self.exec_state,
            host_functions: &self.host_functions,
            parameters: &self.parameters,
            upsert_excluded: &self.upsert_excluded,
            trigger_frames: &self.trigger.frames,
            alias: self.alias.as_deref(),
            attached: self.attached.iter().map(|db| db.engine.exec()).collect::<Vec<_>>().into(),
            attached_lookup: &self.attached_lookup,
        }
    }

    /// 打开一条只读惰性游标:先定型计划并冲刷待物化写入,再交给执行树只读打开。
    ///
    /// 计划在此刻一次成型,整页快照也在这一刻克隆;之后的逐页解码不再回到
    /// 目录,因此游标读取与并发写入完全隔离。
    pub fn open_lazy_cursor(&mut self, statement: &Statement, parameters: &[Cell]) -> Result<Option<LazyCursor>, SqlError> {
        let Some(plan) = self.exec().plan_lazy_scan(statement)?
        else {
            return Ok(None);
        };
        // 游标快照直接克隆镜像页,推迟的索引重建必须先落下。
        self.flush_pending_materialization("cursor")?;
        self.exec().open_lazy_scan(plan, parameters)
    }

    /// 从惰性游标读取至多 limit 行,保持 b-tree 扫描顺序。
    pub fn lazy_cursor_next(&mut self, cursor: &mut LazyCursor, limit: usize) -> Result<LazyBatch, SqlError> {
        if cursor.finished || limit == 0 {
            return Ok(LazyBatch { rows: Vec::new(), done: cursor.finished });
        }
        let saved = std::mem::take(&mut self.parameters);
        self.parameters = cursor.parameters.clone();
        let result = self.exec().lazy_cursor_next_inner(cursor, limit);
        self.parameters = saved;
        result
    }

    /// 把推迟的索引 / 聚簇树重建落进当前镜像。
    ///
    /// 只在事务提交、写盘或读取镜像时调用;重建完成后清位,失败则保留标记让
    /// 调用方还能重试或回滚。
    fn flush_pending_materialization(&mut self, statement: &'static str) -> Result<(), SqlError> {
        if self.pending_indexes {
            self.writer.materialize_indexes().map_err(|error| storage_failure(error).in_statement(statement))?;
            self.pending_indexes = false;
        }
        if self.pending_without_rowid {
            self.writer.materialize_without_rowid().map_err(|error| storage_failure(error).in_statement(statement))?;
            self.pending_without_rowid = false;
        }
        Ok(())
    }

    /// Rowid of the last row this connection inserted successfully, mirroring
    /// `sqlite3_last_insert_rowid`.
    ///
    /// A statement that inserts several rows reports the last row it wrote. If
    /// a later row fails and the statement rolls back, the value stays at the
    /// last row that did pass, because SQLite records it row by row. Statements
    /// that insert no row leave it untouched.
    pub fn last_insert_rowid(&self) -> i64 {
        self.last_insert_rowid
    }

    /// Whether an explicit transaction or savepoint scope is open.
    pub fn in_transaction(&self) -> bool {
        self.transaction.is_some()
    }

    /// Flushes the current database image to `path`.
    ///
    /// Reads of an existing file go through [`Engine::from_writer`]; this is
    /// the matching write-back so a file-backed session survives reopening.
    pub fn save(&mut self, path: impl AsRef<Path>) -> Result<(), SqlError> {
        self.flush_pending_materialization("save")?;
        self.writer.save(path).map_err(|error| storage_failure(error))
    }

    /// Flushes the current database image to `path` without ever exposing a
    /// half-written file, by renaming a complete sibling temporary file over
    /// it.
    pub fn save_atomic(&mut self, path: impl AsRef<Path>) -> Result<(), SqlError> {
        self.flush_pending_materialization("save")?;
        self.writer.save_atomic(path).map_err(|error| storage_failure(error))
    }

    /// Flushes the current image through the storage publication contract.
    ///
    /// The journaled modes live in `database.storage::sqlite`: the engine only
    /// materials deferred index trees and hands its writer to that owner.
    pub fn publish(&mut self, path: &Path, mode: database_storage::sqlite::PublishMode) -> Result<(), SqlError> {
        self.flush_pending_materialization("save")?;
        database_storage::sqlite::journal::publish(path, &mut self.writer, mode).map_err(storage_failure)
    }

    /// Configured page size.
    pub fn page_size(&self) -> u32 {
        self.writer.page_size() as u32
    }

    /// Format-3 library version stamped in the image header.
    pub fn sqlite_version(&self) -> Result<String, SqlError> {
        decode_library_version(self.writer.pages()).map_err(|error| storage_failure(error))
    }

    /// Runs one statement, dispatching to the statement machine that owns it.
    ///
    /// 事务、`ATTACH` / `DETACH`、`VACUUM`、`PRAGMA` 与 `EXPLAIN` 六类语句臂
    /// 留在门面分发点:事务臂委托 `database.execution::sqlite::txn` 机器,
    /// 其余依赖这里的附加库引擎集合与宿主函数注册;其余语句
    /// 全部交给 `database.execution::sqlite::stmt` 的 [`StmtContext`] 机器,
    /// 触发器 drain / fire 循环也在该机器的递归里。
    fn run_triggered(&mut self, statement: Statement) -> Result<ExecResult, SqlError> {
        match &statement {
            Statement::Explain { .. }
            | Statement::Transaction { .. }
            | Statement::Attach { .. }
            | Statement::Detach { .. }
            | Statement::Vacuum { .. }
            | Statement::Pragma { .. } => self.run_statement(statement),
            _ => {
                let mut ctx = self.stmt_ctx();
                ctx.run_triggered(statement)
            }
        }
    }

    /// 组装本引擎的语句执行上下文:分发臂、触发器 drain / fire 与附加库递归
    /// 全部经 [`StmtContext`] 借用门面状态;六类门面语句臂仍走 [`Engine`] 自身,
    /// 其中事务臂经同一上下文委托 `database.execution::sqlite::txn` 机器。
    fn stmt_ctx(&mut self) -> StmtContext<'_> {
        StmtContext {
            writer: &mut self.writer,
            catalog: &mut self.catalog,
            exec_state: &self.exec_state,
            host_functions: &self.host_functions,
            trigger: &mut self.trigger,
            parameters: &mut self.parameters,
            upsert_excluded: &mut self.upsert_excluded,
            last_insert_rowid: &mut self.last_insert_rowid,
            pending_indexes: &mut self.pending_indexes,
            pending_without_rowid: &mut self.pending_without_rowid,
            attached_lookup: &self.attached_lookup,
            alias: self.alias.as_deref(),
            scope: &mut self.scope,
            foreign_keys: self.foreign_keys,
            recursive_triggers: self.recursive_triggers,
            transaction: &mut self.transaction,
            attached: self
                .attached
                .iter_mut()
                .map(|db| AttachedSlot { file: &db.file, dirty: &mut db.dirty, ctx: db.engine.stmt_ctx() })
                .collect(),
        }
    }

    fn run_statement(&mut self, statement: Statement) -> Result<ExecResult, SqlError> {
        // 子查询备忘与已解码行集都只在一条语句的 AST 存活期内有意义。
        self.exec_state.subquery_cache.borrow_mut().clear();
        self.exec_state.exists_probes.borrow_mut().clear();
        self.exec_state.statement_scans.borrow_mut().clear();
        self.exec_state.outer_read.set(false);
        let result = self.run_statement_dispatch(statement);
        // 释放本条语句解码的整表行,避免大表在下一条语句前仍驻留内存。
        self.exec_state.statement_scans.borrow_mut().clear();
        result
    }

    fn run_statement_dispatch(&mut self, statement: Statement) -> Result<ExecResult, SqlError> {
        // 读语句不动索引;`UPDATE` / `DELETE` 在自己的执行分支里按实际改动
        // 的索引精确失效(见 `invalidate_indexes_after_update` 与
        // `patch_index_probes_after_delete`),其余会改行或改 schema 的语句
        // 在这里一刀切,让相关索引的缓存整体作废。
        if !matches!(statement, Statement::Select { .. } | Statement::Explain { .. }) {
            match &statement {
                Statement::Update { .. } | Statement::Delete { .. } => {}
                // `BEGIN` / `COMMIT` 本身不改行:已缓存的索引内容仍然准确,
                // 一刀切只会让包住写入的事务丢掉增量补丁。回滚类动作相反,
                // 会撤销事务内已并入探针的改动,必须走全量失效。
                Statement::Transaction { action: TransactionAction::Begin | TransactionAction::Commit, .. } => {}
                // 只追加行的普通 `INSERT` 不改变既有索引内容:语句提交后会把新行
                // 增量并入探针,这里先全量失效只会让补丁失去对齐基准。
                Statement::Insert { schema: None, table, conflict: None, replace: false, .. }
                    if self.exec().insert_keeps_index_content(table) => {}
                _ => match self.exec().statement_table_key(&statement) {
                    Some(key) => self.exec().invalidate_table_indexes(&key),
                    None => self.exec().invalidate_all_indexes(),
                },
            }
        }
        let Some(schema) = stmt::leading_schema(&statement).map(str::to_owned)
        else {
            if let Some(index) = self.attached_target(&statement) {
                return self.run_attached(index, statement);
            }
            return self.run_statement_inner(statement);
        };
        let lower = schema.to_ascii_lowercase();
        if self.alias.as_deref().is_some_and(|alias| alias.eq_ignore_ascii_case(&schema)) {
            let mut statement = statement;
            stmt::clear_statement_schema(&mut statement, &schema);
            return self.run_statement_inner(statement);
        }
        if lower == "main" || lower == "temp" {
            let mut statement = statement;
            stmt::clear_statement_schema(&mut statement, &schema);
            let previous = std::mem::replace(&mut self.scope, if lower == "main" { TableScope::Main } else { TableScope::Temp });
            let result = self.run_statement_inner(statement);
            self.scope = previous;
            return result;
        }
        match self.attached_lookup.get(&lower).copied() {
            Some(index) => {
                let mut statement = statement;
                stmt::clear_statement_schema(&mut statement, &schema);
                stmt::strip_statement_sql(&mut statement, &schema);
                self.run_attached(index, statement)
            }
            None => Err(stmt::unknown_schema_error(&statement, &schema)),
        }
    }

    /// The position of the attached database an unqualified statement resolves
    /// to, following SQLite's search order after temp and main miss.
    fn attached_target(&self, statement: &Statement) -> Option<usize> {
        // A trigger body resolves unqualified names in the schema of the
        // trigger, never in an attached database.
        if !self.trigger.active_triggers.is_empty() {
            return None;
        }
        let (name, kind) = match statement {
            Statement::Insert { schema: None, table, .. }
            | Statement::Update { schema: None, table, .. }
            | Statement::Delete { schema: None, table, .. }
            | Statement::DropTable { schema: None, name: table, .. }
            | Statement::AlterTable { schema: None, table, .. } => (table, stmt::TargetKind::Table),
            Statement::DropIndex { schema: None, name, .. } => (name, stmt::TargetKind::Index),
            Statement::DropView { schema: None, name, .. } => (name, stmt::TargetKind::View),
            Statement::DropTrigger { schema: None, name, .. } => (name, stmt::TargetKind::Trigger),
            _ => return None,
        };
        let key = name.to_ascii_lowercase();
        if kind == stmt::TargetKind::Table && (self.catalog.temp_lookup.contains_key(&key) || self.catalog.table_lookup.contains_key(&key)) {
            return None;
        }
        self.attached.iter().position(|attached| match kind {
            stmt::TargetKind::Table => attached.engine.catalog.table_lookup.contains_key(&key),
            stmt::TargetKind::Index => attached.engine.catalog.indexes.contains_key(&key),
            stmt::TargetKind::View => attached.engine.catalog.views.contains_key(&key),
            stmt::TargetKind::Trigger => attached.engine.catalog.triggers.contains_key(&key),
        })
    }

    /// Runs one statement inside an attached database with the parameters and
    /// the connection-wide last rowid carried over.
    fn run_attached(&mut self, index: usize, statement: Statement) -> Result<ExecResult, SqlError> {
        let parameters = self.parameters.clone();
        let attached = &mut self.attached[index];
        attached.engine.parameters = parameters;
        let result = attached.engine.run_triggered(statement);
        let rowid = attached.engine.last_insert_rowid;
        attached.engine.parameters.clear();
        let result = result?;
        self.last_insert_rowid = rowid;
        self.attached[index].dirty = true;
        if self.transaction.is_none() {
            self.persist_attached(index)?;
        }
        Ok(result)
    }

    /// Flushes one attached database image back to its file.
    fn persist_attached(&mut self, index: usize) -> Result<(), SqlError> {
        if !self.attached.get(index).is_some_and(|attached| attached.dirty) {
            return Ok(());
        }
        if let Some(path) = self.attached[index].file.clone() {
            self.attached[index].engine.save(&path)?;
        }
        self.attached[index].dirty = false;
        Ok(())
    }

    /// `ATTACH [DATABASE] file AS name`.
    fn attach_database(&mut self, file: &str, name: &str) -> Result<ExecResult, SqlError> {
        let key = name.to_ascii_lowercase();
        if key == "main" || key == "temp" || self.attached_lookup.contains_key(&key) {
            return Err(SqlError::new("database-in-use", format!("database {name} is already in use")).in_statement("attach"));
        }
        if self.attached.len() >= 10 {
            return Err(SqlError::new("too-many-databases", "too many attached databases - max 10").in_statement("attach"));
        }
        let (writer, path, display) = ddl::open_attached_database(file, self.page_size() as usize)?;
        let mut engine = Box::new(Engine::from_writer(writer)?);
        engine.alias = Some(name.to_string());
        // 宿主函数在本会话注册一次后对之后 `ATTACH` 的库同样生效;表内只存放
        // `Arc` 句柄,克隆不会复制宿主实现。
        engine.host_functions = self.host_functions.clone();
        let index = self.attached.len();
        self.attached.push(AttachedDb { name: name.to_string(), file: path, display, engine, dirty: false });
        self.attached_lookup.insert(key, index);
        Ok(ExecResult { statement: "attach", columns: Vec::new(), rows: Vec::new(), affected: 0 })
    }

    /// `DETACH [DATABASE] name`.
    fn detach_database(&mut self, name: &str) -> Result<ExecResult, SqlError> {
        let key = name.to_ascii_lowercase();
        if key == "main" || key == "temp" {
            return Err(SqlError::new("cannot-detach", format!("cannot detach database {name}")).in_statement("detach"));
        }
        let Some(index) = self.attached_lookup.get(&key).copied()
        else {
            return Err(SqlError::new("unknown-database", format!("no such database: {name}")).in_statement("detach"));
        };
        self.persist_attached(index)?;
        self.attached.remove(index);
        self.attached_lookup.clear();
        for (position, attached) in self.attached.iter().enumerate() {
            self.attached_lookup.insert(attached.name.to_ascii_lowercase(), position);
        }
        Ok(ExecResult { statement: "detach", columns: Vec::new(), rows: Vec::new(), affected: 0 })
    }

    /// `VACUUM [schema] [INTO target]`.
    ///
    /// 重建与上游 `VACUUM` 同形:按 `sqlite_master` 的顺序重放表结构,复制行,
    /// 再重建索引、视图与触发器。新镜像没有 freelist 页,并保留页大小、文本
    /// 编码、`user_version` 与 `application_id`。不带 `INTO` 时新镜像替换
    /// 当前页集合;带 `INTO` 时写到目标文件,当前库保持不变。
    ///
    /// 与上游一致的边界:`VACUUM`/`VACUUM INTO` 在事务内拒绝,`VACUUM` 也不能
    /// 出现在触发器体里(解析期按上游报 `near "vacuum": syntax error`);
    /// `temp` 库的 `VACUUM` 是空操作,`VACUUM temp INTO` 写出空库;目标文件
    /// 已存在时报 `output file already exists`,目标父目录缺失或目标是目录时
    /// 报 `unable to open database`,目标不是文本时报 `non-text filename`。
    fn vacuum_statement(&mut self, into: Option<&Expr>) -> Result<ExecResult, SqlError> {
        if self.transaction.is_some() {
            return Err(SqlError::new("SQLITE_ERROR", "cannot VACUUM from within a transaction").in_statement("vacuum"));
        }
        if self.scope == TableScope::Temp {
            if let Some(expression) = into {
                let target = self.vacuum_target(expression)?;
                let empty = SqliteWriter::create(self.writer.page_size()).map_err(|error| storage_failure(error).in_statement("vacuum"))?;
                ddl::write_vacuum_image(&target, empty.pages())?;
            }
            return Ok(ExecResult { statement: "vacuum", columns: Vec::new(), rows: Vec::new(), affected: 0 });
        }
        let image = self.vacuum_image()?;
        if let Some(expression) = into {
            let target = self.vacuum_target(expression)?;
            ddl::write_vacuum_image(&target, &image)?;
        }
        else {
            let synchronous = self.writer.synchronous();
            let mut writer = SqliteWriter::from_pages(image).map_err(|error| storage_failure(error).in_statement("vacuum"))?;
            writer.set_synchronous(synchronous);
            self.writer = writer;
            txn::reload_catalog(&self.writer, &mut self.catalog)?;
            self.exec_state.index_probes.borrow_mut().clear();
            self.exec_state.subquery_cache.borrow_mut().clear();
            self.exec_state.exists_probes.borrow_mut().clear();
            self.exec_state.write_epoch.set(self.exec_state.write_epoch.get() + 1);
        }
        Ok(ExecResult { statement: "vacuum", columns: Vec::new(), rows: Vec::new(), affected: 0 })
    }

    /// 求出 `VACUUM INTO` 的文件名;SQLite 要求表达式的结果是文本。
    fn vacuum_target(&self, expression: &Expr) -> Result<String, SqlError> {
        match self.exec().eval_trigger_expression(expression, "vacuum")? {
            Cell::Text(target) => Ok(target),
            _ => Err(SqlError::new("SQLITE_ERROR", "non-text filename").in_statement("vacuum")),
        }
    }

    /// 生成 `VACUUM` 的重建镜像。
    ///
    /// 顺序与上游一致:先建表,再复制行(触发器尚未建立,复制不会改写数据),
    /// 最后建索引、视图与触发器。`sqlite_sequence` 的高水位先于行复制写入,
    /// 之后的显式 rowid 只会上抬、不会覆盖。`rowid` 表按 rowid 升序复制,
    /// 没有 `INTEGER PRIMARY KEY` 的表因此像上游一样重新编号。
    fn vacuum_image(&mut self) -> Result<Vec<u8>, SqlError> {
        self.flush_pending_materialization("vacuum")?;
        let page_size = self.writer.page_size();
        let encoding = match u32::from_be_bytes(
            self.writer
                .pages()
                .get(56..60)
                .and_then(|marker| marker.try_into().ok())
                .ok_or_else(|| SqlError::new("storage", "sqlite header too short").in_statement("vacuum"))?,
        ) {
            2 => TextEncoding::Utf16Le,
            3 => TextEncoding::Utf16Be,
            _ => TextEncoding::Utf8,
        };
        let user_version = self.writer.user_version();
        let application_id = self.writer.application_id();
        let limits = TableLimits::query();
        let objects = self.writer.schema_objects(limits).map_err(|error| storage_failure(error).in_statement("vacuum"))?;
        let writer = SqliteWriter::create_with_encoding(page_size, encoding).map_err(|error| storage_failure(error).in_statement("vacuum"))?;
        let mut fresh = Engine::from_writer(writer)?;
        for object in &objects {
            if object.kind != SchemaObjectKind::Table || object.name.eq_ignore_ascii_case("sqlite_sequence") {
                continue;
            }
            if let Some(sql) = &object.sql {
                fresh.execute_one(sql)?;
            }
        }
        // `sqlite_sequence` 是保留表,不在主目录里,因此直接读存储层。它在
        // 源库里存在就照样出现在重建镜像里,高水位在建表后先写入。
        let sequence_present =
            objects.iter().any(|object| object.kind == SchemaObjectKind::Table && object.name.eq_ignore_ascii_case("sqlite_sequence"));
        if sequence_present {
            let fresh_present = fresh
                .writer
                .schema_objects(limits)
                .map_err(|error| storage_failure(error).in_statement("vacuum"))?
                .iter()
                .any(|object| object.kind == SchemaObjectKind::Table && object.name.eq_ignore_ascii_case("sqlite_sequence"));
            if !fresh_present {
                // 源库可能只留下一个空的 `sqlite_sequence`(最后一个 AUTOINCREMENT
                // 表被删后行没了、表还在),重建镜像要照样保留它。
                let columns: Vec<ColumnSpec> = ["name", "seq"]
                    .into_iter()
                    .map(|name| ColumnSpec {
                        name: name.to_string(),
                        declared_type: String::new(),
                        not_null: false,
                        default_value: None,
                        default_text: None,
                        primary_key: false,
                        unique: false,
                        autoincrement: false,
                        collate: None,
                    })
                    .collect();
                fresh
                    .writer
                    .create_table_with_primary_key_sql("sqlite_sequence", &columns, &[], Some("CREATE TABLE sqlite_sequence(name,seq)"))
                    .map_err(|error| storage_failure(error).in_statement("vacuum"))?;
            }
            let limits = TableLimits::query();
            for row in self.writer.table_rows("sqlite_sequence", limits).map_err(|error| storage_failure(error).in_statement("vacuum"))? {
                let values = decode_record(&row.payload, encoding, RecordLimits { max_columns: 1024, max_payload_bytes: 16 * 1024 * 1024 })
                    .map_err(|error| storage_failure(error).in_statement("vacuum"))?;
                let cells: Vec<Cell> = values
                    .iter()
                    .map(|value| match value {
                        RecordValue::Null => Cell::Null,
                        RecordValue::Integer(value) => Cell::Integer(*value),
                        RecordValue::Real(value) => Cell::Real(*value),
                        RecordValue::Text(value) => Cell::Text(value.clone()),
                        RecordValue::Blob(value) => Cell::Blob(value.to_vec()),
                    })
                    .collect();
                let slots: Vec<Option<WriteValue>> = cells.iter().map(|cell| Some(dml::cell_write_value(cell))).collect();
                fresh.writer.insert("sqlite_sequence", &slots).map_err(|error| storage_failure(error).in_statement("vacuum"))?;
            }
        }
        for def in self.catalog.tables.clone() {
            if def.temp || def.name.to_ascii_lowercase().starts_with("sqlite_") {
                continue;
            }
            for (_, cells) in self.exec().live_rows(&def, "vacuum")? {
                let slots: Vec<Option<WriteValue>> = cells.iter().map(|cell| Some(dml::cell_write_value(cell))).collect();
                fresh.writer.insert(&def.name, &slots).map_err(|error| storage_failure(error).in_statement("vacuum"))?;
            }
        }
        fresh.writer.materialize_without_rowid().map_err(|error| storage_failure(error).in_statement("vacuum"))?;
        for kind in [SchemaObjectKind::Index, SchemaObjectKind::View, SchemaObjectKind::Trigger] {
            for object in &objects {
                if object.kind != kind {
                    continue;
                }
                if let Some(sql) = &object.sql {
                    fresh.execute_one(sql)?;
                }
            }
        }
        fresh.writer.materialize_indexes().map_err(|error| storage_failure(error).in_statement("vacuum"))?;
        fresh.writer.materialize_without_rowid().map_err(|error| storage_failure(error).in_statement("vacuum"))?;
        fresh.writer.set_user_version(user_version);
        fresh.writer.set_application_id(application_id);
        Ok(fresh.writer.pages().to_vec())
    }

    fn run_statement_inner(&mut self, statement: Statement) -> Result<ExecResult, SqlError> {
        match statement {
            Statement::Explain { query_plan, statement } => explain::statement(&self.exec(), query_plan, &statement),
            // 事务与 savepoint 机器住在 `database.execution::sqlite::txn`(C9b),
            // 门面只保留这里的分发点与连接级状态槽借用。
            Statement::Transaction { action } => self.stmt_ctx().apply_transaction(action),
            Statement::Attach { file, name } => self.attach_database(&file, &name),
            Statement::Detach { name } => self.detach_database(&name),
            Statement::Vacuum { into, .. } => self.vacuum_statement(into.as_ref()),
            Statement::Pragma { schema, name, value } => {
                if name.eq_ignore_ascii_case("database_list") {
                    let mut rows = vec![vec![Cell::Integer(0), Cell::Text("main".to_string()), Cell::Text(String::new())]];
                    if self.catalog.temp_created {
                        rows.push(vec![Cell::Integer(1), Cell::Text("temp".to_string()), Cell::Text(String::new())]);
                    }
                    for (index, attached) in self.attached.iter().enumerate() {
                        rows.push(vec![
                            Cell::Integer(2 + index as i64),
                            Cell::Text(attached.name.clone()),
                            Cell::Text(attached.display.clone()),
                        ]);
                    }
                    return Ok(ExecResult {
                        statement: "pragma",
                        columns: vec!["seq".to_string(), "name".to_string(), "file".to_string()],
                        rows,
                        affected: 0,
                    });
                }
                let argument = value.unwrap_or_default();
                match name.to_ascii_lowercase().as_str() {
                    "table_info" => return pragma::table_info(&self.exec(), schema.as_deref(), &argument, false),
                    "table_xinfo" => return pragma::table_info(&self.exec(), schema.as_deref(), &argument, true),
                    "index_list" => return Ok(pragma::index_list(&self.catalog, &argument)),
                    "index_info" => return Ok(pragma::index_info(&self.catalog, &argument)),
                    "foreign_key_list" => return Ok(pragma::foreign_key_list(&self.catalog, &argument)),
                    "user_version" => {
                        return Ok(match argument.is_empty() {
                            false => {
                                self.writer.set_user_version(argument.trim().parse::<i64>().unwrap_or(0));
                                ExecResult { statement: "pragma", columns: Vec::new(), rows: Vec::new(), affected: 0 }
                            }
                            true => ExecResult {
                                statement: "pragma",
                                columns: vec!["user_version".to_string()],
                                rows: vec![vec![Cell::Integer(self.writer.user_version())]],
                                affected: 0,
                            },
                        });
                    }
                    "application_id" => {
                        return Ok(match argument.is_empty() {
                            false => {
                                self.writer.set_application_id(argument.trim().parse::<i64>().unwrap_or(0));
                                ExecResult { statement: "pragma", columns: Vec::new(), rows: Vec::new(), affected: 0 }
                            }
                            true => ExecResult {
                                statement: "pragma",
                                columns: vec!["application_id".to_string()],
                                rows: vec![vec![Cell::Integer(self.writer.application_id())]],
                                affected: 0,
                            },
                        });
                    }
                    "integrity_check" => return Ok(pragma::integrity_check(&mut self.stmt_ctx(), &argument)),
                    _ => {}
                }
                let boolean = |text: &str| -> bool {
                    match text.to_ascii_lowercase().as_str() {
                        "on" | "true" | "yes" => true,
                        "off" | "false" | "no" => false,
                        other => other.parse::<i64>().map(|value| value > 0).unwrap_or(false),
                    }
                };
                let recursive = name.eq_ignore_ascii_case("recursive_triggers");
                if !name.eq_ignore_ascii_case("foreign_keys") && !recursive {
                    return Err(SqlError::new(
                        "unsupported-pragma",
                        format!(
                            "unsupported pragma `{name}`; this engine supports `foreign_keys`, `recursive_triggers`, `table_info`, `table_xinfo`, `index_list`, `index_info`, `foreign_key_list`, `database_list`, `user_version`, `application_id` and `integrity_check`"
                        ),
                    )
                    .in_clause("PRAGMA")
                    .in_statement("pragma"));
                }
                let flag = if recursive { &mut self.recursive_triggers } else { &mut self.foreign_keys };
                if argument.is_empty() {
                    Ok(ExecResult {
                        statement: "pragma",
                        columns: vec![name.clone()],
                        rows: vec![vec![Cell::Integer(i64::from(*flag))]],
                        affected: 0,
                    })
                }
                else {
                    // SQLite reads the value as a boolean: `on` / `true`
                    // / `yes` and any positive integer enable it, `off` /
                    // `false` / `no` / zero and non-numeric text disable
                    // it.
                    *flag = boolean(&argument);
                    Ok(ExecResult { statement: "pragma", columns: Vec::new(), rows: Vec::new(), affected: 0 })
                }
            }
            // 其余语句臂住在 `database.execution::sqlite::stmt`;门面的
            // `run_triggered` 只把六类门面语句送进本机器,此臂不可达。
            _ => Err(SqlError::new("unsupported-statement", "statement does not run in the facade machine").in_statement(statement.name())),
        }
    }

    /// 预编译期语句元数据:从目录推导结果列标签,绝不执行语句。
    ///
    /// `Ok(Some(标签))` 表示目录足以确定行集表头,标签与成功执行后的
    /// `ExecResult.columns` 逐字节一致(两边共用 [`core_output_labels`] 与
    /// 同一套 `Relation` 标签方法)。`Ok(None)` 表示语句没有行集结果:
    /// 无 `RETURNING` 的 DML、DDL、事务控制、`ATTACH`/`DETACH` 与写形
    /// `PRAGMA`。`Err` 表示目录解析失败(缺表、缺列、歧义列、视图递归超限
    /// 等),与执行期同一阶段的名字检查一起失败;连接层把它折叠成 `None`,
    /// 错误留待真正执行时按既有语义抛出,prepare 不提前拒绝。
    /// schema 限定名(`main.`、`temp.`、附加库、连接别名)按执行路径
    /// [`Engine::run_statement`] 的同一顺序分派,但只走目录,不写镜像。
    pub fn describe(&self, statement: &Statement) -> Result<Option<Vec<String>>, SqlError> {
        let mut statement = statement.clone();
        let scope = match stmt::leading_schema(&statement).map(str::to_owned) {
            None => {
                if let Some(index) = self.attached_target(&statement) {
                    return self.attached[index].engine.describe(&statement);
                }
                TableScope::Default
            }
            Some(schema) => {
                let lower = schema.to_ascii_lowercase();
                if self.alias.as_deref().is_some_and(|alias| alias.eq_ignore_ascii_case(&schema)) {
                    stmt::clear_statement_schema(&mut statement, &schema);
                    TableScope::Default
                }
                else if lower == "main" {
                    stmt::clear_statement_schema(&mut statement, &schema);
                    TableScope::Main
                }
                else if lower == "temp" {
                    stmt::clear_statement_schema(&mut statement, &schema);
                    TableScope::Temp
                }
                else if let Some(index) = self.attached_lookup.get(&lower).copied() {
                    stmt::clear_statement_schema(&mut statement, &schema);
                    stmt::strip_statement_sql(&mut statement, &schema);
                    return self.attached[index].engine.describe(&statement);
                }
                else {
                    return Err(stmt::unknown_schema_error(&statement, &schema));
                }
            }
        };
        describe::statement(&self.exec(), &statement, scope)
    }
}

/// Serializes results as a JSON array for the wasm host.
pub fn results_json(results: &[ExecResult]) -> String {
    let mut out = String::from("[");
    for (index, result) in results.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&result.to_json());
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 建一台空内存引擎。
    fn engine() -> Engine {
        Engine::new(None).expect("blank engine")
    }

    /// 用固定语句序列建好一台引擎。
    fn joined_engine(statements: &[&str]) -> Engine {
        let mut engine = engine();
        for statement in statements {
            engine.execute(*statement).unwrap_or_else(|error| panic!("`{statement}` failed: {error}"));
        }
        engine
    }

    // 详细正确性矩阵已整体下移到下游 SQLite 产品仓 `sqlite-conformance`（tests/engine.rs），
    // 经固定 rev 消费本门面公开面。本用例读取私有字段 `index_probes`，
    // 暂时无法用公开 API 表达，保留于此。
    #[test]
    fn integral_real_keys_match_their_literal_forms() {
        let mut engine = joined_engine(&[
            "CREATE TABLE t(id INTEGER PRIMARY KEY, label TEXT, n INTEGER)",
            "INSERT INTO t VALUES (1,'a',10),(2,'b',20),(3,'c',30)",
            "CREATE INDEX tn ON t (n)",
        ]);
        // JS 绑定的数字是 REAL 存储类；无小数部分时亲和转换后应命中同一行集。
        for (bound_sql, parameters, literal_sql) in [
            ("SELECT label FROM t WHERE id = ?", vec![Cell::Real(3.0)], "SELECT label FROM t WHERE id = 3.0"),
            ("SELECT label FROM t WHERE id = ?", vec![Cell::Real(3.5)], "SELECT label FROM t WHERE id = 3.5"),
            ("SELECT id FROM t WHERE n = ?", vec![Cell::Real(20.0)], "SELECT id FROM t WHERE n = 20.0"),
            (
                "SELECT id FROM t WHERE n BETWEEN ? AND ?",
                vec![Cell::Real(10.0), Cell::Real(20.0)],
                "SELECT id FROM t WHERE n BETWEEN 10.0 AND 20.0",
            ),
        ] {
            let bound = engine.execute_with_parameters(bound_sql, &parameters).expect("bound form").remove(0).rows;
            let literal = engine.execute_one(literal_sql).expect("literal form").rows;
            assert_eq!(bound, literal, "`{bound_sql}` must match `{literal_sql}`");
        }
        // 整数索引探针确实吃到了 REAL 键：缓存里应留下该索引的键集。
        assert!(!engine.exec_state.index_probes.borrow().is_empty(), "an integral REAL key must still use the index probe");
    }
}
