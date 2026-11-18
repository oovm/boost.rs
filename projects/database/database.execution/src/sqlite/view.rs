//! View DML helpers of the SQLite statement machine (C8).
//!
//! The view output expansion itself already lives in `select`
//! (`ExecContext::view_output` / `expand_view`). This module owns the two
//! shared helpers the facade's view dispatch needs: the `FROM`-shape over a
//! view's output columns, and the `INSTEAD OF` gate that decides whether a
//! view write may proceed. The view write statements keep their event firing
//! in the facade dispatch skeleton, which runs the matched trigger bodies.

use std::collections::BTreeMap;

use database_query::sqlite::{SqlError, bind::*, catalog::*, frontend::*, value::*};

/// A `FROM`-shape over one view's output, so view DML can resolve its filter
/// and assignments with the regular expression path.
pub fn view_relation<'a>(label: &str, names: &[String], trigger: Option<&'a TriggerFrame>) -> Relation<'a> {
    let width = names.len();
    Relation {
        sources: vec![Source {
            label: label.to_string(),
            columns: SourceColumns::Materialized { names: names.to_vec(), rowid: false },
            offset: 0,
            join: None,
            star_hidden: vec![false; width],
            unqualified_hidden: vec![false; width],
            star_override: vec![None; width],
        }],
        using: vec![Vec::new()],
        using_slots: Vec::new(),
        trigger,
        excluded: false,
    }
}

/// Runs the `INSTEAD OF` triggers of one view when it has any; a view
/// without one keeps SQLite's non-modifiable diagnostic.
pub fn view_trigger_gate(
    triggers: &BTreeMap<String, TriggerDef>,
    table: &str,
    key: &str,
    timing: TriggerTiming,
    event: &TriggerEvent,
) -> Result<(), SqlError> {
    if triggers
        .values()
        .any(|trigger| trigger.table == key && trigger.timing == timing && crate::sqlite::trigger::trigger_event_matches(&trigger.event, event))
    {
        return Ok(());
    }
    Err(SqlError::new("readonly-view", format!("cannot modify {table} because it is a view"))
        .in_statement(crate::sqlite::trigger::event_statement(event)))
}
