//! Lower VOS expression pipelines into query ops.
//!
//! The read surface mirrors the shared `QueryOp` IR that the local SQL
//! frontend already validated: pipelines end in `.collect()`, table-level
//! execution boundaries (`get` / `first` / `any` / `count`) produce zero-or-
//! one-row results, predicates keep the two-valued field-comparison semantics
//! for plain field paths and fall back to the three-valued expression form for
//! computed operands, and projections carry field paths, `x.*` (identity) and
//! computed expressions.

use data_text_vos::{VosBinaryOp, VosExpr, VosFieldInit, VosLiteral, VosProgram, VosProjItem, VosStmt, VosUnaryOp};

use database_types::{Error, Result, Value};

use super::{
    ops::{
        AggregateField, BinaryOp, BoundValue, CmpOp, CountExpr, DateExtractField, Expr, JoinKind, Pred, ProjectExpr, ProjectField, QueryOp,
        ScalarFunction, SortKey, TextOp,
    },
    params::ParameterDescriptor,
};

fn query_error(message: impl Into<String>) -> Error {
    Error::Schema { message: message.into() }
}

/// Lower a VOS program into an ordered physical op list.
///
/// `parameters` is the ordered descriptor list produced by
/// [`super::params::prepare`]; lowering converts `Name("__yydb_param_<idx>")`
/// Identifier nodes directly into `BoundValue::Slot` references.
pub fn lower_program(program: &VosProgram, parameters: &[ParameterDescriptor]) -> Result<Vec<QueryOp>> {
    let mut bindings: Vec<(String, VosExpr)> = Vec::new();
    for stmt in &program.statements {
        match stmt {
            VosStmt::Let(let_) => bindings.push((let_.name.clone(), let_.value.clone())),
            VosStmt::Expr(expr) => return lower_exec_expr(expr, &bindings, parameters),
        }
    }
    let Some(result) = &program.result
    else {
        return Err(query_error("query program has no result expression"));
    };
    lower_exec_expr(result, &bindings, parameters)
}

fn lower_exec_expr(expr: &VosExpr, bindings: &[(String, VosExpr)], parameters: &[ParameterDescriptor]) -> Result<Vec<QueryOp>> {
    let expr = expand_bindings(expr, bindings)?;
    if let Some(ops) = lower_boundary(&expr, parameters)? {
        return Ok(ops);
    }
    if let Some(ops) = lower_terminal_aggregate(&expr, parameters)? {
        return Ok(ops);
    }
    let pipeline = split_collect(&expr)?;
    let mut ops = lower_pipeline(&pipeline, parameters)?;
    ops.push(QueryOp::Collect);
    Ok(ops)
}

/// Table-level execution boundaries: `Type.get(id)`, `Type.first(pred)`,
/// `Type.any(pred)`, `Type.count(pred)` and the whole-table aggregates
/// `Type.sum(x => x.field)` / `avg` / `min` / `max` / `array_agg`.
///
/// Each lowers to the pipeline that yields its result rows: `get` is a
/// primary-key point lookup, `first` a filtered one-row take, `count` a
/// whole-table count row, `any` that count projected to a boolean, and the
/// aggregates a single global aggregate row named after the method.
fn lower_boundary(expr: &VosExpr, parameters: &[ParameterDescriptor]) -> Result<Option<Vec<QueryOp>>> {
    let VosExpr::Call { callee, args, .. } = expr else {
        return Ok(None);
    };
    let VosExpr::Member { object, name, .. } = callee.as_ref() else {
        return Ok(None);
    };
    // Boundaries are spelled on the table itself, never on a pipeline.
    let VosExpr::Name { name: table, .. } = object.as_ref() else {
        return Ok(None);
    };
    let count_field = || AggregateField::Count { name: "count".to_string(), expr: None };
    // `get` has its own exact-arity check below; the remaining boundaries take
    // at most one predicate. Silently dropping a second predicate would run a
    // different query than the author wrote.
    if matches!(name.as_str(), "first" | "any" | "count") && args.len() > 1 {
        return Err(query_error(format!("`{name}` takes at most one predicate")));
    }
    match name.as_str() {
        "get" => {
            let Some(arg) = args.first() else {
                return Err(query_error("`get` requires the primary-key value"));
            };
            if args.len() != 1 {
                return Err(query_error("`get` takes exactly one primary-key value"));
            }
            let value = lower_bound_value(arg, parameters)?;
            Ok(Some(vec![QueryOp::Get { table: table.clone(), value }]))
        }
        "first" => {
            let mut ops = vec![QueryOp::Scan { table: table.clone() }];
            if let Some(pred) = args.first() {
                ops.push(QueryOp::Filter { predicate: lower_predicate(pred, parameters)? });
            }
            ops.push(QueryOp::Take { count: CountExpr::Bound(BoundValue::Literal(Value::I64(1))) });
            Ok(Some(ops))
        }
        "any" => {
            let mut ops = vec![QueryOp::Scan { table: table.clone() }];
            if let Some(pred) = args.first() {
                ops.push(QueryOp::Filter { predicate: lower_predicate(pred, parameters)? });
            }
            ops.push(QueryOp::Aggregate { group_keys: vec![], aggregates: vec![count_field()] });
            ops.push(QueryOp::Project {
                fields: vec![ProjectField {
                    name: "any".to_string(),
                    expr: ProjectExpr::Computed(Box::new(Expr::Binary {
                        op: BinaryOp::Gt,
                        left: Box::new(Expr::Column { path: vec!["count".to_string()] }),
                        right: Box::new(Expr::Bound(BoundValue::Literal(Value::I64(0)))),
                    })),
                }],
            });
            Ok(Some(ops))
        }
        "count" => {
            let mut ops = vec![QueryOp::Scan { table: table.clone() }];
            if let Some(pred) = args.first() {
                ops.push(QueryOp::Filter { predicate: lower_predicate(pred, parameters)? });
            }
            ops.push(QueryOp::Aggregate { group_keys: vec![], aggregates: vec![count_field()] });
            Ok(Some(ops))
        }
        "sum" | "avg" | "min" | "max" | "array_agg" => {
            let field = terminal_aggregate(name, args, parameters)?;
            Ok(Some(vec![
                QueryOp::Scan { table: table.clone() },
                QueryOp::Aggregate { group_keys: vec![], aggregates: vec![field] },
            ]))
        }
        _ => Ok(None),
    }
}

