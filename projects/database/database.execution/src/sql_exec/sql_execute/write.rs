//! 写入、冲突策略与约束校验。

use super::*;

pub(crate) fn execute_insert(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    plan: &WritePlan,
    dialect: SqlDialect,
) -> Result<SqlResult, SqlFrontendError> {
    match &plan.conflict {
        None => insert_row_if_absent(executor, transaction, plan, false, dialect),
        Some(ConflictPolicy::DoNothing) => insert_row_if_absent(executor, transaction, plan, true, dialect),
        Some(ConflictPolicy::DoUpdate { assignments, predicate }) => {
            upsert_row(executor, transaction, plan, assignments, predicate.as_ref(), dialect)
        }
    }
}

/// 无冲突改写的插入：`PutIfAbsent` 保证并发下也只有一行落地。
///
/// `do_nothing` 为真时冲突按「跳过」处理，否则按唯一约束错误上报。
///
/// 行与它的单列唯一索引条目进同一个提交边界：自动提交是一次 `commit_batch`，
/// 事务内先进 handle 缓冲。
pub(crate) fn insert_row_if_absent(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    plan: &WritePlan,
    do_nothing: bool,
    dialect: SqlDialect,
) -> Result<SqlResult, SqlFrontendError> {
    let mut row = write_plan_to_row(plan)?;
    apply_generated(&plan.generated, &plan.field_types, &mut row, dialect)?;
    let namespace = Namespace(plan.namespace.clone());
    let mut indexes = IndexMaintenance::open(executor, transaction, &plan.namespace, &plan.unique_constraints, &plan.field_types)?;
    // 唯一性复核与索引回填共用同一次全表快照；没有唯一约束时两者都不需要。
    let stored = if plan.unique_constraints.is_empty() {
        Vec::new()
    }
    else {
        scan_rows(executor, transaction, &namespace, &[], None, None, 0, "unique_check")?
    };
    if let Some(constraint) = unique_conflict_in_snapshot(&plan.unique_constraints, &row, None, &[], &stored)? {
        // `ON CONFLICT DO NOTHING` 对任何唯一冲突都跳过，而不是只跳过主键冲突。
        if do_nothing {
            return returning_result(&plan.returning, &plan.field_types, Vec::new(), 0, dialect);
        }
        return Err(duplicate_key_error(&constraint));
    }
    enforce_row_guards(executor, transaction, &plan.check_constraints, &plan.foreign_keys, &plan.field_types, &row, dialect)?;
    if indexes.needs_backfill() {
        indexes.backfill(&stored)?;
    }
    indexes.remember_insert(&row, &plan.rowid);
    let index_commands = indexes.take_commands();
    let command =
        KeyValueCommand { namespace: namespace.clone(), key: plan.rowid.clone(), action: KeyValueAction::PutIfAbsent(encode_row(&row)) };
    let inserted = commit_row_write(executor, transaction, command, CommitCondition::PutIfAbsent, &index_commands, "insert")?;
    let affected = match (inserted, do_nothing) {
        (true, _) => 1,
        (false, true) => 0,
        (false, false) => {
            return Err(SqlFrontendError::new(
                SqlErrorKind::DuplicateKey,
                format!("duplicate key value violates unique constraint: {}", plan.namespace),
            ));
        }
    };
    let written = if affected == 1 { vec![row] } else { Vec::new() };
    returning_result(&plan.returning, &plan.field_types, written, affected, dialect)
}

/// `ON CONFLICT DO UPDATE`：命中既有行时按 `SET` 改写，否则插入新行。
///
/// 事务内改写走 read-modify-write：把读到的 pre-image 交给 OCC，另一个事务
/// 并发提交同一行时 COMMIT 会被拒绝，而不是静默覆盖。`WHERE` 不成立时按
/// 0 行处理，既有行保持原值。
pub(crate) fn upsert_row(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    plan: &WritePlan,
    assignments: &[ConflictAssignment],
    predicate: Option<&SqlCatalogBoundExpression>,
    dialect: SqlDialect,
) -> Result<SqlResult, SqlFrontendError> {
    let namespace = Namespace(plan.namespace.clone());
    let mut indexes = IndexMaintenance::open(executor, transaction, &plan.namespace, &plan.unique_constraints, &plan.field_types)?;
    if indexes.needs_backfill() {
        let stored = index_snapshot(executor, transaction, &namespace)?;
        indexes.backfill(&stored)?;
    }
    // 读到的行可能刚好被并发事务插入，重新读一次即可命中改写路径。
    for _ in 0..2 {
        let existing_bytes = read_row_bytes(executor, transaction, &namespace, &plan.rowid)?;
        // MySQL 的 `ON DUPLICATE KEY UPDATE` 也覆盖次要 `UNIQUE` 冲突：主键没命中时，
        // 先找一条约束列相同的既有行，命中就改写那一行（主键保持不变）。
        let (target_rowid, existing_bytes) = match existing_bytes {
            Some(bytes) => (plan.rowid.clone(), bytes),
            None => {
                let mut proposed = write_plan_to_row(plan)?;
                apply_generated(&plan.generated, &plan.field_types, &mut proposed, dialect)?;
                match find_unique_conflict(executor, transaction, &plan.namespace, &plan.unique_constraints, &proposed)? {
                    Some((rowid, bytes)) => (rowid, bytes),
                    None => {
                        enforce_row_guards(executor, transaction, &plan.check_constraints, &plan.foreign_keys, &plan.field_types, &proposed, dialect)?;
                        indexes.remember_insert(&proposed, &plan.rowid);
                        let index_commands = indexes.take_commands();
                        let command = KeyValueCommand {
                            namespace: namespace.clone(),
                            key: plan.rowid.clone(),
                            action: KeyValueAction::PutIfAbsent(encode_row(&proposed)),
                        };
                        let inserted =
                            commit_row_write(executor, transaction, command, CommitCondition::PutIfAbsent, &index_commands, "upsert")?;
                        if inserted {
                            return returning_result(&plan.returning, &plan.field_types, vec![proposed], 1, dialect);
                        }
                        continue;
                    }
                }
            }
        };
        let existing_row = decode_row(&existing_bytes)?;
        // 赋值右值同时看到既有行（槽 0）与提议写入的 `EXCLUDED` 行（槽 1，列带上
        // `ROW_ID_STRIDE` 偏移），求值与应用由 `conflict_updated_row` 统一承担。
        let Some(row) = conflict_updated_row(&existing_row, plan, assignments, predicate, dialect)?
        else {
            return returning_result(&plan.returning, &plan.field_types, Vec::new(), 0, dialect);
        };
        if let Some(constraint) =
            unique_conflict_on_write(executor, transaction, &plan.namespace, &plan.unique_constraints, &row, Some(&target_rowid))?
        {
            return Err(duplicate_key_error(&constraint));
        }
        enforce_row_guards(executor, transaction, &plan.check_constraints, &plan.foreign_keys, &plan.field_types, &row, dialect)?;
        indexes.remember_replace(Some(&existing_row), &row, &target_rowid);
        let index_commands = indexes.take_commands();
        let command = KeyValueCommand { namespace: namespace.clone(), key: target_rowid, action: KeyValueAction::Put(encode_row(&row)) };
        let written =
            commit_row_write(executor, transaction, command, CommitCondition::ValueEquals(Some(existing_bytes)), &index_commands, "upsert")?;
        if !written {
            return Err(SqlFrontendError::new(
                SqlErrorKind::Conflict,
                format!("concurrent write to row in {} prevented the conflict update", plan.namespace),
            ));
        }
        // MySQL 对「命中并改写」报 2（插入报 1），驱动据此区分两种结果。
        return returning_result(&plan.returning, &plan.field_types, vec![row], 2, dialect);
    }
    Err(SqlFrontendError::new(SqlErrorKind::Conflict, format!("concurrent write to row in {} prevented the conflict update", plan.namespace)))
}

