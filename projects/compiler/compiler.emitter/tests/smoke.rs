//! Minimal smoke for the emitter crate.
//!
//! Detailed target conformance lives in the consuming product repository; this file only
//! proves the crate links and its shared contracts answer on the public surface.

use compiler_emitter::{
    BackendDispatchKind, BackendInputShape, BackendRoute, bundled_backend_capability_descriptor, validate_backend_input, validate_dispatch_for_route,
};
use compiler_target::TargetBackendFamily;

#[test]
fn bundled_capabilities_answer_for_managed_and_wasm_targets() {
    for family in [TargetBackendFamily::Jvm, TargetBackendFamily::Clr, TargetBackendFamily::Wasm] {
        let descriptor = bundled_backend_capability_descriptor(family).expect("bundled descriptor");
        assert_eq!(descriptor.backend_family, family);
    }
}

#[test]
fn backend_boundary_rules_fail_closed() {
    assert!(validate_backend_input(BackendInputShape::default()).is_ok());
    assert!(validate_backend_input(BackendInputShape { contains_open_row_evidence: true, ..BackendInputShape::default() }).is_err());
    assert!(validate_dispatch_for_route(BackendRoute::StaticOnly, BackendDispatchKind::Static).is_ok());
    assert!(validate_dispatch_for_route(BackendRoute::StaticOnly, BackendDispatchKind::EffectHandler).is_err());
}