/// Aggregates lowering accepts on a whole pipeline, e.g.
/// `Account.all().filter(...).sum(a => a.score)`.
fn is_aggregate_method(name: &str) -> bool {
    matches!(name, "sum" | "avg" | "min" | "max" | "array_agg" | "count")
}

/// Lower a terminal aggregate whose receiver is a pipeline rather than a bare
/// table name.
///
/// The receiver lowers exactly like any other read pipeline (minus the final
/// `.collect()`), and the aggregate folds the rows it produces into one row.
fn lower_terminal_aggregate(expr: &VosExpr, parameters: &[ParameterDescriptor]) -> Result<Option<Vec<QueryOp>>> {
    let VosExpr::Call { callee, args, .. } = expr else {
        return Ok(None);
    };
    let VosExpr::Member { object, name, .. } = callee.as_ref() else {
        return Ok(None);
    };
    if !is_aggregate_method(name) {
        return Ok(None);
    }
    // Table-level spellings are execution boundaries handled above.
    if matches!(object.as_ref(), VosExpr::Name { .. }) {
        return Ok(None);
    }
    let mut ops = lower_pipeline(object, parameters)?;
    ops.push(QueryOp::Aggregate { group_keys: vec![], aggregates: vec![terminal_aggregate(name, args, parameters)?] });
    Ok(Some(ops))
}

/// Build the aggregate a terminal method names.
///
/// `count` counts rows (optionally only those its predicate keeps); the other
/// methods fold one value lambda, with an optional second predicate serving as
/// SQL's `FILTER (WHERE ...)`.
fn terminal_aggregate(method: &str, args: &[VosExpr], parameters: &[ParameterDescriptor]) -> Result<AggregateField> {
    if method == "count" {
        let filter = aggregate_filter(args, method, parameters)?;
        let inner = AggregateField::Count { name: method.to_string(), expr: None };
        return Ok(match filter {
            Some(predicate) => AggregateField::Filtered { inner: Box::new(inner), predicate },
            None => inner,
        });
    }
    let Some(first) = args.first() else {
        return Err(query_error(format!("`.{method}` requires a value lambda, e.g. `.{method}(x => x.amount)`")));
    };
    let VosExpr::Lambda(lambda) = first else {
        return Err(query_error(format!("`.{method}` requires a value lambda, e.g. `.{method}(x => x.amount)`")));
    };
    let param = lambda.params.first().map(String::as_str);
    let expr = lower_project_expr(lambda.body.as_ref(), param, parameters)?;
    let filter = aggregate_filter(&args[1..], method, parameters)?;
    Ok(aggregate_field(method, method.to_string(), expr, filter))
}

/// Optional `.method(predicate)` filter of a terminal aggregate.
fn aggregate_filter(args: &[VosExpr], method: &str, parameters: &[ParameterDescriptor]) -> Result<Option<Pred>> {
    match args {
        [] => Ok(None),
        [one] => Ok(Some(lower_predicate(one, parameters)?)),
        _ => Err(query_error(format!("`.{method}` takes at most one filter predicate"))),
    }
}

/// Assemble an aggregate field from its method name and already-lowered parts.
fn aggregate_field(method: &str, name: String, expr: ProjectExpr, filter: Option<Pred>) -> AggregateField {
    let inner = match method {
        "sum" => AggregateField::Sum { name, expr },
        "avg" => AggregateField::Avg { name, expr },
        "min" => AggregateField::Min { name, expr },
        "max" => AggregateField::Max { name, expr },
        "array_agg" => AggregateField::ArrayAgg { name, expr },
        "count" => AggregateField::Count { name, expr: Some(expr) },
        other => unreachable!("`{other}` is not an aggregate method"),
    };
    match filter {
        Some(predicate) => AggregateField::Filtered { inner: Box::new(inner), predicate },
        None => inner,
    }
}

fn expand_bindings(expr: &VosExpr, bindings: &[(String, VosExpr)]) -> Result<VosExpr> {
    match expr {
        VosExpr::Name { name, .. } => {
            if let Some((_, value)) = bindings.iter().rev().find(|(n, _)| n == name) {
                expand_bindings(value, bindings)
            }
            else {
                Ok(expr.clone())
            }
        }
        VosExpr::Member { object, name, name_span, sep, span } => Ok(VosExpr::Member {
            object: Box::new(expand_bindings(object, bindings)?),
            name: name.clone(),
            name_span: name_span.clone(),
            sep: *sep,
            span: span.clone(),
        }),
        VosExpr::Call { callee, args, span } => Ok(VosExpr::Call {
            callee: Box::new(expand_bindings(callee, bindings)?),
            args: args.iter().map(|arg| expand_bindings(arg, bindings)).collect::<Result<Vec<_>>>()?,
            span: span.clone(),
        }),
        other => Ok(other.clone()),
    }
}

fn split_collect(expr: &VosExpr) -> Result<VosExpr> {
    match expr {
        VosExpr::Call { callee, args, span: _ } => match callee.as_ref() {
            VosExpr::Member { object, name, .. } if name == "collect" => {
                if !args.is_empty() {
                    return Err(query_error("`.collect()` takes no arguments"));
                }
                Ok(object.as_ref().clone())
            }
            _ => Err(query_error("read pipelines must end in `.collect()`")),
        },
        _ => Err(query_error("read pipelines must end in `.collect()`")),
    }
}

/// Pipeline state the method list threads through lowering.
///
/// `.group_by(...)` parks its keys here so the projection that follows turns
/// into one [`QueryOp::Aggregate`] instead of a per-row projection.
#[derive(Default)]
struct PipelineState {
    group_keys: Option<Vec<ProjectField>>,
}

