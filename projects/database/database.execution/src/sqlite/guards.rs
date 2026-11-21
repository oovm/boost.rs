//! SQLite DML 的约束守卫:唯一键目录、外键校验与触发器帧辅助。
//!
//! 本模块把只读守卫机器挂在 [`ExecContext`] 上(与 [`super::dml`] 的读写机器
//! 是同一类型,跨模块 impl 合并);写机器在触发与外键动作里经这些方法校验约束。
//! 产品门面不得被本模块反向依赖。

use std::cmp::Ordering;

use database_query::sqlite::{SqlError, bind::*, catalog::*, frontend::*, storage_failure, value::*};
use database_storage::sqlite::{Collation, TableLimits, UpdateValue};

use super::{
    cells::*,
    dml::*,
    eval::{Budget, EvalContext, EvalExpr},
    select::{ExecContext, decode_row},
    window::*,
};

impl<'e> ExecContext<'e> {
    /// Rejects a `UNIQUE` index when two existing rows carry the same key.
    ///
    /// SQLite treats a `NULL` key column as distinct, so rows holding `NULL`
    /// never collide. Key comparison follows the engine's storage-class rules,
    /// which makes `1` and `1.0` collide just like upstream.
    pub fn ensure_unique_keys(&self, table: &str, def: &TableDef, columns: &[IndexColumn]) -> Result<(), SqlError> {
        let mut positions = Vec::with_capacity(columns.len());
        let mut collations = Vec::with_capacity(columns.len());
        for column in columns {
            let position = def.columns.iter().position(|existing| existing.name.eq_ignore_ascii_case(&column.name)).ok_or_else(|| {
                SqlError::new("unknown-column", format!("unknown column `{}` in table `{table}`", column.name)).in_statement("create-index")
            })?;
            positions.push(position);
            // 键项显式 `COLLATE` 优先，否则继承列的声明排序规则。
            collations.push(match &column.collate {
                Some(name) => Collation::parse(name).ok_or_else(|| unknown_collation(name).in_statement("create-index"))?,
                None => def.columns[position].collate.unwrap_or_default(),
            });
        }
        let mut rows = Vec::new();
        for row in self.writer.table_rows(table, TableLimits::query()).map_err(|error| storage_failure(error).in_statement("create-index"))? {
            rows.push(decode_row(&row.payload, def, row.rowid)?);
        }
        for right in 1..rows.len() {
            for left in 0..right {
                let collides = positions.iter().enumerate().all(|(position, column)| {
                    let (first, second) = (&rows[left][*column], &rows[right][*column]);
                    !matches!(first, Cell::Null)
                        && !matches!(second, Cell::Null)
                        && compare_cells_with(first, second, collations[position]) == Ordering::Equal
                });
                if collides {
                    let names =
                        positions.iter().map(|position| format!("{table}.{}", def.columns[*position].name)).collect::<Vec<_>>().join(", ");
                    return Err(SqlError::new("unique-constraint", format!("UNIQUE constraint failed: {names}")).in_statement("create-index"));
                }
            }
        }
        Ok(())
    }

    /// Resolves one foreign key against the live catalog: the parent table
    /// position and the parent key column positions.
    ///
    /// A missing parent table and a parent key that is not unique both report
    /// the codes SQLite uses (`SQLITE_ERROR`), because the same statements
    /// fail identically in the reference engine.
    pub fn foreign_key_parent(&self, child: &TableDef, key: &ForeignKeyDef, statement: &'static str) -> Result<(usize, Vec<usize>), SqlError> {
        let parent_position = *self
            .catalog
            .table_lookup
            .get(&key.parent)
            .ok_or_else(|| SqlError::new("SQLITE_ERROR", format!("no such table: main.{}", key.parent_display)).in_statement(statement))?;
        let parent = &self.catalog.tables[parent_position];
        let positions = if key.parent_columns.is_empty() {
            parent.primary_key_positions().ok_or_else(|| foreign_key_mismatch(child, key, statement))?
        }
        else {
            let mut positions = Vec::with_capacity(key.parent_columns.len());
            for name in &key.parent_columns {
                let position = parent
                    .columns
                    .iter()
                    .position(|column| column.name.eq_ignore_ascii_case(name))
                    .ok_or_else(|| foreign_key_mismatch(child, key, statement))?;
                positions.push(position);
            }
            positions
        };
        if positions.len() != key.columns.len() || !self.parent_key_is_unique(parent, &positions) {
            return Err(foreign_key_mismatch(child, key, statement));
        }
        Ok((parent_position, positions))
    }

