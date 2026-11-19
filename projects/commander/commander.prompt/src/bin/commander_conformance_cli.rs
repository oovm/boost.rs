//! `commander.conformance/v0` fixture CLI (Rust engine side).
//!
//! One command per prompt primitive plus engine-level cases (stdin conflict,
//! machine paths, deferred actions). The shared fixture directory is run
//! against this binary and the TypeScript twin, so every asserted behavior
//! is contract, not convenience. Secrets never leave the secret handle and
//! never reach stdout.
//!
//! Pipe-mode adaptation: the fixture driver spawns this binary with piped
//! streams. A `COMMANDER_CONFORMANCE_FORCE_TTY=1` entry in the sealed child
//! environment reproduces the injected-TTY surface the TypeScript in-process
//! driver uses for its `tty` fixtures. On a forced pipe there is no real
//! local echo, so echo control succeeds through a no-op guard, mirroring the
//! fake raw-mode recorder of the injected stream; a real terminal keeps the
//! genuine echo-free path.

use std::{
    collections::BTreeMap,
    io::Write,
    sync::{Arc, Mutex},
};

use boost_terminal::{ReadEvent, is_tty_input, is_tty_output, read_char_timeout};
use commander_core::{
    ActionRequiredDescriptor, AwaitActionOutcome, AwaitActionRequest, Cli, Command, CommandContext, CommanderError, ConfirmRequest,
    ContinuationSpec, Environment, HandlerOutcome, InputRequest, InputValidationError, InteractiveSpec, Localizer, NextAction, OptionSpec,
    PasswordRequest, Ports, PromptApi, PromptFactory, PromptHooks, PromptPolicy, SelectChoice, SelectRequest, SystemClock, Writer, error_id,
    normalize_argv,
};
use commander_prompt::{EchoRestore, EngineDeps, IoEvent, PromptEngine, PromptIo, TerminalPromptIo};
use serde_json::json;

/// Stable action id used by the await-action fixtures.
const DEMO_ACTION_ID: &str = "conf-dns01-1";
/// Persisted-state reference carried by the deferred descriptor.
const DEMO_STATUS_REF: &str = "conf/state/1";
/// Expiry stamp carried by the deferred descriptor.
const DEMO_EXPIRES_AT: &str = "2026-12-31T00:00:00Z";
/// Driver knob that mirrors the TypeScript in-process injected-TTY surface.
const FORCE_TTY_ENV: &str = "COMMANDER_CONFORMANCE_FORCE_TTY";

// --- host ports -----------------------------------------------------------------

struct StderrWriter;

impl Writer for StderrWriter {
    fn write_line(&self, line: &str) {
        let mut stderr = std::io::stderr().lock();
        let _ = writeln!(stderr, "{line}");
    }
}

/// English catalog: product `conf.*` rows plus the framework chrome the
/// fixtures observe on stderr.
const EN: &[(&str, &str)] = &[
    ("conf.intro", "conformance fixture CLI"),
    ("conf.cmd.confirm", "ask a yes/no question"),
    ("conf.cmd.input", "ask for a line of text"),
    ("conf.cmd.select", "pick one of three choices"),
    ("conf.cmd.password", "collect a secret without echo"),
    ("conf.cmd.await-action", "prepare or resume a persisted external action"),
    ("conf.cmd.conflict", "claim stdin then try to prompt"),
    ("conf.cmd.boom", "throw an arbitrary error"),
    ("conf.opt.value", "machine-path value (skips the prompt)"),
    ("conf.opt.default", "confirm default for an empty line (yes or no)"),
    ("conf.opt.deadline", "absolute prompt deadline in milliseconds"),
    ("conf.opt.secret-from-env", "read the secret from this environment variable instead of prompting"),
    ("conf.opt.action", "stable action id to resume"),
    ("conf.prompt.confirm", "Proceed?"),
    ("conf.prompt.input", "Enter a name"),
    ("conf.prompt.select", "Pick a channel"),
    ("conf.prompt.password", "Enter the secret"),
    ("conf.prompt.await", "Place these records, then confirm:\n{records}"),
    ("conf.choice.alpha", "alpha"),
    ("conf.choice.beta", "beta"),
    ("conf.choice.gamma", "gamma"),
    ("conf.err.empty", "the value must not be empty"),
    ("commander::prompt_canceled", "prompt canceled"),
    ("commander::prompt_timeout", "prompt timed out"),
    ("commander::interactive_unavailable", "interactive prompting is unavailable"),
    ("commander::interaction_busy", "another prompt is already active"),
    ("commander::password_unsupported", "the terminal cannot collect a secret without echo"),
    ("commander::unknown_command", "unknown command: {cmd}"),
    ("commander::unknown_option", "unknown option: {option}"),
    ("commander::missing_option_value", "option {option} requires a value"),
    ("commander::missing_required_option", "missing required option: --{option}"),
    ("commander::option_not_in_enum", "value {value} is not allowed for --{option}"),
    ("commander::option_coerce_failed", "value {value} is invalid for --{option}"),
    ("commander::action_required", "action required: {actionId}"),
    ("commander.ui.usage", "Usage: {name} <command> [options]"),
    ("commander.ui.commands", "Commands:"),
    ("commander.ui.options", "Options:"),
    ("commander.ui.hint.available_options", "Run `{command} --help` for the available options."),
    ("commander.ui.hint.did_you_mean", "did you mean {suggest}?"),
];