fn lower_pipeline(expr: &VosExpr, parameters: &[ParameterDescriptor]) -> Result<Vec<QueryOp>> {
    let mut methods: Vec<(&str, &[VosExpr])> = Vec::new();
    let mut cur = expr;
    loop {
        match cur {
            VosExpr::Call { callee, args, .. } => match callee.as_ref() {
                VosExpr::Member { object, name, .. } => {
                    methods.push((name.as_str(), args.as_slice()));
                    cur = object.as_ref();
                }
                _ => return Err(query_error("unsupported call shape in query pipeline")),
            },
            VosExpr::Member { object, name, .. } if name == "all" => {
                methods.push(("all", &[]));
                cur = object.as_ref();
            }
            VosExpr::Name { name, .. } => {
                let mut ops = vec![QueryOp::Scan { table: name.clone() }];
                let mut state = PipelineState::default();
                for (method, args) in methods.into_iter().rev() {
                    push_method(&mut ops, &mut state, method, args, parameters)?;
                }
                // `.group_by(...)` with no projection still means one row per
                // distinct key: the group keys are the whole output.
                if let Some(group_keys) = state.group_keys.take() {
                    ops.push(QueryOp::Aggregate { group_keys, aggregates: vec![] });
                }
                return Ok(ops);
            }
            _ => return Err(query_error("query pipeline must start from a table name")),
        }
    }
}

fn push_method(ops: &mut Vec<QueryOp>, state: &mut PipelineState, method: &str, args: &[VosExpr], parameters: &[ParameterDescriptor]) -> Result<()> {
    // The projection is what turns parked group keys into an aggregate, so it
    // is the only method allowed to follow `.group_by`. Anything else would
    // silently apply to the input rows instead of the groups.
    if state.group_keys.is_some() && method != "map" {
        return Err(query_error(format!("`.{method}` after `.group_by` is not supported; project the groups first")));
    }
    match method {
        "all" => Ok(()),
        "filter" | "where" => {
            let pred_expr = args.first().ok_or_else(|| query_error(format!("`.{method}` requires a predicate")))?;
            ops.push(QueryOp::Filter { predicate: lower_predicate(pred_expr, parameters)? });
            Ok(())
        }
        "group_by" => {
            if args.is_empty() {
                return Err(query_error("`.group_by` requires at least one key"));
            }
            let mut keys = Vec::with_capacity(args.len());
            for arg in args {
                let VosExpr::Lambda(lambda) = arg else {
                    return Err(query_error("each `.group_by` key must be a lambda"));
                };
                let param = lambda.params.first().map(String::as_str);
                let path = lower_rel_path(lambda.body.as_ref(), param)?;
                if path.is_empty() {
                    return Err(query_error("a `.group_by` key must read a field, not the row itself"));
                }
                // Qualified paths keep join sides apart (`a.id` vs `b.id`); a
                // plain field keeps its bare name.
                let name = path.join(".");
                keys.push(ProjectField { name, expr: ProjectExpr::Scalar { path } });
            }
            state.group_keys = Some(keys);
            Ok(())
        }
        "join" | "left_join" | "right_join" | "full_join" => lower_join(ops, method, args, parameters),
        "map" => {
            let proj = args.first().ok_or_else(|| query_error("`.map` requires a projection"))?;
            if let Some(group_keys) = state.group_keys.take() {
                let (aggregates, fields) = lower_grouped_projection(proj, &group_keys, parameters)?;
                ops.push(QueryOp::Aggregate { group_keys, aggregates });
                ops.push(QueryOp::Project { fields });
                return Ok(());
            }
            // A projection that only lists aggregates is a global aggregate
            // over every row the pipeline produced.
            if let Some((aggregates, fields)) = lower_global_aggregate_projection(proj, parameters)? {
                ops.push(QueryOp::Aggregate { group_keys: vec![], aggregates });
                ops.push(QueryOp::Project { fields });
                return Ok(());
            }
            // `x.*` keeps every public field: the pipeline already carries
            // them, so the identity projection pushes no operator.
            if let Some(fields) = lower_projection(proj, parameters)? {
                ops.push(QueryOp::Project { fields });
            }
            Ok(())
        }
        "sort_by" => {
            ops.push(QueryOp::Sort { keys: lower_sort_keys(args, true, parameters)? });
            Ok(())
        }
        "sort_by_desc" => {
            ops.push(QueryOp::Sort { keys: lower_sort_keys(args, false, parameters)? });
            Ok(())
        }
        "skip" => {
            let count = bound_count(args.first(), method, parameters)?;
            ops.push(QueryOp::Skip { count: CountExpr::Bound(count) });
            Ok(())
        }
        "take" => {
            let count = bound_count(args.first(), method, parameters)?;
            ops.push(QueryOp::Take { count: CountExpr::Bound(count) });
            Ok(())
        }
        "insert" | "update" | "delete" => Err(query_error(format!("write method `.{method}` is not supported by the read pipeline executor"))),
        other => Err(query_error(format!("unsupported pipeline method `.{other}`"))),
    }
}

/// Lower `.join(right, (left, right) => ...)` and its outer-join variants.
///
/// The left pipeline keeps its own column names, so `(a, p) => a.id == p.id`
/// reads the left row by plain field name and finds the joined columns under
/// the second parameter's prefix (`p.id`). Naming both sides this way keeps a
/// projection readable: `x.email` and `x.p.amount`.
fn lower_join(ops: &mut Vec<QueryOp>, method: &str, args: &[VosExpr], parameters: &[ParameterDescriptor]) -> Result<()> {
    if args.len() != 2 {
        return Err(query_error(format!("`.{method}` takes a right-hand pipeline and an `on` predicate")));
    }
    let VosExpr::Lambda(on) = &args[1] else {
        return Err(query_error(format!("`.{method}` requires an `on` lambda such as `(a, o) => a.id == o.account_id`")));
    };
    if on.params.len() != 2 {
        return Err(query_error(format!("`.{method}` `on` must bind both sides as `(left, right) => ...`")));
    }
    let left = on.params[0].as_str();
    let right = on.params[1].as_str();
    if left == right {
        return Err(query_error(format!("`.{method}` `on` must name the two sides differently")));
    }
    let mut right_ops = lower_pipeline(&args[0], parameters)?;
    right_ops.push(QueryOp::Collect);
    let predicate = lower_join_pred(on.body.as_ref(), left, right, parameters)?;
    let kind = match method {
        "join" => JoinKind::Inner,
        "left_join" => JoinKind::Left,
        "right_join" => JoinKind::Right,
        _ => JoinKind::Full,
    };
    ops.push(QueryOp::JoinSubquery { kind, right: right_ops, right_alias: right.to_string(), on: predicate });
    Ok(())
}

