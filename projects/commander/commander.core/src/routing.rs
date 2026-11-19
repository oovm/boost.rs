//! Routing and the single entry point: `Cli::run`.
//!
//! Mirrors the TypeScript twin's `parse`/`dispatch` flow: global peeling,
//! `help <cmd>` routing, deepest-command binding, help markers, unknown
//! command suggestions, strict option parsing, configuration passes, and
//! exactly-once envelope emission.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use commander_types::{ExitCode, envelope_error, error_id};
use serde_json::Value;

use crate::context::CommandContext;
use crate::diag::{Diagnostic, DiagContext, HINT_CODES, localized_or_id};
use crate::env::Environment;
use crate::help;
use crate::parse::{self, ParsedOptions};
use crate::ports::Ports;
use crate::prompt_api::{NullPromptApi, PromptApi, PromptFactory, PromptHooks};
use crate::spec::{Command, HandlerOutcome, OptionSpec, parse_failed};

/// The result of one complete run, returned to the host binary.
#[derive(Debug)]
pub struct RunOutcome {
    /// The frozen exit code for this run.
    pub exit: ExitCode,
    /// The stdout payload: in JSON mode exactly one envelope line, in human
    /// mode the accumulated result/help text. `None` when nothing was written.
    pub stdout: Option<String>,
}

/// Accumulates the stdout payload and enforces exactly-once envelope writes.
pub(crate) struct RunState {
    json: bool,
    envelope_written: AtomicBool,
    result_emitted: AtomicBool,
    chunks: std::sync::Mutex<Vec<String>>,
}

impl RunState {
    fn new(json: bool) -> Self {
        Self {
            json,
            envelope_written: AtomicBool::new(false),
            result_emitted: AtomicBool::new(false),
            chunks: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn json(&self) -> bool {
        self.json
    }

    pub(crate) fn write_envelope(&self, envelope: Value) {
        if !self.json {
            return;
        }
        if self.envelope_written.swap(true, Ordering::SeqCst) {
            return;
        }
        self.result_emitted.store(true, Ordering::SeqCst);
        self.chunks.lock().expect("stdout buffer never poisoned").push(envelope.to_string());
    }

    pub(crate) fn write_payload(&self, text: String) {
        if self.json {
            return;
        }
        self.result_emitted.store(true, Ordering::SeqCst);
        self.chunks.lock().expect("stdout buffer never poisoned").push(text);
    }

    pub(crate) fn write_help_payload(&self, text: String) {
        if self.json {
            return;
        }
        self.chunks.lock().expect("stdout buffer never poisoned").push(text);
    }

    pub(crate) fn result_emitted(&self) -> bool {
        self.result_emitted.load(Ordering::SeqCst)
    }

    fn take_stdout(&self) -> Option<String> {
        let chunks = self.chunks.lock().expect("stdout buffer never poisoned");
        if chunks.is_empty() {
            return None;
        }
        Some(format!("{}\n", chunks.join("\n")))
    }
}

/// A named command-line application: registration tree plus the run entry.
#[derive(Clone)]
pub struct Cli {
    name: String,
    intro: Option<String>,
    global_options: Vec<OptionSpec>,
    commands: Vec<Command>,
    on_diagnostic: Option<crate::diag::DiagnosticHandler>,
}

impl Cli {
    /// Declares a CLI named `name` (shown in usage) with the built-in hidden
    /// global options `--json` and `--no-interactive`.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            intro: None,
            global_options: vec![
                OptionSpec::flag("json").hidden(),
                OptionSpec::flag("no-interactive").hidden(),
            ],
            commands: Vec::new(),
            on_diagnostic: None,
        }
    }

    /// Sets the locale key of the root help intro line.
    pub fn intro(mut self, key: impl Into<String>) -> Self {
        self.intro = Some(key.into());
        self
    }

