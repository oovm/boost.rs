//! 索引探针与 rowid 下推:SQLite 方言读路径的候选集机制。
//!
//! 普通列索引的活动键集缓存、`EXISTS` 探针计划、rowid 等值/区间/`IN` 下推与
//! 写语句候选集都住在本模块,经 [`ExecContext`] 的方法挂载;缓存本体在
//! [`super::SqliteExecState`]。

use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use database_query::sqlite::{bind::*, catalog::*, frontend::*, value::*};
use database_storage::sqlite::{Affinity, Collation, affinity_of};

use super::{cells::*, select::*};

/// 行 rowid 区间下推的候选上限。
///
/// `WHERE <rowid> >= a AND <rowid> <= b` 直接按区间读 b-tree,但只有区间宽度在
/// 这个上限内才展开成候选 rowid;更宽的区间仍然是整表扫描,避免为一条语句
/// 造出与表同量级的候选向量。
const ROWID_RANGE_CANDIDATES: i64 = 4_096;

/// How one base-table scan is bounded before any row is decoded.
///
/// Every variant keeps the emitted row set and row order identical to the
/// unbounded read: the full predicate still runs downstream, and a bounded
/// read only narrows the candidate rows the scan materializes.
pub enum ScanHint {
    /// `WHERE <rowid alias> = <integer literal>`: one candidate rowid.
    Pin(i64),
    /// `WHERE <indexed column> = <literal>` covering every key column of a
    /// plain column index: the rowids the index probe found, ascending.
    Rowids(Vec<i64>),
    /// `ORDER BY <rowid order> LIMIT n` with no other row-reducing clause:
    /// only the first `n` rows in rowid order can reach the output.
    Head(usize),
    /// `<rowid> >= a AND/OR <rowid> <= b` 的单边或双边区间:按 rowid 区间读取,
    /// 只解码区间内的行。缺失的一侧不设界。
    Range {
        /// 区间下界(rowid,含);`None` 表示不设下界。
        lower: Option<i64>,
        /// 区间上界(rowid,含);`None` 表示不设上界。
        upper: Option<i64>,
    },
    /// `LIMIT 0`: no row can reach the output, so the base table is not read at
    /// all. Column names still come from the schema, exactly as the materialized
    /// path would produce them.
    Empty,
}

/// One inclusive/exclusive range bound on an indexed column.
pub struct IndexBound {
    /// Literal the stored key is compared against.
    pub cell: Cell,
    /// Whether a key equal to `cell` is inside the range.
    pub inclusive: bool,
}

/// Cached live key set of one plain column index, valid while `epoch` matches
/// the engine's write epoch.
pub struct IndexProbe {
    /// Write epoch the key set was read under.
    pub epoch: u64,
    /// Rowids sharing a canonical key, ascending.
    pub map: HashMap<Vec<ProbeKey>, Vec<i64>>,
    /// Decoded key values in index order, used by range probes that need the
    /// engine's own comparison instead of a hash lookup.
    pub entries: Vec<(Vec<Cell>, i64)>,
    /// `entries` 按首列排序规则非递减,范围探针因此可以二分定位。
    pub sorted: bool,
    /// 已从表中删除、但还没从 `entries` 里剔除的 rowid。
    ///
    /// 删除语句按行打补丁时只把 rowid 记到这里,避免每条删除都扫一遍整份
    /// 索引项;条目累积到一半时再一次性压缩。
    pub deleted: HashSet<i64>,
    /// A stored key the canonical form cannot reproduce; the probe is then
    /// never used and every query falls back to the full scan.
    pub poisoned: bool,
}

/// 一个相关 `EXISTS` 的索引探针计划。
///
/// 只接受「内层单表 + 一个内层列等于外层列的等值 + 其余只读内层列的合取」
/// 这一形状。计划按语句内 AST 地址缓存,逐行求值只剩一次索引探针和候选行
/// 解码,不再重跑内层 `SELECT` 的整套解析与物化流程。
pub struct ExistsProbePlan {
    /// 内层表名,读取时按目录重新定位。
    pub table: String,
    /// 内层表在 `tables` 中的位置。
    pub table_position: usize,
    /// 参与探针的普通索引名(小写)。
    pub index: String,
    /// 参与等值的内层列位置。
    pub slot: usize,
    /// 参与等值的外层(当前行)单元格位置。
    pub outer: usize,
    /// 索引探针使用的排序规则。
    pub probe_collation: Collation,
    /// 等值比较本身的排序规则;探针只保证候选超集,结果以它复核。
    pub compare_collation: Collation,
    /// 等值之外的其余合取,只读内层列。
    pub residual: Vec<ResolvedExpr>,
    /// 内层行形状,供残差求值。
    pub shape: ScopeShape,
}

/// Emits the Cartesian product, pruning a partial combination as soon as one
/// conjunct over the bound sources is false.
///
/// Sources are bound in `binding_order`; the merged row keeps `FROM` column
/// order throughout. A conjunct that contains a subquery cannot be attributed
/// to one source, so it waits for the complete row instead of pruning early.
/// Collects the `AND` conjuncts of the form `<column> = <literal>` (or the
/// mirrored shape) that name a column of one base table, as `(position, cell)`
/// pairs. A bound parameter counts as a literal once its value is known.
/// Anything outside a plain `AND` of `=` comparisons contributes nothing, so a
/// missed equality only costs the pushdown, never a row.
pub(crate) fn collect_index_equalities(expr: &Expr, def: &TableDef, label: &str, parameters: &[Cell], out: &mut Vec<(usize, Cell)>) {
    match expr {
        Expr::And(left, right) => {
            collect_index_equalities(left, def, label, parameters, out);
            collect_index_equalities(right, def, label, parameters, out);
        }
        Expr::Compare { left, op, right } if *op == CompareOp::Eq => {
            let (column, value) = match (left.as_ref(), right.as_ref()) {
                (Expr::Column(column), value) => (column, known_cell(value, parameters)),
                (value, Expr::Column(column)) => (column, known_cell(value, parameters)),
                _ => return,
            };
            if column.qualifier.as_deref().is_some_and(|qualifier| !qualifier.eq_ignore_ascii_case(label)) {
                return;
            }
            if let Some(cell) = value
                && let Some(position) = def.columns.iter().position(|entry| entry.name.eq_ignore_ascii_case(&column.name))
            {
                out.push((position, cell));
            }
        }
        _ => {}
    }
}