/// Field path in a join predicate.
///
/// A path rooted at the left parameter reads the left row and keeps its plain
/// field names; a path rooted at the right parameter keeps that name as the
/// column prefix, matching how [`QueryOp::JoinSubquery`] merges the joined
/// rows.
fn join_side_path(expr: &VosExpr, left: &str, right: &str) -> Option<Vec<String>> {
    match expr {
        VosExpr::Member { object, name, .. } => {
            let mut path = join_side_path(object, left, right)?;
            path.push(name.clone());
            Some(path)
        }
        VosExpr::Name { name, .. } if name == left => Some(Vec::new()),
        VosExpr::Name { name, .. } if name == right => Some(vec![right.to_string()]),
        _ => None,
    }
}

/// Join predicate lowering: the same two-valued comparisons as a filter, with
/// the joined side's columns kept under their lambda prefix.
fn lower_join_pred(expr: &VosExpr, left: &str, right: &str, parameters: &[ParameterDescriptor]) -> Result<Pred> {
    match expr {
        VosExpr::Literal(VosLiteral::Bool(true)) => Ok(Pred::True),
        VosExpr::Literal(VosLiteral::Bool(false)) => Ok(Pred::False),
        VosExpr::Unary { op: VosUnaryOp::Not, expr, .. } => Ok(Pred::Not(Box::new(lower_join_pred(expr, left, right, parameters)?))),
        VosExpr::Binary { op: VosBinaryOp::And, left: lhs, right: rhs, .. } => Ok(Pred::And(
            Box::new(lower_join_pred(lhs, left, right, parameters)?),
            Box::new(lower_join_pred(rhs, left, right, parameters)?),
        )),
        VosExpr::Binary { op: VosBinaryOp::Or, left: lhs, right: rhs, .. } => Ok(Pred::Or(
            Box::new(lower_join_pred(lhs, left, right, parameters)?),
            Box::new(lower_join_pred(rhs, left, right, parameters)?),
        )),
        VosExpr::Binary { op, left: lhs, right: rhs, .. } => {
            let cmp = match op {
                VosBinaryOp::Eq => CmpOp::Eq,
                VosBinaryOp::Ne => CmpOp::Ne,
                VosBinaryOp::Lt => CmpOp::Lt,
                VosBinaryOp::Le => CmpOp::Le,
                VosBinaryOp::Gt => CmpOp::Gt,
                VosBinaryOp::Ge => CmpOp::Ge,
                _ => return Err(query_error("unsupported comparison in a join predicate")),
            };
            match (join_side_path(lhs, left, right), join_side_path(rhs, left, right)) {
                (Some(left_path), Some(right_path)) if !left_path.is_empty() && !right_path.is_empty() => {
                    Ok(Pred::FieldCmpField { left_path, op: cmp, right_path })
                }
                (Some(path), _) if !path.is_empty() => Ok(Pred::FieldCmp { path, op: cmp, value: lower_bound_value(rhs.as_ref(), parameters)? }),
                (_, Some(_)) => Err(query_error("a join predicate cannot compare a value against a joined field; put the field on the left")),
                _ => {
                    let expr = lower_value_expr(lhs.as_ref(), None, parameters)?;
                    let value = lower_bound_value(rhs.as_ref(), parameters)?;
                    Ok(Pred::ExprCmp { expr: Box::new(expr), op: cmp, value })
                }
            }
        }
        VosExpr::Call { callee, args, .. } => {
            let VosExpr::Member { object, name, .. } = callee.as_ref()
            else {
                return Err(query_error("unsupported call shape in a join predicate"));
            };
            let op = match name.as_str() {
                "contains" => TextOp::Contains,
                "startsWith" => TextOp::StartsWith,
                "endsWith" => TextOp::EndsWith,
                "like" => TextOp::Like,
                "ilike" => TextOp::ILike,
                other => return Err(query_error(format!("unsupported join predicate method `.{other}`"))),
            };
            if args.len() != 1 {
                return Err(query_error(format!("`.{name}()` requires exactly one text argument")));
            }
            let Some(path) = join_side_path(object, left, right).filter(|path| !path.is_empty()) else {
                return Err(query_error(format!("`.{name}()` in a join predicate must read a joined field")));
            };
            let value = match &args[0] {
                VosExpr::Literal(VosLiteral::String(text)) => BoundValue::Literal(Value::Text(text.clone())),
                VosExpr::Name { name, .. } => name_to_slot(name, parameters)?,
                _ => return Err(query_error(format!("`.{name}()` requires a text literal or parameter as its argument"))),
            };
            Ok(Pred::TextTest { path, op, value })
        }
        other => {
            let Some(path) = join_side_path(other, left, right).filter(|path| !path.is_empty()) else {
                return Err(query_error("unsupported join predicate shape"));
            };
            Ok(Pred::FieldBool { path, value: BoundValue::Literal(Value::Bool(true)) })
        }
    }
}

/// Lower the projection of a `.group_by(...).map(...)`.
///
/// Aggregate calls become [`AggregateField`]s; every other field must name one
/// of the group keys, mirroring SQL's `GROUP BY` rule. The emitted
/// [`QueryOp::Project`] renames the aggregate row's columns to the author's
/// projection names.
fn lower_grouped_projection(
    expr: &VosExpr,
    group_keys: &[ProjectField],
    parameters: &[ParameterDescriptor],
) -> Result<(Vec<AggregateField>, Vec<ProjectField>)> {
    let (body, param) = projection_body(expr);
    let VosExpr::StructProj { items, .. } = body else {
        return Err(query_error("a grouped projection must list its fields, e.g. `x => x.{ key, total: x.amount.sum() }`"));
    };
    let mut aggregates = Vec::new();
    let mut fields = Vec::new();
    for item in items {
        let VosProjItem::Field(init) = item else {
            return Err(query_error("`*` cannot expand inside a grouped projection; list the fields explicitly"));
        };
        let path = match &init.value {
            None => vec![init.name.clone()],
            Some(value) => {
                if let Some(aggregate) = lower_aggregate_value(value, &init.name, param, parameters)? {
                    aggregates.push(aggregate);
                    fields.push(ProjectField { name: init.name.clone(), expr: ProjectExpr::Scalar { path: vec![init.name.clone()] } });
                    continue;
                }
                lower_rel_path(value, param)?
            }
        };
        let Some(key) = group_keys.iter().find(|key| matches!(&key.expr, ProjectExpr::Scalar { path: key_path } if *key_path == path))
        else {
            return Err(query_error(format!("projection field `{}` is neither a group key nor an aggregate", init.name)));
        };
        fields.push(ProjectField { name: init.name.clone(), expr: ProjectExpr::Scalar { path: vec![key.name.clone()] } });
    }
    Ok((aggregates, fields))
}

