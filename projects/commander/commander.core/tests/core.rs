//! Behavior conformance for the machine path: parsing, routing, envelopes,
//! exit codes, eligibility, diagnostics, and spec validation.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use commander_core::{
    ActionRequiredDescriptor, Cli, Command, CommanderError, ConfirmRequest, Environment, ExitCode,
    Handler, HandlerOutcome, InteractiveSpec, Localizer, NextAction, OptionSpec, Ports,
    PromptApi, PromptFactory, PromptHooks, Severity, ValueDecl, Writer, error_id,
};
use serde_json::{Value, json};

type Log = Arc<Mutex<Vec<String>>>;

struct TestLocalizer {
    catalog: BTreeMap<String, String>,
}

impl TestLocalizer {
    fn new(entries: &[(&str, &str)]) -> Self {
        Self { catalog: entries.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
    }
}

impl Localizer for TestLocalizer {
    fn t(&self, key: &str, args: &BTreeMap<String, String>) -> String {
        match self.catalog.get(key) {
            None => format!("{{{key}}}"),
            Some(template) => {
                let mut out = template.clone();
                for (name, value) in args {
                    out = out.replace(&format!("{{{name}}}"), value);
                }
                out
            }
        }
    }
}

struct LogWriter(Log);

impl Writer for LogWriter {
    fn write_line(&self, line: &str) {
        self.0.lock().expect("log lock").push(line.to_string());
    }
}

struct FixedClock(AtomicU64);

impl commander_core::Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

fn default_catalog() -> TestLocalizer {
    TestLocalizer::new(&[
        ("commander.ui.usage", "Usage: {name} <command> [options]"),
        ("commander.ui.commands", "Commands:"),
        ("commander.ui.options", "Options:"),
        ("commander.ui.hint.available_options", "Run `{command} --help` for the available options."),
        ("commander.ui.hint.did_you_mean", "did you mean {suggest}?"),
        ("commander::unknown_command", "unknown command: {cmd}"),
        ("commander::unknown_option", "unknown option: {option}"),
        ("commander::missing_option_value", "option {option} requires a value"),
        ("commander::missing_required_option", "missing required option: --{option}"),
        ("commander::option_not_in_enum", "value {value} is not allowed for --{option}"),
        ("commander::option_coerce_failed", "value {value} is invalid for --{option}"),
        ("cli.cmd.check", "Check a document"),
        ("cli.cmd.run", "Run the pipeline"),
        ("cli.opt.path", "Input path"),
        ("cli.opt.tag", "Repeatable tag"),
        ("cli.opt.level", "Log level"),
    ])
}

fn ports_with(localizer: TestLocalizer) -> (Ports, Log) {
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let ports = Ports::new(
        Arc::new(FixedClock(AtomicU64::new(1_000))),
        Arc::new(LogWriter(Arc::clone(&log))),
        Arc::new(localizer),
        "en-US",
    );
    (ports, log)
}

fn ports() -> (Ports, Log) {
    ports_with(default_catalog())
}

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn lines(log: &Log) -> Vec<String> {
    log.lock().expect("log lock").clone()
}

fn envelope(stdout: Option<&String>) -> Value {
    let text = stdout.expect("JSON mode must produce an envelope");
    let value: Value = serde_json::from_str(text.trim()).expect("stdout payload is one JSON line");
    assert_eq!(value["schema"], "wae.cli-result/v0");
    value
}

fn ok_handler(data: Value) -> Handler {
    Arc::new(move |_ctx| HandlerOutcome::Ok(data.clone()))
}

// --- exit codes and envelopes ------------------------------------------------

#[test]
fn success_envelope_is_exactly_one_json_line() {
    let cli = Cli::new("demo").command(
        Command::new("run").handler(Arc::new(|ctx| {
            ctx.emit_result(json!({"a": 1}), None);
            ctx.emit_result(json!({"a": 2}), None);
            HandlerOutcome::Ok(json!({"ignored": true}))
        })),
    );
    let (ports, _log) = ports();
    let env = Environment::bare(false);
    let outcome = cli.run(&argv(&["run", "--json"]), &env, &ports, None);
    assert_eq!(outcome.exit, ExitCode::Ok);
    let text = outcome.stdout.expect("envelope present");
    let value: Value = serde_json::from_str(text.trim()).expect("single JSON line");
    assert_eq!(value["ok"], true);
    assert_eq!(value["data"], json!({"a": 1}));
    assert_eq!(text.trim().lines().count(), 1, "exactly one envelope value");
}

#[test]
fn handler_error_maps_to_frozen_exit_and_envelope() {
    let cli = Cli::new("demo").command(Command::new("boom").handler(Arc::new(|_ctx| {
        HandlerOutcome::Err(CommanderError::from_id(error_id::ACTION_FAILED))
    })));
    let (ports, _log) = ports();
    let outcome =
        cli.run(&argv(&["boom", "--json"]), &Environment::bare(false), &ports, None);
    assert_eq!(outcome.exit, ExitCode::Failure);
    let value = envelope(outcome.stdout.as_ref());
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "commander::action_failed");
    assert_eq!(
        value["error"]["message"], "commander::action_failed",
        "action_failed reports the stable code only"
    );
}

