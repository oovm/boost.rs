//! Target-neutral optimization passes over the UCF program IR.
//!
//! These passes run once, before any ISA encoder, so every backend observes the
//! same optimized program instead of re-implementing folding or dead-code
//! removal per target.

use std::collections::HashSet;

use hardware_program::{BinOp, Expr, Function, Program, ScalarTy, Stmt, Ty, UnOp};

/// Selects which program-level passes run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptimizeOptions {
    /// Fold constant expressions.
    pub constant_folding: bool,
    /// Apply exact algebraic identities.
    pub algebra: bool,
    /// Drop unused `let` bindings and their slots.
    pub dead_lets: bool,
}

impl Default for OptimizeOptions {
    fn default() -> Self {
        Self { constant_folding: true, algebra: true, dead_lets: true }
    }
}

impl OptimizeOptions {
    /// All passes enabled.
    pub fn all() -> Self {
        Self::default()
    }

    /// All passes disabled.
    pub fn none() -> Self {
        Self { constant_folding: false, algebra: false, dead_lets: false }
    }
}

/// Returns an optimized copy of `program`.
///
/// The result is structurally valid for every function that was valid on input.
/// Constructs the passes do not understand are preserved unchanged.
pub fn optimize_program(program: &Program, options: OptimizeOptions) -> Program {
    let functions = program.functions.iter().map(|function| optimize_function(function, options)).collect();
    Program { version: program.version, functions, entry: program.entry }
}

fn optimize_function(function: &Function, options: OptimizeOptions) -> Function {
    let mut body = function.body.clone();
    rewrite_statements(&mut body, options);
    if options.dead_lets {
        body = drop_dead_lets(body);
    }
    let live = live_locals(&body);
    let locals = function.locals.iter().filter(|(id, _)| live.contains(id)).cloned().collect();
    Function { name: function.name.clone(), params: function.params.clone(), locals, body, ret: function.ret.clone() }
}

fn rewrite_statements(statements: &mut [Stmt], options: OptimizeOptions) {
    for statement in statements {
        match statement {
            Stmt::Let { init, .. } => rewrite_expr(init, options),
            Stmt::StoreLocal { value, .. } => rewrite_expr(value, options),
            Stmt::StoreBuffer { index, value, .. } => {
                rewrite_expr(index, options);
                rewrite_expr(value, options);
            }
            Stmt::If { cond, then_body, else_body } => {
                rewrite_expr(cond, options);
                rewrite_statements(then_body, options);
                rewrite_statements(else_body, options);
            }
            Stmt::For { body, .. } => rewrite_statements(body, options),
            Stmt::While { cond, body, .. } => {
                rewrite_expr(cond, options);
                rewrite_statements(body, options);
            }
            Stmt::Return(expr) | Stmt::Expr(expr) => rewrite_expr(expr, options),
            Stmt::Break | Stmt::Continue | Stmt::Cfg { .. } => {}
        }
    }
}

fn rewrite_expr(expr: &mut Expr, options: OptimizeOptions) {
    match expr {
        Expr::BinOp { op, lhs, rhs } => {
            rewrite_expr(lhs, options);
            rewrite_expr(rhs, options);
            if options.constant_folding {
                if let Some(folded) = fold_binary(*op, lhs, rhs) {
                    *expr = folded;
                    return;
                }
            }
            if options.algebra {
                if let Some(simplified) = simplify_binary(*op, lhs, rhs) {
                    *expr = simplified;
                }
            }
        }
        Expr::UnOp { op, arg } => {
            rewrite_expr(arg, options);
            if options.constant_folding {
                if let Some(folded) = fold_unary(*op, arg) {
                    *expr = folded;
                }
            }
        }
        Expr::Select { cond, then_, else_ } => {
            rewrite_expr(cond, options);
            rewrite_expr(then_, options);
            rewrite_expr(else_, options);
        }
        Expr::Vec3Compose { x, y, z } => {
            rewrite_expr(x, options);
            rewrite_expr(y, options);
            rewrite_expr(z, options);
        }
        Expr::LoadBuffer { index, .. } => rewrite_expr(index, options),
        Expr::Call { args, .. } => {
            for arg in args {
                rewrite_expr(arg, options);
            }
        }
        Expr::ConstF32(_)
        | Expr::ConstF64(_)
        | Expr::ConstBool(_)
        | Expr::ConstU32(_)
        | Expr::LoadLocal(_)
        | Expr::LoadUniform(_)
        | Expr::GlobalId => {}
    }
}

