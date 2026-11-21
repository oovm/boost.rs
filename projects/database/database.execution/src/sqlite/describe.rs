//! SQLite 预编译期语句元数据(describe)机器:从目录推导结果列标签,绝不执行语句。
//!
//! 本模块拥有 schema 作用域解析完成后的全部推导:SELECT 与 `WITH` 前言、
//! 复合体、`FROM` 源(派生表 / 视图 / CTE / 系统表 / 附加库回退)、DML 目标
//! 与 `RETURNING`、读形 `PRAGMA` 表头与 `EXPLAIN` 预检。附加库槽位以
//! [`ExecContext::attached`] 的只读嵌套上下文进入,本模块不拥有 attach 编排,
//! 也不得反向依赖任何产品门面。

use std::collections::{BTreeMap, BTreeSet};

use database_query::sqlite::{SqlError, bind::*, catalog::*, frontend::*};
use database_storage::sqlite::{SchemaObjectKind, TableLimits};

use super::{
    explain, pragma,
    select::{
        ExecContext, SystemTable, TableScope, ViewDepthGuard, compound_order_keys, core_output_labels, storage_error, system_for_scope,
        unique_output_names, view_output_names,
    },
    stmt,
    window::limit_integer,
};

/// 在已解析出的 schema 作用域内推导一条语句的结果列标签。
pub fn statement(exec: &ExecContext<'_>, statement: &Statement, scope: TableScope) -> Result<Option<Vec<String>>, SqlError> {
    match statement {
        Statement::Select { .. } => Ok(Some(select_statement(exec, statement, None)?)),
        Statement::Explain { query_plan, statement } => {
            explain::check_tables(exec, statement, &BTreeSet::new())?;
            Ok(Some(if *query_plan {
                vec!["id".to_string(), "parent".to_string(), "notused".to_string(), "detail".to_string()]
            }
            else {
                vec![
                    "addr".to_string(),
                    "opcode".to_string(),
                    "p1".to_string(),
                    "p2".to_string(),
                    "p3".to_string(),
                    "p4".to_string(),
                    "p5".to_string(),
                    "comment".to_string(),
                ]
            }))
        }
        Statement::Pragma { schema, name, value } => pragma_labels(exec, schema.as_deref(), name, value.as_deref()),
        Statement::Insert { schema: _, table, query, returning, .. } => {
            stmt::check_readonly_table(table, "insert")?;
            let key = table.to_ascii_lowercase();
            if let Some(query) = query {
                select_statement(exec, query, None)?;
            }
            if exec.catalog.views.contains_key(&key) {
                return Ok(None);
            }
            let def = target_def(exec, table, &key, scope, "insert")?;
            match returning {
                Some(items) => Ok(Some(TargetShape::new(&def, table, None).plan(&def, items, "insert")?.columns)),
                None => Ok(None),
            }
        }
        Statement::Update { schema: _, table, alias, assignments, from, filter, returning } => {
            stmt::check_readonly_table(table, "update")?;
            let key = table.to_ascii_lowercase();
            if exec.catalog.views.contains_key(&key) {
                if !from.is_empty() {
                    let mut error =
                        SqlError::new("unsupported-clause", "`UPDATE ... FROM` is not supported on a view; name a base table instead");
                    error.clause = Some("FROM".to_string());
                    return Err(error.in_statement("update"));
                }
                if returning.is_some() {
                    return Err(SqlError::new("unsupported-clause", "RETURNING on an INSTEAD OF view update is not supported yet")
                        .in_statement("update"));
                }
                return Ok(None);
            }
            let def = target_def(exec, table, &key, scope, "update")?;
            let shape = update_shape(exec, &def, table, alias.as_deref(), from)?;
            for assignment in assignments {
                if !def.columns.iter().any(|column| column.name.eq_ignore_ascii_case(&assignment.column)) {
                    return Err(SqlError::new("unknown-column", format!("unknown column `{}` in table `{table}`", assignment.column))
                        .in_statement("update"));
                }
                shape.resolve(&assignment.value, "update")?;
            }
            shape.resolve_optional(filter.as_ref(), "update")?;
            match returning {
                Some(items) => Ok(Some(TargetShape::new(&def, table, None).plan(&def, items, "update")?.columns)),
                None => Ok(None),
            }
        }
        Statement::Delete { schema: _, table, alias, filter, returning } => {
            stmt::check_readonly_table(table, "delete")?;
            let key = table.to_ascii_lowercase();
            if exec.catalog.views.contains_key(&key) {
                if returning.is_some() {
                    return Err(SqlError::new("unsupported-clause", "RETURNING on an INSTEAD OF view delete is not supported yet")
                        .in_statement("delete"));
                }
                return Ok(None);
            }
            let def = target_def(exec, table, &key, scope, "delete")?;
            let shape = TargetShape::new(&def, alias.as_deref().unwrap_or(table), None);
            shape.resolve_optional(filter.as_ref(), "delete")?;
            match returning {
                Some(items) => Ok(Some(TargetShape::new(&def, table, None).plan(&def, items, "delete")?.columns)),
                None => Ok(None),
            }
        }
        _ => Ok(None),
    }
}