/// 把一条行写入与它的索引命令放进同一个提交边界。
///
/// 自动提交必须走一次 `commit_batch`：逐条 `execute` 会把一条语句拆成多个 durable
/// 提交点，崩溃后可能留下「行在、索引条目不在」的状态。OCC 条件不成立时返回
/// `Ok(false)`，由调用方决定跳过、重读还是按冲突上报；事务内的索引写入进 handle
/// 缓冲，随 ROLLBACK / savepoint 一起丢弃。
fn commit_row_write(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    command: KeyValueCommand,
    condition: CommitCondition,
    index_commands: &[KeyValueCommand],
    operation: &str,
) -> Result<bool, SqlFrontendError> {
    match transaction {
        Some(handle) => {
            let written = match &condition {
                CommitCondition::PutIfAbsent => {
                    match handle.execute(command).map_err(|e| write_error(format!("TransactionHandle.{operation} failed: {e}"), &e))? {
                        database_query::cluster_kv::KeyValueResult::PutIfAbsent { revision } => revision.is_some(),
                        _ => true,
                    }
                }
                CommitCondition::PutIfPresent => {
                    match handle.execute(command).map_err(|e| write_error(format!("TransactionHandle.{operation} failed: {e}"), &e))? {
                        database_query::cluster_kv::KeyValueResult::PutIfPresent { revision } => revision.is_some(),
                        _ => true,
                    }
                }
                CommitCondition::ValueEquals(pre_image) => {
                    handle
                        .write_if_unchanged(command, pre_image.clone())
                        .map_err(|e| write_error(format!("TransactionHandle.{operation} failed: {e}"), &e))?;
                    true
                }
            };
            if written {
                apply_index_commands_in_transaction(handle, index_commands)?;
            }
            Ok(written)
        }
        // 没有索引段要提交时保持单命令路径：一条命令本身就是一次 durable 提交点。
        None if index_commands.is_empty() => {
            let result = executor.execute(None, command).map_err(|e| internal_error(&format!("KeyValueExecutor.{operation} failed: {e}")))?;
            Ok(result_is_write(result))
        }
        None => {
            // `commit_batch` 只重放 Put / Delete；条件语义由 `condition` 承担。
            let command = match command.action {
                KeyValueAction::PutIfAbsent(bytes) | KeyValueAction::PutIfPresent(bytes) => {
                    KeyValueCommand { namespace: command.namespace, key: command.key, action: KeyValueAction::Put(bytes) }
                }
                _ => command,
            };
            let mut batch = Vec::with_capacity(index_commands.len() + 1);
            batch.push(BufferedCommit { command, condition: Some(condition) });
            batch.extend(index_commands.iter().cloned().map(|command| BufferedCommit { command, condition: None }));
            match executor.commit_batch(None, batch) {
                Ok(()) => Ok(true),
                Err(database_types::cluster::Error::CasConflict { .. }) => Ok(false),
                Err(error) => Err(write_error(format!("KeyValueExecutor.{operation} failed: {error}"), &error)),
            }
        }
    }
}

/// 一条写入命令的实际结果：是否真的改了存储。
fn result_is_write(result: database_query::cluster_kv::KeyValueResult) -> bool {
    use database_query::cluster_kv::KeyValueResult;
    match result {
        KeyValueResult::Put { .. } => true,
        KeyValueResult::Delete { removed } => removed,
        KeyValueResult::PutIfAbsent { revision } | KeyValueResult::PutIfPresent { revision } => revision.is_some(),
        _ => false,
    }
}

pub(crate) fn read_row_bytes(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    namespace: &Namespace,
    rowid: &[u8],
) -> Result<Option<Vec<u8>>, SqlFrontendError> {
    let command = KeyValueCommand { namespace: namespace.clone(), key: rowid.to_vec(), action: KeyValueAction::Get };
    let results = match transaction {
        Some(handle) => {
            handle.execute_read_batch(&[command]).map_err(|e| internal_error(&format!("TransactionHandle.read_batch failed: {e}")))?
        }
        None => {
            executor.execute_read_batch(None, &[command]).map_err(|e| internal_error(&format!("KeyValueExecutor.read_batch failed: {e}")))?
        }
    };
    Ok(match results.into_iter().next() {
        Some(database_query::cluster_kv::KeyValueResult::Get(bytes)) => bytes,
        _ => None,
    })
}

/// 把 `RETURNING` 投影应用到真正落库的行上。没有 `RETURNING` 时写语句不返回结果集。
pub(crate) fn returning_result(
    returning: &Option<ReturningProjections>,
    field_types: &BTreeMap<u64, database_query::catalog_bind::SqlFieldType>,
    written: Vec<RowFieldValues>,
    affected_rows: u64,
    dialect: SqlDialect,
) -> Result<SqlResult, SqlFrontendError> {
    let Some(projections) = returning
    else {
        return Ok(SqlResult { columns: Vec::new(), rows: Vec::new(), affected_rows });
    };
    let columns = returning_column_labels(projections);
    let mut rows = Vec::with_capacity(written.len());
    for row in &written {
        let mut values = Vec::with_capacity(projections.len());
        for (expression, _) in projections {
            values.push(eval_expression(expression, row, field_types, dialect)?);
        }
        rows.push(values);
    }
    Ok(SqlResult { columns, rows, affected_rows })
}

pub(crate) fn returning_column_labels(projections: &ReturningProjections) -> Vec<String> {
    projections.iter().map(|(_, label)| label.clone()).collect()
}

pub(crate) fn execute_multi_insert(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    plans: &[WritePlan],
    dialect: SqlDialect,
) -> Result<SqlResult, SqlFrontendError> {
    // 两条以上写入且没有显式事务时，整条语句必须经执行器的 durable 原子提交边界一次发布；
    // 逐条 `execute` 会让崩溃停在语句中途、只落库一半的行。单行语句本身就是一条命令，
    // 保持既有路径。事务路径由 handle 缓冲、COMMIT 时走同一 `commit_batch` 边界，无需改写。
    if transaction.is_none() && plans.len() > 1 {
        return execute_multi_insert_atomic(executor, plans, dialect);
    }
    if let Some(handle) = transaction {
        // 事务里的逐行写入各自只看已落库快照，同语句后一行的唯一键冲突可能被前一行缓冲
        // 掩盖，所以先按整条语句折叠一次，再连同事务可见快照一起做唯一性复核。
        if plans.len() > 1 && plans.first().is_some_and(|plan| !plan.unique_constraints.is_empty()) {
            let first = plans.first().expect("plans checked above");
            let namespace = Namespace(first.namespace.clone());
            let needs_preimage = plans.iter().any(|plan| plan.conflict.is_some());
            let preimages = load_insert_preimages(executor, Some(handle), plans, &namespace, needs_preimage)?;
            let (order, folded) = fold_insert_plans(plans, &preimages, dialect)?;
            let stored = scan_rows(executor, Some(handle), &namespace, &[], None, None, 0, "unique_check")?;
            ensure_unique_rows(&first.unique_constraints, &order, &folded, &stored)?;
        }
    }
    let columns = plans.first().and_then(|plan| plan.returning.as_ref()).map(returning_column_labels).unwrap_or_default();
    let mut rows = Vec::new();
    let mut total_affected = 0u64;
    for plan in plans {
        let single = execute_insert(executor, transaction, plan, dialect)?;
        rows.extend(single.rows);
        total_affected = total_affected.saturating_add(single.affected_rows);
    }
    Ok(SqlResult { columns, rows, affected_rows: total_affected })
}

/// 批次里一行的折叠结果：最终行值、是否产生写入、以及针对基存 pre-image 的 OCC 条件。
pub(crate) struct BatchedRow {
    /// 折叠后的当前行值。`None` 表示该行在基存中不存在且尚未有赋值折叠进来。
    current: Option<RowFieldValues>,
    /// 是否把一次写入提交进批次（`DO NOTHING` 跳过行与谓词不成立的改写行都是 `false`）。
    applied: bool,
    /// 提交边界内复核的 pre-image 条件。
    condition: CommitCondition,
}

