//! Host-provided environment snapshot and the frozen CI rule.

use std::collections::BTreeMap;

/// Everything about the surrounding process that a run may consult.
///
/// The core never reads real environment variables or terminal state; the
/// host captures both and passes them in. `tty` is the pair probe result
/// (input stream attached to a terminal **and** the interactive output
/// channel attached to a terminal), mirroring the TypeScript twin which
/// checks `stdin` and `stderr`.
#[derive(Debug, Clone)]
pub struct Environment {
    /// Whether both the input stream and the interactive output channel are terminals.
    pub tty: bool,
    /// The captured environment block; keys are matched case-sensitively.
    pub env: BTreeMap<String, String>,
}

impl Environment {
    /// Builds a snapshot from the TTY pair probe and an environment block.
    pub fn new(tty: bool, env: impl IntoIterator<Item = (String, String)>) -> Self {
        Self { tty, env: env.into_iter().collect() }
    }

    /// Builds a snapshot with an empty environment block.
    pub fn bare(tty: bool) -> Self {
        Self { tty, env: BTreeMap::new() }
    }

    /// The frozen CI rule: `CI` unset, empty, `false`, or `0` means not CI;
    /// any other value means CI. No other `CI_*` marker is matched.
    pub fn ci(&self) -> bool {
        match self.env.get("CI") {
            None => false,
            Some(value) => !(value.is_empty() || value == "false" || value == "0"),
        }
    }
}
