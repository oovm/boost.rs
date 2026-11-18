//! SQLite 门面的语句分发与触发器 drain / fire 机器。
//!
//! 门面 `Engine` 只保留事务、`ATTACH` / `DETACH`、`VACUUM`、`PRAGMA` 与
//! `EXPLAIN` 六类语句臂;其余语句臂、`run_statement` 分发路由、schema 限定名
//! 解析、附加库递归执行与触发器事件队列的 drain / fire 循环全部住在
//! [`StmtContext`] 上。触发体语句与附加库语句都经由本模块递归,不再回到门面。
//! 本模块不得反向依赖任何产品门面。

use std::{collections::BTreeMap, path::PathBuf, rc::Rc};

use database_query::sqlite::{SqlError, bind::*, catalog::*, frontend::*, json_string, storage_failure, value::*};
use database_storage::sqlite::{Collation, DropIndexOutcome, SchemaObjectKind, SqliteWriter, UpdateValue, WriteValue, apply_write_affinity};

use super::{
    cells::truthiness,
    ddl::{self},
    dml::{self, DmlContext, DmlRowEvent},
    eval::{Budget, EvalContext, EvalExpr, EvalReturningPlan},
    host::HostFunctionTable,
    select::{ExecContext, SqliteExecState, TableScope, join_product, row_matches},
    trigger::{self, MAX_TRIGGER_DEPTH, TriggerState, event_statement, trigger_event_matches},
    view::{self, view_relation},
};

/// Result of one executed statement.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecResult {
    /// Lowercase statement name (`create-table`, `create-index`, `insert`, `select`).
    pub statement: &'static str,
    /// Column labels in result order.
    pub columns: Vec<String>,
    /// Result rows; empty for DDL and inserts.
    pub rows: Vec<Vec<Cell>>,
    /// Rows created by an insert, or 1 for DDL.
    pub affected: usize,
}

impl ExecResult {
    /// Serializes one result as the documented JSON envelope.
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"statement\":");
        out.push_str(&json_string(self.statement));
        out.push_str(",\"affected\":");
        out.push_str(&self.affected.to_string());
        out.push_str(",\"columns\":[");
        for (index, column) in self.columns.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&json_string(column));
        }
        out.push_str("],\"rows\":[");
        for (row_index, row) in self.rows.iter().enumerate() {
            if row_index > 0 {
                out.push(',');
            }
            out.push('[');
            for (cell_index, cell) in row.iter().enumerate() {
                if cell_index > 0 {
                    out.push(',');
                }
                out.push_str(&cell_json(cell));
            }
            out.push(']');
        }
        out.push_str("]}");
        out
    }
}

fn cell_json(cell: &Cell) -> String {
    match cell {
        Cell::Null => "null".to_string(),
        Cell::Integer(value) => value.to_string(),
        Cell::Real(value) => {
            if value.is_finite() {
                // Keep the value visibly real (`1.0`, `9.223372036854776e18`)
                // so a whole-valued real never degrades into an integer when
                // the JSON envelope is parsed back.
                format!("{value:?}")
            }
            else {
                "null".to_string()
            }
        }
        Cell::Text(value) => json_string(value),
        Cell::Blob(bytes) => {
            let mut hex = String::with_capacity(bytes.len() * 2 + 12);
            hex.push_str("{\"$blob\":\"");
            for byte in bytes {
                hex.push_str(&format!("{byte:02x}"));
            }
            hex.push_str("\"}");
            hex
        }
    }
}

/// Object family an unqualified `DROP` or `ALTER` statement may target in an
/// attached database.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    /// A base table.
    Table,
    /// A named index.
    Index,
    /// A view.
    View,
    /// A trigger.
    Trigger,
}

/// The schema qualifier that decides where a statement runs.
pub fn leading_schema(statement: &Statement) -> Option<&str> {
    let primary = match statement {
        Statement::CreateTable { schema, .. }
        | Statement::CreateView { schema, .. }
        | Statement::CreateIndex { schema, .. }
        | Statement::DropTable { schema, .. }
        | Statement::DropIndex { schema, .. }
        | Statement::DropView { schema, .. }
        | Statement::DropTrigger { schema, .. }
        | Statement::AlterTable { schema, .. }
        | Statement::Insert { schema, .. }
        | Statement::Update { schema, .. }
        | Statement::Delete { schema, .. }
        | Statement::Pragma { schema, .. }
        | Statement::Vacuum { schema, .. }
        | Statement::CreateTrigger { schema, .. } => schema.as_deref(),
        _ => None,
    };
    if primary.is_some() {
        return primary;
    }
    match statement {
        Statement::CreateTrigger { table_schema, .. } => table_schema.as_deref(),
        _ => None,
    }
}

/// Drops the qualifier that matches `schema` from a statement so it can run
/// inside that schema.
pub fn clear_statement_schema(statement: &mut Statement, schema: &str) {
    fn clear(slot: &mut Option<String>, schema: &str) {
        if slot.as_deref().is_some_and(|value| value.eq_ignore_ascii_case(schema)) {
            *slot = None;
        }
    }
    match statement {
        Statement::CreateTable { schema: slot, .. }
        | Statement::CreateView { schema: slot, .. }
        | Statement::CreateIndex { schema: slot, .. }
        | Statement::DropTable { schema: slot, .. }
        | Statement::DropIndex { schema: slot, .. }
        | Statement::DropView { schema: slot, .. }
        | Statement::DropTrigger { schema: slot, .. }
        | Statement::AlterTable { schema: slot, .. }
        | Statement::Insert { schema: slot, .. }
        | Statement::Update { schema: slot, .. }
        | Statement::Delete { schema: slot, .. }
        | Statement::Pragma { schema: slot, .. }
        | Statement::Vacuum { schema: slot, .. } => clear(slot, schema),
        Statement::CreateTrigger { schema: slot, table_schema, .. } => {
            clear(slot, schema);
            clear(table_schema, schema);
        }
        _ => {}
    }
}

/// SQLite stores an attached database's objects without their schema
/// qualifier; the same rewrite keeps the stored text reloadable.
pub fn strip_statement_sql(statement: &mut Statement, schema: &str) {
    match statement {
        Statement::CreateTable { sql, .. }
        | Statement::CreateView { sql, .. }
        | Statement::CreateTrigger { sql, .. }
        | Statement::CreateIndex { sql, .. } => {
            *sql = strip_schema_qualifiers(sql, schema);
        }
        _ => {}
    }
}

/// Error for a schema name that is neither `main`, `temp`, nor attached.
pub fn unknown_schema_error(statement: &Statement, schema: &str) -> SqlError {
    match statement {
        Statement::Insert { table, .. } | Statement::Update { table, .. } | Statement::Delete { table, .. } => {
            SqlError::new("unknown-table", format!("no such table: {schema}.{table}")).in_statement(statement.name())
        }
        _ => SqlError::new("unknown-database", format!("unknown database {schema}")).in_statement(statement.name()),
    }
}

/// The schema tables are read-only for user writes.
pub fn check_readonly_table(name: &str, statement: &'static str) -> Result<(), SqlError> {
    if ddl::is_schema_table(name) || name.eq_ignore_ascii_case("sqlite_sequence") {
        return Err(SqlError::new("readonly-system-table", format!("table {name} may not be modified")).in_statement(statement));
    }
    Ok(())
}

/// One attached database's statement slot, borrowed from the owning engine.
///
/// The attached engine runs through the same [`StmtContext`] machine, so a
/// statement routed into it keeps firing that database's own triggers.
pub struct AttachedSlot<'a> {
    /// File the attached database lives in; `None` for an in-memory one.
    pub file: &'a Option<PathBuf>,
    /// The in-memory image differs from the file on disk.
    pub dirty: &'a mut bool,
    /// The attached engine's own statement context.
    pub ctx: StmtContext<'a>,
}

/// 一条语句的完整执行上下文。
///
/// 门面 `Engine` 在分发非门面语句时把自身的可变视图注入这里;分发路由、
/// 语句臂、触发器 drain / fire 与附加库递归全部经 [`StmtContext`] 完成。
pub struct StmtContext<'a> {
    /// 存储写入器:行数据的可变入口。
    pub writer: &'a mut SqliteWriter,
    /// 当前 schema 目录的可变视图。
    pub catalog: &'a mut SqliteCatalog,
    /// 跨语句缓存状态,所有权在门面。
    pub exec_state: &'a SqliteExecState,
    /// 会话宿主函数表。
    pub host_functions: &'a HostFunctionTable,
    /// 触发器事件队列与 `NEW` / `OLD` 帧栈,所有权在门面。
    pub trigger: &'a mut TriggerState,
    /// 绑定到 `?` / `:name` 占位符的参数;附加库执行会改写内层引擎的槽位。
    pub parameters: &'a mut Vec<Cell>,
    /// `ON CONFLICT DO UPDATE` 的 `excluded` 行。
    pub upsert_excluded: &'a mut Vec<Cell>,
    /// 最近一次成功写入行的 rowid,所有权在门面。
    pub last_insert_rowid: &'a mut i64,
    /// 推迟的索引 b-tree 重建标记,所有权在门面。
    pub pending_indexes: &'a mut bool,
    /// 推迟的 WITHOUT ROWID 聚簇树重建标记,所有权在门面。
    pub pending_without_rowid: &'a mut bool,
    /// 小写附加库 schema 名到附加槽位下标的映射。
    pub attached_lookup: &'a BTreeMap<String, usize>,
    /// 本数据库被附加时使用的 schema 名。
    pub alias: Option<&'a str>,
    /// 当前语句的 schema 作用域。
    pub scope: &'a mut TableScope,
    /// `PRAGMA foreign_keys` 的当前值。
    pub foreign_keys: bool,
    /// `PRAGMA recursive_triggers` 的当前值。
    pub recursive_triggers: bool,
    /// 是否处于显式事务中(延迟外键检查与附加库落盘时机的判定)。
    pub in_transaction: bool,
    /// `ATTACH DATABASE` 的每个附加库各自的语句槽位,按 attach 顺序。
    pub attached: Vec<AttachedSlot<'a>>,
}