#[test]
fn failure_message_localizes_or_falls_back_to_code() {
    let cli = Cli::new("demo").command(Command::new("bad").handler(Arc::new(|_ctx| {
        HandlerOutcome::Err(
            CommanderError::from_id("demo::broken")
                .with_arg("path", "/tmp/x".to_string()),
        )
    })));
    let (ports, _log) = ports();
    let outcome = cli.run(&argv(&["bad", "--json"]), &Environment::bare(false), &ports, None);
    assert_eq!(outcome.exit, ExitCode::Failure);
    let value = envelope(outcome.stdout.as_ref());
    assert_eq!(value["error"]["code"], "demo::broken");
    assert_eq!(value["error"]["message"], "demo::broken", "unresolved key falls back to the code");
}

#[test]
fn action_required_outcome_yields_exit_five_with_descriptor() {
    let cli = Cli::new("demo").command(Command::new("issue").handler(Arc::new(|ctx| {
        ctx.action_required(ActionRequiredDescriptor {
            action_id: "dns-txt".to_string(),
            status_ref: Some("order-7".to_string()),
            expires_at: None,
            next_action: Some(NextAction {
                command: "demo".to_string(),
                argv: argv(&["issue", "--resume"]),
            }),
        })
    })));
    let (ports, _log) = ports();
    let outcome = cli.run(&argv(&["issue", "--json"]), &Environment::bare(false), &ports, None);
    assert_eq!(outcome.exit, ExitCode::ActionRequired);
    let value = envelope(outcome.stdout.as_ref());
    assert_eq!(value["error"]["code"], "commander::action_required");
    assert_eq!(value["actionRequired"]["actionId"], "dns-txt");
    assert_eq!(value["actionRequired"]["statusRef"], "order-7");
    assert!(
        value["actionRequired"].get("expiresAt").is_none(),
        "unset descriptor fields are omitted"
    );
    assert_eq!(value["actionRequired"]["nextAction"]["argv"][0], "issue");
}

#[test]
fn emit_failure_writes_stderr_line_in_human_mode() {
    let cli = Cli::new("demo").command(Command::new("f").handler(Arc::new(|ctx| {
        ctx.emit_failure(&CommanderError::from_id("demo::nope"), None);
        HandlerOutcome::Err(CommanderError::from_id("demo::nope"))
    })));
    let (ports, log) = ports();
    let outcome = cli.run(&argv(&["f"]), &Environment::bare(false), &ports, None);
    assert_eq!(outcome.exit, ExitCode::Failure);
    assert!(outcome.stdout.is_none(), "human mode writes no envelope");
    let captured = lines(&log);
    assert!(captured.iter().any(|l| l == "demo::nope: demo::nope"), "{captured:?}");
}

// --- CI rule and eligibility -------------------------------------------------

#[test]
fn ci_rule_matches_frozen_semantics() {
    for (value, expected) in [
        (None, false),
        (Some(""), false),
        (Some("false"), false),
        (Some("0"), false),
        (Some("1"), true),
        (Some("true"), true),
        (Some("GitHub_Actions"), true),
    ] {
        let mut env_map = BTreeMap::new();
        if let Some(value) = value {
            env_map.insert("CI".to_string(), value.to_string());
        }
        let env = Environment { tty: true, env: env_map };
        assert_eq!(env.ci(), expected, "CI={value:?}");
    }
}