    /// Whether `positions` identifies at most one parent row: the primary key,
    /// a column-level `UNIQUE` / `PRIMARY KEY`, or an explicit unique index.
    pub fn parent_key_is_unique(&self, parent: &TableDef, positions: &[usize]) -> bool {
        if let Some(alias) = parent.rowid_alias {
            if positions == [alias] {
                return true;
            }
        }
        if !parent.primary_key.is_empty() && same_positions(&parent.primary_key, positions) {
            return true;
        }
        let declared: Vec<usize> =
            parent.columns.iter().enumerate().filter(|(_, column)| column.primary_key).map(|(position, _)| position).collect();
        if !declared.is_empty() && same_positions(&declared, positions) {
            return true;
        }
        if positions.len() == 1 && parent.columns[positions[0]].unique {
            return true;
        }
        if positions.iter().all(|position| parent.columns[*position].unique) {
            return true;
        }
        let parent_key = parent.name.to_ascii_lowercase();
        self.catalog.indexes.values().any(|index| index.table == parent_key && index.unique && same_positions(&index.columns, positions))
    }

    /// Every foreign key in the catalog that references `parent`, as
    /// `(child table position, key)` pairs; the child may be `parent` itself.
    pub fn referencing_foreign_keys(&self, parent: &str) -> Vec<(usize, ForeignKeyDef)> {
        let mut found = Vec::new();
        for (position, table) in self.catalog.tables.iter().enumerate() {
            for key in &table.foreign_keys {
                if key.parent.eq_ignore_ascii_case(parent) {
                    found.push((position, key.clone()));
                }
            }
        }
        found
    }

    /// Verifies the rows an `INSERT` just wrote against every foreign key of
    /// their table. The check runs after the writes so a self-reference may
    /// point at a later row of the same statement.
    pub fn check_insert_foreign_keys(
        &self,
        def: &TableDef,
        rowids: &[i64],
        statement: &'static str,
        in_transaction: bool,
    ) -> Result<(), SqlError> {
        if def.foreign_keys.is_empty() {
            return Ok(());
        }
        let live = self.live_rows(def, statement)?;
        for key in &def.foreign_keys {
            if key.deferred && in_transaction {
                continue;
            }
            let (parent_position, parent_columns) = self.foreign_key_parent(def, key, statement)?;
            let parent = &self.catalog.tables[parent_position];
            let parent_rows = self.live_rows(parent, statement)?;
            for (rowid, cells) in &live {
                if !rowids.contains(rowid) {
                    continue;
                }
                let values = pick(cells, &key.columns);
                // A NULL in any child key column satisfies the constraint.
                if values.iter().any(|cell| matches!(cell, Cell::Null)) {
                    continue;
                }
                if !parent_rows.iter().any(|(_, parent_cells)| keys_equal(&values, parent_cells, &parent_columns)) {
                    return Err(foreign_key_violation(statement));
                }
            }
        }
        Ok(())
    }

