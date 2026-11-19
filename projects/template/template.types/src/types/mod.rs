//! Type system helpers (`data.text`-free). Full AST typecheck removed with the external parser.

pub mod rust_integration;
pub mod type_annotation;
pub mod type_info;

pub use type_annotation::*;
pub use type_info::*;