fn interactive_snapshot_handler() -> Handler {
    Arc::new(|ctx: &mut commander_core::CommandContext<'_>| {
        HandlerOutcome::Ok(json!({
            "interactive": ctx.is_interactive(),
            "locale": ctx.locale,
        }))
    })
}

fn run_interactive(cli_argv: &[&str], tty: bool, ci: Option<&str>) -> Value {
    let cli = Cli::new("demo").command(Command::new("probe").handler(interactive_snapshot_handler()));
    let (ports, _log) = ports();
    let mut env_map = BTreeMap::new();
    if let Some(ci) = ci {
        env_map.insert("CI".to_string(), ci.to_string());
    }
    let env = Environment { tty, env: env_map };
    let outcome = cli.run(&argv(cli_argv), &env, &ports, None);
    assert_eq!(outcome.exit, ExitCode::Ok);
    let text = outcome.stdout.expect("probe emits a payload");
    let value: Value = serde_json::from_str(text.trim()).expect("payload is JSON");
    if value.get("schema").is_some() { value["data"].clone() } else { value }
}

#[test]
fn eligibility_is_tty_and_not_json_and_not_ci_and_not_no_interactive() {
    assert_eq!(run_interactive(&["probe"], true, None)["interactive"], json!(true));
    assert_eq!(run_interactive(&["probe", "--json"], true, None)["interactive"], json!(false));
    assert_eq!(run_interactive(&["probe"], false, None)["interactive"], json!(false));
    assert_eq!(
        run_interactive(&["probe"], true, Some("1"))["interactive"],
        json!(false),
        "CI truthy disables interactivity"
    );
    assert_eq!(
        run_interactive(&["probe"], true, Some("false"))["interactive"],
        json!(true),
        "CI=false keeps the tty snapshot eligible"
    );
    assert_eq!(
        run_interactive(&["probe", "--no-interactive"], true, None)["interactive"],
        json!(false)
    );
}

#[test]
fn locale_is_carried_into_the_context() {
    let cli = Cli::new("demo").command(Command::new("probe").handler(interactive_snapshot_handler()));
    let (ports, _log) = ports();
    let outcome = cli.run(&argv(&["probe", "--json"]), &Environment::bare(false), &ports, None);
    let value = envelope(outcome.stdout.as_ref());
    assert_eq!(value["data"]["locale"], "en-US");
}

// --- parsing -----------------------------------------------------------------

fn parse_probe_cli() -> Cli {
    Cli::new("demo").command(
        Command::new("parse")
            .option(OptionSpec::flag("verbose").short('v').help("cli.opt.tag"))
            .option(OptionSpec::required_value("out"))
            .option(OptionSpec::optional_value("color"))
            .option(OptionSpec::required_value("tag").repeatable())
            .option(OptionSpec::required_value("level").enum_values(["info", "warn"]))
            .option(OptionSpec::required_value("count").coerce(commander_core::Coerce::Int))
            .option(OptionSpec::required_value("mode").default_value("auto"))
            .option(OptionSpec::required_value("must").required())
            .with_passthrough()
            .handler(Arc::new(|ctx: &mut commander_core::CommandContext<'_>| {
                let options = ctx.options();
                HandlerOutcome::Ok(json!({
                    "verbose": options.bool_value("verbose").unwrap_or(false),
                    "out": options.str_value("out"),
                    "color": options.get("color"),
                    "tags": options.string_list("tag"),
                    "level": options.str_value("level"),
                    "count": options.str_value("count"),
                    "mode": options.str_value("mode"),
                    "must": options.str_value("must"),
                    "positionals": options.positionals(),
                }))
            })),
    )
}

fn parse_json(args: &[&str]) -> (Value, ExitCode) {
    let cli = parse_probe_cli();
    let (ports, _log) = ports();
    let mut full = vec!["parse", "--must", "m", "--json"];
    full.extend_from_slice(args);
    let outcome = cli.run(&argv(&full), &Environment::bare(false), &ports, None);
    let value = envelope(outcome.stdout.as_ref());
    (value, outcome.exit)
}