    /// Adds a global option parsed before command routing.
    pub fn global_option(mut self, option: OptionSpec) -> Self {
        self.global_options.push(option);
        self
    }

    /// Adds a root command.
    pub fn command(mut self, command: Command) -> Self {
        self.commands.push(command);
        self
    }

    /// Registers a diagnostic sink; without it rows render to stderr.
    pub fn on_diagnostic(mut self, handler: crate::diag::DiagnosticHandler) -> Self {
        self.on_diagnostic = Some(handler);
        self
    }

    /// Recursively checks the whole registration tree.
    pub fn validate(&self) -> Result<(), commander_types::CommanderError> {
        let mut longs: BTreeSet<String> = BTreeSet::new();
        let mut shorts: BTreeSet<char> = BTreeSet::new();
        for option in &self.global_options {
            option.validate()?;
            if !longs.insert(option.long.clone()) {
                return Err(parse_failed(&format!("duplicate global option `--{}`", option.long)));
            }
            if let Some(c) = option.short {
                if !shorts.insert(c) {
                    return Err(parse_failed(&format!("duplicate global short option `-{c}`")));
                }
            }
        }
        let mut identities: BTreeSet<String> = BTreeSet::new();
        for command in &self.commands {
            for identity in std::iter::once(&command.name).chain(&command.aliases) {
                if !identities.insert(identity.clone()) {
                    return Err(parse_failed(&format!("duplicate command name or alias `{identity}`")));
                }
            }
            command.validate()?;
        }
        Ok(())
    }

    /// Executes one run against the injected environment and ports.
    ///
    /// `prompt` wires the interactive engine factory; without it every
    /// interactive call fails as `commander::interactive_unavailable` while
    /// `await_action` outside interactivity still returns the action-required
    /// descriptor. The library never writes real stdout, reads real streams,
    /// or terminates the process.
    pub fn run(
        &self,
        argv: &[String],
        env: &Environment,
        ports: &Ports,
        prompt: Option<&Arc<dyn PromptFactory>>,
    ) -> RunOutcome {
        if let Err(err) = self.validate() {
            self.emit_diagnostic(&Diagnostic::error(err.id.clone(), err.args.clone()), ports);
            return RunOutcome { exit: err.exit, stdout: None };
        }
        let peeled = parse::peel(argv, &self.global_options);
        let mut rest = peeled.rest;
        if rest.first().map(String::as_str) == Some("--") {
            rest.remove(0);
        }
        let json = peeled.options.bool_value("json").unwrap_or(false);
        let no_interactive = peeled.options.bool_value("no-interactive").unwrap_or(false);
        let interactive = env.tty && !json && !env.ci() && !no_interactive;
        let state = RunState::new(json);

        let exit = if rest.first().map(String::as_str) == Some("help") {
            self.route_help(&rest[1..], ports, &state)
        } else if rest.is_empty() || rest.iter().all(|token| is_help_token(token)) {
            self.print_text(help::root_help(&self.name, self.intro.as_deref(), &self.global_options, &self.commands, ports.localizer.as_ref()), ports, &state);
            ExitCode::Ok
        } else {
            let token = rest[0].clone();
            let matched = self.commands.iter().find(|c| c.name == token || c.aliases.iter().any(|a| *a == token));
            match matched {
                None => {
                    let names: Vec<String> = self
                        .commands
                        .iter()
                        .flat_map(|c| std::iter::once(c.name.clone()).chain(c.aliases.clone()))
                        .collect();
                    let suggest = closest_match(&token, &names);
                    let mut args = BTreeMap::new();
                    args.insert("cmd".to_string(), token);
                    if !suggest.is_empty() {
                        args.insert("suggest".to_string(), suggest);
                    }
                    self.emit_diagnostic(&Diagnostic::error(error_id::UNKNOWN_COMMAND, args.clone()), ports);
                    state.write_envelope(self.failure_envelope(ports, error_id::UNKNOWN_COMMAND, &args, None));
                    ExitCode::Usage
                }
                Some(command) => self.dispatch(
                    command,
                    vec![self.name.clone(), token],
                    rest[1..].to_vec(),
                    &peeled.options,
                    interactive,
                    ports,
                    &state,
                    prompt,
                ),
            }
        };
        RunOutcome { exit, stdout: state.take_stdout() }
    }