/// 非事务多行 INSERT 的原子发布：按语句顺序把每行折叠成最终值与 OCC 条件，整批交给
/// `KeyValueExecutor::commit_batch` 的单分片 durable 原子边界。
///
/// 冲突策略在提交前折叠：`DO NOTHING` 跳过基存已存在的行（语句内重复主键同样按跳过），
/// `DO UPDATE` 基于 pre-image 求值谓词与赋值（语句内重复主键顺序折叠到同一行，与逐行语义
/// 一致），无策略时语句内重复主键直接报唯一约束错误。预读与提交之间存在并发窗口，条件被
/// 并发提交打断时整批拒绝并用最新快照重排一次；无策略的并发主键冲突则直接上报唯一约束错误。
pub(crate) fn execute_multi_insert_atomic(
    executor: &dyn KeyValueExecutor,
    plans: &[WritePlan],
    dialect: SqlDialect,
) -> Result<SqlResult, SqlFrontendError> {
    let first = plans.first().expect("atomic multi-insert requires at least one plan");
    let namespace = Namespace(first.namespace.clone());
    let unique_conflict =
        || SqlFrontendError::new(SqlErrorKind::DuplicateKey, format!("duplicate key value violates unique constraint: {}", first.namespace));
    let needs_preimage = plans.iter().any(|plan| plan.conflict.is_some());
    let mut indexes = IndexMaintenance::open(executor, None, &first.namespace, &first.unique_constraints, &first.field_types)?;
    for attempt in 0..2 {
        // 冲突策略需要先看到基存 pre-image 才能决定跳过还是改写；无策略的行交给提交边界的
        // OCC 条件复核，不做多余预读。
        let preimages = load_insert_preimages(executor, None, plans, &namespace, needs_preimage)?;
        let (order, folded) = fold_insert_plans(plans, &preimages, dialect)?;

        // 基存快照同时服务唯一性复核与索引回填。
        let stored = if first.unique_constraints.is_empty() {
            Vec::new()
        }
        else {
            scan_rows(executor, None, &namespace, &[], None, None, 0, "unique_check")?
        };
        if !first.unique_constraints.is_empty() {
            // 基存快照 + 同语句已折叠行共同决定 `UNIQUE` 冲突，整批在提交前拦截。
            ensure_unique_rows(&first.unique_constraints, &order, &folded, &stored)?;
        }
        if indexes.needs_backfill() {
            indexes.backfill(&stored)?;
        }
        for rowid in &order {
            let entry = &folded[rowid];
            if !entry.applied {
                continue;
            }
            let current = entry.current.as_ref().expect("applied entry carries a row");
            enforce_row_guards(executor, None, &first.check_constraints, &first.foreign_keys, &first.field_types, current, dialect)?;
            let old_row = preimages.get(rowid).and_then(|bytes| bytes.as_ref()).map(|bytes| decode_row(bytes)).transpose()?;
            indexes.remember_replace(old_row.as_ref(), current, rowid);
        }
        let index_commands = indexes.take_commands();

        let mut batch: Vec<BufferedCommit> = order
            .iter()
            .filter_map(|rowid| {
                let entry = &folded[rowid];
                entry.applied.then(|| BufferedCommit {
                    command: KeyValueCommand {
                        namespace: namespace.clone(),
                        key: rowid.clone(),
                        action: KeyValueAction::Put(encode_row(entry.current.as_ref().expect("applied entry carries a row"))),
                    },
                    condition: Some(entry.condition.clone()),
                })
            })
            .collect();
        batch.extend(index_commands.into_iter().map(|command| BufferedCommit { command, condition: None }));
        match executor.commit_batch(None, batch) {
            Ok(()) => {
                let written: Vec<RowFieldValues> = order
                    .iter()
                    .filter_map(|rowid| {
                        let entry = &folded[rowid];
                        entry.applied.then(|| entry.current.clone().expect("applied entry carries a row"))
                    })
                    .collect();
                let affected = written.len() as u64;
                return returning_result(&first.returning, &first.field_types, written, affected, dialect);
            }
            Err(database_types::cluster::Error::CasConflict { .. }) if needs_preimage && attempt == 0 => continue,
            Err(database_types::cluster::Error::CasConflict { .. }) if needs_preimage => {
                return Err(SqlFrontendError::new(
                    SqlErrorKind::Conflict,
                    format!("concurrent writes to {} prevented the batch commit", first.namespace),
                ));
            }
            Err(database_types::cluster::Error::CasConflict { .. }) => return Err(unique_conflict()),
            Err(error) => {
                return Err(internal_error(&format!("KeyValueExecutor.commit_batch failed: {error}")));
            }
        }
    }
    Err(SqlFrontendError::new(SqlErrorKind::Conflict, format!("concurrent writes to {} prevented the batch commit", first.namespace)))
}

/// Reads the base rows a conflict policy needs. Transaction reads go through the
/// handle so rows buffered earlier in the same transaction count as existing.
fn load_insert_preimages(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    plans: &[WritePlan],
    namespace: &Namespace,
    needed: bool,
) -> Result<BTreeMap<Vec<u8>, Option<Vec<u8>>>, SqlFrontendError> {
    let mut preimages = BTreeMap::new();
    if !needed {
        return Ok(preimages);
    }
    let mut rowids: Vec<Vec<u8>> = Vec::new();
    let mut seen = BTreeSet::new();
    for plan in plans {
        if seen.insert(plan.rowid.clone()) {
            rowids.push(plan.rowid.clone());
        }
    }
    let commands: Vec<KeyValueCommand> =
        rowids.iter().map(|rowid| KeyValueCommand { namespace: namespace.clone(), key: rowid.clone(), action: KeyValueAction::Get }).collect();
    let results = match transaction {
        Some(handle) => {
            handle.execute_read_batch(&commands).map_err(|e| internal_error(&format!("TransactionHandle.read_batch failed: {e}")))?
        }
        None => {
            executor.execute_read_batch(None, &commands).map_err(|e| internal_error(&format!("KeyValueExecutor.read_batch failed: {e}")))?
        }
    };
    for (rowid, result) in rowids.iter().zip(results) {
        let bytes = match result {
            database_query::cluster_kv::KeyValueResult::Get(bytes) => bytes,
            other => return Err(internal_error(&format!("unexpected storage result for pre-read: {other:?}"))),
        };
        preimages.insert(rowid.clone(), bytes);
    }
    Ok(preimages)
}

/// Folds every statement row into its final value and OCC condition, in first
/// appearance order. Shared by the atomic path and the transaction pre-check so
/// both reject the same in-statement duplicate primary keys.
fn fold_insert_plans(
    plans: &[WritePlan],
    preimages: &BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    dialect: SqlDialect,
) -> Result<(Vec<Vec<u8>>, BTreeMap<Vec<u8>, BatchedRow>), SqlFrontendError> {
    let namespace = plans.first().map(|plan| plan.namespace.clone()).unwrap_or_default();
    let mut order: Vec<Vec<u8>> = Vec::new();
    let mut folded: BTreeMap<Vec<u8>, BatchedRow> = BTreeMap::new();
    for plan in plans {
        match &plan.conflict {
            None => {
                if folded.contains_key(&plan.rowid) {
                    return Err(SqlFrontendError::new(
                        SqlErrorKind::DuplicateKey,
                        format!("duplicate key value violates unique constraint: {namespace}"),
                    ));
                }
                order.push(plan.rowid.clone());
                let mut current = write_plan_to_row(plan)?;
                apply_generated(&plan.generated, &plan.field_types, &mut current, dialect)?;
                folded.insert(plan.rowid.clone(), BatchedRow { current: Some(current), applied: true, condition: CommitCondition::PutIfAbsent });
            }
            Some(ConflictPolicy::DoNothing) => {
                if folded.contains_key(&plan.rowid) {
                    continue;
                }
                let existing = preimages.get(&plan.rowid).cloned().flatten();
                let (current, applied) = match &existing {
                    Some(bytes) => (Some(decode_row(bytes)?), false),
                    None => {
                        let mut row = write_plan_to_row(plan)?;
                        apply_generated(&plan.generated, &plan.field_types, &mut row, dialect)?;
                        (Some(row), true)
                    }
                };
                order.push(plan.rowid.clone());
                folded.insert(plan.rowid.clone(), BatchedRow { current, applied, condition: CommitCondition::PutIfAbsent });
            }
            Some(ConflictPolicy::DoUpdate { assignments, predicate }) => {
                if !folded.contains_key(&plan.rowid) {
                    let existing = preimages.get(&plan.rowid).cloned().flatten();
                    let condition = match &existing {
                        Some(bytes) => CommitCondition::ValueEquals(Some(bytes.clone())),
                        None => CommitCondition::PutIfAbsent,
                    };
                    let current = match &existing {
                        Some(bytes) => Some(decode_row(bytes)?),
                        None => None,
                    };
                    order.push(plan.rowid.clone());
                    folded.insert(plan.rowid.clone(), BatchedRow { current, applied: false, condition });
                }
                let entry = folded.get_mut(&plan.rowid).expect("entry inserted above");
                match &entry.current {
                    None => {
                        let mut row = write_plan_to_row(plan)?;
                        apply_generated(&plan.generated, &plan.field_types, &mut row, dialect)?;
                        entry.current = Some(row);
                        entry.applied = true;
                    }
                    Some(existing_row) => {
                        if let Some(updated) = conflict_updated_row(existing_row, plan, assignments, predicate.as_ref(), dialect)? {
                            entry.current = Some(updated);
                            entry.applied = true;
                        }
                    }
                }
            }
        }
    }
    Ok((order, folded))
}