/// 定位一条 DML 的目标表定义,镜像执行路径的查找顺序:temp 先于 main,
/// `main.`/`temp.` 作用域只查各自一侧。返回克隆的定义,让调用方在只读
/// 借用目录的同时安全持有它。
fn target_def(exec: &ExecContext<'_>, table: &str, key: &str, scope: TableScope, statement: &'static str) -> Result<TableDef, SqlError> {
    let temp_position = if scope == TableScope::Main { None } else { exec.catalog.temp_lookup.get(key).copied() };
    let main_position = if scope == TableScope::Temp {
        None
    }
    else {
        match temp_position {
            Some(_) => None,
            None => Some(
                *exec
                    .catalog
                    .table_lookup
                    .get(key)
                    .ok_or_else(|| SqlError::new("unknown-table", format!("unknown table `{table}`")).in_statement(statement))?,
            ),
        }
    };
    Ok(match temp_position {
        Some(position) => exec.catalog.temp_tables[position].def.clone(),
        None => exec.catalog.tables[main_position.expect("main table resolved")].clone(),
    })
}

/// 构造 `UPDATE ... FROM` 解析赋值与谓词所用的 `FROM` 形状,镜像执行期
/// 的关系;目标表以目录列名进入,派生 `FROM` 源只推导名字。
fn update_shape<'a>(
    exec: &ExecContext<'_>,
    def: &'a TableDef,
    table: &str,
    alias: Option<&str>,
    from: &[TableRef],
) -> Result<TargetShape<'a>, SqlError> {
    if from.is_empty() {
        return Ok(TargetShape::new(def, alias.unwrap_or(table), None));
    }
    let width = def.columns.len();
    let mut sources = vec![Source {
        label: alias.unwrap_or(table).to_string(),
        columns: SourceColumns::Materialized {
            names: def.columns.iter().map(|column| column.name.clone()).collect(),
            rowid: !def.without_rowid,
        },
        offset: 0,
        join: None,
        star_hidden: vec![false; width],
        unqualified_hidden: vec![false; width],
        star_override: vec![None; width],
    }];
    let mut offset = width + usize::from(!def.without_rowid);
    for entry in from {
        let (names, rowid) = source(exec, entry, None)?;
        let visible = names.len();
        sources.push(Source {
            label: entry.label().to_string(),
            columns: SourceColumns::Materialized { names, rowid },
            offset,
            join: entry.join,
            star_hidden: vec![false; visible],
            unqualified_hidden: vec![false; visible],
            star_override: vec![None; visible],
        });
        offset += visible + usize::from(rowid);
    }
    let source_count = sources.len();
    let relation = Relation { sources, using: vec![Vec::new(); source_count], using_slots: Vec::new(), trigger: None, excluded: false };
    let scope = relation.shape();
    Ok(TargetShape { relation, scope })
}

