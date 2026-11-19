use serde::{Deserialize, Serialize};

/// A structured command a later run can execute to continue the action.
///
/// This is a literal command and argv list, never a shell string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NextAction {
    /// The invocable command name.
    pub command: String,
    /// Literal argv tokens for the continuation run.
    pub argv: Vec<String>,
}

/// Descriptor for domain work that needs an external action before a later
/// run can continue it.
///
/// It carries references only: no tokens, no private keys, no secrets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionRequiredDescriptor {
    /// Stable action identity owned by the domain.
    pub action_id: String,
    /// Reference into the domain's persisted state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_ref: Option<String>,
    /// RFC 3339 expiry of the pending external action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Structured continuation the caller may run after the action.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_action: Option<NextAction>,
}
