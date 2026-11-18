//! Wasm MIR contract tests. Included bodies construct [`ExecutableFunction`] directly.

pub use compiler_target::{Identifier, QualifiedName};
pub use compiler_emitter::FragmentSubmission;

pub mod contracts {
    pub use compiler_emitter::contracts::*;
}

pub mod executable_provider {
    pub use compiler_emitter::executable_provider::*;
}

pub mod testing {
    pub use compiler_emitter::testing::*;
}

pub fn lower_fragment_mir_to_wasm_module(
    submission: &FragmentSubmission,
    export_name: &str,
) -> compiler_emitter::emitter_backend_wasi::WasmBinaryModule {
    testing::lower_fragment_to_wasm_mir_module(submission, export_name)
}

/// Returns the number of stable Node host imports injected for a module with no
/// source-declared imports. The list is part of the JS-glue ABI contract.
pub fn import_count_for_main() -> u8 {
    12
}

pub use testing::WASM_GC_ANYREF;

mod tests {
    use super::*;
    include!("../src/lowering/backends/wasm/mir/tests.rs");
}
