//! SQLite 门面的 DML 执行机器:INSERT / UPDATE / DELETE 的行读写、计划类型与触发器排队。
//!
//! 只读机器(行读取、候选 rowid、upsert 冲突判定、UPDATE/DELETE 计划)挂在
//! [`ExecContext`] 上;写机器(逐行写入、触发器事件排队、外键动作、托管索引
//! 刷新)挂在 [`DmlContext`] 上。产品门面的 `Engine` 只保留语句分发与目录维护,
//! 经这两个上下文借用这里的实现;本模块不得反向依赖任何产品门面。

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashSet},
    rc::Rc,
};

use database_query::sqlite::{SqlError, bind::*, catalog::*, frontend::*, storage_failure, value::*};
use database_storage::sqlite::{Collation, SqliteWriter, TableLimits, TableRow, UpdateValue, WriteValue, apply_write_affinity};

use super::{
    cells::*,
    eval::{Budget, EvalContext, EvalExpr, EvalReturningPlan},
    guards::*,
    host::HostFunctionTable,
    select::{ExecContext, SqliteExecState, TableScope, decode_row},
    window::*,
};

/// 写语句候选 rowid 合并成区间读的最大缝隙。
///
/// 候选按 rowid 升序,缝隙不超过这个值的相邻候选交给同一次 `table_range`,用少量
/// 多余行换掉一次 b-tree 下降。
const CANDIDATE_GAP: i64 = 8;

/// 一条 DML 语句排队的行级触发器事件。
///
/// 排队而不是内联触发,让语句自身的写入与触发器程序保持 SQLite 的行序,写路径
/// 本身不因触发器改变。
pub struct DmlRowEvent {
    /// 事件所属的小写表名。
    pub table: String,
    /// 触发器相对行变更的执行时机。
    pub timing: TriggerTiming,
    /// 触发器响应的行变更。
    pub event: TriggerEvent,
    /// 触发器帧暴露的列名。
    pub columns: Vec<String>,
    /// `INSERT` / `UPDATE` 帧的新行;事件没有新行时为 `None`。
    pub new: Option<Vec<Cell>>,
    /// `UPDATE` / `DELETE` 帧的旧行;事件没有旧行时为 `None`。
    pub old: Option<Vec<Cell>>,
}

/// 一条写语句的执行上下文。
///
/// 门面 `Engine` 在分发 INSERT / UPDATE / DELETE 时把写入侧的可变视图注入这里;
/// 写机器按可变引用改写入镜像与目录,只读机器仍经 [`DmlContext::exec`] 回到
/// [`ExecContext`]。
pub struct DmlContext<'a> {
    /// 存储写入器:行数据的可变入口。
    pub writer: &'a mut SqliteWriter,
    /// 当前 schema 目录的可变视图。
    pub catalog: &'a mut SqliteCatalog,
    /// 跨语句缓存状态,所有权在门面。
    pub exec_state: &'a SqliteExecState,
    /// 会话宿主函数表。
    pub host_functions: &'a HostFunctionTable,
    /// 绑定到 `?` / `:name` 占位符的参数。
    pub parameters: &'a [Cell],
    /// `ON CONFLICT DO UPDATE` 的 `excluded` 行,写机器逐行改写。
    pub upsert_excluded: &'a mut Vec<Cell>,
    /// 正在执行的触发器体帧,最外层在前。
    pub trigger_frames: &'a [TriggerFrame],
    /// 当前语句的表别名(`CURRENT OF` 判定)。
    pub alias: Option<&'a str>,
    /// `ATTACH DATABASE` 的每个附加库各自的执行上下文,按 attach 顺序。
    pub attached: Rc<[ExecContext<'a>]>,
    /// 小写附加库 schema 名到 `attached` 下标的映射。
    pub attached_lookup: &'a BTreeMap<String, usize>,
    /// 本语句排队的行级触发器事件。
    pub pending_events: &'a mut Vec<DmlRowEvent>,
    /// 推迟的索引 b-tree 重建标记,所有权在门面。
    pub pending_indexes: &'a mut bool,
    /// 推迟的 WITHOUT ROWID 聚簇树重建标记,所有权在门面。
    pub pending_without_rowid: &'a mut bool,
    /// 最近一次成功写入行的 rowid,所有权在门面。
    pub last_insert_rowid: &'a mut i64,
    /// `PRAGMA foreign_keys` 的当前值。
    pub foreign_keys: bool,
    /// `PRAGMA recursive_triggers` 的当前值。
    pub recursive_triggers: bool,
    /// 是否处于显式事务中(延迟外键检查的判定)。
    pub in_transaction: bool,
    /// 当前语句的 schema 作用域。
    pub scope: TableScope,
}

