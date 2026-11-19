//! Command tree declaration: options, commands, and handler outcomes.

use std::{collections::BTreeSet, sync::Arc};

use commander_types::{ActionRequiredDescriptor, CommanderError, InteractiveSpec, PromptPolicy};
use serde_json::Value;

use crate::CommandContext;

/// How an option consumes its value from argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueDecl {
    /// A boolean switch; presence is the value and no argv token is consumed.
    Flag,
    /// A value must be attached with `=` or follow as the next argv token.
    Required,
    /// A value may be attached with `=`; a bare token records an empty value.
    OptionalValue,
}

/// Type validation applied to collected string values on the machine path.
///
/// Values stay stored as their literal strings; coercion only rejects input
/// that cannot represent the declared type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Coerce {
    /// No type check beyond the declared enum, if any.
    #[default]
    None,
    /// The value must parse as a 64-bit signed integer.
    Int,
    /// The value must parse as a 64-bit float.
    Float,
    /// The value must be one of `true`, `false`, `1`, `0`.
    Bool,
}

/// A declared option on a command.
#[derive(Debug, Clone)]
pub struct OptionSpec {
    /// The `--long` name; unique within the owning command.
    pub long: String,
    /// The optional single-character `-s` alias.
    pub short: Option<char>,
    /// How the option consumes its value.
    pub value: ValueDecl,
    /// Whether the option may appear more than once.
    pub repeatable: bool,
    /// Accepted literal values; empty means any value is accepted.
    pub enum_values: Vec<String>,
    /// Whether the value is mandatory on the machine path.
    pub required: bool,
    /// Literal default applied when the option is absent from argv.
    pub default: Option<String>,
    /// Type validation applied to collected values.
    pub coerce: Coerce,
    /// Whether the option is hidden from help listings.
    pub hidden: bool,
    /// Optional display group for help listings.
    pub group: Option<String>,
    /// Locale key for the option's help row.
    pub help_key: Option<String>,
    /// Interactive declaration attached to this option, if any.
    pub interactive: Option<InteractiveSpec>,
}

impl OptionSpec {
    /// Declares a boolean switch.
    pub fn flag(long: impl Into<String>) -> Self {
        Self {
            long: long.into(),
            short: None,
            value: ValueDecl::Flag,
            repeatable: false,
            enum_values: Vec::new(),
            required: false,
            default: None,
            coerce: Coerce::None,
            hidden: false,
            group: None,
            help_key: None,
            interactive: None,
        }
    }

    /// Declares an option whose value must be supplied.
    pub fn required_value(long: impl Into<String>) -> Self {
        Self { value: ValueDecl::Required, ..Self::flag(long) }
    }

    /// Declares an option whose value may be attached with `=`.
    pub fn optional_value(long: impl Into<String>) -> Self {
        Self { value: ValueDecl::OptionalValue, ..Self::flag(long) }
    }

    /// Adds a single-character alias.
    pub fn short(mut self, c: char) -> Self {
        self.short = Some(c);
        self
    }

    /// Allows the option to appear more than once.
    pub fn repeatable(mut self) -> Self {
        self.repeatable = true;
        self
    }

    /// Restricts values to the listed literals.
    pub fn enum_values(mut self, values: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.enum_values = values.into_iter().map(Into::into).collect();
        self
    }

    /// Marks the option mandatory on the machine path.
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// Sets the literal default used when the option is absent.
    pub fn default_value(mut self, value: impl Into<String>) -> Self {
        self.default = Some(value.into());
        self
    }

    /// Sets the type validation for collected values.
    pub fn coerce(mut self, coerce: Coerce) -> Self {
        self.coerce = coerce;
        self
    }

    /// Hides the option from help listings.
    pub fn hidden(mut self) -> Self {
        self.hidden = true;
        self
    }

    /// Assigns a display group for help listings.
    pub fn group(mut self, group: impl Into<String>) -> Self {
        self.group = Some(group.into());
        self
    }

    /// Sets the locale key for the help row.
    pub fn help(mut self, key: impl Into<String>) -> Self {
        self.help_key = Some(key.into());
        self
    }

    /// Attaches the interactive machine-path declaration.
    pub fn interactive(mut self, spec: InteractiveSpec) -> Self {
        self.interactive = Some(spec);
        self
    }

