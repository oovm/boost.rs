//! Conformance for the five prompt primitives over a scripted IO seam.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use commander_core::{
    AwaitActionOutcome, AwaitActionRequest, Clock, ConfirmRequest, Environment, InputRequest, InputValidationError, Localizer, PasswordRequest,
    PromptApi, PromptHooks, SelectChoice, SelectRequest, Writer,
};
use commander_prompt::{EchoRestore, EngineDeps, IoEvent, PromptEngine, PromptIo};
use commander_types::{CommanderError, ExitCode, error_id};

struct MapLocalizer(BTreeMap<String, String>);

impl MapLocalizer {
    fn new(entries: &[(&str, &str)]) -> Self {
        Self(entries.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect())
    }
}

impl Localizer for MapLocalizer {
    fn t(&self, key: &str, _args: &BTreeMap<String, String>) -> String {
        self.0.get(key).cloned().unwrap_or_else(|| format!("{{{key}}}"))
    }
}

struct LogWriter(Arc<Mutex<Vec<String>>>);

impl Writer for LogWriter {
    fn write_line(&self, line: &str) {
        self.0.lock().expect("log").push(line.to_string());
    }
}

struct StepClock(AtomicU64);

impl Clock for StepClock {
    fn now_ms(&self) -> u64 {
        self.0.fetch_add(10, Ordering::SeqCst)
    }
}

/// Scripted IO: pops events, then EOF forever. Tracks echo disable/restore.
struct FakeIo {
    events: VecDeque<IoEvent>,
    echo_capable: bool,
    echo_off: Arc<AtomicBool>,
    reads: Arc<AtomicBool>,
}

struct FakeEchoGuard(Arc<AtomicBool>);

impl Drop for FakeEchoGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl EchoRestore for FakeEchoGuard {}

impl PromptIo for FakeIo {
    fn read_char(&mut self, _timeout_ms: u64) -> IoEvent {
        self.reads.store(true, Ordering::SeqCst);
        self.events.pop_front().unwrap_or(IoEvent::Eof)
    }

    fn supports_echo_control(&self) -> bool {
        self.echo_capable
    }

    fn disable_echo(&mut self) -> Option<Box<dyn EchoRestore>> {
        if !self.echo_capable {
            return None;
        }
        self.echo_off.store(true, Ordering::SeqCst);
        Some(Box::new(FakeEchoGuard(Arc::clone(&self.echo_off))))
    }
}

fn bytes(text: &str) -> VecDeque<IoEvent> {
    text.bytes().map(IoEvent::Byte).collect()
}

struct Harness {
    log: Arc<Mutex<Vec<String>>>,
    echo_off: Arc<AtomicBool>,
    reads: Arc<AtomicBool>,
    lease: Arc<AtomicBool>,
    stdin_claimed: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

impl Harness {
    fn lines(&self) -> Vec<String> {
        self.log.lock().expect("log").clone()
    }
}

fn make_engine(events: VecDeque<IoEvent>, echo_capable: bool, interactive: bool) -> (PromptEngine, Harness) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let echo_off = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicBool::new(false));
    let lease = Arc::new(AtomicBool::new(false));
    let stdin_claimed = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));
    let deps = EngineDeps {
        io: Arc::new(Mutex::new(FakeIo { events, echo_capable, echo_off: Arc::clone(&echo_off), reads: Arc::clone(&reads) })),
        writer: Arc::new(LogWriter(Arc::clone(&log))),
        localizer: Arc::new(MapLocalizer::new(&[
            ("cli.prompt.go", "Proceed?"),
            ("cli.prompt.name", "Name"),
            ("cli.prompt.pick", "Pick one"),
            ("cli.label.a", "Alpha"),
            ("cli.label.b", "Beta"),
            ("cli.prompt.secret", "Token"),
            ("cli.prompt.dns", "Add the TXT record"),
            ("commander.prompt.confirm_choices", "(y/n)"),
            ("commander.prompt.await_action_hint", "Press enter to continue or d to defer."),
            ("demo::too_short", "too short"),
        ])),
        clock: Arc::new(StepClock(AtomicU64::new(0))),
        cancel: Arc::clone(&cancel),
    };
    let hooks = PromptHooks { interactive, lease: Arc::clone(&lease), stdin_claimed: Arc::clone(&stdin_claimed) };
    (PromptEngine::new(deps, hooks), Harness { log, echo_off, reads, lease, stdin_claimed, cancel })
}