/// Simplified-Chinese catalog: the product `conf.*` rows.
const ZH_CN: &[(&str, &str)] = &[
    ("conf.intro", "符合性夹具 CLI"),
    ("conf.cmd.confirm", "询问一个是/否问题"),
    ("conf.cmd.input", "请求一行文本"),
    ("conf.cmd.select", "从三个选项中选择一个"),
    ("conf.cmd.password", "无回显收集秘密"),
    ("conf.cmd.await-action", "准备或续跑已落盘的外部动作"),
    ("conf.cmd.conflict", "占用 stdin 后尝试提示"),
    ("conf.cmd.boom", "抛出任意错误"),
    ("conf.opt.value", "机器路径值（跳过提示）"),
    ("conf.opt.default", "空回车时的确认默认值（yes 或 no）"),
    ("conf.opt.deadline", "提示的绝对期限（毫秒）"),
    ("conf.opt.secret-from-env", "从该环境变量读取秘密而不提示"),
    ("conf.opt.action", "要续跑的稳定动作 id"),
    ("conf.prompt.confirm", "是否继续？"),
    ("conf.prompt.input", "请输入名称"),
    ("conf.prompt.select", "请选择通道"),
    ("conf.prompt.password", "请输入秘密"),
    ("conf.prompt.await", "请放置以下记录后确认：\n{records}"),
    ("conf.choice.alpha", "alpha"),
    ("conf.choice.beta", "beta"),
    ("conf.choice.gamma", "gamma"),
    ("conf.err.empty", "值不能为空"),
];

fn catalog_row(locale: &str, key: &str) -> Option<&'static str> {
    let table: &[(&str, &str)] = if locale == "zh-CN" { ZH_CN } else { EN };
    table
        .iter()
        .find(|(row_key, _)| *row_key == key)
        .map(|(_, value)| *value)
        .or_else(|| if locale == "zh-CN" { catalog_row("en-US", key) } else { None })
}

struct CatalogLocalizer {
    locale: String,
}

impl Localizer for CatalogLocalizer {
    fn t(&self, key: &str, args: &BTreeMap<String, String>) -> String {
        match catalog_row(&self.locale, key) {
            None => format!("{{{key}}}"),
            Some(template) => {
                let mut out = template.to_string();
                for (name, value) in args {
                    out = out.replace(&format!("{{{name}}}"), value);
                }
                out
            }
        }
    }
}

// --- prompt IO seam ---------------------------------------------------------------

/// No-op echo guard for forced pipe runs (mirrors the fake raw-mode stream).
struct PipeEchoGuard;

impl EchoRestore for PipeEchoGuard {}

/// Reads the real standard input through the terminal primitives; on a
/// forced pipe, echo control resolves to the no-op guard.
///
/// Pipe reads go through one background blocking reader plus a channel so
/// the engine's bounded waits stay honored: a child's piped stdin handle
/// does not support the console wait-then-read combination.
struct ConformanceIo {
    forced: bool,
    pipe_rx: Option<std::sync::mpsc::Receiver<ReadEvent>>,
}

fn spawn_pipe_reader() -> std::sync::mpsc::Receiver<ReadEvent> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read as _;
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        let mut byte = [0u8; 1];
        loop {
            match handle.read(&mut byte) {
                Ok(1) => {
                    if sender.send(ReadEvent::Byte(byte[0])).is_err() {
                        break;
                    }
                }
                Ok(_) => {
                    let _ = sender.send(ReadEvent::Eof);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => {
                    let _ = sender.send(ReadEvent::Eof);
                    break;
                }
            }
        }
    });
    receiver
}

