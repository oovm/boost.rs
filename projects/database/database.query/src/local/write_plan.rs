//! Write-program recognition and schema-macro expansion.
//!
//! These helpers work on the parsed VOS program only: they decide whether a
//! program is a mutation and replace schema macros with their declared bodies.
//! Applying the resulting writes is execution work and lives in
//! `database_execution::local`.

use schema_input_vos::syntax::{VosDeclarationKind, VosExpr, VosProgram, VosRoot, VosStmt};

use database_types::{Error, Result};

fn plan_error(message: impl Into<String>) -> Error {
    Error::Schema { message: message.into() }
}

/// Quick AST-level check: does this VOS program look like a write / mutation
/// (insert, update, delete, macro call)?
///
/// [`crate::local::prepare`] uses it to short-circuit DML programs past the
/// read-pipeline lowerer.
pub fn is_write_program(program: &VosProgram) -> bool {
    let is_write = |expr: &VosExpr| is_macro_call(expr) || is_write_call(expr);
    for stmt in &program.statements {
        if let VosStmt::Expr(expr) = stmt {
            if is_write(expr) {
                return true;
            }
        }
    }
    if let Some(result) = &program.result {
        if is_write(result) {
            return true;
        }
    }
    false
}

/// Replace every schema-macro call with the body the schema document declares.
///
/// Write binding runs this before it decides whether a program is a write: a
/// macro call is not a `.insert(…)` until it has been expanded, and an
/// unexpandable macro must stay an error rather than become an accepted write.
pub fn expand_program(program: &VosProgram, schema_document: Option<&str>) -> Result<VosProgram> {
    let mut statements = Vec::new();
    for stmt in &program.statements {
        match stmt {
            VosStmt::Let(_) => statements.push(stmt.clone()),
            VosStmt::Expr(expr) => expand_expr_into_program(expr, schema_document, &mut statements)?,
        }
    }

    let result = match &program.result {
        None => None,
        Some(expr) if is_macro_call(expr) => {
            let body = expand_macro_call(expr, schema_document)?;
            for stmt in body.statements {
                match stmt {
                    VosStmt::Expr(inner) => {
                        expand_expr_into_program(&inner, schema_document, &mut statements)?;
                    }
                    VosStmt::Let(let_) => statements.push(VosStmt::Let(let_)),
                }
            }
            body.result
        }
        Some(expr) => Some(expr.clone()),
    };

    Ok(VosProgram { micros: program.micros.clone(), statements, result, span: program.span.clone() })
}

fn expand_expr_into_program(expr: &VosExpr, schema_document: Option<&str>, statements: &mut Vec<VosStmt>) -> Result<()> {
    if is_macro_call(expr) {
        let body = expand_macro_call(expr, schema_document)?;
        for stmt in body.statements {
            match stmt {
                VosStmt::Expr(inner) => {
                    expand_expr_into_program(&inner, schema_document, statements)?;
                }
                VosStmt::Let(let_) => statements.push(VosStmt::Let(let_)),
            }
        }
        if let Some(result) = body.result {
            if is_macro_call(&result) {
                expand_expr_into_program(&result, schema_document, statements)?;
            }
            else {
                statements.push(VosStmt::Expr(result));
            }
        }
        return Ok(());
    }
    statements.push(VosStmt::Expr(expr.clone()));
    Ok(())
}

fn expand_macro_call(expr: &VosExpr, schema_document: Option<&str>) -> Result<VosProgram> {
    let VosExpr::Call { callee, args, .. } = expr
    else {
        return Err(plan_error("expected macro call"));
    };
    let VosExpr::Name { name, .. } = callee.as_ref()
    else {
        return Err(plan_error("expected bare macro name call"));
    };
    if !args.is_empty() {
        return Err(plan_error(format!("Phase 1 macro expansion does not bind parameters for `{name}()`")));
    }
    let document = schema_document.ok_or_else(|| plan_error("macro expansion requires an installed schema document"))?;
    let document = data_text_vos::parse(document).map_err(|diagnostics| Error::Schema { message: diagnostics })?;
    find_macro_body(&document, name).ok_or_else(|| plan_error(format!("unknown schema macro `{name}`")))
}

fn find_macro_body(document: &VosRoot, name: &str) -> Option<VosProgram> {
    document
        .declarations
        .iter()
        .find(|declaration| declaration.kind == VosDeclarationKind::Macro && declaration.name.as_deref() == Some(name))
        .and_then(|declaration| declaration.body_program.clone())
}

/// Whether a program contains at least one `.insert(…)`, `.update(…)` or
/// `.delete(…)` expression.
///
/// Used by parameter binding to refuse a read pipeline before it can enter the
/// DML executor. Broader than [`is_write_program`], which only recognizes the
/// zero-argument `Type { … }.insert()` shape consumed by `prepare`.
pub fn has_write_call(program: &VosProgram) -> bool {
    program.statements.iter().any(|stmt| matches!(stmt, VosStmt::Expr(expr) if is_write_call(expr)))
        || program.result.as_ref().is_some_and(is_write_call)
}

/// Whether `expr` is a zero-argument call of a bare name, i.e. a schema macro.
pub fn is_macro_call(expr: &VosExpr) -> bool {
    matches!(
        expr,
        VosExpr::Call {
            callee,
            args,
            ..
        } if args.is_empty() && matches!(callee.as_ref(), VosExpr::Name { .. })
    )
}

/// Whether `expr` is `.insert(…)`, `.update(…)` or `.delete(…)`.
pub fn is_write_call(expr: &VosExpr) -> bool {
    matches!(
        expr,
        VosExpr::Call {
            callee,
            ..
        } if matches!(
            callee.as_ref(),
            VosExpr::Member { name, .. } if matches!(name.as_str(), "insert" | "update" | "delete")
        )
    )
}