/// Collects the lower and upper bounds of an `AND` conjunction on one column.
///
/// Only a plain `AND` of `<column> <op> <literal>` (or the mirrored shape)
/// contributes, where a bound parameter counts as a literal once its value is
/// known; any other shape is ignored, so a missed bound only costs the
/// pushdown. When several bounds name the same side, the last one wins, which
/// stays a superset of the true result because the full predicate still runs.
pub(crate) fn collect_range_bounds(
    expr: &Expr,
    def: &TableDef,
    label: &str,
    position: usize,
    parameters: &[Cell],
    lower: &mut Option<IndexBound>,
    upper: &mut Option<IndexBound>,
) {
    match expr {
        Expr::And(left, right) => {
            collect_range_bounds(left, def, label, position, parameters, lower, upper);
            collect_range_bounds(right, def, label, position, parameters, lower, upper);
        }
        Expr::Compare { left, op, right } => {
            let (column, value, flipped) = match (left.as_ref(), right.as_ref()) {
                (Expr::Column(column), value) => (column, known_cell(value, parameters), false),
                (value, Expr::Column(column)) => (column, known_cell(value, parameters), true),
                _ => return,
            };
            let Some(cell) = value
            else {
                return;
            };
            if column.qualifier.as_deref().is_some_and(|qualifier| !qualifier.eq_ignore_ascii_case(label)) {
                return;
            }
            if def.columns.get(position).is_none_or(|entry| !entry.name.eq_ignore_ascii_case(&column.name)) {
                return;
            }
            let op = if flipped { flip_range_op(*op) } else { *op };
            let bound = IndexBound { cell, inclusive: matches!(op, CompareOp::GtEq | CompareOp::LtEq) };
            match op {
                CompareOp::Gt | CompareOp::GtEq => *lower = Some(bound),
                CompareOp::Lt | CompareOp::LtEq => *upper = Some(bound),
                _ => {}
            }
        }
        _ => {}
    }
}

/// Mirrors a range comparison written with the literal on the left.
pub(crate) fn flip_range_op(op: CompareOp) -> CompareOp {
    match op {
        CompareOp::Gt => CompareOp::Lt,
        CompareOp::GtEq => CompareOp::LtEq,
        CompareOp::Lt => CompareOp::Gt,
        CompareOp::LtEq => CompareOp::GtEq,
        other => other,
    }
}

/// Automatic indexes one table owns, in the writer's numbering order. Each
/// entry is (suffix number, keyed column positions, `PRAGMA` origin).
pub fn autoindex_definitions(def: &TableDef) -> Vec<(usize, Vec<usize>, &'static str)> {
    let mut number = 0usize;
    let mut definitions = Vec::new();
    for (position, column) in def.columns.iter().enumerate() {
        if (column.primary_key || column.unique) && def.rowid_alias != Some(position) {
            number += 1;
            definitions.push((number, vec![position], if column.primary_key { "pk" } else { "u" }));
        }
    }
    for constraint in &def.table_constraints {
        if constraint.primary && (def.rowid_alias.is_some() || def.without_rowid) {
            continue;
        }
        number += 1;
        definitions.push((number, constraint.columns.clone(), if constraint.primary { "pk" } else { "u" }));
    }
    definitions
}

impl<'e> ExecContext<'e> {
    /// Detects a `WHERE <rowid> = <integer literal>` conjunct that bounds a
    /// single base rowid table to one candidate row.
    ///
    /// Only the plain main-schema rowid-table branch of [`ExecContext::select_source`]
    /// honors the pin; every other source shape (join, derived table, view, temp
    /// table, `WITHOUT ROWID`, attached schema, non-integer literal, or an
    /// equality sitting under `OR` / `NOT`) yields `None` so the scan stays a
    /// full read. The predicate is still evaluated in full downstream, so a pin
    /// never changes the emitted row set or order — it only narrows the
    /// candidate rows the scan decodes.
    pub(crate) fn rowid_pin(&self, from: &[TableRef], filter: &Option<Expr>) -> Option<i64> {
        if from.len() != 1 {
            return None;
        }
        let table = &from[0];
        if table.query.is_some() {
            return None;
        }
        // An unqualified name reaches the bounded branch only when no temp
        // object shadows it; `temp.` never does. A view re-evaluates its query
        // and is not bounded here.
        let key = table.name.to_ascii_lowercase();
        let reaches_main = match table.schema.as_deref() {
            None => !self.catalog.temp_lookup.contains_key(&key) && !self.catalog.temp_views.contains_key(&key),
            Some(qualifier) => {
                let lower = qualifier.to_ascii_lowercase();
                lower == "main" || self.alias.as_deref().is_some_and(|alias| alias.eq_ignore_ascii_case(&lower))
            }
        };
        if !reaches_main || self.catalog.views.contains_key(&key) {
            return None;
        }
        let position = *self.catalog.table_lookup.get(&key)?;
        let def = &self.catalog.tables[position];
        if def.without_rowid {
            return None;
        }
        let alias_name = def.rowid_alias.map(|index| def.columns[index].name.clone());
        Self::rowid_eq_conjunct(filter.as_ref()?, alias_name.as_deref(), table.label(), self.parameters)
    }

    /// `WHERE <rowid> IN (<integers>)` on one bounded rowid table: the listed
    /// rowids, ascending and de-duplicated.
    ///
    /// Only an all-integer `IN` list on the rowid-alias (or `rowid` /
    /// `_rowid_` / `oid`) name qualifies; anything else keeps the full read.
    /// The predicate still runs downstream, so the candidate set only narrows
    /// what the scan decodes and never changes the emitted rows or their order.
    pub(crate) fn rowid_in_pin(&self, from: &[TableRef], filter: &Option<Expr>) -> Option<Vec<i64>> {
        let (def, table) = self.bounded_scan_target(from)?;
        let alias_name = def.rowid_alias.map(|index| def.columns[index].name.clone());
        Self::rowid_in_conjunct(filter.as_ref()?, alias_name.as_deref(), table.label(), self.parameters)
    }

    /// `WHERE <rowid> >= <a> AND <rowid> <= <b>`: the rowids inside the stated
    /// interval, ascending. Bounds are widened to inclusive and the interval is
    /// capped, so an unhelpful range keeps the full read instead of building a
    /// huge candidate vector.
    pub(crate) fn rowid_range_pin(&self, from: &[TableRef], filter: &Option<Expr>) -> Option<Vec<i64>> {
        let (def, table) = self.bounded_scan_target(from)?;
        let alias_name = def.rowid_alias.map(|index| def.columns[index].name.clone());
        Self::rowid_range_candidates(filter.as_ref()?, alias_name.as_deref(), table.label(), self.parameters)
    }

    /// Rowid candidates of a bounded `WHERE <rowid> >= <a> AND <rowid> <= <b>`
    /// conjunction. Only intervals that state both bounds qualify.
    pub fn rowid_range_candidates(expr: &Expr, alias_name: Option<&str>, label: &str, parameters: &[Cell]) -> Option<Vec<i64>> {
        let mut lower: Option<i64> = None;
        let mut upper: Option<i64> = None;
        Self::collect_rowid_range(expr, alias_name, label, parameters, &mut lower, &mut upper);
        let (start, end) = (lower?, upper?);
        // 宽度用 checked 差值判定:极端边界(i64::MIN..i64::MAX)不展开候选。
        let span = end.checked_sub(start)?;
        if span < 0 || span >= ROWID_RANGE_CANDIDATES {
            return None;
        }
        Some((start..=end).collect())
    }