/// Recognise a projection whose fields are all aggregates, so a `.map` with no
/// `.group_by` folds every row into one.
fn lower_global_aggregate_projection(
    expr: &VosExpr,
    parameters: &[ParameterDescriptor],
) -> Result<Option<(Vec<AggregateField>, Vec<ProjectField>)>> {
    let (body, _) = projection_body(expr);
    let VosExpr::StructProj { items, .. } = body else {
        return Ok(None);
    };
    let any_aggregate = items.iter().any(|item| match item {
        VosProjItem::Field(init) => init.value.as_ref().is_some_and(is_aggregate_call),
        VosProjItem::Star { .. } => false,
    });
    if !any_aggregate {
        return Ok(None);
    }
    let (aggregates, fields) = lower_grouped_projection(expr, &[], parameters)?;
    Ok(Some((aggregates, fields)))
}

/// Whether an expression is spelled as an aggregate call (`x.amount.sum()`,
/// `count()`), without lowering it.
fn is_aggregate_call(expr: &VosExpr) -> bool {
    let VosExpr::Call { callee, .. } = expr else {
        return false;
    };
    match callee.as_ref() {
        VosExpr::Member { name, .. } => is_aggregate_method(name),
        VosExpr::Name { name, .. } => name == "count",
        _ => false,
    }
}

/// Lower one aggregate expression to its [`AggregateField`]; `None` when the
/// expression is not an aggregate call.
///
/// Method style folds a field (`x.amount.sum()`), free `count()` counts rows,
/// and a trailing lambda on either is the `FILTER (WHERE ...)` predicate.
fn lower_aggregate_value(
    expr: &VosExpr,
    output_name: &str,
    param: Option<&str>,
    parameters: &[ParameterDescriptor],
) -> Result<Option<AggregateField>> {
    let VosExpr::Call { callee, args, .. } = expr else {
        return Ok(None);
    };
    match callee.as_ref() {
        VosExpr::Member { object, name, .. } if is_aggregate_method(name) => {
            let value = lower_project_expr(object, param, parameters)?;
            let filter = aggregate_filter(args, name, parameters)?;
            Ok(Some(aggregate_field(name, output_name.to_string(), value, filter)))
        }
        VosExpr::Name { name, .. } if name == "count" => {
            let filter = aggregate_filter(args, "count", parameters)?;
            let inner = AggregateField::Count { name: output_name.to_string(), expr: None };
            Ok(Some(match filter {
                Some(predicate) => AggregateField::Filtered { inner: Box::new(inner), predicate },
                None => inner,
            }))
        }
        _ => Ok(None),
    }
}

/// Split a `.map` argument into its projected body and lambda parameter.
fn projection_body(expr: &VosExpr) -> (&VosExpr, Option<&str>) {
    match expr {
        VosExpr::Lambda(lambda) => (lambda.body.as_ref(), lambda.params.first().map(String::as_str)),
        other => (other, None),
    }
}

fn lower_predicate(expr: &VosExpr, parameters: &[ParameterDescriptor]) -> Result<Pred> {
    match expr {
        VosExpr::Lambda(lambda) => {
            let param = lambda.params.first().map(String::as_str);
            lower_pred_body(lambda.body.as_ref(), param, parameters)
        }
        other => lower_pred_body(other, None, parameters),
    }
}

fn lower_pred_body(expr: &VosExpr, param: Option<&str>, parameters: &[ParameterDescriptor]) -> Result<Pred> {
    match expr {
        VosExpr::Literal(VosLiteral::Bool(true)) => Ok(Pred::True),
        VosExpr::Literal(VosLiteral::Bool(false)) => Ok(Pred::False),
        VosExpr::Unary { op: VosUnaryOp::Not, expr, .. } => Ok(Pred::Not(Box::new(lower_pred_body(expr, param, parameters)?))),
        VosExpr::Binary { op: VosBinaryOp::And, left, right, .. } => {
            Ok(Pred::And(Box::new(lower_pred_body(left, param, parameters)?), Box::new(lower_pred_body(right, param, parameters)?)))
        }
        VosExpr::Binary { op: VosBinaryOp::Or, left, right, .. } => {
            Ok(Pred::Or(Box::new(lower_pred_body(left, param, parameters)?), Box::new(lower_pred_body(right, param, parameters)?)))
        }
        VosExpr::Binary { op, left, right, .. } => {
            let cmp = match op {
                VosBinaryOp::Eq => CmpOp::Eq,
                VosBinaryOp::Ne => CmpOp::Ne,
                VosBinaryOp::Lt => CmpOp::Lt,
                VosBinaryOp::Le => CmpOp::Le,
                VosBinaryOp::Gt => CmpOp::Gt,
                VosBinaryOp::Ge => CmpOp::Ge,
                _ => return Err(query_error("unsupported comparison in filter")),
            };
            let left_path = lower_field_path(left.as_ref(), param).ok().filter(|path| !path.is_empty());
            let right_path = lower_field_path(right.as_ref(), param).ok().filter(|path| !path.is_empty());
            match (left_path, right_path) {
                (Some(left_path), Some(right_path)) => Ok(Pred::FieldCmpField { left_path, op: cmp, right_path }),
                (Some(path), None) => {
                    // Literal/parameter right-hand side keeps the two-valued
                    // field comparison, including `== null` presence tests.
                    let value = lower_bound_value(right.as_ref(), parameters)?;
                    if matches!(op, VosBinaryOp::Eq) {
                        if let BoundValue::Literal(Value::Bool(b)) = &value {
                            let value = *b;
                            return Ok(Pred::FieldBool { path, value: BoundValue::Literal(Value::Bool(value)) });
                        }
                    }
                    Ok(Pred::FieldCmp { path, op: cmp, value })
                }
                (None, _) => {
                    // Computed left operand: the comparison is the
                    // three-valued expression form against a bound value.
                    let expr = lower_value_expr(left.as_ref(), param, parameters)?;
                    let value = lower_bound_value(right.as_ref(), parameters)?;
                    Ok(Pred::ExprCmp { expr: Box::new(expr), op: cmp, value })
                }
            }
        }
        VosExpr::Call { callee, args, .. } => {
            let VosExpr::Member { object, name, .. } = callee.as_ref()
            else {
                return Err(query_error("unsupported call shape in a filter predicate"));
            };
            let op = match name.as_str() {
                "contains" => TextOp::Contains,
                "startsWith" => TextOp::StartsWith,
                "endsWith" => TextOp::EndsWith,
                // SQL 通配模式：`like("a%")` 与 `ilike("A%")` 直接对应 SQL 的
                // `LIKE` / `ILIKE`，两端共用同一匹配语义。
                "like" => TextOp::Like,
                "ilike" => TextOp::ILike,
                other => return Err(query_error(format!("unsupported predicate method `.{other}`"))),
            };
            if args.len() != 1 {
                return Err(query_error(format!("`.{name}()` requires exactly one text argument")));
            }
            let path = lower_field_path(object, param)?;
            if path.is_empty() {
                return Err(query_error(format!("`.{name}()` requires a field access on the left")));
            }
            let value = match &args[0] {
                VosExpr::Literal(VosLiteral::String(text)) => BoundValue::Literal(Value::Text(text.clone())),
                VosExpr::Name { name, .. } => name_to_slot(name, parameters)?,
                _ => {
                    return Err(query_error(format!("`.{name}()` requires a text literal or parameter as its argument")));
                }
            };
            Ok(Pred::TextTest { path, op, value })
        }
        _ => {
            let path = lower_field_path(expr, param)?;
            if path.is_empty() {
                return Err(query_error("unsupported predicate shape"));
            }
            Ok(Pred::FieldBool { path, value: BoundValue::Literal(Value::Bool(true)) })
        }
    }
}