impl<'a> DmlContext<'a> {
    /// 组装本上下文的只读执行视图:写机器内部的读路径全部走这里。
    pub fn exec(&self) -> ExecContext<'_> {
        ExecContext {
            writer: &*self.writer,
            catalog: &*self.catalog,
            state: self.exec_state,
            host_functions: self.host_functions,
            parameters: self.parameters,
            upsert_excluded: &*self.upsert_excluded,
            trigger_frames: self.trigger_frames,
            alias: self.alias,
            attached: self.attached.clone(),
            attached_lookup: self.attached_lookup,
        }
    }

    /// 写完一条语句后标记索引 b-tree 需要重建。
    ///
    /// 索引 b-tree 只能整树重建,单条语句重建一次会让事务内第 n 条写入付掉
    /// O(n) 的重建成本,所以任何写语句都只置位。重建推迟到真正读取镜像的
    /// 入口(提交不再重建,`snapshot` / `save` / `VACUUM` /
    /// `PRAGMA integrity_check` / 惰性游标打开),这些入口都会先调用
    /// [`DmlContext::flush_pending_materialization`],因此读到的镜像仍然与内存
    /// 键集一致。
    pub fn materialize_indexes_after_write(&mut self, _statement: &'static str) -> Result<(), SqlError> {
        *self.pending_indexes = true;
        Ok(())
    }

    /// 同 [`DmlContext::materialize_indexes_after_write`],针对 WITHOUT ROWID 表的聚簇树。
    ///
    /// 与索引键集不同,`WITHOUT ROWID` 表的行只存在于这份聚簇树里,普通
    /// `SELECT` 会直接读镜像页。因此这里立即重建,不留延迟标记,否则同一条
    /// 连接里写完再读会看到空表。
    pub fn materialize_without_rowid_after_write(&mut self, statement: &'static str) -> Result<(), SqlError> {
        *self.pending_without_rowid = true;
        self.flush_pending_materialization(statement)
    }

    /// 把推迟的索引 / 聚簇树重建落进当前镜像。
    ///
    /// 只在事务提交、写盘或读取镜像时调用;重建完成后清位,失败则保留标记让
    /// 调用方还能重试或回滚。
    pub fn flush_pending_materialization(&mut self, statement: &'static str) -> Result<(), SqlError> {
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

    /// Queues one `BEFORE` update event of a row.
    pub fn queue_update_before(
        &mut self,
        table: &str,
        def: &TableDef,
        columns: &[String],
        assigned: &[String],
        rowid: i64,
        old: &[Cell],
        new: &[Cell],
    ) {
        self.queue_update_event(table, def, columns, assigned, rowid, old, new, TriggerTiming::Before);
    }

    /// Queues one `AFTER` update event of a row.
    pub fn queue_update_after(
        &mut self,
        table: &str,
        def: &TableDef,
        columns: &[String],
        assigned: &[String],
        rowid: i64,
        old: &[Cell],
        new: &[Cell],
    ) {
        self.queue_update_event(table, def, columns, assigned, rowid, old, new, TriggerTiming::After);
    }

    /// Queues the `BEFORE` and `AFTER` events of one updated row.
    pub fn queue_update_events(
        &mut self,
        table: &str,
        def: &TableDef,
        columns: &[String],
        assigned: &[String],
        rowid: i64,
        old: &[Cell],
        new: &[Cell],
    ) {
        self.queue_update_before(table, def, columns, assigned, rowid, old, new);
        self.queue_update_after(table, def, columns, assigned, rowid, old, new);
    }

    /// Queues one `UPDATE` event of a row at the requested timing.
    pub fn queue_update_event(
        &mut self,
        table: &str,
        def: &TableDef,
        columns: &[String],
        assigned: &[String],
        rowid: i64,
        old: &[Cell],
        new: &[Cell],
        timing: TriggerTiming,
    ) {
        let new_rowid = def
            .rowid_alias
            .and_then(|alias| new.get(alias))
            .and_then(|cell| match cell {
                Cell::Integer(value) => Some(*value),
                _ => None,
            })
            .unwrap_or(rowid);
        let old = frame_cells(old, def.columns.len(), rowid);
        let new = frame_cells(new, def.columns.len(), new_rowid);
        self.pending_events.push(DmlRowEvent {
            table: table.to_string(),
            timing,
            event: TriggerEvent::Update(assigned.to_vec()),
            columns: columns.to_vec(),
            new: Some(new),
            old: Some(old),
        });
    }

    /// Queues one `BEFORE` delete event of a row.
    pub fn queue_delete_before(&mut self, table: &str, columns: &[String], rowid: i64, old: &[Cell]) {
        self.queue_delete_event(table, columns, rowid, old, TriggerTiming::Before);
    }

    /// Queues one `AFTER` delete event of a row.
    pub fn queue_delete_after(&mut self, table: &str, columns: &[String], rowid: i64, old: &[Cell]) {
        self.queue_delete_event(table, columns, rowid, old, TriggerTiming::After);
    }

    /// Queues the `BEFORE` and `AFTER` events of one deleted row.
    pub fn queue_delete_events(&mut self, table: &str, columns: &[String], rowid: i64, old: &[Cell]) {
        self.queue_delete_before(table, columns, rowid, old);
        self.queue_delete_after(table, columns, rowid, old);
    }

    /// Queues one `DELETE` event of a row at the requested timing.
    pub fn queue_delete_event(&mut self, table: &str, columns: &[String], rowid: i64, old: &[Cell], timing: TriggerTiming) {
        let old = frame_cells(old, columns.len() - 1, rowid);
        self.pending_events.push(DmlRowEvent {
            table: table.to_string(),
            timing,
            event: TriggerEvent::Delete,
            columns: columns.to_vec(),
            new: None,
            old: Some(old),
        });
    }

    /// Whether an `ON CONFLICT (rowid-alias)` target can be resolved by
    /// probing the primary key instead of materializing the whole table.
    ///
    /// The fast path only applies when nothing else can observe the table:
    /// no temporary shadow table, no other uniqueness rule, no managed unique
    /// index, no trigger, no foreign key, and no assignment that moves the
    /// rowid. Everything else keeps the full-scan [`Self::apply_upsert`].
    pub fn upsert_rowid_fast_path(&self, def: &TableDef, table_key: &str, plan: &UpsertPlan, temp_position: Option<usize>) -> bool {
        temp_position.is_none()
            && def.rowid_alias.is_some()
            && matches!(plan.keys.as_slice(), [UniqueKey::Rowid(_)])
            && plan.managed.is_empty()
            && plan.others.is_empty()
            && plan.other_managed.is_empty()
            && plan.assignments.iter().all(|(position, _)| Some(*position) != def.rowid_alias)
            && (!self.foreign_keys || (def.foreign_keys.is_empty() && self.exec().referencing_foreign_keys(&def.name).is_empty()))
            && !self.exec().table_has_triggers(table_key)
    }

    /// Applies one supplied row of a rowid-keyed upsert by probing only the
    /// conflicting primary-key row.
    ///
    /// `None` means the row has no explicit rowid value, so the caller must
    /// fall back to the scanning path. Rows written earlier in the same
    /// statement are already in storage, so a later supplied row still sees
    /// them exactly as a full scan would.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_upsert_rowid(
        &mut self,
        def: &TableDef,
        table_key: &str,
        table: &str,
        plan: &UpsertPlan,
        slots: &[Option<WriteValue>],
        returning: Option<&ReturningPlan>,
        shape: &ScopeShape,
        budget: &Budget,
    ) -> Result<Option<UpsertOutcome>, SqlError> {
        let width = def.columns.len();
        let Some(alias) = def.rowid_alias
        else {
            return Ok(None);
        };
        let rowid = match slots.get(alias).and_then(|slot| slot.as_ref()) {
            Some(WriteValue::Integer(rowid)) => *rowid,
            _ => return Ok(None),
        };
        let payload = self.writer.row_payload(table, rowid).map_err(|error| storage_failure(error).in_statement("insert"))?;
        let Some(payload) = payload
        else {
            let inserted = self.writer.insert(table, slots).map_err(|error| storage_failure(error).in_statement("insert"))?;
            return Ok(Some(UpsertOutcome::Inserted(inserted)));
        };
        if matches!(plan.action, UpsertAction::Nothing) {
            return Ok(Some(UpsertOutcome::Skipped));
        }
        // Lean rewrite: every assignment is a direct `excluded.<column>` with no
        // `DO UPDATE WHERE`, no `RETURNING` and no `CHECK` constraints. The stored
        // row keeps its unassigned columns and the assigned ones take the
        // supplied (already affinity-normalized) values, so the expression-eval
        // machinery, the joined-row map and `plan_update` are skipped entirely.
        let lean = returning.is_none()
            && plan.filter.is_none()
            && def.checks.is_empty()
            && plan.assignments.iter().all(|(_, expr)| matches!(expr, ResolvedExpr::ExcludedColumn { .. }));
        if lean {
            let candidate = check_row(def, slots, rowid);
            let mut values: Vec<UpdateValue> = vec![UpdateValue::Keep; width];
            for (index, expr) in &plan.assignments {
                let ResolvedExpr::ExcludedColumn { index: supplied } = expr
                else {
                    unreachable!("lean upsert assignments are all excluded-column references")
                };
                let value = cell_write_value(&candidate[*supplied]);
                if let Some(error) = strict_value_violation(def, *index, &value, "insert") {
                    return Err(error);
                }
                values[*index] = UpdateValue::Set(value);
            }
            if let Err(error) = self.writer.update(table, rowid, &values) {
                return Err(write_failure(error, "insert", def));
            }
            return Ok(Some(UpsertOutcome::Updated { rowid, rows: Vec::new() }));
        }
        let stored = decode_row(&payload, def, rowid)?;
        let candidate = check_row(def, slots, rowid);
        let mut eval_cells = stored.clone();
        eval_cells.push(Cell::Integer(rowid));
        *self.upsert_excluded = candidate[..width].to_vec();
        if let Some(filter) = &plan.filter {
            let ctx = EvalContext {
                exec: self.exec(),
                parameters: self.parameters,
                upsert_excluded: &*self.upsert_excluded,
                trigger_frames: self.trigger_frames,
                outer_read: &self.exec_state.outer_read,
                subquery_cache: &self.exec_state.subquery_cache,
                host_functions: self.host_functions,
                outer: &[],
                current: shape,
                aggregates: &[],
                windows: &[],
                window_row: 0,
                budget,
                ctes: None,
            };
            let keep = truthiness(&filter.eval(&eval_cells, &ctx)?) == Some(true);
            if !keep {
                self.upsert_excluded.clear();
                return Ok(Some(UpsertOutcome::Skipped));
            }
        }
        let mut joined = BTreeMap::new();
        joined.insert(rowid, eval_cells);
        let state = vec![(rowid, stored)];
        let planned =
            self.exec().plan_update(def, table_key, &plan.assignments, None, Some(&joined), returning, shape, budget, Some(&state), None);
        self.upsert_excluded.clear();
        let planned = planned?;
        if planned.writes.is_empty() {
            return Ok(Some(UpsertOutcome::Skipped));
        }
        for (_, values) in &planned.writes {
            for (position, value) in values.iter().enumerate() {
                if let UpdateValue::Set(value) = value {
                    if let Some(error) = strict_value_violation(def, position, value, "insert") {
                        return Err(error);
                    }
                }
            }
        }
        for (write_rowid, values) in &planned.writes {
            if let Err(error) = self.writer.update(table, *write_rowid, values) {
                return Err(write_failure(error, "insert", def));
            }
        }
        Ok(Some(UpsertOutcome::Updated { rowid, rows: planned.returning }))
    }

    /// Applies one supplied row of an upsert: insert it, skip it, or rewrite the
    /// stored row it conflicts with. `live` is the statement's own view of the
    /// table so a later supplied row sees rows written earlier in the same
    /// statement, exactly like SQLite.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_upsert(
        &mut self,
        def: &TableDef,
        table_key: &str,
        table: &str,
        plan: &UpsertPlan,
        slots: &[Option<WriteValue>],
        temp_position: Option<usize>,
        live: &mut Vec<(i64, Vec<Cell>)>,
        returning: Option<&ReturningPlan>,
        shape: &ScopeShape,
        budget: &Budget,
    ) -> Result<UpsertOutcome, SqlError> {
        let width = def.columns.len();
        // The rowid the supplied row will occupy. An explicit `INTEGER PRIMARY
        // KEY` value is used as written, otherwise SQLite's `max(rowid) + 1`.
        let explicit_rowid = def.rowid_alias.and_then(|alias| slots.get(alias).and_then(|slot| slot.as_ref())).and_then(|value| match value {
            WriteValue::Integer(value) => Some(*value),
            _ => None,
        });
        let candidate_rowid = match explicit_rowid {
            Some(rowid) => rowid,
            None => live.iter().map(|(rowid, _)| *rowid).max().unwrap_or(0) + 1,
        };
        let candidate = check_row(def, slots, candidate_rowid);
        let mut conflict_rowid = None;
        for key in &plan.keys {
            for (rowid, cells) in live.iter() {
                if upsert_key_conflicts(key, &candidate, candidate_rowid, *rowid, cells) {
                    conflict_rowid = Some(*rowid);
                    break;
                }
            }
            if conflict_rowid.is_some() {
                break;
            }
        }
        if conflict_rowid.is_none() {
            for target in &plan.managed {
                let Some(wanted) = self.exec().managed_target_key(def, target, &candidate)?
                else {
                    continue;
                };
                for (rowid, cells) in live.iter() {
                    if let Some(other) = self.exec().managed_target_key(def, target, cells)? {
                        if managed_keys_conflict(&wanted, &other, &target.index.collations) {
                            conflict_rowid = Some(*rowid);
                            break;
                        }
                    }
                }
                if conflict_rowid.is_some() {
                    break;
                }
            }
        }
        let unique_failed =
            |label: String| SqlError::new("unique-constraint", format!("UNIQUE constraint failed: {label}")).in_statement("insert");
        let Some(rowid) = conflict_rowid
        else {
            if let Some(label) = self.exec().upsert_other_conflict(def, plan, &candidate, candidate_rowid, live, -1)? {
                return Err(unique_failed(label));
            }
            let inserted = if let Some(position) = temp_position {
                let keys = self.exec().unique_keys(def, table_key);
                let table_state = &mut self.catalog.temp_tables[position];
                temp_insert_row(def, &mut table_state.rows, slots, &keys)?
            }
            else {
                self.writer.insert(table, slots).map_err(|error| storage_failure(error).in_statement("insert"))?
            };
            live.push((inserted, candidate[..width].to_vec()));
            return Ok(UpsertOutcome::Inserted(inserted));
        };
        if matches!(plan.action, UpsertAction::Nothing) {
            if let Some(label) = self.exec().upsert_other_conflict(def, plan, &candidate, candidate_rowid, live, -1)? {
                return Err(unique_failed(label));
            }
            return Ok(UpsertOutcome::Skipped);
        }
        if let Some(label) = self.exec().upsert_other_conflict(def, plan, &candidate, candidate_rowid, live, rowid)? {
            return Err(unique_failed(label));
        }
        // `DO UPDATE` rewrites the conflicting row the query planner would have
        // matched: the stored row plus this rowid, narrowed by the clause's own
        // `WHERE`.
        let mut predicate = ResolvedExpr::Compare {
            left: Box::new(ResolvedExpr::Column { source: 0, index: width }),
            op: CompareOp::Eq,
            right: Box::new(ResolvedExpr::Cell(Cell::Integer(rowid))),
            collation: Collation::Binary,
        };
        if let Some(filter) = &plan.filter {
            predicate = ResolvedExpr::And(Box::new(predicate), Box::new(filter.clone()));
        }
        *self.upsert_excluded = candidate[..width].to_vec();
        let planned = self.exec().plan_update(def, table_key, &plan.assignments, Some(&predicate), None, returning, shape, budget, None, None);
        self.upsert_excluded.clear();
        let planned = planned?;
        if planned.writes.is_empty() {
            // `DO UPDATE ... WHERE` was false: SQLite leaves the row alone.
            return Ok(UpsertOutcome::Skipped);
        }
        for (_, values) in &planned.writes {
            for (position, value) in values.iter().enumerate() {
                if let UpdateValue::Set(value) = value {
                    if let Some(error) = strict_value_violation(def, position, value, "insert") {
                        return Err(error);
                    }
                }
            }
        }
        let find_old = |rowid: i64| live.iter().find(|(candidate, _)| *candidate == rowid).map(|(_, cells)| cells.clone()).unwrap_or_default();
        // A `DO UPDATE` is an ordinary UPDATE for trigger and foreign-key
        // purposes: SQLite fires the update triggers with the rewritten row and
        // re-checks references the statement moves.
        let queue_triggers = self.exec().table_has_triggers(table_key);
        let trigger_columns = trigger_frame_columns(def);
        let assigned_names: Vec<String> = plan.assignments.iter().map(|(position, _)| def.columns[*position].name.clone()).collect();
        let assigned: Vec<bool> = (0..width).map(|position| plan.assignments.iter().any(|(target, _)| *target == position)).collect();
        let mut events: Vec<(i64, Vec<Cell>, Vec<Cell>)> = Vec::new();
        if let Some(position) = temp_position {
            let table_state = &mut self.catalog.temp_tables[position];
            for (write_rowid, values) in &planned.writes {
                let cells = table_state
                    .rows
                    .get_mut(write_rowid)
                    .ok_or_else(|| SqlError::new("storage", "temp row disappeared").in_statement("insert"))?;
                let old_cells = cells.clone();
                for (slot, value) in values.iter().enumerate() {
                    if let UpdateValue::Set(value) = value {
                        cells[slot] = write_value_cell(value);
                    }
                }
                if queue_triggers {
                    events.push((*write_rowid, old_cells, cells.clone()));
                }
            }
        }
        else {
            for (write_rowid, values) in &planned.writes {
                let old_cells = find_old(*write_rowid);
                let mut new_cells = old_cells.clone();
                if new_cells.len() < width {
                    new_cells.resize(width, Cell::Null);
                }
                for (slot, value) in values.iter().enumerate() {
                    if let UpdateValue::Set(value) = value {
                        new_cells[slot] = write_value_cell(value);
                    }
                }
                if queue_triggers {
                    self.queue_update_before(table_key, def, &trigger_columns, &assigned_names, *write_rowid, &old_cells, &new_cells);
                    events.push((*write_rowid, old_cells.clone(), new_cells.clone()));
                }
                let written = if def.without_rowid {
                    let old_values: Vec<WriteValue> = old_cells.iter().map(cell_write_value).collect();
                    let new_values: Vec<WriteValue> = new_cells.iter().map(cell_write_value).collect();
                    self.writer.update_without_rowid(table, &old_values, &new_values)
                }
                else {
                    self.writer.update(table, *write_rowid, values)
                };
                if let Err(error) = written {
                    return Err(write_failure(error, "insert", def));
                }
            }
        }
        if temp_position.is_some() {
            for (updated_rowid, old_cells, new_cells) in &events {
                self.queue_update_events(table_key, def, &trigger_columns, &assigned_names, *updated_rowid, old_cells, new_cells);
            }
        }
        if temp_position.is_none() && self.foreign_keys {
            let changes: Vec<(Vec<Cell>, Option<Vec<Cell>>)> = planned
                .writes
                .iter()
                .map(|(write_rowid, values)| {
                    let old = find_old(*write_rowid);
                    let mut next = old.clone();
                    if next.len() < width {
                        next.resize(width, Cell::Null);
                    }
                    for (slot, value) in values.iter().enumerate() {
                        if let UpdateValue::Set(value) = value {
                            next[slot] = write_value_cell(value);
                        }
                    }
                    (old, Some(next))
                })
                .collect();
            let (index_needed, without_needed) = self.apply_foreign_key_actions(def, &changes, "insert", 0)?;
            if index_needed {
                self.materialize_indexes_after_write("insert")?;
            }
            if without_needed {
                self.materialize_without_rowid_after_write("insert")?;
            }
        }
        if self.foreign_keys {
            self.exec().check_update_foreign_keys(def, &planned, live, &assigned, "insert", self.in_transaction)?;
        }
        if temp_position.is_none() {
            for (updated_rowid, old_cells, new_cells) in &events {
                self.queue_update_after(table_key, def, &trigger_columns, &assigned_names, *updated_rowid, old_cells, new_cells);
            }
        }
        if temp_position.is_none() && !self.exec().table_has_triggers(table_key) {
            // Without triggers nothing else can rewrite the table, so the
            // statement's own writes are enough to keep the in-memory rows
            // current; reloading every row per upsert would make a batch of
            // single-row upserts quadratic.
            for (write_rowid, values) in &planned.writes {
                if let Some((_, cells)) = live.iter_mut().find(|(candidate, _)| *candidate == *write_rowid) {
                    for (slot, value) in values.iter().enumerate() {
                        if let (UpdateValue::Set(value), Some(cell)) = (value, cells.get_mut(slot)) {
                            *cell = write_value_cell(value);
                        }
                    }
                }
            }
        }
        else {
            *live = self.exec().live_rows(def, "insert")?;
        }
        let rowid = planned.writes.first().map(|(rowid, _)| *rowid).unwrap_or(rowid);
        Ok(UpsertOutcome::Updated { rowid, rows: planned.returning })
    }

    /// Applies one `REPLACE` row: delete every stored row that conflicts on a
    /// uniqueness rule, then insert the supplied row.
    ///
    /// The delete is an ordinary row removal for foreign-key purposes, but a
    /// `DELETE` trigger only fires when `recursive_triggers` is on, matching
    /// SQLite's documented `REPLACE` behavior.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_replace(
        &mut self,
        def: &TableDef,
        table_key: &str,
        table: &str,
        slots: &[Option<WriteValue>],
        temp_position: Option<usize>,
        recursive_triggers: bool,
        queue_triggers: bool,
        trigger_columns: &[String],
        deleted: &mut Vec<(i64, Vec<Cell>)>,
    ) -> Result<i64, SqlError> {
        let explicit_rowid = def.rowid_alias.and_then(|alias| slots.get(alias).and_then(|slot| slot.as_ref())).and_then(|value| match value {
            WriteValue::Integer(value) => Some(*value),
            _ => None,
        });
        // 已给出 rowid、且表上没有其它唯一键/表达式索引时，`REPLACE` 只可能
        // 与同一 rowid 的旧行冲突：按 rowid 点读即可，不必为每条语句扫全表。
        let extra_unique = self.exec().unique_keys(def, table_key).into_iter().any(|key| !matches!(key, UniqueKey::Rowid(_)));
        let managed_targets = self.exec().upsert_managed_indexes(def, table_key);
        if let Some(rowid) = explicit_rowid.filter(|_| !def.without_rowid && !extra_unique && managed_targets.is_empty()) {
            let old = match temp_position {
                Some(position) => self.catalog.temp_tables[position].rows.get(&rowid).cloned(),
                None => self.exec().live_rows_at(def, "insert", &[rowid])?.into_iter().next().map(|(_, cells)| cells),
            };
            if let Some(old) = old {
                if queue_triggers && recursive_triggers {
                    self.queue_delete_before(table_key, trigger_columns, rowid, &old);
                }
                if let Some(position) = temp_position {
                    self.catalog.temp_tables[position].rows.remove(&rowid);
                }
                else {
                    self.writer.delete(table, rowid).map_err(|error| write_failure(error, "insert", def))?;
                }
                deleted.push((rowid, old.clone()));
                if queue_triggers && recursive_triggers {
                    self.queue_delete_after(table_key, trigger_columns, rowid, &old);
                }
            }
            if queue_triggers {
                self.pending_events.push(DmlRowEvent {
                    table: table_key.to_string(),
                    timing: TriggerTiming::Before,
                    event: TriggerEvent::Insert,
                    columns: trigger_columns.to_vec(),
                    new: Some(check_row(def, slots, rowid)),
                    old: None,
                });
            }
            return match temp_position {
                Some(position) => {
                    let keys = self.exec().unique_keys(def, table_key);
                    let table_state = &mut self.catalog.temp_tables[position];
                    temp_insert_row(def, &mut table_state.rows, slots, &keys)
                }
                None => self.writer.insert(table, slots).map_err(|error| storage_failure(error).in_statement("insert")),
            };
        }
        let live = self.exec().live_rows(def, "insert")?;
        let candidate_rowid = match explicit_rowid {
            Some(rowid) => rowid,
            None => live.iter().map(|(rowid, _)| *rowid).max().unwrap_or(0) + 1,
        };
        let candidate = check_row(def, slots, candidate_rowid);
        let mut conflicts: BTreeSet<i64> = BTreeSet::new();
        for unique in self.exec().unique_keys(def, table_key) {
            for (rowid, cells) in &live {
                if upsert_key_conflicts(&unique, &candidate, candidate_rowid, *rowid, cells) {
                    conflicts.insert(*rowid);
                }
            }
        }
        for target in self.exec().upsert_managed_indexes(def, table_key) {
            let Some(wanted) = self.exec().managed_target_key(def, &target, &candidate)?
            else {
                continue;
            };
            for (rowid, cells) in &live {
                if let Some(other) = self.exec().managed_target_key(def, &target, cells)? {
                    if managed_keys_conflict(&wanted, &other, &target.index.collations) {
                        conflicts.insert(*rowid);
                    }
                }
            }
        }
        for rowid in conflicts {
            let old = live.iter().find(|(candidate, _)| *candidate == rowid).map(|(_, cells)| cells.clone()).unwrap_or_default();
            if queue_triggers && recursive_triggers {
                self.queue_delete_before(table_key, trigger_columns, rowid, &old);
            }
            if let Some(position) = temp_position {
                self.catalog.temp_tables[position].rows.remove(&rowid);
            }
            else {
                let removed = if def.without_rowid {
                    let old_values: Vec<WriteValue> = old.iter().map(cell_write_value).collect();
                    self.writer.delete_without_rowid(table, &old_values)
                }
                else {
                    self.writer.delete(table, rowid)
                };
                if let Err(error) = removed {
                    return Err(write_failure(error, "insert", def));
                }
            }
            deleted.push((rowid, old.clone()));
            if queue_triggers && recursive_triggers {
                // `AFTER DELETE` is queued as soon as the row is gone, so the
                // replacement's delete trigger runs before its insert trigger.
                self.queue_delete_after(table_key, trigger_columns, rowid, &old);
            }
        }
        if queue_triggers {
            // The conflict-resolution deletes have already run, so a
            // `BEFORE INSERT` trigger observes the state after them.
            let before_rowid = def
                .rowid_alias
                .and_then(|alias| slots.get(alias).and_then(|slot| slot.as_ref()))
                .and_then(|value| match value {
                    WriteValue::Integer(value) => Some(*value),
                    _ => None,
                })
                .unwrap_or(-1);
            self.pending_events.push(DmlRowEvent {
                table: table_key.to_string(),
                timing: TriggerTiming::Before,
                event: TriggerEvent::Insert,
                columns: trigger_columns.to_vec(),
                new: Some(check_row(def, slots, before_rowid)),
                old: None,
            });
        }
        if let Some(position) = temp_position {
            let keys = self.exec().unique_keys(def, table_key);
            let table_state = &mut self.catalog.temp_tables[position];
            temp_insert_row(def, &mut table_state.rows, slots, &keys)
        }
        else {
            self.writer.insert(table, slots).map_err(|error| storage_failure(error).in_statement("insert"))
        }
    }

    /// Rebuilds the key sets of every managed index on `table` from the live
    /// rows. Called inside the statement's transaction, before it commits.
    pub fn refresh_managed_indexes(&mut self, table: &str, def: &TableDef, statement: &'static str) -> Result<(), SqlError> {
        let managed: Vec<IndexDef> =
            self.catalog.indexes.values().filter(|index| index.table.eq_ignore_ascii_case(table) && index.managed).cloned().collect();
        if managed.is_empty() {
            return Ok(());
        }
        let rows = self.exec().live_rows(def, statement)?;
        for index in managed {
            let entries = self.exec().managed_index_entries(def, &index.terms, index.predicate.as_ref(), &rows)?;
            self.writer.replace_managed_index_entries(&index.name, &entries).map_err(|error| storage_failure(error).in_statement(statement))?;
        }
        Ok(())
    }

    /// Applies the `ON DELETE` / `ON UPDATE` actions of every foreign key that
    /// references `parent`, recursing into the tables those actions touch.
    ///
    /// `changes` carries the cells every changed parent row had before the
    /// statement and, for a rewrite, the cells it has now; `None` means the row
    /// was removed. `NO ACTION` and `RESTRICT` rows are left for the
    /// end-of-statement checks. Returns whether a touched child owns
    /// materialized indexes or a stored WITHOUT ROWID b-tree.
    pub fn apply_foreign_key_actions(
        &mut self,
        parent: &TableDef,
        changes: &[(Vec<Cell>, Option<Vec<Cell>>)],
        statement: &'static str,
        depth: usize,
    ) -> Result<(bool, bool), SqlError> {
        if depth > 100 {
            return Err(SqlError::new("constraint-failed", "foreign key cascade exceeded the depth limit").in_statement(statement));
        }
        let deleting = changes.iter().all(|(_, next)| next.is_none());
        let mut needs_index = false;
        let mut needs_without_rowid = false;
        for (child_position, key) in self.exec().referencing_foreign_keys(&parent.name) {
            let action = if deleting { key.on_delete_action } else { key.on_update_action };
            if matches!(action, ForeignKeyAction::NoAction | ForeignKeyAction::Restrict) {
                continue;
            }
            let child = self.catalog.tables[child_position].clone();
            if child.temp {
                continue;
            }
            let (_, parent_columns) = self.exec().foreign_key_parent(&child, &key, statement)?;
            let mut pairs: Vec<(Vec<Cell>, Option<Vec<Cell>>)> = Vec::new();
            for (old, next) in changes {
                let old_values = pick(old, &parent_columns);
                if old_values.iter().any(|cell| matches!(cell, Cell::Null)) {
                    continue;
                }
                let new_values = next.as_ref().map(|cells| pick(cells, &parent_columns));
                if let Some(values) = &new_values {
                    if cells_equal(&old_values, values) {
                        continue;
                    }
                }
                pairs.push((old_values, new_values));
            }
            if pairs.is_empty() {
                continue;
            }
            let child_rows = self.exec().live_rows(&child, statement)?;
            let child_key = child.name.to_ascii_lowercase();
            let frame_columns = trigger_frame_columns(&child);
            let fires = self.exec().table_has_triggers(&child_key);
            let assigned: Vec<String> = key.columns.iter().map(|position| child.columns[*position].name.clone()).collect();
            let mut deleted: Vec<(i64, Vec<Cell>)> = Vec::new();
            let mut updated: Vec<(i64, Vec<Cell>, Vec<Cell>)> = Vec::new();
            for (rowid, cells) in &child_rows {
                let values = pick(cells, &key.columns);
                if values.iter().any(|cell| matches!(cell, Cell::Null)) {
                    continue;
                }
                let Some((_, next)) = pairs.iter().find(|(old, _)| cells_equal(old, &values))
                else {
                    continue;
                };
                match action {
                    ForeignKeyAction::Cascade => match next {
                        None => deleted.push((*rowid, cells.clone())),
                        Some(next) => {
                            let mut rewritten = cells.clone();
                            for (position, value) in key.columns.iter().zip(next.iter()) {
                                rewritten[*position] = value.clone();
                            }
                            updated.push((*rowid, cells.clone(), rewritten));
                        }
                    },
                    ForeignKeyAction::SetNull | ForeignKeyAction::SetDefault => {
                        let mut rewritten = cells.clone();
                        for position in &key.columns {
                            let column = &child.columns[*position];
                            let value = match action {
                                ForeignKeyAction::SetNull => Cell::Null,
                                _ => {
                                    let default = column.default_value.clone().unwrap_or(WriteValue::Null);
                                    write_value_cell(&apply_write_affinity(&column.declared_type, default))
                                }
                            };
                            if matches!(value, Cell::Null) && column.not_null {
                                return Err(SqlError::new(
                                    "not-null-constraint",
                                    format!("NOT NULL constraint failed: {}.{}", child.name, column.name),
                                )
                                .in_statement(statement));
                            }
                            rewritten[*position] = value;
                        }
                        updated.push((*rowid, cells.clone(), rewritten));
                    }
                    _ => {}
                }
            }
            if deleted.is_empty() && updated.is_empty() {
                continue;
            }
            let table_key = child.name.clone();
            for (rowid, old) in &deleted {
                if fires {
                    self.queue_delete_before(&table_key, &frame_columns, *rowid, old);
                }
                let written = if child.without_rowid {
                    let values: Vec<WriteValue> = old.iter().map(cell_write_value).collect();
                    self.writer.delete_without_rowid(&table_key, &values)
                }
                else {
                    self.writer.delete(&table_key, *rowid)
                };
                written.map_err(|error| write_failure(error, statement, &child))?;
                if fires {
                    self.queue_delete_after(&table_key, &frame_columns, *rowid, old);
                }
            }
            for (rowid, old, next) in &updated {
                if fires {
                    self.queue_update_before(&table_key, &child, &frame_columns, &assigned, *rowid, old, next);
                }
                let written = if child.without_rowid {
                    let old_values: Vec<WriteValue> = old.iter().map(cell_write_value).collect();
                    let new_values: Vec<WriteValue> = next.iter().map(cell_write_value).collect();
                    self.writer.update_without_rowid(&table_key, &old_values, &new_values)
                }
                else {
                    let values: Vec<UpdateValue> = next.iter().map(|cell| UpdateValue::Set(cell_write_value(cell))).collect();
                    self.writer.update(&table_key, *rowid, &values)
                };
                written.map_err(|error| write_failure(error, statement, &child))?;
                if fires {
                    self.queue_update_after(&table_key, &child, &frame_columns, &assigned, *rowid, old, next);
                }
            }
            // Rewritten child rows still have to resolve every foreign key,
            // which catches `SET DEFAULT` values that name no parent.
            let validated: Vec<i64> = updated
                .iter()
                .map(|(rowid, _, next)| {
                    child
                        .rowid_alias
                        .and_then(|alias| match next.get(alias) {
                            Some(Cell::Integer(value)) => Some(*value),
                            _ => None,
                        })
                        .unwrap_or(*rowid)
                })
                .collect();
            if !validated.is_empty() {
                self.exec().check_insert_foreign_keys(&child, &validated, statement, self.in_transaction)?;
            }
            self.refresh_managed_indexes(&child_key, &child, statement)?;
            if self.exec().has_indexes(&child, &child_key) {
                needs_index = true;
            }
            if child.without_rowid {
                needs_without_rowid = true;
            }
            let child_changes: Vec<(Vec<Cell>, Option<Vec<Cell>>)> = if deleting {
                deleted.iter().map(|(_, old)| (old.clone(), None)).collect()
            }
            else {
                updated.iter().map(|(_, old, next)| (old.clone(), Some(next.clone()))).collect()
            };
            let (index_needed, without_needed) = self.apply_foreign_key_actions(&child, &child_changes, statement, depth + 1)?;
            needs_index |= index_needed;
            needs_without_rowid |= without_needed;
        }
        Ok((needs_index, needs_without_rowid))
    }
}

