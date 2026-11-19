//! Real terminal adapter over `boost-terminal`.

use std::sync::{Arc, Mutex};

use boost_terminal::{EchoGuard, ReadEvent, disable_echo, is_tty_input, read_char_timeout};
use commander_core::{Ports, PromptApi, PromptFactory, PromptHooks};

use crate::engine::{EchoRestore, EngineDeps, IoEvent, PromptEngine, PromptIo};

/// [`PromptIo`] backed by the process terminal through `boost-terminal`.
///
/// Echo control is only claimed when standard input is attached to a
/// terminal; otherwise `password` fails as `commander::password_unsupported`
/// and never degrades to visible input.
pub struct TerminalPromptIo;

impl PromptIo for TerminalPromptIo {
    fn read_char(&mut self, timeout_ms: u64) -> IoEvent {
        match read_char_timeout(timeout_ms) {
            ReadEvent::Byte(byte) => IoEvent::Byte(byte),
            ReadEvent::Eof => IoEvent::Eof,
            ReadEvent::Timeout => IoEvent::Timeout,
        }
    }

    fn supports_echo_control(&self) -> bool {
        is_tty_input()
    }

    fn disable_echo(&mut self) -> Option<Box<dyn EchoRestore>> {
        disable_echo().ok().map(|guard| Box::new(EchoRestoreAdapter { _guard: guard }) as Box<dyn EchoRestore>)
    }
}

/// Adapts the platform echo guard to the engine's RAII trait.
///
/// The guard only touches the console handle from the thread that owns the
/// active prompt (the engine serializes the stream behind a mutex), so
/// crossing the `Box<dyn EchoRestore>: Send` bound is sound here.
struct EchoRestoreAdapter {
    _guard: EchoGuard,
}

// SAFETY: see the type comment; the guard is created and dropped inside one
// locked prompt operation on a single thread.
unsafe impl Send for EchoRestoreAdapter {}

impl EchoRestore for EchoRestoreAdapter {}

/// Builds [`PromptEngine`] instances bound to the process terminal.
pub struct TerminalFactory {
    deps: EngineDeps,
}

impl TerminalFactory {
    /// Derives the shared engine dependencies from the run's ports.
    pub fn from_ports(ports: &Ports) -> Self {
        Self {
            deps: EngineDeps {
                io: Arc::new(Mutex::new(TerminalPromptIo)),
                writer: ports.writer.clone(),
                localizer: ports.localizer.clone(),
                clock: ports.clock.clone(),
                cancel: ports.cancel.clone(),
            },
        }
    }
}

impl PromptFactory for TerminalFactory {
    fn create(&self, hooks: PromptHooks) -> Arc<dyn PromptApi> {
        Arc::new(PromptEngine::new(self.deps.clone(), hooks))
    }
}
