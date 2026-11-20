//! `WITH RECURSIVE` 不动点物化。
//!
//! 递归 CTE 的结果不住在 KV 存储里：执行层在当前线程维护一份「工作集」，
//! 每轮把上一轮新增的增量行按合成关系的字段编码物化进去，step 计划对
//! `cte:{type_id}` 命名空间的扫描由 [`working_set_scan`] 直接在这份内存行上
//! 完成，不触碰执行器。不动点收敛后，全量结果替换进工作集，consumer 计划
//! 照常执行；无论成功还是失败，工作集都随守卫的 `Drop` 移除，语句之间不残留。
//!
//! 轮次语义与原生引擎一致：step 只对「本轮新增」求值（不是全量累积），
//! `UNION` 按整行去重（`NULL` 与 `NULL` 视为相同），`UNION ALL` 全收，
//! 超过 [`MAX_RECURSIVE_ROUNDS`] 轮仍未收敛即报错。

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use database_query::{
    catalog_bind::{SqlCatalogField, synthetic_namespace},
    sql::{SqlDialect, SqlFrontendError},
    sql_execute::{RowFieldValues, SqlResult, SqlValue, encode_row},
    sql_planner::RecursiveCtePlan,
};

use crate::cluster::{KeyValueExecutor, TransactionHandle};

use super::{execute_plan, internal_error, serialize_group_key, sql_value_to_bytes, value_to_literal};

/// 不动点轮次上限，与原生引擎同阈值同文案：超限说明 step 产生了单调增长的行集。
const MAX_RECURSIVE_ROUNDS: usize = 10_000;

thread_local! {
    /// 当前执行线程上活跃的递归 CTE 工作集：namespace →（行键，编码行）按插入顺序。
    ///
    /// 行键是 8 字节大端序号，字节序即插入序，KV 扫描的「按 key 字节序返回」语义
    /// 在内存里原样保持，`ORDER BY` 之前的行序与真实表扫描一致。
    static WORKING_SETS: RefCell<BTreeMap<String, Vec<(Vec<u8>, Vec<u8>)>>> = const { RefCell::new(BTreeMap::new()) };
}

/// 工作集扫描拦截：namespace 属于某个活跃递归 CTE 时，在内存行上完成
/// 前缀 / 游标 / limit 过滤并返回；否则返回 `None`，调用方照常走 KV。
pub(crate) fn working_set_scan(
    namespace: &str,
    prefix: &[u8],
    after: Option<&[u8]>,
    before: Option<&[u8]>,
    limit: usize,
) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
    WORKING_SETS.with(|sets| {
        let rows = sets.borrow().get(namespace).cloned()?;
        let mut scanned = Vec::new();
        for (rowid, value) in rows {
            if !prefix.is_empty() && !rowid.starts_with(prefix) {
                continue;
            }
            if after.is_some_and(|cursor| rowid.as_slice() <= cursor) {
                continue;
            }
            if before.is_some_and(|cursor| rowid.as_slice() >= cursor) {
                break;
            }
            scanned.push((rowid, value));
            if limit > 0 && scanned.len() >= limit {
                break;
            }
        }
        Some(scanned)
    })
}

/// 一个合成 namespace 的生命周期守卫：安装时接管该键，`Drop` 时移除。
///
/// 嵌套递归 CTE 各自持有不同合成 id 的守卫；外层守卫在内层 consumer 全部
/// 结束后才释放，因此内层 seed/step 能扫到外层已收敛的全量结果。
struct WorkingSetGuard {
    namespace: String,
}

impl WorkingSetGuard {
    fn install(namespace: String) -> Self {
        WORKING_SETS.with(|sets| sets.borrow_mut().insert(namespace.clone(), Vec::new()));
        Self { namespace }
    }

    fn replace(&self, rows: Vec<(Vec<u8>, Vec<u8>)>) {
        WORKING_SETS.with(|sets| sets.borrow_mut().insert(self.namespace.clone(), rows));
    }
}