    /// Verifies an `UPDATE` against both sides of every foreign key it touches.
    ///
    /// Child keys are re-checked only when the statement assigns one of their
    /// columns. Referenced parent keys are re-checked only when the statement
    /// assigns one of their columns, and then only child rows still holding a
    /// value the update removed are examined.
    pub fn check_update_foreign_keys(
        &self,
        def: &TableDef,
        plan: &UpdatePlan,
        old_rows: &[(i64, Vec<Cell>)],
        assigned: &[bool],
        statement: &'static str,
        in_transaction: bool,
    ) -> Result<(), SqlError> {
        for key in &def.foreign_keys {
            if key.deferred && in_transaction {
                continue;
            }
            if !key.columns.iter().any(|position| assigned[*position]) {
                continue;
            }
            let (parent_position, parent_columns) = self.foreign_key_parent(def, key, statement)?;
            let parent_rows = self.live_rows(&self.catalog.tables[parent_position], statement)?;
            for (rowid, values) in &plan.writes {
                let old = row_of(old_rows, *rowid);
                let next = apply_assignment(old, values);
                let child_values = pick(&next, &key.columns);
                if child_values.iter().any(|cell| matches!(cell, Cell::Null)) {
                    continue;
                }
                if !parent_rows.iter().any(|(_, parent_cells)| keys_equal(&child_values, parent_cells, &parent_columns)) {
                    return Err(foreign_key_violation(statement));
                }
            }
        }
        for (child_position, key) in self.referencing_foreign_keys(&def.name) {
            if key.deferred && in_transaction && key.on_update_action != ForeignKeyAction::Restrict {
                continue;
            }
            let child = &self.catalog.tables[child_position];
            let (parent_position, parent_columns) = self.foreign_key_parent(child, &key, statement)?;
            if !parent_columns.iter().any(|position| assigned.get(*position).copied().unwrap_or(false)) {
                continue;
            }
            let mut changed: Vec<Vec<Cell>> = Vec::new();
            for (rowid, values) in &plan.writes {
                let old = row_of(old_rows, *rowid);
                let old_values = pick(old, &parent_columns);
                if old_values.iter().any(|cell| matches!(cell, Cell::Null)) {
                    continue;
                }
                let next = apply_assignment(old, values);
                let new_values = pick(&next, &parent_columns);
                if !cells_equal(&old_values, &new_values) {
                    changed.push(old_values);
                }
            }
            if changed.is_empty() {
                continue;
            }
            let parent_rows = self.live_rows(&self.catalog.tables[parent_position], statement)?;
            let child_rows = self.live_rows(child, statement)?;
            for (_, child_cells) in &child_rows {
                let child_values = pick(child_cells, &key.columns);
                if child_values.iter().any(|cell| matches!(cell, Cell::Null)) {
                    continue;
                }
                if changed.iter().any(|value| cells_equal(value, &child_values))
                    && !parent_rows.iter().any(|(_, parent_cells)| keys_equal(&child_values, parent_cells, &parent_columns))
                {
                    return Err(foreign_key_violation(statement));
                }
            }
        }
        Ok(())
    }

    /// Verifies a `DELETE`: its own foreign keys are still resolved (a malformed
    /// or missing parent fails the statement like SQLite), and every child row
    /// that referenced a removed parent key is looked for after the removal.
    pub fn check_delete_foreign_keys(
        &self,
        def: &TableDef,
        old_rows: &[(i64, Vec<Cell>)],
        removed: &[i64],
        statement: &'static str,
        in_transaction: bool,
    ) -> Result<(), SqlError> {
        for key in &def.foreign_keys {
            self.foreign_key_parent(def, key, statement)?;
        }
        for (child_position, key) in self.referencing_foreign_keys(&def.name) {
            if key.deferred && in_transaction && key.on_delete_action != ForeignKeyAction::Restrict {
                continue;
            }
            let child = &self.catalog.tables[child_position];
            let (_, parent_columns) = self.foreign_key_parent(child, &key, statement)?;
            let mut gone: Vec<Vec<Cell>> = Vec::new();
            for (rowid, cells) in old_rows {
                if !removed.contains(rowid) {
                    continue;
                }
                let values = pick(cells, &parent_columns);
                if values.iter().any(|cell| matches!(cell, Cell::Null)) {
                    continue;
                }
                gone.push(values);
            }
            if gone.is_empty() {
                continue;
            }
            let child_rows = self.live_rows(child, statement)?;
            for (_, child_cells) in &child_rows {
                let child_values = pick(child_cells, &key.columns);
                if child_values.iter().any(|cell| matches!(cell, Cell::Null)) {
                    continue;
                }
                if gone.iter().any(|value| cells_equal(value, &child_values)) {
                    return Err(foreign_key_violation(statement));
                }
            }
        }
        Ok(())
    }

