//! The `ctx.prompt` contract owned by the core and implemented by `commander-prompt`.
//!
//! The core defines the request types, the five prompt operations, the
//! per-run hook bundle (eligibility snapshot, single active prompt lease,
//! stdin-source claim), and the factory seam through which hosts wire a real
//! engine into `run`. No prompt state machine lives here.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use commander_types::{ActionRequiredDescriptor, CommanderError, NextAction, SecretString, error_id};

/// Ask the operator a yes/no question.
#[derive(Debug, Clone, Default)]
pub struct ConfirmRequest {
    /// Locale key of the question text.
    pub key: String,
    /// Outcome applied only when the operator submits an empty line.
    pub default: Option<bool>,
    /// Relative timeout in milliseconds; the deadline is absolute once the
    /// request starts and validation retries never reset it.
    pub deadline_ms: Option<u64>,
}

impl ConfirmRequest {
    /// Builds a confirm request from the question key.
    pub fn new(key: impl Into<String>) -> Self {
        Self { key: key.into(), ..Self::default() }
    }

    /// Sets the empty-line default.
    pub fn default_value(mut self, value: bool) -> Self {
        self.default = Some(value);
        self
    }

    /// Sets the relative timeout.
    pub fn deadline_ms(mut self, ms: u64) -> Self {
        self.deadline_ms = Some(ms);
        self
    }
}

/// Validation problem reported by [`InputRequest::validate`]: a stable code
/// plus safe typed args. Never carries the entered secret.
#[derive(Debug, Clone)]
pub struct InputValidationError {
    /// Stable message id used to localize the feedback line.
    pub code: String,
    /// Safe typed arguments for localization.
    pub args: BTreeMap<String, String>,
}

impl InputValidationError {
    /// Builds a validation problem from a stable code.
    pub fn new(code: impl Into<String>) -> Self {
        Self { code: code.into(), args: BTreeMap::new() }
    }

    /// Adds one safe typed argument.
    pub fn with_arg(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.args.insert(key.into(), value.into());
        self
    }
}

type Validator = dyn Fn(&str) -> Option<InputValidationError> + Send + Sync;

/// Ask the operator for one non-secret line of text.
#[derive(Default)]
pub struct InputRequest {
    /// Locale key of the question text.
    pub key: String,
    /// Optional validation; a reported problem is written to stderr and the
    /// question is re-asked under the same deadline.
    pub validate: Option<Box<Validator>>,
    /// Relative timeout in milliseconds.
    pub deadline_ms: Option<u64>,
}

impl InputRequest {
    /// Builds an input request from the question key.
    pub fn new(key: impl Into<String>) -> Self {
        Self { key: key.into(), ..Self::default() }
    }

    /// Attaches a validation callback.
    pub fn validate(mut self, f: impl Fn(&str) -> Option<InputValidationError> + Send + Sync + 'static) -> Self {
        self.validate = Some(Box::new(f));
        self
    }

    /// Sets the relative timeout.
    pub fn deadline_ms(mut self, ms: u64) -> Self {
        self.deadline_ms = Some(ms);
        self
    }
}

/// One select entry: the stable `value` returned on pick and the localized
/// `label_key` displayed in the list.
#[derive(Debug, Clone)]
pub struct SelectChoice {
    /// Stable value returned to the handler; never an array index.
    pub value: String,
    /// Locale key rendered as the choice label.
    pub label_key: String,
}

impl SelectChoice {
    /// Builds one choice.
    pub fn new(value: impl Into<String>, label_key: impl Into<String>) -> Self {
        Self { value: value.into(), label_key: label_key.into() }
    }
}

/// Ask the operator to pick one choice from a numbered list.
#[derive(Debug, Clone, Default)]
pub struct SelectRequest {
    /// Locale key of the question text.
    pub key: String,
    /// The listed choices; a pick returns the entry's stable value.
    pub choices: Vec<SelectChoice>,
    /// Relative timeout in milliseconds.
    pub deadline_ms: Option<u64>,
}

impl SelectRequest {
    /// Builds a select request from the question key and choices.
    pub fn new(key: impl Into<String>, choices: Vec<SelectChoice>) -> Self {
        Self { key: key.into(), choices, deadline_ms: None }
    }

    /// Sets the relative timeout.
    pub fn deadline_ms(mut self, ms: u64) -> Self {
        self.deadline_ms = Some(ms);
        self
    }
}

/// Ask the operator for a secret with terminal echo disabled.
#[derive(Debug, Clone, Default)]
pub struct PasswordRequest {
    /// Locale key of the question text.
    pub key: String,
    /// Relative timeout in milliseconds.
    pub deadline_ms: Option<u64>,
}

impl PasswordRequest {
    /// Builds a password request from the question key.
    pub fn new(key: impl Into<String>) -> Self {
        Self { key: key.into(), ..Self::default() }
    }

    /// Sets the relative timeout.
    pub fn deadline_ms(mut self, ms: u64) -> Self {
        self.deadline_ms = Some(ms);
        self
    }
}

