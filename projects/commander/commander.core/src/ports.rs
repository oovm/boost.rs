//! Injected host ports. The core never reads real process state itself.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Monotonic-enough wall clock in milliseconds since the Unix epoch.
///
/// Prompt deadlines are absolute values on this clock; validation retries
/// must not reset them.
pub trait Clock: Send + Sync {
    /// The current time in milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
}

/// A [`Clock`] backed by the system time.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// A line sink for human diagnostics and prompt output.
///
/// Hosts wire this to standard error. The library never writes standard
/// output through any port, keeping the JSON envelope the sole stdout
/// content and its emitter the host binary.
pub trait Writer: Send + Sync {
    /// Emits one complete line (the newline is added by the host).
    fn write_line(&self, line: &str);
}

/// Localizes stable keys with typed safe arguments.
///
/// Framework text uses `commander.*` keys and product prompts use
/// `cli.prompt.*`; missing keys are a host release error, and the returned
/// text must never embed secret values.
pub trait Localizer: Send + Sync {
    /// Resolves one key with its typed arguments to display text.
    fn t(&self, key: &str, args: &BTreeMap<String, String>) -> String;
}

/// The host ports one run executes against.
#[derive(Clone)]
pub struct Ports {
    /// The clock used for absolute prompt deadlines.
    pub clock: Arc<dyn Clock>,
    /// The stderr-style line sink for diagnostics and prompts.
    pub writer: Arc<dyn Writer>,
    /// The catalog-backed localizer.
    pub localizer: Arc<dyn Localizer>,
    /// The process-level cancellation token owned by the unique entry point.
    pub cancel: Arc<AtomicBool>,
    /// The resolved BCP-47 locale tag for this run.
    pub locale: String,
}

impl Ports {
    /// Builds ports from individual parts with an uncancelled token.
    pub fn new(
        clock: Arc<dyn Clock>,
        writer: Arc<dyn Writer>,
        localizer: Arc<dyn Localizer>,
        locale: impl Into<String>,
    ) -> Self {
        Self { clock, writer, localizer, cancel: Arc::new(AtomicBool::new(false)), locale: locale.into() }
    }
}