/// Convert a `Name("__yydb_param_<name>")` Identifier (emitted by our
/// scan_and_rewrite preprocessor) into a proper `BoundValue::Slot`.
fn name_to_slot(name: &str, parameters: &[ParameterDescriptor]) -> Result<BoundValue> {
    super::params::name_to_slot(name, parameters)
}

/// Literal or parameter reference usable as a bound comparison value.
fn lower_bound_value(expr: &VosExpr, parameters: &[ParameterDescriptor]) -> Result<BoundValue> {
    match expr {
        VosExpr::Literal(VosLiteral::Bool(b)) => Ok(BoundValue::Literal(Value::Bool(*b))),
        VosExpr::Literal(VosLiteral::Int(t)) => {
            let n: i64 = t.parse().map_err(|_| query_error(format!("invalid integer literal `{t}`")))?;
            Ok(BoundValue::Literal(Value::I64(n)))
        }
        VosExpr::Literal(VosLiteral::Float(t)) => {
            let n: f64 = t.parse().map_err(|_| query_error(format!("invalid float literal `{t}`")))?;
            Ok(BoundValue::Literal(Value::F64(n)))
        }
        VosExpr::Literal(VosLiteral::String(s)) => Ok(BoundValue::Literal(Value::Text(s.clone()))),
        VosExpr::Literal(VosLiteral::Null) => Ok(BoundValue::Literal(Value::Null)),
        VosExpr::Name { name, .. } => name_to_slot(name, parameters),
        _ => Err(query_error("filter comparisons require a literal or parameter right-hand side")),
    }
}

fn lower_field_path(expr: &VosExpr, param: Option<&str>) -> Result<Vec<String>> {
    match expr {
        VosExpr::Member { object, name, .. } => {
            let mut path = lower_field_path(object.as_ref(), param)?;
            path.push(name.clone());
            Ok(path)
        }
        VosExpr::Name { name, .. } if Some(name.as_str()) == param => Ok(Vec::new()),
        _ => Err(query_error("filter field paths must be rooted at the lambda parameter")),
    }
}

/// Relative field path that also accepts a bare field name as the receiver
/// field, mirroring the VOS projection name-resolution rule.
fn lower_rel_path(expr: &VosExpr, param: Option<&str>) -> Result<Vec<String>> {
    match expr {
        VosExpr::Member { object, name, .. } => {
            let mut path = lower_rel_path(object.as_ref(), param)?;
            path.push(name.clone());
            Ok(path)
        }
        VosExpr::Name { name, .. } if Some(name.as_str()) == param => Ok(Vec::new()),
        VosExpr::Name { name, .. } => Ok(vec![name.clone()]),
        _ => Err(query_error("value expressions must be built from field paths, literals and functions")),
    }
}

/// Lower a value expression (projection value, computed sort key, computed
/// predicate operand) into the shared expression IR.
fn lower_value_expr(expr: &VosExpr, param: Option<&str>, parameters: &[ParameterDescriptor]) -> Result<Expr> {
    match expr {
        VosExpr::Literal(_) => Ok(Expr::Bound(lower_bound_value(expr, parameters)?)),
        VosExpr::Name { name, .. } => {
            if let Ok(slot) = name_to_slot(name, parameters) {
                return Ok(Expr::Bound(slot));
            }
            if Some(name.as_str()) == param {
                return Err(query_error("the row binding itself is not a value expression; access its fields"));
            }
            Ok(Expr::Column { path: vec![name.clone()] })
        }
        VosExpr::Member { .. } => {
            let path = lower_rel_path(expr, param)?;
            if path.is_empty() {
                return Err(query_error("the row binding itself is not a value expression; access its fields"));
            }
            Ok(Expr::Column { path })
        }
        VosExpr::Binary { op, left, right, .. } => Ok(Expr::Binary {
            op: lower_binary_op(*op)?,
            left: Box::new(lower_value_expr(left.as_ref(), param, parameters)?),
            right: Box::new(lower_value_expr(right.as_ref(), param, parameters)?),
        }),
        VosExpr::Unary { op: VosUnaryOp::Not, expr, .. } => Ok(Expr::Not(Box::new(lower_value_expr(expr.as_ref(), param, parameters)?))),
        VosExpr::Call { callee, args, .. } => lower_call_expr(callee, args, param, parameters),
        _ => Err(query_error("unsupported value expression in VOS pipeline")),
    }
}