fn assert_err(err: CommanderError, id: &str, exit: ExitCode) {
    assert_eq!(err.id, id, "message args: {:?}", err.args);
    assert_eq!(err.exit, exit);
}

// --- eligibility and lease ----------------------------------------------------

#[test]
fn non_interactive_acquire_fails_as_unavailable() {
    let (engine, harness) = make_engine(bytes("y\n"), true, false);
    let err = engine.confirm(&ConfirmRequest::new("cli.prompt.go")).unwrap_err();
    assert_err(err, error_id::INTERACTIVE_UNAVAILABLE, ExitCode::Capability);
    assert!(!harness.reads.load(Ordering::SeqCst), "no bytes are read when ineligible");
    assert!(harness.lines().is_empty(), "no question is written when ineligible");
}

#[test]
fn claimed_stdin_blocks_prompts() {
    let (engine, harness) = make_engine(bytes("y\n"), true, true);
    harness.stdin_claimed.store(true, Ordering::SeqCst);
    let err = engine.confirm(&ConfirmRequest::new("cli.prompt.go")).unwrap_err();
    assert_err(err, error_id::INTERACTIVE_UNAVAILABLE, ExitCode::Capability);
}

#[test]
fn busy_lease_fails_as_interaction_busy() {
    let (engine, harness) = make_engine(bytes("y\n"), true, true);
    harness.lease.store(true, Ordering::SeqCst);
    let err = engine.confirm(&ConfirmRequest::new("cli.prompt.go")).unwrap_err();
    assert_err(err, error_id::INTERACTION_BUSY, ExitCode::Capability);
}

#[test]
fn lease_releases_after_success() {
    let (engine, harness) = make_engine(bytes("y\n"), true, true);
    assert!(engine.confirm(&ConfirmRequest::new("cli.prompt.go")).unwrap());
    assert!(!harness.lease.load(Ordering::SeqCst));
}

// --- confirm ------------------------------------------------------------------

#[test]
fn confirm_accepts_yes_no_and_default_on_empty() {
    let (engine, harness) = make_engine(bytes("y\n"), true, true);
    assert!(engine.confirm(&ConfirmRequest::new("cli.prompt.go")).unwrap());
    assert_eq!(harness.lines(), vec!["Proceed? (y/n)"], "question with choices hint goes to stderr");

    let (engine, _h) = make_engine(bytes("NO\n"), true, true);
    assert!(!engine.confirm(&ConfirmRequest::new("cli.prompt.go")).unwrap());

    let (engine, _h) = make_engine(bytes("\n"), true, true);
    assert!(engine.confirm(&ConfirmRequest::new("cli.prompt.go").default_value(true)).unwrap());

    let (engine, harness) = make_engine(bytes("maybe\nyes\n"), true, true);
    assert!(engine.confirm(&ConfirmRequest::new("cli.prompt.go")).unwrap());
    assert_eq!(harness.lines(), vec!["Proceed? (y/n)", "Proceed? (y/n)"], "garbage re-prompts");
}

#[test]
fn confirm_eof_cancels() {
    let (engine, _h) = make_engine(VecDeque::new(), true, true);
    let err = engine.confirm(&ConfirmRequest::new("cli.prompt.go")).unwrap_err();
    assert_err(err, error_id::PROMPT_CANCELED, ExitCode::Canceled);
}

#[test]
fn confirm_ctrl_c_and_ctrl_d_cancel() {
    for control in [0x03u8, 0x04] {
        let mut events = VecDeque::new();
        events.push_back(IoEvent::Byte(control));
        let (engine, _h) = make_engine(events, true, true);
        let err = engine.confirm(&ConfirmRequest::new("cli.prompt.go")).unwrap_err();
        assert_err(err, error_id::PROMPT_CANCELED, ExitCode::Canceled);
    }
}

