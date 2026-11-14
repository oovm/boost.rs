//! Dejavu core types: values, errors, i18n — no Oak.
//!
//! Parsing, IR, rendering and AOT codegen live in `template-input-dejavu`.

#![deny(missing_docs)]

pub mod errors;
pub mod i18n;
pub mod types;
pub mod values;

pub use errors::{CompileError, DejavuError, DejavuResult, RuntimeError};
pub use i18n::{SimpleTranslator, TranslationData, TranslationProvider};
pub use types::*;
pub use values::{Context, IntoValue, Value};