    pub(crate) fn validate(&self) -> Result<(), CommanderError> {
        if self.long.is_empty() {
            return Err(parse_failed("option has an empty long name"));
        }
        if self.enum_values.iter().any(|v| v.is_empty()) {
            return Err(parse_failed(&format!("option `--{}` has an empty enum value", self.long)));
        }
        if let Some(default) = &self.default {
            validate_value(self, default)
                .map_err(|_| parse_failed(&format!("option `--{}` default is outside its declared value set", self.long)))?;
        }
        Ok(())
    }
}

/// The result a command handler returns to the execution layer.
#[derive(Debug, Clone)]
pub enum HandlerOutcome {
    /// Success; the value becomes the `data` field of the result envelope.
    Ok(Value),
    /// A typed failure; its id and frozen exit code drive the outcome.
    Err(CommanderError),
    /// Domain work is prepared but needs an external action before a later
    /// run; maps to exit 5 with the descriptor attached.
    ActionRequired {
        /// Stable domain or framework code for the envelope.
        code: String,
        /// Safe human summary for stderr diagnostics and the envelope.
        message: String,
        /// Structured continuation descriptor; never carries secrets.
        descriptor: ActionRequiredDescriptor,
    },
}

/// A command handler bound to a command node.
pub type Handler = Arc<dyn Fn(&mut CommandContext<'_>) -> HandlerOutcome + Send + Sync>;

/// A node in the command tree.
#[derive(Clone)]
pub struct Command {
    /// The invocable name; unique among siblings.
    pub name: String,
    /// Alternate spellings that resolve to this command.
    pub aliases: Vec<String>,
    /// Options declared on this command.
    pub options: Vec<OptionSpec>,
    /// Nested commands.
    pub subcommands: Vec<Command>,
    /// Whether the command exposes the `--` passthrough tail.
    pub passthrough: bool,
    /// Locale key for the command's help text.
    pub help_key: Option<String>,
    /// Interactive declaration attached to this command, if any.
    pub interactive: Option<InteractiveSpec>,
    /// The handler executed when this command is resolved.
    pub handler: Option<Handler>,
}

impl Command {
    /// Declares a command node by name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            aliases: Vec::new(),
            options: Vec::new(),
            subcommands: Vec::new(),
            passthrough: false,
            help_key: None,
            interactive: None,
            handler: None,
        }
    }

    /// Adds an alternate spelling.
    pub fn alias(mut self, alias: impl Into<String>) -> Self {
        self.aliases.push(alias.into());
        self
    }

    /// Adds a declared option.
    pub fn option(mut self, option: OptionSpec) -> Self {
        self.options.push(option);
        self
    }

    /// Adds a nested command.
    pub fn subcommand(mut self, command: Command) -> Self {
        self.subcommands.push(command);
        self
    }

    /// Declares that this command exposes the `--` passthrough tail.
    pub fn with_passthrough(mut self) -> Self {
        self.passthrough = true;
        self
    }

    /// Sets the locale key for the command's help text.
    pub fn help(mut self, key: impl Into<String>) -> Self {
        self.help_key = Some(key.into());
        self
    }

    /// Attaches the interactive machine-path declaration.
    pub fn interactive(mut self, spec: InteractiveSpec) -> Self {
        self.interactive = Some(spec);
        self
    }

    /// Binds the handler executed when this command is resolved.
    pub fn handler(mut self, handler: Handler) -> Self {
        self.handler = Some(handler);
        self
    }

    /// Recursively checks the tree for declaration mistakes.
    ///
    /// `run` invokes this before parsing; hosts may call it at startup to
    /// fail fast. Any violation is a development error surfaced as
    /// `commander::parse_failed`.
    pub fn validate(&self) -> Result<(), CommanderError> {
        if self.name.is_empty() {
            return Err(parse_failed("command has an empty name"));
        }
        let mut longs = Vec::new();
        let mut shorts = Vec::new();
        for option in &self.options {
            option.validate()?;
            if longs.contains(&option.long) {
                return Err(parse_failed(&format!("duplicate option `--{}`", option.long)));
            }
            longs.push(option.long.clone());
            if let Some(c) = option.short {
                if shorts.contains(&c) {
                    return Err(parse_failed(&format!("duplicate short option `-{c}`")));
                }
                shorts.push(c);
            }
        }
        let keys: BTreeSet<String> = self.options.iter().map(|o| o.long.clone()).collect();
        if let Some(spec) = &self.interactive {
            validate_interactive_spec(spec, &keys)?;
        }
        for option in &self.options {
            if let Some(spec) = &option.interactive {
                validate_interactive_spec(spec, &keys)?;
            }
        }
        let mut identities = vec![self.name.clone()];
        for sub in &self.subcommands {
            for identity in sub.aliases.iter().chain(std::iter::once(&sub.name)) {
                if identities.contains(identity) {
                    return Err(parse_failed(&format!("duplicate command name or alias `{identity}`")));
                }
                identities.push(identity.clone());
            }
            sub.validate()?;
        }
        Ok(())
    }

    pub(crate) fn find_subcommand(&self, token: &str) -> Option<&Command> {
        self.subcommands.iter().find(|c| c.name == token || c.aliases.iter().any(|a| a == token))
    }
}