#[test]
fn parses_flags_attached_values_and_repeatables() {
    let (value, exit) = parse_json(&[
        "-v",
        "--out=a.txt",
        "--color",
        "--tag",
        "x",
        "--tag",
        "y",
        "--",
        "pos1",
        "-not-an-option",
    ]);
    assert_eq!(exit, ExitCode::Ok);
    let data = &value["data"];
    assert_eq!(data["verbose"], json!(true));
    assert_eq!(data["out"], "a.txt");
    assert_eq!(data["color"], json!(true), "bare optionalValue records true");
    assert_eq!(data["tags"], json!(["x", "y"]));
    assert_eq!(data["mode"], "auto", "default applies when absent");
    assert_eq!(data["positionals"], json!(["pos1", "-not-an-option"]), "passthrough tail is positional");
}

#[test]
fn bare_dash_is_a_value_not_an_option_marker() {
    let (value, exit) = parse_json(&["--out", "-"]);
    assert_eq!(exit, ExitCode::Ok);
    assert_eq!(value["data"]["out"], "-");
}

#[test]
fn missing_value_fails_with_usage_exit() {
    let (value, exit) = parse_json(&["--out"]);
    assert_eq!(exit, ExitCode::Usage);
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "commander::missing_option_value");
    assert_eq!(value["error"]["message"], "option --out requires a value");
}

#[test]
fn unknown_option_fails_with_usage_exit() {
    let (value, exit) = parse_json(&["--nope"]);
    assert_eq!(exit, ExitCode::Usage);
    assert_eq!(value["error"]["code"], "commander::unknown_option");
    assert_eq!(value["error"]["message"], "unknown option: --nope");
}

#[test]
fn missing_required_option_fails() {
    let cli = Cli::new("demo").command(
        Command::new("need")
            .option(OptionSpec::required_value("file").required())
            .handler(ok_handler(json!({}))),
    );
    let (ports, _log) = ports();
    let outcome = cli.run(&argv(&["need", "--json"]), &Environment::bare(false), &ports, None);
    assert_eq!(outcome.exit, ExitCode::Usage);
    let value = envelope(outcome.stdout.as_ref());
    assert_eq!(value["error"]["code"], "commander::missing_required_option");
    assert_eq!(value["error"]["message"], "missing required option: --file");
}

#[test]
fn enum_rejects_only_provided_values() {
    let cli = Cli::new("demo").command(
        Command::new("lvl")
            .option(OptionSpec::required_value("level").enum_values(["info", "warn"]).default_value("info"))
            .handler(ok_handler(json!({}))),
    );
    let (ports, _log) = ports();
    let outcome = cli.run(&argv(&["lvl", "--json"]), &Environment::bare(false), &ports, None);
    assert_eq!(outcome.exit, ExitCode::Ok, "default is not re-validated at run time");
    let outcome =
        cli.run(&argv(&["lvl", "--level", "trace", "--json"]), &Environment::bare(false), &ports, None);
    assert_eq!(outcome.exit, ExitCode::Usage);
    let value = envelope(outcome.stdout.as_ref());
    assert_eq!(value["error"]["code"], "commander::option_not_in_enum");
    assert_eq!(value["error"]["message"], "value trace is not allowed for --level");
}

#[test]
fn coerce_rejects_bad_provided_values() {
    let (value, exit) = parse_json(&["--count", "abc"]);
    assert_eq!(exit, ExitCode::Usage);
    assert_eq!(value["error"]["code"], "commander::option_coerce_failed");
}

