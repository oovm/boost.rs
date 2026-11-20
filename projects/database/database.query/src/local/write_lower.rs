//! VOS write lowering into backend-neutral [`WriteOp`] plans.
//!
//! The author surface (`Type::insert({ … })`, `Type { … }.insert()`, keyed
//! `Type::update({ … })` / `Type::delete({ … })` and schema macros) lowers
//! here, exactly like the SQL frontend lowers its statements: every statement
//! becomes one [`WriteOp`] that execution applies. Nothing in this module
//! touches storage, so a program can be lowered for planning (record windows)
//! and for execution from the same code path.
//!
//! ## Phase 1 write shapes
//!
//! | Shape | Lowered plan | Effect |
//! |-------|--------------|--------|
//! | `Type::insert({ … })` / `Type { … }.insert()` | `Insert` with a primary-key `Replace` conflict clause | Upsert the whole row |
//! | `Type::update({ pk: …, field: … })` | `Update` bounded by `pk = …`, `require_match` | Patch a keyed row, a missing key is an error |
//! | `Type::delete({ pk: … })` | `Delete` bounded by `pk = …` | Remove a keyed row, a missing key is a no-op |
//!
//! Collection forms (`Type.filter(…).update(…)`), lambda patches and
//! projection patches stay [`Error::Unsupported`]: Phase 1 has no predicate
//! evaluator here, so it never guesses which rows a filter would match.
//!
//! ## Field values
//!
//! A lowered plan carries the raw frontend value (literal or bound parameter),
//! after it has been validated against the column's canonical type. Execution
//! coerces that value once when it builds the row, which is the same single
//! coercion the SQL frontend relies on. The value the lowering *must*
//! materialize — a keyed write's primary-key predicate — is coerced here,
//! because a comparison against stored rows has to use the stored type.

use database_formats::yy::row::row_key;
use database_types::{Error, Result, TableUniqueConstraint, Value};
use schema_input_vos::{
    syntax::{VosExpr, VosFieldInit, VosLiteral, VosProgram, VosStmt},
    types::{ResolvedCanonicalType, ResolvedContract, ResolvedTypeContract},
};

use super::{
    catalog::{field_on_table, primary_field_name, table_storage_name, table_type_by_name},
    coerce, constraints,
    ops::{BoundValue, CmpOp, ConflictAction, ConflictClause, Pred, QueryRow, WriteAssignment, WriteExpr, WriteField, WriteOp},
    params::PARAM_PREFIX,
    write_plan::{expand_program, is_macro_call},
};

fn lower_error(message: impl Into<String>) -> Error {
    Error::Schema { message: message.into() }
}

/// Lower one VOS write program into the write operators execution applies.
///
/// Schema macros are expanded first, so a macro that expands to keyed writes
/// lowers like the statements it declares. `place` decides where a value that
/// lives outside the row lands; the lowering only needs it for the key value
/// it materializes, so a planning caller can pass a preview placement and
/// never write an object.
///
/// The returned vector holds one operator per write statement, in statement
/// order. A program without a write statement is an error, and a leftover
/// macro call (a schema document was required) is reported instead of being
/// silently skipped.
pub fn lower_write_program(
    program: &VosProgram,
    contract: &ResolvedContract,
    schema_document: Option<&str>,
    params: &[Value],
    place: &mut dyn FnMut(Value) -> Result<Value>,
) -> Result<Vec<WriteOp>> {
    let expanded = expand_program(program, schema_document)?;
    let mut bindings: Vec<(String, VosExpr)> = Vec::new();
    let mut ops = Vec::new();

    for stmt in &expanded.statements {
        match stmt {
            VosStmt::Let(let_) => bindings.push((let_.name.clone(), let_.value.clone())),
            VosStmt::Expr(expr) => ops.push(lower_write_expr(expr, contract, &bindings, params, place)?),
        }
    }

    if let Some(result) = &expanded.result {
        if is_macro_call(result) {
            return Err(lower_error("macro call could not be expanded (schema document required)"));
        }
        ops.push(lower_write_expr(result, contract, &bindings, params, place)?);
    }

    if ops.is_empty() {
        return Err(lower_error("execute program has no write statements"));
    }
    Ok(ops)
}

