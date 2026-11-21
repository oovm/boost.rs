//! SQLite `EXPLAIN` / `EXPLAIN QUERY PLAN` 机器。
//!
//! 本模块拥有 `EXPLAIN [QUERY PLAN]` 的表存在性预检与示意行集推导:
//! 计划行走 SQLite 的四列形状,程序行走 SQLite 的八列形状,内容是本
//! 树遍历解释器的粗粒度步骤,不是真实 VDBE 反汇编。本模块只读取
//! [`ExecContext`] 的目录与附加库名表,不得反向依赖任何产品门面。

use std::collections::BTreeSet;

use database_query::sqlite::{SqlError, frontend::*, value::*};

use super::{
    ddl,
    select::{ExecContext, SystemTable},
    stmt::ExecResult,
};

/// `EXPLAIN [QUERY PLAN]` reports this engine's own execution shape.
///
/// The column headers follow SQLite, but the rows describe this tree-walking
/// interpreter, not a SQLite VDBE program. Opcode names are this engine's
/// steps, so the rows are not comparable with the real engine's disassembly.
/// `EXPLAIN QUERY PLAN` reports the accesses that will really run: an index
/// seek when this engine's own probe gates narrow the scan through an index,
/// and a plain scan otherwise. The detail strings follow SQLite's wording
/// (`SEARCH ... USING INDEX ...`) so the access path is comparable with the
/// official planner, but the rows still describe this interpreter, not a
/// SQLite VDBE program.
pub fn statement(exec: &ExecContext<'_>, query_plan: bool, statement: &Statement) -> Result<ExecResult, SqlError> {
    check_tables(exec, statement, &BTreeSet::new())?;
    let (columns, rows) = if query_plan {
        (vec!["id".to_string(), "parent".to_string(), "notused".to_string(), "detail".to_string()], self::query_plan(exec, statement))
    }
    else {
        (
            vec![
                "addr".to_string(),
                "opcode".to_string(),
                "p1".to_string(),
                "p2".to_string(),
                "p3".to_string(),
                "p4".to_string(),
                "p5".to_string(),
                "comment".to_string(),
            ],
            program(statement),
        )
    };
    Ok(ExecResult { statement: "explain", columns, rows, affected: 0 })
}

/// `EXPLAIN` still resolves a missing source or target table, matching the
/// prepare-time check the real engine performs. Column resolution is left
/// to the statement itself because the program is not executed.
pub fn check_tables(exec: &ExecContext<'_>, statement: &Statement, ctes: &BTreeSet<String>) -> Result<(), SqlError> {
    match statement {
        Statement::Select { with, first, compounds, .. } => {
            let mut scope = ctes.clone();
            for entry in with {
                scope.insert(entry.name.to_ascii_lowercase());
            }
            for table in first.from.iter().chain(compounds.iter().flat_map(|term| term.core.from.iter())) {
                check_source(exec, table, &scope)?;
            }
            Ok(())
        }
        Statement::Insert { table, query, .. } => {
            if !table_known(exec, table) {
                return Err(SqlError::new("unknown-table", format!("no such table: {table}")));
            }
            match query {
                Some(query) => check_tables(exec, query, ctes),
                None => Ok(()),
            }
        }
        Statement::Update { table, from, .. } => {
            if !table_known(exec, table) {
                return Err(SqlError::new("unknown-table", format!("no such table: {table}")));
            }
            for source in from {
                check_source(exec, source, ctes)?;
            }
            Ok(())
        }
        Statement::Delete { table, .. } => {
            if table_known(exec, table) {
                Ok(())
            }
            else {
                Err(SqlError::new("unknown-table", format!("no such table: {table}")))
            }
        }
        Statement::Explain { statement, .. } => check_tables(exec, statement, ctes),
        _ => Ok(()),
    }
}

/// One `FROM` source: a derived table recurses into its own query, a named
/// table must exist in a schema that the statement can see.
fn check_source(exec: &ExecContext<'_>, table: &TableRef, scope: &BTreeSet<String>) -> Result<(), SqlError> {
    if let Some(query) = &table.query {
        return check_tables(exec, query, scope);
    }
    if let Some(schema) = &table.schema {
        let schema = schema.to_ascii_lowercase();
        if schema != "main" && schema != "temp" && exec.attached_lookup.contains_key(&schema) {
            return Ok(());
        }
    }
    if scope.contains(&table.name.to_ascii_lowercase()) || table_known(exec, &table.name) {
        Ok(())
    }
    else {
        Err(SqlError::new("unknown-table", format!("no such table: {}", table.name)))
    }
}

/// Whether a name resolves to a table, view or system catalog table.
fn table_known(exec: &ExecContext<'_>, name: &str) -> bool {
    let key = name.to_ascii_lowercase();
    exec.catalog.table_lookup.contains_key(&key)
        || exec.catalog.temp_lookup.contains_key(&key)
        || exec.catalog.views.contains_key(&key)
        || exec.catalog.temp_views.contains_key(&key)
        || ddl::is_schema_table(name)
        || SystemTable::from_name(name).is_some()
        || key == "sqlite_sequence"
}