#[test]
fn global_options_merge_without_overriding_command_values() {
    let cli = Cli::new("demo")
        .global_option(OptionSpec::required_value("profile").default_value("dev"))
        .global_option(OptionSpec::required_value("tag"))
        .command(
            Command::new("g")
                .option(OptionSpec::required_value("tag"))
                .handler(Arc::new(|ctx: &mut commander_core::CommandContext<'_>| {
                    HandlerOutcome::Ok(json!({
                        "profile": ctx.options().str_value("profile"),
                        "tag": ctx.options().str_value("tag"),
                    }))
                })),
        );
    let (ports, _log) = ports();
    let outcome = cli.run(
        &argv(&["--profile", "prod", "g", "--tag", "cmd", "--json"]),
        &Environment::bare(false),
        &ports,
        None,
    );
    let value = envelope(outcome.stdout.as_ref());
    assert_eq!(value["data"]["profile"], "prod");
    assert_eq!(value["data"]["tag"], "cmd");
    let outcome = cli.run(&argv(&["g", "--json"]), &Environment::bare(false), &ports, None);
    let value = envelope(outcome.stdout.as_ref());
    assert!(
        value["data"]["profile"].is_null(),
        "peel never applies global defaults, matching the TypeScript engine"
    );
    assert!(value["data"]["tag"].is_null());
}

// --- routing -----------------------------------------------------------------

#[test]
fn empty_argv_prints_root_help_with_exit_zero() {
    let cli = Cli::new("demo")
        .command(Command::new("check").help("cli.cmd.check").handler(ok_handler(json!({}))))
        .command(Command::new("run").help("cli.cmd.run").handler(ok_handler(json!({}))));
    let (ports, log) = ports();
    let outcome = cli.run(&argv(&[]), &Environment::bare(false), &ports, None);
    assert_eq!(outcome.exit, ExitCode::Ok);
    let text = outcome.stdout.expect("help goes to stdout payload in human mode");
    assert!(text.contains("Usage: demo"), "{text}");
    assert!(text.contains("check"), "{text}");
    assert!(text.contains("Run the pipeline"), "{text}");
    assert!(lines(&log).is_empty(), "human help never touches stderr");
}

#[test]
fn json_mode_help_goes_to_stderr_and_leaves_stdout_empty() {
    let cli = Cli::new("demo").command(Command::new("check").handler(ok_handler(json!({}))));
    let (ports, log) = ports();
    let outcome = cli.run(&argv(&["--help", "--json"]), &Environment::bare(false), &ports, None);
    assert_eq!(outcome.exit, ExitCode::Ok);
    assert!(outcome.stdout.is_none(), "help never enters the envelope channel");
    let captured = lines(&log);
    assert!(captured.iter().any(|l| l.starts_with("Usage: demo")), "{captured:?}");
}

#[test]
fn help_subcommand_routes_to_command_help() {
    let cli = Cli::new("demo").command(
        Command::new("document")
            .subcommand(Command::new("check").help("cli.cmd.check").handler(ok_handler(json!({})))),
    );
    let (ports, _log) = ports();
    let outcome = cli.run(&argv(&["help", "document", "check"]), &Environment::bare(false), &ports, None);
    assert_eq!(outcome.exit, ExitCode::Ok);
    let text = outcome.stdout.expect("help payload");
    assert!(text.contains("demo document check"), "{text}");
    assert!(text.contains("Check a document"), "{text}");
}

#[test]
fn deepest_command_binds_before_help_marker() {
    let cli = Cli::new("demo").command(
        Command::new("document")
            .subcommand(Command::new("check").help("cli.cmd.check").handler(ok_handler(json!({})))),
    );
    let (ports, _log) = ports();
    let outcome = cli.run(&argv(&["document", "check", "--help"]), &Environment::bare(false), &ports, None);
    let text = outcome.stdout.expect("help payload");
    assert!(text.contains("demo document check"), "help binds to `check`, not `document`: {text}");
}

#[test]
fn unknown_root_command_suggests_closest_name() {
    let cli = Cli::new("demo")
        .command(Command::new("check").handler(ok_handler(json!({}))))
        .command(Command::new("run").handler(ok_handler(json!({}))));
    let (ports, log) = ports();
    let outcome = cli.run(&argv(&["che", "--json"]), &Environment::bare(false), &ports, None);
    assert_eq!(outcome.exit, ExitCode::Usage);
    let value = envelope(outcome.stdout.as_ref());
    assert_eq!(value["error"]["code"], "commander::unknown_command");
    let captured = lines(&log);
    assert!(captured.iter().any(|l| l == "unknown command: che"), "{captured:?}");
    assert!(captured.iter().any(|l| l == "did you mean check?"), "{captured:?}");
    assert!(
        captured.iter().any(|l| l.starts_with("Run `demo --help`")),
        "hint line names the CLI root: {captured:?}"
    );
}