impl<'e> ExecContext<'e> {
    /// Reads every row of `def`, decoded and sorted by rowid.
    pub fn live_rows(&self, def: &TableDef, statement: &'static str) -> Result<Vec<(i64, Vec<Cell>)>, SqlError> {
        if def.temp {
            let position = *self
                .catalog
                .temp_lookup
                .get(&def.name.to_ascii_lowercase())
                .ok_or_else(|| SqlError::new("storage", format!("temp table `{}` disappeared", def.name)).in_statement(statement))?;
            return Ok(self.catalog.temp_tables[position].rows.iter().map(|(rowid, cells)| (*rowid, cells.clone())).collect());
        }
        if def.without_rowid {
            // WITHOUT ROWID rows carry no rowid; a scan position is used as
            // the identity the update / delete planners sort by.
            let stored = self
                .writer
                .without_rowid_rows(&def.name, TableLimits::query())
                .map_err(|error| storage_failure(error).in_statement(statement))?;
            let mut rows = Vec::with_capacity(stored.len());
            for (position, payload) in stored.iter().enumerate() {
                rows.push((position as i64, def.decode_without_rowid(payload)?));
            }
            return Ok(rows);
        }
        let stored =
            self.writer.table_rows(&def.name, TableLimits::query()).map_err(|error| storage_failure(error).in_statement(statement))?;
        let mut rows = Vec::with_capacity(stored.len());
        for row in &stored {
            rows.push((row.rowid, decode_row(&row.payload, def, row.rowid)?));
        }
        rows.sort_by_key(|(rowid, _)| *rowid);
        Ok(rows)
    }

