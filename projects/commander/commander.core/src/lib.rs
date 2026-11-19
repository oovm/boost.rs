#![doc = include_str!("../readme.md")]
#![deny(missing_docs)]

mod context;
mod diag;
mod env;
mod help;
mod parse;
mod ports;
mod prompt_api;
mod routing;
mod spec;

pub use commander_types::{
    ActionRequiredDescriptor, CommanderError, ContinuationSpec, ExitCode, InteractiveSpec,
    NextAction, PromptPolicy, SecretString, error_id,
};

pub use crate::context::CommandContext;
pub use crate::diag::{DiagContext, Diagnostic, DiagnosticHandler, Severity};
pub use crate::env::Environment;
pub use crate::parse::ParsedOptions;
pub use crate::ports::{Clock, Localizer, Ports, SystemClock, Writer};
pub use crate::prompt_api::{
    AwaitActionOutcome, AwaitActionRequest, ConfirmRequest, InputRequest, InputValidationError,
    PasswordRequest, PromptApi, PromptFactory, PromptHooks, SelectChoice, SelectRequest,
};
pub use crate::routing::{Cli, RunOutcome, normalize_argv};
pub use crate::spec::{Coerce, Command, Handler, HandlerOutcome, OptionSpec, ValueDecl};
