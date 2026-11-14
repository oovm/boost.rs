//! Language-neutral shader front-end.
//!
//! Parser crates (`hardware-input-wgsl`, `hardware-input-hlsl`,
//! `hardware-input-glsl`) convert their own syntax trees into [`ast::ShaderModule`]
//! and hand it to [`lower_module`]. This crate never parses shader text and never
//! emits a target ISA.

/// Language-neutral shader syntax.
pub mod ast;
mod error;
mod lower;

pub use error::FrontendError;
pub use lower::{LoweredShader, ResourceBinding, UniformBinding, lower_module};
