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
        AggregateField, BinaryOp, BoundValue, CmpOp, CountExpr, DateExtractField, Expr, Pred, ProjectExpr, ProjectField, QueryOp, ScalarFunction,
        SortKey, TextOp,
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
    let pipeline = split_collect(&expr)?;
    let mut ops = lower_pipeline(&pipeline, parameters)?;
    ops.push(QueryOp::Collect);
    Ok(ops)
}

/// Table-level execution boundaries: `Type.get(id)`, `Type.first(pred)`,
/// `Type.any(pred)` and `Type.count(pred)`.
///
/// Each lowers to the pipeline that yields its result rows: `get` is a
/// primary-key point lookup, `first` a filtered one-row take, `count` a
/// whole-table count row and `any` that count projected to a boolean.
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
        _ => Ok(None),
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
                for (method, args) in methods.into_iter().rev() {
                    push_method(&mut ops, method, args, parameters)?;
                }
                return Ok(ops);
            }
            _ => return Err(query_error("query pipeline must start from a table name")),
        }
    }
}

fn push_method(ops: &mut Vec<QueryOp>, method: &str, args: &[VosExpr], parameters: &[ParameterDescriptor]) -> Result<()> {
    match method {
        "all" => Ok(()),
        "filter" | "where" => {
            let pred_expr = args.first().ok_or_else(|| query_error(format!("`.{method}` requires a predicate")))?;
            ops.push(QueryOp::Filter { predicate: lower_predicate(pred_expr, parameters)? });
            Ok(())
        }
        "map" => {
            let proj = args.first().ok_or_else(|| query_error("`.map` requires a projection"))?;
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