/// Wait for the operator to complete an external action, or defer it.
#[derive(Debug, Clone, Default)]
pub struct AwaitActionRequest {
    /// Stable action identity owned by the domain.
    pub action_id: String,
    /// Locale key of the instruction body shown to the operator.
    pub instructions_key: String,
    /// Safe typed arguments for the instruction body.
    pub args: BTreeMap<String, String>,
    /// Reference into the domain's persisted state.
    pub status_ref: Option<String>,
    /// RFC 3339 expiry of the pending external action.
    pub expires_at: Option<String>,
    /// Structured continuation a later run may execute.
    pub next_action: Option<NextAction>,
    /// Relative timeout in milliseconds.
    pub deadline_ms: Option<u64>,
}

impl AwaitActionRequest {
    /// Builds an await request from the action id and instruction key.
    pub fn new(action_id: impl Into<String>, instructions_key: impl Into<String>) -> Self {
        Self { action_id: action_id.into(), instructions_key: instructions_key.into(), ..Self::default() }
    }

    /// Sets the persisted-state reference.
    pub fn status_ref(mut self, value: impl Into<String>) -> Self {
        self.status_ref = Some(value.into());
        self
    }

    /// Sets the expiry stamp.
    pub fn expires_at(mut self, value: impl Into<String>) -> Self {
        self.expires_at = Some(value.into());
        self
    }

    /// Sets the structured continuation.
    pub fn next_action(mut self, value: NextAction) -> Self {
        self.next_action = Some(value);
        self
    }

    /// Sets the relative timeout.
    pub fn deadline_ms(mut self, ms: u64) -> Self {
        self.deadline_ms = Some(ms);
        self
    }
}

/// Outcome of [`PromptApi::await_action`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AwaitActionOutcome {
    /// The operator confirmed the external action is complete.
    Confirmed,
    /// The operator deferred the action; the descriptor carries the resume
    /// facts for a later run.
    Deferred,
    /// Prompting was not eligible: the domain must resume in another run.
    /// Delivered as a value, never as a hang, and completion is not claimed.
    ActionRequired(ActionRequiredDescriptor),
}

/// The five context-scoped prompt operations exposed to handlers.
///
/// Every call re-checks eligibility at acquire time and takes the single
/// active prompt lease. Questions and validation feedback go to the stderr
/// writer; stdout stays data-only.
pub trait PromptApi: Send + Sync {
    /// Asks a yes/no question.
    fn confirm(&self, req: &ConfirmRequest) -> Result<bool, CommanderError>;
    /// Asks for one non-secret line with optional validation retries.
    fn input(&self, req: &InputRequest) -> Result<String, CommanderError>;
    /// Asks for a numbered pick and returns the stable value.
    fn select(&self, req: &SelectRequest) -> Result<String, CommanderError>;
    /// Asks for a secret with echo disabled; never degrades to plain input.
    fn password(&self, req: &PasswordRequest) -> Result<SecretString, CommanderError>;
    /// Waits for an external action or defers it to another run.
    fn await_action(&self, req: &AwaitActionRequest) -> Result<AwaitActionOutcome, CommanderError>;
}

/// Per-run state the prompt engine reads and writes.
#[derive(Clone)]
pub struct PromptHooks {
    /// Eligibility snapshot for this run (`tty && !json && !ci && !no-interactive`).
    pub interactive: bool,
    /// The single active prompt lease shared by every call in this context.
    pub lease: Arc<AtomicBool>,
    /// Set once the command consumed stdin as a data source (`claim_stdin`).
    pub stdin_claimed: Arc<AtomicBool>,
}

/// Builds one prompt engine per dispatched command context.
pub trait PromptFactory: Send + Sync {
    /// Creates the engine bound to this run's hooks.
    fn create(&self, hooks: PromptHooks) -> Arc<dyn PromptApi>;
}

/// Stand-in used when no prompt engine is wired: interactive calls fail as a
/// capability error; `await_action` outside interactivity still returns the
/// action-required descriptor instead of hanging.
pub(crate) struct NullPromptApi {
    pub(crate) hooks: PromptHooks,
}

impl PromptApi for NullPromptApi {
    fn confirm(&self, _req: &ConfirmRequest) -> Result<bool, CommanderError> {
        Err(unavailable())
    }

    fn input(&self, _req: &InputRequest) -> Result<String, CommanderError> {
        Err(unavailable())
    }

    fn select(&self, _req: &SelectRequest) -> Result<String, CommanderError> {
        Err(unavailable())
    }

    fn password(&self, _req: &PasswordRequest) -> Result<SecretString, CommanderError> {
        Err(unavailable())
    }

    fn await_action(&self, req: &AwaitActionRequest) -> Result<AwaitActionOutcome, CommanderError> {
        if !self.hooks.interactive || self.hooks.stdin_claimed.load(Ordering::SeqCst) {
            return Ok(AwaitActionOutcome::ActionRequired(descriptor_of(req)));
        }
        Err(unavailable())
    }
}

pub(crate) fn descriptor_of(req: &AwaitActionRequest) -> ActionRequiredDescriptor {
    ActionRequiredDescriptor {
        action_id: req.action_id.clone(),
        status_ref: req.status_ref.clone(),
        expires_at: req.expires_at.clone(),
        next_action: req.next_action.clone(),
    }
}

fn unavailable() -> CommanderError {
    CommanderError::from_id(error_id::INTERACTIVE_UNAVAILABLE)
}