    /// Re-checks every deferred foreign key against the state `COMMIT` would
    /// publish. Immediate keys were already enforced per statement, so only
    /// deferred keys are walked. A violation leaves the transaction open, as
    /// SQLite does, so the caller can repair the rows and commit again.
    pub fn check_deferred_foreign_keys(&self, foreign_keys: bool) -> Result<(), SqlError> {
        if !foreign_keys {
            return Ok(());
        }
        for child in &self.catalog.tables {
            for key in child.foreign_keys.iter().filter(|key| key.deferred) {
                let (parent_position, parent_columns) = self.foreign_key_parent(child, key, "commit")?;
                let parent = &self.catalog.tables[parent_position];
                let parent_rows = self.live_rows(parent, "commit")?;
                let child_rows = self.live_rows(child, "commit")?;
                for (_, cells) in &child_rows {
                    let values = pick(cells, &key.columns);
                    if values.iter().any(|cell| matches!(cell, Cell::Null)) {
                        continue;
                    }
                    if !parent_rows.iter().any(|(_, parent_cells)| keys_equal(&values, parent_cells, &parent_columns)) {
                        return Err(foreign_key_violation("commit"));
                    }
                }
            }
        }
        Ok(())
    }

    /// Every uniqueness rule an `UPDATE` has to re-check, in the order SQLite
    /// reports them: the rowid first, then the primary key, then column-level
    /// `UNIQUE`, then explicit unique indexes.
    pub fn unique_keys(&self, def: &TableDef, table_key: &str) -> Vec<UniqueKey> {
        let label = |positions: &[usize]| {
            positions.iter().map(|position| format!("{}.{}", def.name, def.columns[*position].name)).collect::<Vec<_>>().join(", ")
        };
        let rowid_label = match def.rowid_alias {
            Some(alias) => format!("{}.{}", def.name, def.columns[alias].name),
            None => format!("{}.rowid", def.name),
        };
        let mut keys = Vec::new();
        if !def.without_rowid {
            keys.push(UniqueKey::Rowid(rowid_label));
        }
        // A WITHOUT ROWID table has no rowid and its primary key is the
        // clustered key, so that key is the first uniqueness rule.
        if def.without_rowid && !def.primary_key.is_empty() {
            keys.push(UniqueKey::Columns(def.primary_key.clone(), label(&def.primary_key), self.key_collations(def, &def.primary_key)));
        }
        else if def.primary_key_index && !def.primary_key.is_empty() {
            keys.push(UniqueKey::Columns(def.primary_key.clone(), label(&def.primary_key), self.key_collations(def, &def.primary_key)));
        }
        for (position, column) in def.columns.iter().enumerate() {
            if Some(position) == def.rowid_alias {
                continue;
            }
            if column.primary_key || column.unique {
                keys.push(UniqueKey::Columns(vec![position], label(&[position]), self.key_collations(def, &[position])));
            }
        }
        for constraint in &def.table_constraints {
            if !constraint.primary {
                keys.push(UniqueKey::Columns(
                    constraint.columns.clone(),
                    label(&constraint.columns),
                    self.key_collations(def, &constraint.columns),
                ));
            }
        }
        for index in self.catalog.indexes.values() {
            // Managed indexes are expression or partial, so their uniqueness is
            // enforced while their keys are rebuilt from the live rows.
            if !def.temp && !index.managed && index.table == table_key && index.unique {
                let collations = if index.collations.len() == index.columns.len() {
                    index.collations.clone()
                }
                else {
                    self.key_collations(def, &index.columns)
                };
                keys.push(UniqueKey::Columns(index.columns.clone(), label(&index.columns), collations));
            }
        }
        keys
    }