    /// 按候选 rowid 读行,候选升序取回。
    ///
    /// 相邻间隔不超过 [`CANDIDATE_GAP`] 的候选合并成一次区间读,避免逐行下降。
    /// 区间读会带回缝隙里的少量非候选行,谓词与唯一性判定照旧处理,因此只是多读
    /// 几行,不会改变语句结果。
    pub fn live_rows_at(&self, def: &TableDef, statement: &'static str, rowids: &[i64]) -> Result<Vec<(i64, Vec<Cell>)>, SqlError> {
        if def.temp {
            return Ok(self.live_rows(def, statement)?.into_iter().filter(|(rowid, _)| rowids.contains(rowid)).collect());
        }
        let mut sorted = rowids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let mut rows = Vec::with_capacity(sorted.len());
        let mut position = 0;
        while position < sorted.len() {
            let start = sorted[position];
            let mut end = start;
            let mut next = position + 1;
            while next < sorted.len() && sorted[next] - end <= CANDIDATE_GAP {
                end = sorted[next];
                next += 1;
            }
            let stored = if start == end {
                // 单行候选直接走 writer 的 rowid 下钻：点查/按唯一键改写是
                // DML 的常见形态，不必为一行重建区间读取上下文。
                match self.writer.row_payload(&def.name, start).map_err(|error| storage_failure(error).in_statement(statement))? {
                    Some(payload) => vec![TableRow { rowid: start, payload }],
                    None => Vec::new(),
                }
            }
            else {
                self.writer
                    .table_range(&def.name, start, end, TableLimits::query())
                    .map_err(|error| storage_failure(error).in_statement(statement))?
            };
            for row in &stored {
                rows.push((row.rowid, decode_row(&row.payload, def, row.rowid)?));
            }
            position = next;
        }
        rows.sort_by_key(|(rowid, _)| *rowid);
        Ok(rows)
    }