fn fold_binary(op: BinOp, lhs: &Expr, rhs: &Expr) -> Option<Expr> {
    match (lhs, rhs) {
        (Expr::ConstF32(lhs), Expr::ConstF32(rhs)) => match op {
            BinOp::Add => Some(Expr::ConstF32(lhs + rhs)),
            BinOp::Sub => Some(Expr::ConstF32(lhs - rhs)),
            BinOp::Mul => Some(Expr::ConstF32(lhs * rhs)),
            BinOp::Div if *rhs != 0.0 => Some(Expr::ConstF32(lhs / rhs)),
            BinOp::Mod if *rhs != 0.0 => Some(Expr::ConstF32(lhs % rhs)),
            BinOp::Min => Some(Expr::ConstF32(lhs.min(*rhs))),
            BinOp::Max => Some(Expr::ConstF32(lhs.max(*rhs))),
            BinOp::Pow => Some(Expr::ConstF32(lhs.powf(*rhs))),
            BinOp::Lt => Some(Expr::ConstBool(lhs < rhs)),
            BinOp::Le => Some(Expr::ConstBool(lhs <= rhs)),
            BinOp::Eq => Some(Expr::ConstBool(lhs == rhs)),
            BinOp::Ne => Some(Expr::ConstBool(lhs != rhs)),
            BinOp::Ge => Some(Expr::ConstBool(lhs >= rhs)),
            BinOp::Gt => Some(Expr::ConstBool(lhs > rhs)),
            _ => None,
        },
        (Expr::ConstF64(lhs), Expr::ConstF64(rhs)) => match op {
            BinOp::Add => Some(Expr::ConstF64(lhs + rhs)),
            BinOp::Sub => Some(Expr::ConstF64(lhs - rhs)),
            BinOp::Mul => Some(Expr::ConstF64(lhs * rhs)),
            BinOp::Div if *rhs != 0.0 => Some(Expr::ConstF64(lhs / rhs)),
            BinOp::Mod if *rhs != 0.0 => Some(Expr::ConstF64(lhs % rhs)),
            BinOp::Min => Some(Expr::ConstF64(lhs.min(*rhs))),
            BinOp::Max => Some(Expr::ConstF64(lhs.max(*rhs))),
            BinOp::Pow => Some(Expr::ConstF64(lhs.powf(*rhs))),
            BinOp::Lt => Some(Expr::ConstBool(lhs < rhs)),
            BinOp::Le => Some(Expr::ConstBool(lhs <= rhs)),
            BinOp::Eq => Some(Expr::ConstBool(lhs == rhs)),
            BinOp::Ne => Some(Expr::ConstBool(lhs != rhs)),
            BinOp::Ge => Some(Expr::ConstBool(lhs >= rhs)),
            BinOp::Gt => Some(Expr::ConstBool(lhs > rhs)),
            _ => None,
        },
        (Expr::ConstU32(lhs), Expr::ConstU32(rhs)) => match op {
            BinOp::Add => lhs.checked_add(*rhs).map(Expr::ConstU32),
            BinOp::Sub => lhs.checked_sub(*rhs).map(Expr::ConstU32),
            BinOp::Mul => lhs.checked_mul(*rhs).map(Expr::ConstU32),
            BinOp::Div if *rhs != 0 => Some(Expr::ConstU32(lhs / rhs)),
            BinOp::Mod if *rhs != 0 => Some(Expr::ConstU32(lhs % rhs)),
            BinOp::Min => Some(Expr::ConstU32(*lhs.min(rhs))),
            BinOp::Max => Some(Expr::ConstU32(*lhs.max(rhs))),
            BinOp::Lt => Some(Expr::ConstBool(lhs < rhs)),
            BinOp::Le => Some(Expr::ConstBool(lhs <= rhs)),
            BinOp::Eq => Some(Expr::ConstBool(lhs == rhs)),
            BinOp::Ne => Some(Expr::ConstBool(lhs != rhs)),
            BinOp::Ge => Some(Expr::ConstBool(lhs >= rhs)),
            BinOp::Gt => Some(Expr::ConstBool(lhs > rhs)),
            _ => None,
        },
        (Expr::ConstBool(lhs), Expr::ConstBool(rhs)) => match op {
            BinOp::Eq => Some(Expr::ConstBool(lhs == rhs)),
            BinOp::Ne => Some(Expr::ConstBool(lhs != rhs)),
            _ => None,
        },
        _ => None,
    }
}

fn fold_unary(op: UnOp, arg: &Expr) -> Option<Expr> {
    match (op, arg) {
        (UnOp::Neg, Expr::ConstF32(value)) => Some(Expr::ConstF32(-value)),
        (UnOp::Neg, Expr::ConstF64(value)) => Some(Expr::ConstF64(-value)),
        (UnOp::Sqrt, Expr::ConstF32(value)) if *value >= 0.0 => Some(Expr::ConstF32(value.sqrt())),
        (UnOp::Sqrt, Expr::ConstF64(value)) if *value >= 0.0 => Some(Expr::ConstF64(value.sqrt())),
        (UnOp::Sin, Expr::ConstF32(value)) => Some(Expr::ConstF32(value.sin())),
        (UnOp::Sin, Expr::ConstF64(value)) => Some(Expr::ConstF64(value.sin())),
        (UnOp::Cos, Expr::ConstF32(value)) => Some(Expr::ConstF32(value.cos())),
        (UnOp::Cos, Expr::ConstF64(value)) => Some(Expr::ConstF64(value.cos())),
        (UnOp::Abs, Expr::ConstF32(value)) => Some(Expr::ConstF32(value.abs())),
        (UnOp::Abs, Expr::ConstF64(value)) => Some(Expr::ConstF64(value.abs())),
        (UnOp::Ln, Expr::ConstF32(value)) if *value > 0.0 => Some(Expr::ConstF32(value.ln())),
        (UnOp::Ln, Expr::ConstF64(value)) if *value > 0.0 => Some(Expr::ConstF64(value.ln())),
        (UnOp::IsFinite, Expr::ConstF32(value)) => Some(Expr::ConstBool(value.is_finite())),
        (UnOp::IsFinite, Expr::ConstF64(value)) => Some(Expr::ConstBool(value.is_finite())),
        _ => None,
    }
}