    fn route_help(&self, tokens: &[String], ports: &Ports, state: &RunState) -> ExitCode {
        if tokens.is_empty() {
            self.print_text(help::root_help(&self.name, self.intro.as_deref(), &self.global_options, &self.commands, ports.localizer.as_ref()), ports, state);
            return ExitCode::Ok;
        }
        let mut pool: &[Command] = &self.commands;
        let mut matched: Option<&Command> = None;
        let mut path = vec![self.name.clone()];
        for token in tokens {
            let found = pool.iter().find(|c| &c.name == token || c.aliases.iter().any(|a| a == token));
            match found {
                Some(command) => {
                    matched = Some(command);
                    path.push(token.clone());
                    pool = &command.subcommands;
                }
                None => break,
            }
        }
        match matched {
            None => self.print_text(
                help::root_help(&self.name, self.intro.as_deref(), &self.global_options, &self.commands, ports.localizer.as_ref()),
                ports,
                state,
            ),
            Some(command) => self.print_command_help(command, &path, ports, state),
        }
        ExitCode::Ok
    }

    #[allow(clippy::too_many_arguments)]
    fn dispatch(
        &self,
        root: &Command,
        mut path: Vec<String>,
        mut remaining: Vec<String>,
        global_opts: &ParsedOptions,
        interactive: bool,
        ports: &Ports,
        state: &RunState,
        prompt: Option<&Arc<dyn PromptFactory>>,
    ) -> ExitCode {
        // `--` ends option parsing; keep head and raw tail separate so tail
        // tokens are never interpreted as options or help markers.
        let separator = remaining.iter().position(|token| token == "--");
        let tail: Vec<String> = separator.map_or_else(Vec::new, |i| remaining[i + 1..].to_vec());
        if let Some(i) = separator {
            remaining.truncate(i);
        }

        // Resolve the deepest command before interpreting help markers, so
        // `demo document check --help` binds to `check` instead of `document`.
        let mut target: &Command = root;
        while !target.subcommands.is_empty() && !remaining.is_empty() {
            let token = remaining[0].clone();
            if token.starts_with('-') || is_help_token(&token) {
                break;
            }
            let Some(child) = target.find_subcommand(&token) else {
                break;
            };
            target = child;
            path.push(token);
            remaining.remove(0);
        }

        if remaining.iter().any(|token| is_help_token(token)) {
            self.print_command_help(target, &path, ports, state);
            return ExitCode::Ok;
        }

        if target.handler.is_none() {
            if !remaining.is_empty() || !tail.is_empty() {
                let child_names: Vec<String> = target
                    .subcommands
                    .iter()
                    .flat_map(|c| std::iter::once(c.name.clone()).chain(c.aliases.clone()))
                    .collect();
                let suggest = remaining
                    .first()
                    .filter(|token| !token.starts_with('-'))
                    .map_or_else(String::new, |token| closest_match(token, &child_names));
                let mut cmd_tokens: Vec<String> = path[1..].to_vec();
                cmd_tokens.extend(remaining.iter().cloned());
                cmd_tokens.extend(tail.iter().cloned());
                let mut args = BTreeMap::new();
                args.insert("cmd".to_string(), cmd_tokens.join(" "));
                if !suggest.is_empty() {
                    args.insert("suggest".to_string(), suggest);
                }
                self.emit_diagnostic(&Diagnostic::error(error_id::UNKNOWN_COMMAND, args.clone()), ports);
                state.write_envelope(self.failure_envelope(ports, error_id::UNKNOWN_COMMAND, &args, None));
                return ExitCode::Usage;
            }
            self.print_command_help(target, &path, ports, state);
            return ExitCode::Ok;
        }

        let parsed = match parse::strict(&remaining, &target.options) {
            Ok(mut parsed) => match parse::apply_config(&target.options, &mut parsed.options, &parsed.used) {
                Ok(()) => Ok(parsed),
                Err(err) => Err(err),
            },
            Err(err) => Err(err),
        };
        let parsed = match parsed {
            Ok(parsed) => parsed,
            Err(err) => {
                let mut args = err.args.clone();
                args.insert("cmd".to_string(), path[1..].join(" "));
                self.emit_diagnostic(&Diagnostic::error(err.id.clone(), args.clone()), ports);
                state.write_envelope(self.failure_envelope(ports, &err.id, &args, None));
                return err.exit;
            }
        };
        let mut options = parsed.options;
        options.extend_positionals(tail);
        options.merge_missing(global_opts);

        let stdin_claimed = Arc::new(AtomicBool::new(false));
        let lease = Arc::new(AtomicBool::new(false));
        let api: Arc<dyn PromptApi> = match prompt {
            Some(factory) => factory.create(PromptHooks {
                interactive,
                lease: lease.clone(),
                stdin_claimed: stdin_claimed.clone(),
            }),
            None => Arc::new(NullPromptApi {
                hooks: PromptHooks { interactive, lease, stdin_claimed: stdin_claimed.clone() },
            }),
        };

        let handler = target.handler.as_ref().expect("handler presence checked above");
        let mut ctx = CommandContext::new(
            self,
            target,
            path,
            ports,
            state,
            options,
            interactive,
            api,
            stdin_claimed,
        );
        match handler(&mut ctx) {
            HandlerOutcome::Ok(data) => {
                if !state.result_emitted() {
                    ctx.emit_result(data, None);
                }
                ExitCode::Ok
            }
            HandlerOutcome::Err(err) => {
                self.emit_diagnostic(&Diagnostic::error(err.id.clone(), err.args.clone()), ports);
                state.write_envelope(self.failure_envelope(ports, &err.id, &err.args, None));
                err.exit
            }
            HandlerOutcome::ActionRequired { code, message, descriptor } => {
                let mut args = BTreeMap::new();
                args.insert("actionId".to_string(), descriptor.action_id.clone());
                self.emit_diagnostic(&Diagnostic::error(code.clone(), args), ports);
                state.write_envelope(envelope_error(&code, &message, Some(&descriptor)));
                ExitCode::ActionRequired
            }
        }
    }