impl PromptIo for ConformanceIo {
    fn read_char(&mut self, timeout_ms: u64) -> IoEvent {
        if is_tty_input() {
            return match read_char_timeout(timeout_ms) {
                ReadEvent::Byte(byte) => IoEvent::Byte(byte),
                ReadEvent::Eof => IoEvent::Eof,
                ReadEvent::Timeout => IoEvent::Timeout,
            };
        }
        let receiver = self.pipe_rx.get_or_insert_with(spawn_pipe_reader);
        match receiver.recv_timeout(std::time::Duration::from_millis(timeout_ms.max(1))) {
            Ok(ReadEvent::Byte(byte)) => IoEvent::Byte(byte),
            Ok(ReadEvent::Eof) => IoEvent::Eof,
            Ok(ReadEvent::Timeout) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => IoEvent::Timeout,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => IoEvent::Eof,
        }
    }

    fn supports_echo_control(&self) -> bool {
        is_tty_input() || self.forced
    }

    fn disable_echo(&mut self) -> Option<Box<dyn EchoRestore>> {
        if is_tty_input() {
            TerminalPromptIo.disable_echo()
        }
        else if self.forced {
            Some(Box::new(PipeEchoGuard))
        }
        else {
            None
        }
    }
}

/// Builds one [`PromptEngine`] per dispatched command context.
struct ConformanceFactory {
    deps: EngineDeps,
}

impl PromptFactory for ConformanceFactory {
    fn create(&self, hooks: PromptHooks) -> Arc<dyn PromptApi> {
        Arc::new(PromptEngine::new(self.deps.clone(), hooks))
    }
}

// --- option helpers -------------------------------------------------------------

fn str_option(ctx: &CommandContext<'_>, key: &str) -> Option<String> {
    ctx.options().str_value(key).map(str::to_string)
}

fn deadline_ms(ctx: &CommandContext<'_>) -> Option<u64> {
    str_option(ctx, "deadline").and_then(|raw| raw.parse::<u64>().ok())
}

fn interactive(prompt_key: &str, machine_input: &str, resume: bool) -> InteractiveSpec {
    InteractiveSpec {
        policy: PromptPolicy::Optional,
        prompt_keys: BTreeMap::from([(prompt_key.to_string(), format!("conf.prompt.{prompt_key}"))]),
        non_interactive_inputs: vec![machine_input.to_string()],
        continuation: if resume { Some(ContinuationSpec { resume_inputs: vec![machine_input.to_string()] }) } else { None },
    }
}

// --- command tree ---------------------------------------------------------------

