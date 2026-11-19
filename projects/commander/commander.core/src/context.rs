//! The per-run `CommandContext` handed to handlers.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use commander_types::{
    ActionRequiredDescriptor, CommanderError, ExitCode, SecretString, envelope_error, envelope_ok,
};
use serde_json::{Value, to_string_pretty};

use crate::parse::ParsedOptions;
use crate::ports::Ports;
use crate::prompt_api::{
    AwaitActionOutcome, AwaitActionRequest, ConfirmRequest, InputRequest, PasswordRequest,
    PromptApi, SelectRequest,
};
use crate::routing::RunState;
use crate::spec::{Command, HandlerOutcome};
use crate::{Cli, Diagnostic, Severity};

/// Everything a handler may do through the framework.
///
/// The context is the only channel to the outside: localization, stderr
/// diagnostics, result emission, prompting, and stdin claiming all go
/// through it. It never touches the real process directly.
pub struct CommandContext<'a> {
    /// `[cli name, command token, subcommand token, ...]`.
    pub command_path: Vec<String>,
    /// The command path joined with spaces, without the CLI name.
    pub command_name: String,
    /// Resolved BCP-47 locale tag for this run.
    pub locale: String,

    cli: &'a Cli,
    command: &'a Command,
    ports: &'a Ports,
    state: &'a RunState,
    options: ParsedOptions,
    interactive: bool,
    json: bool,
    prompt: Arc<dyn PromptApi>,
    stdin_claimed: Arc<AtomicBool>,
}

impl<'a> CommandContext<'a> {
    pub(crate) fn new(
        cli: &'a Cli,
        command: &'a Command,
        command_path: Vec<String>,
        ports: &'a Ports,
        state: &'a RunState,
        options: ParsedOptions,
        interactive: bool,
        prompt: Arc<dyn PromptApi>,
        stdin_claimed: Arc<AtomicBool>,
    ) -> Self {
        let command_name = command_path[1..].join(" ");
        Self {
            command_path,
            command_name,
            locale: ports.locale.clone(),
            cli,
            command,
            ports,
            state,
            options,
            interactive,
            json: state.json(),
            prompt,
            stdin_claimed,
        }
    }

    /// Resolves one locale key with typed safe arguments.
    pub fn t(&self, key: &str, args: &BTreeMap<String, String>) -> String {
        self.ports.localizer.t(key, args)
    }

    /// The parsed machine input for the resolved command.
    pub fn options(&self) -> &ParsedOptions {
        &self.options
    }

    /// The context-scoped prompt engine. Every call re-checks eligibility at
    /// acquire time and takes the single active prompt lease.
    pub fn prompt(&self) -> &dyn PromptApi {
        self.prompt.as_ref()
    }

    /// Convenience pass-throughs mirroring the TypeScript prompt API.
    pub fn confirm(&self, req: &ConfirmRequest) -> Result<bool, CommanderError> {
        self.prompt.confirm(req)
    }

    /// Asks for one non-secret line.
    pub fn input(&self, req: &InputRequest) -> Result<String, CommanderError> {
        self.prompt.input(req)
    }

    /// Asks for a numbered pick and returns the stable value.
    pub fn select(&self, req: &SelectRequest) -> Result<String, CommanderError> {
        self.prompt.select(req)
    }

    /// Asks for a secret with echo disabled.
    pub fn password(&self, req: &PasswordRequest) -> Result<SecretString, CommanderError> {
        self.prompt.password(req)
    }

    /// Waits for an external action or defers it. Outside interactivity the
    /// engine returns [`AwaitActionOutcome::ActionRequired`] with the resume
    /// descriptor; the handler maps it onto [`HandlerOutcome::ActionRequired`].
    pub fn await_action(&self, req: &AwaitActionRequest) -> Result<AwaitActionOutcome, CommanderError> {
        self.prompt.await_action(req)
    }

    /// The eligibility snapshot for this run.
    pub fn is_interactive(&self) -> bool {
        self.interactive
    }