/// Record key a single keyed write operator addresses, when the operator names
/// exactly one row of a table without uniqueness rules.
///
/// `None` means "cannot prove this is a point write": the caller must fall
/// back to the full path. A table carrying a uniqueness rule falls back unless
/// the record map already holds every row, because its conflict check reads the
/// whole table.
pub fn point_write_key(
    op: &WriteOp,
    contract: &ResolvedContract,
    constraints: &[TableUniqueConstraint],
    holds_every_row: bool,
) -> Result<Option<String>> {
    let (table, pk_value) = match op {
        WriteOp::Insert { table, rows, .. } => {
            let Some(row) = rows.first()
            else {
                return Ok(None);
            };
            let entry = table_type_by_name(contract, table)?;
            let pk_field = primary_field_name(entry)?;
            let Some(field) = row.iter().find(|field| field.name == pk_field)
            else {
                return Ok(None);
            };
            let BoundValue::Literal(value) = &field.value
            else {
                return Ok(None);
            };
            let slot = field_on_table(entry, pk_field)?;
            let Ok(pk_value) = coerce::value_to_scalar(value.clone(), &slot.canonical_type, contract)
            else {
                // A non-scalar key cannot be turned into a record key here; fall
                // back to the full execution path instead of failing planning.
                return Ok(None);
            };
            (table.as_str(), pk_value)
        }
        WriteOp::Update { table, predicate, .. } | WriteOp::Delete { table, predicate, .. } => {
            let Some(Pred::FieldCmp { value: BoundValue::Literal(value), .. }) = predicate
            else {
                return Ok(None);
            };
            (table.as_str(), value.clone())
        }
        WriteOp::InsertFromQuery { .. } => return Ok(None),
    };
    let entry = table_type_by_name(contract, table)?;
    let storage_name = table_storage_name(entry)?;
    if constraints::has_rules(constraints, storage_name) && !holds_every_row {
        return Ok(None);
    }
    Ok(Some(row_key(storage_name, &constraints::value_text(&pk_value)?)))
}

/// Lower a whole program and return the record keys its point writes address.
///
/// `None` means at least one statement cannot be narrowed (collection write,
/// uniqueness rule with a cold index, non-literal key, …); the caller must fall
/// back to the full execution path. Lowering failures are reported as `None`
/// because planning must never surface an execution error before the write runs.
pub fn point_write_keys(
    program: &VosProgram,
    contract: &ResolvedContract,
    declared: &[TableUniqueConstraint],
    schema_document: Option<&str>,
    params: &[Value],
    holds_every_row: bool,
    place: &mut dyn FnMut(Value) -> Result<Value>,
) -> Result<Option<Vec<String>>> {
    let constraints = constraints::merge(constraints::from_contract(contract)?, declared);
    let Ok(ops) = lower_write_program(program, contract, schema_document, params, place)
    else {
        return Ok(None);
    };
    let mut keys = Vec::with_capacity(ops.len());
    for op in &ops {
        let Some(key) = point_write_key(op, contract, &constraints, holds_every_row)?
        else {
            return Ok(None);
        };
        keys.push(key);
    }
    keys.sort();
    keys.dedup();
    Ok(Some(keys))
}

fn lower_write_expr(
    expr: &VosExpr,
    contract: &ResolvedContract,
    bindings: &[(String, VosExpr)],
    params: &[Value],
    place: &mut dyn FnMut(Value) -> Result<Value>,
) -> Result<WriteOp> {
    let VosExpr::Call { callee, .. } = expr
    else {
        return Err(lower_error("Phase 1 execute expects a write expression"));
    };
    let VosExpr::Member { name, .. } = callee.as_ref()
    else {
        return Err(lower_error("Phase 1 execute expects a write expression"));
    };
    match name.as_str() {
        "insert" => lower_insert(expr, contract, bindings, params),
        "update" => lower_keyed_write(expr, contract, "update", bindings, params, place),
        "delete" => lower_keyed_write(expr, contract, "delete", bindings, params, place),
        _ => Err(Error::Unsupported("Phase 1 execute supports `.insert()`, `.update()` and `.delete()` only")),
    }
}