impl Drop for WorkingSetGuard {
    fn drop(&mut self) {
        WORKING_SETS.with(|sets| {
            sets.borrow_mut().remove(&self.namespace);
        });
    }
}

/// 执行一个不动点计划：seed 起步，增量喂 step 直到不再产生新行，全量交给 consumer。
pub(crate) fn execute_recursive_cte(
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    plan: &RecursiveCtePlan,
    dialect: SqlDialect,
) -> Result<SqlResult, SqlFrontendError> {
    let namespace = synthetic_namespace(plan.type_id);
    let mut seq: u64 = 0;
    let mut accepted: BTreeSet<String> = BTreeSet::new();
    let mut all: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut delta: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

    let seed = execute_plan(executor, transaction, &plan.seed, dialect)?;
    for row in seed.rows {
        absorb_row(&plan.fields, row, plan.union_all, &mut accepted, &mut seq, &mut all, &mut delta)?;
    }

    let guard = WorkingSetGuard::install(namespace);
    let mut rounds: usize = 0;
    while !delta.is_empty() {
        rounds += 1;
        if rounds > MAX_RECURSIVE_ROUNDS {
            return Err(SqlFrontendError::unsupported(format!(
                "recursive CTE `{}` exceeded {MAX_RECURSIVE_ROUNDS} rounds without reaching a fixpoint",
                plan.name
            )));
        }
        guard.replace(std::mem::take(&mut delta));
        let produced = execute_plan(executor, transaction, &plan.step, dialect)?;
        let mut next: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for row in produced.rows {
            absorb_row(&plan.fields, row, plan.union_all, &mut accepted, &mut seq, &mut all, &mut next)?;
        }
        delta = next;
    }

    guard.replace(std::mem::take(&mut all));
    execute_plan(executor, transaction, &plan.consumer, dialect)
}

/// 递归进不动点计划的三个子计划，物化其中的子查询值。
pub(crate) fn resolve_recursive_subqueries(
    plan: &mut RecursiveCtePlan,
    executor: &dyn KeyValueExecutor,
    transaction: Option<&dyn TransactionHandle>,
    dialect: SqlDialect,
) -> Result<(), SqlFrontendError> {
    super::resolve_subqueries(&mut plan.seed, executor, transaction, dialect)?;
    super::resolve_subqueries(&mut plan.step, executor, transaction, dialect)?;
    super::resolve_subqueries(&mut plan.consumer, executor, transaction, dialect)
}

/// 把一行结果折进全量集与本轮增量。
///
/// 去重键用整行 `SqlValue` 的规范序列化（`NULL` 与 `NULL` 相同），与原生引擎的
/// `UNION` 语义同口径；`UNION ALL` 跳过去重直接收下。编码行按合成字段的 row id
/// （等于 field id，槽位固定 0）写入 TLV，扫描侧按同一组 id 解码。
fn absorb_row(
    fields: &[SqlCatalogField],
    row: Vec<SqlValue>,
    union_all: bool,
    accepted: &mut BTreeSet<String>,
    seq: &mut u64,
    all: &mut Vec<(Vec<u8>, Vec<u8>)>,
    delta: &mut Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<(), SqlFrontendError> {
    if row.len() != fields.len() {
        return Err(internal_error(format!("recursive CTE produced {} values for {} columns", row.len(), fields.len())));
    }
    let key = serialize_group_key(&row);
    if !union_all && !accepted.insert(key) {
        return Ok(());
    }
    let mut values = RowFieldValues::new();
    for (field, value) in fields.iter().zip(row) {
        let literal = value_to_literal(value);
        if let Some(bytes) = sql_value_to_bytes(&literal, &field.field_type)? {
            values.insert(field.field_id, bytes);
        }
    }
    let rowid = {
        let number = *seq;
        *seq += 1;
        number.to_be_bytes().to_vec()
    };
    let encoded = encode_row(&values);
    all.push((rowid.clone(), encoded.clone()));
    delta.push((rowid, encoded));
    Ok(())
}
