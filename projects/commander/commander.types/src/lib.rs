#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

mod action;
mod envelope;
mod error;
mod exit;
mod interactive;
mod secret;

pub use action::{ActionRequiredDescriptor, NextAction};
pub use envelope::{RESULT_SCHEMA, envelope_error, envelope_ok};
pub use error::{CommanderError, error_id};
pub use exit::ExitCode;
pub use interactive::{ContinuationSpec, InteractiveSpec, PromptPolicy};
pub use secret::SecretString;