#[test]
fn group_without_handler_prints_help() {
    let cli = Cli::new("demo").command(
        Command::new("document").subcommand(Command::new("check").handler(ok_handler(json!({})))),
    );
    let (ports, _log) = ports();
    let outcome = cli.run(&argv(&["document"]), &Environment::bare(false), &ports, None);
    assert_eq!(outcome.exit, ExitCode::Ok);
    assert!(outcome.stdout.expect("group help").contains("demo document"));
    let outcome = cli.run(&argv(&["document", "zzz", "--json"]), &Environment::bare(false), &ports, None);
    assert_eq!(outcome.exit, ExitCode::Usage);
    let value = envelope(outcome.stdout.as_ref());
    assert_eq!(value["error"]["code"], "commander::unknown_command");
    assert_eq!(value["error"]["message"], "unknown command: document zzz");
}

#[test]
fn command_without_handler_and_extra_tokens_fails_unknown_command() {
    let cli = Cli::new("demo").command(Command::new("plain"));
    let (ports, _log) = ports();
    let outcome = cli.run(&argv(&["plain", "extra", "--json"]), &Environment::bare(false), &ports, None);
    assert_eq!(outcome.exit, ExitCode::Usage);
    assert_eq!(envelope(outcome.stdout.as_ref())["error"]["code"], "commander::unknown_command");
}

// --- tree validation ----------------------------------------------------------

fn spec(policy: commander_core::PromptPolicy, keys: &[(&str, &str)], inputs: &[&str]) -> InteractiveSpec {
    InteractiveSpec {
        policy,
        prompt_keys: keys.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        non_interactive_inputs: inputs.iter().map(|s| s.to_string()).collect(),
        continuation: None,
    }
}

fn run_validate(cli: &Cli) -> (ExitCode, Log) {
    let (ports, log) = ports();
    let outcome = cli.run(&argv(&[]), &Environment::bare(false), &ports, None);
    (outcome.exit, log)
}

#[test]
fn required_policy_needs_prompt_keys_and_machine_inputs() {
    let cli = Cli::new("demo").command(
        Command::new("x")
            .option(OptionSpec::required_value("file"))
            .interactive(spec(commander_core::PromptPolicy::Required, &[], &["file"]))
            .handler(ok_handler(json!({}))),
    );
    let (exit, log) = run_validate(&cli);
    assert_eq!(exit, ExitCode::Failure, "config error exits 1");
    assert!(lines(&log).iter().any(|l| l.contains("commander::interactive_spec_invalid")));

    let cli = Cli::new("demo").command(
        Command::new("x")
            .option(OptionSpec::required_value("file"))
            .interactive(spec(commander_core::PromptPolicy::Required, &[("file", "cli.prompt.file")], &[]))
            .handler(ok_handler(json!({}))),
    );
    let (exit, _log) = run_validate(&cli);
    assert_eq!(exit, ExitCode::Failure, "required policy needs a machine input path");

    let cli = Cli::new("demo").command(
        Command::new("x")
            .option(OptionSpec::required_value("file"))
            .interactive(spec(
                commander_core::PromptPolicy::Required,
                &[("file", "cli.prompt.file")],
                &["file"],
            ))
            .handler(ok_handler(json!({}))),
    );
    let (exit, _log) = run_validate(&cli);
    assert_eq!(exit, ExitCode::Ok);
}

#[test]
fn input_refs_must_resolve() {
    let cli = Cli::new("demo").command(
        Command::new("x")
            .option(OptionSpec::required_value("file"))
            .interactive(spec(commander_core::PromptPolicy::Optional, &[], &["ghost"]))
            .handler(ok_handler(json!({}))),
    );
    let (exit, _log) = run_validate(&cli);
    assert_eq!(exit, ExitCode::Failure, "unknown bare reference");

    let cli = Cli::new("demo").command(
        Command::new("x")
            .option(OptionSpec::flag("force"))
            .interactive(spec(commander_core::PromptPolicy::Optional, &[], &["flag:force", "config:root"]))
            .handler(ok_handler(json!({}))),
    );
    let (exit, _log) = run_validate(&cli);
    assert_eq!(exit, ExitCode::Ok, "flag: and config: prefixes resolve");

    let cli = Cli::new("demo").command(
        Command::new("x")
            .interactive(spec(commander_core::PromptPolicy::Optional, &[], &["flag:missing"]))
            .handler(ok_handler(json!({}))),
    );
    let (exit, _log) = run_validate(&cli);
    assert_eq!(exit, ExitCode::Failure);
}

