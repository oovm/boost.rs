//! JVM MIR contract tests. Included bodies construct [`ExecutableFunction`] directly
//! (no frontend MIR), so this crate root re-exports driver modules as `crate::*`.

pub use compiler_target::{Identifier, QualifiedName};
pub use compiler_emitter::{
    FragmentSubmission,
    emitter_backend_jvm::{JvmInstruction, JvmMethodDescriptor, JvmTypeDescriptor},
};
pub use ordered_float;

pub mod contracts {
    pub use compiler_emitter::contracts::*;
}

pub mod executable_provider {
    pub use compiler_emitter::executable_provider::*;
}

pub mod testing {
    pub use compiler_emitter::testing::*;
}

pub use compiler_emitter::testing::{JvmLocalKind, jvm_local_slot_conflicts};

pub fn lower_mir_function_to_jvm(
    submission: &FragmentSubmission,
    operation: &compiler_target::QualifiedName,
    mir_function: &contracts::ExecutableFunction,
) -> compiler_emitter::emitter_backend_jvm::JvmMethodSignature {
    testing::lower_mir_to_jvm_method(submission, operation, mir_function)
}

mod tests {
    use super::*;
    include!("../src/lowering/backends/jvm/mir/tests.rs");
}