impl<'a> StmtContext<'a> {
    /// 组装只读执行上下文:按引用共享目录、写入器、缓存与参数。
    pub fn exec(&self) -> ExecContext<'_> {
        ExecContext {
            writer: &*self.writer,
            catalog: &*self.catalog,
            state: self.exec_state,
            host_functions: self.host_functions,
            parameters: &*self.parameters,
            upsert_excluded: &*self.upsert_excluded,
            trigger_frames: &self.trigger.frames,
            alias: self.alias,
            attached: self.attached.iter().map(|slot| slot.ctx.exec()).collect::<Vec<_>>().into(),
            attached_lookup: self.attached_lookup,
        }
    }

    /// 组装写语句执行上下文:写入侧按可变引用共享,读路径仍经 [`StmtContext::exec`]。
    pub fn dml(&mut self) -> DmlContext<'_> {
        DmlContext {
            writer: &mut *self.writer,
            catalog: &mut *self.catalog,
            exec_state: self.exec_state,
            host_functions: self.host_functions,
            parameters: &*self.parameters,
            upsert_excluded: &mut *self.upsert_excluded,
            trigger_frames: &self.trigger.frames,
            alias: self.alias,
            attached: self.attached.iter().map(|slot| slot.ctx.exec()).collect::<Vec<_>>().into(),
            attached_lookup: self.attached_lookup,
            pending_events: &mut self.trigger.pending_events,
            pending_indexes: &mut *self.pending_indexes,
            pending_without_rowid: &mut *self.pending_without_rowid,
            last_insert_rowid: &mut *self.last_insert_rowid,
            foreign_keys: self.foreign_keys,
            recursive_triggers: self.recursive_triggers,
            in_transaction: self.in_transaction,
            scope: *self.scope,
        }
    }

    /// 组装 DDL / schema 持久化执行上下文:机器在 `database.execution::sqlite::ddl`。
    pub fn ddl(&mut self) -> super::ddl::DdlContext<'_> {
        super::ddl::DdlContext {
            writer: &mut *self.writer,
            catalog: &mut *self.catalog,
            exec_state: self.exec_state,
            host_functions: self.host_functions,
            parameters: &*self.parameters,
            upsert_excluded: &*self.upsert_excluded,
            trigger_frames: &self.trigger.frames,
            alias: self.alias,
            attached: self.attached.iter().map(|slot| slot.ctx.exec()).collect::<Vec<_>>().into(),
            attached_lookup: self.attached_lookup,
            scope: *self.scope,
        }
    }

    /// Whether an explicit transaction or savepoint scope is open.
    pub fn in_transaction(&self) -> bool {
        self.in_transaction
    }

    /// 写完一条语句后标记索引 b-tree 需要重建。
    fn materialize_indexes_after_write(&mut self, _statement: &'static str) -> Result<(), SqlError> {
        *self.pending_indexes = true;
        Ok(())
    }

    /// 同 [`StmtContext::materialize_indexes_after_write`],针对 WITHOUT ROWID 表的聚簇树。
    fn materialize_without_rowid_after_write(&mut self, statement: &'static str) -> Result<(), SqlError> {
        *self.pending_without_rowid = true;
        self.flush_pending_materialization(statement)
    }

    /// 把推迟的索引 / 聚簇树重建落进当前镜像。
    fn flush_pending_materialization(&mut self, statement: &'static str) -> Result<(), SqlError> {
        if *self.pending_indexes {
            self.writer.materialize_indexes().map_err(|error| storage_failure(error).in_statement(statement))?;
            *self.pending_indexes = false;
        }
        if *self.pending_without_rowid {
            self.writer.materialize_without_rowid().map_err(|error| storage_failure(error).in_statement(statement))?;
            *self.pending_without_rowid = false;
        }
        Ok(())
    }

    /// Table name as reported by diagnostics under the active schema scope.
    fn scoped_name(&self, table: &str) -> String {
        match *self.scope {
            TableScope::Default => table.to_string(),
            TableScope::Main => format!("main.{table}"),
            TableScope::Temp => format!("temp.{table}"),
        }
    }

    /// Error for a table that the active `main.` / `temp.` qualifier cannot
    /// find; an unqualified miss keeps the existing diagnostic.
    fn table_missing(&self, table: &str, statement: &'static str) -> SqlError {
        let message = match *self.scope {
            TableScope::Default => format!("unknown table `{table}`"),
            _ => format!("no such table: {}", self.scoped_name(table)),
        };
        SqlError::new("unknown-table", message).in_statement(statement)
    }

    /// Runs one statement and fires the row triggers attached to its target.
    ///
    /// A DML statement queues one event per affected row; the events are
    /// drained after the statement's writes are in place, in SQLite's
    /// `BEFORE` / `AFTER` order. Trigger bodies run through this same entry
    /// point, so a body statement still fires the other triggers. With
    /// `recursive_triggers` off, a trigger already on the stack is skipped.
    pub fn run_triggered(&mut self, statement: Statement) -> Result<ExecResult, SqlError> {
        // 全库没有任何触发器时不可能排入事件,语句不需要为「触发器失败回滚」
        // 额外开一层写事务;直接走常规语句路径。
        if self.catalog.triggers.is_empty() {
            return self.run_statement(statement);
        }
        let fired = match &statement {
            Statement::Insert { schema, table, .. } => Some((schema.as_deref(), table.clone())),
            Statement::Update { schema, table, .. } => Some((schema.as_deref(), table.clone())),
            Statement::Delete { schema, table, .. } => Some((schema.as_deref(), table.clone())),
            _ => None,
        };
        let Some((schema, table)) = fired
        else {
            return self.run_statement(statement);
        };
        if !self.target_is_local(schema, &table) {
            return self.run_statement(statement);
        }
        let saved_events = std::mem::take(&mut self.trigger.pending_events);
        // A trigger that fails has to undo the statement that fired it, like
        // SQLite's per-statement rollback, so the whole statement runs inside
        // one writer level owned here.
        self.writer.begin().map_err(|error| storage_failure(error).in_statement("trigger"))?;
        let outcome = match self.run_statement(statement) {
            Ok(result) => {
                let events = std::mem::take(&mut self.trigger.pending_events);
                let mut failure = None;
                for event in events {
                    if let Err(error) = self.fire_event(event) {
                        failure = Some(error);
                        break;
                    }
                }
                match failure {
                    Some(error) => {
                        let _ = self.writer.rollback();
                        Err(error)
                    }
                    None => {
                        self.writer.commit().map_err(|error| storage_failure(error).in_statement("trigger"))?;
                        Ok(result)
                    }
                }
            }
            Err(error) => {
                let _ = self.writer.rollback();
                Err(error)
            }
        };
        self.trigger.pending_events = saved_events;
        outcome
    }

    /// Whether an unqualified or schema-qualified target table lives in one of
    /// the schemas this engine owns; an attached target runs in its own engine
    /// and must not fire the triggers of `main`.
    fn target_is_local(&self, schema: Option<&str>, table: &str) -> bool {
        trigger::target_is_local(&*self.catalog, self.alias, self.attached.is_empty(), schema, table)
    }

    /// Fires the triggers one queued row event matches.
    fn fire_event(&mut self, event: DmlRowEvent) -> Result<(), SqlError> {
        let statement = event_statement(&event.event);
        if self.recursive_triggers && self.trigger.active_triggers.len() >= MAX_TRIGGER_DEPTH {
            return Err(SqlError::new("trigger-depth", "too many levels of trigger recursion").in_statement(statement));
        }
        let frame = TriggerFrame { columns: event.columns, new: event.new, old: event.old };
        self.trigger.frames.push(frame);
        let outcome = self.fire_matching(&event.table, &event.event, event.timing);
        self.trigger.frames.pop();
        outcome
    }

    /// Runs every trigger of `table` whose event and timing match, newest
    /// first, which is the order SQLite uses for triggers of the same event.
    fn fire_matching(&mut self, table: &str, event: &TriggerEvent, timing: TriggerTiming) -> Result<(), SqlError> {
        let mut matched: Vec<(String, TriggerDef)> = self
            .catalog
            .triggers
            .iter()
            .filter(|(_, trigger)| trigger.table == table && trigger.timing == timing && trigger_event_matches(&trigger.event, event))
            .map(|(key, trigger)| (key.clone(), trigger.clone()))
            .collect();
        matched.sort_by(|left, right| right.1.order.cmp(&left.1.order));
        let statement = event_statement(event);
        for (key, trigger) in matched {
            if !self.recursive_triggers && self.trigger.active_triggers.contains(&key) {
                continue;
            }
            if self.recursive_triggers && self.trigger.active_triggers.len() >= MAX_TRIGGER_DEPTH {
                return Err(SqlError::new("trigger-depth", "too many levels of trigger recursion").in_statement(statement));
            }
            if let Some(when) = &trigger.when {
                match self.exec().trigger_when_holds(when) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => return Err(error),
                }
            }
            self.trigger.active_triggers.push(key);
            let mut outcome = Ok(());
            for statement in trigger.body.clone() {
                outcome = self.run_triggered(statement).map(|_| ());
                if outcome.is_err() {
                    break;
                }
            }
            self.trigger.active_triggers.pop();
            outcome?;
        }
        Ok(())
    }

    /// Runs the `INSTEAD OF` triggers of one view when it has any; a view
    /// without one keeps SQLite's non-modifiable diagnostic.
    fn view_trigger_gate(&self, table: &str, key: &str, timing: TriggerTiming, event: &TriggerEvent) -> Result<(), SqlError> {
        view::view_trigger_gate(&self.catalog.triggers, table, key, timing, event)
    }

    /// `INSERT INTO <view>` dispatched to its `INSTEAD OF INSERT` triggers.
    fn run_view_insert(
        &mut self,
        table: &str,
        key: &str,
        columns: Option<&[String]>,
        rows: &[Vec<InsertValue>],
        default_values: bool,
    ) -> Result<ExecResult, SqlError> {
        let event = TriggerEvent::Insert;
        self.view_trigger_gate(table, key, TriggerTiming::InsteadOf, &event)?;
        let view = self.catalog.views.get(key).expect("view resolved").clone();
        let (names, _) = self.exec().view_output(&view)?;
        let mut frames: Vec<Vec<Cell>> = Vec::with_capacity(rows.len().max(1));
        if default_values {
            frames.push(vec![Cell::Null; names.len()]);
        }
        else {
            for values in rows {
                let mut cells = vec![Cell::Null; names.len()];
                if let Some(list) = columns {
                    for (slot, name) in list.iter().enumerate() {
                        let position = names.iter().position(|column| column.eq_ignore_ascii_case(name)).ok_or_else(|| {
                            SqlError::new("unknown-column", format!("table {table} has no column named {name}")).in_statement("insert")
                        })?;
                        if let Some(value) = values.get(slot) {
                            cells[position] = self.insert_value_cell(value, "insert")?;
                        }
                    }
                }
                else {
                    for (slot, value) in values.iter().enumerate() {
                        if let Some(cell) = cells.get_mut(slot) {
                            *cell = self.insert_value_cell(value, "insert")?;
                        }
                    }
                }
                frames.push(cells);
            }
        }
        let saved = *self.last_insert_rowid;
        for cells in frames {
            let frame = TriggerFrame { columns: names.clone(), new: Some(cells), old: None };
            self.trigger.frames.push(frame);
            let outcome = self.fire_matching(key, &event, TriggerTiming::InsteadOf);
            self.trigger.frames.pop();
            outcome?;
        }
        *self.last_insert_rowid = saved;
        Ok(ExecResult { statement: "insert", columns: Vec::new(), rows: Vec::new(), affected: 0 })
    }

    /// Evaluates one `INSERT` value, including the trigger-body expressions.
    fn insert_value_cell(&self, value: &InsertValue, statement: &'static str) -> Result<Cell, SqlError> {
        match value {
            InsertValue::Literal(literal) => Ok(literal_cell(literal)),
            InsertValue::Parameter { index, .. } => Ok(self.parameters.get(*index).cloned().unwrap_or(Cell::Null)),
            InsertValue::Expression(expression) => self.exec().eval_trigger_expression(expression, statement),
        }
    }

    /// `UPDATE <view>` dispatched to its `INSTEAD OF UPDATE` triggers.
    fn run_view_update(
        &mut self,
        table: &str,
        key: &str,
        alias: Option<&str>,
        assignments: &[Assignment],
        filter: Option<&Expr>,
        returning: Option<&SelectItems>,
    ) -> Result<ExecResult, SqlError> {
        let assigned: Vec<String> = assignments.iter().map(|assignment| assignment.column.clone()).collect();
        let event = TriggerEvent::Update(assigned);
        self.view_trigger_gate(table, key, TriggerTiming::InsteadOf, &event)?;
        if returning.is_some() {
            return Err(
                SqlError::new("unsupported-clause", "RETURNING on an INSTEAD OF view update is not supported yet").in_statement("update")
            );
        }
        let view = self.catalog.views.get(key).expect("view resolved").clone();
        let (names, rows) = self.exec().view_output(&view)?;
        // A placeholder frame lets `old.` / `new.` references resolve; the
        // values are replaced row by row below.
        let placeholder = vec![Cell::Null; names.len()];
        self.trigger.frames.push(TriggerFrame { columns: names.clone(), new: Some(placeholder.clone()), old: Some(placeholder) });
        let resolved = (|| -> Result<(Option<ResolvedExpr>, Vec<(usize, ResolvedExpr)>, ScopeShape), SqlError> {
            let relation = view_relation(alias.unwrap_or(table), &names, self.trigger.frames.last());
            let shape = relation.shape();
            let mut aggregates = Vec::new();
            let mut windows = Vec::new();
            let predicate = match filter {
                Some(filter) => Some(resolve_expr(filter, &relation, &[], &[], &mut windows, &mut aggregates, false, false)?),
                None => None,
            };
            let mut targets = Vec::with_capacity(assignments.len());
            for assignment in assignments {
                let index = names.iter().position(|column| column.eq_ignore_ascii_case(&assignment.column)).ok_or_else(|| {
                    SqlError::new("unknown-column", format!("table {table} has no column named {}", assignment.column)).in_statement("update")
                })?;
                let expression = resolve_expr(&assignment.value, &relation, &[], &[], &mut windows, &mut aggregates, false, false)?;
                targets.push((index, expression));
            }
            Ok((predicate, targets, shape))
        })();
        let (predicate, targets, shape) = match resolved {
            Ok(resolved) => resolved,
            Err(error) => {
                self.trigger.frames.pop();
                return Err(error);
            }
        };
        let budget = Budget::new();
        let saved = *self.last_insert_rowid;
        let mut outcome = Ok(());
        for row in rows {
            let row = row.get(..names.len()).map(|cells| cells.to_vec()).unwrap_or_else(|| vec![Cell::Null; names.len()]);
            match row_matches(&predicate, &row, &self.exec(), &shape, &budget) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
            let ctx = EvalContext {
                exec: self.exec(),
                parameters: &*self.parameters,
                upsert_excluded: &*self.upsert_excluded,
                trigger_frames: &self.trigger.frames,
                outer_read: &self.exec_state.outer_read,
                subquery_cache: &self.exec_state.subquery_cache,
                host_functions: self.host_functions,
                outer: &[],
                current: &shape,
                aggregates: &[],
                windows: &[],
                window_row: 0,
                budget: &budget,
                ctes: None,
            };
            let mut next = row.clone();
            for (index, expression) in &targets {
                match expression.eval(&row, &ctx) {
                    Ok(value) => next[*index] = value,
                    Err(error) => {
                        outcome = Err(error.in_statement("update"));
                        break;
                    }
                }
            }
            if outcome.is_err() {
                break;
            }
            if let Some(frame) = self.trigger.frames.last_mut() {
                frame.old = Some(row);
                frame.new = Some(next);
            }
            outcome = self.fire_matching(key, &event, TriggerTiming::InsteadOf);
            if outcome.is_err() {
                break;
            }
        }
        self.trigger.frames.pop();
        *self.last_insert_rowid = saved;
        outcome?;
        Ok(ExecResult { statement: "update", columns: Vec::new(), rows: Vec::new(), affected: 0 })
    }

    /// `DELETE FROM <view>` dispatched to its `INSTEAD OF DELETE` triggers.
    fn run_view_delete(
        &mut self,
        table: &str,
        key: &str,
        alias: Option<&str>,
        filter: Option<&Expr>,
        returning: Option<&SelectItems>,
    ) -> Result<ExecResult, SqlError> {
        let event = TriggerEvent::Delete;
        self.view_trigger_gate(table, key, TriggerTiming::InsteadOf, &event)?;
        if returning.is_some() {
            return Err(
                SqlError::new("unsupported-clause", "RETURNING on an INSTEAD OF view delete is not supported yet").in_statement("delete")
            );
        }
        let view = self.catalog.views.get(key).expect("view resolved").clone();
        let (names, rows) = self.exec().view_output(&view)?;
        let placeholder = vec![Cell::Null; names.len()];
        self.trigger.frames.push(TriggerFrame { columns: names.clone(), new: None, old: Some(placeholder) });
        let resolved = (|| -> Result<(Option<ResolvedExpr>, ScopeShape), SqlError> {
            let relation = view_relation(alias.unwrap_or(table), &names, self.trigger.frames.last());
            let shape = relation.shape();
            let mut aggregates = Vec::new();
            let mut windows = Vec::new();
            let predicate = match filter {
                Some(filter) => Some(resolve_expr(filter, &relation, &[], &[], &mut windows, &mut aggregates, false, false)?),
                None => None,
            };
            Ok((predicate, shape))
        })();
        let (predicate, shape) = match resolved {
            Ok(resolved) => resolved,
            Err(error) => {
                self.trigger.frames.pop();
                return Err(error);
            }
        };
        let budget = Budget::new();
        let saved = *self.last_insert_rowid;
        let mut outcome = Ok(());
        for row in rows {
            let row = row.get(..names.len()).map(|cells| cells.to_vec()).unwrap_or_else(|| vec![Cell::Null; names.len()]);
            match row_matches(&predicate, &row, &self.exec(), &shape, &budget) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    outcome = Err(error);
                    break;
                }
            }
            if let Some(frame) = self.trigger.frames.last_mut() {
                frame.old = Some(row);
            }
            outcome = self.fire_matching(key, &event, TriggerTiming::InsteadOf);
            if outcome.is_err() {
                break;
            }
        }
        self.trigger.frames.pop();
        *self.last_insert_rowid = saved;
        outcome?;
        Ok(ExecResult { statement: "delete", columns: Vec::new(), rows: Vec::new(), affected: 0 })
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
        let Some(schema) = leading_schema(&statement).map(str::to_owned)
        else {
            if let Some(index) = self.attached_target(&statement) {
                return self.run_attached(index, statement);
            }
            return self.run_statement_inner(statement);
        };
        let lower = schema.to_ascii_lowercase();
        if self.alias.as_deref().is_some_and(|alias| alias.eq_ignore_ascii_case(&schema)) {
            let mut statement = statement;
            clear_statement_schema(&mut statement, &schema);
            return self.run_statement_inner(statement);
        }
        if lower == "main" || lower == "temp" {
            let mut statement = statement;
            clear_statement_schema(&mut statement, &schema);
            let previous = std::mem::replace(self.scope, if lower == "main" { TableScope::Main } else { TableScope::Temp });
            let result = self.run_statement_inner(statement);
            *self.scope = previous;
            return result;
        }
        match self.attached_lookup.get(&lower).copied() {
            Some(index) => {
                let mut statement = statement;
                clear_statement_schema(&mut statement, &schema);
                strip_statement_sql(&mut statement, &schema);
                self.run_attached(index, statement)
            }
            None => Err(unknown_schema_error(&statement, &schema)),
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
            | Statement::AlterTable { schema: None, table, .. } => (table, TargetKind::Table),
            Statement::DropIndex { schema: None, name, .. } => (name, TargetKind::Index),
            Statement::DropView { schema: None, name, .. } => (name, TargetKind::View),
            Statement::DropTrigger { schema: None, name, .. } => (name, TargetKind::Trigger),
            _ => return None,
        };
        let key = name.to_ascii_lowercase();
        if kind == TargetKind::Table && (self.catalog.temp_lookup.contains_key(&key) || self.catalog.table_lookup.contains_key(&key)) {
            return None;
        }
        self.attached.iter().position(|attached| match kind {
            TargetKind::Table => attached.ctx.catalog.table_lookup.contains_key(&key),
            TargetKind::Index => attached.ctx.catalog.indexes.contains_key(&key),
            TargetKind::View => attached.ctx.catalog.views.contains_key(&key),
            TargetKind::Trigger => attached.ctx.catalog.triggers.contains_key(&key),
        })
    }

    /// Runs one statement inside an attached database with the parameters and
    /// the connection-wide last rowid carried over.
    fn run_attached(&mut self, index: usize, statement: Statement) -> Result<ExecResult, SqlError> {
        let parameters = self.parameters.clone();
        let attached = &mut self.attached[index];
        *attached.ctx.parameters = parameters;
        let result = attached.ctx.run_triggered(statement);
        let rowid = *attached.ctx.last_insert_rowid;
        attached.ctx.parameters.clear();
        let result = result?;
        *self.last_insert_rowid = rowid;
        *self.attached[index].dirty = true;
        if !self.in_transaction {
            self.persist_attached(index)?;
        }
        Ok(result)
    }

    /// Flushes one attached database image back to its file.
    fn persist_attached(&mut self, index: usize) -> Result<(), SqlError> {
        if !self.attached.get(index).is_some_and(|attached| *attached.dirty) {
            return Ok(());
        }
        if let Some(path) = self.attached[index].file.clone() {
            self.attached[index].ctx.flush_pending_materialization("save")?;
            self.attached[index].ctx.writer.save(&path).map_err(|error| storage_failure(error))?;
        }
        *self.attached[index].dirty = false;
        Ok(())
    }

    fn run_statement_inner(&mut self, statement: Statement) -> Result<ExecResult, SqlError> {
        match statement {
            // 事务、`ATTACH` / `DETACH`、`VACUUM`、`PRAGMA` 与 `EXPLAIN` 的
            // 语句臂留在门面 `Engine`;它们依赖门面的事务簿记、附加库引擎
            // 集合与宿主函数注册。只有触发体语句才可能带着它们进入本机器,
            // 此时按不可达臂如实拒绝。
            Statement::Explain { .. }
            | Statement::Transaction { .. }
            | Statement::Attach { .. }
            | Statement::Detach { .. }
            | Statement::Vacuum { .. }
            | Statement::Pragma { .. } => {
                Err(SqlError::new("unsupported-statement", "this statement only runs at the top level of a connection")
                    .in_statement(statement.name()))
            }
            Statement::CreateTable {
                schema: _,
                name,
                if_not_exists,
                columns,
                primary_key,
                table_constraints,
                checks,
                foreign_keys,
                strict,
                without_rowid,
                temp,
                query,
                sql,
            } => {
                if let Some(query) = query {
                    self.ddl().create_table_as(name, if_not_exists, temp, &query)?;
                    return Ok(ExecResult { statement: "create-table", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                }
                let key = name.to_ascii_lowercase();
                // A WITHOUT ROWID table's writer call needs the key columns
                // whether they were declared inline or as a table constraint.
                let without_rowid_keys: Vec<String> = if primary_key.is_empty() {
                    columns.iter().filter(|column| column.primary_key).map(|column| column.name.clone()).collect()
                }
                else {
                    primary_key.clone()
                };
                ddl::check_reserved_object_name(&name, "create-table")?;
                if temp {
                    // The temp schema owns a separate namespace: a main object
                    // with the same name can coexist and temp wins in name
                    // resolution.
                    if self.catalog.temp_lookup.contains_key(&key) {
                        if if_not_exists {
                            return Ok(ExecResult { statement: "create-table", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                        }
                        return Err(SqlError::new("table-exists", format!("table {name} already exists")).in_statement("create-table"));
                    }
                    let mut def =
                        SqliteCatalog::table_entry(name, columns, primary_key, table_constraints, checks, foreign_keys, strict, without_rowid)?;
                    def.temp = true;
                    // SQLite reuses a freed temp page when one is available
                    // and otherwise extends the temp database by one page.
                    let rootpage = match self.catalog.temp_free_pages.pop_first() {
                        Some(page) => page,
                        None => {
                            let page = self.catalog.temp_next_page;
                            self.catalog.temp_next_page += 1;
                            page
                        }
                    };
                    let master_rowid = self.catalog.temp_next_rowid;
                    self.catalog.temp_next_rowid += 1;
                    let position = self.catalog.temp_tables.len();
                    self.catalog.temp_lookup.insert(key, position);
                    self.catalog.temp_tables.push(TempTable { def, sql, rootpage, master_rowid, rows: BTreeMap::new() });
                    self.catalog.temp_created = true;
                    return Ok(ExecResult { statement: "create-table", columns: Vec::new(), rows: Vec::new(), affected: 1 });
                }
                if self.catalog.indexes.contains_key(&key) {
                    return Err(SqlError::new("index-exists", format!("there is already an index named {name}")).in_statement("create-table"));
                }
                if self.catalog.views.contains_key(&key) {
                    return Err(SqlError::new("view-exists", format!("there is already a view named {name}")).in_statement("create-table"));
                }
                if self.catalog.table_lookup.contains_key(&key) {
                    if if_not_exists {
                        return Ok(ExecResult { statement: "create-table", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                    }
                    return Err(SqlError::new("table-exists", format!("table {name} already exists")).in_statement("create-table"));
                }
                let def = SqliteCatalog::table_entry(
                    name.clone(),
                    columns.clone(),
                    primary_key.clone(),
                    table_constraints.clone(),
                    checks.clone(),
                    foreign_keys.clone(),
                    strict,
                    without_rowid,
                )?;
                if without_rowid {
                    self.writer
                        .create_table_without_rowid_sql(&name, &columns, &without_rowid_keys, Some(&sql))
                        .map_err(|error| storage_failure(error).in_statement("create-table"))?;
                }
                else {
                    self.writer
                        .create_table_with_primary_key_sql(&name, &columns, &primary_key, Some(&sql))
                        .map_err(|error| storage_failure(error).in_statement("create-table"))?;
                }
                // Appending keeps the catalog in creation order, which is the
                // order `sqlite_master` reports.
                let position = self.catalog.tables.len();
                self.catalog.table_lookup.insert(key, position);
                self.catalog.tables.push(def);
                Ok(ExecResult { statement: "create-table", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            Statement::CreateView { schema, name, if_not_exists, columns, query, temp, sql } => {
                let key = name.to_ascii_lowercase();
                ddl::check_reserved_object_name(&name, "create-view")?;
                // `CREATE TEMP VIEW` and `CREATE VIEW temp.v` both target the
                // connection-owned schema, whose objects shadow main ones.
                let temp_target = temp
                    || *self.scope == TableScope::Temp
                    || schema.as_deref().is_some_and(|qualifier| qualifier.eq_ignore_ascii_case("temp"));
                if temp_target {
                    if self.catalog.temp_lookup.contains_key(&key) || self.catalog.temp_views.contains_key(&key) {
                        if if_not_exists {
                            return Ok(ExecResult { statement: "create-view", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                        }
                        return Err(SqlError::new("table-exists", format!("table {name} already exists")).in_statement("create-view"));
                    }
                    // `sqlite_temp_master` stores the statement without `TEMP`
                    // and without a `temp.` qualifier.
                    let stored = schema.as_deref().map(|qualifier| strip_schema_qualifiers(&sql, qualifier)).unwrap_or(sql);
                    let master_rowid = self.catalog.temp_next_rowid;
                    self.catalog.temp_next_rowid += 1;
                    self.catalog.temp_views.insert(key, TempView { def: ViewDef { name, columns, query: *query }, sql: stored, master_rowid });
                    self.catalog.temp_created = true;
                    return Ok(ExecResult { statement: "create-view", columns: Vec::new(), rows: Vec::new(), affected: 1 });
                }
                if self.catalog.indexes.contains_key(&key) {
                    return Err(SqlError::new("index-exists", format!("there is already an index named {name}")).in_statement("create-view"));
                }
                if self.catalog.views.contains_key(&key) {
                    if if_not_exists {
                        return Ok(ExecResult { statement: "create-view", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                    }
                    return Err(SqlError::new("view-exists", format!("view {name} already exists")).in_statement("create-view"));
                }
                if self.catalog.tables.iter().any(|def| def.name.eq_ignore_ascii_case(&name)) {
                    if if_not_exists {
                        return Ok(ExecResult { statement: "create-view", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                    }
                    return Err(SqlError::new("table-exists", format!("table {name} already exists")).in_statement("create-view"));
                }
                self.writer.create_view(&name, &sql).map_err(|error| storage_failure(error).in_statement("create-view"))?;
                self.catalog.views.insert(key, ViewDef { name, columns, query: *query });
                Ok(ExecResult { statement: "create-view", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            Statement::DropView { schema, name, if_exists } => {
                let key = name.to_ascii_lowercase();
                let temp_target =
                    *self.scope == TableScope::Temp || schema.as_deref().is_some_and(|qualifier| qualifier.eq_ignore_ascii_case("temp"));
                // Unqualified `DROP VIEW` searches temp before main, so a temp
                // view disappears while its main namesake survives.
                if schema.is_none() || temp_target {
                    if self.catalog.temp_views.remove(&key).is_some() {
                        self.catalog.triggers.retain(|_, trigger| !(trigger.temp && trigger.table == key));
                        return Ok(ExecResult { statement: "drop-view", columns: Vec::new(), rows: Vec::new(), affected: 1 });
                    }
                    if temp_target {
                        if if_exists {
                            return Ok(ExecResult { statement: "drop-view", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                        }
                        return Err(SqlError::new("unknown-view", format!("no such view: {name}")).in_statement("drop-view"));
                    }
                }
                if self.catalog.tables.iter().any(|def| def.name.eq_ignore_ascii_case(&name)) {
                    return Err(SqlError::new("wrong-object", format!("use DROP TABLE to delete table {name}")).in_statement("drop-view"));
                }
                // An index is not a view, so SQLite reports it as missing
                // rather than as the wrong object kind.
                if self.catalog.indexes.contains_key(&key) {
                    if if_exists {
                        return Ok(ExecResult { statement: "drop-view", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                    }
                    return Err(SqlError::new("unknown-view", format!("no such view: {name}")).in_statement("drop-view"));
                }
                if self.catalog.views.remove(&key).is_none() {
                    if if_exists {
                        return Ok(ExecResult { statement: "drop-view", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                    }
                    return Err(SqlError::new("unknown-view", format!("no such view: {name}")).in_statement("drop-view"));
                }
                self.writer.drop_view(&name).map_err(|error| storage_failure(error).in_statement("drop-view"))?;
                self.catalog.triggers.retain(|_, trigger| trigger.table != key);
                Ok(ExecResult { statement: "drop-view", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            Statement::CreateTrigger { schema: _, name, if_not_exists, timing, event, table_schema: _, table, when, body, sql } => {
                let key = name.to_ascii_lowercase();
                ddl::check_reserved_object_name(&name, "create-trigger")?;
                if self.catalog.triggers.contains_key(&key) {
                    if if_not_exists {
                        return Ok(ExecResult { statement: "create-trigger", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                    }
                    return Err(SqlError::new("trigger-exists", format!("trigger {name} already exists")).in_statement("create-trigger"));
                }
                let table_key = table.to_ascii_lowercase();
                let temp_target = if *self.scope == TableScope::Main { None } else { self.catalog.temp_lookup.get(&table_key).copied() };
                let main_target = if temp_target.is_some() { None } else { self.catalog.table_lookup.get(&table_key).copied() };
                let view_target =
                    if temp_target.is_some() || main_target.is_some() { false } else { self.catalog.views.contains_key(&table_key) };
                if temp_target.is_none() && main_target.is_none() && !view_target {
                    let qualifier = match *self.scope {
                        TableScope::Temp => "temp",
                        _ => "main",
                    };
                    return Err(SqlError::new("unknown-table", format!("no such table: {qualifier}.{table}")).in_statement("create-trigger"));
                }
                let timing_text = match timing {
                    TriggerTiming::Before => "BEFORE",
                    TriggerTiming::After => "AFTER",
                    TriggerTiming::InsteadOf => "INSTEAD OF",
                };
                if view_target && !matches!(timing, TriggerTiming::InsteadOf) {
                    return Err(SqlError::new("trigger-target", format!("cannot create {timing_text} trigger on view: {table}"))
                        .in_statement("create-trigger"));
                }
                if !view_target && matches!(timing, TriggerTiming::InsteadOf) {
                    return Err(SqlError::new("trigger-target", format!("cannot create INSTEAD OF trigger on table: {table}"))
                        .in_statement("create-trigger"));
                }
                let temp = *self.scope == TableScope::Temp || temp_target.is_some();
                let order = self.catalog.trigger_sequence;
                self.catalog.trigger_sequence += 1;
                let master_rowid = if temp {
                    let rowid = self.catalog.temp_next_rowid;
                    self.catalog.temp_next_rowid += 1;
                    self.catalog.temp_created = true;
                    rowid
                }
                else {
                    self.writer.create_trigger(&name, &table, &sql).map_err(|error| storage_failure(error).in_statement("create-trigger"))?;
                    0
                };
                self.catalog
                    .triggers
                    .insert(key, TriggerDef { name, table: table_key, timing, event, when, body, order, temp, sql, master_rowid });
                Ok(ExecResult { statement: "create-trigger", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            Statement::DropTrigger { schema: _, name, if_exists } => {
                let key = name.to_ascii_lowercase();
                let Some(trigger) = self.catalog.triggers.remove(&key)
                else {
                    if if_exists {
                        return Ok(ExecResult { statement: "drop-trigger", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                    }
                    return Err(SqlError::new("unknown-trigger", format!("no such trigger: {name}")).in_statement("drop-trigger"));
                };
                if !trigger.temp {
                    self.writer.drop_trigger(&trigger.name).map_err(|error| storage_failure(error).in_statement("drop-trigger"))?;
                }
                Ok(ExecResult { statement: "drop-trigger", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            Statement::Insert { schema: _, table, columns, rows, default_values, query, replace, ignore, conflict, returning } => {
                let key = table.to_ascii_lowercase();
                check_readonly_table(&table, "insert")?;
                // A `SELECT` source is evaluated before any row is written, so
                // `INSERT INTO t SELECT ... FROM t` reads the table's original
                // contents, exactly like SQLite.
                let selected_rows: Option<Vec<Vec<Cell>>> = match &query {
                    Some(query) => {
                        let budget = Budget::new();
                        Some(self.exec().select_statement(query, &[], &budget, None)?.rows)
                    }
                    None => None,
                };
                if self.catalog.views.contains_key(&key) {
                    let literal_rows: Vec<Vec<InsertValue>> = match &selected_rows {
                        Some(rows) => rows.iter().map(|row| row.iter().map(dml::cell_insert_value).collect()).collect(),
                        None => rows.clone(),
                    };
                    return self.run_view_insert(&table, &key, columns.as_deref(), &literal_rows, default_values);
                }
                let temp_position = if *self.scope == TableScope::Main { None } else { self.catalog.temp_lookup.get(&key).copied() };
                let main_position = if *self.scope == TableScope::Temp {
                    None
                }
                else {
                    match temp_position {
                        Some(_) => None,
                        None => Some(*self.catalog.table_lookup.get(&key).ok_or_else(|| self.table_missing(&table, "insert"))?),
                    }
                };
                // Temp tables are mutated in memory, so their definition is
                // cloned: the loop below needs the catalog while it borrows
                // the temp row map mutably.
                let def_storage = match temp_position {
                    Some(position) => self.catalog.temp_tables[position].def.clone(),
                    None => self.catalog.tables[main_position.expect("main table resolved")].clone(),
                };
                let def = &def_storage;
                let returning_shape = TargetShape::new(def, &table, None);
                let returning = match &returning {
                    Some(items) => Some(returning_shape.plan(def, items, "insert")?),
                    None => None,
                };
                let upsert = match &conflict {
                    Some(clause) => {
                        if replace {
                            return Err(SqlError::new("syntax", "ON CONFLICT clause is not allowed with REPLACE").in_statement("insert"));
                        }
                        Some(self.exec().prepare_upsert(def, &key, &table, clause)?)
                    }
                    None => None,
                };
                // 只追加行的语句可以把既有索引探针按新行增量补齐,不必等下一条
                // 查询整份重建。带 `ON CONFLICT` / `REPLACE` / 触发器的语句可能
                // 改写或删除既有行,仍走常规失效。
                let watch_probes = conflict.is_none()
                    && !replace
                    && temp_position.is_none()
                    && !self.exec().table_has_triggers(&key)
                    && self.exec().table_has_probes(&key);
                let mut probe_rows: Vec<(i64, Vec<Cell>)> = Vec::new();
                let fast_upsert = upsert.as_ref().is_some_and(|plan| self.dml().upsert_rowid_fast_path(def, &key, plan, temp_position));
                let mut upsert_live = if upsert.is_some() && !fast_upsert { self.exec().live_rows(def, "insert")? } else { Vec::new() };
                let mut upsert_live_loaded = upsert.is_some() && !fast_upsert;
                let mut slots_by_row = Vec::with_capacity(rows.len().max(1));
                if default_values {
                    // `DEFAULT VALUES` leaves every slot unset so the writer fills
                    // it from the column default (and reports NOT NULL violations).
                    slots_by_row.push(vec![None; def.columns.len()]);
                }
                for values in &rows {
                    let mut resolved: Vec<WriteValue> = Vec::with_capacity(values.len());
                    for value in values {
                        let cell = match value {
                            InsertValue::Literal(literal) => literal_cell(literal),
                            InsertValue::Parameter { index, .. } => self.parameters.get(*index).cloned().unwrap_or(Cell::Null),
                            InsertValue::Expression(expression) => self.exec().eval_trigger_expression(expression, "insert")?,
                        };
                        resolved.push(dml::cell_write_value(&cell));
                    }
                    slots_by_row.push(dml::self_contained_arity(def, &table, columns.as_deref(), &resolved)?);
                }
                if let Some(rows) = &selected_rows {
                    for row in rows {
                        let resolved: Vec<WriteValue> = row.iter().map(dml::cell_write_value).collect();
                        slots_by_row.push(dml::select_arity(def, &table, columns.as_deref(), &resolved)?);
                    }
                }
                // SQLite applies column affinity to every supplied value before
                // the row reaches conflict detection: a `REAL` bound to an
                // `INTEGER`-affinity column with no fractional part becomes an
                // integer. Normalize the slots here so the upsert rowid probe and
                // the scanning conflict path compare the same storage class the
                // writer will store, instead of missing the conflict and surfacing
                // a spurious `UNIQUE` error.
                for slots in &mut slots_by_row {
                    for (position, slot) in slots.iter_mut().enumerate() {
                        if let Some(value) = slot.take() {
                            *slot = Some(apply_write_affinity(&def.columns[position].declared_type, value));
                        }
                    }
                }
                // A `STRICT` table rejects a value whose post-affinity storage
                // class differs from the declared type, before any row is
                // written. `INSERT OR IGNORE` skips the offending row instead.
                if ignore {
                    slots_by_row.retain(|slots| {
                        slots.iter().enumerate().all(|(position, slot)| match slot {
                            Some(value) => dml::strict_value_violation(def, position, value, "insert").is_none(),
                            None => true,
                        })
                    });
                }
                else {
                    for slots in &slots_by_row {
                        for (position, slot) in slots.iter().enumerate() {
                            if let Some(value) = slot {
                                if let Some(error) = dml::strict_value_violation(def, position, value, "insert") {
                                    return Err(error);
                                }
                            }
                        }
                    }
                }
                // One row at a time inside a transaction: a failing `CHECK`
                // must undo every row of a multi-row statement, as SQLite does.
                // Temp tables snapshot their row map instead of opening one.
                let temp_snapshot = temp_position.map(|position| self.catalog.temp_tables[position].rows.clone());
                if temp_position.is_none() {
                    self.writer.begin().map_err(|error| storage_failure(error).in_statement("insert"))?;
                }
                let temp_keys = if temp_position.is_some() { self.exec().unique_keys(def, &key) } else { Vec::new() };
                let check_shape = ScopeShape { columns: Vec::new() };
                let check_budget = Budget::new();
                let queue_triggers = self.exec().table_has_triggers(&key);
                let trigger_columns = dml::trigger_frame_columns(def);
                let recursive_triggers = self.recursive_triggers;
                let mut replace_deleted: Vec<(i64, Vec<Cell>)> = Vec::new();
                let mut replace_index_needed = false;
                let mut replace_without_needed = false;
                let mut rowids = Vec::with_capacity(slots_by_row.len());
                let mut last_written: Option<i64> = None;
                let mut affected = 0usize;
                let mut returned: Vec<Vec<Cell>> = Vec::new();
                let mut failure = None;
                for slots in &slots_by_row {
                    // A `BEFORE INSERT` trigger sees the row before it is
                    // written: defaults are already applied and an unassigned
                    // rowid is `-1`, exactly like SQLite.
                    if queue_triggers && !replace {
                        let before_rowid = def
                            .rowid_alias
                            .and_then(|alias| slots.get(alias).and_then(|slot| slot.as_ref()))
                            .and_then(|value| match value {
                                WriteValue::Integer(value) => Some(*value),
                                _ => None,
                            })
                            .unwrap_or(-1);
                        let cells = dml::check_row(def, slots, before_rowid);
                        self.dml().queue_insert_before(&key, &trigger_columns, cells);
                    }
                    let outcome = if replace {
                        match self.dml().apply_replace(
                            def,
                            &key,
                            &table,
                            slots,
                            temp_position,
                            recursive_triggers,
                            queue_triggers,
                            &trigger_columns,
                            &mut replace_deleted,
                        ) {
                            Ok(rowid) => dml::UpsertOutcome::Inserted(rowid),
                            Err(error) => {
                                failure = Some(error);
                                break;
                            }
                        }
                    }
                    else {
                        match &upsert {
                            Some(plan) => {
                                let fast = if fast_upsert {
                                    Some(self.dml().apply_upsert_rowid(
                                        def,
                                        &key,
                                        &table,
                                        plan,
                                        slots,
                                        returning.as_ref(),
                                        &returning_shape.scope,
                                        &check_budget,
                                    ))
                                }
                                else {
                                    None
                                };
                                let outcome = match fast {
                                    Some(Ok(Some(outcome))) => Ok(outcome),
                                    Some(Err(error)) => Err(error),
                                    Some(Ok(None)) | None => {
                                        if !upsert_live_loaded {
                                            upsert_live = match self.exec().live_rows(def, "insert") {
                                                Ok(rows) => rows,
                                                Err(error) => {
                                                    failure = Some(error);
                                                    break;
                                                }
                                            };
                                            upsert_live_loaded = true;
                                        }
                                        self.dml().apply_upsert(
                                            def,
                                            &key,
                                            &table,
                                            plan,
                                            slots,
                                            temp_position,
                                            &mut upsert_live,
                                            returning.as_ref(),
                                            &returning_shape.scope,
                                            &check_budget,
                                        )
                                    }
                                };
                                match outcome {
                                    Ok(outcome) => outcome,
                                    Err(error) => {
                                        failure = Some(error);
                                        break;
                                    }
                                }
                            }
                            None => {
                                let inserted = if let Some(position) = temp_position {
                                    let table = &mut self.catalog.temp_tables[position];
                                    dml::temp_insert_row(def, &mut table.rows, slots, &temp_keys)
                                }
                                else {
                                    self.writer.insert(&table, slots).map_err(|error| storage_failure(error).in_statement("insert"))
                                };
                                match inserted {
                                    Ok(rowid) => dml::UpsertOutcome::Inserted(rowid),
                                    Err(error) if ignore && dml::ignoreable_insert_error(&error) => continue,
                                    Err(error) => {
                                        failure = Some(error);
                                        break;
                                    }
                                }
                            }
                        }
                    };
                    let rowid = match outcome {
                        dml::UpsertOutcome::Skipped => continue,
                        dml::UpsertOutcome::Inserted(rowid) => {
                            affected += 1;
                            rowid
                        }
                        dml::UpsertOutcome::Updated { rowid, rows } => {
                            // `plan_update` already validated the rewritten row
                            // and evaluated `RETURNING` against it.
                            affected += 1;
                            last_written = Some(rowid);
                            returned.extend(rows);
                            continue;
                        }
                    };
                    // 行像只在 CHECK / RETURNING / 触发器 / 索引探针缓存需要时
                    // 才拼出来:普通插入路径不再为每行复制一份单元格向量。
                    let needs_cells = !def.checks.is_empty() || returning.is_some() || queue_triggers || watch_probes;
                    let cells = if needs_cells { dml::check_row(def, slots, rowid) } else { Vec::new() };
                    let ctx = EvalContext {
                        exec: self.exec(),
                        parameters: &*self.parameters,
                        upsert_excluded: &*self.upsert_excluded,
                        trigger_frames: &self.trigger.frames,
                        outer_read: &self.exec_state.outer_read,
                        subquery_cache: &self.exec_state.subquery_cache,
                        host_functions: self.host_functions,
                        outer: &[],
                        current: &check_shape,
                        aggregates: &[],
                        windows: &[],
                        window_row: 0,
                        budget: &check_budget,
                        ctes: None,
                    };
                    if let Err(error) = self.exec().check_row_constraints(def, &cells, &check_shape, &check_budget) {
                        failure = Some(error);
                    }
                    if failure.is_some() {
                        break;
                    }
                    // The row passed the writer's NOT NULL / UNIQUE / PRIMARY
                    // KEY checks and this statement's CHECK constraints, so
                    // SQLite would have recorded its rowid already. A later
                    // failure rolls the row back but does not undo the record.
                    last_written = Some(rowid);
                    if let Some(plan) = &returning {
                        match plan.eval(&cells, &ctx) {
                            Ok(row) => returned.push(row),
                            Err(error) => {
                                failure = Some(error);
                                break;
                            }
                        }
                    }
                    rowids.push(rowid);
                    if watch_probes {
                        probe_rows.push((rowid, cells.clone()));
                    }
                    if queue_triggers {
                        self.dml().queue_insert_after(&key, &trigger_columns, &cells);
                    }
                }
                // Referential checks run after every row is written and before
                // the transaction commits: a failing statement leaves no row
                // behind, and a self-reference may point at a later row of the
                // same statement.
                // 只有本表或被别的表引用时才需要外键检查:没有外键关系的表上
                // `REPLACE` 不再为每条语句做一次全表快照。
                let replace_needs_foreign_keys = self.foreign_keys
                    && temp_position.is_none()
                    && (!def.foreign_keys.is_empty() || !self.exec().referencing_foreign_keys(&def.name).is_empty());
                if failure.is_none() && replace_needs_foreign_keys && !replace_deleted.is_empty() {
                    let changes: Vec<(Vec<Cell>, Option<Vec<Cell>>)> = replace_deleted.iter().map(|(_, cells)| (cells.clone(), None)).collect();
                    match self.dml().apply_foreign_key_actions(def, &changes, "insert", 0) {
                        Ok((index_needed, without_needed)) => {
                            replace_index_needed |= index_needed;
                            replace_without_needed |= without_needed;
                        }
                        Err(error) => failure = Some(error),
                    }
                    if failure.is_none() {
                        let removed: Vec<i64> = replace_deleted.iter().map(|(rowid, _)| *rowid).collect();
                        // 只有开启外键时才需要整表快照：普通 `REPLACE` 不再为每条
                        // 语句扫一遍全表。
                        let replace_old_rows = self.exec().live_rows(def, "insert")?;
                        if let Err(error) =
                            self.exec().check_delete_foreign_keys(def, &replace_old_rows, &removed, "insert", self.in_transaction())
                        {
                            failure = Some(error);
                        }
                    }
                }
                if failure.is_none() && self.foreign_keys {
                    if let Err(error) = self.exec().check_insert_foreign_keys(def, &rowids, "insert", self.in_transaction()) {
                        failure = Some(error);
                    }
                }
                if let Some(rowid) = last_written {
                    if !def.without_rowid {
                        *self.last_insert_rowid = rowid;
                    }
                }
                if let Some(error) = failure {
                    match (temp_position, temp_snapshot) {
                        (Some(position), Some(snapshot)) => self.catalog.temp_tables[position].rows = snapshot,
                        _ => {
                            let _ = self.writer.rollback();
                        }
                    }
                    return Err(error);
                }
                if temp_position.is_none() {
                    if let Err(error) = self.dml().refresh_managed_indexes(&key, def, "insert") {
                        let _ = self.writer.rollback();
                        return Err(error);
                    }
                    self.writer.commit().map_err(|error| storage_failure(error).in_statement("insert"))?;
                    if watch_probes {
                        self.exec().patch_index_probes_after_insert(&key, &probe_rows);
                    }
                }
                // Explicit indexes and the automatic ones built for any
                // `PRIMARY KEY` / `UNIQUE` constraint live only as key sets
                // until they are materialized into b-tree pages, so a write to
                // an indexed table has to rebuild them before the image is read.
                if temp_position.is_none() && (self.exec().has_indexes(def, &key) || replace_index_needed) {
                    self.materialize_indexes_after_write("insert")?;
                }
                if temp_position.is_none() && (def.without_rowid || replace_without_needed) {
                    self.materialize_without_rowid_after_write("insert")?;
                }
                if let Some(plan) = &returning {
                    return Ok(ExecResult { statement: "insert", columns: plan.columns.clone(), rows: returned, affected });
                }
                if default_values {
                    // `DEFAULT VALUES` reports neither columns nor rows, like
                    // SQLite; the stored row is written the same way.
                    return Ok(ExecResult { statement: "insert", columns: Vec::new(), rows: Vec::new(), affected });
                }
                if def.without_rowid {
                    // A WITHOUT ROWID insert has no rowid to report.
                    return Ok(ExecResult { statement: "insert", columns: Vec::new(), rows: Vec::new(), affected });
                }
                let rows = rowids.into_iter().map(|rowid| vec![Cell::Integer(rowid)]).collect();
                Ok(ExecResult { statement: "insert", columns: vec!["rowid".to_string()], rows, affected })
            }
            Statement::CreateIndex { schema: _, name, table, if_not_exists, unique, columns, predicate, sql } => {
                let table_key = table.to_ascii_lowercase();
                if ddl::is_schema_table(&table) {
                    return Err(
                        SqlError::new("readonly-system-table", format!("table {table} may not be indexed")).in_statement("create-index")
                    );
                }
                if self.catalog.views.contains_key(&table_key) {
                    return Err(SqlError::new("readonly-view", "views may not be indexed").in_statement("create-index"));
                }
                ddl::check_reserved_object_name(&name, "create-index")?;
                let position = *self
                    .catalog
                    .table_lookup
                    .get(&table_key)
                    .ok_or_else(|| SqlError::new("unknown-table", format!("unknown table `{table}`")).in_statement("create-index"))?;
                let def_storage = self.catalog.tables[position].clone();
                let def = &def_storage;
                let key = name.to_ascii_lowercase();
                if self.catalog.table_lookup.contains_key(&key) {
                    return Err(SqlError::new("table-exists", format!("there is already a table named `{name}`")).in_statement("create-index"));
                }
                if self.catalog.indexes.contains_key(&key) {
                    if if_not_exists {
                        return Ok(ExecResult { statement: "create-index", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                    }
                    return Err(SqlError::new("index-exists", format!("index `{name}` already exists")).in_statement("create-index"));
                }
                if self.catalog.views.contains_key(&key) {
                    return Err(
                        SqlError::new("index-exists", format!("there is already an object named `{name}`")).in_statement("create-index")
                    );
                }
                // Expression keys and a `WHERE` predicate make the key values
                // row-dependent, so the engine computes them and hands the exact
                // entries to the writer.
                let managed = predicate.is_some() || columns.iter().any(|column| column.expr.is_some() || column.collate.is_some());
                if managed {
                    let shape = TargetShape::new(def, &table, None);
                    let mut terms = Vec::with_capacity(columns.len());
                    let mut collations = Vec::with_capacity(columns.len());
                    for column in &columns {
                        let resolved = match &column.expr {
                            Some(expr) => shape.resolve(expr, "create-index")?,
                            None => {
                                let position =
                                    def.columns.iter().position(|existing| existing.name.eq_ignore_ascii_case(&column.name)).ok_or_else(
                                        || {
                                            SqlError::new("unknown-column", format!("unknown column `{}` in table `{table}`", column.name))
                                                .in_statement("create-index")
                                        },
                                    )?;
                                ResolvedExpr::Column { source: 0, index: position }
                            }
                        };
                        // 键项显式 `COLLATE` 优先，裸列继承列声明，表达式按 `BINARY`。
                        collations.push(match &column.collate {
                            Some(name) => Collation::parse(name).ok_or_else(|| unknown_collation(name).in_statement("create-index"))?,
                            None => match &column.expr {
                                Some(_) => Collation::Binary,
                                None => def
                                    .columns
                                    .iter()
                                    .find(|existing| existing.name.eq_ignore_ascii_case(&column.name))
                                    .and_then(|existing| existing.collate)
                                    .unwrap_or_default(),
                            },
                        });
                        terms.push(resolved);
                    }
                    let predicate = shape.resolve_optional(predicate.as_ref(), "create-index")?;
                    let rows = self.exec().live_rows(def, "create-index")?;
                    let entries = self.exec().managed_index_entries(def, &terms, predicate.as_ref(), &rows)?;
                    if unique {
                        let names = columns.iter().map(|column| format!("{table}.{}", column.name)).collect::<Vec<_>>().join(", ");
                        if dml::managed_unique_violation(&entries, &collations) {
                            return Err(
                                SqlError::new("unique-constraint", format!("UNIQUE constraint failed: {names}")).in_statement("create-index")
                            );
                        }
                    }
                    self.writer
                        .create_managed_index(&name, &table, unique, &sql, &entries)
                        .map_err(|error| storage_failure(error).in_statement("create-index"))?;
                    let order = self.catalog.index_sequence;
                    self.catalog.index_sequence += 1;
                    self.catalog.indexes.insert(
                        key,
                        IndexDef {
                            name: name.clone(),
                            table: table_key,
                            columns: Vec::new(),
                            unique,
                            order,
                            managed: true,
                            terms,
                            predicate,
                            collations,
                        },
                    );
                    return Ok(ExecResult { statement: "create-index", columns: Vec::new(), rows: Vec::new(), affected: 1 });
                }
                let mut positions = Vec::with_capacity(columns.len());
                let mut collations = Vec::with_capacity(columns.len());
                for column in &columns {
                    let position =
                        def.columns.iter().position(|existing| existing.name.eq_ignore_ascii_case(&column.name)).ok_or_else(|| {
                            SqlError::new("unknown-column", format!("unknown column `{}` in table `{table}`", column.name))
                                .in_statement("create-index")
                        })?;
                    positions.push(position);
                    collations.push(match &column.collate {
                        Some(name) => Collation::parse(name).ok_or_else(|| unknown_collation(name).in_statement("create-index"))?,
                        None => def.columns[position].collate.unwrap_or_default(),
                    });
                }
                if unique {
                    self.exec().ensure_unique_keys(&table, def, &columns)?;
                }
                let names = columns.iter().map(|column| column.name.clone()).collect::<Vec<_>>();
                self.writer
                    .create_index_with_sql(&name, &table, &names, unique, Some(&sql))
                    .map_err(|error| storage_failure(error).in_statement("create-index"))?;
                // 空表上新键的索引:探针从空集开始,后续 `INSERT` / `DELETE`
                // 逐行增量维护,首次区间删除或等值查询不必再整份解码索引。
                let empty_index = self.writer.index_is_empty(&name).unwrap_or(false);
                let order = self.catalog.index_sequence;
                self.catalog.index_sequence += 1;
                self.catalog.indexes.insert(
                    key,
                    IndexDef {
                        name: name.clone(),
                        table: table_key,
                        columns: positions,
                        unique,
                        order,
                        managed: false,
                        terms: Vec::new(),
                        predicate: None,
                        collations,
                    },
                );
                if empty_index {
                    self.exec().install_empty_index_probe(&name);
                }
                Ok(ExecResult { statement: "create-index", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            Statement::Update { schema: _, table, alias, assignments, from, filter, returning } => {
                check_readonly_table(&table, "update")?;
                let key = table.to_ascii_lowercase();
                if self.catalog.views.contains_key(&key) {
                    if !from.is_empty() {
                        let mut error =
                            SqlError::new("unsupported-clause", "`UPDATE ... FROM` is not supported on a view; name a base table instead");
                        error.clause = Some("FROM".to_string());
                        return Err(error.in_statement("update"));
                    }
                    return self.run_view_update(&table, &key, alias.as_deref(), &assignments, filter.as_ref(), returning.as_ref());
                }
                let temp_position = if *self.scope == TableScope::Main { None } else { self.catalog.temp_lookup.get(&key).copied() };
                let main_position = if *self.scope == TableScope::Temp {
                    None
                }
                else {
                    match temp_position {
                        Some(_) => None,
                        None => Some(*self.catalog.table_lookup.get(&key).ok_or_else(|| self.table_missing(&table, "update"))?),
                    }
                };
                let def_storage = match temp_position {
                    Some(position) => self.catalog.temp_tables[position].def.clone(),
                    None => self.catalog.tables[main_position.expect("main table resolved")].clone(),
                };
                let def = &def_storage;
                let budget = Budget::new();
                // An `UPDATE ... FROM` resolves assignments and the predicate
                // against the target row plus every `FROM` source, so a bare
                // column may come from either side.
                let (shape, from_tables) = if from.is_empty() {
                    (TargetShape::new(def, alias.as_deref().unwrap_or(&table), self.trigger.frames.last()), Vec::new())
                }
                else {
                    let width = def.columns.len();
                    let mut sources = vec![Source {
                        label: alias.as_deref().unwrap_or(&table).to_string(),
                        columns: SourceColumns::Table(def),
                        offset: 0,
                        join: None,
                        star_hidden: vec![false; width],
                        unqualified_hidden: vec![false; width],
                        star_override: vec![None; width],
                    }];
                    let mut offset = width + usize::from(!def.without_rowid);
                    let mut from_tables: Vec<Rc<Vec<Vec<Cell>>>> = Vec::with_capacity(from.len());
                    let exec = self.exec();
                    for entry in &from {
                        let (columns, rows) = exec.select_source(entry, &budget, None, None, None)?;
                        let visible = columns.width();
                        let width = visible + usize::from(columns.hidden_rowid());
                        sources.push(Source {
                            label: entry.label().to_string(),
                            columns,
                            offset,
                            join: None,
                            star_hidden: vec![false; visible],
                            unqualified_hidden: vec![false; visible],
                            star_override: vec![None; visible],
                        });
                        offset += width;
                        from_tables.push(rows);
                    }
                    let source_count = sources.len();
                    let relation = Relation {
                        sources,
                        using: vec![Vec::new(); source_count],
                        using_slots: Vec::new(),
                        trigger: self.trigger.frames.last(),
                        excluded: false,
                    };
                    let scope = relation.shape();
                    (TargetShape { relation, scope }, from_tables)
                };
                let mut targets: Vec<(usize, ResolvedExpr)> = Vec::with_capacity(assignments.len());
                for assignment in &assignments {
                    let index =
                        def.columns.iter().position(|column| column.name.eq_ignore_ascii_case(&assignment.column)).ok_or_else(|| {
                            SqlError::new("unknown-column", format!("unknown column `{}` in table `{table}`", assignment.column))
                                .in_statement("update")
                        })?;
                    targets.push((index, shape.resolve(&assignment.value, "update")?));
                }
                let predicate = shape.resolve_optional(filter.as_ref(), "update")?;
                // SQLite resolves `RETURNING` against the real table name; the
                // `AS` alias of the update target is not visible there.
                let returning_shape = TargetShape::new(def, &table, None);
                let returning = match &returning {
                    Some(items) => Some(returning_shape.plan(def, items, "update")?),
                    None => None,
                };
                // A target row joined to several `FROM` rows is updated once,
                // using the first matching join row, like SQLite.
                let joined = if from.is_empty() {
                    None
                }
                else {
                    let product = join_product(&from_tables);
                    let mut joined: BTreeMap<i64, Vec<Cell>> = BTreeMap::new();
                    if !product.is_empty() {
                        let live = self.exec().live_rows(def, "update")?;
                        let ctx = EvalContext {
                            exec: self.exec(),
                            parameters: &*self.parameters,
                            upsert_excluded: &*self.upsert_excluded,
                            trigger_frames: &self.trigger.frames,
                            outer_read: &self.exec_state.outer_read,
                            subquery_cache: &self.exec_state.subquery_cache,
                            host_functions: self.host_functions,
                            outer: &[],
                            current: &shape.scope,
                            aggregates: &[],
                            windows: &[],
                            window_row: 0,
                            budget: &budget,
                            ctes: None,
                        };
                        for (rowid, cells) in &live {
                            let mut base = cells.clone();
                            if !def.without_rowid {
                                base.push(Cell::Integer(*rowid));
                            }
                            for extra in &product {
                                let mut merged = base.clone();
                                merged.extend(extra.iter().cloned());
                                let keep = match &predicate {
                                    Some(predicate) => truthiness(&predicate.eval(&merged, &ctx)?) == Some(true),
                                    None => true,
                                };
                                if keep {
                                    joined.insert(*rowid, merged);
                                    break;
                                }
                            }
                        }
                    }
                    Some(joined)
                };
                let mut needs_index = self.exec().has_indexes(def, &key);
                // `WHERE <rowid> = <key>` 或普通列索引谓词命中时，扫描不必整表解码。
                let candidates = if from.is_empty() {
                    self.exec().update_candidates(def, &key, &table, alias.as_deref(), filter.as_ref(), &targets)
                }
                else {
                    None
                };
                let plan = self.exec().plan_update(
                    def,
                    &key,
                    &targets,
                    predicate.as_ref(),
                    joined.as_ref(),
                    returning.as_ref(),
                    &shape.scope,
                    &budget,
                    None,
                    candidates.as_deref(),
                )?;
                // Old row values are needed to tell which referenced keys this
                // statement rewrites, so they come from the plan's pre-statement
                // snapshot instead of decoding the table a second time.
                let old_rows = &plan.live;
                let assigned: Vec<bool> =
                    def.columns.iter().enumerate().map(|(index, _)| targets.iter().any(|(target, _)| *target == index)).collect();
                for (_, values) in &plan.writes {
                    for (position, value) in values.iter().enumerate() {
                        if let UpdateValue::Set(value) = value {
                            if let Some(error) = dml::strict_value_violation(def, position, value, "update") {
                                return Err(error);
                            }
                        }
                    }
                }
                let queue_triggers = self.exec().table_has_triggers(&key);
                let trigger_columns = dml::trigger_frame_columns(def);
                let assigned_names: Vec<String> = assignments.iter().map(|assignment| assignment.column.clone()).collect();
                let old_lookup: BTreeMap<i64, Vec<Cell>> =
                    if queue_triggers || def.without_rowid || self.foreign_keys { old_rows.iter().cloned().collect() } else { BTreeMap::new() };
                // The `AFTER` events wait until referential actions and checks
                // have run, matching SQLite's `BEFORE`, cascade, `AFTER` order.
                let mut after_events: Vec<(i64, Vec<Cell>, Vec<Cell>)> = Vec::new();
                if let Some(position) = temp_position {
                    let mut events: Vec<(i64, Vec<Cell>, Vec<Cell>)> = Vec::new();
                    {
                        let table = &mut self.catalog.temp_tables[position];
                        for (rowid, values) in &plan.writes {
                            let cells = table
                                .rows
                                .get_mut(rowid)
                                .ok_or_else(|| SqlError::new("storage", "temp row disappeared").in_statement("update"))?;
                            let old_cells = cells.clone();
                            for (slot, value) in values.iter().enumerate() {
                                if let UpdateValue::Set(value) = value {
                                    cells[slot] = write_value_cell(value);
                                }
                            }
                            if queue_triggers {
                                events.push((*rowid, old_cells, cells.clone()));
                            }
                        }
                    }
                    for (rowid, old_cells, new_cells) in events {
                        self.dml().queue_update_events(&key, def, &trigger_columns, &assigned_names, rowid, &old_cells, &new_cells);
                    }
                }
                else {
                    self.writer.begin().map_err(|error| storage_failure(error).in_statement("update"))?;
                    // 规划阶段已经解出每一行的原值:写侧直接复用这份行像,不再从
                    // b-tree 读一次 payload 并重新解码。
                    let mut live_cursor = 0usize;
                    for (rowid, values) in &plan.writes {
                        while live_cursor < plan.live.len() && plan.live[live_cursor].0 < *rowid {
                            live_cursor += 1;
                        }
                        let previous = plan.live.get(live_cursor).filter(|(id, _)| id == rowid).map(|(_, cells)| cells.as_slice());
                        // 行像只有触发器和 WITHOUT ROWID 表需要;普通 rowid 表把
                        // 赋值交给写侧,不为每一行拼一份完整的单元格向量。
                        let needs_cells = queue_triggers || def.without_rowid;
                        let mut new_cells = if needs_cells { old_lookup.get(rowid).cloned().unwrap_or_default() } else { Vec::new() };
                        if needs_cells {
                            if new_cells.len() < def.columns.len() {
                                new_cells.resize(def.columns.len(), Cell::Null);
                            }
                            for (slot, value) in values.iter().enumerate() {
                                if let UpdateValue::Set(value) = value {
                                    new_cells[slot] = write_value_cell(value);
                                }
                            }
                        }
                        if queue_triggers {
                            let old_cells = old_lookup.get(rowid).cloned().unwrap_or_default();
                            self.dml().queue_update_before(&key, def, &trigger_columns, &assigned_names, *rowid, &old_cells, &new_cells);
                            after_events.push((*rowid, old_cells, new_cells.clone()));
                        }
                        let written = if def.without_rowid {
                            let old_values: Vec<WriteValue> =
                                old_lookup.get(rowid).cloned().unwrap_or_default().iter().map(dml::cell_write_value).collect();
                            let new_values: Vec<WriteValue> = new_cells.iter().map(dml::cell_write_value).collect();
                            self.writer.update_without_rowid(&table, &old_values, &new_values)
                        }
                        else if let Some(previous) = previous {
                            let before = dml::cells_write_values(previous);
                            self.writer.update_with_values(&table, *rowid, values, before)
                        }
                        else {
                            self.writer.update(&table, *rowid, values)
                        };
                        if let Err(error) = written {
                            let _ = self.writer.rollback();
                            return Err(dml::write_failure(error, "update", def));
                        }
                    }
                }
                let mut needs_without_rowid = temp_position.is_none() && def.without_rowid;
                if temp_position.is_none() && self.foreign_keys {
                    let changes: Vec<(Vec<Cell>, Option<Vec<Cell>>)> = plan
                        .writes
                        .iter()
                        .map(|(rowid, values)| {
                            let old = old_lookup.get(rowid).cloned().unwrap_or_default();
                            let mut next = old.clone();
                            if next.len() < def.columns.len() {
                                next.resize(def.columns.len(), Cell::Null);
                            }
                            for (slot, value) in values.iter().enumerate() {
                                if let UpdateValue::Set(value) = value {
                                    next[slot] = write_value_cell(value);
                                }
                            }
                            (old, Some(next))
                        })
                        .collect();
                    match self.dml().apply_foreign_key_actions(def, &changes, "update", 0) {
                        Ok((index_needed, without_needed)) => {
                            needs_index |= index_needed;
                            needs_without_rowid |= without_needed;
                        }
                        Err(error) => {
                            let _ = self.writer.rollback();
                            return Err(error);
                        }
                    }
                }
                if self.foreign_keys {
                    if let Err(error) = self.exec().check_update_foreign_keys(def, &plan, &old_rows, &assigned, "update", self.in_transaction())
                    {
                        if temp_position.is_none() {
                            let _ = self.writer.rollback();
                        }
                        return Err(error);
                    }
                }
                if temp_position.is_none() {
                    for (rowid, old_cells, new_cells) in &after_events {
                        self.dml().queue_update_after(&key, def, &trigger_columns, &assigned_names, *rowid, old_cells, new_cells);
                    }
                }
                if temp_position.is_none() {
                    if let Err(error) = self.dml().refresh_managed_indexes(&key, def, "update") {
                        let _ = self.writer.rollback();
                        return Err(error);
                    }
                    self.writer.commit().map_err(|error| storage_failure(error).in_statement("update"))?;
                }
                if temp_position.is_none() && needs_index {
                    self.materialize_indexes_after_write("update")?;
                }
                if needs_without_rowid {
                    self.materialize_without_rowid_after_write("update")?;
                }
                // 外键动作会直接改写子表行,索引内容不再只由本表语句决定;
                // 其余情况按被赋值的列精确失效,只改非索引列时缓存原样保留。
                let foreign_actions =
                    self.foreign_keys && (!def.foreign_keys.is_empty() || !self.exec().referencing_foreign_keys(&def.name).is_empty());
                if foreign_actions {
                    self.exec().invalidate_all_indexes();
                }
                else {
                    self.exec().invalidate_indexes_after_update(def, &key, &assigned);
                }
                Ok(ExecResult {
                    statement: "update",
                    columns: returning.as_ref().map(|plan| plan.columns.clone()).unwrap_or_default(),
                    rows: plan.returning,
                    affected: plan.affected,
                })
            }
            Statement::Delete { schema: _, table, alias, filter, returning } => {
                check_readonly_table(&table, "delete")?;
                let key = table.to_ascii_lowercase();
                if self.catalog.views.contains_key(&key) {
                    return self.run_view_delete(&table, &key, alias.as_deref(), filter.as_ref(), returning.as_ref());
                }
                let temp_position = if *self.scope == TableScope::Main { None } else { self.catalog.temp_lookup.get(&key).copied() };
                let main_position = if *self.scope == TableScope::Temp {
                    None
                }
                else {
                    match temp_position {
                        Some(_) => None,
                        None => Some(*self.catalog.table_lookup.get(&key).ok_or_else(|| self.table_missing(&table, "delete"))?),
                    }
                };
                // Temp rows are removed from the in-memory map, so their
                // definition is cloned to keep the catalog readable while the
                // map is borrowed mutably.
                let def_storage = match temp_position {
                    Some(position) => self.catalog.temp_tables[position].def.clone(),
                    None => self.catalog.tables[main_position.expect("main table resolved")].clone(),
                };
                let def = &def_storage;
                let shape = TargetShape::new(def, alias.as_deref().unwrap_or(&table), self.trigger.frames.last());
                let predicate = shape.resolve_optional(filter.as_ref(), "delete")?;
                let returning_shape = TargetShape::new(def, &table, None);
                let returning = match &returning {
                    Some(items) => Some(returning_shape.plan(def, items, "delete")?),
                    None => None,
                };
                let budget = Budget::new();
                let mut needs_index = self.exec().has_indexes(def, &key);
                // `WHERE <rowid> = <key>` 或普通列索引谓词命中时，删除不必整表解码。
                let candidates = self.exec().delete_candidates(def, &key, &table, alias.as_deref(), filter.as_ref());
                let plan =
                    self.exec().plan_delete(def, predicate.as_ref(), returning.as_ref(), &shape.scope, &budget, candidates.as_deref())?;
                // A temp table snapshots its row map instead of opening a
                // writer transaction, so a failing referential check undoes the
                // removal exactly like a rollback would.
                let temp_snapshot = temp_position.map(|position| self.catalog.temp_tables[position].rows.clone());
                let queue_triggers = self.exec().table_has_triggers(&key);
                let trigger_columns = dml::trigger_frame_columns(def);
                // The deleted rows' referenced keys are only visible before the
                // removal, so the snapshot comes from the plan; 只有触发器和外键
                // 动作需要按 rowid 查旧行，其余路径不建这张表。
                let old_lookup: BTreeMap<i64, Vec<Cell>> =
                    if queue_triggers || self.foreign_keys { plan.live.iter().cloned().collect() } else { BTreeMap::new() };
                let mut after_events: Vec<(i64, Vec<Cell>)> = Vec::new();
                if let Some(position) = temp_position {
                    {
                        let table = &mut self.catalog.temp_tables[position];
                        for rowid in &plan.removed {
                            table.rows.remove(rowid);
                        }
                    }
                    if queue_triggers {
                        for rowid in &plan.removed {
                            let old_cells = old_lookup.get(rowid).cloned().unwrap_or_default();
                            self.dml().queue_delete_events(&key, &trigger_columns, *rowid, &old_cells);
                        }
                    }
                }
                else {
                    self.writer.begin().map_err(|error| storage_failure(error).in_statement("delete"))?;
                    // 删除语句在规划阶段已经解出被删行:有索引键要维护时把这些值
                    // 直接交给写侧,没有索引时连行像都不必读,只删 b-tree 项。
                    let maintain_indexes = needs_index && !def.without_rowid;
                    let mut live_cursor = 0usize;
                    for rowid in &plan.removed {
                        if queue_triggers {
                            let old_cells = old_lookup.get(rowid).cloned().unwrap_or_default();
                            self.dml().queue_delete_before(&key, &trigger_columns, *rowid, &old_cells);
                            after_events.push((*rowid, old_cells));
                        }
                        let removed = if def.without_rowid {
                            let old_values: Vec<WriteValue> =
                                old_lookup.get(rowid).cloned().unwrap_or_default().iter().map(dml::cell_write_value).collect();
                            self.writer.delete_without_rowid(&table, &old_values)
                        }
                        else if maintain_indexes {
                            while live_cursor < plan.live.len() && plan.live[live_cursor].0 < *rowid {
                                live_cursor += 1;
                            }
                            match plan.live.get(live_cursor).filter(|(id, _)| id == rowid) {
                                Some((_, cells)) => {
                                    let values = dml::cells_write_values(cells);
                                    self.writer.delete_with_values(&table, *rowid, &values)
                                }
                                None => self.writer.delete(&table, *rowid),
                            }
                        }
                        else {
                            self.writer.delete_rowid(&table, *rowid)
                        };
                        if let Err(error) = removed {
                            let _ = self.writer.rollback();
                            return Err(dml::write_failure(error, "delete", def));
                        }
                    }
                }
                let mut needs_without_rowid = temp_position.is_none() && def.without_rowid;
                if temp_position.is_none() && self.foreign_keys {
                    let changes: Vec<(Vec<Cell>, Option<Vec<Cell>>)> =
                        plan.removed.iter().map(|rowid| (old_lookup.get(rowid).cloned().unwrap_or_default(), None)).collect();
                    match self.dml().apply_foreign_key_actions(def, &changes, "delete", 0) {
                        Ok((index_needed, without_needed)) => {
                            needs_index |= index_needed;
                            needs_without_rowid |= without_needed;
                        }
                        Err(error) => {
                            let _ = self.writer.rollback();
                            return Err(error);
                        }
                    }
                }
                if self.foreign_keys {
                    if let Err(error) = self.exec().check_delete_foreign_keys(def, &plan.live, &plan.removed, "delete", self.in_transaction()) {
                        match (temp_position, temp_snapshot) {
                            (Some(position), Some(snapshot)) => self.catalog.temp_tables[position].rows = snapshot,
                            _ => {
                                let _ = self.writer.rollback();
                            }
                        }
                        return Err(error);
                    }
                }
                if temp_position.is_none() {
                    for (rowid, old_cells) in &after_events {
                        self.dml().queue_delete_after(&key, &trigger_columns, *rowid, old_cells);
                    }
                }
                if temp_position.is_none() {
                    if let Err(error) = self.dml().refresh_managed_indexes(&key, def, "delete") {
                        let _ = self.writer.rollback();
                        return Err(error);
                    }
                    self.writer.commit().map_err(|error| storage_failure(error).in_statement("delete"))?;
                }
                if temp_position.is_none() && needs_index {
                    self.materialize_indexes_after_write("delete")?;
                }
                if needs_without_rowid {
                    self.materialize_without_rowid_after_write("delete")?;
                }
                // 同 `UPDATE`:外键级联删除会改动子表,只能全量失效;否则
                // 把被删行从整份索引快照里精确剔除。
                let foreign_actions =
                    self.foreign_keys && (!def.foreign_keys.is_empty() || !self.exec().referencing_foreign_keys(&def.name).is_empty());
                if foreign_actions {
                    self.exec().invalidate_all_indexes();
                }
                else {
                    self.exec().patch_index_probes_after_delete(&key, &plan.removed, &plan.live);
                }
                Ok(ExecResult {
                    statement: "delete",
                    columns: returning.as_ref().map(|plan| plan.columns.clone()).unwrap_or_default(),
                    rows: plan.returning,
                    affected: plan.removed.len(),
                })
            }
            Statement::DropTable { schema: _, name, if_exists } => {
                if ddl::is_schema_table(&name) {
                    return Err(SqlError::new("readonly-system-table", format!("table {name} may not be dropped")).in_statement("drop-table"));
                }
                let key = name.to_ascii_lowercase();
                // The temp schema is searched before main, so `DROP TABLE`
                // removes the temp object when both schemas share the name.
                let temp_position = if *self.scope == TableScope::Main { None } else { self.catalog.temp_lookup.get(&key).copied() };
                if temp_position.is_none() && self.catalog.temp_views.contains_key(&key) {
                    return Err(SqlError::new("wrong-object", format!("use DROP VIEW to delete view {name}")).in_statement("drop-table"));
                }
                if let Some(position) = temp_position {
                    let dropped = self.catalog.temp_tables.remove(position);
                    // The freed page goes back to the temp allocator, so the
                    // next temp table reuses it like SQLite does.
                    self.catalog.temp_free_pages.insert(dropped.rootpage);
                    self.catalog.temp_lookup.remove(&key);
                    ddl::reindex_temp_tables(&mut self.catalog);
                    self.catalog.triggers.retain(|_, trigger| !(trigger.temp && trigger.table == key));
                    return Ok(ExecResult { statement: "drop-table", columns: Vec::new(), rows: Vec::new(), affected: 1 });
                }
                let objects = ddl::schema_objects(&*self.writer, "drop-table")?;
                let present = objects.iter().any(|object| object.kind == SchemaObjectKind::Table && object.name.eq_ignore_ascii_case(&name));
                if !present {
                    if objects.iter().any(|object| object.kind == SchemaObjectKind::View && object.name.eq_ignore_ascii_case(&name)) {
                        return Err(SqlError::new("wrong-object", format!("use DROP VIEW to delete view {name}")).in_statement("drop-table"));
                    }
                    if if_exists {
                        return Ok(ExecResult { statement: "drop-table", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                    }
                    return Err(
                        SqlError::new("unknown-table", format!("no such table: {}", self.scoped_name(&name))).in_statement("drop-table")
                    );
                }
                if name.eq_ignore_ascii_case("sqlite_sequence") {
                    return Err(SqlError::new("readonly-system-table", format!("table {name} may not be dropped")).in_statement("drop-table"));
                }
                let key = name.to_ascii_lowercase();
                let position = *self
                    .catalog
                    .table_lookup
                    .get(&key)
                    .ok_or_else(|| SqlError::new("unknown-table", format!("no such table: {name}")).in_statement("drop-table"))?;
                self.writer.begin().map_err(|error| storage_failure(error).in_statement("drop-table"))?;
                match self.writer.drop_table(&name) {
                    Ok(true) => {}
                    Ok(false) => {
                        let _ = self.writer.rollback();
                        return Err(SqlError::new("unknown-table", format!("no such table: {name}")).in_statement("drop-table"));
                    }
                    Err(error) => {
                        let _ = self.writer.rollback();
                        return Err(storage_failure(error).in_statement("drop-table"));
                    }
                }
                self.writer.commit().map_err(|error| storage_failure(error).in_statement("drop-table"))?;
                self.catalog.tables.remove(position);
                ddl::reindex_tables(&mut self.catalog);
                self.catalog.indexes.retain(|_, index| index.table != key);
                self.catalog.triggers.retain(|_, trigger| trigger.table != key);
                Ok(ExecResult { statement: "drop-table", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            Statement::DropIndex { schema: _, name, if_exists } => {
                self.writer.begin().map_err(|error| storage_failure(error).in_statement("drop-index"))?;
                let outcome = match self.writer.drop_index(&name) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        let _ = self.writer.rollback();
                        return Err(storage_failure(error).in_statement("drop-index"));
                    }
                };
                match outcome {
                    DropIndexOutcome::Dropped => {}
                    DropIndexOutcome::Missing => {
                        let _ = self.writer.rollback();
                        if if_exists {
                            return Ok(ExecResult { statement: "drop-index", columns: Vec::new(), rows: Vec::new(), affected: 0 });
                        }
                        return Err(SqlError::new("unknown-index", format!("no such index: {name}")).in_statement("drop-index"));
                    }
                    DropIndexOutcome::Automatic => {
                        let _ = self.writer.rollback();
                        return Err(SqlError::new(
                            "autoindex-immutable",
                            "index associated with UNIQUE or PRIMARY KEY constraint cannot be dropped",
                        )
                        .in_statement("drop-index"));
                    }
                }
                self.writer.commit().map_err(|error| storage_failure(error).in_statement("drop-index"))?;
                self.catalog.indexes.remove(&name.to_ascii_lowercase());
                Ok(ExecResult { statement: "drop-index", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            Statement::AlterTable { schema: _, table, action } => {
                ddl::run_alter_table(&mut self.writer, &mut self.catalog, table, action)?;
                Ok(ExecResult { statement: "alter-table", columns: Vec::new(), rows: Vec::new(), affected: 1 })
            }
            Statement::Select { with, recursive, first, compounds, order, limit } => {
                // One budget per statement, shared by every nested subquery, so
                // deep correlation reports a structured error instead of
                // running until the host timeout.
                let budget = Budget::new();
                let outcome = self.exec().run_select(&with, recursive, &first, &compounds, order, limit, &[], &budget, None)?;
                Ok(ExecResult { statement: "select", columns: outcome.columns, rows: outcome.rows, affected: 0 })
            }
        }
    }
}