fn simplify_binary(op: BinOp, lhs: &Expr, rhs: &Expr) -> Option<Expr> {
    match (op, lhs, rhs) {
        (BinOp::Add, value, Expr::ConstU32(0)) | (BinOp::Add, Expr::ConstU32(0), value) => Some(value.clone()),
        (BinOp::Sub, value, Expr::ConstU32(0)) => Some(value.clone()),
        (BinOp::Mul, value, Expr::ConstU32(1)) | (BinOp::Mul, Expr::ConstU32(1), value) => Some(value.clone()),
        (BinOp::Div, value, Expr::ConstU32(1)) => Some(value.clone()),
        (BinOp::Mul, value, Expr::ConstF32(value_one)) if *value_one == 1.0 => Some(value.clone()),
        (BinOp::Mul, Expr::ConstF32(value_one), value) if *value_one == 1.0 => Some(value.clone()),
        (BinOp::Div, value, Expr::ConstF32(value_one)) if *value_one == 1.0 => Some(value.clone()),
        _ => None,
    }
}

fn drop_dead_lets(mut body: Vec<Stmt>) -> Vec<Stmt> {
    loop {
        let used = live_locals(&body);
        let before = body.len();
        prune_statements(&mut body, &used);
        if body.len() == before {
            return body;
        }
    }
}

fn prune_statements(statements: &mut Vec<Stmt>, used: &HashSet<u32>) {
    statements.retain(|statement| !matches!(statement, Stmt::Let { id, .. } if !used.contains(id)));
    for statement in statements.iter_mut() {
        match statement {
            Stmt::If { then_body, else_body, .. } => {
                prune_statements(then_body, used);
                prune_statements(else_body, used);
            }
            Stmt::For { body, .. } | Stmt::While { body, .. } => prune_statements(body, used),
            _ => {}
        }
    }
}

fn live_locals(body: &[Stmt]) -> HashSet<u32> {
    let mut used = HashSet::new();
    collect_statement_uses(body, &mut used);
    used
}

fn collect_statement_uses(statements: &[Stmt], used: &mut HashSet<u32>) {
    for statement in statements {
        match statement {
            Stmt::Let { init, .. } => collect_expr_uses(init, used),
            Stmt::StoreLocal { id, value } => {
                used.insert(*id);
                collect_expr_uses(value, used);
            }
            Stmt::StoreBuffer { index, value, .. } => {
                collect_expr_uses(index, used);
                collect_expr_uses(value, used);
            }
            Stmt::If { cond, then_body, else_body } => {
                collect_expr_uses(cond, used);
                collect_statement_uses(then_body, used);
                collect_statement_uses(else_body, used);
            }
            Stmt::For { iter, body, .. } => {
                used.insert(*iter);
                collect_statement_uses(body, used);
            }
            Stmt::While { cond, body, .. } => {
                collect_expr_uses(cond, used);
                collect_statement_uses(body, used);
            }
            Stmt::Return(expr) | Stmt::Expr(expr) => collect_expr_uses(expr, used),
            Stmt::Break | Stmt::Continue | Stmt::Cfg { .. } => {}
        }
    }
}

fn collect_expr_uses(expr: &Expr, used: &mut HashSet<u32>) {
    match expr {
        Expr::LoadLocal(id) => {
            used.insert(*id);
        }
        Expr::BinOp { lhs, rhs, .. } => {
            collect_expr_uses(lhs, used);
            collect_expr_uses(rhs, used);
        }
        Expr::UnOp { arg, .. } => collect_expr_uses(arg, used),
        Expr::Select { cond, then_, else_ } => {
            collect_expr_uses(cond, used);
            collect_expr_uses(then_, used);
            collect_expr_uses(else_, used);
        }
        Expr::Vec3Compose { x, y, z } => {
            collect_expr_uses(x, used);
            collect_expr_uses(y, used);
            collect_expr_uses(z, used);
        }
        Expr::LoadBuffer { index, .. } => collect_expr_uses(index, used),
        Expr::Call { args, .. } => {
            for arg in args {
                collect_expr_uses(arg, used);
            }
        }
        Expr::ConstF32(_) | Expr::ConstF64(_) | Expr::ConstBool(_) | Expr::ConstU32(_) | Expr::LoadUniform(_) | Expr::GlobalId => {}
    }
}

/// Reports whether `ty` is a scalar type the passes can fold.
pub fn is_foldable_scalar(ty: &Ty) -> bool {
    matches!(ty, Ty::Scalar(ScalarTy::F32 | ScalarTy::F64 | ScalarTy::U32 | ScalarTy::Bool))
}