/// Rejects `UNIQUE` conflicts among the folded statement rows and the stored
/// snapshot before anything is written.
fn ensure_unique_rows(
    constraints: &[UniqueConstraintPlan],
    order: &[Vec<u8>],
    folded: &BTreeMap<Vec<u8>, BatchedRow>,
    stored: &[(Vec<u8>, Vec<u8>)],
) -> Result<(), SqlFrontendError> {
    let mut snapshot = UniqueSnapshot::build(constraints, stored)?;
    for rowid in order {
        let entry = &folded[rowid];
        if !entry.applied {
            continue;
        }
        let row = entry.current.as_ref().expect("applied entry carries a row");
        if let Some(constraint) = snapshot.conflict(row, Some(rowid)) {
            return Err(duplicate_key_error(&constraint));
        }
        snapshot.remember(rowid, row);
    }
    Ok(())
}

/// 计算 `ON CONFLICT DO UPDATE` 命中既有行后的最终行值。
///
/// 谓词只对既有行求值，不成立时返回 `None` 表示跳过；赋值在既有行与 `EXCLUDED` 行合并后的
/// 作用域里求值（列带 `ROW_ID_STRIDE` 偏移），再统一应用，等价于 PostgreSQL 的「同时赋值」语义。
///
/// Single-row `upsert_row` and atomic `execute_multi_insert_atomic` share this function so
/// conflict semantics exist in one place.
pub(crate) fn conflict_updated_row(
    existing: &RowFieldValues,
    plan: &WritePlan,
    assignments: &[ConflictAssignment],
    predicate: Option<&SqlCatalogBoundExpression>,
    dialect: SqlDialect,
) -> Result<Option<RowFieldValues>, SqlFrontendError> {
    if let Some(predicate) = predicate {
        let value = eval_expression(predicate, existing, &plan.field_types, dialect)?;
        if !truthy(&value) {
            return Ok(None);
        }
    }
    let mut scope = existing.clone();
    let mut scope_types = plan.field_types.clone();
    for field in &plan.fields {
        let excluded_row_id = field.field_id + ROW_ID_STRIDE;
        if let Some(bytes) = sql_value_to_bytes(&field.value, &field.field_type)? {
            scope.insert(excluded_row_id, bytes);
        }
        scope_types.insert(excluded_row_id, field.field_type.clone());
    }
    let mut updates = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        let value = eval_expression(&assignment.value, &scope, &scope_types, dialect)?;
        let literal = value_to_literal(value);
        check_write_literal(&literal, &assignment.field_type, &assignment.field_name, dialect, assignment.capacity)?;
        updates.push((assignment.field_id, sql_value_to_bytes(&literal, &assignment.field_type)?));
    }
    let mut row = existing.clone();
    for (field_id, bytes) in updates {
        match bytes {
            Some(bytes) => row.insert(field_id, bytes),
            None => row.remove(field_id),
        }
    }
    // A stored generated column is recomputed from the updated row, exactly as
    // PostgreSQL does on every write that touches the row.
    apply_generated(&plan.generated, &plan.field_types, &mut row, dialect)?;
    Ok(Some(row))
}

/// 执行 `INSERT ... SELECT`：源查询走既有读路径，全部行通过宽度、类型与主键唯一性
/// 校验后才开始写入，保证语句级原子性。
pub(crate) fn execute_insert_select(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    plan: &InsertSelectPlan,
    dialect: SqlDialect,
) -> Result<SqlResult, SqlFrontendError> {
    let source = execute_plan(executor, transaction, &plan.source, dialect)?;
    let mut write_plans = Vec::with_capacity(source.rows.len());
    for row in source.rows {
        if row.len() != plan.fields.len() {
            return Err(SqlFrontendError::syntax(format!(
                "INSERT ... SELECT produced {} values for {} target columns",
                row.len(),
                plan.fields.len()
            )));
        }
        let literals: Vec<SqlBoundLiteral> = row.into_iter().map(value_to_literal).collect();
        // 主键可以不在第一列：按 field_id 定位目标列，再按该列声明类型编码 rowid。
        let pk_index = plan
            .fields
            .iter()
            .position(|field| field.field_id == plan.pk_field_id)
            .ok_or_else(|| SqlFrontendError::syntax("INSERT ... SELECT must provide the table primary key column"))?;
        let rowid = pk_literal_to_rowid(&literals[pk_index], &plan.fields[pk_index].field_type)?;
        let mut fields = Vec::with_capacity(plan.fields.len());
        for (column, value) in plan.fields.iter().zip(literals) {
            check_write_literal(&value, &column.field_type, &column.field_name, dialect, column.capacity)?;
            fields.push(WriteField { field_id: column.field_id, value, field_type: column.field_type.clone(), capacity: column.capacity });
        }
        write_plans.push(WritePlan {
            namespace: plan.namespace.clone(),
            rowid,
            fields,
            field_types: plan.field_types.clone(),
            conflict: plan.conflict.clone(),
            returning: plan.returning.clone(),
            unique_constraints: plan.unique_constraints.clone(),
            check_constraints: plan.check_constraints.clone(),
            foreign_keys: plan.foreign_keys.clone(),
            generated: Vec::new(),
        });
    }
    // 没有冲突策略时，源行之间与目标表内都不能出现重复主键或次要 `UNIQUE`，
    // 预检保证整条语句不发布；有策略时冲突交给逐行 `DO NOTHING` / `DO UPDATE`。
    if plan.conflict.is_none() {
        ensure_insert_targets_absent(executor, transaction, &write_plans)?;
        ensure_unique_targets_absent(executor, transaction, &write_plans)?;
    }
    // 源查询没有产出行时写语句仍有结果形状（`RETURNING` 列定义，0 行）。
    if write_plans.is_empty() {
        let columns = plan.returning.as_ref().map(returning_column_labels).unwrap_or_default();
        return Ok(SqlResult { columns, rows: Vec::new(), affected_rows: 0 });
    }
    execute_multi_insert(executor, transaction, &write_plans, dialect)
}

/// 写入前确认源行之间与目标表内都不存在重复主键，让唯一约束冲突整条语句不发布。
pub(crate) fn ensure_insert_targets_absent(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    plans: &[WritePlan],
) -> Result<(), SqlFrontendError> {
    let Some(first) = plans.first()
    else {
        return Ok(());
    };
    let conflict = |plan: &WritePlan| {
        SqlFrontendError::new(SqlErrorKind::DuplicateKey, format!("duplicate key value violates unique constraint: {}", plan.namespace))
    };
    let namespace = Namespace(first.namespace.clone());
    let mut seen = BTreeSet::new();
    let mut commands = Vec::with_capacity(plans.len());
    for plan in plans {
        if !seen.insert(plan.rowid.clone()) {
            return Err(conflict(plan));
        }
        commands.push(KeyValueCommand { namespace: namespace.clone(), key: plan.rowid.clone(), action: KeyValueAction::Get });
    }
    let results = match transaction {
        Some(handle) => {
            handle.execute_read_batch(&commands).map_err(|e| internal_error(&format!("TransactionHandle.read_batch failed: {e}")))?
        }
        None => {
            executor.execute_read_batch(None, &commands).map_err(|e| internal_error(&format!("KeyValueExecutor.read_batch failed: {e}")))?
        }
    };
    if results.iter().any(|result| matches!(result, database_query::cluster_kv::KeyValueResult::Get(Some(_)))) {
        return Err(conflict(first));
    }
    Ok(())
}