fn build_cli(env_map: &BTreeMap<String, String>) -> Cli {
    let cli = Cli::new("conformance-cli").intro("conf.intro").global_option(OptionSpec::required_value("locale").hidden());

    let cli = cli.command(
        Command::new("confirm")
            .help("conf.cmd.confirm")
            .option(OptionSpec::required_value("value").help("conf.opt.value"))
            .option(OptionSpec::required_value("default").help("conf.opt.default"))
            .option(OptionSpec::required_value("deadline").help("conf.opt.deadline"))
            .interactive(interactive("confirm", "value", false))
            .handler(Arc::new(|ctx: &mut CommandContext<'_>| {
                if let Some(machine) = str_option(ctx, "value") {
                    let confirmed = machine == "yes";
                    let data = json!({"confirmed": confirmed, "source": "machine"});
                    ctx.emit_result(data.clone(), Some(format!("confirmed={confirmed} source=machine")));
                    return HandlerOutcome::Ok(data);
                }
                let mut request =
                    ConfirmRequest::new("conf.prompt.confirm").default_value(str_option(ctx, "default").as_deref() == Some("yes"));
                if let Some(ms) = deadline_ms(ctx) {
                    request = request.deadline_ms(ms);
                }
                match ctx.confirm(&request) {
                    Ok(confirmed) => {
                        let data = json!({"confirmed": confirmed});
                        ctx.emit_result(data.clone(), Some(format!("confirmed={confirmed}")));
                        HandlerOutcome::Ok(data)
                    }
                    Err(err) => HandlerOutcome::Err(err),
                }
            })),
    );

    let cli = cli.command(
        Command::new("input")
            .help("conf.cmd.input")
            .option(OptionSpec::required_value("value").help("conf.opt.value"))
            .option(OptionSpec::required_value("deadline").help("conf.opt.deadline"))
            .interactive(interactive("input", "value", false))
            .handler(Arc::new(|ctx: &mut CommandContext<'_>| {
                if let Some(machine) = str_option(ctx, "value") {
                    let data = json!({"value": machine.clone(), "source": "machine"});
                    ctx.emit_result(data.clone(), Some(machine));
                    return HandlerOutcome::Ok(data);
                }
                let mut request = InputRequest::new("conf.prompt.input")
                    .validate(|text| if text.is_empty() { Some(InputValidationError::new("conf.err.empty")) } else { None });
                if let Some(ms) = deadline_ms(ctx) {
                    request = request.deadline_ms(ms);
                }
                match ctx.input(&request) {
                    Ok(value) => {
                        let data = json!({"value": value.clone()});
                        ctx.emit_result(data.clone(), Some(value));
                        HandlerOutcome::Ok(data)
                    }
                    Err(err) => HandlerOutcome::Err(err),
                }
            })),
    );

    let cli = cli.command(
        Command::new("select")
            .help("conf.cmd.select")
            .option(OptionSpec::required_value("value").help("conf.opt.value"))
            .option(OptionSpec::required_value("deadline").help("conf.opt.deadline"))
            .interactive(interactive("select", "value", false))
            .handler(Arc::new(|ctx: &mut CommandContext<'_>| {
                let choices = vec![
                    SelectChoice::new("alpha", "conf.choice.alpha"),
                    SelectChoice::new("beta", "conf.choice.beta"),
                    SelectChoice::new("gamma", "conf.choice.gamma"),
                ];
                let machine = str_option(ctx, "value");
                if let Some(picked) = choices.iter().find(|choice| Some(&choice.value) == machine.as_ref()) {
                    let value = picked.value.clone();
                    let data = json!({"value": value.clone(), "source": "machine"});
                    ctx.emit_result(data.clone(), Some(value));
                    return HandlerOutcome::Ok(data);
                }
                let mut request = SelectRequest::new("conf.prompt.select", choices);
                if let Some(ms) = deadline_ms(ctx) {
                    request = request.deadline_ms(ms);
                }
                match ctx.select(&request) {
                    Ok(value) => {
                        let data = json!({"value": value.clone()});
                        ctx.emit_result(data.clone(), Some(value));
                        HandlerOutcome::Ok(data)
                    }
                    Err(err) => HandlerOutcome::Err(err),
                }
            })),
    );

    let secret_env = env_map.clone();
    let cli = cli.command(
        Command::new("password")
            .help("conf.cmd.password")
            .option(OptionSpec::required_value("secret-from-env").help("conf.opt.secret-from-env"))
            .option(OptionSpec::required_value("deadline").help("conf.opt.deadline"))
            .interactive(interactive("password", "secret-from-env", false))
            .handler(Arc::new(move |ctx: &mut CommandContext<'_>| {
                if let Some(env_name) = str_option(ctx, "secret-from-env") {
                    let raw = secret_env.get(&env_name).cloned().unwrap_or_default();
                    let data = json!({"length": raw.chars().count()});
                    ctx.emit_result(data.clone(), Some(format!("length={}", raw.chars().count())));
                    return HandlerOutcome::Ok(data);
                }
                let mut request = PasswordRequest::new("conf.prompt.password");
                if let Some(ms) = deadline_ms(ctx) {
                    request = request.deadline_ms(ms);
                }
                match ctx.password(&request) {
                    Ok(secret) => {
                        // Only the length is ever reported; the value stays behind the handle.
                        let length = secret.expose_secret().chars().count();
                        let data = json!({"length": length});
                        ctx.emit_result(data.clone(), Some(format!("length={length}")));
                        HandlerOutcome::Ok(data)
                    }
                    Err(err) => HandlerOutcome::Err(err),
                }
            })),
    );

    let cli = cli.command(
        Command::new("await-action")
            .help("conf.cmd.await-action")
            .option(OptionSpec::required_value("action").help("conf.opt.action"))
            .option(OptionSpec::required_value("deadline").help("conf.opt.deadline"))
            .interactive(interactive("await", "action", true))
            .handler(Arc::new(|ctx: &mut CommandContext<'_>| {
                if let Some(resume) = str_option(ctx, "action") {
                    let data = json!({"resumed": resume.clone()});
                    ctx.emit_result(data.clone(), Some(format!("resumed={resume}")));
                    return HandlerOutcome::Ok(data);
                }
                let descriptor = ActionRequiredDescriptor {
                    action_id: DEMO_ACTION_ID.to_string(),
                    status_ref: Some(DEMO_STATUS_REF.to_string()),
                    expires_at: Some(DEMO_EXPIRES_AT.to_string()),
                    next_action: Some(NextAction {
                        command: "conformance-cli".to_string(),
                        argv: vec!["await-action".to_string(), "--action".to_string(), DEMO_ACTION_ID.to_string()],
                    }),
                };
                let mut request = AwaitActionRequest::new(DEMO_ACTION_ID, "conf.prompt.await")
                    .status_ref(DEMO_STATUS_REF)
                    .expires_at(DEMO_EXPIRES_AT)
                    .next_action(descriptor.next_action.clone().expect("descriptor carries the continuation"));
                request.args.insert("records".to_string(), "TXT  _acme-challenge.conf.test  demo-value".to_string());
                if let Some(ms) = deadline_ms(ctx) {
                    request = request.deadline_ms(ms);
                }
                match ctx.await_action(&request) {
                    Ok(AwaitActionOutcome::Confirmed) => {
                        let data = json!({"resumed": DEMO_ACTION_ID});
                        ctx.emit_result(data.clone(), Some(format!("resumed={DEMO_ACTION_ID}")));
                        HandlerOutcome::Ok(data)
                    }
                    Ok(AwaitActionOutcome::Deferred) => ctx.action_required(descriptor),
                    Ok(AwaitActionOutcome::ActionRequired(found)) => ctx.action_required(found),
                    Err(err) => HandlerOutcome::Err(err),
                }
            })),
    );

    let cli = cli.command(
        Command::new("conflict")
            .help("conf.cmd.conflict")
            .interactive(InteractiveSpec {
                policy: PromptPolicy::Never,
                prompt_keys: BTreeMap::from([("confirm".to_string(), "conf.prompt.confirm".to_string())]),
                non_interactive_inputs: Vec::new(),
                continuation: None,
            })
            .handler(Arc::new(|ctx: &mut CommandContext<'_>| {
                ctx.claim_stdin();
                match ctx.confirm(&ConfirmRequest::new("conf.prompt.confirm")) {
                    Ok(confirmed) => {
                        let data = json!({"confirmed": confirmed});
                        ctx.emit_result(data.clone(), Some(format!("confirmed={confirmed}")));
                        HandlerOutcome::Ok(data)
                    }
                    Err(err) => HandlerOutcome::Err(err),
                }
            })),
    );

    cli.command(Command::new("boom").help("conf.cmd.boom").handler(Arc::new(|_ctx| {
        // An arbitrary domain failure surfaces as the stable `action_failed`
        // code; its message never enters the envelope or stdout.
        HandlerOutcome::Err(CommanderError::from_id(error_id::ACTION_FAILED))
    })))
}