#[test]
fn confirm_absolute_deadline_times_out_immediately() {
    let (engine, _h) = make_engine(bytes("y\n"), true, true);
    let err = engine.confirm(&ConfirmRequest::new("cli.prompt.go").deadline_ms(0)).unwrap_err();
    assert_err(err, error_id::PROMPT_TIMEOUT, ExitCode::Canceled);
}

// --- input --------------------------------------------------------------------

#[test]
fn input_returns_raw_line_and_retries_on_validation() {
    let (engine, harness) = make_engine(bytes("ab\nabcd\n"), true, true);
    let req = InputRequest::new("cli.prompt.name")
        .validate(|value| if value.chars().count() < 3 { Some(InputValidationError::new("demo::too_short")) } else { None });
    assert_eq!(engine.input(&req).unwrap(), "abcd");
    let captured = harness.lines();
    assert_eq!(captured[0], "Name");
    assert_eq!(captured[1], "too short", "validation feedback goes to stderr");
    assert_eq!(captured[2], "Name", "the question is re-asked after feedback");
}

#[test]
fn input_validation_falls_back_to_bare_code() {
    let (engine, harness) = make_engine(bytes("ab\nabcd\n"), true, true);
    let req = InputRequest::new("cli.prompt.name")
        .validate(|value| if value == "ab" { Some(InputValidationError::new("demo::unlocalized")) } else { None });
    assert_eq!(engine.input(&req).unwrap(), "abcd");
    assert!(harness.lines().iter().any(|l| l == "demo::unlocalized"));
}

#[test]
fn input_accepts_utf8_and_keeps_trailing_spaces() {
    let (engine, _h) = make_engine(bytes("你好 \n"), true, true);
    assert_eq!(engine.input(&InputRequest::new("cli.prompt.name")).unwrap(), "你好 ");
}

// --- select -------------------------------------------------------------------

#[test]
fn select_lists_choices_and_returns_stable_value() {
    let (engine, harness) = make_engine(bytes("2\n"), true, true);
    let req = SelectRequest::new("cli.prompt.pick", vec![SelectChoice::new("alpha", "cli.label.a"), SelectChoice::new("beta", "cli.label.b")]);
    assert_eq!(engine.select(&req).unwrap(), "beta");
    assert_eq!(harness.lines(), vec!["Pick one", "  1) Alpha", "  2) Beta", ""], "list on stderr");
}

#[test]
fn select_rejects_out_of_range_and_reprompts() {
    let (engine, harness) = make_engine(bytes("9\n1\n"), true, true);
    let req = SelectRequest::new("cli.prompt.pick", vec![SelectChoice::new("alpha", "cli.label.a")]);
    assert_eq!(engine.select(&req).unwrap(), "alpha");
    assert!(harness.lines().iter().any(|l| l == "9"), "unresolved invalid template falls back to the raw line");
}

// --- password -----------------------------------------------------------------

#[test]
fn password_requires_echo_control_and_never_degrades() {
    let (engine, harness) = make_engine(bytes("s3cret\n"), false, true);
    let err = engine.password(&PasswordRequest::new("cli.prompt.secret")).unwrap_err();
    assert_err(err, error_id::PASSWORD_UNSUPPORTED, ExitCode::Capability);
    assert_eq!(harness.lines(), vec!["Token"], "question only; no visible fallback read");
}

#[test]
fn password_reads_secret_and_restores_echo() {
    let (engine, harness) = make_engine(bytes("s3\n"), true, true);
    let secret = engine.password(&PasswordRequest::new("cli.prompt.secret")).unwrap();
    assert_eq!(secret.expose_secret(), "s3");
    assert!(!harness.echo_off.load(Ordering::SeqCst), "echo restored after success");
    let captured = harness.lines();
    assert_eq!(captured[0], "Token");
    assert_eq!(captured[captured.len() - 1], "", "newline written after the secret line");
}