/// 推导一条读形 `PRAGMA` 的静态表头;写形 `PRAGMA` 没有行集结果,如实
/// 返回 `None`。`table_info`/`table_xinfo` 指向视图时镜像执行期的
/// `view_output` 校验:视图体推导失败(缺列、宽度不符)即整体失败。
fn pragma_labels(exec: &ExecContext<'_>, schema: Option<&str>, name: &str, value: Option<&str>) -> Result<Option<Vec<String>>, SqlError> {
    if name.eq_ignore_ascii_case("database_list") {
        return Ok(Some(vec!["seq".to_string(), "name".to_string(), "file".to_string()]));
    }
    let argument = value.unwrap_or_default();
    match name.to_ascii_lowercase().as_str() {
        "table_info" | "table_xinfo" => {
            let extended = name.eq_ignore_ascii_case("table_xinfo");
            if pragma::table_def(exec.catalog, schema, argument).is_none() {
                if let Some(view) = pragma::view_def(exec.catalog, schema, argument) {
                    let _depth = ViewDepthGuard::enter()?;
                    let actual = select_statement(exec, &view.query, None)?;
                    view_output_names(view, &actual)?;
                }
            }
            let mut columns =
                vec!["cid".to_string(), "name".to_string(), "type".to_string(), "notnull".to_string(), "dflt_value".to_string(), "pk".to_string()];
            if extended {
                columns.push("hidden".to_string());
            }
            Ok(Some(columns))
        }
        "index_list" => Ok(Some(vec!["seq".to_string(), "name".to_string(), "unique".to_string(), "origin".to_string(), "partial".to_string()])),
        "index_info" => Ok(Some(vec!["seqno".to_string(), "cid".to_string(), "name".to_string()])),
        "foreign_key_list" => Ok(Some(vec![
            "id".to_string(),
            "seq".to_string(),
            "table".to_string(),
            "from".to_string(),
            "to".to_string(),
            "on_update".to_string(),
            "on_delete".to_string(),
            "match".to_string(),
        ])),
        "integrity_check" => Ok(Some(vec!["integrity_check".to_string()])),
        "user_version" | "application_id" | "foreign_keys" | "recursive_triggers" => {
            Ok(if argument.is_empty() { Some(vec![name.to_string()]) } else { None })
        }
        _ => Err(SqlError::new(
            "unsupported-pragma",
            format!(
                "unsupported pragma `{name}`; this engine supports `foreign_keys`, `recursive_triggers`, `table_info`, `table_xinfo`, `index_list`, `index_info`, `foreign_key_list`, `database_list`, `user_version`, `application_id` and `integrity_check`"
            ),
        )
        .in_clause("PRAGMA")
        .in_statement("pragma")),
    }
}

/// 推导一条已解析 `SELECT`(含其 `WITH` 前言)的结果列标签。
/// `env` 是外层已物化的 CTE 名字表,与执行期的 `CteEnv` 平行:派生表
/// 能看见外层 CTE,视图体看不见(镜像执行路径的传参)。
fn select_statement(exec: &ExecContext<'_>, statement: &Statement, env: Option<&DescribeEnv>) -> Result<Vec<String>, SqlError> {
    let Statement::Select { with, first, compounds, order, limit, .. } = statement
    else {
        return Err(SqlError::new("unsupported-statement", format!("a subquery must be a `SELECT` but found `{}`", statement.name()))
            .in_statement("select"));
    };
    if with.is_empty() {
        return select_body(exec, first, compounds, order, limit.as_ref(), env);
    }
    let mut owned = DescribeEnv { parent: env, tables: BTreeMap::new() };
    for entry in with {
        let columns = match exec.recursive_start(entry)? {
            // 递归项:镜像 `materialize_recursive` 的种子推导——种子列
            // 即 CTE 列,且不做去重改名(执行路径同样没有)。
            Some(start) => {
                let Statement::Select { with: inner_with, first, compounds, .. } = entry.query.as_ref()
                else {
                    return Err(SqlError::new("unsupported-statement", "a `WITH` body must be a `SELECT`").in_statement("select"));
                };
                let seed = Statement::Select {
                    with: inner_with.clone(),
                    recursive: false,
                    first: first.clone(),
                    compounds: compounds[..start].to_vec(),
                    order: Vec::new(),
                    limit: None,
                };
                let seed_columns = select_statement(exec, &seed, Some(&owned))?;
                if let Some(names) = &entry.columns {
                    if names.len() != seed_columns.len() {
                        let mut error = SqlError::new(
                            "arity-mismatch",
                            format!("table {} has {} values for {} columns", entry.name, seed_columns.len(), names.len()),
                        )
                        .in_statement("select");
                        error.clause = Some("WITH".to_string());
                        return Err(error);
                    }
                }
                entry.columns.clone().unwrap_or(seed_columns)
            }
            None => {
                let result_columns = select_statement(exec, &entry.query, Some(&owned))?;
                if let Some(names) = &entry.columns {
                    if names.len() != result_columns.len() {
                        let mut error = SqlError::new(
                            "arity-mismatch",
                            format!("table {} has {} values for {} columns", entry.name, result_columns.len(), names.len()),
                        )
                        .in_statement("select");
                        error.clause = Some("WITH".to_string());
                        return Err(error);
                    }
                }
                unique_output_names(&entry.columns.clone().unwrap_or(result_columns))
            }
        };
        owned.tables.insert(entry.name.to_ascii_lowercase(), columns);
    }
    select_body(exec, first, compounds, order, limit.as_ref(), Some(&owned))
}

