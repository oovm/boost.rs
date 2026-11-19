use std::collections::BTreeMap;

use commander_types::{
    ActionRequiredDescriptor, CommanderError, ContinuationSpec, ExitCode, InteractiveSpec, NextAction, PromptPolicy, RESULT_SCHEMA,
    SecretString, envelope_error, envelope_ok, error_id,
};
use serde_json::json;

#[test]
fn exit_codes_are_frozen() {
    assert_eq!(ExitCode::Ok.as_i32(), 0);
    assert_eq!(ExitCode::Failure.as_i32(), 1);
    assert_eq!(ExitCode::Usage.as_i32(), 2);
    assert_eq!(ExitCode::Capability.as_i32(), 3);
    assert_eq!(ExitCode::Canceled.as_i32(), 4);
    assert_eq!(ExitCode::ActionRequired.as_i32(), 5);
}

#[test]
fn error_ids_map_to_frozen_exits() {
    assert_eq!(error_id::exit_for(error_id::UNKNOWN_COMMAND), ExitCode::Usage);
    assert_eq!(error_id::exit_for(error_id::OPTION_COERCE_FAILED), ExitCode::Usage);
    assert_eq!(error_id::exit_for(error_id::INTERACTIVE_UNAVAILABLE), ExitCode::Capability);
    assert_eq!(error_id::exit_for(error_id::PASSWORD_UNSUPPORTED), ExitCode::Capability);
    assert_eq!(error_id::exit_for(error_id::PROMPT_TIMEOUT), ExitCode::Canceled);
    assert_eq!(error_id::exit_for(error_id::ACTION_FAILED), ExitCode::Failure);
}

#[test]
fn command_error_displays_by_id_only() {
    let error =
        CommanderError::from_id(error_id::UNKNOWN_OPTION).with_arg("option", "--dns").with_arg("localeKey", "commander.error.unknownOption");
    assert_eq!(error.to_string(), "commander::unknown_option");
    assert_eq!(error.exit, ExitCode::Usage);
    assert_eq!(error.args.get("option").map(String::as_str), Some("--dns"));
}

#[test]
fn secret_never_prints_its_contents() {
    let secret = SecretString::new("hunter2");
    let rendered = format!("{secret:?}");
    assert_eq!(rendered, "SecretString(▮)");
    assert!(!rendered.contains("hunter2"));
    assert_eq!(secret.expose_secret(), "hunter2");
}

#[test]
fn success_envelope_matches_the_frozen_shape() {
    let envelope = envelope_ok(json!({ "path": "out/cert.pem" }));
    assert_eq!(
        envelope,
        json!({
            "schema": RESULT_SCHEMA,
            "ok": true,
            "data": { "path": "out/cert.pem" },
        })
    );
}

#[test]
fn failure_envelope_carries_action_required_only_when_given() {
    let descriptor = ActionRequiredDescriptor {
        action_id: "dns-txt-01".to_string(),
        status_ref: Some("state/order-7".to_string()),
        expires_at: Some("2026-11-20T00:00:00Z".to_string()),
        next_action: Some(NextAction { command: "demo resume".to_string(), argv: vec!["--action-id".to_string(), "dns-txt-01".to_string()] }),
    };
    let with_action = envelope_error(error_id::INTERACTIVE_UNAVAILABLE, "prompting is unavailable", Some(&descriptor));
    assert_eq!(with_action["schema"], json!(RESULT_SCHEMA));
    assert_eq!(with_action["ok"], json!(false));
    assert_eq!(with_action["error"]["code"], json!("commander::interactive_unavailable"));
    assert_eq!(with_action["actionRequired"]["actionId"], json!("dns-txt-01"));
    assert_eq!(with_action["actionRequired"]["statusRef"], json!("state/order-7"));
    assert_eq!(with_action["actionRequired"]["expiresAt"], json!("2026-11-20T00:00:00Z"));
    assert_eq!(with_action["actionRequired"]["nextAction"]["argv"], json!(["--action-id", "dns-txt-01"]));

    let plain = envelope_error(error_id::ACTION_FAILED, "domain step failed", None);
    assert!(plain.get("actionRequired").is_none());
}

#[test]
fn interactive_spec_round_trips_with_camel_case_keys() {
    let mut prompt_keys = BTreeMap::new();
    prompt_keys.insert("confirm".to_string(), "cli.prompt.issue.confirm".to_string());
    let spec = InteractiveSpec {
        policy: PromptPolicy::Required,
        prompt_keys,
        non_interactive_inputs: vec!["--yes".to_string()],
        continuation: Some(ContinuationSpec { resume_inputs: vec!["--action-id".to_string()] }),
    };
    let value = serde_json::to_value(&spec).expect("plain data serializes");
    assert_eq!(value["policy"], json!("required"));
    assert_eq!(value["promptKeys"]["confirm"], json!("cli.prompt.issue.confirm"));
    assert_eq!(value["nonInteractiveInputs"], json!(["--yes"]));
    assert_eq!(value["continuation"]["resumeInputs"], json!(["--action-id"]));

    let back: InteractiveSpec = serde_json::from_value(value).expect("round trip");
    assert_eq!(back, spec);
}
