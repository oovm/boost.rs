use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// When a declared value may be collected through an interactive prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PromptPolicy {
    /// The prompt may fill the value when the context is interactive.
    Optional,
    /// The value is mandatory; a non-interactive context must fail with
    /// `commander::interactive_unavailable` rather than guess.
    Required,
    /// The value is never collected through prompting.
    Never,
}

/// Cross-run continuation declared alongside an interactive spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContinuationSpec {
    /// Option names that a later run must accept to resume this step.
    pub resume_inputs: Vec<String>,
}

/// Machine-path declaration attached to an option or command.
///
/// A single locale key is never a sufficient machine path: the spec carries
/// the policy, the prompt keys, the accepted non-interactive inputs, and the
/// continuation when the step can span runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InteractiveSpec {
    /// Prompting policy for the declared value.
    pub policy: PromptPolicy,
    /// Locale keys used by the prompts of this step.
    pub prompt_keys: BTreeMap<String, String>,
    /// Inputs that satisfy this step without prompting.
    pub non_interactive_inputs: Vec<String>,
    /// Continuation declaration when the step can resume in a later run.
    pub continuation: Option<ContinuationSpec>,
}