/// 推导一个 `SELECT` 语句体(可能复合)的结果列标签。`LIMIT` 表达式按
/// 执行路径 `eval_limit` 的同一位置、同一关系求名字:只解析
/// 不求值,字面量才做整数转换检查,避免预编译期执行任何子查询。
fn select_body(
    exec: &ExecContext<'_>,
    first: &SelectCore,
    compounds: &[CompoundTerm],
    order: &[OrderKey],
    limit: Option<&Limit>,
    env: Option<&DescribeEnv>,
) -> Result<Vec<String>, SqlError> {
    if let Some(limit) = limit {
        let relation = Relation { sources: Vec::new(), using: Vec::new(), using_slots: Vec::new(), trigger: None, excluded: false };
        let mut aggregates = Vec::new();
        let mut windows = Vec::new();
        if let Some(offset) = &limit.offset {
            let resolved = resolve_expr(offset, &relation, &[], &[], &mut windows, &mut aggregates, false, false)?;
            if let ResolvedExpr::Cell(cell) = &resolved {
                limit_integer(cell, "OFFSET")?;
            }
        }
        let resolved = resolve_expr(&limit.count, &relation, &[], &[], &mut windows, &mut aggregates, false, false)?;
        if let ResolvedExpr::Cell(cell) = &resolved {
            limit_integer(cell, "LIMIT")?;
        }
    }
    if compounds.is_empty() {
        return core(exec, first, order, env);
    }
    let columns = core(exec, first, &[], env)?;
    for term in compounds {
        let right = core(exec, &term.core, &[], env)?;
        if right.len() != columns.len() {
            let mut error = SqlError::new(
                "arity-mismatch",
                format!("SELECTs to the left and right of {} do not have the same number of result columns", term.op.text()),
            )
            .in_statement("select");
            error.clause = Some(term.op.clause().to_string());
            error.position = Some(term.position);
            return Err(error);
        }
    }
    if !order.is_empty() {
        compound_order_keys(order, &columns)?;
    }
    Ok(columns)
}

/// 推导一个 `SELECT` 核心的结果列标签。
///
/// 与执行路径 `select` 同一顺序:先按目录构造每个 `FROM` 源
/// 的名字表,再规划 `USING`/`NATURAL` 合并,然后解析 `ON`、投影、
/// `WHERE`、`GROUP BY`、`HAVING`、`ORDER BY`——任何一处名字解析失败即
/// 整体失败(执行路径同样在读行之前全部解析,坏名字 fail-closed)。
/// 标签由 [`core_output_labels`] 产出,与执行期共用。派生表、视图与
/// CTE 只递归推导名字,不物化任何行。
fn core(exec: &ExecContext<'_>, core: &SelectCore, order: &[OrderKey], env: Option<&DescribeEnv>) -> Result<Vec<String>, SqlError> {
    let SelectCore { distinct: _, items, from, filter, group, having, windows: named } = core;
    let mut sources = Vec::with_capacity(from.len());
    let mut offset = 0;
    for table in from {
        let (names, rowid) = source(exec, table, env)?;
        let width = names.len();
        sources.push(Source {
            label: table.label().to_string(),
            columns: SourceColumns::Materialized { names, rowid },
            offset,
            join: table.join,
            star_hidden: vec![false; width],
            unqualified_hidden: vec![false; width],
            star_override: vec![None; width],
        });
        offset += width + usize::from(rowid);
    }
    let mut relation = Relation { sources, using: vec![Vec::new(); from.len()], using_slots: Vec::new(), trigger: None, excluded: false };
    relation.plan_joins(from)?;
    for (index, table) in from.iter().enumerate() {
        if let Some(on) = &table.on {
            let mut scratch_windows = Vec::new();
            let mut scratch_aggregates = Vec::new();
            let resolved = resolve_expr(on, &relation, &[], &[], &mut scratch_windows, &mut scratch_aggregates, false, false)?;
            let mut refs = Vec::new();
            resolved.collect_sources(&mut refs);
            if refs.iter().any(|source| *source > index) {
                return Err(SqlError::new(
                    "unknown-column",
                    format!("the `ON` clause of source {} may not reference a later source", index + 1),
                )
                .in_statement("select"));
            }
        }
    }
    let mut specs = Vec::new();
    let mut windows: Vec<ResolvedWindow> = Vec::new();
    if let SelectItems::Named(exprs) = items {
        for expr in exprs {
            resolve_expr(expr, &relation, &[], named, &mut windows, &mut specs, true, true)?;
        }
    }
    let labels = core_output_labels(&relation, items);
    if let Some(predicate) = filter {
        let mut scratch_windows = Vec::new();
        let mut scratch_aggregates = Vec::new();
        resolve_expr(predicate, &relation, &[], &[], &mut scratch_windows, &mut scratch_aggregates, false, false)?;
    }
    for expr in group {
        if let Expr::Literal(Literal::Integer(position)) = expr {
            let position = *position;
            if position < 1 || position as usize > labels.len() {
                return Err(SqlError::new(
                    "order-term-out-of-range",
                    format!("GROUP BY term out of range - should be between 1 and {}", labels.len()),
                )
                .in_statement("select"));
            }
            continue;
        }
        let mut scratch_windows = Vec::new();
        let mut scratch_aggregates = Vec::new();
        resolve_expr(expr, &relation, &[], &[], &mut scratch_windows, &mut scratch_aggregates, false, false)?;
    }
    if let Some(having_expr) = having {
        resolve_expr(having_expr, &relation, &[], named, &mut windows, &mut specs, true, false)?;
    }
    if having.is_some() && group.is_empty() && specs.is_empty() {
        let mut error = SqlError::new("misuse-of-aggregate", "HAVING clause on a non-aggregate query").in_statement("select");
        error.clause = Some("HAVING".to_string());
        return Err(error);
    }
    for entry in order {
        if let Expr::Literal(Literal::Integer(position)) = &entry.expr {
            let position = *position;
            if position < 1 || position as usize > labels.len() {
                return Err(SqlError::new(
                    "order-term-out-of-range",
                    format!("ORDER BY term out of range - should be between 1 and {}", labels.len()),
                )
                .in_statement("select"));
            }
            continue;
        }
        if let Expr::Column(column) = &entry.expr {
            if column.qualifier.is_none() && labels.iter().any(|label| label.eq_ignore_ascii_case(&column.name)) {
                continue;
            }
        }
        resolve_expr(&entry.expr, &relation, &[], named, &mut windows, &mut specs, true, true)?;
    }
    Ok(labels)
}

