use hardware_optimize::{OptimizeOptions, optimize_program};
use hardware_program::{BinOp, CpuInterpreter, Expr, Function, Program, Stmt, Ty, UnOp, Value, validate_program};

fn program_with(body: Vec<Stmt>, locals: Vec<(u32, Ty)>, ret: Ty) -> Program {
    Program { version: 1, functions: vec![Function { name: "optimized".into(), params: Vec::new(), locals, body, ret }], entry: 0 }
}

#[test]
fn folds_constant_arithmetic_and_comparisons() {
    let body = vec![Stmt::Return(Expr::BinOp {
        op: BinOp::Add,
        lhs: Box::new(Expr::BinOp { op: BinOp::Mul, lhs: Box::new(Expr::ConstF32(2.0)), rhs: Box::new(Expr::ConstF32(3.0)) }),
        rhs: Box::new(Expr::ConstF32(1.0)),
    })];
    let program = program_with(body, Vec::new(), Ty::f32());
    let optimized = optimize_program(&program, OptimizeOptions::all());

    assert_eq!(optimized.functions[0].body, vec![Stmt::Return(Expr::ConstF32(7.0))]);
    validate_program(&optimized).expect("optimized program stays valid");
    let result = CpuInterpreter::new().run(&optimized).expect("CPU execution");
    assert_eq!(result.value, Value::F32(7.0));
}

#[test]
fn folds_unary_math() {
    let body = vec![Stmt::Return(Expr::UnOp { op: UnOp::Sqrt, arg: Box::new(Expr::ConstF64(9.0)) })];
    let program = program_with(body, Vec::new(), Ty::f64());
    let optimized = optimize_program(&program, OptimizeOptions::all());
    assert_eq!(optimized.functions[0].body, vec![Stmt::Return(Expr::ConstF64(3.0))]);
}

#[test]
fn applies_exact_integer_identities_only() {
    let body = vec![Stmt::Return(Expr::BinOp { op: BinOp::Add, lhs: Box::new(Expr::LoadUniform(0)), rhs: Box::new(Expr::ConstU32(0)) })];
    let program = program_with(body, Vec::new(), Ty::f32());
    let optimized = optimize_program(&program, OptimizeOptions::all());
    assert_eq!(optimized.functions[0].body, vec![Stmt::Return(Expr::LoadUniform(0))]);
}

#[test]
fn keeps_float_addition_by_zero() {
    let body = vec![Stmt::Return(Expr::BinOp { op: BinOp::Add, lhs: Box::new(Expr::LoadUniform(0)), rhs: Box::new(Expr::ConstF32(0.0)) })];
    let program = program_with(body, Vec::new(), Ty::f32());
    let optimized = optimize_program(&program, OptimizeOptions::all());
    assert!(matches!(optimized.functions[0].body.as_slice(), [Stmt::Return(Expr::BinOp { op: BinOp::Add, .. })]));
}

#[test]
fn drops_unused_lets_and_their_slots() {
    let body = vec![
        Stmt::Let { id: 0, ty: Ty::f32(), init: Expr::ConstF32(1.0) },
        Stmt::Let { id: 1, ty: Ty::f32(), init: Expr::ConstF32(2.0) },
        Stmt::Return(Expr::LoadLocal(1)),
    ];
    let program = program_with(body, vec![(0, Ty::f32()), (1, Ty::f32())], Ty::f32());
    let optimized = optimize_program(&program, OptimizeOptions::all());

    assert_eq!(
        optimized.functions[0].body,
        vec![Stmt::Let { id: 1, ty: Ty::f32(), init: Expr::ConstF32(2.0) }, Stmt::Return(Expr::LoadLocal(1))]
    );
    assert_eq!(optimized.functions[0].locals.len(), 1);
    validate_program(&optimized).expect("dropping dead lets keeps the program valid");
}

#[test]
fn keeps_locals_written_through_store_local() {
    let body = vec![
        Stmt::Let { id: 0, ty: Ty::f32(), init: Expr::ConstF32(1.0) },
        Stmt::StoreLocal { id: 0, value: Expr::ConstF32(2.0) },
        Stmt::Return(Expr::ConstF32(0.0)),
    ];
    let program = program_with(body, vec![(0, Ty::f32())], Ty::f32());
    let optimized = optimize_program(&program, OptimizeOptions::all());
    validate_program(&optimized).expect("store to a declated local stays valid");
    assert_eq!(optimized.functions[0].locals.len(), 1);
}

#[test]
fn disabled_options_preserve_the_program() {
    let body = vec![Stmt::Return(Expr::BinOp { op: BinOp::Add, lhs: Box::new(Expr::ConstF32(1.0)), rhs: Box::new(Expr::ConstF32(2.0)) })];
    let program = program_with(body.clone(), Vec::new(), Ty::f32());
    let optimized = optimize_program(&program, OptimizeOptions::none());
    assert_eq!(optimized.functions[0].body, body);
}
