use hardware_emitter::{CodegenOptions, TargetIsa, encode_program_traced, encode_shader_traced};
use hardware_harness::{AnalysisOptions, EventKind, FindingCode, Tracer, analyze};
use hardware_program::{Expr, Function, Program, Stmt, Ty};
use hardware_types::ShaderProgram;

#[test]
fn traces_successful_emission() {
    let mut tracer = Tracer::new();
    let module = encode_shader_traced(&ShaderProgram::dispatch_fill("ucf_fill", 1.0), TargetIsa::Ptx, &mut tracer).expect("PTX");
    assert!(!module.bytes.is_empty());

    let trace = tracer.finish();
    assert!(trace.events.iter().any(|event| matches!(event.kind, EventKind::Emitted { .. })));
    assert!(trace.spans.iter().any(|span| span.name == "ptx"));
    assert!(analyze(&trace, &AnalysisOptions::default()).is_empty());
}

#[test]
fn traces_unsupported_program_encoding() {
    let program = Program {
        version: 1,
        functions: vec![Function {
            name: "fill".into(),
            params: Vec::new(),
            locals: Vec::new(),
            body: vec![Stmt::Return(Expr::ConstF32(1.0))],
            ret: Ty::f32(),
        }],
        entry: 0,
    };
    let mut tracer = Tracer::new();
    let error =
        encode_program_traced(&program, "fill", TargetIsa::Dxbc, CodegenOptions::default(), &mut tracer).expect_err("no DXBC program encoder");
    assert!(error.to_string().contains("unsupported"));

    let trace = tracer.finish();
    assert!(trace.events.iter().any(|event| matches!(event.kind, EventKind::EmitFailed { .. })));
    let findings = analyze(&trace, &AnalysisOptions::default());
    assert!(findings.iter().any(|finding| finding.code == FindingCode::EmitFailure));
}