/// 推导一个 `FROM` 源暴露的列名与隐藏 rowid 标志,镜像执行路径
/// `select_source` 的解析顺序:派生表 → schema 限定 → CTE →
/// 系统表 → temp 视图 → 视图 → temp 表 → main 表 → 附加库回退。
/// 每一步只取名字:派生表与视图递归 [`select_statement`],
/// 基础表直接读目录定义,系统表用静态表头。
fn source(exec: &ExecContext<'_>, table: &TableRef, env: Option<&DescribeEnv>) -> Result<(Vec<String>, bool), SqlError> {
    if let Some(query) = &table.query {
        let columns = select_statement(exec, query, env)?;
        return Ok((unique_output_names(&columns), false));
    }
    let scope = match table.schema.as_deref() {
        None => Some(TableScope::Default),
        Some(qualifier) => {
            let lower = qualifier.to_ascii_lowercase();
            if lower == "main" {
                Some(TableScope::Main)
            }
            else if lower == "temp" {
                Some(TableScope::Temp)
            }
            else if exec.alias.as_deref().is_some_and(|alias| alias.eq_ignore_ascii_case(qualifier)) {
                Some(TableScope::Default)
            }
            else {
                None
            }
        }
    };
    let Some(scope) = scope
    else {
        let qualifier = table.schema.as_deref().unwrap_or_default();
        let index = match exec.attached_lookup.get(&qualifier.to_ascii_lowercase()).copied() {
            Some(index) => index,
            None => {
                return Err(SqlError::new("unknown-table", format!("no such table: {qualifier}.{}", table.name)).in_statement("select"));
            }
        };
        return attached_source(exec, index, table);
    };
    if table.schema.is_none() {
        if let Some(names) = env.and_then(|env| env.lookup(&table.name)) {
            return Ok((names.clone(), false));
        }
    }
    if let Some(system) = system_for_scope(scope, &table.name) {
        return system_rows(exec, system, &table.name);
    }
    let key = table.name.to_ascii_lowercase();
    if scope != TableScope::Main {
        if let Some(view) = exec.catalog.temp_views.get(&key) {
            let _depth = ViewDepthGuard::enter()?;
            let actual = select_statement(exec, &view.def.query, None)?;
            return Ok((view_output_names(&view.def, &actual)?, false));
        }
    }
    if scope != TableScope::Temp {
        if let Some(view) = exec.catalog.views.get(&key) {
            let _depth = ViewDepthGuard::enter()?;
            let actual = select_statement(exec, &view.query, None)?;
            return Ok((view_output_names(view, &actual)?, false));
        }
    }
    if scope != TableScope::Main {
        if let Some(position) = exec.catalog.temp_lookup.get(&key).copied() {
            let def = &exec.catalog.temp_tables[position].def;
            return Ok((def.columns.iter().map(|column| column.name.clone()).collect(), !def.without_rowid));
        }
    }
    if scope != TableScope::Temp {
        if let Some(position) = exec.catalog.table_lookup.get(&key).copied() {
            let def = &exec.catalog.tables[position];
            return Ok((def.columns.iter().map(|column| column.name.clone()).collect(), !def.without_rowid));
        }
    }
    if table.schema.is_none() {
        for index in 0..exec.attached.len() {
            if let Ok(source) = attached_source(exec, index, table) {
                return Ok(source);
            }
        }
    }
    let message = match table.schema.as_deref() {
        None => format!("unknown table `{}`", table.name),
        Some(qualifier) => format!("no such table: {qualifier}.{}", table.name),
    };
    Err(SqlError::new("unknown-table", message).in_statement("select"))
}