/// Planner rows in SQLite's four-column shape. Ids are allocated here in
/// visit order and `notused` stays zero because the real value is an
/// internal bitmap, not an address this engine can produce.
fn query_plan(exec: &ExecContext<'_>, statement: &Statement) -> Vec<Vec<Cell>> {
    let mut rows = Vec::new();
    let mut next_id = 1_i64;
    plan_rows(exec, statement, &mut rows, &mut next_id);
    rows
}

fn plan_rows(exec: &ExecContext<'_>, statement: &Statement, rows: &mut Vec<Vec<Cell>>, next_id: &mut i64) {
    match statement {
        Statement::Select { first, compounds, order, .. } => {
            plan_core(exec, first, rows, next_id);
            for term in compounds {
                plan_core(exec, &term.core, rows, next_id);
            }
            if !first.group.is_empty() {
                push_plan_row(rows, next_id, "USE TEMP B-TREE FOR GROUP BY");
            }
            if first.distinct {
                push_plan_row(rows, next_id, "USE TEMP B-TREE FOR DISTINCT");
            }
            if !order.is_empty() {
                push_plan_row(rows, next_id, "USE TEMP B-TREE FOR ORDER BY");
            }
        }
        Statement::Insert { query: Some(query), .. } => plan_rows(exec, query, rows, next_id),
        Statement::Update { table, from, .. } => {
            push_plan_row(rows, next_id, &format!("SCAN {table}"));
            for source in from {
                push_plan_row(rows, next_id, &format!("SCAN {}", table_ref_label(source)));
            }
        }
        Statement::Delete { table, .. } => {
            push_plan_row(rows, next_id, &format!("SCAN {table}"));
        }
        Statement::Explain { statement, .. } => plan_rows(exec, statement, rows, next_id),
        _ => {}
    }
}

fn plan_core(exec: &ExecContext<'_>, core: &SelectCore, rows: &mut Vec<Vec<Cell>>, next_id: &mut i64) {
    if core.from.is_empty() {
        push_plan_row(rows, next_id, "SCAN CONSTANT ROW");
    }
    let single = core.from.len() == 1;
    for table in &core.from {
        // 与执行层同一套下推门禁:只有单表 `WHERE` 真的会把这次扫描收窄到
        // 某个索引的候选行时,计划才报告索引访问;否则如实报告全表扫描。
        let access = if single { index_access(exec, table, &core.filter) } else { None };
        match access {
            Some(detail) => push_plan_row(rows, next_id, &detail),
            None => push_plan_row(rows, next_id, &format!("SCAN {}", table_ref_label(table))),
        }
    }
}

/// One index access path, or `None` when the executor would scan the table.
///
/// The gates are the executor's own (`index_probe_rows` / `index_range_rows`),
/// so the reported plan and the rows that really get read cannot disagree: a
/// poisoned key set, a non-matching affinity or a managed index falls back to
/// the scan in both places.
fn index_access(exec: &ExecContext<'_>, table: &TableRef, filter: &Option<Expr>) -> Option<String> {
    let filter = filter.as_ref()?;
    let (def, table_ref) = exec.bounded_scan_target(std::slice::from_ref(table))?;
    let label = table_ref.label();
    let table_key = def.name.to_ascii_lowercase();
    for index in exec.catalog.indexes.values() {
        if index.managed || index.table != table_key {
            continue;
        }
        let Some(position) = index.columns.first().copied()
        else {
            continue;
        };
        let column = &def.columns[position].name;
        if exec.index_probe_rows(index, def, filter, label).is_some() {
            return Some(format!("SEARCH {label} USING INDEX {} ({column}=?)", index.name));
        }
        let mut lower = None;
        let mut upper = None;
        super::probe::collect_range_bounds(filter, def, label, position, exec.parameters, &mut lower, &mut upper);
        if exec.index_range_rows(index, def, filter, label).is_some() {
            let mut terms = Vec::new();
            if let Some(bound) = &lower {
                terms.push(format!("{column}{}?", if bound.inclusive { ">=" } else { ">" }));
            }
            if let Some(bound) = &upper {
                terms.push(format!("{column}{}?", if bound.inclusive { "<=" } else { "<" }));
            }
            let shape = if terms.is_empty() { String::new() } else { format!(" ({})", terms.join(" AND ")) };
            return Some(format!("SEARCH {label} USING INDEX {}{shape}", index.name));
        }
    }
    None
}

