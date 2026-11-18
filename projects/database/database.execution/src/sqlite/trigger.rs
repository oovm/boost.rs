//! Trigger event state and the leaf machinery of the SQLite trigger machine.
//!
//! C8 relocates the `NEW` / `OLD` frame stack, the row-event queue and the
//! active-trigger stack into [`TriggerState`], owned by the execution side.
//! The event evaluation helpers (`WHEN`, trigger-body expressions), the
//! local-target gate and the `INSERT` enqueue points live here too. The
//! statement-dispatch edges of `run_triggered` / `fire_matching` stay in the
//! facade skeleton: trigger bodies are arbitrary statements and the dispatch
//! skeleton (attach orchestration, transactions, DDL) does not move with this
//! batch.

use database_query::sqlite::{SqlError, bind::*, catalog::*, frontend::*, value::*};

use crate::sqlite::{
    dml::DmlRowEvent,
    eval::{Budget, EvalContext, EvalExpr},
    select::ExecContext,
};

/// Maximum nesting of recursive trigger programs, like SQLite's limit.
pub const MAX_TRIGGER_DEPTH: usize = 100;

/// Trigger event queue and `NEW` / `OLD` frames of the statement machine.
///
/// The owner of the three stacks is the execution side; the facade holds one
/// instance and assembles it into the statement contexts (same transitional
/// arrangement as `SqliteExecState`, pending the C9 owner ruling).
#[derive(Default)]
pub struct TriggerState {
    /// `NEW` / `OLD` frames of the trigger bodies currently executing,
    /// outermost first.
    pub frames: Vec<TriggerFrame>,
    /// Row-level trigger events queued by the statement being executed.
    pub pending_events: Vec<DmlRowEvent>,
    /// Lowercase names of the triggers whose body is executing right now.
    /// With SQLite's default `recursive_triggers = OFF` a trigger program is
    /// not re-invoked by changes it caused itself, directly or indirectly,
    /// while the other triggers still fire.
    pub active_triggers: Vec<String>,
}

/// Statement label a trigger event reports in diagnostics.
pub fn event_statement(event: &TriggerEvent) -> &'static str {
    match event {
        TriggerEvent::Insert => "insert",
        TriggerEvent::Update(_) => "update",
        TriggerEvent::Delete => "delete",
    }
}

/// Whether a definition's event covers an incoming statement event.
pub fn trigger_event_matches(defined: &TriggerEvent, incoming: &TriggerEvent) -> bool {
    match (defined, incoming) {
        (TriggerEvent::Insert, TriggerEvent::Insert) | (TriggerEvent::Delete, TriggerEvent::Delete) => true,
        (TriggerEvent::Update(defined), TriggerEvent::Update(assigned)) => {
            defined.is_empty() || defined.iter().any(|column| assigned.iter().any(|assigned| assigned.eq_ignore_ascii_case(column)))
        }
        _ => false,
    }
}

/// Whether an unqualified or schema-qualified target table lives in one of
/// the schemas this engine owns; an attached target runs in its own engine
/// and must not fire the triggers of `main`.
pub fn target_is_local(catalog: &SqliteCatalog, alias: Option<&str>, attached_empty: bool, schema: Option<&str>, table: &str) -> bool {
    let key = table.to_ascii_lowercase();
    match schema {
        Some(qualifier) => {
            let lower = qualifier.to_ascii_lowercase();
            lower == "main" || lower == "temp" || alias.is_some_and(|alias| alias.eq_ignore_ascii_case(qualifier))
        }
        None => {
            catalog.temp_lookup.contains_key(&key)
                || catalog.table_lookup.contains_key(&key)
                || catalog.views.contains_key(&key)
                || attached_empty
        }
    }
}

impl ExecContext<'_> {
    /// Evaluates one trigger `WHEN` predicate against the current frame.
    pub fn trigger_when_holds(&self, when: &Expr) -> Result<bool, SqlError> {
        let relation =
            Relation { sources: Vec::new(), using: Vec::new(), using_slots: Vec::new(), trigger: self.trigger_frames.last(), excluded: false };
        let shape = relation.shape();
        let mut aggregates = Vec::new();
        let mut windows = Vec::new();
        let resolved = resolve_expr(when, &relation, &[], &[], &mut windows, &mut aggregates, false, false)?;
        let budget = Budget::new();
        let ctx = EvalContext {
            exec: self.clone(),
            parameters: self.parameters,
            upsert_excluded: self.upsert_excluded,
            trigger_frames: self.trigger_frames,
            outer_read: &self.state.outer_read,
            subquery_cache: &self.state.subquery_cache,
            host_functions: self.host_functions,
            outer: &[],
            current: &shape,
            aggregates: &[],
            windows: &[],
            window_row: 0,
            budget: &budget,
            ctes: None,
        };
        Ok(crate::sqlite::cells::truthiness(&resolved.eval(&[], &ctx)?) == Some(true))
    }

    /// Evaluates one `VALUES` expression of a trigger body against the active
    /// `NEW` / `OLD` frame.
    pub fn eval_trigger_expression(&self, expression: &Expr, statement: &'static str) -> Result<Cell, SqlError> {
        let relation =
            Relation { sources: Vec::new(), using: Vec::new(), using_slots: Vec::new(), trigger: self.trigger_frames.last(), excluded: false };
        let shape = relation.shape();
        let mut aggregates = Vec::new();
        let mut windows = Vec::new();
        let resolved = resolve_expr(expression, &relation, &[], &[], &mut windows, &mut aggregates, false, false)
            .map_err(|error| error.in_statement(statement))?;
        let budget = Budget::new();
        let ctx = EvalContext {
            exec: self.clone(),
            parameters: self.parameters,
            upsert_excluded: self.upsert_excluded,
            trigger_frames: self.trigger_frames,
            outer_read: &self.state.outer_read,
            subquery_cache: &self.state.subquery_cache,
            host_functions: self.host_functions,
            outer: &[],
            current: &shape,
            aggregates: &[],
            windows: &[],
            window_row: 0,
            budget: &budget,
            ctes: None,
        };
        resolved.eval(&[], &ctx)
    }
}

impl crate::sqlite::dml::DmlContext<'_> {
    /// Queues the `BEFORE INSERT` event of one row of the running statement.
    pub fn queue_insert_before(&mut self, table: &str, columns: &[String], cells: Vec<Cell>) {
        self.pending_events.push(DmlRowEvent {
            table: table.to_string(),
            timing: TriggerTiming::Before,
            event: TriggerEvent::Insert,
            columns: columns.to_vec(),
            new: Some(cells),
            old: None,
        });
    }

    /// Queues the `AFTER INSERT` event of one row of the running statement.
    pub fn queue_insert_after(&mut self, table: &str, columns: &[String], cells: &[Cell]) {
        self.pending_events.push(DmlRowEvent {
            table: table.to_string(),
            timing: TriggerTiming::After,
            event: TriggerEvent::Insert,
            columns: columns.to_vec(),
            new: Some(cells.to_vec()),
            old: None,
        });
    }
}
