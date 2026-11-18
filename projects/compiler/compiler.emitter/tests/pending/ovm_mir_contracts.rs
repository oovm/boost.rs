pub use compiler_emitter::{FragmentSubmission, testing::lower_fragment_to_ovm_module as lower_fragment_mir_to_ovm_module};

mod tests {
    include!("../src/lowering/backends/ovm/mir/tests.rs");
}