/// 写入前确认候选行之间与目标表内都不存在 `UNIQUE` 冲突。
pub(crate) fn ensure_unique_targets_absent(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    plans: &[WritePlan],
) -> Result<(), SqlFrontendError> {
    let Some(first) = plans.first()
    else {
        return Ok(());
    };
    if first.unique_constraints.is_empty() {
        return Ok(());
    }
    let namespace = Namespace(first.namespace.clone());
    let stored = scan_rows(executor, transaction, &namespace, &[], None, None, 0, "unique_check")?;
    let mut snapshot = UniqueSnapshot::build(&first.unique_constraints, &stored)?;
    for plan in plans {
        let row = write_plan_to_row(plan)?;
        if let Some(constraint) = snapshot.conflict(&row, Some(&plan.rowid)) {
            return Err(duplicate_key_error(&constraint));
        }
        snapshot.remember(&plan.rowid, &row);
    }
    Ok(())
}

pub(crate) fn execute_update(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    plan: &UpdatePlan,
    dialect: SqlDialect,
) -> Result<SqlResult, SqlFrontendError> {
    let namespace = Namespace(plan.namespace.clone());
    let get_cmd = KeyValueCommand { namespace: namespace.clone(), key: plan.rowid.clone(), action: KeyValueAction::Get };
    let batch_result = match transaction {
        Some(handle) => {
            handle.execute_read_batch(&[get_cmd.clone()]).map_err(|e| internal_error(&format!("TransactionHandle.read_batch failed: {e}")))?
        }
        None => {
            executor.execute_read_batch(None, &[get_cmd]).map_err(|e| internal_error(&format!("KeyValueExecutor.read_batch failed: {e}")))?
        }
    };
    let existing_bytes = match batch_result.into_iter().next() {
        Some(database_query::cluster_kv::KeyValueResult::Get(Some(b))) => b,
        _ => return Ok(SqlResult { columns: Vec::new(), rows: Vec::new(), affected_rows: 0 }),
    };
    // UPDATE 必须保留未赋值的列，因此这里解码完整 TLV；字段值仍按
    // `plan.field_types` 走类型化解释，不会退回无类型猜测。
    let mut row = decode_row(&existing_bytes)?;

    // 先评估 remaining_predicate —— 不通过就不写，影响 0 行。
    if let Some(ref predicate) = plan.remaining_predicate {
        let v = eval_expression(predicate, &row, &plan.field_types, dialect)?;
        if !truthy(&v) {
            return Ok(SqlResult { columns: Vec::new(), rows: Vec::new(), affected_rows: 0 });
        }
    }

    // 所有 SET 右值先基于改写前的行求值，再统一应用：`SET a = b, b = a` 必须交换，
    // 与 `ON CONFLICT DO UPDATE` 的同时赋值语义一致。
    let mut updates = Vec::with_capacity(plan.fields.len());
    for assignment in &plan.fields {
        let value = eval_row_expression(&assignment.value, &row, &plan.field_types, dialect, executor, transaction)?;
        let literal = value_to_literal(value);
        check_write_literal(&literal, &assignment.field_type, &assignment.field_name, dialect, assignment.capacity)?;
        updates.push((assignment.field_id, sql_value_to_bytes(&literal, &assignment.field_type)?));
    }
    for (field_id, bytes) in updates {
        match bytes {
            Some(bytes) => row.insert(field_id, bytes),
            None => row.remove(field_id),
        }
    }
    apply_generated(&plan.generated, &plan.field_types, &mut row, dialect)?;
    if let Some(constraint) =
        unique_conflict_on_write(executor, transaction, &plan.namespace, &plan.unique_constraints, &row, Some(&plan.rowid))?
    {
        return Err(duplicate_key_error(&constraint));
    }
    enforce_row_guards(executor, transaction, &plan.check_constraints, &plan.foreign_keys, &plan.field_types, &row, dialect)?;
    let mut indexes = IndexMaintenance::open(executor, transaction, &plan.namespace, &plan.unique_constraints, &plan.field_types)?;
    if indexes.needs_backfill() {
        let stored = index_snapshot(executor, transaction, &namespace)?;
        indexes.backfill(&stored)?;
    }
    let old_row = decode_row(&existing_bytes)?;
    indexes.remember_replace(Some(&old_row), &row, &plan.rowid);
    let index_commands = indexes.take_commands();
    let command = KeyValueCommand { namespace, key: plan.rowid.clone(), action: KeyValueAction::Put(encode_row(&row)) };
    // 事务内 UPDATE 是 read-modify-write：把读到的 pre-image 交给 OCC，
    // 另一个事务在此期间提交同一行时 COMMIT 会被拒绝，而不是静默覆盖。
    commit_row_write(executor, transaction, command, CommitCondition::ValueEquals(Some(existing_bytes)), &index_commands, "update")?;
    Ok(SqlResult { columns: Vec::new(), rows: Vec::new(), affected_rows: 1 })
}

pub(crate) fn execute_delete(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    plan: &DeletePlan,
    dialect: SqlDialect,
) -> Result<SqlResult, SqlFrontendError> {
    let namespace = Namespace(plan.namespace.clone());
    let mut indexes = IndexMaintenance::open(executor, transaction, &plan.namespace, &plan.unique_constraints, &plan.field_types)?;
    if indexes.needs_backfill() {
        let stored = index_snapshot(executor, transaction, &namespace)?;
        indexes.backfill(&stored)?;
    }

    // 事务内 DELETE 也要捕获 pre-image 供 OCC 校验；有 remaining_predicate 时还必须
    // 先 Get 拿到行评估，不通过就不删。索引条目要从被删的行上摘除，所以同样需要它。
    let pre_image = if transaction.is_some() || plan.remaining_predicate.is_some() || indexes.is_active() {
        let get_cmd = KeyValueCommand { namespace: namespace.clone(), key: plan.rowid.clone(), action: KeyValueAction::Get };
        let batch_result = read_batch(executor, transaction, &[get_cmd])?;
        let existing_bytes = match batch_result.into_iter().next() {
            Some(database_query::cluster_kv::KeyValueResult::Get(Some(b))) => b,
            // 行已不存在 — 也是 0 影响行
            _ => return Ok(SqlResult { columns: Vec::new(), rows: Vec::new(), affected_rows: 0 }),
        };
        if let Some(ref predicate) = plan.remaining_predicate {
            let needed_ids = expression_needed_field_ids(&[], &[], Some(predicate));
            let row = decode_row_fields(&existing_bytes, &needed_ids)?;
            let v = eval_expression(predicate, &row, &plan.field_types, dialect)?;
            if !truthy(&v) {
                return Ok(SqlResult { columns: Vec::new(), rows: Vec::new(), affected_rows: 0 });
            }
        }
        if indexes.is_active() {
            let row = decode_row(&existing_bytes)?;
            indexes.remember_remove(Some(&row), &plan.rowid);
        }
        Some(existing_bytes)
    }
    else {
        None
    };

    let index_commands = indexes.take_commands();
    let command = KeyValueCommand { namespace, key: plan.rowid.clone(), action: KeyValueAction::Delete };
    // 事务内 DELETE 同样交给 OCC：pre-image 已被并发事务改写时 COMMIT 被拒绝。
    let written = commit_row_write(executor, transaction, command, CommitCondition::ValueEquals(pre_image), &index_commands, "delete")?;
    Ok(SqlResult { columns: Vec::new(), rows: Vec::new(), affected_rows: u64::from(written) })
}

/// 一条语句扫描加谓词求值后命中的一行：最终写入命令与扫描时看到的 pre-image。
///
/// `pre_image` 会作为 `CommitCondition::ValueEquals(Some(pre_image))` 交给提交边界复核，
/// 因此读改写与删除共享同一套 OCC 语义，不随语句动词分叉。
pub(crate) struct StatementChange {
    /// 该行最终要提交的命令（`Put` 或 `Delete`）。
    command: KeyValueCommand,
    /// 扫描时读到的基存行值，提交时用作 OCC 条件。
    pre_image: Vec<u8>,
}