#[test]
fn never_policy_rejects_declared_inputs() {
    let cli = Cli::new("demo").command(
        Command::new("x")
            .option(OptionSpec::required_value("file"))
            .interactive(spec(commander_core::PromptPolicy::Never, &[], &["file"]))
            .handler(ok_handler(json!({}))),
    );
    let (exit, _log) = run_validate(&cli);
    assert_eq!(exit, ExitCode::Failure);
}

#[test]
fn duplicate_names_are_rejected() {
    let cli = Cli::new("demo")
        .command(Command::new("run").handler(ok_handler(json!({}))))
        .command(Command::new("run").handler(ok_handler(json!({}))));
    let (exit, _log) = run_validate(&cli);
    assert_eq!(exit, ExitCode::Usage, "parse_failed exits 2");

    let cli = Cli::new("demo").command(
        Command::new("run")
            .option(OptionSpec::flag("x"))
            .option(OptionSpec::flag("x"))
            .handler(ok_handler(json!({}))),
    );
    let (exit, _log) = run_validate(&cli);
    assert_eq!(exit, ExitCode::Usage);
}

// --- context helpers ----------------------------------------------------------

#[test]
fn ctx_fail_warn_note_emit_diagnostics() {
    let cli = Cli::new("demo").command(Command::new("c").handler(Arc::new(|ctx| {
        ctx.warn("cli.warn", BTreeMap::new());
        ctx.note("cli.note", BTreeMap::new());
        HandlerOutcome::Err(ctx.fail("cli.boom", BTreeMap::new()))
    })));
    let (ports, log) = ports();
    let outcome = cli.run(&argv(&["c", "--json"]), &Environment::bare(false), &ports, None);
    assert_eq!(outcome.exit, ExitCode::Failure);
    let captured = lines(&log);
    assert!(captured.iter().any(|l| l == "cli.warn"), "{captured:?}");
    assert!(captured.iter().any(|l| l == "cli.note"), "{captured:?}");
    assert!(captured.iter().any(|l| l == "cli.boom"), "{captured:?}");
}

#[test]
fn registered_diagnostic_handler_replaces_default_rendering() {
    let seen: Log = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let cli = Cli::new("demo")
        .on_diagnostic(Arc::new(move |diag, _ctx| {
            sink.lock().expect("sink").push(format!("{}:{}", diag.severity as u8, diag.code));
        }))
        .command(Command::new("ghost"));
    let (ports, log) = ports();
    let outcome = cli.run(&argv(&["ghost", "extra", "--json"]), &Environment::bare(false), &ports, None);
    assert_eq!(outcome.exit, ExitCode::Usage);
    assert_eq!(lines(&seen), vec![format!("{}:commander::unknown_command", Severity::Error as u8)]);
    assert_eq!(
        lines(&log),
        vec!["Run `demo --help` for the available options.".to_string()],
        "the handler replaces only the default message row, hints stay framework UI"
    );
}

#[test]
fn claim_stdin_blocks_prompt_engines() {
    // The core owns the claim flag; the prompt crate's engine reads it.
    // Here we assert the context surface round-trips through the shared hook.
    let cli = Cli::new("demo").command(Command::new("p").handler(Arc::new(|ctx| {
        ctx.claim_stdin();
        HandlerOutcome::Ok(json!({"claimed": ctx.stdin_claimed()}))
    })));
    let (ports, _log) = ports();
    let outcome = cli.run(&argv(&["p", "--json"]), &Environment::bare(false), &ports, None);
    assert_eq!(envelope(outcome.stdout.as_ref())["data"]["claimed"], json!(true));
}

// --- prompt seam --------------------------------------------------------------

struct AlwaysBusy;

