//! PostgreSQL `EXPLAIN` over the shared planner.
//!
//! The plan text names the operators the executor actually runs, so a client
//! reading `EXPLAIN` sees the same shape that will execute. `ANALYZE` is
//! accepted because the planner and executor both run in-process: the statement
//! executes and the root line reports the real row count.

use std::sync::Arc;

use database_query::{
    SqlCatalog, SqlCatalogBoundStatement, SqlColumn, SqlDialect, SqlExecutionPlan, SqlFieldType, SqlValue, bind_sql_catalog,
    plan_statement_for_dialect,
};

use super::session::{PgsqlSqlRuntime, TransactionSession};
use super::statement::{ExecutionOutcome, StatementError, StatementOutcome, statement_error};

/// Builds the `EXPLAIN` result set for one statement.
pub fn execute_explain(
    sql: &str,
    analyze: bool,
    runtime: Option<&Arc<PgsqlSqlRuntime>>,
    session: &mut TransactionSession,
) -> Result<StatementOutcome, StatementError> {
    if session.is_aborted() {
        return Err(StatementError::new("25P02", super::session::ABORTED_BLOCK));
    }
    let Some(runtime) = runtime
    else {
        return Err(StatementError::new("0A000", "PostgreSQL query execution is not implemented (no SQL runtime)"));
    };
    if database_query::is_ddl_statement(sql) || database_query::parse_session_command(sql).map(|command| command.is_some()).unwrap_or(false) {
        return Err(StatementError::new("0A000", "EXPLAIN only supports SELECT, INSERT, UPDATE and DELETE"));
    }
    let catalog = runtime.schema.catalog();
    let bound = bind_sql_catalog(sql, &catalog, catalog.schema_version(), SqlDialect::Postgres).map_err(|error| {
        session.fail();
        let (sqlstate, message) = statement_error("plan", error);
        StatementError::new(sqlstate, message)
    })?;
    if !matches!(
        &bound,
        SqlCatalogBoundStatement::Select(_)
            | SqlCatalogBoundStatement::Insert(_)
            | SqlCatalogBoundStatement::Update(_)
            | SqlCatalogBoundStatement::Delete(_)
            | SqlCatalogBoundStatement::SetOperation(_)
    ) {
        return Err(StatementError::new("0A000", "EXPLAIN only supports SELECT, INSERT, UPDATE and DELETE"));
    }
    let plan = plan_statement_for_dialect(bound, &catalog, SqlDialect::Postgres).map_err(|error| {
        session.fail();
        let (sqlstate, message) = statement_error("plan", error);
        StatementError::new(sqlstate, message)
    })?;
    let mut lines = plan_lines(&plan, &catalog);
    if analyze {
        let handle_ref: Option<&dyn crate::cluster::TransactionHandle> = session.transaction_handle();
        let result = crate::sql_exec::execute_plan(runtime.executor.as_ref(), handle_ref, &plan, SqlDialect::Postgres).map_err(|error| {
            session.fail();
            let (sqlstate, message) = statement_error("execution", error);
            StatementError::new(sqlstate, message)
        })?;
        if let Some(root) = lines.first_mut() {
            *root = format!("{root} (actual rows={})", result.rows.len());
        }
    }
    let description = vec![SqlColumn { name: "QUERY PLAN".to_owned(), field_type: SqlFieldType::String, field_id: None }];
    let rows = lines.into_iter().map(|line| vec![SqlValue::String(line)]).collect();
    Ok(StatementOutcome::Result(ExecutionOutcome { is_read: true, description, rows, command_tag: "EXPLAIN".to_owned() }))
}

fn plan_lines(plan: &SqlExecutionPlan, catalog: &SqlCatalog) -> Vec<String> {
    match plan {
        SqlExecutionPlan::Read(read) => vec![format!("Index Scan using primary key on {}", table_name(catalog, &read.namespace))],
        SqlExecutionPlan::Scan(scan) => {
            let mut lines = Vec::new();
            let table = table_name(catalog, &scan.namespace);
            if scan.group_by.is_some() {
                lines.push("HashAggregate".to_owned());
            }
            lines.push(if scan.index.is_some() {
                format!("Index Scan on {table}")
            }
            else {
                format!("Seq Scan on {table}")
            });
            if scan.remaining_predicate.is_some() {
                lines.push("  Filter: (row predicate)".to_owned());
            }
            if scan.order_by.is_some() {
                lines.push("Sort".to_owned());
            }
            lines
        }
        SqlExecutionPlan::Join(join) => {
            let mut lines = vec![if join.has_aggregate { "HashAggregate".to_owned() } else { "Nested Loop".to_owned() }];
            lines.push(format!("  -> Seq Scan on {}", table_name(catalog, &join.base.namespace)));
            for source in &join.joins {
                let label = match source.kind {
                    database_query::SqlBoundJoinKind::Inner | database_query::SqlBoundJoinKind::Cross => "Seq Scan",
                    database_query::SqlBoundJoinKind::Left => "Hash Left Join",
                    database_query::SqlBoundJoinKind::Right => "Hash Right Join",
                    database_query::SqlBoundJoinKind::Full => "Hash Full Join",
                };
                lines.push(format!("  -> {label} on {}", table_name(catalog, &source.namespace)));
            }
            if join.order_by.is_some() {
                lines.push("Sort".to_owned());
            }
            lines
        }
        SqlExecutionPlan::Scalar(_) => vec!["Result".to_owned()],
        SqlExecutionPlan::SetOperation(set) => {
            let mut lines = vec!["Append".to_owned()];
            for operand in &set.operands {
                for line in plan_lines(operand, catalog) {
                    lines.push(format!("  {line}"));
                }
            }
            if set.order_by.is_some() {
                lines.push("Sort".to_owned());
            }
            lines
        }
        SqlExecutionPlan::Insert(write) => vec![format!("Insert on {}", table_name(catalog, &write.namespace))],
        SqlExecutionPlan::MultiInsert(writes) => {
            let table = writes.first().map(|write| table_name(catalog, &write.namespace)).unwrap_or_default();
            vec![format!("Insert on {table} ({} rows)", writes.len())]
        }
        SqlExecutionPlan::InsertSelect(plan) => {
            let mut lines = plan_lines(&plan.source, catalog);
            lines.push(format!("Insert on {}", table_name(catalog, &plan.namespace)));
            lines
        }
        SqlExecutionPlan::Update(update) => vec![format!("Update on {} (primary key)", table_name(catalog, &update.namespace))],
        SqlExecutionPlan::UpdateScan(update) => {
            let mut lines = vec![format!("Seq Scan on {}", table_name(catalog, &update.namespace))];
            lines.push("Update".to_owned());
            lines
        }
        SqlExecutionPlan::Delete(delete) => vec![format!("Delete on {} (primary key)", table_name(catalog, &delete.namespace))],
        SqlExecutionPlan::DeleteScan(delete) => {
            let mut lines = vec![format!("Seq Scan on {}", table_name(catalog, &delete.namespace))];
            lines.push("Delete".to_owned());
            lines
        }
    }
}

/// Resolves a `table:{type_id}` namespace to its catalog name.
fn table_name(catalog: &SqlCatalog, namespace: &str) -> String {
    let type_id = namespace.strip_prefix("table:").and_then(|tail| tail.parse::<u64>().ok());
    type_id
        .and_then(|type_id| catalog.tables().iter().find(|table| table.type_id == type_id).map(|table| table.name.clone()))
        .unwrap_or_else(|| namespace.to_owned())
}