fn lower_binary_op(op: VosBinaryOp) -> Result<BinaryOp> {
    Ok(match op {
        VosBinaryOp::Eq => BinaryOp::Eq,
        VosBinaryOp::Ne => BinaryOp::Ne,
        VosBinaryOp::Lt => BinaryOp::Lt,
        VosBinaryOp::Le => BinaryOp::Le,
        VosBinaryOp::Gt => BinaryOp::Gt,
        VosBinaryOp::Ge => BinaryOp::Ge,
        VosBinaryOp::And => BinaryOp::And,
        VosBinaryOp::Or => BinaryOp::Or,
        VosBinaryOp::Add => BinaryOp::Add,
        VosBinaryOp::Sub => BinaryOp::Sub,
        VosBinaryOp::Mul => BinaryOp::Mul,
        VosBinaryOp::Div => BinaryOp::Div,
    })
}

/// Function calls in both documented spellings: the method style
/// (`x.email.lower()`, `x.meta.at("k")`) and the free style
/// (`coalesce(a, b)`, `now()`), all lowered to the same `ScalarFunction` /
/// `JsonAccess` IR the SQL frontend already executes.
fn unary_function(label: &str, function: ScalarFunction, call_args: Vec<Expr>) -> Result<Expr> {
    if call_args.len() != 1 {
        return Err(query_error(format!("`{label}` takes exactly one argument")));
    }
    Ok(Expr::Function { function, args: call_args })
}

fn lower_call_expr(callee: &VosExpr, args: &[VosExpr], param: Option<&str>, parameters: &[ParameterDescriptor]) -> Result<Expr> {
    let lowered_args = || args.iter().map(|arg| lower_value_expr(arg, param, parameters)).collect::<Result<Vec<_>>>();
    match callee {
        VosExpr::Member { object, name, .. } => {
            let receiver = lower_value_expr(object.as_ref(), param, parameters)?;
            let extra = lowered_args()?;
            match name.as_str() {
                "lower" if extra.is_empty() => Ok(Expr::Function { function: ScalarFunction::Lower, args: vec![receiver] }),
                "upper" if extra.is_empty() => Ok(Expr::Function { function: ScalarFunction::Upper, args: vec![receiver] }),
                "trim" if extra.is_empty() => Ok(Expr::Function { function: ScalarFunction::Trim, args: vec![receiver] }),
                "length" if extra.is_empty() => Ok(Expr::Function { function: ScalarFunction::Length, args: vec![receiver] }),
                "substr" if !extra.is_empty() && extra.len() <= 2 => {
                    let mut call_args = vec![receiver];
                    call_args.extend(extra);
                    Ok(Expr::Function { function: ScalarFunction::Substr, args: call_args })
                }
                "replace" if extra.len() == 2 => {
                    let mut call_args = vec![receiver];
                    call_args.extend(extra);
                    Ok(Expr::Function { function: ScalarFunction::Replace, args: call_args })
                }
                "coalesce" if extra.len() == 1 => {
                    let mut call_args = vec![receiver];
                    call_args.extend(extra);
                    Ok(Expr::Function { function: ScalarFunction::Coalesce, args: call_args })
                }
                "concat" if extra.len() == 1 => {
                    let mut call_args = vec![receiver];
                    call_args.extend(extra);
                    Ok(Expr::Function { function: ScalarFunction::Concat, args: call_args })
                }
                "year" | "month" | "day" | "hour" | "minute" | "second" | "quarter" | "epoch" | "dow" | "doy" | "week" if extra.is_empty() => {
                    let field = match name.as_str() {
                        "year" => DateExtractField::Year,
                        "month" => DateExtractField::Month,
                        "day" => DateExtractField::Day,
                        "hour" => DateExtractField::Hour,
                        "minute" => DateExtractField::Minute,
                        "second" => DateExtractField::Second,
                        "quarter" => DateExtractField::Quarter,
                        "epoch" => DateExtractField::Epoch,
                        "dow" => DateExtractField::Dow,
                        "doy" => DateExtractField::Doy,
                        _ => DateExtractField::Week,
                    };
                    Ok(Expr::Function { function: ScalarFunction::Extract(field), args: vec![receiver] })
                }
                "at" if extra.len() == 1 => {
                    let Some(key) = extra.into_iter().next() else { unreachable!() };
                    Ok(Expr::JsonAccess { left: Box::new(receiver), key: Box::new(key), as_text: false })
                }
                "text" if extra.len() == 1 => {
                    let Some(key) = extra.into_iter().next() else { unreachable!() };
                    Ok(Expr::JsonAccess { left: Box::new(receiver), key: Box::new(key), as_text: true })
                }
                other => Err(query_error(format!("unsupported VOS method `.{other}` in a value expression"))),
            }
        }
        VosExpr::Name { name, .. } => {
            let call_args = lowered_args()?;
            match name.as_str() {
                "lower" => unary_function("lower", ScalarFunction::Lower, call_args),
                "upper" => unary_function("upper", ScalarFunction::Upper, call_args),
                "trim" => unary_function("trim", ScalarFunction::Trim, call_args),
                "length" => unary_function("length", ScalarFunction::Length, call_args),
                "char_length" => unary_function("char_length", ScalarFunction::CharLength, call_args),
                "year" => unary_function("year", ScalarFunction::Year, call_args),
                "month" => unary_function("month", ScalarFunction::Month, call_args),
                "day" => unary_function("day", ScalarFunction::Day, call_args),
                "substr" if call_args.len() == 2 || call_args.len() == 3 => Ok(Expr::Function { function: ScalarFunction::Substr, args: call_args }),
                "replace" if call_args.len() == 3 => Ok(Expr::Function { function: ScalarFunction::Replace, args: call_args }),
                "coalesce" if call_args.len() >= 2 => Ok(Expr::Function { function: ScalarFunction::Coalesce, args: call_args }),
                "concat" if call_args.len() >= 2 => Ok(Expr::Function { function: ScalarFunction::Concat, args: call_args }),
                "now" if call_args.is_empty() => Ok(Expr::Function { function: ScalarFunction::Now, args: vec![] }),
                "current_timestamp" if call_args.is_empty() => Ok(Expr::Function { function: ScalarFunction::CurrentTimestamp, args: vec![] }),
                "current_date" if call_args.is_empty() => Ok(Expr::Function { function: ScalarFunction::CurrentDate, args: vec![] }),
                "current_time" if call_args.is_empty() => Ok(Expr::Function { function: ScalarFunction::CurrentTime, args: vec![] }),
                other => Err(query_error(format!("unsupported VOS function `{other}` in a value expression"))),
            }
        }
        _ => Err(query_error("unsupported call shape in a value expression")),
    }
}