/// Checks one interactive declaration against the owning command's option table.
///
/// Mirrors the TypeScript twin: every prompt key value must be a non-empty id,
/// `required` policy needs at least one prompt key and one machine input path,
/// `never` policy must not declare inputs, and every input reference in
/// `non_interactive_inputs` plus `continuation.resume_inputs` must resolve to
/// an option key, `flag:<option-key>`, or `config:<name>`. Violations are a
/// registration error surfaced as `commander::interactive_spec_invalid` (exit 1).
pub(crate) fn validate_interactive_spec(spec: &InteractiveSpec, option_keys: &BTreeSet<String>) -> Result<(), CommanderError> {
    let invalid = |detail: &str| -> CommanderError {
        CommanderError::from_id("commander::interactive_spec_invalid").with_arg("detail", detail.to_string())
    };
    if spec.prompt_keys.values().any(|key| key.is_empty()) {
        return Err(invalid("prompt key values must be non-empty ids"));
    }
    if spec.policy == PromptPolicy::Required {
        if spec.prompt_keys.is_empty() {
            return Err(invalid("policy `required` needs at least one prompt key"));
        }
        if spec.non_interactive_inputs.is_empty() {
            return Err(invalid("policy `required` needs at least one machine input path"));
        }
    }
    if spec.policy == PromptPolicy::Never && !spec.non_interactive_inputs.is_empty() {
        return Err(invalid("policy `never` must not declare non-interactive inputs"));
    }
    let resume = spec.continuation.as_ref().map(|c| c.resume_inputs.as_slice()).unwrap_or(&[]);
    for entry in spec.non_interactive_inputs.iter().chain(resume) {
        if entry.is_empty() {
            return Err(invalid("input reference must be a non-empty string"));
        }
        if let Some(name) = entry.strip_prefix("flag:") {
            if name.is_empty() || !option_keys.contains(name) {
                return Err(invalid(&format!("unknown flag reference `{entry}`")));
            }
        }
        else if let Some(name) = entry.strip_prefix("config:") {
            if name.is_empty() {
                return Err(invalid("empty config reference"));
            }
        }
        else if !option_keys.contains(entry) {
            return Err(invalid(&format!("unknown input reference `{entry}`")));
        }
    }
    Ok(())
}

pub(crate) fn validate_value(spec: &OptionSpec, value: &str) -> Result<(), CommanderError> {
    if !spec.enum_values.is_empty() && !spec.enum_values.iter().any(|v| v == value) {
        return Err(CommanderError::from_id(commander_types::error_id::OPTION_NOT_IN_ENUM)
            .with_arg("option", spec.long.clone())
            .with_arg("value", value));
    }
    let ok = match spec.coerce {
        Coerce::None => true,
        Coerce::Int => value.parse::<i64>().is_ok(),
        Coerce::Float => value.parse::<f64>().is_ok(),
        Coerce::Bool => matches!(value.to_ascii_lowercase().as_str(), "true" | "false" | "1" | "0"),
    };
    if !ok {
        return Err(CommanderError::from_id(commander_types::error_id::OPTION_COERCE_FAILED)
            .with_arg("option", spec.long.clone())
            .with_arg("value", value));
    }
    Ok(())
}

pub(crate) fn parse_failed(reason: &str) -> CommanderError {
    CommanderError::from_id(commander_types::error_id::PARSE_FAILED).with_arg("reason", reason)
}
