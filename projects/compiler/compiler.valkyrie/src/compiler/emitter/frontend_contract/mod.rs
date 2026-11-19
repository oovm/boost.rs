//! Public frontend-facing contract facade.

pub mod control_flow_payload;
pub mod type_concretize;

pub use control_flow_payload::{ProtocolDiagnostic, validate_future_protocol, witness_bindings_for_effect_with_diagnostics};

pub use type_concretize::{ConcretizeError, concretize_mir_function_types, concretize_type};