    pub(crate) fn print_command_help(&self, command: &Command, path: &[String], ports: &Ports, state: &RunState) {
        let text = help::command_help(&self.name, command, &path[1..], &self.global_options, ports.localizer.as_ref());
        self.print_text(text, ports, state);
    }

    /// Routes help text: stderr in JSON mode (stdout stays envelope-only),
    /// stdout payload in human mode.
    fn print_text(&self, text: String, ports: &Ports, state: &RunState) {
        if state.json() {
            for line in text.split('\n') {
                ports.writer.write_line(line);
            }
        } else {
            state.write_help_payload(text);
        }
    }

    pub(crate) fn emit_diagnostic(&self, diag: &Diagnostic, ports: &Ports) {
        let ctx = DiagContext { locale: ports.locale.as_str(), localizer: ports.localizer.as_ref() };
        if let Some(handler) = &self.on_diagnostic {
            handler(diag, &ctx);
        } else {
            let from_catalog = localized_or_id(ports.localizer.as_ref(), &diag.code, &diag.args);
            let business = diag.message.clone().filter(|m| *m != diag.code);
            let msg = from_catalog.or(business).unwrap_or_else(|| diag.code.clone());
            ports.writer.write_line(&msg);
        }
        if HINT_CODES.contains(&diag.code.as_str()) {
            let cmd = diag.args.get("cmd").map(String::as_str).unwrap_or("").trim();
            let hint_target = if diag.code == error_id::UNKNOWN_COMMAND || cmd.is_empty() {
                self.name.clone()
            } else {
                format!("{} {}", self.name, cmd)
            };
            let mut args = BTreeMap::new();
            args.insert("command".to_string(), hint_target);
            if let Some(text) = localized_or_id(ports.localizer.as_ref(), "commander.ui.hint.available_options", &args) {
                ports.writer.write_line(&text);
            }
        }
        let suggest = diag.args.get("suggest").cloned().unwrap_or_default();
        if !suggest.is_empty() {
            let mut args = BTreeMap::new();
            args.insert("suggest".to_string(), suggest);
            if let Some(text) = localized_or_id(ports.localizer.as_ref(), "commander.ui.hint.did_you_mean", &args) {
                ports.writer.write_line(&text);
            }
        }
    }