    /// Folds the tightest rowid bounds of one predicate into `lower` / `upper`.
    /// Exclusive comparisons are widened to inclusive on purpose: the
    /// candidate set is only a superset and the predicate runs again later.
    pub(crate) fn collect_rowid_range(
        expr: &Expr,
        alias_name: Option<&str>,
        label: &str,
        parameters: &[Cell],
        lower: &mut Option<i64>,
        upper: &mut Option<i64>,
    ) {
        match expr {
            Expr::And(left, right) => {
                Self::collect_rowid_range(left, alias_name, label, parameters, lower, upper);
                Self::collect_rowid_range(right, alias_name, label, parameters, lower, upper);
            }
            Expr::Compare { left, op, right } if matches!(op, CompareOp::Gt | CompareOp::GtEq | CompareOp::Lt | CompareOp::LtEq) => {
                let (column, value, flipped) = match (left.as_ref(), right.as_ref()) {
                    (Expr::Column(column), value) => (column, known_cell(value, parameters), false),
                    (value, Expr::Column(column)) => (column, known_cell(value, parameters), true),
                    _ => return,
                };
                let Some(cell) = value
                else {
                    return;
                };
                let value = match cell {
                    Cell::Integer(value) => value,
                    Cell::Real(real) => match integral_rowid(real) {
                        Some(value) => value,
                        None => return,
                    },
                    _ => return,
                };
                if column.qualifier.as_deref().is_some_and(|qualifier| !qualifier.eq_ignore_ascii_case(label)) {
                    return;
                }
                let rowid_named = is_rowid_alias(&column.name);
                let alias_named = alias_name.is_some_and(|name| name.eq_ignore_ascii_case(&column.name));
                if !(rowid_named || alias_named) {
                    return;
                }
                let op = if flipped { flip_range_op(*op) } else { *op };
                match op {
                    CompareOp::Gt | CompareOp::GtEq => *lower = Some(lower.map_or(value, |bound| bound.max(value))),
                    CompareOp::Lt | CompareOp::LtEq => *upper = Some(upper.map_or(value, |bound| bound.min(value))),
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Walks the top-level `AND` chain of a predicate for a `rowid = <int>` (or
    /// `<int> = rowid`) comparison on the rowid-alias column or an implicit
    /// `rowid` / `_rowid_` / `oid` name. Anything outside a plain `AND` of
    /// `=` comparisons yields `None`.
    ///
    /// The right-hand side may be an integer literal or a bound parameter whose
    /// value is already known at execution time; a non-integer binding keeps the
    /// full scan, exactly like a non-integer literal.
    pub fn rowid_eq_conjunct(expr: &Expr, alias_name: Option<&str>, label: &str, parameters: &[Cell]) -> Option<i64> {
        match expr {
            Expr::And(left, right) => Self::rowid_eq_conjunct(left, alias_name, label, parameters)
                .or_else(|| Self::rowid_eq_conjunct(right, alias_name, label, parameters)),
            Expr::Compare { left, op, right } if *op == CompareOp::Eq => {
                let (column, value) = match (left.as_ref(), right.as_ref()) {
                    (Expr::Column(column), value) => (column, known_cell(value, parameters)?),
                    (value, Expr::Column(column)) => (column, known_cell(value, parameters)?),
                    _ => return None,
                };
                // JS 绑定的数字默认是 REAL 存储类，而 rowid 列是 INTEGER 亲和。
                // SQLite 比较前会做亲和转换，`id = 3.0` 与 `id = 3` 命中同一行；
                // 这里把无小数部分的 REAL 折成 INTEGER 才不丢掉这条下推。
                let value = match value {
                    Cell::Integer(value) => value,
                    Cell::Real(real) => integral_rowid(real)?,
                    _ => return None,
                };
                if column.qualifier.as_deref().is_some_and(|qualifier| !qualifier.eq_ignore_ascii_case(label)) {
                    return None;
                }
                let rowid_named = is_rowid_alias(&column.name);
                let alias_named = alias_name.is_some_and(|name| name.eq_ignore_ascii_case(&column.name));
                if rowid_named || alias_named { Some(value) } else { None }
            }
            _ => None,
        }
    }

    /// Walks the top-level `AND` chain for a `rowid IN (<integers>)` list and
    /// returns the listed rowids ascending.
    ///
    /// The list must be a plain (not `NOT`) `IN` over the rowid-alias or
    /// implicit rowid name, with every item a known integer. Bound values are
    /// accepted once they are bound, exactly like the equality pin; anything
    /// else keeps the full scan.
    pub fn rowid_in_conjunct(expr: &Expr, alias_name: Option<&str>, label: &str, parameters: &[Cell]) -> Option<Vec<i64>> {
        match expr {
            Expr::And(left, right) => Self::rowid_in_conjunct(left, alias_name, label, parameters)
                .or_else(|| Self::rowid_in_conjunct(right, alias_name, label, parameters)),
            Expr::In { value, list, negated: false } => {
                let Expr::Column(column) = value.as_ref()
                else {
                    return None;
                };
                if column.qualifier.as_deref().is_some_and(|qualifier| !qualifier.eq_ignore_ascii_case(label)) {
                    return None;
                }
                let rowid_named = is_rowid_alias(&column.name);
                let alias_named = alias_name.is_some_and(|name| name.eq_ignore_ascii_case(&column.name));
                if !(rowid_named || alias_named) || list.is_empty() {
                    return None;
                }
                let mut rowids = Vec::with_capacity(list.len());
                for item in list {
                    let value = match known_cell(item, parameters)? {
                        Cell::Integer(value) => value,
                        Cell::Real(real) => integral_rowid(real)?,
                        _ => return None,
                    };
                    rowids.push(value);
                }
                rowids.sort_unstable();
                rowids.dedup();
                Some(rowids)
            }
            _ => None,
        }
    }

    /// Single base rowid table of the main schema that a bounded scan may
    /// target: one `FROM` item, no derived table, no temp object or view
    /// shadowing the name, not `WITHOUT ROWID`. This is the shape gate shared
    /// by the point-pin and index-pin paths; anything else keeps the full read.
    pub(crate) fn bounded_scan_target<'a>(&'a self, from: &'a [TableRef]) -> Option<(&'a TableDef, &'a TableRef)> {
        if from.len() != 1 {
            return None;
        }
        let table = &from[0];
        if table.query.is_some() {
            return None;
        }
        let key = table.name.to_ascii_lowercase();
        let reaches_main = match table.schema.as_deref() {
            None => !self.catalog.temp_lookup.contains_key(&key) && !self.catalog.temp_views.contains_key(&key),
            Some(qualifier) => {
                let lower = qualifier.to_ascii_lowercase();
                lower == "main" || self.alias.as_deref().is_some_and(|alias| alias.eq_ignore_ascii_case(&lower))
            }
        };
        if !reaches_main || self.catalog.views.contains_key(&key) {
            return None;
        }
        let position = *self.catalog.table_lookup.get(&key)?;
        let def = &self.catalog.tables[position];
        if def.without_rowid {
            return None;
        }
        Some((def, table))
    }

    /// The pin/head hint for one `FROM` list: a point pin first, then the
    /// candidate rowids of `IN`, a rowid interval or an index probe. Every pin
    /// only narrows what the scan decodes; the predicate still runs in full, so
    /// the emitted rows and their order never change.
    pub(crate) fn scan_hint(&self, from: &[TableRef], filter: &Option<Expr>) -> Option<ScanHint> {
        match self.rowid_pin(from, filter) {
            Some(rowid) => Some(ScanHint::Pin(rowid)),
            None => self
                .rowid_in_pin(from, filter)
                .or_else(|| self.rowid_range_pin(from, filter))
                .map(ScanHint::Rowids)
                .or_else(|| self.rowid_bound_hint(from, filter))
                .or_else(|| self.index_pin(from, filter).map(ScanHint::Rowids)),
        }
    }

    /// 相关子查询里的等值下推:`where <indexed col> = <外层列>`。
    ///
    /// 与外层字面量下推不同,这里的键只在执行期由外层行决定。形状被刻意限窄:
    /// 单张普通 rowid 表、比较的一侧是本表的一列且该列是某个普通索引的首列、
    /// 另一侧直接就是一层外引用。命中时按外层值查一次索引探针,只解码候选行;
    /// 谓词仍整体重跑,所以候选超集不影响结果。
    pub(crate) fn runtime_index_pin(&self, from: &[TableRef], filter: &Option<Expr>, outer: &[Scope]) -> Option<ScanHint> {
        if from.len() != 1 || outer.len() != 1 {
            return None;
        }
        let filter = filter.as_ref()?;
        let def = self.prunable_def(&from[0])?;
        let width = def.columns.len();
        let source = Source {
            label: from[0].label().to_string(),
            columns: SourceColumns::Table(def),
            offset: 0,
            join: None,
            star_hidden: vec![false; width],
            unqualified_hidden: vec![false; width],
            star_override: vec![None; width],
        };
        let relation = Relation { sources: vec![source], using: vec![Vec::new()], using_slots: Vec::new(), trigger: None, excluded: false };
        let outer_shapes: Vec<ScopeShape> = outer.iter().map(Scope::shape).collect();
        let mut windows = Vec::new();
        let mut specs = Vec::new();
        let resolved = resolve_expr(filter, &relation, &outer_shapes, &[], &mut windows, &mut specs, false, false).ok()?;
        if !windows.is_empty() || !specs.is_empty() {
            return None;
        }
        let mut conjuncts = Vec::new();
        split_conjuncts(resolved, &mut conjuncts);
        for conjunct in &conjuncts {
            let ResolvedExpr::Compare { left, op: CompareOp::Eq, right, .. } = conjunct
            else {
                continue;
            };
            let (slot, probe) = match (left.as_ref(), right.as_ref()) {
                (ResolvedExpr::Column { source: 0, index: slot }, probe) => (*slot, probe),
                (probe, ResolvedExpr::Column { source: 0, index: slot }) => (*slot, probe),
                _ => continue,
            };
            let ResolvedExpr::OuterColumn { depth: 0, index } = probe
            else {
                continue;
            };
            let Some(value) = outer[0].columns.get(*index).map(|column| column.value.clone())
            else {
                continue;
            };
            if let Some(rowids) = self.index_probe_by_column(def, slot, &value) {
                return Some(ScanHint::Rowids(rowids));
            }
        }
        None
    }

    /// 等值连接的内层按索引取值:只把匹配外层键的右行交给连接。
    ///
    /// 形状刻意限得很窄:内连 / 左连的 `ON` 恰好是 `<本源裸列> = <更早源的裸列>`,
    /// 两列声明的排序规则相同,且本源码列是某个普通单列索引的首列。命中时用
    /// 已物化的外层列值逐值查索引,把候选 rowid 当作内层的扫描提示;`ON` 仍在
    /// 候选上重跑,所以候选只是超集,行集与左主序都不变。右 / 全连接会用到未
    /// 匹配的右行,因此不做这层过滤。
    pub(crate) fn join_index_pin(
        &self,
        from: &[TableRef],
        index: usize,
        tables: &[Rc<Vec<Vec<Cell>>>],
        masks: Option<&[Option<Vec<bool>>]>,
    ) -> Option<ScanHint> {
        let table = from.get(index)?;
        if !matches!(table.join, Some(JoinKind::Inner) | Some(JoinKind::Left)) {
            return None;
        }
        let Expr::Compare { left, op: CompareOp::Eq, right } = table.on.as_ref()?
        else {
            return None;
        };
        let (Expr::Column(inner), Expr::Column(probe)) = (left.as_ref(), right.as_ref())
        else {
            return None;
        };
        let (inner, probe) = if column_qualified_as(inner, table.label()) {
            (inner, probe)
        }
        else if column_qualified_as(probe, table.label()) {
            (probe, inner)
        }
        else {
            return None;
        };
        let def = self.prunable_def(table)?;
        let position = def.columns.iter().position(|column| column.name.eq_ignore_ascii_case(&inner.name))?;
        // rowid 别名列没有存储值,按它取键会读到 NULL;这类连接保持整表扫描。
        if Some(position) == def.rowid_alias {
            return None;
        }
        let qualifier = probe.qualifier.as_deref()?;
        let owner = (0..index).find(|candidate| from[*candidate].label().eq_ignore_ascii_case(qualifier))?;
        let owner_def = self.prunable_def(&from[owner])?;
        let owner_position = owner_def.columns.iter().position(|column| column.name.eq_ignore_ascii_case(&probe.name))?;
        // 投影掩码可能跳过探针列,行内只剩 NULL 占位,此时不能取键。
        if masks
            .and_then(|masks| masks.get(owner))
            .and_then(|mask| mask.as_deref())
            .is_some_and(|mask| !mask.get(owner_position).copied().unwrap_or(true))
        {
            return None;
        }
        let collation = def.columns[position].collate.unwrap_or_default();
        if owner_def.columns[owner_position].collate.unwrap_or_default() != collation {
            return None;
        }
        let table_key = def.name.to_ascii_lowercase();
        let usable = self.catalog.indexes.values().any(|candidate| {
            !candidate.managed
                && candidate.table == table_key
                && candidate.columns.as_slice() == [position]
                && candidate.collations.first().copied().unwrap_or_default() == collation
        });
        if !usable {
            return None;
        }
        let rows = tables.get(owner)?;
        let mut rowids = std::collections::BTreeSet::new();
        for row in rows.iter() {
            let Some(cell) = row.get(owner_position)
            else {
                return None;
            };
            // `NULL = anything` 永远不成立,不贡献候选,也不该让整条连接退回整表扫描。
            if matches!(cell, Cell::Null) {
                continue;
            }
            rowids.extend(self.index_probe_by_column(def, position, cell)?);
        }
        Some(ScanHint::Rowids(rowids.into_iter().collect()))
    }

    /// 按一列的运行期取值查普通索引,返回候选 rowid。
    pub(crate) fn index_probe_by_column(&self, def: &TableDef, position: usize, cell: &Cell) -> Option<Vec<i64>> {
        let table_key = def.name.to_ascii_lowercase();
        for index in self.catalog.indexes.values() {
            if index.managed || index.table != table_key || index.columns.as_slice() != [position] {
                continue;
            }
            let affinity = affinity_of(&def.columns[position].declared_type);
            let cell = match (affinity, cell) {
                (Affinity::Integer, Cell::Real(real)) => Cell::Integer(integral_rowid(*real)?),
                _ => cell.clone(),
            };
            match (&affinity, &cell) {
                (Affinity::Text, Cell::Text(_)) | (Affinity::Real, Cell::Real(_)) | (Affinity::Integer, Cell::Integer(_)) => {}
                _ => continue,
            }
            let collation = index.collations.first().copied().unwrap_or_default();
            let key = probe_key(&cell, collation)?;
            if matches!(key, ProbeKey::Wide(_)) {
                return None;
            }
            return self.index_probe_rowids(index, &[key]);
        }
        None
    }

    /// `<rowid> >= a` / `<rowid> <= b` 的单边或双边区间提示。
    /// 与 [`ExecContext::rowid_range_pin`] 不同,这里不要求两侧都有界,也不展开候选
    /// rowid:读取侧直接用同一个区间走 b-tree 范围读,缺失的一侧不设界。谓词仍
    /// 在解码后的行上重跑,所以区间只是候选超集。
    pub(crate) fn rowid_bound_hint(&self, from: &[TableRef], filter: &Option<Expr>) -> Option<ScanHint> {
        let (def, table) = self.bounded_scan_target(from)?;
        let alias_name = def.rowid_alias.map(|index| def.columns[index].name.clone());
        let filter = filter.as_ref()?;
        let mut lower = None;
        let mut upper = None;
        Self::collect_rowid_range(filter, alias_name.as_deref(), table.label(), self.parameters, &mut lower, &mut upper);
        if lower.is_none() && upper.is_none() {
            return None;
        }
        Some(ScanHint::Range { lower, upper })
    }

    /// `WHERE` conjuncts that read only one source of a multi-source `FROM`,
    /// re-combined into that source's own filter.
    ///
    /// A conjunct is kept only when every column it reads is qualified with the
    /// source's own label, or is an unqualified declared column no other source
    /// can shadow. The whole predicate still runs after the join, so narrowing
    /// the source read with this filter cannot change the result set.
    pub(crate) fn source_filter(&self, index: usize, from: &[TableRef], filter: &Option<Expr>) -> Option<Expr> {
        let filter = filter.as_ref()?;
        let table = &from[index];
        let def = self.prunable_def(table)?;
        let label = table.label();
        let mut conjuncts = Vec::new();
        self.collect_source_conjuncts(filter, def, label, from, index, &mut conjuncts);
        let mut combined = conjuncts.into_iter();
        let mut filter = combined.next()?;
        for conjunct in combined {
            filter = Expr::And(Box::new(filter), Box::new(conjunct));
        }
        Some(filter)
    }

    /// Splits a raw `AND` chain into the conjuncts that belong to one source.
    pub(crate) fn collect_source_conjuncts(
        &self,
        expr: &Expr,
        def: &TableDef,
        label: &str,
        from: &[TableRef],
        index: usize,
        out: &mut Vec<Expr>,
    ) {
        match expr {
            Expr::And(left, right) => {
                self.collect_source_conjuncts(left, def, label, from, index, out);
                self.collect_source_conjuncts(right, def, label, from, index, out);
            }
            single => {
                if self.conjunct_is_source_local(single, def, label, from, index) {
                    out.push(single.clone());
                }
            }
        }
    }

    /// Whether every column a conjunct reads provably resolves to one source.
    pub(crate) fn conjunct_is_source_local(&self, expr: &Expr, def: &TableDef, label: &str, from: &[TableRef], index: usize) -> bool {
        let mut refs = Vec::new();
        if !collect_unmasked_columns(expr, &mut refs) {
            return false;
        }
        for column in refs {
            if let Some(qualifier) = &column.qualifier {
                if !qualifier.eq_ignore_ascii_case(label) {
                    return false;
                }
                continue;
            }
            // 裸列名可能被任意一个来源解析,只有其它来源都不提供同名列时
            // 才能确定归属;隐式 rowid 名字在多表下同样有歧义。
            if is_rowid_alias(&column.name) {
                return false;
            }
            if !def.columns.iter().any(|entry| entry.name.eq_ignore_ascii_case(&column.name)) {
                return false;
            }
            for (other, table) in from.iter().enumerate() {
                if other == index {
                    continue;
                }
                match self.prunable_def(table) {
                    Some(other_def) => {
                        if other_def.columns.iter().any(|entry| entry.name.eq_ignore_ascii_case(&column.name)) {
                            return false;
                        }
                    }
                    None => return false,
                }
            }
        }
        true
    }

    /// Detects a `WHERE` conjunction that pins every key column of one plain
    /// column index to a literal with `=`, and returns the matching rowids in
    /// ascending order.
    ///
    /// The gate is deliberately narrow so the probe can never miss a row the
    /// full predicate would keep. Only non-managed indexes over declared
    /// columns qualify, each key column's literal must match the column's
    /// affinity class exactly (a `TEXT` column with a text literal, an
    /// `INTEGER` column with an integer literal, a `REAL` column with a real
    /// literal), and a stored key the canonical signature cannot reproduce
    /// poisons the whole probe. The full predicate still runs downstream, so
    /// over-fetching is harmless. Within one full-key equality the index
    /// entries are ordered by rowid, which is also the order of the full
    /// table scan, so the emitted rows keep their position.
    pub(crate) fn index_pin(&self, from: &[TableRef], filter: &Option<Expr>) -> Option<Vec<i64>> {
        let (def, table) = self.bounded_scan_target(from)?;
        let filter = filter.as_ref()?;
        let table_key = def.name.to_ascii_lowercase();
        for index in self.catalog.indexes.values() {
            if index.managed || index.table != table_key || index.columns.is_empty() {
                continue;
            }
            if let Some(rowids) = self.index_probe_rows(index, def, filter, table.label()) {
                return Some(rowids);
            }
            if let Some(rowids) = self.index_range_rows(index, def, filter, table.label()) {
                return Some(rowids);
            }
        }
        None
    }

    /// Builds the probe key from the equality literals and looks it up in the
    /// index's cached key set. `None` means the shape is not pushdown-safe and
    /// the caller must fall back to the full scan.
    pub fn index_probe_rows(&self, index: &IndexDef, def: &TableDef, filter: &Expr, label: &str) -> Option<Vec<i64>> {
        let mut equalities: Vec<(usize, Cell)> = Vec::new();
        collect_index_equalities(filter, def, label, self.parameters, &mut equalities);
        if equalities.is_empty() {
            return None;
        }
        let mut key = Vec::with_capacity(index.columns.len());
        for (slot, position) in index.columns.iter().enumerate() {
            let (_, cell) = equalities.iter().find(|(column, _)| column == position)?;
            let affinity = affinity_of(&def.columns[*position].declared_type);
            // INTEGER 亲和列上的绑定数字是 REAL 存储类；无小数部分时折成
            // INTEGER，索引键的规范签名才会与存储键一致。
            let cell = match (affinity, cell) {
                (Affinity::Integer, Cell::Real(real)) => Cell::Integer(integral_rowid(*real)?),
                _ => cell.clone(),
            };
            match (affinity, &cell) {
                (Affinity::Text, Cell::Text(_)) | (Affinity::Real, Cell::Real(_)) | (Affinity::Integer, Cell::Integer(_)) => {}
                _ => return None,
            }
            let collation = index.collations.get(slot).copied().unwrap_or_default();
            key.push(probe_key(&cell, collation)?);
        }
        if key.iter().any(|part| matches!(part, ProbeKey::Wide(_))) {
            return None;
        }
        self.index_probe_rowids(index, &key)
    }

    /// Returns the rowids stored under one probe key, rebuilding the cached
    /// key set first when the write epoch moved on.
    pub(crate) fn index_probe_rowids(&self, index: &IndexDef, key: &[ProbeKey]) -> Option<Vec<i64>> {
        let version = self.index_version(&index.name);
        let name = index.name.to_ascii_lowercase();
        {
            let cache = self.state.index_probes.borrow();
            if let Some(probe) = cache.get(&name) {
                if probe.epoch == version {
                    return if probe.poisoned { None } else { probe.map.get(key).cloned() };
                }
            }
        }
        let probe = self.build_index_probe(index, version);
        let rowids = if probe.poisoned { None } else { probe.map.get(key).cloned() };
        self.state.index_probes.borrow_mut().insert(name, probe);
        rowids
    }

    /// Detects a range predicate on the index's leading column and returns the
    /// matching rowids in ascending order.
    ///
    /// The gate is as narrow as the equality probe: only non-managed indexes
    /// over declared columns qualify, every bound literal must match the
    /// column's affinity class, and a poisoned key set keeps the full scan. The
    /// full predicate still runs downstream, so a wider candidate set is safe,
    /// and ascending rowids preserve the full-scan order.
    pub fn index_range_rows(&self, index: &IndexDef, def: &TableDef, filter: &Expr, label: &str) -> Option<Vec<i64>> {
        let position = *index.columns.first()?;
        let mut lower: Option<IndexBound> = None;
        let mut upper: Option<IndexBound> = None;
        collect_range_bounds(filter, def, label, position, self.parameters, &mut lower, &mut upper);
        if lower.is_none() && upper.is_none() {
            return None;
        }
        let affinity = affinity_of(&def.columns[position].declared_type);
        for bound in [lower.as_mut(), upper.as_mut()].into_iter().flatten() {
            // 绑定数字是 REAL 存储类，INTEGER 亲和列的等值 / 范围比较会先做亲和
            // 转换；无小数部分的 REAL 折成 INTEGER 才能继续用索引探针。
            if let (Affinity::Integer, Cell::Real(real)) = (&affinity, &bound.cell) {
                match integral_rowid(*real) {
                    Some(value) => bound.cell = Cell::Integer(value),
                    None => return None,
                }
            }
            match (&affinity, &bound.cell) {
                (Affinity::Text, Cell::Text(_)) | (Affinity::Real, Cell::Real(_)) | (Affinity::Integer, Cell::Integer(_)) => {}
                _ => return None,
            }
        }
        let collation = index.collations.first().copied().unwrap_or_default();
        self.index_range_rowids(index, &lower, &upper, collation)
    }

    /// Filters the cached index entries against the range bounds, rebuilding
    /// the cache first when the write epoch moved on.
    pub(crate) fn index_range_rowids(
        &self,
        index: &IndexDef,
        lower: &Option<IndexBound>,
        upper: &Option<IndexBound>,
        collation: Collation,
    ) -> Option<Vec<i64>> {
        let version = self.index_version(&index.name);
        let name = index.name.to_ascii_lowercase();
        let select = |probe: &IndexProbe| -> Option<Vec<i64>> {
            if probe.poisoned {
                return None;
            }
            // 索引项按首列有序时,用两次二分取出候选区间,只遍历命中项。
            if probe.sorted {
                let collation = collation;
                let lower_start = match lower {
                    None => 0,
                    Some(bound) => probe.entries.partition_point(|(key, _)| match key.first() {
                        Some(cell) => {
                            let ordering = compare_cells_with(cell, &bound.cell, collation);
                            ordering == Ordering::Less || (!bound.inclusive && ordering == Ordering::Equal)
                        }
                        None => true,
                    }),
                };
                let upper_end = match upper {
                    None => probe.entries.len(),
                    Some(bound) => probe.entries.partition_point(|(key, _)| match key.first() {
                        Some(cell) => {
                            let ordering = compare_cells_with(cell, &bound.cell, collation);
                            ordering == Ordering::Less || (bound.inclusive && ordering == Ordering::Equal)
                        }
                        None => false,
                    }),
                };
                if lower_start >= upper_end {
                    return Some(Vec::new());
                }
                let mut rowids: Vec<i64> = probe.entries[lower_start..upper_end]
                    .iter()
                    .map(|(_, rowid)| *rowid)
                    .filter(|rowid| !probe.deleted.contains(rowid))
                    .collect();
                rowids.sort_unstable();
                // 重新插入同一个 rowid 会留下一条旧键墓碑条目,候选按 rowid
                // 去重后与整表扫描的行集一致。
                rowids.dedup();
                return Some(rowids);
            }
            let mut rowids = Vec::new();
            for (key, rowid) in &probe.entries {
                if probe.deleted.contains(rowid) {
                    continue;
                }
                let Some(cell) = key.first()
                else {
                    continue;
                };
                if let Some(bound) = lower {
                    let ordering = compare_cells_with(cell, &bound.cell, collation);
                    if ordering == Ordering::Less || (!bound.inclusive && ordering == Ordering::Equal) {
                        continue;
                    }
                }
                if let Some(bound) = upper {
                    let ordering = compare_cells_with(cell, &bound.cell, collation);
                    if ordering == Ordering::Greater || (!bound.inclusive && ordering == Ordering::Equal) {
                        continue;
                    }
                }
                rowids.push(*rowid);
            }
            rowids.sort_unstable();
            rowids.dedup();
            Some(rowids)
        };
        {
            let cache = self.state.index_probes.borrow();
            if let Some(probe) = cache.get(&name) {
                if probe.epoch == version {
                    return select(probe);
                }
            }
        }
        let probe = self.build_index_probe(index, version);
        let rowids = select(&probe);
        self.state.index_probes.borrow_mut().insert(name, probe);
        rowids
    }

    /// Decodes one index's live key set into canonical signatures. A stored
    /// key outside the canonical form (a NaN or a wide integer) poisons the
    /// probe so every later query keeps the full scan.
    pub(crate) fn build_index_probe(&self, index: &IndexDef, epoch: u64) -> IndexProbe {
        let mut probe = IndexProbe { epoch, map: HashMap::new(), entries: Vec::new(), sorted: false, deleted: HashSet::new(), poisoned: false };
        let entries = match self.writer.index_entries_decoded(&index.name) {
            Ok(entries) => entries,
            Err(_) => {
                probe.poisoned = true;
                return probe;
            }
        };
        probe.entries.reserve(entries.len());
        for (values, rowid) in entries {
            if values.len() != index.columns.len() {
                probe.poisoned = true;
                break;
            }
            let cells: Vec<Cell> = values.iter().map(write_value_cell).collect();
            let mut key = Vec::with_capacity(values.len());
            let mut canonical = true;
            for (cell, collation) in cells.iter().zip(&index.collations) {
                match probe_key(cell, *collation) {
                    Some(part) if !matches!(part, ProbeKey::Wide(_)) => key.push(part),
                    _ => {
                        canonical = false;
                        break;
                    }
                }
            }
            if !canonical {
                probe.poisoned = true;
                break;
            }
            probe.map.entry(key).or_default().push(rowid);
            probe.entries.push((cells, rowid));
        }
        if !probe.poisoned {
            for rowids in probe.map.values_mut() {
                rowids.sort_unstable();
            }
            // 索引项通常按首列有序存储,确认一次后范围探针即可二分。
            let collation = index.collations.first().copied().unwrap_or_default();
            probe.sorted = probe.entries.windows(2).all(|pair| match (pair[0].0.first(), pair[1].0.first()) {
                (Some(left), Some(right)) => compare_cells_with(left, right, collation) != Ordering::Greater,
                _ => false,
            });
        }
        probe
    }

    /// 一条索引的内容版本:从未登记过的索引视为 `0`。
    pub(crate) fn index_version(&self, name: &str) -> u64 {
        self.state.index_versions.borrow().get(&name.to_ascii_lowercase()).copied().unwrap_or(0)
    }

    /// 在空索引上登记一份空探针,让后续写语句从第一行起就增量维护键集。
    ///
    /// 只在 `CREATE INDEX` 建在空表上时调用:此时键集为空,登记成本为常数,
    /// 而首次查询 / 区间删除不必再整份解码索引项。
    pub fn install_empty_index_probe(&self, name: &str) {
        let epoch = self.index_version(name);
        let probe = IndexProbe { epoch, map: HashMap::new(), entries: Vec::new(), sorted: true, deleted: HashSet::new(), poisoned: false };
        self.state.index_probes.borrow_mut().insert(name.to_ascii_lowercase(), probe);
    }

    /// 推进时钟并返回新 epoch。
    pub(crate) fn next_epoch(&self) -> u64 {
        let next = self.state.write_epoch.get().wrapping_add(1);
        self.state.write_epoch.set(next);
        next
    }

    /// 取出一条写语句的目标表名(小写)。
    ///
    /// 只有裸表名或 `main` / 本库别名限定的 DML 才能精确到单表;其余形态
    /// (其它 attached schema、DDL 等)返回 `None`,调用方按全量失效处理。
    pub fn statement_table_key(&self, statement: &Statement) -> Option<String> {
        let (schema, table) = match statement {
            Statement::Insert { schema, table, .. } | Statement::Update { schema, table, .. } | Statement::Delete { schema, table, .. } => {
                (schema.as_deref(), table)
            }
            _ => return None,
        };
        match schema {
            None => Some(table.to_ascii_lowercase()),
            Some(qualifier) => {
                let lower = qualifier.to_ascii_lowercase();
                if lower == "main" || self.alias.as_deref().is_some_and(|alias| alias.eq_ignore_ascii_case(&lower)) {
                    Some(table.to_ascii_lowercase())
                }
                else {
                    None
                }
            }
        }
    }

    /// 让一张表上的全部索引缓存失效。
    pub fn invalidate_table_indexes(&self, table_key: &str) {
        let epoch = self.next_epoch();
        let mut versions = self.state.index_versions.borrow_mut();
        for index in self.catalog.indexes.values() {
            if index.table == table_key {
                versions.insert(index.name.to_ascii_lowercase(), epoch);
            }
        }
    }

    /// 让全部索引缓存失效。
    pub fn invalidate_all_indexes(&self) {
        let epoch = self.next_epoch();
        let mut versions = self.state.index_versions.borrow_mut();
        for index in self.catalog.indexes.values() {
            versions.insert(index.name.to_ascii_lowercase(), epoch);
        }
    }

    /// `INSERT` 执行后把新行增量补进既有索引探针。
    ///
    /// 与删除侧的 [`ExecContext::patch_index_probes_after_delete`] 对称:只要语句只
    /// 追加行、不改写既有行,缓存内容加上这批新行就等于当前索引,不必让下一次
    /// 查询整份重建。重新插入的 rowid 会先清掉墓碑,否则探针会把它当作已删除。
    ///
    /// 追加后只要首列仍非递减,`entries` 就继续按首列有序,范围探针仍可二分;
    /// 键序被打乱时才退回线性扫描,由下一次会改写行的语句按常规失效并重建。
    pub fn patch_index_probes_after_insert(&self, table_key: &str, rows: &[(i64, Vec<Cell>)]) {
        if rows.is_empty() {
            return;
        }
        let epoch = self.next_epoch();
        let mut patched: Vec<String> = Vec::new();
        {
            let mut probes = self.state.index_probes.borrow_mut();
            for index in self.catalog.indexes.values() {
                if index.table != table_key || index.managed || index.columns.is_empty() {
                    continue;
                }
                let name = index.name.to_ascii_lowercase();
                let version = self.index_version(&index.name);
                let Some(probe) = probes.get_mut(&name)
                else {
                    continue;
                };
                // 只有与当前索引内容同步的探针才能追加:被其它语句改写过的
                // 旧快照必须留给下一条查询整份重建。
                if probe.poisoned || probe.epoch != version {
                    continue;
                }
                let mut canonical = true;
                for (rowid, cells) in rows {
                    let mut key = Vec::with_capacity(index.columns.len());
                    let mut key_cells = Vec::with_capacity(index.columns.len());
                    for (slot, position) in index.columns.iter().enumerate() {
                        let collation = index.collations.get(slot).copied().unwrap_or_default();
                        let Some(cell) = cells.get(*position)
                        else {
                            canonical = false;
                            break;
                        };
                        match probe_key(cell, collation) {
                            Some(part) if !matches!(part, ProbeKey::Wide(_)) => key.push(part),
                            _ => {
                                canonical = false;
                                break;
                            }
                        }
                        key_cells.push(cell.clone());
                    }
                    if !canonical {
                        break;
                    }
                    probe.deleted.remove(rowid);
                    let rowids = probe.map.entry(key).or_default();
                    let at = rowids.partition_point(|existing| existing < rowid);
                    if rowids.get(at) != Some(rowid) {
                        rowids.insert(at, *rowid);
                    }
                    // 只有新键落在首列序的前面时才破坏有序性;按递增键插入的
                    // 常见形态保持 `sorted`,范围探针继续走二分。
                    if probe.sorted {
                        let collation = index.collations.first().copied().unwrap_or_default();
                        if let (Some(last), Some(next)) = (probe.entries.last().and_then(|(cells, _)| cells.first()), key_cells.first())
                            && compare_cells_with(last, next, collation) == Ordering::Greater
                        {
                            probe.sorted = false;
                        }
                    }
                    probe.entries.push((key_cells, *rowid));
                }
                if !canonical {
                    probe.poisoned = true;
                    continue;
                }
                patched.push(name);
            }
        }
        if patched.is_empty() {
            return;
        }
        {
            let mut versions = self.state.index_versions.borrow_mut();
            for name in &patched {
                versions.insert(name.clone(), epoch);
            }
        }
        // 补丁后的探针内容仍准确,把版本对齐到新 epoch 继续复用。
        let mut probes = self.state.index_probes.borrow_mut();
        for name in &patched {
            if let Some(probe) = probes.get_mut(name) {
                probe.epoch = epoch;
            }
        }
    }

    /// 表上是否存在可增量维护的索引探针缓存。
    pub fn table_has_probes(&self, table_key: &str) -> bool {
        let probes = self.state.index_probes.borrow();
        self.catalog.indexes.values().any(|index| {
            index.table == table_key && !index.managed && !index.columns.is_empty() && probes.contains_key(&index.name.to_ascii_lowercase())
        })
    }

    /// 普通 `INSERT` 是否只往表尾追加行、因而保持既有索引内容不变。
    ///
    /// 条件与插入路径里 `watch_probes` 的判定一致:主库的普通 rowid 表、无
    /// 触发器、已有可增量维护的探针,且语句没有 `ON CONFLICT` / `REPLACE`。
    /// 成立时语句执行前不必让索引缓存整体失效,提交后的增量补丁即可对齐。
    pub fn insert_keeps_index_content(&self, table: &str) -> bool {
        let key = table.to_ascii_lowercase();
        if self.catalog.temp_lookup.contains_key(&key) || self.table_has_triggers(&key) {
            return false;
        }
        let Some(position) = self.catalog.table_lookup.get(&key).copied()
        else {
            return false;
        };
        if self.catalog.tables[position].without_rowid {
            return false;
        }
        self.table_has_probes(&key)
    }

    /// `UPDATE` 执行后只让真正被改动的索引失效。
    ///
    /// 索引项只由被索引列的值和行 rowid 决定:赋值没有碰到任何索引列、也没改
    /// rowid 时,索引内容与语句执行前逐项一致,缓存可以继续复用。托管索引
    /// (表达式、`COLLATE`、部分索引)的键可能依赖任意列,一律按失效处理。
    pub fn invalidate_indexes_after_update(&self, def: &TableDef, table_key: &str, assigned: &[bool]) {
        let rowid_moved = def.rowid_alias.is_some_and(|position| assigned.get(position).copied().unwrap_or(false));
        let mut dirty: Vec<String> = Vec::new();
        for index in self.catalog.indexes.values() {
            if index.table != table_key {
                continue;
            }
            let affected = rowid_moved
                || index.managed
                || index.columns.is_empty()
                || index.columns.iter().any(|position| assigned.get(*position).copied().unwrap_or(true));
            if affected {
                dirty.push(index.name.to_ascii_lowercase());
            }
        }
        if dirty.is_empty() {
            return;
        }
        let epoch = self.next_epoch();
        let mut versions = self.state.index_versions.borrow_mut();
        for name in dirty {
            versions.insert(name, epoch);
        }
    }

    /// `DELETE` 执行后把被删行从已缓存的索引探针里摘掉。
    ///
    /// 缓存是整份索引的快照,按 rowid 精确剔除与整树重建等价,因此不必让
    /// 每次删除都重付一遍全索引解码。托管索引与已过期的缓存退化为常规失效,
    /// 由下一次探针重建。
    pub fn patch_index_probes_after_delete(&self, table_key: &str, removed: &[i64], live: &[(i64, Vec<Cell>)]) {
        let removed: HashSet<i64> = removed.iter().copied().collect();
        let mut stale: Vec<String> = Vec::new();
        let mut patched: Vec<String> = Vec::new();
        {
            let mut probes = self.state.index_probes.borrow_mut();
            for index in self.catalog.indexes.values() {
                if index.table != table_key {
                    continue;
                }
                let name = index.name.to_ascii_lowercase();
                let version = self.index_version(&index.name);
                let Some(probe) = probes.get_mut(&name)
                else {
                    continue;
                };
                if index.managed || index.columns.is_empty() || probe.poisoned || probe.epoch != version {
                    stale.push(name);
                    continue;
                }
                for (rowid, cells) in live {
                    if !removed.contains(rowid) {
                        continue;
                    }
                    let mut key = Vec::with_capacity(index.columns.len());
                    let mut canonical = true;
                    for (slot, position) in index.columns.iter().enumerate() {
                        let collation = index.collations.get(slot).copied().unwrap_or_default();
                        match cells.get(*position).and_then(|cell| probe_key(cell, collation)) {
                            Some(part) if !matches!(part, ProbeKey::Wide(_)) => key.push(part),
                            _ => {
                                canonical = false;
                                break;
                            }
                        }
                    }
                    if !canonical {
                        probe.poisoned = true;
                        break;
                    }
                    let emptied = match probe.map.get_mut(&key) {
                        Some(rowids) => {
                            rowids.retain(|id| id != rowid);
                            rowids.is_empty()
                        }
                        None => false,
                    };
                    if emptied {
                        probe.map.remove(&key);
                    }
                }
                if probe.poisoned {
                    stale.push(name);
                    continue;
                }
                if !removed.is_empty() {
                    // 只记墓碑;条目累积到一半时再一次性压缩,单条删除不再
                    // 扫整份索引项。
                    probe.deleted.extend(removed.iter().copied());
                    if probe.deleted.len() * 2 > probe.entries.len() {
                        let deleted = std::mem::take(&mut probe.deleted);
                        probe.entries.retain(|(_, rowid)| !deleted.contains(rowid));
                    }
                }
                patched.push(name);
            }
        }
        if stale.is_empty() && patched.is_empty() {
            return;
        }
        let epoch = self.next_epoch();
        {
            let mut versions = self.state.index_versions.borrow_mut();
            for name in stale.iter().chain(patched.iter()) {
                versions.insert(name.clone(), epoch);
            }
        }
        // 打过补丁的探针内容仍然准确,只把版本对齐到新 epoch 继续复用。
        if !patched.is_empty() {
            let mut probes = self.state.index_probes.borrow_mut();
            for name in &patched {
                if let Some(probe) = probes.get_mut(name) {
                    probe.epoch = epoch;
                }
            }
        }
    }
}