/// 把一条 DML 语句的命中行提交到 durable 原子边界。
///
/// 无显式事务时，整条语句必须经一次 `KeyValueExecutor::commit_batch(None, batch)` 发布：
/// 逐行 `execute` 会把同一条语句拆成多个 durable 提交点，中途存储失败或进程退出会留下
/// 前几行已可见、后几行未写的半提交状态。每行携带
/// `CommitCondition::ValueEquals(Some(pre_image))`，并发改写同一行时整批以 `CasConflict`
/// 拒绝并上报 `SqlErrorKind::Conflict`，不静默重试，也不退回逐行写。
///
/// 有显式事务时各行进入 handle 缓冲，由 COMMIT 时同一条 `commit_batch` 边界发布，
/// 这里不提前提交。空变更集不触碰提交边界，直接返回 0。
///
/// `index_commands` 是整条语句的索引维护段（回填 / 删除旧条目 / 写入新条目），与行写入
/// 同批发布；没有命中行时只有索引段时也要提交，否则回填永远落不了地。
pub(crate) fn apply_statement_changes(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    changes: Vec<StatementChange>,
    index_commands: Vec<KeyValueCommand>,
    namespace: &Namespace,
    operation: &str,
) -> Result<u64, SqlFrontendError> {
    if changes.is_empty() && index_commands.is_empty() {
        return Ok(0);
    }
    let affected = changes.len() as u64;
    match transaction {
        Some(handle) => {
            for change in changes {
                handle
                    .write_if_unchanged(change.command, Some(change.pre_image))
                    .map_err(|e| write_error(format!("TransactionHandle.{operation} failed: {e}"), &e))?;
            }
            apply_index_commands_in_transaction(handle, &index_commands)?;
        }
        None => {
            let mut batch: Vec<BufferedCommit> = changes
                .into_iter()
                .map(|change| BufferedCommit { command: change.command, condition: Some(CommitCondition::ValueEquals(Some(change.pre_image))) })
                .collect();
            batch.extend(index_commands.into_iter().map(|command| BufferedCommit { command, condition: None }));
            executor.commit_batch(None, batch).map_err(|error| match error {
                database_types::cluster::Error::CasConflict { .. } => SqlFrontendError::new(
                    SqlErrorKind::Conflict,
                    format!("concurrent writes to {} prevented the {operation} batch commit", namespace.0),
                ),
                other => internal_error(&format!("KeyValueExecutor.commit_batch failed: {other}")),
            })?;
        }
    }
    Ok(affected)
}

/// 多行 UPDATE：扫描 rowid 区间，逐行判定 WHERE 后改写，返回真实影响行数。
///
/// 命中行先累积成一条语句变更集，再统一走 [`apply_statement_changes`] 的提交边界；
/// 自动提交下只产生一次 durable commit，事务内则交给 handle 缓冲到 COMMIT。
pub(crate) fn execute_update_scan(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    plan: &UpdateScanPlan,
    dialect: SqlDialect,
) -> Result<SqlResult, SqlFrontendError> {
    let namespace = Namespace(plan.namespace.clone());
    let scanned = scan_rows(
        executor,
        transaction,
        &namespace,
        &plan.rowid_prefix,
        plan.rowid_after.as_deref(),
        plan.rowid_before.as_deref(),
        0,
        "update_scan",
    )?;
    let mut indexes = IndexMaintenance::open(executor, transaction, &plan.namespace, &plan.unique_constraints, &plan.field_types)?;
    if indexes.needs_backfill() {
        let stored = index_snapshot(executor, transaction, &namespace)?;
        indexes.backfill(&stored)?;
    }
    let mut changes = Vec::with_capacity(scanned.len());
    // 一条语句的写入要到 `apply_statement_changes` 才发布，循环里的每一行看到的
    // 基存快照完全相同。快照在第一次命中时读一次并建成约束元组的哈希索引，
    // 避免每个命中行都全表重扫、逐行解码；零命中语句则不付这份成本。
    let mut unique_snapshot: Option<UniqueSnapshot<'_>> = None;
    let predicate_ids = plan.predicate.as_ref().map(|predicate| expression_needed_field_ids(&[], &[], Some(predicate)));
    for (rowid, pre_image) in scanned {
        // 选择性 UPDATE 先用谓词列判定命中，命中后才解码整行：改写需要保留未赋值的
        // 列，但未命中的行不值得付整行解码的成本。
        if let (Some(predicate), Some(ids)) = (&plan.predicate, &predicate_ids) {
            let candidate = decode_row_fields(&pre_image, ids)?;
            let value = eval_expression(predicate, &candidate, &plan.field_types, dialect)?;
            if !truthy(&value) {
                continue;
            }
        }
        let mut row = decode_row(&pre_image)?;
        // SET 右值基于改写前的行求值，全部求完再统一应用（同时赋值语义）。
        let mut updates = Vec::with_capacity(plan.fields.len());
        for assignment in &plan.fields {
            let value = eval_row_expression(&assignment.value, &row, &plan.field_types, dialect, executor, transaction)?;
            let literal = value_to_literal(value);
            check_write_literal(&literal, &assignment.field_type, &assignment.field_name, dialect, assignment.capacity)?;
            updates.push((assignment.field_id, sql_value_to_bytes(&literal, &assignment.field_type)?));
        }
        let old_row = row.clone();
        for (field_id, bytes) in updates {
            match bytes {
                Some(bytes) => row.insert(field_id, bytes),
                None => row.remove(field_id),
            }
        }
        apply_generated(&plan.generated, &plan.field_types, &mut row, dialect)?;
        if !plan.unique_constraints.is_empty() {
            if unique_snapshot.is_none() {
                let stored = scan_rows(executor, transaction, &namespace, &[], None, None, 0, "unique_check")?;
                unique_snapshot = Some(UniqueSnapshot::build(&plan.unique_constraints, &stored)?);
            }
            let snapshot = unique_snapshot.as_mut().expect("snapshot was built above");
            if let Some(constraint) = snapshot.conflict(&row, Some(&rowid)) {
                return Err(duplicate_key_error(&constraint));
            }
            snapshot.remember(&rowid, &row);
        }
        enforce_row_guards(executor, transaction, &plan.check_constraints, &plan.foreign_keys, &plan.field_types, &row, dialect)?;
        indexes.remember_replace(Some(&old_row), &row, &rowid);
        changes.push(StatementChange {
            command: KeyValueCommand { namespace: namespace.clone(), key: rowid, action: KeyValueAction::Put(encode_row(&row)) },
            pre_image,
        });
    }
    let index_commands = indexes.take_commands();
    let affected = apply_statement_changes(executor, transaction, changes, index_commands, &namespace, "update_scan")?;
    Ok(SqlResult { columns: Vec::new(), rows: Vec::new(), affected_rows: affected })
}

/// 多行 DELETE：扫描 rowid 区间，逐行判定 WHERE 后删除，返回真实影响行数。
///
/// 与多行 UPDATE 共用同一条语句变更集与提交边界，删除命中行在一次 durable commit 内发布。
pub(crate) fn execute_delete_scan(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    plan: &DeleteScanPlan,
    dialect: SqlDialect,
) -> Result<SqlResult, SqlFrontendError> {
    let namespace = Namespace(plan.namespace.clone());
    let scanned = scan_rows(
        executor,
        transaction,
        &namespace,
        &plan.rowid_prefix,
        plan.rowid_after.as_deref(),
        plan.rowid_before.as_deref(),
        0,
        "delete_scan",
    )?;
    let mut indexes = IndexMaintenance::open(executor, transaction, &plan.namespace, &plan.unique_constraints, &plan.field_types)?;
    if indexes.needs_backfill() {
        let stored = index_snapshot(executor, transaction, &namespace)?;
        indexes.backfill(&stored)?;
    }
    let mut changes = Vec::with_capacity(scanned.len());
    let predicate_ids = plan.predicate.as_ref().map(|predicate| expression_needed_field_ids(&[], &[], Some(predicate)));
    for (rowid, pre_image) in scanned {
        if let (Some(predicate), Some(ids)) = (&plan.predicate, &predicate_ids) {
            let row = decode_row_fields(&pre_image, ids)?;
            let value = eval_expression(predicate, &row, &plan.field_types, dialect)?;
            if !truthy(&value) {
                continue;
            }
        }
        if indexes.is_active() {
            let row = decode_row(&pre_image)?;
            indexes.remember_remove(Some(&row), &rowid);
        }
        changes.push(StatementChange {
            command: KeyValueCommand { namespace: namespace.clone(), key: rowid, action: KeyValueAction::Delete },
            pre_image,
        });
    }
    let index_commands = indexes.take_commands();
    let affected = apply_statement_changes(executor, transaction, changes, index_commands, &namespace, "delete_scan")?;
    Ok(SqlResult { columns: Vec::new(), rows: Vec::new(), affected_rows: affected })
}