    /// `DELETE` 的候选 rowid:裸 `<rowid> = <键>` 等值,或普通列索引的探针结果。
    ///
    /// 与 SELECT 的 `rowid_pin` / `index_pin` 同一判定:只认裸 `AND` 链上的等值与
    /// 首列范围比较,键可以是字面量或执行期已绑定的参数,索引必须是未被托管改造的
    /// 普通列索引。谓词仍会在候选行上重跑一遍,所以候选只是超集,不会改变删除集合。
    pub fn delete_candidates(
        &self,
        def: &TableDef,
        table_key: &str,
        table: &str,
        alias: Option<&str>,
        filter: Option<&Expr>,
    ) -> Option<Vec<i64>> {
        if def.without_rowid || def.temp {
            return None;
        }
        self.write_candidates(def, table_key, alias.unwrap_or(table), filter)
    }

    /// `UPDATE` 的候选 rowid,约束与 [`Engine::delete_candidates`] 相同。
    ///
    /// `UPDATE` 还要用整表做唯一性冲突判定,因此只在「唯一键只有 rowid,且没有任何
    /// 赋值改动 rowid」时才收窄。这两条成立时,收窄后的行集与整表在冲突判定上等价;
    /// 外键检查只看被写入的行,因此不需要整表。`WITHOUT ROWID` 表一律走整表读取。
    pub fn update_candidates(
        &self,
        def: &TableDef,
        table_key: &str,
        table: &str,
        alias: Option<&str>,
        filter: Option<&Expr>,
        targets: &[(usize, ResolvedExpr)],
    ) -> Option<Vec<i64>> {
        if def.without_rowid || def.temp {
            return None;
        }
        if targets.iter().any(|(index, _)| Some(*index) == def.rowid_alias) {
            return None;
        }
        if self.unique_keys(def, table_key).iter().any(|key| !matches!(key, UniqueKey::Rowid(_))) {
            return None;
        }
        self.write_candidates(def, table_key, alias.unwrap_or(table), filter)
    }

    /// 按谓词取写语句的候选 rowid:先认裸 `<rowid> = <键>`,再逐个普通列索引试
    /// 全键等值与首列范围。返回的 rowid 升序,可能含谓词不成立的行。
    pub fn write_candidates(&self, def: &TableDef, table_key: &str, label: &str, filter: Option<&Expr>) -> Option<Vec<i64>> {
        let filter = filter?;
        let alias_name = def.rowid_alias.map(|index| def.columns[index].name.clone());
        if let Some(rowid) = ExecContext::rowid_eq_conjunct(filter, alias_name.as_deref(), label, self.parameters) {
            return Some(vec![rowid]);
        }
        // `IN (<rowids>)` 与 `>= a AND <= b` 同样是 rowid 上的精确候选集,
        // 不必为一条写语句解码整表再逐行判断。
        if let Some(rowids) = ExecContext::rowid_in_conjunct(filter, alias_name.as_deref(), label, self.parameters) {
            return Some(rowids);
        }
        if let Some(rowids) = ExecContext::rowid_range_candidates(filter, alias_name.as_deref(), label, self.parameters) {
            return Some(rowids);
        }
        for index in self.catalog.indexes.values() {
            if index.managed || index.table != table_key || index.columns.is_empty() {
                continue;
            }
            if let Some(rowids) = self.index_probe_rows(index, def, filter, label) {
                return Some(rowids);
            }
            if let Some(rowids) = self.index_range_rows(index, def, filter, label) {
                return Some(rowids);
            }
        }
        None
    }

    /// Resolves one `ON CONFLICT` clause against the insert target: the
    /// uniqueness rules its target names, plus the `DO UPDATE` assignments
    /// evaluated against the stored row and the supplied (`excluded`) row.
    pub fn prepare_upsert(&self, def: &TableDef, table_key: &str, label: &str, clause: &ConflictClause) -> Result<UpsertPlan, SqlError> {
        let mismatch = || {
            SqlError::new("conflict-target", "ON CONFLICT clause does not match any PRIMARY KEY or UNIQUE constraint").in_statement("insert")
        };
        // The target predicate is resolved against the insert target alone:
        // `excluded` is not in scope there, exactly like SQLite.
        let target_predicate = match (&clause.target, &clause.predicate) {
            (Some(_), Some(expression)) => {
                let relation = target_relation(def, label, None);
                Some(ExecContext::resolve_upsert_expr(&relation, expression)?)
            }
            (Some(_), None) => None,
            (None, Some(_)) => return Err(mismatch()),
            (None, None) => None,
        };
        let positions = match &clause.target {
            None => None,
            Some(columns) => {
                let mut positions = Vec::with_capacity(columns.len());
                for column in columns {
                    let position = def
                        .columns
                        .iter()
                        .position(|existing| existing.name.eq_ignore_ascii_case(column))
                        .ok_or_else(|| SqlError::new("unknown-column", format!("no such column: {column}")).in_statement("insert"))?;
                    positions.push(position);
                }
                Some(positions)
            }
        };
        let all_keys = self.unique_keys(def, table_key);
        let (keys, others) = match &positions {
            None => {
                let mut keys = all_keys;
                if def.without_rowid {
                    keys.retain(|key| !matches!(key, UniqueKey::Rowid(_)));
                }
                (keys, Vec::new())
            }
            Some(positions) if target_predicate.is_some() => {
                // A target `WHERE` can only name a partial index, so no plain
                // unique constraint or full index is the conflict resolver.
                (Vec::new(), all_keys)
            }
            Some(positions) => {
                let keys = self.upsert_target_keys(def, table_key, positions);
                let others = all_keys
                    .into_iter()
                    .filter(|key| match key {
                        UniqueKey::Rowid(_) => !(positions.len() == 1 && def.rowid_alias == Some(positions[0])),
                        UniqueKey::Columns(existing, _, _) => !same_positions(existing, positions),
                    })
                    .collect();
                (keys, others)
            }
        };
        let all_managed = self.upsert_managed_indexes(def, table_key);
        let (managed, other_managed) = match &positions {
            None => (all_managed, Vec::new()),
            Some(positions) => {
                let mut managed = Vec::new();
                let mut other_managed = Vec::new();
                for target in all_managed {
                    if managed_target_matches(&target, positions, target_predicate.as_ref()) {
                        managed.push(target);
                    }
                    else {
                        other_managed.push(target);
                    }
                }
                (managed, other_managed)
            }
        };
        if positions.is_some() && keys.is_empty() && managed.is_empty() {
            return Err(mismatch());
        }
        let (action, assignments, filter) = match &clause.action {
            ConflictAction::Nothing => (UpsertAction::Nothing, Vec::new(), None),
            ConflictAction::Update { assignments, predicate } => {
                let relation = upsert_relation(def, label);
                let mut targets = Vec::with_capacity(assignments.len());
                for assignment in assignments {
                    let index =
                        def.columns.iter().position(|column| column.name.eq_ignore_ascii_case(&assignment.column)).ok_or_else(|| {
                            SqlError::new("unknown-column", format!("no such column: {}", assignment.column)).in_statement("insert")
                        })?;
                    targets.push((index, ExecContext::resolve_upsert_expr(&relation, &assignment.value)?));
                }
                let filter = match predicate {
                    Some(expression) => Some(ExecContext::resolve_upsert_expr(&relation, expression)?),
                    None => None,
                };
                (UpsertAction::Update, targets, filter)
            }
        };
        Ok(UpsertPlan { keys, managed, others, other_managed, action, assignments, filter })
    }

    /// Resolves one expression of a `DO UPDATE` clause against the upsert
    /// relation, which exposes both the stored row and `excluded.<column>`.
    pub fn resolve_upsert_expr(relation: &Relation, expression: &Expr) -> Result<ResolvedExpr, SqlError> {
        let mut windows = Vec::new();
        let mut aggregates = Vec::new();
        resolve_expr(expression, relation, &[], &[], &mut windows, &mut aggregates, false, false).map_err(|error| error.in_statement("insert"))
    }

    /// Every unique engine-managed index of `table`, which includes partial and
    /// expression indexes.
    pub fn upsert_managed_indexes(&self, def: &TableDef, table_key: &str) -> Vec<ManagedTarget> {
        if def.temp {
            return Vec::new();
        }
        self.catalog
            .indexes
            .values()
            .filter(|index| index.managed && index.unique && index.table == table_key)
            .map(|index| {
                let label = managed_index_label(def, index);
                ManagedTarget { index: index.clone(), label }
            })
            .collect()
    }

    /// The uniqueness rules one `ON CONFLICT (columns)` target matches.
    pub fn upsert_target_keys(&self, def: &TableDef, table_key: &str, positions: &[usize]) -> Vec<UniqueKey> {
        self.unique_keys(def, table_key)
            .into_iter()
            .filter(|key| match key {
                UniqueKey::Rowid(_) => !def.without_rowid && positions.len() == 1 && def.rowid_alias == Some(positions[0]),
                UniqueKey::Columns(existing, _, _) => same_positions(existing, positions),
            })
            .collect()
    }

