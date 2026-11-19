#![doc = include_str!("readme.md")]
#![deny(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "std")]
mod platform;
#[cfg(feature = "std")]
mod tty;

#[cfg(feature = "std")]
pub use tty::{EchoGuard, ReadEvent, TerminalError, disable_echo, is_tty_input, is_tty_output, read_char_timeout};

/// Enables virtual terminal escape sequences on Windows consoles when possible.
///
/// Returns `true` when escape sequence support was successfully enabled and
/// `false` otherwise. Without the `std` feature the crate assumes ANSI support
/// and returns `true` without touching the platform.
pub fn enable_virtual_terminal() -> bool {
    #[cfg(feature = "std")]
    {
        platform::enable_virtual_terminal()
    }
    #[cfg(not(feature = "std"))]
    {
        true
    }
}
