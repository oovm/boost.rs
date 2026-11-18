//! 窗口函数执行机器:SQLite 方言的 `WINDOW` / `OVER` 求值。
//!
//! 行帧、分区、peer 组与逐窗口列的填充住在 [`ExecContext::window_context`]
//! 一侧;表达式求值与行比较复用 [`database_query`] 与同树的其余模块。

use std::cmp::Ordering;

use database_query::sqlite::{SqlError, bind::*, frontend::*, value::*};
use database_storage::sqlite::Collation;

use super::{
    cells::*,
    eval::{Budget, CteEnv, EvalContext, EvalExpr, SortCompare},
    select::*,
};

/// One row of the input a window partition is computed over: either a source
/// row or the representative row of a group, plus that row's aggregate values.
#[derive(Clone)]
pub struct WindowRow {
    /// Row cells.
    pub values: Vec<Cell>,
    /// Whole-result aggregate values, empty in an ungrouped query.
    pub aggregates: Vec<Cell>,
}

/// One resolved `ORDER BY` key.
pub(crate) enum SortKey {
    /// Zero-based index into the projection output.
    Projected(usize),
    /// Expression over the merged source row.
    Expression(ResolvedExpr),
}

/// One `GROUP BY` term.
pub(crate) enum GroupKey {
    /// A one-based projection ordinal.
    Projected(usize, Collation),
    /// Expression over the merged source row.
    Expression(ResolvedExpr, Collation),
}

/// Which position of a frame `first_value`/`last_value`/`nth_value` reads.
#[derive(Clone, Copy)]
pub(crate) enum ValuePick {
    /// First row of the frame.
    First,
    /// Last row of the frame.
    Last,
    /// `nth` row of the frame.
    Nth,
}

