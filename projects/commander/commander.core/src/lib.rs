#![doc = include_str!("readme.md")]
#![warn(missing_docs)]

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
    ActionRequiredDescriptor, CommanderError, ContinuationSpec, ExitCode, InteractiveSpec, NextAction, PromptPolicy, SecretString, error_id,
};

pub use crate::{
    context::CommandContext,
    diag::{DiagContext, Diagnostic, DiagnosticHandler, Severity},
    env::Environment,
    parse::ParsedOptions,
    ports::{Clock, Localizer, Ports, SystemClock, Writer},
    prompt_api::{
        AwaitActionOutcome, AwaitActionRequest, ConfirmRequest, InputRequest, InputValidationError, PasswordRequest, PromptApi, PromptFactory,
        PromptHooks, SelectChoice, SelectRequest,
    },
    routing::{Cli, RunOutcome, normalize_argv},
    spec::{Coerce, Command, Handler, HandlerOutcome, OptionSpec, ValueDecl},
};
