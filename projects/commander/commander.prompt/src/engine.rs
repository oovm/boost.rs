//! The prompt engine: five primitives over an injected IO seam.
//!
//! Behavior mirrors the TypeScript `PromptEngine` one-to-one:
//! - Questions and validation feedback go to the stderr writer; stdout stays
//!   data-only.
//! - One active prompt per context (`commander::interaction_busy`).
//! - EOF or Ctrl+C after a prompt started → `commander::prompt_canceled`
//!   (exit 4); the deadline is absolute for the whole request and validation
//!   retries never reset it (`commander::prompt_timeout`, exit 4).
//! - `select` returns the stable `value`, never an array index.
//! - `password` requires echo control; without it the prompt fails as
//!   `commander::password_unsupported` (exit 3) and never degrades to plain
//!   input. Echo is restored on every exit path.
//! - Non-interactive `await_action` returns the action-required descriptor so
//!   the caller resumes in another run; it never hangs and never claims
//!   completion.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use commander_core::{
    AwaitActionOutcome, AwaitActionRequest, Clock, ConfirmRequest, InputRequest, Localizer,
    PasswordRequest, PromptApi, PromptHooks, SelectRequest, Writer,
};
use commander_types::{ActionRequiredDescriptor, CommanderError, SecretString, error_id};

/// Poll interval between cancellation and deadline checks.
const POLL_MS: u64 = 250;

/// One read event from the IO seam.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoEvent {
    /// One byte was read.
    Byte(u8),
    /// The stream ended.
    Eof,
    /// No byte arrived before the timeout.
    Timeout,
}

/// Restores terminal echo when dropped; RAII on every exit path.
pub trait EchoRestore: Send {}

/// The IO seam the engine reads through.
pub trait PromptIo: Send {
    /// Reads one byte, waiting at most `timeout_ms`.
    fn read_char(&mut self, timeout_ms: u64) -> IoEvent;
    /// Whether echo can be turned off on this stream.
    fn supports_echo_control(&self) -> bool;
    /// Turns echo off; `None` when the stream cannot do it.
    fn disable_echo(&mut self) -> Option<Box<dyn EchoRestore>>;
}

/// Everything the engine needs from the host, shared across contexts.
#[derive(Clone)]
pub struct EngineDeps {
    /// The IO seam (one terminal input stream per host process).
    pub io: Arc<Mutex<dyn PromptIo>>,
    /// Localized human output channel (stderr). Never stdout.
    pub writer: Arc<dyn Writer>,
    /// The catalog-backed localizer.
    pub localizer: Arc<dyn Localizer>,
    /// The clock used for absolute prompt deadlines.
    pub clock: Arc<dyn Clock>,
    /// The process-level cancellation token.
    pub cancel: Arc<AtomicBool>,
}

/// One command context's prompt engine, bound to the run's hooks.
pub struct PromptEngine {
    deps: EngineDeps,
    hooks: PromptHooks,
}

impl PromptEngine {
    /// Binds an engine to one run's hook bundle.
    pub fn new(deps: EngineDeps, hooks: PromptHooks) -> Self {
        Self { deps, hooks }
    }