/// 推导附加数据库中一个 `FROM` 源的名字表,镜像执行路径的 `attached_source`。
fn attached_source(exec: &ExecContext<'_>, index: usize, table: &TableRef) -> Result<(Vec<String>, bool), SqlError> {
    let engine = &exec.attached[index];
    if let Some(system) = SystemTable::from_name(&table.name) {
        return system_rows(engine, system, &table.name);
    }
    let key = table.name.to_ascii_lowercase();
    if let Some(view) = engine.catalog.temp_views.get(&key) {
        let _depth = ViewDepthGuard::enter()?;
        let actual = select_statement(engine, &view.def.query, None)?;
        return Ok((view_output_names(&view.def, &actual)?, false));
    }
    if let Some(view) = engine.catalog.views.get(&key) {
        let _depth = ViewDepthGuard::enter()?;
        let actual = select_statement(engine, &view.query, None)?;
        return Ok((view_output_names(view, &actual)?, false));
    }
    if let Some(position) = engine.catalog.table_lookup.get(&key).copied() {
        let def = &engine.catalog.tables[position];
        return Ok((def.columns.iter().map(|column| column.name.clone()).collect(), !def.without_rowid));
    }
    let qualifier = table.schema.as_deref().unwrap_or_default();
    Err(SqlError::new("unknown-table", format!("no such table: {qualifier}.{}", table.name)).in_statement("select"))
}

/// 系统表的静态列名与隐藏 rowid 标志,镜像执行路径 `system_rows` 的
/// 名字部分:`sqlite_sequence` 只在目录里真的存在时可用,与执行期一致。
fn system_rows(exec: &ExecContext<'_>, system: SystemTable, name: &str) -> Result<(Vec<String>, bool), SqlError> {
    match system {
        SystemTable::Master | SystemTable::TempMaster => {
            Ok((vec!["type".to_string(), "name".to_string(), "tbl_name".to_string(), "rootpage".to_string(), "sql".to_string()], true))
        }
        SystemTable::Sequence => {
            let present = exec
                .writer
                .schema_objects(TableLimits::query())
                .map_err(storage_error)?
                .iter()
                .any(|object| object.kind == SchemaObjectKind::Table && object.name.eq_ignore_ascii_case(name));
            if !present {
                return Err(SqlError::new("unknown-table", format!("no such table: {name}")).in_statement("select"));
            }
            Ok((vec!["name".to_string(), "seq".to_string()], true))
        }
    }
}

/// 一个 `WITH` 条目在 describe 路径上的名字表,与执行期的 `CteEnv` 平行但
/// 只携带列名,不携带行。链式父作用域镜像 CTE 作用域的嵌套可见性。
struct DescribeEnv<'a> {
    parent: Option<&'a DescribeEnv<'a>>,
    tables: BTreeMap<String, Vec<String>>,
}

impl<'a> DescribeEnv<'a> {
    fn lookup(&self, name: &str) -> Option<&Vec<String>> {
        self.tables.get(&name.to_ascii_lowercase()).or_else(|| self.parent.and_then(|parent| parent.lookup(name)))
    }
}