pub(crate) fn write_plan_to_row(plan: &WritePlan) -> Result<RowFieldValues, SqlFrontendError> {
    let mut row = RowFieldValues::new();
    for field in &plan.fields {
        // NULL 不写入 TLV：字段缺失就是 NULL，空 bytes 留给真正的空串。
        if let Some(bytes) = sql_value_to_bytes(&field.value, &field.field_type)? {
            row.insert(field.field_id, bytes);
        }
    }
    Ok(row)
}

/// 把一行的约束列折叠成可比较的元组；任一列为 NULL 时返回 `None`（SQL 的
/// `UNIQUE` 允许多行 NULL）。
pub(crate) fn constraint_tuple(row: &RowFieldValues, columns: &[(u64, String)]) -> Option<Vec<Vec<u8>>> {
    let mut tuple = Vec::with_capacity(columns.len());
    for (field_id, _) in columns {
        tuple.push(row.get(*field_id)?.to_vec());
    }
    Some(tuple)
}

/// 候选行与既有行构成的 `UNIQUE` 冲突描述。
pub(crate) fn unique_conflict_between(
    constraints: &[UniqueConstraintPlan],
    candidate: &RowFieldValues,
    existing: &RowFieldValues,
) -> Option<String> {
    constraints.iter().filter(|constraint| constraint.unique).find_map(|constraint| {
        let left = constraint_tuple(candidate, &constraint.columns)?;
        let right = constraint_tuple(existing, &constraint.columns)?;
        (left == right).then(|| {
            let names: Vec<&str> = constraint.columns.iter().map(|(_, name)| name.as_str()).collect();
            format!("UNIQUE ({})", names.join(", "))
        })
    })
}

/// 一条语句内共享的 `UNIQUE` 快照索引。
///
/// 多行 UPDATE / INSERT 的每一行都要与「基存快照 + 本语句已处理行」比对。
/// 逐行重扫并把每一行与候选行两两比较是 O(行数 × 命中数)；这里把约束元组
/// 序列化成哈希 key，构建一次后每行只查自己的元组。
struct UniqueSnapshot<'a> {
    constraints: Vec<&'a UniqueConstraintPlan>,
    buckets: Vec<std::collections::HashMap<Vec<u8>, Vec<Vec<u8>>>>,
}

impl<'a> UniqueSnapshot<'a> {
    fn build(constraints: &'a [UniqueConstraintPlan], stored: &[(Vec<u8>, Vec<u8>)]) -> Result<Self, SqlFrontendError> {
        let ids = constraint_field_ids(constraints);
        // 普通二级索引只维护条目，不参与唯一性判定。
        let constraints: Vec<&UniqueConstraintPlan> = constraints.iter().filter(|constraint| constraint.unique).collect();
        let mut snapshot = Self { buckets: constraints.iter().map(|_| std::collections::HashMap::new()).collect(), constraints };
        if snapshot.constraints.is_empty() {
            return Ok(snapshot);
        }
        for (rowid, bytes) in stored {
            let row = decode_row_fields(bytes, &ids)?;
            snapshot.remember(rowid, &row);
        }
        Ok(snapshot)
    }

    /// 返回与候选行冲突的约束描述；`excluded_rowid` 用于排除正在改写的那一行。
    fn conflict(&self, candidate: &RowFieldValues, excluded_rowid: Option<&[u8]>) -> Option<String> {
        for (constraint, bucket) in self.constraints.iter().zip(&self.buckets) {
            let Some(tuple) = constraint_tuple(candidate, &constraint.columns)
            else {
                continue;
            };
            let Some(rowids) = bucket.get(&constraint_key(&tuple))
            else {
                continue;
            };
            if rowids.iter().any(|rowid| excluded_rowid.is_none_or(|excluded| rowid.as_slice() != excluded)) {
                let names: Vec<&str> = constraint.columns.iter().map(|(_, name)| name.as_str()).collect();
                return Some(format!("UNIQUE ({})", names.join(", ")));
            }
        }
        None
    }

    /// 把一条已经通过校验的行加入索引，供同语句后续行查重。
    fn remember(&mut self, rowid: &[u8], row: &RowFieldValues) {
        for (constraint, bucket) in self.constraints.iter().zip(&mut self.buckets) {
            if let Some(tuple) = constraint_tuple(row, &constraint.columns) {
                bucket.entry(constraint_key(&tuple)).or_default().push(rowid.to_vec());
            }
        }
    }
}

/// 把约束元组编码成无歧义的哈希 key（每列前置长度）。
fn constraint_key(tuple: &[Vec<u8>]) -> Vec<u8> {
    let mut key = Vec::new();
    for value in tuple {
        key.extend_from_slice(&(value.len() as u32).to_be_bytes());
        key.extend_from_slice(value);
    }
    key
}

/// 在基存快照与同语句已产出的行中查找 `UNIQUE` 冲突。
///
/// `excluded_rowid` 排除当前行自身（UPDATE 时它仍然存在于旧快照里）。
/// `pending` 是同一条语句里已经通过校验的最终行，用来发现语句内重复。
pub(crate) fn unique_conflict_in_snapshot(
    constraints: &[UniqueConstraintPlan],
    candidate: &RowFieldValues,
    excluded_rowid: Option<&[u8]>,
    pending: &[(Vec<u8>, RowFieldValues)],
    stored: &[(Vec<u8>, Vec<u8>)],
) -> Result<Option<String>, SqlFrontendError> {
    for (rowid, row) in pending {
        if excluded_rowid.is_some_and(|excluded| excluded == rowid.as_slice()) {
            continue;
        }
        if let Some(conflict) = unique_conflict_between(constraints, candidate, row) {
            return Ok(Some(conflict));
        }
    }
    for (rowid, bytes) in stored {
        if excluded_rowid.is_some_and(|excluded| excluded == rowid.as_slice()) {
            continue;
        }
        // 冲突只取决于约束列；整行解码会把与约束无关的列也复制一遍。
        let row = decode_row_fields(bytes, &constraint_field_ids(constraints))?;
        if let Some(conflict) = unique_conflict_between(constraints, candidate, &row) {
            return Ok(Some(conflict));
        }
    }
    Ok(None)
}

/// 一组唯一约束引用到的全部字段 ID，去重后供局部行解码使用。
fn constraint_field_ids(constraints: &[UniqueConstraintPlan]) -> Vec<u64> {
    let mut ids = Vec::new();
    for constraint in constraints {
        for (field_id, _) in &constraint.columns {
            if !ids.contains(field_id) {
                ids.push(*field_id);
            }
        }
    }
    ids
}

/// 在基存快照里找第一条与 `candidate` 冲突次要 `UNIQUE` 的行。
///
/// 返回值是 `(rowid, row bytes)`，供 `ON DUPLICATE KEY UPDATE` 把改写落到
/// 实际冲突的那一行；没有约束或没有冲突时返回 `None`。
fn find_unique_conflict(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    namespace: &str,
    constraints: &[UniqueConstraintPlan],
    candidate: &RowFieldValues,
) -> Result<Option<(Vec<u8>, Vec<u8>)>, SqlFrontendError> {
    if constraints.is_empty() {
        return Ok(None);
    }
    let namespace = Namespace(namespace.to_string());
    let stored = scan_rows(executor, transaction, &namespace, &[], None, None, 0, "unique_check")?;
    let ids = constraint_field_ids(constraints);
    for (rowid, bytes) in stored {
        let row = decode_row_fields(&bytes, &ids)?;
        if unique_conflict_between(constraints, candidate, &row).is_some() {
            return Ok(Some((rowid, bytes)));
        }
    }
    Ok(None)
}