impl PromptApi for AlwaysBusy {
    fn confirm(&self, _req: &ConfirmRequest) -> Result<bool, CommanderError> {
        Err(CommanderError::from_id(error_id::INTERACTION_BUSY))
    }
    fn input(&self, _req: &commander_core::InputRequest) -> Result<String, CommanderError> {
        unreachable!()
    }
    fn select(&self, _req: &commander_core::SelectRequest) -> Result<String, CommanderError> {
        unreachable!()
    }
    fn password(&self, _req: &commander_core::PasswordRequest) -> Result<commander_core::SecretString, CommanderError> {
        unreachable!()
    }
    fn await_action(&self, _req: &commander_core::AwaitActionRequest) -> Result<commander_core::AwaitActionOutcome, CommanderError> {
        unreachable!()
    }
}

struct FixedFactory;

impl PromptFactory for FixedFactory {
    fn create(&self, _hooks: PromptHooks) -> Arc<dyn PromptApi> {
        Arc::new(AlwaysBusy)
    }
}

#[test]
fn factory_wires_the_prompt_api_into_the_context() {
    let cli = Cli::new("demo").command(Command::new("p").handler(Arc::new(|ctx| match ctx
        .confirm(&ConfirmRequest::new("cli.prompt.go"))
    {
        Ok(value) => HandlerOutcome::Ok(json!({"confirmed": value})),
        Err(err) => HandlerOutcome::Err(err),
    })));
    let (ports, _log) = ports();
    let factory: Arc<dyn PromptFactory> = Arc::new(FixedFactory);
    let outcome =
        cli.run(&argv(&["p", "--json"]), &Environment::bare(true), &ports, Some(&factory));
    assert_eq!(outcome.exit, ExitCode::Capability);
    assert_eq!(envelope(outcome.stdout.as_ref())["error"]["code"], "commander::interaction_busy");
}

#[test]
fn without_factory_interactive_calls_fail_as_unavailable() {
    let cli = Cli::new("demo").command(Command::new("p").handler(Arc::new(|ctx| match ctx
        .confirm(&ConfirmRequest::new("cli.prompt.go"))
    {
        Ok(value) => HandlerOutcome::Ok(json!({"confirmed": value})),
        Err(err) => HandlerOutcome::Err(err),
    })));
    let (ports, _log) = ports();
    let outcome = cli.run(&argv(&["p", "--json"]), &Environment::bare(true), &ports, None);
    assert_eq!(outcome.exit, ExitCode::Capability);
    assert_eq!(envelope(outcome.stdout.as_ref())["error"]["code"], "commander::interactive_unavailable");
}

#[test]
fn null_engine_await_action_outside_interactivity_returns_descriptor() {
    use commander_core::{AwaitActionOutcome, AwaitActionRequest};
    let cli = Cli::new("demo").command(Command::new("a").handler(Arc::new(|ctx| {
        match ctx.await_action(&AwaitActionRequest::new("dns-txt", "cli.prompt.dns")) {
            Ok(AwaitActionOutcome::ActionRequired(descriptor)) => ctx.action_required(descriptor),
            Ok(_) => HandlerOutcome::Ok(json!({"unexpected": true})),
            Err(err) => HandlerOutcome::Err(err),
        }
    })));
    let (ports, _log) = ports();
    let outcome = cli.run(&argv(&["a", "--json"]), &Environment::bare(false), &ports, None);
    assert_eq!(outcome.exit, ExitCode::ActionRequired);
    let value = envelope(outcome.stdout.as_ref());
    assert_eq!(value["error"]["code"], "commander::action_required");
    assert_eq!(value["actionRequired"]["actionId"], "dns-txt");
}

#[test]
fn value_decl_and_option_builders_round_trip() {
    let option = OptionSpec::required_value("out")
        .short('o')
        .repeatable()
        .enum_values(["a", "b"])
        .required()
        .default_value("a")
        .coerce(commander_core::Coerce::None)
        .hidden()
        .group("io")
        .help("cli.opt.out");
    assert_eq!(option.value, ValueDecl::Required);
    assert!(option.repeatable && option.required && option.hidden);
    assert_eq!(option.enum_values, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(option.group.as_deref(), Some("io"));
    assert_eq!(option.help_key.as_deref(), Some("cli.opt.out"));
}
