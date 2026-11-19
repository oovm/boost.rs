use core::fmt;

/// Frozen process exit codes shared by every commander host.
///
/// Hosts map the outcome of a run onto exactly one of these values. The
/// library itself never terminates the process; only the entry binary does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExitCode {
    /// 0 — success.
    Ok = 0,
    /// 1 — execution or protocol failure, including domain operation timeouts.
    Failure = 1,
    /// 2 — parsing or validation failure, including missing explicit machine input.
    Usage = 2,
    /// 3 — missing capability, such as an interactive prompt in a non-interactive context.
    Capability = 3,
    /// 4 — canceled after a prompt started: EOF, Ctrl+C, explicit abort, or timeout.
    Canceled = 4,
    /// 5 — the domain prepared work that needs an external action before a later run.
    ActionRequired = 5,
}

impl ExitCode {
    /// The numeric process exit code.
    pub const fn as_i32(self) -> i32 {
        self as i32
    }
}

impl fmt::Display for ExitCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_i32())
    }
}
