use std::{collections::BTreeMap, fmt};

use crate::ExitCode;

/// Stable framework error ids and their frozen exit mapping.
///
/// Ids are separated from localized text: hosts localize by id plus typed
/// safe args, and automation matches on the id, never on prose.
pub mod error_id {
    use super::ExitCode;

    /// Parsing or dispatch resolved to a command that does not exist.
    pub const UNKNOWN_COMMAND: &str = "commander::unknown_command";
    /// An option token does not exist on the resolved command.
    pub const UNKNOWN_OPTION: &str = "commander::unknown_option";
    /// An option that takes a value was not followed by one.
    pub const MISSING_OPTION_VALUE: &str = "commander::missing_option_value";
    /// The argv stream is structurally invalid.
    pub const PARSE_FAILED: &str = "commander::parse_failed";
    /// A required option is absent from the machine input.
    pub const MISSING_REQUIRED_OPTION: &str = "commander::missing_required_option";
    /// A supplied value is outside the declared enum.
    pub const OPTION_NOT_IN_ENUM: &str = "commander::option_not_in_enum";
    /// A supplied value cannot be coerced to the declared type.
    pub const OPTION_COERCE_FAILED: &str = "commander::option_coerce_failed";
    /// An interactive prompt was attempted where prompting is not allowed.
    pub const INTERACTIVE_UNAVAILABLE: &str = "commander::interactive_unavailable";
    /// Another prompt already owns the single active prompt lease.
    pub const INTERACTION_BUSY: &str = "commander::interaction_busy";
    /// The terminal cannot guarantee echo-free input, so no secret was read.
    pub const PASSWORD_UNSUPPORTED: &str = "commander::password_unsupported";
    /// A started prompt ended through EOF or Ctrl+C.
    pub const PROMPT_CANCELED: &str = "commander::prompt_canceled";
    /// A started prompt exceeded its absolute deadline.
    pub const PROMPT_TIMEOUT: &str = "commander::prompt_timeout";
    /// A domain action failed after its inputs were accepted.
    pub const ACTION_FAILED: &str = "commander::action_failed";

    /// Maps a stable framework error id to its frozen exit code.
    ///
    /// Unknown ids map to [`ExitCode::Failure`]; domain codes keep their own
    /// meaning and exit codes outside this framework table.
    pub fn exit_for(id: &str) -> ExitCode {
        match id {
            UNKNOWN_COMMAND
            | UNKNOWN_OPTION
            | MISSING_OPTION_VALUE
            | PARSE_FAILED
            | MISSING_REQUIRED_OPTION
            | OPTION_NOT_IN_ENUM
            | OPTION_COERCE_FAILED => ExitCode::Usage,
            INTERACTIVE_UNAVAILABLE | INTERACTION_BUSY | PASSWORD_UNSUPPORTED => ExitCode::Capability,
            PROMPT_CANCELED | PROMPT_TIMEOUT => ExitCode::Canceled,
            ACTION_FAILED => ExitCode::Failure,
            _ => ExitCode::Failure,
        }
    }
}

/// A structured commander error carrying only safe, typed arguments.
///
/// The error never carries secret values: `args` must only hold values that
/// are safe to localize and to place into logs or envelopes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommanderError {
    /// Stable framework error id.
    pub id: String,
    /// Frozen exit code for this error.
    pub exit: ExitCode,
    /// Safe typed arguments for localization. Never carries secret values.
    pub args: BTreeMap<String, String>,
}

impl CommanderError {
    /// Creates an error with an explicit exit code and no arguments.
    pub fn new(id: impl Into<String>, exit: ExitCode) -> Self {
        Self { id: id.into(), exit, args: BTreeMap::new() }
    }

    /// Creates an error whose exit code is the frozen mapping of its id.
    pub fn from_id(id: &str) -> Self {
        Self::new(id, error_id::exit_for(id))
    }

    /// Adds one safe typed argument, replacing any previous value for the key.
    pub fn with_arg(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.args.insert(key.into(), value.into());
        self
    }
}

impl fmt::Display for CommanderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.id)
    }
}

impl std::error::Error for CommanderError {}