    /// The key values one managed index produces for a row, or `None` when the
    /// row is outside the index's partial predicate.
    pub fn managed_target_key(&self, def: &TableDef, target: &ManagedTarget, cells: &[Cell]) -> Result<Option<Vec<Cell>>, SqlError> {
        let shape = TargetShape::new(def, &def.name, None);
        let budget = Budget::new();
        let aggregates: Vec<Cell> = Vec::new();
        let windows: Vec<Vec<Cell>> = Vec::new();
        let context = EvalContext {
            exec: self.clone(),
            parameters: self.parameters,
            upsert_excluded: self.upsert_excluded,
            trigger_frames: self.trigger_frames,
            outer_read: &self.state.outer_read,
            subquery_cache: &self.state.subquery_cache,
            host_functions: self.host_functions,
            outer: &[],
            current: &shape.scope,
            aggregates: &aggregates,
            windows: &windows,
            window_row: 0,
            budget: &budget,
            ctes: None,
        };
        if let Some(predicate) = &target.index.predicate {
            if truthiness(&predicate.eval(cells, &context)?) != Some(true) {
                return Ok(None);
            }
        }
        let mut key = Vec::with_capacity(target.index.terms.len());
        for term in &target.index.terms {
            key.push(term.eval(cells, &context)?);
        }
        Ok(Some(key))
    }