// --- entry ------------------------------------------------------------------------

fn resolve_locale(argv: &[String], env_map: &BTreeMap<String, String>) -> String {
    let mut index = 0;
    while index < argv.len() {
        let token = argv[index].as_str();
        if token == "--locale" {
            if let Some(value) = argv.get(index + 1) {
                return value.clone();
            }
        }
        else if let Some(value) = token.strip_prefix("--locale=") {
            return value.to_string();
        }
        index += 1;
    }
    for name in ["LOCALE", "LC_ALL", "LANG"] {
        if let Some(value) = env_map.get(name)
            && !value.is_empty()
        {
            return value.clone();
        }
    }
    "en-US".to_string()
}

fn main() {
    let argv = normalize_argv(&std::env::args().collect::<Vec<String>>());
    let env_map: BTreeMap<String, String> = std::env::vars().collect();
    let forced = env_map.get(FORCE_TTY_ENV).is_some_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
    let tty = (is_tty_input() && is_tty_output()) || forced;
    let locale = resolve_locale(&argv, &env_map);

    let localizer: Arc<dyn Localizer> = Arc::new(CatalogLocalizer { locale: locale.clone() });
    let writer: Arc<dyn Writer> = Arc::new(StderrWriter);
    let ports = Ports::new(Arc::new(SystemClock), Arc::clone(&writer), Arc::clone(&localizer), locale);
    let deps = EngineDeps {
        io: Arc::new(Mutex::new(ConformanceIo { forced, pipe_rx: None })),
        writer,
        localizer,
        clock: Arc::new(SystemClock),
        cancel: Arc::clone(&ports.cancel),
    };
    let factory: Arc<dyn PromptFactory> = Arc::new(ConformanceFactory { deps });

    let cli = build_cli(&env_map);
    let outcome = cli.run(&argv, &Environment { tty, env: env_map }, &ports, Some(&factory));

    if let Some(text) = outcome.stdout {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        let _ = handle.write_all(text.as_bytes());
        let _ = handle.flush();
    }
    std::process::exit(outcome.exit.as_i32());
}