    /// Declares that this command consumed stdin as a data source. After this
    /// every `prompt.*` call fails with `commander::interactive_unavailable`.
    pub fn claim_stdin(&self) {
        self.stdin_claimed.store(true, Ordering::SeqCst);
    }

    /// Whether [`Self::claim_stdin`] has been called.
    pub fn stdin_claimed(&self) -> bool {
        self.stdin_claimed.load(Ordering::SeqCst)
    }

    /// Emits the final result. JSON mode writes exactly one
    /// `wae.cli-result/v0` success envelope; human mode writes the `human`
    /// text or the pretty-printed data to the stdout payload.
    pub fn emit_result(&self, data: Value, human: Option<String>) {
        if self.json {
            self.state.write_envelope(envelope_ok(data));
        } else {
            let text = human.unwrap_or_else(|| {
                to_string_pretty(&data).unwrap_or_else(|_| data.to_string())
            });
            self.state.write_payload(text);
        }
    }

    /// Emits a typed failure. JSON mode writes one envelope; human mode
    /// writes `code: message` to stderr.
    pub fn emit_failure(
        &self,
        error: &CommanderError,
        descriptor: Option<&ActionRequiredDescriptor>,
    ) {
        let message = crate::routing::failure_message(self.ports, &error.id, &error.args);
        if self.json {
            self.state.write_envelope(envelope_error(&error.id, &message, descriptor));
        } else {
            self.ports.writer.write_line(&format!("{}: {}", error.id, message));
        }
    }

    /// Builds the exit-5 action-required outcome carrying the resume descriptor.
    pub fn action_required(&self, descriptor: ActionRequiredDescriptor) -> HandlerOutcome {
        let mut args = BTreeMap::new();
        args.insert("actionId".to_string(), descriptor.action_id.clone());
        let message = crate::diag::localized_or_id(self.ports.localizer.as_ref(), ACTION_REQUIRED_CODE, &args)
            .unwrap_or_else(|| ACTION_REQUIRED_CODE.to_string());
        HandlerOutcome::ActionRequired {
            code: ACTION_REQUIRED_CODE.to_string(),
            message,
            descriptor,
        }
    }

    /// Emits an error diagnostic and returns a `commander::action_failed`-style
    /// configuration failure (exit 1) carrying the given code and args.
    ///
    /// Mirrors the TypeScript `ctx.fail`: the handler must return the error.
    pub fn fail(&self, code: &str, args: BTreeMap<String, String>) -> CommanderError {
        self.emit_diagnostic(Diagnostic {
            severity: Severity::Error,
            code: code.to_string(),
            args: args.clone(),
            message: None,
        });
        let mut error = CommanderError::new(code, ExitCode::Failure);
        for (key, value) in args {
            error = error.with_arg(key, value);
        }
        error
    }

    /// Emits a warning diagnostic.
    pub fn warn(&self, code: &str, args: BTreeMap<String, String>) {
        self.emit_diagnostic(Diagnostic { severity: Severity::Warning, code: code.to_string(), args, message: None });
    }

    /// Emits an advice diagnostic.
    pub fn note(&self, code: &str, args: BTreeMap<String, String>) {
        self.emit_diagnostic(Diagnostic { severity: Severity::Advice, code: code.to_string(), args, message: None });
    }

    /// Writes one raw line to the stderr writer.
    pub fn stderr(&self, line: &str) {
        self.ports.writer.write_line(line);
    }

    /// Prints the help text of the resolved command.
    pub fn print_help(&self) {
        self.cli.print_command_help(self.command, &self.command_path, self.ports, self.state);
    }

    fn emit_diagnostic(&self, diag: Diagnostic) {
        self.cli.emit_diagnostic(&diag, self.ports);
    }
}

/// Stable code for the exit-5 action-required envelope.
pub(crate) const ACTION_REQUIRED_CODE: &str = "commander::action_required";