    /// Re-checks eligibility at acquire time and takes the single active
    /// prompt lease.
    fn acquire(&self) -> Result<LeaseGuard<'_>, CommanderError> {
        if !self.hooks.interactive || self.hooks.stdin_claimed.load(Ordering::SeqCst) {
            return Err(CommanderError::from_id(error_id::INTERACTIVE_UNAVAILABLE));
        }
        if self.hooks.lease.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err()
        {
            return Err(CommanderError::from_id(error_id::INTERACTION_BUSY));
        }
        Ok(LeaseGuard(&self.hooks.lease))
    }

    fn t(&self, key: &str, args: &BTreeMap<String, String>) -> String {
        self.deps.localizer.t(key, args)
    }

    /// Resolves a chrome id, treating the unresolved sentinel `{{id}}` as absent.
    fn hint(&self, key: &str, args: &BTreeMap<String, String>) -> String {
        let value = self.t(key, args);
        if value == format!("{{{key}}}") { String::new() } else { value }
    }

    /// Absolute deadline anchor for one request (retries share it).
    fn deadline(&self, deadline_ms: Option<u64>) -> Option<u64> {
        deadline_ms.map(|ms| self.deps.clock.now_ms() + ms)
    }

    fn wait_slice(&self, until: Option<u64>) -> Result<u64, CommanderError> {
        match until {
            None => Ok(POLL_MS),
            Some(until) => {
                let now = self.deps.clock.now_ms();
                if now >= until {
                    return Err(CommanderError::from_id(error_id::PROMPT_TIMEOUT));
                }
                Ok((until - now).min(POLL_MS))
            }
        }
    }

    /// Reads one line from the IO seam with the question on stderr.
    fn ask_line(&self, question: &str, until: Option<u64>) -> Result<String, CommanderError> {
        for line in question.split('\n') {
            self.deps.writer.write_line(line);
        }
        let mut buffer: Vec<u8> = Vec::new();
        loop {
            if self.deps.cancel.load(Ordering::SeqCst) {
                return Err(CommanderError::from_id(error_id::PROMPT_CANCELED));
            }
            let wait = self.wait_slice(until)?;
            let event = {
                let mut io = self.deps.io.lock().expect("prompt io lock is never poisoned");
                io.read_char(wait)
            };
            match event {
                IoEvent::Byte(b'\n' | b'\r') => {
                    return Ok(String::from_utf8_lossy(&buffer).into_owned());
                }
                IoEvent::Byte(0x03 | 0x04) => {
                    return Err(CommanderError::from_id(error_id::PROMPT_CANCELED));
                }
                IoEvent::Byte(byte) => buffer.push(byte),
                IoEvent::Timeout => {}
                IoEvent::Eof => return Err(CommanderError::from_id(error_id::PROMPT_CANCELED)),
            }
        }
    }

    /// Reads one secret line with echo off; the caller owns the guard.
    fn read_secret(
        &self,
        io: &mut dyn PromptIo,
        until: Option<u64>,
    ) -> Result<String, CommanderError> {
        let mut buffer: Vec<u8> = Vec::new();
        loop {
            if self.deps.cancel.load(Ordering::SeqCst) {
                return Err(CommanderError::from_id(error_id::PROMPT_CANCELED));
            }
            let wait = self.wait_slice(until)?;
            match io.read_char(wait) {
                IoEvent::Byte(b'\n' | b'\r') => {
                    return Ok(String::from_utf8_lossy(&buffer).into_owned());
                }
                IoEvent::Byte(0x03 | 0x04) => {
                    return Err(CommanderError::from_id(error_id::PROMPT_CANCELED));
                }
                IoEvent::Byte(0x08 | 0x7f) => {
                    buffer.pop();
                }
                IoEvent::Byte(0x1b) => {}
                IoEvent::Byte(byte) => buffer.push(byte),
                IoEvent::Timeout => {}
                IoEvent::Eof => return Err(CommanderError::from_id(error_id::PROMPT_CANCELED)),
            }
        }
    }
}

/// Releases the single active prompt lease when the operation completes.
struct LeaseGuard<'a>(&'a AtomicBool);

impl Drop for LeaseGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn descriptor_of(req: &AwaitActionRequest) -> ActionRequiredDescriptor {
    ActionRequiredDescriptor {
        action_id: req.action_id.clone(),
        status_ref: req.status_ref.clone(),
        expires_at: req.expires_at.clone(),
        next_action: req.next_action.clone(),
    }
}

impl PromptApi for PromptEngine {
    fn confirm(&self, req: &ConfirmRequest) -> Result<bool, CommanderError> {
        let _lease = self.acquire()?;
        let until = self.deadline(req.deadline_ms);
        let choices = self.hint("commander.prompt.confirm_choices", &BTreeMap::new());
        let question = self.t(&req.key, &BTreeMap::new());
        let question = if choices.is_empty() { question } else { format!("{question} {choices}") };
        loop {
            let line = self.ask_line(&question, until)?;
            let value = line.trim().to_lowercase();
            if value == "y" || value == "yes" {
                return Ok(true);
            }
            if value == "n" || value == "no" {
                return Ok(false);
            }
            if value.is_empty() {
                if let Some(default) = req.default {
                    return Ok(default);
                }
            }
            // Anything else (including empty without a declared default) re-prompts.
        }
    }

