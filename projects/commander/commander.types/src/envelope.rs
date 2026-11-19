use serde_json::{Value, json};

use crate::ActionRequiredDescriptor;

/// Schema id of the machine-readable CLI result envelope.
pub const RESULT_SCHEMA: &str = "wae.cli-result/v0";

/// Builds the success envelope for the given data value.
///
/// The execution layer emits this exactly once to stdout in JSON mode.
pub fn envelope_ok(data: Value) -> Value {
    json!({
        "schema": RESULT_SCHEMA,
        "ok": true,
        "data": data,
    })
}

/// Builds the failure envelope for a stable code and a safe summary.
///
/// `action_required` is attached only for the action-required outcome; the
/// descriptor is serialized with its camelCase contract field names.
pub fn envelope_error(code: &str, message: &str, action_required: Option<&ActionRequiredDescriptor>) -> Value {
    let mut envelope = json!({
        "schema": RESULT_SCHEMA,
        "ok": false,
        "error": {
            "code": code,
            "message": message,
        },
    });
    if let Some(descriptor) = action_required {
        let value = serde_json::to_value(descriptor).expect("action required descriptors are plain data and always serialize");
        envelope["actionRequired"] = value;
    }
    envelope
}
