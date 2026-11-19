//! `commander.conformance/v0` fixtures driver (Rust engine side).
//!
//! The fixture directory comes from `COMMANDER_CONFORMANCE_FIXTURES`. When
//! the variable is unset the test prints a skip note and passes: this crate
//! never reverse-depends on a host repository and never hardcodes a path
//! outside it.
//!
//! Mapping rulings against the TypeScript driver:
//! - Both `inproc` and `pipe` fixtures run here as spawned subprocesses
//!   with piped streams; `argv` is passed verbatim (no interpreter prefix).
//! - The in-process driver's injected `isTty` surface is reproduced by
//!   `COMMANDER_CONFORMANCE_FORCE_TTY=1` in the sealed child environment,
//!   so `tty` fixtures exercise the interactive engine over pipes.
//! - `rawModeRestored` observes the in-process fake stream's recorded
//!   raw-mode calls. A spawned pipe has no such recorder, so the field is
//!   not observable and is skipped by default; every other asserted field
//!   (exit code, envelope subset, contains / not-contains, secret absence,
//!   single-JSON stdout) still runs and must pass.

use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

/// Knob that points the driver at the shared fixtures directory.
const FIXTURES_ENV: &str = "COMMANDER_CONFORMANCE_FIXTURES";
/// Sealed-child knob mirroring the in-process injected-TTY surface.
const FORCE_TTY_ENV: &str = "COMMANDER_CONFORMANCE_FORCE_TTY";
/// The Rust twin of the fixture CLI, built from this package.
const BIN: &str = env!("CARGO_BIN_EXE_commander-conformance-cli");
const FIXTURE_SCHEMA: &str = "commander.conformance/v0";
const RESULT_SCHEMA: &str = "wae.cli-result/v0";
/// Interrupt byte the driver feeds for `sigint` fixtures.
const ETX: &[u8] = &[0x03];
const WATCHDOG: Duration = Duration::from_secs(15);

fn string_field(obj: &Value, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_string)
}