/// Lower `Type::insert({ … })` / `Type { … }.insert()` to a whole-row upsert.
///
/// The conflict clause names the primary key and replaces the conflicting row,
/// which is what makes an insert on an existing key overwrite the row: the old
/// row's unique keys are retracted before the incoming row is checked, so a
/// rewritten row never conflicts with itself.
fn lower_insert(expr: &VosExpr, contract: &ResolvedContract, bindings: &[(String, VosExpr)], params: &[Value]) -> Result<WriteOp> {
    let VosExpr::Call { callee, args, .. } = expr
    else {
        return Err(lower_error("Phase 1 execute expects an insert expression"));
    };
    let VosExpr::Member { object, name, .. } = callee.as_ref()
    else {
        return Err(lower_error("Phase 1 execute expects an insert expression"));
    };
    if name != "insert" {
        return Err(lower_error(format!("unsupported execute expression `.{}()`", name)));
    }

    let (table, entry, row) = if args.len() == 1 {
        let table = type_name_from_expr(object)?;
        let entry = table_type_by_name(contract, &table)?;
        let VosExpr::AnonObject { fields, .. } = expand_bindings(&args[0], bindings)?
        else {
            return Err(lower_error("Phase 1 execute expects `Type::insert({ … })` with an object literal"));
        };
        let row = lower_field_inits(entry, contract, &fields, params)?;
        (table, entry, row)
    }
    else if args.is_empty() {
        let object = expand_bindings(object, bindings)?;
        let VosExpr::TypedObject { ty, fields, .. } = object
        else {
            return Err(lower_error("Phase 1 execute supports `Type { … }.insert()` or `Type::insert({ … })`"));
        };
        let entry = table_type_by_name(contract, &ty)?;
        let row = lower_field_inits(entry, contract, &fields, params)?;
        (ty, entry, row)
    }
    else {
        return Err(lower_error("`.insert()` accepts zero or one object argument"));
    };

    let pk_field = primary_field_name(entry)?;
    if !row.contains_key(pk_field) {
        return Err(lower_error(format!("insert row is missing primary key field `{pk_field}`")));
    }
    Ok(WriteOp::Insert {
        table,
        rows: vec![write_fields(&row)],
        conflict: Some(ConflictClause { columns: vec![pk_field.to_owned()], action: ConflictAction::Replace }),
        returning: None,
    })
}

/// Lower `Type::update({ … })` / `Type::delete({ … })` to an operator bounded by
/// the object's primary key.
fn lower_keyed_write(
    expr: &VosExpr,
    contract: &ResolvedContract,
    verb: &str,
    bindings: &[(String, VosExpr)],
    params: &[Value],
    place: &mut dyn FnMut(Value) -> Result<Value>,
) -> Result<WriteOp> {
    let VosExpr::Call { callee, args, .. } = expr
    else {
        return Err(lower_error("Phase 1 execute expects a write expression"));
    };
    let VosExpr::Member { object, .. } = callee.as_ref()
    else {
        return Err(lower_error("Phase 1 execute expects a write expression"));
    };
    let table = match object.as_ref() {
        VosExpr::Name { name, .. } => name.clone(),
        _ => {
            return Err(Error::Unsupported(
                "Phase 1 keyed writes target a table type (`Type::update({ … })` or `Type::delete({ … })`), not a \
                 filtered query",
            ));
        }
    };
    if args.len() != 1 {
        return Err(Error::Unsupported("Phase 1 keyed writes require exactly one plain object argument"));
    }
    let entry = table_type_by_name(contract, &table)?;
    let VosExpr::AnonObject { fields, .. } = expand_bindings(&args[0], bindings)?
    else {
        return Err(Error::Unsupported("Phase 1 keyed writes require a plain `{ … }` object, not a lambda or projection"));
    };
    let object = lower_field_inits(entry, contract, &fields, params)?;
    let pk_field = primary_field_name(entry)?;
    let pk = object.get(pk_field).ok_or_else(|| lower_error(format!("`.{verb}()` object must carry the primary key field `{pk_field}`")))?;
    let pk_slot = field_on_table(entry, pk_field)?;
    let pk_value = place(coerce::value_to_scalar(pk.clone(), &pk_slot.canonical_type, contract)?)?;
    let predicate = Some(Pred::FieldCmp { path: vec![pk_field.to_owned()], op: CmpOp::Eq, value: BoundValue::Literal(pk_value) });

    if verb == "update" {
        // The key already names the row, so only the remaining fields patch it.
        let assignments = object
            .iter()
            .filter(|(field, _)| field.as_str() != pk_field)
            .map(|(field, value)| WriteAssignment { field: field.clone(), value: WriteExpr::Bound(BoundValue::Literal(value.clone())) })
            .collect();
        return Ok(WriteOp::Update { table, assignments, predicate, require_match: true });
    }
    // The object argument may carry more than the key, but only the key is
    // matched, so a stale caller still deletes exactly one row and a key that
    // is already gone is a no-op.
    Ok(WriteOp::Delete { table, predicate, returning: None })
}