/// Stepping program in SQLite's eight-column shape. This engine evaluates a
/// statement tree, so the rows are the coarse steps it takes: open the
/// sources, loop, produce a row, and close.
fn program(statement: &Statement) -> Vec<Vec<Cell>> {
    let mut steps: Vec<ProgramStep> = Vec::new();
    match statement {
        Statement::Select { first, compounds, order, .. } => {
            let mut cursors = 0_i64;
            let mut sources: Vec<&TableRef> = first.from.iter().collect();
            for term in compounds {
                sources.extend(term.core.from.iter());
            }
            for source in sources {
                steps.push(ProgramStep::read(cursors, table_ref_label(source)));
                cursors += 1;
            }
            if cursors > 0 {
                steps.push(ProgramStep::plain("Rewind", 0));
                steps.push(ProgramStep::plain("ResultRow", 0));
                steps.push(ProgramStep::next(0));
                for cursor in 0..cursors {
                    steps.push(ProgramStep::plain("Close", cursor));
                }
            }
            else {
                steps.push(ProgramStep::plain("ResultRow", 0));
            }
            if !order.is_empty() {
                steps.push(ProgramStep::plain("Sort", 0));
            }
        }
        Statement::Insert { table, query, .. } => {
            if let Some(query) = query {
                let _ = query;
                steps.push(ProgramStep::plain("OpenRead", 0));
                steps.push(ProgramStep::plain("Rewind", 0));
                steps.push(ProgramStep::plain("Next", 0));
            }
            steps.push(ProgramStep::write(table));
            steps.push(ProgramStep::plain("Insert", 0));
            steps.push(ProgramStep::plain("Close", 0));
        }
        Statement::Update { table, .. } | Statement::Delete { table, .. } => {
            steps.push(ProgramStep::write(table));
            steps.push(ProgramStep::plain("Rewind", 0));
            steps.push(ProgramStep::plain("Next", 0));
            steps.push(ProgramStep::plain("Close", 0));
        }
        Statement::Transaction { .. } => steps.push(ProgramStep::plain("Transaction", 0)),
        Statement::Pragma { name, .. } => steps.push(ProgramStep::named("Pragma", name)),
        Statement::Attach { name, .. } => steps.push(ProgramStep::named("Attach", name)),
        Statement::Detach { name } => steps.push(ProgramStep::named("Detach", name)),
        Statement::Explain { statement, .. } => steps.push(ProgramStep::named("Explain", statement.name())),
        _ => steps.push(ProgramStep::named("ParseSchema", statement.name())),
    }
    let halt = steps.len() as i64 + 1;
    let mut rows = Vec::with_capacity(steps.len() + 2);
    rows.push(program_row(0, "Init", 0, halt, 0, None, 0));
    for (index, step) in steps.iter().enumerate() {
        rows.push(program_row(index as i64 + 1, step.opcode, step.p1, step.p2, 0, step.p4.as_deref(), step.p5));
    }
    rows.push(program_row(halt, "Halt", 0, 0, 0, None, 0));
    rows
}

/// One step of the schematic program returned by `EXPLAIN`.
struct ProgramStep {
    opcode: &'static str,
    p1: i64,
    p2: i64,
    p4: Option<String>,
    p5: i64,
}

impl ProgramStep {
    fn plain(opcode: &'static str, p1: i64) -> Self {
        Self { opcode, p1, p2: 0, p4: None, p5: 0 }
    }

    fn next(p1: i64) -> Self {
        Self { opcode: "Next", p1, p2: 0, p4: None, p5: 1 }
    }

    fn read(cursor: i64, label: String) -> Self {
        Self { opcode: "OpenRead", p1: cursor, p2: 0, p4: Some(label), p5: 0 }
    }

    fn write(name: &str) -> Self {
        Self { opcode: "OpenWrite", p1: 0, p2: 0, p4: Some(name.to_string()), p5: 0 }
    }

    fn named(opcode: &'static str, name: &str) -> Self {
        Self { opcode, p1: 0, p2: 0, p4: Some(name.to_string()), p5: 0 }
    }
}

/// `FROM` label used by `EXPLAIN` rows. A derived table has no name, so the
/// empty label becomes `(subquery)` to stay readable.
fn table_ref_label(table: &TableRef) -> String {
    match table.label() {
        label if label.is_empty() => "(subquery)".to_string(),
        label => label.to_string(),
    }
}

fn push_plan_row(rows: &mut Vec<Vec<Cell>>, next_id: &mut i64, detail: &str) {
    rows.push(vec![Cell::Integer(*next_id), Cell::Integer(0), Cell::Integer(0), Cell::Text(detail.to_string())]);
    *next_id += 1;
}

fn program_row(addr: i64, opcode: &str, p1: i64, p2: i64, p3: i64, p4: Option<&str>, p5: i64) -> Vec<Cell> {
    vec![
        Cell::Integer(addr),
        Cell::Text(opcode.to_string()),
        Cell::Integer(p1),
        Cell::Integer(p2),
        Cell::Integer(p3),
        p4.map(|p4| Cell::Text(p4.to_string())).unwrap_or(Cell::Null),
        Cell::Integer(p5),
        Cell::Null,
    ]
}
