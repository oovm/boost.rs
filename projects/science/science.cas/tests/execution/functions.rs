//! Built-in unary function registry contract.

use science_cas::execution::builtins::registry::lookup_unary;
use science_ir::UnaryFunction;

#[test]
fn registry_contains_closed_unary_functions() {
    for f in [
        UnaryFunction::Exp,
        UnaryFunction::Sin,
        UnaryFunction::Sinh,
        UnaryFunction::ArcTan,
        UnaryFunction::Gamma,
        UnaryFunction::Erf,
        UnaryFunction::Abs,
        UnaryFunction::Sign,
    ] {
        assert!(lookup_unary(f).is_some(), "missing {f:?}");
    }
}
