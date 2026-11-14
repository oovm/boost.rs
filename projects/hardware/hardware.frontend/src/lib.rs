//! UCF shader front-end.
//!
//! Authored shader text is parsed by `oak-wgsl` and lowered into the generic
//! UCF program IR. This crate never lexes shader source and never emits a
//! target ISA.

mod error;
mod lower;

pub use error::FrontendError;
pub use lower::{LoweredShader, ResourceBinding, lower_wgsl};