/// 扫描整张表后校验候选行，供单行 INSERT / UPDATE 使用。
pub(crate) fn unique_conflict_on_write(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    namespace: &str,
    constraints: &[UniqueConstraintPlan],
    candidate: &RowFieldValues,
    excluded_rowid: Option<&[u8]>,
) -> Result<Option<String>, SqlFrontendError> {
    if constraints.is_empty() {
        return Ok(None);
    }
    let namespace = Namespace(namespace.to_string());
    let stored = scan_rows(executor, transaction, &namespace, &[], None, None, 0, "unique_check")?;
    unique_conflict_in_snapshot(constraints, candidate, excluded_rowid, &[], &stored)
}

/// 对最终行求值存储生成列，覆盖客户端不可能提供的值。
///
/// 在唯一性复核、`CHECK`/`FOREIGN KEY` 校验与落库之前调用，因此约束看到的是
/// 生成列的最终值，与 PostgreSQL 的「先算生成列、再校验」顺序一致。
pub(crate) fn apply_generated(
    generated: &[GeneratedColumnPlan],
    field_types: &BTreeMap<u64, database_query::catalog_bind::SqlFieldType>,
    row: &mut RowFieldValues,
    dialect: SqlDialect,
) -> Result<(), SqlFrontendError> {
    for column in generated {
        let value = eval_expression(&column.expression, row, field_types, dialect)?;
        let literal = value_to_literal(value);
        check_write_literal(&literal, &column.field_type, &column.name, dialect, column.capacity)?;
        match sql_value_to_bytes(&literal, &column.field_type)? {
            Some(bytes) => {
                row.insert(column.field_id, bytes);
            }
            None => {
                row.remove(column.field_id);
            }
        }
    }
    Ok(())
}

/// `ALTER TABLE ... ADD COLUMN ... GENERATED ... STORED` 回填：为既有行计算新列。
///
/// 行与它的索引条目仍走同一条语句变更集，因此在一次 durable commit 内发布：
/// 中途失败不会留下「列已注册、旧行没有生成值」的半状态。
pub(crate) fn backfill_generated_column(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    namespace: &str,
    column: &GeneratedColumnPlan,
    field_types: &BTreeMap<u64, database_query::catalog_bind::SqlFieldType>,
    dialect: SqlDialect,
) -> Result<u64, SqlFrontendError> {
    let namespace = Namespace(namespace.to_string());
    let scanned = scan_rows(executor, transaction, &namespace, &[], None, None, 0, "generated_backfill")?;
    let mut changes = Vec::with_capacity(scanned.len());
    for (rowid, pre_image) in scanned {
        let mut row = decode_row(&pre_image)?;
        apply_generated(std::slice::from_ref(column), field_types, &mut row, dialect)?;
        changes.push(StatementChange {
            command: KeyValueCommand { namespace: namespace.clone(), key: rowid, action: KeyValueAction::Put(encode_row(&row)) },
            pre_image,
        });
    }
    apply_statement_changes(executor, transaction, changes, Vec::new(), &namespace, "generated_backfill")
}

/// `CHECK` 与 `FOREIGN KEY` 的最终行值门禁：写命令进入提交边界前逐条复核。
///
/// `CHECK` 遵循 SQL 三值逻辑，只有 FALSE 违反，`NULL` 通过（与 PostgreSQL 一致）。
/// `FOREIGN KEY` 列缺失或为 `NULL` 视为满足；非空值按父表主键 rowid 在父表
/// namespace 点查，未命中即孤儿引用，按 `ForeignKeyViolation` 上报。
pub(crate) fn enforce_row_guards(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    check_constraints: &[CheckConstraintPlan],
    foreign_keys: &[ForeignKeyPlan],
    field_types: &BTreeMap<u64, database_query::catalog_bind::SqlFieldType>,
    row: &RowFieldValues,
    dialect: SqlDialect,
) -> Result<(), SqlFrontendError> {
    for check in check_constraints {
        let value = eval_expression(&check.expression, row, field_types, dialect)?;
        if sql_truth(&value) == Some(false) {
            return Err(SqlFrontendError::new(SqlErrorKind::CheckViolation, format!("new row violates check constraint: {}", check.text)));
        }
    }
    for key in foreign_keys {
        let Some(value) = row.value_typed(key.column, field_types)
        else {
            continue;
        };
        if matches!(value, SqlValue::Null) {
            continue;
        }
        let rowid = pk_literal_to_rowid(&value_to_literal(value), &key.parent_field_type)?;
        if read_row_bytes(executor, transaction, &Namespace(key.namespace.clone()), &rowid)?.is_none() {
            return Err(SqlFrontendError::new(
                SqlErrorKind::ForeignKeyViolation,
                format!("foreign key constraint references no row in table: {}", key.parent_name),
            ));
        }
    }
    Ok(())
}

/// `UNIQUE` 冲突统一按 `DuplicateKey` 上报，由方言层映射成 `23505` / `1062`。
pub(crate) fn duplicate_key_error(constraint: &str) -> SqlFrontendError {
    SqlFrontendError::new(SqlErrorKind::DuplicateKey, format!("duplicate key value violates unique constraint {constraint}"))
}

pub(crate) fn sql_value_to_bytes(
    lit: &SqlBoundLiteral,
    field_type: &database_query::catalog_bind::SqlFieldType,
) -> Result<Option<Vec<u8>>, SqlFrontendError> {
    // 存储形态由列类型决定：uuid 文本转成 16 字节原文，其余类型保留原有字节。
    field_type.storage_bytes_of(lit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use database_query::catalog_bind::SqlFieldType;

    fn insert_plan(rowid: &[u8], email: &str, constraints: &[UniqueConstraintPlan]) -> WritePlan {
        WritePlan {
            namespace: "table:users".to_string(),
            rowid: rowid.to_vec(),
            fields: vec![WriteField {
                field_id: 1,
                value: SqlBoundLiteral::String(email.to_string()),
                field_type: SqlFieldType::String,
                capacity: None,
            }],
            field_types: BTreeMap::from([(1, SqlFieldType::String)]),
            conflict: None,
            returning: None,
            unique_constraints: constraints.to_vec(),
            check_constraints: Vec::new(),
            foreign_keys: Vec::new(),
            generated: Vec::new(),
        }
    }

    #[test]
    fn later_statement_row_duplicating_a_unique_value_is_rejected() {
        let constraints = vec![UniqueConstraintPlan { columns: vec![(1, "email".to_string())], unique: true }];
        let plans = vec![insert_plan(b"first", "same@example.com", &constraints), insert_plan(b"second", "same@example.com", &constraints)];
        let (order, folded) = fold_insert_plans(&plans, &BTreeMap::new(), SqlDialect::Postgres).expect("fold succeeds");
        let error = ensure_unique_rows(&constraints, &order, &folded, &[]).expect_err("the later row must clash");
        assert_eq!(error.kind, SqlErrorKind::DuplicateKey);
    }

    #[test]
    fn distinct_unique_values_pass_the_statement_check() {
        let constraints = vec![UniqueConstraintPlan { columns: vec![(1, "email".to_string())], unique: true }];
        let plans = vec![insert_plan(b"first", "a@example.com", &constraints), insert_plan(b"second", "b@example.com", &constraints)];
        let (order, folded) = fold_insert_plans(&plans, &BTreeMap::new(), SqlDialect::Postgres).expect("fold succeeds");
        ensure_unique_rows(&constraints, &order, &folded, &[]).expect("distinct values must pass");
    }

    #[test]
    fn repeated_primary_key_without_a_conflict_policy_is_rejected() {
        let constraints: Vec<UniqueConstraintPlan> = Vec::new();
        let plans = vec![insert_plan(b"same", "a@example.com", &constraints), insert_plan(b"same", "b@example.com", &constraints)];
        let error = match fold_insert_plans(&plans, &BTreeMap::new(), SqlDialect::Postgres) {
            Err(error) => error,
            Ok(_) => panic!("repeated rowid must clash"),
        };
        assert_eq!(error.kind, SqlErrorKind::DuplicateKey);
    }
}
