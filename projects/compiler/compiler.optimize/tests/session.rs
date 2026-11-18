//! 优化会话门：理论合并、规则筛选、饱和统计与投影计划必须按提交事实返回。

use compiler_optimize::{
    CapabilityTag, FutamuraProjectionFamily, HostProjectionBoundary, Identifier, ObjectAlgebraicDimension, ObjectAlgebraicProgram,
    OptimizationRequest, OptimizationSession, ProjectionPolicy, QualifiedName, RewritePhase, RewriteRule, RewriteTheory, Term,
    TheoryBundle, builtin_core_term_rewrites, parse_term,
};
use compiler_types::ReferenceManagement;

fn name(parts: &[&str]) -> QualifiedName {
    QualifiedName::new(parts.iter().map(|part| Identifier::new(part)).collect())
}

fn policy() -> ProjectionPolicy {
    ProjectionPolicy {
        family: FutamuraProjectionFamily::Wasm,
        host_boundary: HostProjectionBoundary::WasmJsGlue,
        reference_management: ReferenceManagement::HostGc,
        prefer_small_artifacts: true,
        preserve_effect_boundaries: false,
    }
}

#[test]
fn theory_bundle_merges_shared_and_fragment_rules_once() {
    let rule = RewriteRule {
        name: Identifier::new("fragment.rule"),
        phase: RewritePhase::Saturate,
        required_capabilities: Vec::new(),
        allowed_projection_families: Vec::new(),
    };
    let mut shared = RewriteTheory::default();
    shared.register(rule.clone());
    let mut fragment = RewriteTheory::default();
    fragment.register(rule);
    let merged = TheoryBundle { shared, fragment }.merged();
    assert_eq!(merged.rules.len(), 1);
    assert_eq!(merged.count_in_phase(RewritePhase::Saturate), 1);
}

#[test]
fn session_reports_saturation_and_projection_plan() {
    let program = ObjectAlgebraicProgram {
        module_name: name(&["app"]),
        exports: vec![name(&["app", "render"])],
        dimensions: vec![ObjectAlgebraicDimension {
            name: Identifier::new("render"),
            exported_operations: vec![name(&["core", "add"])],
            required_capabilities: vec![CapabilityTag::new("host.fs")],
            reference_management_hint: None,
        }],
        structured_terms: vec![parse_term("core.add(0, x)").expect("valid term")],
    };
    let request = OptimizationRequest {
        program,
        capabilities: vec![CapabilityTag::new("host.fs")],
        rewrite_theory: RewriteTheory { term_rewrites: builtin_core_term_rewrites(), ..RewriteTheory::default() },
        projection_policy: policy(),
    };
    let result = OptimizationSession.optimize(request);
    assert_eq!(result.egraph.operation_count, 1);
    assert!(result.egraph.saturated);
    assert_eq!(result.projection.family, FutamuraProjectionFamily::Wasm);
    assert_eq!(result.projection.host_boundary, HostProjectionBoundary::WasmJsGlue);
    assert_eq!(result.projection.preserved_capabilities, vec![CapabilityTag::new("host.fs")]);
    assert_eq!(result.program.structured_terms[0], Term::Symbol(name(&["x"])));
}

#[test]
fn projection_family_names_are_stable() {
    assert_eq!(FutamuraProjectionFamily::MachineCode.as_str(), "futa_machine_code");
    assert_eq!(FutamuraProjectionFamily::Ovm.as_str(), "futa_vm");
}