#[test]
fn password_backspace_and_escape_are_handled() {
    let mut events = VecDeque::new();
    events.extend([IoEvent::Byte(b'a'), IoEvent::Byte(0x7f), IoEvent::Byte(b'b'), IoEvent::Byte(0x1b), IoEvent::Byte(b'\n')]);
    let (engine, _h) = make_engine(events, true, true);
    let secret = engine.password(&PasswordRequest::new("cli.prompt.secret")).unwrap();
    assert_eq!(secret.expose_secret(), "b");
}

#[test]
fn password_ctrl_c_restores_echo_and_cancels() {
    let mut events = VecDeque::new();
    events.push_back(IoEvent::Byte(0x03));
    let (engine, harness) = make_engine(events, true, true);
    let err = engine.password(&PasswordRequest::new("cli.prompt.secret")).unwrap_err();
    assert_err(err, error_id::PROMPT_CANCELED, ExitCode::Canceled);
    assert!(!harness.echo_off.load(Ordering::SeqCst), "echo restored on the cancel path");
}

#[test]
fn password_timeout_restores_echo() {
    let (engine, harness) = make_engine(VecDeque::from([IoEvent::Timeout]), true, true);
    let err = engine.password(&PasswordRequest::new("cli.prompt.secret").deadline_ms(0)).unwrap_err();
    assert_err(err, error_id::PROMPT_TIMEOUT, ExitCode::Canceled);
    assert!(!harness.echo_off.load(Ordering::SeqCst));
}

// --- await_action ---------------------------------------------------------------

#[test]
fn await_action_outside_interactivity_returns_descriptor_without_reading() {
    let (engine, harness) = make_engine(VecDeque::new(), true, false);
    let req = AwaitActionRequest::new("dns-txt", "cli.prompt.dns").status_ref("order-7");
    match engine.await_action(&req).unwrap() {
        AwaitActionOutcome::ActionRequired(descriptor) => {
            assert_eq!(descriptor.action_id, "dns-txt");
            assert_eq!(descriptor.status_ref.as_deref(), Some("order-7"));
        }
        other => panic!("expected action required, got {other:?}"),
    }
    assert!(!harness.reads.load(Ordering::SeqCst), "never hangs outside interactivity");
    assert!(harness.lines().is_empty());
}

#[test]
fn await_action_confirms_on_empty_line_and_defers_on_d() {
    let (engine, harness) = make_engine(bytes("\n"), true, true);
    let req = AwaitActionRequest::new("dns-txt", "cli.prompt.dns");
    assert_eq!(engine.await_action(&req).unwrap(), AwaitActionOutcome::Confirmed);
    assert_eq!(harness.lines(), vec!["Add the TXT record", "Press enter to continue or d to defer.", ""], "body and hint go to stderr");

    let (engine, _h) = make_engine(bytes("d\n"), true, true);
    assert_eq!(engine.await_action(&req).unwrap(), AwaitActionOutcome::Deferred);

    let (engine, _h) = make_engine(bytes("huh\ndefer\n"), true, true);
    assert_eq!(engine.await_action(&req).unwrap(), AwaitActionOutcome::Deferred);
}

#[test]
fn await_action_eof_cancels() {
    let (engine, _h) = make_engine(VecDeque::new(), true, true);
    let err = engine.await_action(&AwaitActionRequest::new("dns-txt", "cli.prompt.dns")).unwrap_err();
    assert_err(err, error_id::PROMPT_CANCELED, ExitCode::Canceled);
}

// --- cancellation token ---------------------------------------------------------

#[test]
fn cancel_token_cancels_an_awaiting_line() {
    let (engine, harness) = make_engine(bytes("y\n"), true, true);
    harness.cancel.store(true, Ordering::SeqCst);
    let err = engine.confirm(&ConfirmRequest::new("cli.prompt.go")).unwrap_err();
    assert_err(err, error_id::PROMPT_CANCELED, ExitCode::Canceled);
}

// --- Environment snapshot sanity (shared rule with the core) ---------------------

#[test]
fn environment_ci_rule_is_shared_with_the_core() {
    let mut env = BTreeMap::new();
    env.insert("CI".to_string(), "false".to_string());
    assert!(!Environment { tty: true, env }.ci());
}