    fn input(&self, req: &InputRequest) -> Result<String, CommanderError> {
        let _lease = self.acquire()?;
        let until = self.deadline(req.deadline_ms);
        let question = self.t(&req.key, &BTreeMap::new());
        loop {
            let value = self.ask_line(&question, until)?;
            let Some(problem) = req.validate.as_ref().and_then(|validate| validate(&value)) else {
                return Ok(value);
            };
            let hint = self.hint(&problem.code, &problem.args);
            let message = if hint.is_empty() { problem.code.clone() } else { hint };
            self.deps.writer.write_line(&message);
        }
    }

    fn select(&self, req: &SelectRequest) -> Result<String, CommanderError> {
        let _lease = self.acquire()?;
        let until = self.deadline(req.deadline_ms);
        let mut lines = vec![self.t(&req.key, &BTreeMap::new())];
        for (index, choice) in req.choices.iter().enumerate() {
            lines.push(format!("  {}) {}", index + 1, self.t(&choice.label_key, &BTreeMap::new())));
        }
        let question = format!("{}\n", lines.join("\n"));
        loop {
            let line = self.ask_line(&question, until)?;
            let trimmed = line.trim();
            let picked = trimmed
                .parse::<f64>()
                .ok()
                .filter(|number| number.fract() == 0.0 && *number >= 1.0)
                .map(|number| number as usize)
                .filter(|index| *index <= req.choices.len());
            if let Some(index) = picked {
                return Ok(req.choices[index - 1].value.clone());
            }
            let mut args = BTreeMap::new();
            args.insert("value".to_string(), trimmed.to_string());
            let hint = self.hint("commander.prompt.select_invalid", &args);
            let message = if hint.is_empty() { trimmed.to_string() } else { hint };
            self.deps.writer.write_line(&message);
        }
    }

    fn password(&self, req: &PasswordRequest) -> Result<SecretString, CommanderError> {
        let _lease = self.acquire()?;
        let until = self.deadline(req.deadline_ms);
        self.deps.writer.write_line(&self.t(&req.key, &BTreeMap::new()));
        let mut io = self.deps.io.lock().expect("prompt io lock is never poisoned");
        if !io.supports_echo_control() {
            return Err(CommanderError::from_id(error_id::PASSWORD_UNSUPPORTED));
        }
        let Some(echo) = io.disable_echo() else {
            return Err(CommanderError::from_id(error_id::PASSWORD_UNSUPPORTED));
        };
        let result = self.read_secret(&mut *io, until);
        drop(echo);
        drop(io);
        let secret = result?;
        self.deps.writer.write_line("");
        Ok(SecretString::new(secret))
    }

    fn await_action(&self, req: &AwaitActionRequest) -> Result<AwaitActionOutcome, CommanderError> {
        if !self.hooks.interactive || self.hooks.stdin_claimed.load(Ordering::SeqCst) {
            return Ok(AwaitActionOutcome::ActionRequired(descriptor_of(req)));
        }
        let _lease = self.acquire()?;
        let until = self.deadline(req.deadline_ms);
        let body = self.t(&req.instructions_key, &req.args);
        let hint = self.hint("commander.prompt.await_action_hint", &BTreeMap::new());
        let question =
            if hint.is_empty() { format!("{body}\n") } else { format!("{body}\n{hint}\n") };
        loop {
            let line = self.ask_line(&question, until)?;
            let value = line.trim().to_lowercase();
            if value.is_empty() {
                return Ok(AwaitActionOutcome::Confirmed);
            }
            if value == "d" || value == "defer" {
                return Ok(AwaitActionOutcome::Deferred);
            }
        }
    }
}