    /// 各键位按列声明取得的排序规则，供唯一性与冲突检测使用。
    pub fn key_collations(&self, def: &TableDef, positions: &[usize]) -> Vec<Collation> {
        positions.iter().map(|position| def.columns[*position].collate.unwrap_or_default()).collect()
    }

    /// True when the table owns any index whose b-tree has to be rewritten
    /// after a write.
    pub fn has_indexes(&self, def: &TableDef, table_key: &str) -> bool {
        def.primary_key_index
            || def
                .columns
                .iter()
                .enumerate()
                .any(|(position, column)| (column.primary_key || column.unique) && Some(position) != def.rowid_alias)
            || def.table_constraints.iter().any(|constraint| !constraint.primary)
            || (!def.temp && self.catalog.indexes.values().any(|index| index.table == table_key))
    }

    /// Evaluates the declared `CHECK` constraints of `def` against one row.
    ///
    /// `cells` carries the row in declared order plus the appended rowid slot
    /// the CHECK expressions resolve against; the first failing constraint is
    /// reported with its `CHECK` clause so the statement surface keeps its
    /// structured error shape. A `NULL` result passes, like SQLite.
    pub fn check_row_constraints(&self, def: &TableDef, cells: &[Cell], shape: &ScopeShape, budget: &Budget) -> Result<(), SqlError> {
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
        for check in &def.checks {
            let value = check.expr.eval(cells, &ctx)?;
            // SQLite only fails a check whose result is false; a NULL result passes.
            if truthiness(&value) == Some(false) {
                let mut error = SqlError::new(
                    "check-constraint",
                    format!("CHECK constraint failed: {}", check.name.clone().unwrap_or_else(|| check.text.clone())),
                )
                .in_statement("insert");
                error.clause = Some("CHECK".to_string());
                return Err(error);
            }
        }
        Ok(())
    }
}

/// The error SQLite raises when a parent key is not a unique key of its table.
pub fn foreign_key_mismatch(child: &TableDef, key: &ForeignKeyDef, statement: &'static str) -> SqlError {
    SqlError::new("SQLITE_ERROR", format!("foreign key mismatch - \"{}\" referencing \"{}\"", child.name, key.parent_display))
        .in_statement(statement)
}

/// The error SQLite raises when a non-NULL child key has no parent row.
pub fn foreign_key_violation(statement: &'static str) -> SqlError {
    SqlError::new("SQLITE_CONSTRAINT_FOREIGNKEY", "FOREIGN KEY constraint failed").in_statement(statement)
}

/// Selects the cells at `positions` from one decoded row.
pub fn pick(cells: &[Cell], positions: &[usize]) -> Vec<Cell> {
    positions.iter().map(|position| cells[*position].clone()).collect()
}

/// Whether one child key equals the parent key held by `parent_cells`.
pub fn keys_equal(child_values: &[Cell], parent_cells: &[Cell], parent_columns: &[usize]) -> bool {
    child_values.iter().zip(parent_columns.iter()).all(|(child, position)| compare_cells(child, &parent_cells[*position]) == Ordering::Equal)
}

/// Copies one pre-statement row and applies an `UPDATE` assignment to it.
pub fn apply_assignment(old: &[Cell], values: &[UpdateValue]) -> Vec<Cell> {
    let mut next = old.to_vec();
    for (index, value) in values.iter().enumerate() {
        if let UpdateValue::Set(value) = value {
            next[index] = write_value_cell(value);
        }
    }
    next
}

/// The pre-statement cells of one rowid; the plan only contains live rowids.
pub fn row_of<'a>(rows: &'a [(i64, Vec<Cell>)], rowid: i64) -> &'a [Cell] {
    rows.iter().find(|(id, _)| *id == rowid).map(|(_, cells)| cells.as_slice()).unwrap_or(&[])
}

/// Whether two column-position lists name the same key, ignoring order.
pub fn same_positions(left: &[usize], right: &[usize]) -> bool {
    let mut left = left.to_vec();
    let mut right = right.to_vec();
    left.sort_unstable();
    right.sort_unstable();
    left == right
}