fn lower_sort_keys(args: &[VosExpr], ascending: bool, parameters: &[ParameterDescriptor]) -> Result<Vec<SortKey>> {
    if args.is_empty() {
        return Err(query_error("`.sort_by` requires at least one key lambda"));
    }
    args.iter()
        .map(|arg| {
            let VosExpr::Lambda(lambda) = arg else {
                return Err(query_error("each `.sort_by` key must be a lambda"));
            };
            let param = lambda.params.first().map(String::as_str);
            lower_sort_key(lambda.body.as_ref(), param, ascending, parameters)
        })
        .collect()
}

fn lower_sort_key(body: &VosExpr, param: Option<&str>, ascending: bool, parameters: &[ParameterDescriptor]) -> Result<SortKey> {
    // Direction suffix `x.field.asc()` / `x.field.desc()` overrides the
    // method default, per the locked VOS sort surface.
    if let VosExpr::Call { callee, args, .. } = body {
        if args.is_empty() {
            if let VosExpr::Member { object, name, .. } = callee.as_ref() {
                if matches!(name.as_str(), "asc" | "desc") {
                    return lower_sort_key(object.as_ref(), param, name == "asc", parameters);
                }
            }
        }
    }
    if let Ok(path) = lower_field_path(body, param) {
        if !path.is_empty() {
            let field = path.last().cloned().unwrap_or_default();
            return Ok(SortKey { field, expr: None, ascending });
        }
    }
    let expr = lower_value_expr(body, param, parameters)?;
    let field = match &expr {
        Expr::Column { path } => path.last().cloned().unwrap_or_else(|| "sort_key".to_string()),
        _ => "sort_key".to_string(),
    };
    Ok(SortKey { field, expr: Some(expr), ascending })
}

/// Lower a `.map` projection. `None` means the identity (`x.*`): every public
/// field the row already carries.
fn lower_projection(expr: &VosExpr, parameters: &[ParameterDescriptor]) -> Result<Option<Vec<ProjectField>>> {
    let (body, param) = match expr {
        VosExpr::Lambda(lambda) => (lambda.body.as_ref(), lambda.params.first().map(String::as_str)),
        other => (other, None),
    };
    match body {
        VosExpr::StarProj { .. } => Ok(None),
        VosExpr::StructProj { items, .. } => {
            let mut fields = Vec::new();
            for item in items {
                match item {
                    VosProjItem::Star { .. } => {
                        return Err(query_error("`*` inside `x.{ ... }` is not supported; list the fields explicitly"));
                    }
                    VosProjItem::Field(init) => fields.push(lower_project_field(init, param, parameters)?),
                }
            }
            Ok(Some(fields))
        }
        _ => Err(query_error("`.map` expects `x => x.*` or `x => x.{ ... }`")),
    }
}

fn lower_project_field(init: &VosFieldInit, param: Option<&str>, parameters: &[ParameterDescriptor]) -> Result<ProjectField> {
    let expr = match &init.value {
        None => ProjectExpr::Scalar { path: vec![init.name.clone()] },
        Some(value) => lower_project_expr(value, param, parameters)?,
    };
    Ok(ProjectField { name: init.name.clone(), expr })
}

fn lower_project_expr(expr: &VosExpr, param: Option<&str>, parameters: &[ParameterDescriptor]) -> Result<ProjectExpr> {
    match expr {
        VosExpr::StructProj { receiver, items, .. } => {
            let path = lower_field_path(receiver.as_ref(), param)?;
            let mut fields = Vec::new();
            for item in items {
                match item {
                    VosProjItem::Star { .. } => {
                        return Err(query_error("nested projection does not expand `*` yet; list the fields explicitly"));
                    }
                    VosProjItem::Field(init) => fields.push(lower_project_field(init, param, parameters)?),
                }
            }
            Ok(ProjectExpr::Nested { path, fields })
        }
        other => {
            if let VosExpr::Name { name, .. } = other {
                if let Ok(slot) = name_to_slot(name, parameters) {
                    return Ok(ProjectExpr::Computed(Box::new(Expr::Bound(slot))));
                }
            }
            if let Ok(path) = lower_rel_path(other, param) {
                if !path.is_empty() {
                    return Ok(ProjectExpr::Scalar { path });
                }
            }
            Ok(ProjectExpr::Computed(Box::new(lower_value_expr(other, param, parameters)?)))
        }
    }
}

fn bound_count(expr: Option<&VosExpr>, method: &str, parameters: &[ParameterDescriptor]) -> Result<BoundValue> {
    let Some(expr) = expr
    else {
        return Err(query_error(format!("`.{method}` requires a count argument")));
    };
    match expr {
        VosExpr::Literal(VosLiteral::Int(t)) => {
            let n: i64 = t.parse().map_err(|_| query_error(format!("invalid `{method}` count")))?;
            Ok(BoundValue::Literal(Value::I64(n)))
        }
        // A rewritten parameter Identifier `__yydb_param_N`.
        VosExpr::Name { name, .. } => name_to_slot(name, parameters),
        _ => Err(query_error(format!("`.{method}` count must be an integer literal or parameter"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_expression(source: &str) -> VosExpr {
        let program = data_text_vos::parse_program(source).expect("fixture parses");
        if let Some(expr) = program.result {
            return expr;
        }
        match program.statements.into_iter().next().expect("one statement") {
            VosStmt::Expr(expr) => expr,
            other => panic!("expected an expression statement, got {other:?}"),
        }
    }

    #[test]
    fn first_rejects_a_second_predicate_instead_of_ignoring_it() {
        let expr = first_expression("User.first(x => x.id == 1, x => x.id == 2)");
        let err = lower_boundary(&expr, &[]).expect_err("two predicates must be rejected");
        match err {
            Error::Schema { message } => assert!(message.contains("at most one predicate"), "unexpected message: {message}"),
            other => panic!("expected a schema error, got {other:?}"),
        }
    }

    #[test]
    fn count_accepts_zero_or_one_predicate() {
        assert!(lower_boundary(&first_expression("User.count()"), &[]).expect("bare count lowers").is_some());
        assert!(lower_boundary(&first_expression("User.count(x => x.id == 1)"), &[]).expect("filtered count lowers").is_some());
    }
}