/// Compares two input rows by partition keys, then by ORDER BY keys.
pub(crate) fn compare_window_rows(
    left_partition: &[Cell],
    left_order: &[Cell],
    right_partition: &[Cell],
    right_order: &[Cell],
    order: &[(ResolvedExpr, SortDirection)],
) -> Ordering {
    for (left, right) in left_partition.iter().zip(right_partition) {
        let ordering = compare_cells(left, right);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    for (index, (_, direction)) in order.iter().enumerate() {
        let ordering = direction.compare(&left_order[index], &right_order[index]);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// Row-wise equality using SQLite's cross-type numeric rule (`1` equals `1.0`).
pub fn cells_equal(left: &[Cell], right: &[Cell]) -> bool {
    left.len() == right.len() && left.iter().zip(right).all(|(left, right)| compare_cells(left, right) == Ordering::Equal)
}

/// Integer offset a frame edge carries; `None` for `UNBOUNDED` and `CURRENT ROW`.
pub(crate) fn range_offset(bound: &ResolvedFrameBound, which: &str) -> Result<Option<i64>, SqlError> {
    match &bound.offset {
        Some(ResolvedExpr::Cell(Cell::Integer(value))) if *value >= 0 => Ok(Some(*value)),
        Some(_) => Err(SqlError::new("invalid-frame", format!("frame {which} offset must be a non-negative integer")).in_statement("select")),
        None => Ok(None),
    }
}

/// Value an `RANGE` edge names on the ORDER BY axis, given the sort direction.
pub(crate) fn range_edge(edge: FrameEdge, offset: Option<i64>, current: f64, descending: bool) -> f64 {
    let offset = offset.unwrap_or(0) as f64;
    match edge {
        FrameEdge::UnboundedPreceding => {
            if descending {
                f64::INFINITY
            }
            else {
                f64::NEG_INFINITY
            }
        }
        FrameEdge::UnboundedFollowing => {
            if descending {
                f64::NEG_INFINITY
            }
            else {
                f64::INFINITY
            }
        }
        FrameEdge::CurrentRow => current,
        FrameEdge::Preceding => {
            if descending {
                current + offset
            }
            else {
                current - offset
            }
        }
        FrameEdge::Following => {
            if descending {
                current - offset
            }
            else {
                current + offset
            }
        }
    }
}

/// Resolves one `ROWS` frame edge to a position in the partition.
pub(crate) fn rows_edge(bound: &ResolvedFrameBound, which: &str, slot: usize, len: usize) -> Result<i64, SqlError> {
    let position = slot as i64;
    Ok(match bound.edge {
        FrameEdge::UnboundedPreceding => 0,
        FrameEdge::UnboundedFollowing => len.saturating_sub(1) as i64,
        FrameEdge::CurrentRow => position,
        // Frames never extend past either end of the partition.
        FrameEdge::Preceding => position.saturating_sub(range_offset(bound, which)?.unwrap_or(0)).max(0),
        FrameEdge::Following => position.saturating_add(range_offset(bound, which)?.unwrap_or(0)).min(len.saturating_sub(1) as i64),
    })
}

/// Resolves one `GROUPS` frame edge to a position in the partition.
///
/// Peer groups are contiguous, so `group_of` (each row's dense group number)
/// both locates the target group and tells whether it lies outside the
/// partition.
pub(crate) fn groups_edge(
    bound: &ResolvedFrameBound,
    which: &str,
    slot: usize,
    len: usize,
    peer_start: &[usize],
    peer_end: &[usize],
    group_of: &[i64],
) -> Result<i64, SqlError> {
    let starting = which == "starting";
    // `CURRENT ROW` names the row's whole peer group: a start edge lands on its
    // first row and an end edge on its last.
    if bound.edge == FrameEdge::CurrentRow {
        return Ok(if starting { peer_start[slot] as i64 } else { peer_end[slot] as i64 });
    }
    let group = group_of[slot];
    let target = match bound.edge {
        FrameEdge::UnboundedPreceding => return Ok(if starting { 0 } else { -1 }),
        FrameEdge::UnboundedFollowing => {
            return Ok(if starting { len as i64 } else { len.saturating_sub(1) as i64 });
        }
        FrameEdge::Preceding => group.saturating_sub(range_offset(bound, which)?.unwrap_or(0)),
        FrameEdge::Following => group.saturating_add(range_offset(bound, which)?.unwrap_or(0)),
        FrameEdge::CurrentRow => unreachable!("handled above"),
    };
    if target < 0 {
        return Ok(if starting { 0 } else { -1 });
    }
    // Frames never extend past either end of the partition.
    let first = group_of.partition_point(|value| *value < target);
    if first >= len {
        return Ok(if starting { len as i64 } else { len as i64 - 1 });
    }
    Ok(if starting { peer_start[first] as i64 } else { peer_end[first] as i64 })
}

/// `EXCLUDE` decision for one frame position computed for `slot`.
pub(crate) fn frame_row_excluded(exclude: FrameExclude, position: usize, slot: usize, peer_start: &[usize], peer_end: &[usize]) -> bool {
    match exclude {
        FrameExclude::NoOthers => false,
        FrameExclude::CurrentRow => position == slot,
        FrameExclude::Group => position >= peer_start[slot] && position <= peer_end[slot],
        // `TIES` drops the peers but keeps the row being computed, even when it
        // is the only member of its group.
        FrameExclude::Ties => position >= peer_start[slot] && position <= peer_end[slot] && position != slot,
    }
}

/// Bucket count of `ntile(n)`; SQLite rejects zero and negative counts.
pub(crate) fn ntile_buckets(cell: &Cell) -> Result<usize, SqlError> {
    match numeric(cell) {
        Some(Number::Int(value)) if value > 0 => Ok(value as usize),
        Some(Number::Real(value)) if value.is_finite() && value.fract() == 0.0 && value > 0.0 && value <= usize::MAX as f64 => {
            Ok(value as usize)
        }
        _ => Err(SqlError::new("invalid-argument", "argument of ntile must be a positive integer").in_statement("select")),
    }
}

/// Position argument of `nth_value`; SQLite rejects zero and negative values.
pub(crate) fn nth_position(cell: &Cell) -> Result<i64, SqlError> {
    match numeric(cell) {
        Some(Number::Int(value)) if value > 0 => Ok(value),
        Some(Number::Real(value)) if value.is_finite() && value.fract() == 0.0 && value > 0.0 && value <= i64::MAX as f64 => Ok(value as i64),
        _ => Err(SqlError::new("invalid-argument", "second argument to nth_value must be a positive integer").in_statement("select")),
    }
}

/// Offset of `lag`/`lead`; a non-integer offset makes SQLite yield `NULL`.
pub(crate) fn lag_offset(cell: &Cell) -> Option<i64> {
    match numeric(cell)? {
        Number::Int(value) => Some(value),
        Number::Real(value) if value.is_finite() && value.fract() == 0.0 && value >= i64::MIN as f64 && value <= i64::MAX as f64 => {
            Some(value as i64)
        }
        Number::Real(_) => None,
    }
}

/// Converts a `LIMIT`/`OFFSET` expression result to an integer, or reports
/// SQLite's `datatype mismatch`.
pub fn limit_integer(cell: &Cell, clause: &str) -> Result<i64, SqlError> {
    let converted = match cell {
        Cell::Integer(value) => Some(*value),
        Cell::Real(value) => real_integer(*value),
        // Text first tries the exact 64-bit form, then the numeric one, so
        // `'9223372036854775807'` survives where an `f64` round-trip would not.
        Cell::Text(text) => match text.trim().parse::<i64>() {
            Ok(value) => Some(value),
            Err(_) => text.trim().parse::<f64>().ok().and_then(real_integer),
        },
        Cell::Null | Cell::Blob(_) => None,
    };
    converted.ok_or_else(|| {
        let mut error = SqlError::new("datatype-mismatch", "datatype mismatch").in_statement("select");
        error.clause = Some(clause.to_string());
        error
    })
}

pub(crate) fn real_integer(value: f64) -> Option<i64> {
    if value.is_finite() && value.fract() == 0.0 && value >= -9223372036854775808.0 && value < 9223372036854775808.0 {
        Some(value as i64)
    }
    else {
        None
    }
}

/// Clamps a limit pair to the rows actually available.
pub(crate) fn limit_range(len: usize, limit: Option<(usize, Option<usize>)>) -> (usize, usize) {
    match limit {
        None => (0, len),
        Some((offset, count)) => {
            let start = offset.min(len);
            let end = match count {
                Some(count) => start.saturating_add(count).min(len),
                None => len,
            };
            (start, end)
        }
    }
}

impl<'e> ExecContext<'e> {
    /// Computes every window of one `SELECT` core over its input rows.
    ///
    /// Returns one value column per window (indexed by input row), the
    /// composite sort key of every input row, and the descending flag of each
    /// key column. The composite key concatenates, for each window in
    /// declaration order, its partition keys then its order keys, so the first
    /// window is the primary sort and later windows only break its ties — the
    /// order SQLite emits when no trailing `ORDER BY` is written.
    pub(crate) fn evaluate_windows(
        &self,
        windows: &[ResolvedWindow],
        inputs: &[WindowRow],
        shape: &ScopeShape,
        outer: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv>,
    ) -> Result<(Vec<Vec<Cell>>, Vec<Vec<Cell>>, Vec<SortDirection>), SqlError> {
        let mut values: Vec<Vec<Cell>> = vec![Vec::new(); windows.len()];
        let mut keys: Vec<Vec<Cell>> = vec![Vec::new(); inputs.len()];
        let mut descending: Vec<SortDirection> = Vec::new();
        // Key expressions are evaluated once per window; the sorts then run
        // from the last window to the first. Each later window is a tie-breaker
        // of the previous one, which reproduces SQLite's sorter chain: the
        // first window is the primary order of the output, and within its peer
        // groups the later windows decide which row a ranking function sees
        // first.
        let mut partitions: Vec<Vec<Vec<Cell>>> = Vec::with_capacity(windows.len());
        let mut orders: Vec<Vec<Vec<Cell>>> = Vec::with_capacity(windows.len());
        for window in windows {
            let mut window_partitions: Vec<Vec<Cell>> = Vec::with_capacity(inputs.len());
            let mut window_orders: Vec<Vec<Cell>> = Vec::with_capacity(inputs.len());
            for row in inputs {
                let ctx = EvalContext {
                    exec: self.clone(),
                    parameters: self.parameters,
                    upsert_excluded: self.upsert_excluded,
                    trigger_frames: self.trigger_frames,
                    outer_read: &self.state.outer_read,
                    subquery_cache: &self.state.subquery_cache,
                    host_functions: self.host_functions,
                    outer,
                    current: shape,
                    aggregates: &row.aggregates,
                    windows: &[],
                    window_row: 0,
                    budget,
                    ctes,
                };
                let mut partition = Vec::with_capacity(window.partition.len());
                for expr in &window.partition {
                    partition.push(expr.eval(&row.values, &ctx)?);
                }
                let mut order = Vec::with_capacity(window.order.len());
                for (expr, _) in &window.order {
                    order.push(expr.eval(&row.values, &ctx)?);
                }
                window_partitions.push(partition);
                window_orders.push(order);
            }
            partitions.push(window_partitions);
            orders.push(window_orders);
            descending.resize(descending.len() + window.partition.len(), SortDirection::default());
            descending.extend(window.order.iter().map(|(_, direction)| *direction));
        }
        for (index, key_row) in keys.iter_mut().enumerate() {
            for window in 0..windows.len() {
                key_row.extend(partitions[window][index].iter().cloned());
                key_row.extend(orders[window][index].iter().cloned());
            }
        }
        let mut order: Vec<usize> = (0..inputs.len()).collect();
        for index in (0..windows.len()).rev() {
            let window = &windows[index];
            let window_partitions = &partitions[index];
            let window_orders = &orders[index];
            order.sort_by(|left, right| {
                compare_window_rows(
                    &window_partitions[*left],
                    &window_orders[*left],
                    &window_partitions[*right],
                    &window_orders[*right],
                    &window.order,
                )
            });
            let mut column = vec![Cell::Null; inputs.len()];
            let mut start = 0;
            while start < order.len() {
                let mut end = start + 1;
                while end < order.len() && cells_equal(&window_partitions[order[end]], &window_partitions[order[start]]) {
                    end += 1;
                }
                self.evaluate_window(window, &order[start..end], window_orders, inputs, &mut column, shape, outer, budget, ctes)?;
                start = end;
            }
            values[index] = column;
        }
        Ok((values, keys, descending))
    }

    /// Computes one window over one partition.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn evaluate_window(
        &self,
        window: &ResolvedWindow,
        indices: &[usize],
        orders: &[Vec<Cell>],
        inputs: &[WindowRow],
        column: &mut [Cell],
        shape: &ScopeShape,
        outer: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv>,
    ) -> Result<(), SqlError> {
        let len = indices.len();
        // Peer groups: rows whose every ORDER BY value compares equal. With no
        // ORDER BY the whole partition is one peer group, which is what makes
        // ranking functions report ties across a whole partition.
        let mut peer_start = vec![0usize; len];
        let mut peer_end = vec![len - 1; len];
        let mut dense = vec![0i64; len];
        let mut group = 0i64;
        let mut at = 0;
        while at < len {
            let mut end = at + 1;
            while end < len && cells_equal(&orders[indices[at]], &orders[indices[end]]) {
                end += 1;
            }
            for slot in at..end {
                peer_start[slot] = at;
                peer_end[slot] = end - 1;
                dense[slot] = group;
            }
            group += 1;
            at = end;
        }
        match &window.function {
            ResolvedWindowFunction::RowNumber => {
                for slot in 0..len {
                    column[indices[slot]] = Cell::Integer(slot as i64 + 1);
                }
            }
            ResolvedWindowFunction::Rank => {
                for slot in 0..len {
                    column[indices[slot]] = Cell::Integer(peer_start[slot] as i64 + 1);
                }
            }
            ResolvedWindowFunction::DenseRank => {
                for slot in 0..len {
                    column[indices[slot]] = Cell::Integer(dense[slot] + 1);
                }
            }
            ResolvedWindowFunction::PercentRank => {
                for slot in 0..len {
                    column[indices[slot]] = if len == 1 { Cell::Real(0.0) } else { Cell::Real(peer_start[slot] as f64 / (len - 1) as f64) };
                }
            }
            ResolvedWindowFunction::CumeDist => {
                for slot in 0..len {
                    column[indices[slot]] = Cell::Real((peer_end[slot] + 1) as f64 / len as f64);
                }
            }
            ResolvedWindowFunction::Ntile(buckets) => {
                let ctx = self.window_context(inputs, indices[0], shape, outer, budget, ctes);
                let count = ntile_buckets(&buckets.eval(&inputs[indices[0]].values, &ctx)?)?;
                // Bigger buckets first: the first `len % count` buckets hold one
                // extra row.
                let base = len / count;
                let extra = len % count;
                let mut slot = 0;
                for bucket in 0..count {
                    let size = if bucket < extra { base + 1 } else { base };
                    for _ in 0..size {
                        if slot >= len {
                            break;
                        }
                        column[indices[slot]] = Cell::Integer(bucket as i64 + 1);
                        slot += 1;
                    }
                }
            }
            ResolvedWindowFunction::Lag { value, offset, default } => {
                self.evaluate_offset_function(value, offset, default, true, indices, inputs, column, shape, outer, budget, ctes)?;
            }
            ResolvedWindowFunction::Lead { value, offset, default } => {
                self.evaluate_offset_function(value, offset, default, false, indices, inputs, column, shape, outer, budget, ctes)?;
            }
            ResolvedWindowFunction::FirstValue(value) => {
                self.evaluate_frame_value(
                    ValuePick::First,
                    value,
                    None,
                    window,
                    indices,
                    orders,
                    inputs,
                    column,
                    &peer_start,
                    &peer_end,
                    &dense,
                    shape,
                    outer,
                    budget,
                    ctes,
                )?;
            }
            ResolvedWindowFunction::LastValue(value) => {
                self.evaluate_frame_value(
                    ValuePick::Last,
                    value,
                    None,
                    window,
                    indices,
                    orders,
                    inputs,
                    column,
                    &peer_start,
                    &peer_end,
                    &dense,
                    shape,
                    outer,
                    budget,
                    ctes,
                )?;
            }
            ResolvedWindowFunction::NthValue { value, position } => {
                self.evaluate_frame_value(
                    ValuePick::Nth,
                    value,
                    Some(position),
                    window,
                    indices,
                    orders,
                    inputs,
                    column,
                    &peer_start,
                    &peer_end,
                    &dense,
                    shape,
                    outer,
                    budget,
                    ctes,
                )?;
            }
            ResolvedWindowFunction::Aggregate { function, argument, separator, distinct, filter, order, collation } => {
                self.evaluate_frame_aggregate(
                    *function,
                    argument.as_ref(),
                    separator.as_ref(),
                    *distinct,
                    *collation,
                    filter.as_ref(),
                    order,
                    window,
                    indices,
                    orders,
                    inputs,
                    column,
                    &peer_start,
                    &peer_end,
                    &dense,
                    shape,
                    outer,
                    budget,
                    ctes,
                )?;
            }
        }
        Ok(())
    }

    /// Context of one input row, carrying that row's whole-result aggregates.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn window_context<'a>(
        &'a self,
        inputs: &'a [WindowRow],
        index: usize,
        shape: &'a ScopeShape,
        outer: &'a [Scope],
        budget: &'a Budget,
        ctes: Option<&'a CteEnv>,
    ) -> EvalContext<'a> {
        EvalContext {
            exec: self.clone(),
            parameters: self.parameters,
            upsert_excluded: self.upsert_excluded,
            trigger_frames: self.trigger_frames,
            outer_read: &self.state.outer_read,
            subquery_cache: &self.state.subquery_cache,
            host_functions: self.host_functions,
            outer,
            current: shape,
            aggregates: &inputs[index].aggregates,
            windows: &[],
            window_row: 0,
            budget,
            ctes,
        }
    }

    /// `lag`/`lead`: the value of a row `offset` positions away.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn evaluate_offset_function(
        &self,
        value: &ResolvedExpr,
        offset: &Option<ResolvedExpr>,
        default: &Option<ResolvedExpr>,
        backward: bool,
        indices: &[usize],
        inputs: &[WindowRow],
        column: &mut [Cell],
        shape: &ScopeShape,
        outer: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv>,
    ) -> Result<(), SqlError> {
        let len = indices.len();
        for slot in 0..len {
            let ctx = self.window_context(inputs, indices[slot], shape, outer, budget, ctes);
            let step = match offset {
                None => 1,
                Some(expr) => match lag_offset(&expr.eval(&inputs[indices[slot]].values, &ctx)?) {
                    Some(step) => step,
                    // A non-integer offset makes SQLite produce NULL, it does
                    // not raise, so the row contributes nothing.
                    None => {
                        column[indices[slot]] = Cell::Null;
                        continue;
                    }
                },
            };
            let target = if backward { (slot as i64).checked_sub(step) } else { (slot as i64).checked_add(step) };
            let target = match target {
                Some(target) if target >= 0 && (target as usize) < len => target as usize,
                _ => {
                    column[indices[slot]] = match default {
                        Some(default) => default.eval(&inputs[indices[slot]].values, &ctx)?,
                        None => Cell::Null,
                    };
                    continue;
                }
            };
            let target_ctx = self.window_context(inputs, indices[target], shape, outer, budget, ctes);
            column[indices[slot]] = value.eval(&inputs[indices[target]].values, &target_ctx)?;
        }
        Ok(())
    }

    /// `first_value`/`last_value`/`nth_value`: a value read from the frame.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn evaluate_frame_value(
        &self,
        pick: ValuePick,
        value: &ResolvedExpr,
        position: Option<&ResolvedExpr>,
        window: &ResolvedWindow,
        indices: &[usize],
        orders: &[Vec<Cell>],
        inputs: &[WindowRow],
        column: &mut [Cell],
        peer_start: &[usize],
        peer_end: &[usize],
        group_of: &[i64],
        shape: &ScopeShape,
        outer: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv>,
    ) -> Result<(), SqlError> {
        let len = indices.len();
        for slot in 0..len {
            let ctx = self.window_context(inputs, indices[slot], shape, outer, budget, ctes);
            let (start, end) = self.frame_bounds(window, slot, len, indices, orders, peer_start, peer_end, group_of, &ctx)?;
            let exclude = window.frame.as_ref().map_or(FrameExclude::NoOthers, |frame| frame.exclude);
            // Every position still in the frame after EXCLUDE, in frame order.
            let mut positions = (start.max(0)..=end.min(len as i64 - 1))
                .filter(|position| !frame_row_excluded(exclude, *position as usize, slot, peer_start, peer_end))
                .map(|position| position as usize);
            let wanted = match pick {
                ValuePick::First => positions.next(),
                ValuePick::Last => positions.last(),
                ValuePick::Nth => {
                    let position = position.expect("nth_value carries a position");
                    match nth_position(&position.eval(&inputs[indices[slot]].values, &ctx)?)? {
                        0 => unreachable!("nth_position rejects zero"),
                        nth => positions.nth(nth as usize - 1),
                    }
                }
            };
            let Some(wanted) = wanted
            else {
                column[indices[slot]] = Cell::Null;
                continue;
            };
            let target_ctx = self.window_context(inputs, indices[wanted], shape, outer, budget, ctes);
            column[indices[slot]] = value.eval(&inputs[indices[wanted]].values, &target_ctx)?;
        }
        Ok(())
    }

    /// Aggregate over the frame of each row.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn evaluate_frame_aggregate(
        &self,
        function: AggregateFunction,
        argument: Option<&ResolvedExpr>,
        separator: Option<&ResolvedExpr>,
        distinct: bool,
        collation: Collation,
        filter: Option<&ResolvedExpr>,
        order: &[(ResolvedExpr, SortDirection)],
        window: &ResolvedWindow,
        indices: &[usize],
        orders: &[Vec<Cell>],
        inputs: &[WindowRow],
        column: &mut [Cell],
        peer_start: &[usize],
        peer_end: &[usize],
        group_of: &[i64],
        shape: &ScopeShape,
        outer: &[Scope],
        budget: &Budget,
        ctes: Option<&CteEnv>,
    ) -> Result<(), SqlError> {
        let len = indices.len();
        let exclude = window.frame.as_ref().map_or(FrameExclude::NoOthers, |frame| frame.exclude);
        for slot in 0..len {
            let ctx = self.window_context(inputs, indices[slot], shape, outer, budget, ctes);
            let (start, end) = self.frame_bounds(window, slot, len, indices, orders, peer_start, peer_end, group_of, &ctx)?;
            let mut entries: Vec<(Cell, Cell, Vec<Cell>)> = Vec::new();
            for position in start.max(0)..=end.min(len as i64 - 1) {
                if frame_row_excluded(exclude, position as usize, slot, peer_start, peer_end) {
                    continue;
                }
                let row = &inputs[indices[position as usize]];
                let row_ctx = self.window_context(inputs, indices[position as usize], shape, outer, budget, ctes);
                if let Some(filter) = filter {
                    if truthiness(&filter.eval(&row.values, &row_ctx)?) != Some(true) {
                        continue;
                    }
                }
                let value = match argument {
                    Some(argument) => argument.eval(&row.values, &row_ctx)?,
                    None => Cell::Null,
                };
                let separator = match separator {
                    Some(separator) => separator.eval(&row.values, &row_ctx)?,
                    None => Cell::Null,
                };
                let keys = order.iter().map(|(expr, _)| expr.eval(&row.values, &row_ctx)).collect::<Result<Vec<_>, SqlError>>()?;
                entries.push((value, separator, keys));
            }
            let (values, separators) = shape_aggregate_inputs(entries, distinct, order, collation);
            column[indices[slot]] = aggregate_cells(function, &values, if separator.is_some() { Some(&separators) } else { None });
        }
        Ok(())
    }

    /// Start and end positions (inclusive) of a row's frame inside its partition.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn frame_bounds(
        &self,
        window: &ResolvedWindow,
        slot: usize,
        len: usize,
        indices: &[usize],
        orders: &[Vec<Cell>],
        peer_start: &[usize],
        peer_end: &[usize],
        group_of: &[i64],
        _ctx: &EvalContext,
    ) -> Result<(i64, i64), SqlError> {
        let Some(frame) = &window.frame
        else {
            // No ORDER BY means the whole partition; otherwise the default is
            // `RANGE UNBOUNDED PRECEDING AND CURRENT ROW`, whose CURRENT ROW
            // edge is the last peer, not the current position.
            return Ok(if window.order.is_empty() { (0, len as i64 - 1) } else { (0, peer_end[slot] as i64) });
        };
        match frame.units {
            FrameUnits::Rows => {
                let start = rows_edge(&frame.start, "starting", slot, len)?;
                let end = rows_edge(&frame.end, "ending", slot, len)?;
                Ok((start, end))
            }
            FrameUnits::Groups => Ok((
                groups_edge(&frame.start, "starting", slot, len, peer_start, peer_end, group_of)?,
                groups_edge(&frame.end, "ending", slot, len, peer_start, peer_end, group_of)?,
            )),
            FrameUnits::Range => self.range_bounds(window, frame, slot, len, indices, orders, peer_start, peer_end),
        }
    }

    /// Value-based `RANGE` frame: rows whose ORDER BY value lies in the interval
    /// the edges describe, with `NULL`/text rows only ever matching peers.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn range_bounds(
        &self,
        window: &ResolvedWindow,
        frame: &ResolvedFrame,
        slot: usize,
        len: usize,
        indices: &[usize],
        orders: &[Vec<Cell>],
        peer_start: &[usize],
        peer_end: &[usize],
    ) -> Result<(i64, i64), SqlError> {
        let descending = window.order.first().is_some_and(|(_, direction)| direction.descending);
        let current = if window.order.len() == 1 { orders[indices[slot]].first().cloned() } else { None };
        // RANGE offsets are numeric only: text and blob ordering keys compare by
        // collation, so SQLite narrows such a frame to the peer group instead of
        // coercing the key with `numeric()`.
        let coerced = match current.as_ref() {
            Some(Cell::Integer(value)) => Some(Number::Int(*value)),
            Some(Cell::Real(value)) => Some(Number::Real(*value)),
            _ => None,
        };
        let offset_start = range_offset(&frame.start, "starting")?;
        let offset_end = range_offset(&frame.end, "ending")?;
        if (offset_start.is_some() || offset_end.is_some()) && window.order.len() != 1 {
            let mut error =
                SqlError::new("invalid-frame", "RANGE with offset PRECEDING/FOLLOWING requires one ORDER BY expression").in_statement("select");
            error.clause = Some("RANGE".to_string());
            return Err(error);
        }
        // A non-numeric current value can only match its own peers; unbounded
        // edges still reach the edges of the partition.
        let Some(current) = coerced
        else {
            let start = match frame.start.edge {
                FrameEdge::UnboundedPreceding => 0,
                FrameEdge::UnboundedFollowing => len as i64,
                _ => peer_start[slot] as i64,
            };
            let end = match frame.end.edge {
                FrameEdge::UnboundedFollowing => len as i64 - 1,
                FrameEdge::UnboundedPreceding => -1,
                _ => peer_end[slot] as i64,
            };
            return Ok((start, end));
        };
        let current = number_f64(current);
        // Each edge names a value on the ORDER BY axis. Descending order flips
        // PRECEDING towards larger values, so the frame is the interval between
        // the two edges, and is empty when they are in the wrong order.
        let start_edge = range_edge(frame.start.edge, offset_start, current, descending);
        let end_edge = range_edge(frame.end.edge, offset_end, current, descending);
        let ordered = if descending { start_edge >= end_edge } else { start_edge <= end_edge };
        if !ordered {
            return Ok((1, 0));
        }
        let low = start_edge.min(end_edge);
        let high = start_edge.max(end_edge);
        // An unbounded edge spans that whole side of the partition, so rows
        // whose ORDER BY key has no numeric form are not dropped from it.
        let start_unbounded = matches!(frame.start.edge, FrameEdge::UnboundedPreceding);
        let end_unbounded = matches!(frame.end.edge, FrameEdge::UnboundedFollowing);
        let mut first = start_unbounded.then_some(0);
        let mut last = end_unbounded.then_some(len - 1);
        for position in 0..len {
            let Some(value) = orders[indices[position]].first().and_then(numeric)
            else {
                continue;
            };
            let value = number_f64(value);
            if value >= low && value <= high {
                first.get_or_insert(position);
                if !end_unbounded {
                    last = Some(position);
                }
            }
        }
        // NULL sorts before every numeric key in ascending order, so the
        // leading run of rows without a numeric key stays inside a frame that
        // begins at the partition start even when its other edge is bounded.
        if start_unbounded && !descending && first.is_some() {
            let nulls = (0..len).take_while(|position| orders[indices[*position]].first().and_then(numeric).is_none()).count();
            if nulls > 0 {
                last = Some(last.map_or(nulls - 1, |last| last.max(nulls - 1)));
            }
        }
        Ok(match (first, last) {
            (Some(first), Some(last)) => (first as i64, last as i64),
            _ => (1, 0),
        })
    }
}