    fn failure_envelope(
        &self,
        ports: &Ports,
        code: &str,
        args: &BTreeMap<String, String>,
        descriptor: Option<&commander_types::ActionRequiredDescriptor>,
    ) -> Value {
        envelope_error(code, &failure_message(ports, code, args), descriptor)
    }
}

/// Builds the envelope message for one failure code: `commander::action_failed`
/// reports the stable code only (wrapped messages never enter the envelope);
/// every other code resolves through the localizer and falls back to the code.
pub(crate) fn failure_message(ports: &Ports, code: &str, args: &BTreeMap<String, String>) -> String {
    if code == error_id::ACTION_FAILED {
        return code.to_string();
    }
    localized_or_id(ports.localizer.as_ref(), code, args).unwrap_or_else(|| code.to_string())
}

/// Help markers recognized before any option parsing: `help`, `-h`, `--help`.
fn is_help_token(token: &str) -> bool {
    token == "help" || token == "-h" || token == "--help"
}

/// Strips the leading program name when hosts pass full `std::env::args()`.
///
/// The TypeScript twin peels `node` plus the script path; the Rust twin is
/// host-driven, so this only removes a first token that looks like an
/// executable path (contains a separator or ends in `.exe`).
pub fn normalize_argv(argv: &[String]) -> Vec<String> {
    if argv.len() >= 1 && (argv[0].contains('/') || argv[0].contains('\\') || argv[0].ends_with(".exe")) {
        return argv[1..].to_vec();
    }
    argv.to_vec()
}

/// Simple Levenshtein nearest match among candidates; returns the top-1 name
/// when one is close enough, never a comma-joined list.
pub(crate) fn closest_match(input: &str, candidates: &[String]) -> String {
    if input.is_empty() || candidates.is_empty() {
        return String::new();
    }
    let lower = input.to_lowercase();
    let threshold = std::cmp::max(2, (input.chars().count() as f64 * 0.6).floor() as usize + 1);
    let mut best: Option<(usize, String)> = None;
    for candidate in candidates {
        let dist = levenshtein(&lower, &candidate.to_lowercase());
        let better = match &best {
            None => true,
            Some((bd, _)) => dist < *bd,
        };
        if dist <= threshold && better {
            best = Some((dist, candidate.clone()));
        }
    }
    best.map_or(String::new(), |(_, name)| name)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let left: Vec<char> = a.chars().collect();
    let right: Vec<char> = b.chars().collect();
    if left.is_empty() {
        return right.len();
    }
    if right.is_empty() {
        return left.len();
    }
    let mut prev: Vec<usize> = (0..=right.len()).collect();
    let mut curr = vec![0usize; right.len() + 1];
    for i in 1..=left.len() {
        curr[0] = i;
        for j in 1..=right.len() {
            let cost = usize::from(left[i - 1] != right[j - 1]);
            curr[j] = (curr[j - 1] + 1).min(prev[j] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[right.len()]
}