fn type_name_from_expr(expr: &VosExpr) -> Result<String> {
    match expr {
        VosExpr::Name { name, .. } => Ok(name.clone()),
        VosExpr::Member { object, .. } => type_name_from_expr(object.as_ref()),
        _ => Err(lower_error("insert target must be a table type name")),
    }
}

fn table_display_name(entry: &ResolvedTypeContract) -> String {
    entry.canonical_path.join("::")
}

fn write_fields(row: &QueryRow) -> Vec<WriteField> {
    row.iter().map(|(name, value)| WriteField { name: name.clone(), value: BoundValue::Literal(value.clone()) }).collect()
}

fn lower_field_inits(entry: &ResolvedTypeContract, contract: &ResolvedContract, fields: &[VosFieldInit], params: &[Value]) -> Result<QueryRow> {
    let mut row = QueryRow::new();
    for init in fields {
        let slot = field_on_table(entry, &init.name)
            .map_err(|_| lower_error(format!("unknown field `{}` on `{}`", init.name, table_display_name(entry))))?;
        let value = init.value.as_ref().ok_or_else(|| lower_error(format!("field `{}` requires an explicit initializer", init.name)))?;
        row.insert(init.name.clone(), eval_value(value, &slot.canonical_type, contract, params)?);
    }
    Ok(row)
}

/// Read one field initializer and validate it against the column type.
///
/// The raw value is kept: execution coerces it exactly once while building the
/// row, and an already-coerced `date` / `datetime` / `decimal` cannot be
/// coerced a second time. Validation still happens here so an unusable
/// initializer fails before any statement of the program runs.
fn eval_value(expr: &VosExpr, ty: &ResolvedCanonicalType, contract: &ResolvedContract, params: &[Value]) -> Result<Value> {
    match expr {
        VosExpr::Literal(literal) => {
            let raw = match literal {
                VosLiteral::Null => Value::Null,
                VosLiteral::Bool(value) => Value::Bool(*value),
                VosLiteral::Int(text) => Value::I64(text.parse().map_err(|_| lower_error(format!("invalid integer literal `{text}`")))?),
                VosLiteral::String(text) => Value::Text(text.clone()),
                VosLiteral::Float(text) => Value::F64(text.parse().map_err(|_| lower_error(format!("invalid REAL literal `{text}`")))?),
            };
            coerce::value_to_scalar(raw.clone(), ty, contract)?;
            Ok(raw)
        }
        VosExpr::Name { name, .. } => {
            let Some(slot) = name.strip_prefix(PARAM_PREFIX)
            else {
                return Err(lower_error("Phase 1 execute only supports literal field initializers and bound parameters"));
            };
            let index: usize = slot.parse().map_err(|_| lower_error(format!("malformed parameter slot `{name}`")))?;
            let value = params.get(index).ok_or_else(|| lower_error(format!("parameter slot {index} has no bound value")))?;
            coerce::value_to_scalar(value.clone(), ty, contract)?;
            Ok(value.clone())
        }
        _ => Err(lower_error("Phase 1 execute only supports literal field initializers and bound parameters")),
    }
}

/// Replace every `Let`-bound name with the expression it names.
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
        VosExpr::TypedObject { ty, ty_span, fields, span } => {
            let mut expanded = Vec::with_capacity(fields.len());
            for init in fields {
                expanded.push(VosFieldInit {
                    name: init.name.clone(),
                    name_span: init.name_span.clone(),
                    value: init.value.as_ref().map(|value| expand_bindings(value, bindings)).transpose()?,
                    span: init.span.clone(),
                });
            }
            Ok(VosExpr::TypedObject { ty: ty.clone(), ty_span: ty_span.clone(), fields: expanded, span: span.clone() })
        }
        VosExpr::AnonObject { fields, span } => {
            let mut expanded = Vec::with_capacity(fields.len());
            for init in fields {
                expanded.push(VosFieldInit {
                    name: init.name.clone(),
                    name_span: init.name_span.clone(),
                    value: init.value.as_ref().map(|value| expand_bindings(value, bindings)).transpose()?,
                    span: init.span.clone(),
                });
            }
            Ok(VosExpr::AnonObject { fields: expanded, span: span.clone() })
        }
        other => Ok(other.clone()),
    }
}