fn string_list_field(obj: &Value, key: &str) -> Vec<String> {
    obj.get(key).and_then(Value::as_array).map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

fn env_field(obj: &Value, key: &str) -> BTreeMap<String, String> {
    obj.get(key)
        .and_then(Value::as_object)
        .map(|map| map.iter().filter_map(|(name, value)| value.as_str().map(|v| (name.clone(), v.to_string()))).collect())
        .unwrap_or_default()
}

fn read_stream(mut stream: impl Read) -> String {
    let mut buffer = Vec::new();
    let _ = stream.read_to_end(&mut buffer);
    String::from_utf8_lossy(&buffer).into_owned()
}

/// Mirrors the TypeScript `matchSubset`: objects match declared keys,
/// arrays match exactly, scalars compare by equality.
fn match_subset(expected: &Value, actual: &Value, path: &str, failures: &mut Vec<String>) {
    match expected {
        Value::Object(map) => {
            let Some(found) = actual.as_object()
            else {
                failures.push(format!("at `{path}`: expected an object, got `{actual}`"));
                return;
            };
            for (key, value) in map {
                match found.get(key) {
                    Some(nested) => match_subset(value, nested, &format!("{path}.{key}"), failures),
                    None => failures.push(format!("at `{path}`: missing key `{key}`")),
                }
            }
        }
        Value::Array(items) => {
            let Some(found) = actual.as_array()
            else {
                failures.push(format!("at `{path}`: expected an array, got `{actual}`"));
                return;
            };
            if found.len() != items.len() {
                failures.push(format!("at `{path}`: expected {} items, got {}", items.len(), found.len()));
                return;
            }
            for (index, (item, found_item)) in items.iter().zip(found).enumerate() {
                match_subset(item, found_item, &format!("{path}[{index}]"), failures);
            }
        }
        scalar => {
            if scalar != actual {
                failures.push(format!("at `{path}`: expected `{scalar}`, got `{actual}`"));
            }
        }
    }
}

fn assert_expectations(fixture: &Value, code: i32, stdout: &str, stderr: &str, failures: &mut Vec<String>) {
    let Some(expect) = fixture.get("expect")
    else {
        return;
    };
    if let Some(expected) = expect.get("exitCode").and_then(Value::as_i64)
        && code != i32::try_from(expected).unwrap_or(i32::MIN)
    {
        failures.push(format!("exitCode: expected {expected}, got {code}; stdout={stdout:?} stderr={stderr:?}"));
    }
    if let Some(envelope) = expect.get("envelope") {
        match serde_json::from_str::<Value>(stdout.trim()) {
            Ok(actual) => match_subset(envelope, &actual, "envelope", failures),
            Err(error) => failures.push(format!("envelope: stdout is not JSON ({error}); stdout={stdout:?}")),
        }
    }
    for needle in string_list_field(expect, "stderrContains") {
        if !stderr.contains(&needle) {
            failures.push(format!("stderrContains: missing {needle:?}; stderr={stderr:?}"));
        }
    }
    for needle in string_list_field(expect, "stdoutContains") {
        if !stdout.contains(&needle) {
            failures.push(format!("stdoutContains: missing {needle:?}; stdout={stdout:?}"));
        }
    }
    for needle in string_list_field(expect, "stdoutNotContains") {
        if stdout.contains(&needle) {
            failures.push(format!("stdoutNotContains: found {needle:?}; stdout={stdout:?}"));
        }
    }
    for needle in string_list_field(expect, "secretAbsent") {
        if stdout.contains(&needle) || stderr.contains(&needle) {
            failures.push(format!("secretAbsent: secret {needle:?} leaked; stdout={stdout:?} stderr={stderr:?}"));
        }
    }
    if expect.get("stdoutSingleJson").and_then(Value::as_bool) == Some(true) {
        match serde_json::from_str::<Value>(stdout.trim()) {
            Ok(Value::Object(object)) => {
                if object.get("schema").and_then(Value::as_str) != Some(RESULT_SCHEMA) {
                    failures.push(format!("stdoutSingleJson: schema is not {RESULT_SCHEMA:?}"));
                }
            }
            Ok(other) => failures.push(format!("stdoutSingleJson: not one JSON object: `{other}`")),
            Err(error) => {
                failures.push(format!("stdoutSingleJson: stdout is not exactly one JSON value ({error}); stdout={stdout:?}"));
            }
        }
    }
    // `rawModeRestored` is not observable over spawned pipes; see the
    // mapping rulings in the module header. All other fields are asserted.
    let _ = string_field(expect, "rawModeRestored");
}

fn run_fixture(path: &Path) -> (String, Vec<String>) {
    let raw = std::fs::read_to_string(path).unwrap_or_else(|error| panic!("fixture {}: {error}", path.display()));
    let fixture: Value = serde_json::from_str(&raw).unwrap_or_else(|error| panic!("fixture {}: invalid JSON ({error})", path.display()));
    let id = string_field(&fixture, "id").unwrap_or_default();
    if fixture.get("schema").and_then(Value::as_str) != Some(FIXTURE_SCHEMA) {
        panic!("fixture {id:?}: schema must be {FIXTURE_SCHEMA:?}");
    }
    if id.is_empty() {
        panic!("fixture {} must carry a non-empty id", path.display());
    }
    if !matches!(fixture.get("mode").and_then(Value::as_str), Some("inproc") | Some("pipe")) {
        panic!("fixture {id}: mode must be inproc or pipe");
    }
    let Some(argv) = fixture.get("argv").and_then(Value::as_array)
    else {
        panic!("fixture {id}: argv must be an array");
    };
    let argv: Vec<String> = argv.iter().filter_map(Value::as_str).map(str::to_string).collect();

    let stdin_chunks = string_list_field(&fixture, "stdin");
    let hold_stdin = fixture.get("holdStdin").and_then(Value::as_bool) == Some(true);
    let sigint = fixture.get("sigint").and_then(Value::as_bool) == Some(true);

    let mut command = Command::new(BIN);
    command
        .args(&argv)
        .env_remove("CI")
        .env_remove("LANG")
        .env_remove("LC_ALL")
        .env_remove("LOCALE")
        .env(FORCE_TTY_ENV, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in env_field(&fixture, "env") {
        command.env(name, value);
    }
    let mut child = command.spawn().unwrap_or_else(|error| panic!("fixture {id}: spawn failed ({error})"));

    let mut stdin = child.stdin.take();
    if let Some(handle) = stdin.as_mut() {
        for chunk in &stdin_chunks {
            handle.write_all(chunk.as_bytes()).expect("stdin write");
            handle.flush().expect("stdin flush");
        }
        if sigint {
            thread::sleep(Duration::from_millis(50));
            let _ = handle.write_all(ETX);
            let _ = handle.flush();
        }
    }
    if !hold_stdin {
        let _ = stdin.take();
    }

    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() > WATCHDOG {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().expect("wait after watchdog kill");
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("fixture {id}: try_wait failed ({error})"),
        }
    };
    let _ = stdin.take();
    let stdout = read_stream(child.stdout.take().expect("stdout piped"));
    let stderr = read_stream(child.stderr.take().expect("stderr piped"));
    let code = if timed_out { -1 } else { status.code().unwrap_or(-1) };

    let mut failures = Vec::new();
    if timed_out {
        failures.push(format!("watchdog: killed after {}s", WATCHDOG.as_secs()));
    }
    assert_expectations(&fixture, code, &stdout, &stderr, &mut failures);
    (id, failures)
}

#[test]
fn commander_conformance_fixtures() {
    let Some(directory) = std::env::var_os(FIXTURES_ENV)
    else {
        eprintln!(
            "skipping `commander.conformance/v0`: {FIXTURES_ENV} is not set; \
             point it at the shared fixtures directory to run the fixture CLI twin"
        );
        return;
    };
    let directory = PathBuf::from(directory);
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("fixtures directory {directory:?}: {error}"))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no fixtures under {directory:?}");

    let mut report = Vec::new();
    let mut failed = 0_usize;
    for path in &paths {
        let (id, failures) = run_fixture(path);
        if failures.is_empty() {
            report.push(format!("PASS {id}"));
        }
        else {
            failed += 1;
            for failure in failures {
                report.push(format!("FAIL {id}: {failure}"));
            }
        }
    }
    println!("{}", report.join("\n"));
    assert_eq!(failed, 0, "{failed} of {} fixtures failed (see printed report)", paths.len());
}