    /// The first uniqueness rule outside the conflict target the supplied row
    /// still violates; SQLite aborts the statement instead of resolving it.
    pub fn upsert_other_conflict(
        &self,
        def: &TableDef,
        plan: &UpsertPlan,
        candidate: &[Cell],
        candidate_rowid: i64,
        live: &[(i64, Vec<Cell>)],
        skip: i64,
    ) -> Result<Option<String>, SqlError> {
        for key in &plan.others {
            let wanted = key.values(candidate, candidate_rowid);
            for (rowid, cells) in live {
                if *rowid == skip {
                    continue;
                }
                let other = key.values(cells, *rowid);
                let collides = wanted.iter().zip(&other).all(|(first, second)| {
                    !matches!(first, Cell::Null) && !matches!(second, Cell::Null) && compare_cells(first, second) == Ordering::Equal
                });
                if collides {
                    return Ok(Some(key.label().to_string()));
                }
            }
        }
        for target in &plan.other_managed {
            let Some(wanted) = self.managed_target_key(def, target, candidate)?
            else {
                continue;
            };
            for (rowid, cells) in live {
                if *rowid == skip {
                    continue;
                }
                if let Some(other) = self.managed_target_key(def, target, cells)? {
                    if managed_keys_conflict(&wanted, &other, &target.index.collations) {
                        return Ok(Some(target.label.clone()));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Computes the `(key values, rowid)` entries of one managed index by
    /// evaluating its terms and predicate against every live row.
    pub fn managed_index_entries(
        &self,
        def: &TableDef,
        terms: &[ResolvedExpr],
        predicate: Option<&ResolvedExpr>,
        rows: &[(i64, Vec<Cell>)],
    ) -> Result<Vec<(Vec<WriteValue>, i64)>, SqlError> {
        let shape = TargetShape::new(def, &def.name, None);
        let budget = Budget::new();
        let aggregates: Vec<Cell> = Vec::new();
        let windows: Vec<Vec<Cell>> = Vec::new();
        let mut entries = Vec::with_capacity(rows.len());
        for (rowid, cells) in rows {
            let context = EvalContext {
                exec: self.clone(),
                parameters: self.parameters,
                upsert_excluded: self.upsert_excluded,
                trigger_frames: self.trigger_frames,
                outer_read: &self.state.outer_read,
                subquery_cache: &self.state.subquery_cache,
                host_functions: self.host_functions,
                outer: &[],
                current: &shape.scope,
                aggregates: &aggregates,
                windows: &windows,
                window_row: 0,
                budget: &budget,
                ctes: None,
            };
            if let Some(predicate) = predicate {
                if truthiness(&predicate.eval(cells, &context)?) != Some(true) {
                    continue;
                }
            }
            let mut key = Vec::with_capacity(terms.len());
            for term in terms {
                key.push(cell_write_value(&term.eval(cells, &context)?));
            }
            entries.push((key, *rowid));
        }
        Ok(entries)
    }

    /// Plans one `UPDATE` without touching storage: every right-hand side is
    /// evaluated against the pre-statement row, assignments apply left to right
    /// (last one wins), and each row is checked against the *current* table, so
    /// a rowid shift behaves exactly like SQLite's ascending-order pass.
    ///
    /// `joined` carries the row shape of an `UPDATE ... FROM` statement: the
    /// target columns, its rowid, and every `FROM` source appended, for the
    /// target rows whose join produced a match. A row outside the map is not
    /// updated, and the predicate has already been applied to its merged row.
    pub fn plan_update(
        &self,
        def: &TableDef,
        table_key: &str,
        targets: &[(usize, ResolvedExpr)],
        predicate: Option<&ResolvedExpr>,
        joined: Option<&BTreeMap<i64, Vec<Cell>>>,
        returning: Option<&ReturningPlan>,
        shape: &ScopeShape,
        budget: &Budget,
        state_override: Option<&[(i64, Vec<Cell>)]>,
        candidates: Option<&[i64]>,
    ) -> Result<UpdatePlan, SqlError> {
        let width = def.columns.len();
        let keys = self.unique_keys(def, table_key);
        let owned_live = match state_override {
            Some(_) => None,
            None => Some(match candidates {
                Some(rowids) => self.live_rows_at(def, "update", rowids)?,
                None => self.live_rows(def, "update")?,
            }),
        };
        let live: &[(i64, Vec<Cell>)] = match state_override {
            Some(rows) => rows,
            None => owned_live.as_deref().unwrap_or_default(),
        };
        // 冲突判定的状态只在存在非 rowid 唯一键时才需要整份拷贝。rowid 唯一性
        // 用活动 rowid 集合判定,于是整表 `UPDATE` 每行不再扫一遍整张表。
        let column_keys: Vec<&UniqueKey> = keys.iter().filter(|key| matches!(key, UniqueKey::Columns(..))).collect();
        let rowid_label: Option<&str> = keys.iter().find_map(|key| match key {
            UniqueKey::Rowid(label) => Some(label.as_str()),
            UniqueKey::Columns(..) => None,
        });
        let mut state = if column_keys.is_empty() { Vec::new() } else { live.to_vec() };
        // 只有赋值可能改动 rowid 时才需要活动 rowid 集合:整表更新通常一个
        // `INTEGER PRIMARY KEY` 都没碰,构建与逐行维护这份集合纯属浪费。
        let rowid_move_possible = targets.iter().any(|(index, _)| Some(*index) == def.rowid_alias);
        let mut live_rowids: HashSet<i64> = if rowid_move_possible { live.iter().map(|(rowid, _)| *rowid).collect() } else { HashSet::new() };
        let mut writes = Vec::new();
        let mut returned = Vec::new();
        let mut affected = 0usize;
        // 逐行复用的求值与赋值缓冲:整表 `UPDATE` 因此不再为每一行重新分配行向量。
        let mut eval_cells: Vec<Cell> = Vec::with_capacity(width + 1);
        let mut next: Vec<Cell> = Vec::with_capacity(width);
        let mut assigned = vec![false; width];
        for (position, (rowid, cells)) in live.iter().enumerate() {
            eval_cells.clear();
            match joined {
                Some(joined) => match joined.get(rowid) {
                    Some(row) => eval_cells.extend_from_slice(row),
                    None => continue,
                },
                None => {
                    eval_cells.extend_from_slice(cells);
                    eval_cells.push(Cell::Integer(*rowid));
                }
            }
            let ctx = EvalContext {
                exec: self.clone(),
                parameters: self.parameters,
                upsert_excluded: self.upsert_excluded,
                trigger_frames: self.trigger_frames,
                outer_read: &self.state.outer_read,
                subquery_cache: &self.state.subquery_cache,
                host_functions: self.host_functions,
                outer: &[],
                current: shape,
                aggregates: &[],
                windows: &[],
                window_row: 0,
                budget,
                ctes: None,
            };
            if joined.is_none() {
                if let Some(predicate) = predicate {
                    if truthiness(&predicate.eval(&eval_cells, &ctx)?) != Some(true) {
                        continue;
                    }
                }
            }
            next.clear();
            next.extend_from_slice(cells);
            assigned.fill(false);
            let mut next_rowid = *rowid;
            for (index, target) in targets {
                let value = target.eval(&eval_cells, &ctx)?;
                let column = &def.columns[*index];
                let cell = write_value_cell(&apply_write_affinity(&column.declared_type, cell_write_value(&value)));
                if Some(*index) == def.rowid_alias {
                    match cell {
                        Cell::Null => {
                            return Err(if column.not_null {
                                SqlError::new("not-null-constraint", format!("NOT NULL constraint failed: {}.{}", def.name, column.name))
                            }
                            else {
                                SqlError::new("datatype-mismatch", "datatype mismatch")
                            }
                            .in_statement("update"));
                        }
                        Cell::Integer(value) => {
                            next_rowid = value;
                            next[*index] = Cell::Integer(value);
                        }
                        _ => return Err(SqlError::new("datatype-mismatch", "datatype mismatch").in_statement("update")),
                    }
                }
                else {
                    if matches!(cell, Cell::Null) && column.not_null {
                        return Err(SqlError::new("not-null-constraint", format!("NOT NULL constraint failed: {}.{}", def.name, column.name))
                            .in_statement("update"));
                    }
                    next[*index] = cell;
                }
                assigned[*index] = true;
            }
            if !def.checks.is_empty() {
                let mut check_cells = next.clone();
                check_cells.push(Cell::Integer(next_rowid));
                for check in &def.checks {
                    if truthiness(&check.expr.eval(&check_cells, &ctx)?) == Some(false) {
                        let name = check.name.clone().unwrap_or_else(|| check.text.clone());
                        return Err(SqlError::new("check-constraint", format!("CHECK constraint failed: {name}"))
                            .in_clause("CHECK")
                            .in_statement("update"));
                    }
                }
            }
            if let Some(label) = rowid_label {
                // `UNIQUE(rowid)` 只在换成另一个仍被占用的 rowid 时冲突。
                if next_rowid != *rowid && live_rowids.contains(&next_rowid) {
                    return Err(SqlError::new("unique-constraint", format!("UNIQUE constraint failed: {label}")).in_statement("update"));
                }
            }
            for key in &column_keys {
                if let Some(label) = unique_conflict(std::slice::from_ref(*key), &next, next_rowid, &state, *rowid) {
                    return Err(SqlError::new("unique-constraint", format!("UNIQUE constraint failed: {label}")).in_statement("update"));
                }
            }
            if !state.is_empty() && position < state.len() {
                state[position].0 = next_rowid;
                state[position].1 = next.clone();
            }
            if rowid_move_possible {
                live_rowids.remove(rowid);
                live_rowids.insert(next_rowid);
            }
            if let Some(returning) = returning {
                let mut new_cells = next.clone();
                new_cells.push(Cell::Integer(next_rowid));
                returned.push(returning.eval(&new_cells, &ctx)?);
            }
            let values = def
                .columns
                .iter()
                .enumerate()
                .map(|(index, _)| if assigned[index] { UpdateValue::Set(cell_write_value(&next[index])) } else { UpdateValue::Keep })
                .collect();
            writes.push((*rowid, values));
            affected += 1;
        }
        // 复用的快照优先直接交出所有权，避免再复制一份整表。
        let live_snapshot = match owned_live {
            Some(rows) => rows,
            None => live.to_vec(),
        };
        Ok(UpdatePlan { writes, returning: returned, affected, live: live_snapshot })
    }

    /// Plans one `DELETE`: the matching rowids in ascending order, plus any
    /// `RETURNING` rows, which see the row as it was before removal.
    ///
    /// `candidates` narrows the scan to the rowids a `WHERE <rowid> = <key>`
    /// equality or an index probe can reach. The predicate still runs over every
    /// candidate, so a stale or mismatched probe only costs the pushdown and
    /// never changes the removed set.
    pub fn plan_delete(
        &self,
        def: &TableDef,
        predicate: Option<&ResolvedExpr>,
        returning: Option<&ReturningPlan>,
        shape: &ScopeShape,
        budget: &Budget,
        candidates: Option<&[i64]>,
    ) -> Result<DeletePlan, SqlError> {
        let live = match candidates {
            Some(rowids) => self.live_rows_at(def, "delete", rowids)?,
            None => self.live_rows(def, "delete")?,
        };
        let mut removed = Vec::new();
        let mut returned = Vec::new();
        // 逐行复用的求值缓冲:批量删除不再为每一行复制一份行向量。
        let mut eval_cells: Vec<Cell> = Vec::with_capacity(def.columns.len() + 1);
        for (rowid, cells) in &live {
            eval_cells.clear();
            eval_cells.extend_from_slice(cells);
            eval_cells.push(Cell::Integer(*rowid));
            let ctx = EvalContext {
                exec: self.clone(),
                parameters: self.parameters,
                upsert_excluded: self.upsert_excluded,
                trigger_frames: self.trigger_frames,
                outer_read: &self.state.outer_read,
                subquery_cache: &self.state.subquery_cache,
                host_functions: self.host_functions,
                outer: &[],
                current: shape,
                aggregates: &[],
                windows: &[],
                window_row: 0,
                budget,
                ctes: None,
            };
            if let Some(predicate) = predicate {
                if truthiness(&predicate.eval(&eval_cells, &ctx)?) != Some(true) {
                    continue;
                }
            }
            if let Some(returning) = returning {
                returned.push(returning.eval(&eval_cells, &ctx)?);
            }
            removed.push(*rowid);
        }
        Ok(DeletePlan { removed, returning: returned, live })
    }
}

/// One uniqueness rule an `UPDATE` or an `ON CONFLICT` target re-checks
/// against the live table.
#[derive(Clone)]
pub enum UniqueKey {
    /// The hidden rowid, reported under `t.rowid` or `t.<alias>`.
    Rowid(String),
    /// One or more column positions, plus the `t.a, t.b` message fragment and
    /// the collation each key position compares with.
    Columns(Vec<usize>, String, Vec<Collation>),
}

impl UniqueKey {
    /// The key values of one row; `cells` holds the declared columns only.
    pub fn values(&self, cells: &[Cell], rowid: i64) -> Vec<Cell> {
        match self {
            Self::Rowid(_) => vec![Cell::Integer(rowid)],
            Self::Columns(positions, _, _) => positions.iter().map(|position| cells[*position].clone()).collect(),
        }
    }

    /// The key name SQLite reports in constraint messages.
    pub fn label(&self) -> &str {
        match self {
            Self::Rowid(label) | Self::Columns(_, label, _) => label,
        }
    }
}

/// Everything an `UPDATE` will write, computed before storage is touched so a
/// constraint failure can never leave a half-updated table.
pub struct UpdatePlan {
    /// Current rowid plus one slot per column, in ascending rowid order.
    pub writes: Vec<(i64, Vec<UpdateValue>)>,
    /// `RETURNING` rows, evaluated against the new row values.
    pub returning: Vec<Vec<Cell>>,
    /// How many rows the plan rewrites.
    pub affected: usize,
    /// 规划期读到的行集快照。处理阶段直接复用它，不再重复整表解码。
    pub live: Vec<(i64, Vec<Cell>)>,
}

/// Everything a `DELETE` will remove.
pub struct DeletePlan {
    /// The rowids the plan removes.
    pub removed: Vec<i64>,
    /// `RETURNING` rows, evaluated against the row as it was.
    pub returning: Vec<Vec<Cell>>,
    /// 规划期读到的行集快照（以 `removed` 为子集），供触发器与外键动作复用。
    pub live: Vec<(i64, Vec<Cell>)>,
}

/// One engine-managed unique index an `ON CONFLICT` target matched.
#[derive(Clone)]
pub struct ManagedTarget {
    /// The partial or expression index the target selected.
    pub index: IndexDef,
    /// `t.a, t.b` fragment for uniqueness failures on the index.
    pub label: String,
}

/// The conflict target and rewrite of one `INSERT ... ON CONFLICT` clause.
pub struct UpsertPlan {
    /// Uniqueness rules the clause reacts to.
    pub keys: Vec<UniqueKey>,
    /// Managed indexes the clause reacts to.
    pub managed: Vec<ManagedTarget>,
    /// Other uniqueness rules that still abort the statement when the supplied
    /// row violates them.
    pub others: Vec<UniqueKey>,
    /// Managed indexes outside the target that still abort the statement.
    pub other_managed: Vec<ManagedTarget>,
    /// `DO NOTHING` or `DO UPDATE`.
    pub action: UpsertAction,
    /// `DO UPDATE SET` targets with their resolved right-hand sides.
    pub assignments: Vec<(usize, ResolvedExpr)>,
    /// Optional `DO UPDATE ... WHERE` predicate over the stored and supplied
    /// rows.
    pub filter: Option<ResolvedExpr>,
}

/// What an `ON CONFLICT` clause does with a conflicting row.
pub enum UpsertAction {
    /// `DO NOTHING`: the supplied row is discarded.
    Nothing,
    /// `DO UPDATE`: the conflicting stored row is rewritten.
    Update,
}

/// Result of applying one supplied row of an upsert.
pub enum UpsertOutcome {
    /// `DO NOTHING` skipped the row.
    Skipped,
    /// The row was written with this rowid.
    Inserted(i64),
    /// The conflicting row was rewritten.
    Updated {
        /// Rowid of the rewritten row.
        rowid: i64,
        /// `RETURNING` rows evaluated against the new values.
        rows: Vec<Vec<Cell>>,
    },
}

/// Column names a trigger frame exposes: the declared columns plus `rowid`.
pub fn trigger_frame_columns(def: &TableDef) -> Vec<String> {
    let mut columns: Vec<String> = def.columns.iter().map(|column| column.name.clone()).collect();
    columns.push("rowid".to_string());
    columns
}

/// Appends the rowid slot a trigger frame expects to one declared row.
pub fn frame_cells(cells: &[Cell], width: usize, rowid: i64) -> Vec<Cell> {
    let mut out = cells.to_vec();
    out.resize(width, Cell::Null);
    out.push(Cell::Integer(rowid));
    out
}

/// `INSERT ... SELECT` slot mapping: SQLite reports a different arity message
/// depending on whether an explicit column list was written.
pub fn select_arity(
    def: &TableDef,
    table: &str,
    columns: Option<&[String]>,
    values: &[WriteValue],
) -> Result<Vec<Option<WriteValue>>, SqlError> {
    match columns {
        Some(names) => {
            if values.len() != names.len() {
                return Err(
                    SqlError::new("arity-mismatch", format!("{} values for {} columns", values.len(), names.len())).in_statement("insert")
                );
            }
            self_contained_arity(def, table, Some(names), values)
        }
        None => {
            if values.len() != def.columns.len() {
                return Err(SqlError::new(
                    "arity-mismatch",
                    format!("table {table} has {} columns but {} values were supplied", def.columns.len(), values.len()),
                )
                .in_statement("insert"));
            }
            self_contained_arity(def, table, None, values)
        }
    }
}

/// Converts one select result cell into an `INSERT` value when the target is a
/// view handled by `INSTEAD OF` triggers.
pub fn cell_insert_value(cell: &Cell) -> InsertValue {
    InsertValue::Literal(match cell {
        Cell::Null => Literal::Null,
        Cell::Integer(value) => Literal::Integer(*value),
        Cell::Real(value) => Literal::Real(*value),
        Cell::Text(value) => Literal::Text(value.clone()),
        Cell::Blob(value) => Literal::Blob(value.clone()),
    })
}

/// Resolves the columns and values of one `INSERT` into per-declared-column
/// slots; columns the statement does not assign stay `None`.
pub fn self_contained_arity(
    def: &TableDef,
    table: &str,
    columns: Option<&[String]>,
    values: &[WriteValue],
) -> Result<Vec<Option<WriteValue>>, SqlError> {
    let mut slots = vec![None; def.columns.len()];
    match columns {
        Some(names) => {
            if names.len() != values.len() {
                return Err(SqlError::new(
                    "arity-mismatch",
                    format!("insert into `{table}` has {} columns but {} values", names.len(), values.len()),
                )
                .in_statement("insert"));
            }
            let mut seen = Vec::new();
            for (name, value) in names.iter().zip(values) {
                let index = def.columns.iter().position(|column| column.name.eq_ignore_ascii_case(name)).ok_or_else(|| {
                    SqlError::new("unknown-column", format!("unknown column `{name}` in table `{table}`")).in_statement("insert")
                })?;
                if seen.contains(&index) {
                    return Err(
                        SqlError::new("duplicate-column", format!("column `{name}` is listed twice in the insert")).in_statement("insert")
                    );
                }
                seen.push(index);
                slots[index] = Some(value.clone());
            }
        }
        None => {
            if values.len() != def.columns.len() {
                return Err(SqlError::new(
                    "arity-mismatch",
                    format!("insert into `{table}` expects {} values but received {}", def.columns.len(), values.len()),
                )
                .in_statement("insert"));
            }
            for (index, value) in values.iter().enumerate() {
                slots[index] = Some(value.clone());
            }
        }
    }
    Ok(slots)
}

/// Builds the row a `CHECK` is evaluated against right after an insert.
///
/// Missing slots take the column default, the rowid alias is filled from the
/// rowid the writer assigned, and one extra cell carries the hidden rowid so a
/// check that names `rowid` reads it.
pub fn check_row(def: &TableDef, slots: &[Option<WriteValue>], rowid: i64) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(def.columns.len() + 1);
    for (index, slot) in slots.iter().enumerate() {
        let column = &def.columns[index];
        let value = match slot {
            Some(value) => value.clone(),
            None => column.default_value.clone().unwrap_or(WriteValue::Null),
        };
        cells.push(write_value_cell(&apply_write_affinity(&column.declared_type, value)));
    }
    if let Some(alias) = def.rowid_alias {
        if matches!(cells.get(alias), Some(Cell::Null)) {
            cells[alias] = Cell::Integer(rowid);
        }
    }
    cells.push(Cell::Integer(rowid));
    cells
}

/// `INSERT OR IGNORE` 吞掉 UNIQUE / NOT NULL / CHECK 冲突，其余错误照常上报。
pub fn ignoreable_insert_error(error: &SqlError) -> bool {
    let message = error.message.as_str();
    message.contains("UNIQUE constraint failed")
        || message.contains("NOT NULL constraint failed")
        || message.contains("CHECK constraint failed")
}

/// Reports one `STRICT` storage-class violation: the value is first converted
/// by the column affinity, then has to land in the declared storage class.
pub fn strict_value_violation(def: &TableDef, position: usize, value: &WriteValue, statement: &'static str) -> Option<SqlError> {
    if !def.strict || matches!(value, WriteValue::Null) {
        return None;
    }
    let column = &def.columns[position];
    let converted = apply_write_affinity(&column.declared_type, value.clone());
    let declared = column.declared_type.to_ascii_uppercase();
    let conforms = match declared.as_str() {
        "INT" | "INTEGER" => matches!(converted, WriteValue::Integer(_)),
        "REAL" => matches!(converted, WriteValue::Real(_)),
        "TEXT" => matches!(converted, WriteValue::Text(_)),
        "BLOB" => matches!(converted, WriteValue::Blob(_)),
        _ => true,
    };
    if conforms {
        return None;
    }
    let stored = match converted {
        WriteValue::Real(_) => "REAL",
        WriteValue::Text(_) => "TEXT",
        WriteValue::Blob(_) => "BLOB",
        _ => "INTEGER",
    };
    Some(
        SqlError::new("constraint-datatype", format!("cannot store {stored} value in {declared} column {}.{}", def.name, column.name))
            .in_statement(statement),
    )
}

/// Converts one decoded cell into the write value that stores it back unchanged.
pub fn cell_write_value(cell: &Cell) -> WriteValue {
    match cell {
        Cell::Null => WriteValue::Null,
        Cell::Integer(value) => WriteValue::Integer(*value),
        Cell::Real(value) => WriteValue::Real(*value),
        Cell::Text(value) => WriteValue::Text(value.clone()),
        Cell::Blob(value) => WriteValue::Blob(value.clone()),
    }
}

/// 把一行已解码的单元格整体转成写侧的存储值,供写语句复用自己已读出的行像。
pub fn cells_write_values(cells: &[Cell]) -> Vec<WriteValue> {
    cells.iter().map(cell_write_value).collect()
}

/// Inserts one row into an in-memory temp table and returns its rowid.
///
/// The rowid is explicit when the table has an `INTEGER PRIMARY KEY` alias and
/// the statement supplied one, and `max(rowid) + 1` otherwise. NOT NULL and the
/// primary key / UNIQUE keys are enforced here because temp tables never reach
/// the format-3 writer.
pub fn temp_insert_row(
    def: &TableDef,
    rows: &mut BTreeMap<i64, Vec<Cell>>,
    slots: &[Option<WriteValue>],
    keys: &[UniqueKey],
) -> Result<i64, SqlError> {
    let mut rowid = rows.keys().next_back().copied().unwrap_or(0) + 1;
    if let Some(alias) = def.rowid_alias {
        match slots.get(alias).and_then(|slot| slot.as_ref()) {
            Some(WriteValue::Integer(value)) => rowid = *value,
            Some(WriteValue::Null) | None => {}
            Some(_) => return Err(SqlError::new("datatype-mismatch", "datatype mismatch").in_statement("insert")),
        }
    }
    let cells = check_row(def, slots, rowid);
    // `check_row` appends the rowid as a hidden slot for CHECK/RETURNING
    // evaluation; the stored temp row keeps only the declared columns and the
    // row map key carries the rowid.
    let cells: Vec<Cell> = cells.into_iter().take(def.columns.len()).collect();
    for (position, column) in def.columns.iter().enumerate() {
        if column.not_null && matches!(cells[position], Cell::Null) {
            return Err(SqlError::new("not-null-constraint", format!("NOT NULL constraint failed: {}.{}", def.name, column.name))
                .in_statement("insert"));
        }
    }
    let live: Vec<(i64, Vec<Cell>)> = rows.iter().map(|(rowid, cells)| (*rowid, cells.clone())).collect();
    if let Some(label) = unique_conflict(keys, &cells, rowid, &live, i64::MIN) {
        return Err(SqlError::new("unique-constraint", format!("UNIQUE constraint failed: {label}")).in_statement("insert"));
    }
    rows.insert(rowid, cells);
    Ok(rowid)
}

/// Declared column positions of a managed index whose terms are all plain
/// columns, or `None` for a real expression key.
pub fn managed_term_positions(index: &IndexDef) -> Option<Vec<usize>> {
    index
        .terms
        .iter()
        .map(|term| match term {
            ResolvedExpr::Column { source: 0, index: column } => Some(*column),
            _ => None,
        })
        .collect()
}

/// Whether one managed index is the one an `ON CONFLICT (columns)` target names.
pub fn managed_target_matches(target: &ManagedTarget, positions: &[usize], predicate: Option<&ResolvedExpr>) -> bool {
    if target.index.predicate.as_ref() != predicate {
        return false;
    }
    match managed_term_positions(&target.index) {
        Some(terms) => !terms.is_empty() && same_positions(&terms, positions),
        None => false,
    }
}

/// `t.a, t.b` fragment of a managed index, matching SQLite's uniqueness text.
pub fn managed_index_label(def: &TableDef, index: &IndexDef) -> String {
    match managed_term_positions(index) {
        Some(positions) if !positions.is_empty() => {
            positions.iter().map(|position| format!("{}.{}", def.name, def.columns[*position].name)).collect::<Vec<_>>().join(", ")
        }
        _ => format!("index '{}'", index.name),
    }
}

/// Whether two computed managed-index keys collide, with SQLite's rule that a
/// `NULL` component never conflicts.
pub fn managed_keys_conflict(first: &[Cell], second: &[Cell], collations: &[Collation]) -> bool {
    first.len() == second.len()
        && first.iter().zip(second).enumerate().all(|(position, (left, right))| {
            !matches!(left, Cell::Null)
                && !matches!(right, Cell::Null)
                && compare_cells_with(left, right, collations.get(position).copied().unwrap_or_default()) == Ordering::Equal
        })
}

/// Whether one supplied upsert row collides with one stored row on `key`.
///
/// A `NULL` in any key column never conflicts, exactly like a unique index.
pub fn upsert_key_conflicts(key: &UniqueKey, candidate: &[Cell], candidate_rowid: i64, existing_rowid: i64, existing: &[Cell]) -> bool {
    match key {
        UniqueKey::Rowid(_) => candidate_rowid == existing_rowid,
        UniqueKey::Columns(positions, _, collations) => positions.iter().enumerate().all(|(index, position)| {
            let first = &candidate[*position];
            let second = &existing[*position];
            !matches!(first, Cell::Null)
                && !matches!(second, Cell::Null)
                && compare_cells_with(first, second, collations.get(index).copied().unwrap_or_default()) == Ordering::Equal
        }),
    }
}

/// Reports the first uniqueness rule the candidate row violates against the
/// live table, skipping the row being replaced.
pub fn unique_conflict(keys: &[UniqueKey], candidate: &[Cell], rowid: i64, live: &[(i64, Vec<Cell>)], skip: i64) -> Option<String> {
    for key in keys {
        let wanted = key.values(candidate, rowid);
        for (other_rowid, other_cells) in live {
            if *other_rowid == skip {
                continue;
            }
            let other = key.values(other_cells, *other_rowid);
            let collations = match key {
                UniqueKey::Rowid(_) => &[][..],
                UniqueKey::Columns(_, _, collations) => collations.as_slice(),
            };
            let collides = wanted.iter().zip(&other).enumerate().all(|(index, (first, second))| {
                !matches!(first, Cell::Null)
                    && !matches!(second, Cell::Null)
                    && compare_cells_with(first, second, collations.get(index).copied().unwrap_or_default()) == Ordering::Equal
            });
            if collides {
                return Some(key.label().to_string());
            }
        }
    }
    None
}

/// Converts a storage rejection of an `UPDATE` / `DELETE` into a structured
/// error, naming the table when the writer does not say which key failed.
pub fn write_failure(error: database_types::Error, statement: &'static str, def: &TableDef) -> SqlError {
    let mapped = storage_failure(error);
    let message = if mapped.code == "unique-constraint" {
        format!("UNIQUE constraint failed in table `{}`", def.name)
    }
    else if mapped.code == "not-null-constraint" || mapped.message.contains("NOT NULL") {
        format!("NOT NULL constraint failed in table `{}`", def.name)
    }
    else if mapped.message.contains("integer") || mapped.message.contains("datatype") {
        return SqlError::new("datatype-mismatch", "datatype mismatch").in_statement(statement);
    }
    else {
        mapped.message
    };
    SqlError::new(mapped.code, message).in_statement(statement)
}

/// 管理型索引键里是否存在按排序规则等价的重复项，`NULL` 键不算冲突。
pub fn managed_unique_violation(entries: &[(Vec<WriteValue>, i64)], collations: &[Collation]) -> bool {
    for right in 1..entries.len() {
        for left in 0..right {
            let collides = entries[left].0.iter().zip(&entries[right].0).enumerate().all(|(position, (first, second))| {
                if matches!(first, WriteValue::Null) || matches!(second, WriteValue::Null) {
                    return false;
                }
                let collation = collations.get(position).copied().unwrap_or_default();
                match (first, second) {
                    (WriteValue::Text(first), WriteValue::Text(second)) => collation.compare_text(first, second).is_eq(),
                    _ => first == second,
                }
            });
            if collides {
                return true;
            }
        }
    }
    false
}
