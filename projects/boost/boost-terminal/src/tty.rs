//! TTY probes, echo control, and pollable single-character reads.
//!
//! Everything here sits behind the `std` feature and uses raw `extern`
//! declarations only: the crate keeps zero external dependencies. Unsupported
//! targets report structured capability failures instead of pretending the
//! operation succeeded.

use std::fmt;

/// Structured failure for terminal capability operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalError {
    /// Echo control is unavailable on this target or for this stream.
    ///
    /// Callers must surface this as a capability error and must never fall
    /// back to an echoed read when a secret is requested.
    EchoUnavailable,
}

impl fmt::Display for TerminalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EchoUnavailable => f.write_str("terminal echo control is unavailable"),
        }
    }
}

impl std::error::Error for TerminalError {}

/// Outcome of one bounded single-character read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadEvent {
    /// One raw byte was read from standard input.
    Byte(u8),
    /// The input stream reached end of file or cannot be read.
    Eof,
    /// The bounded wait expired without any input becoming available.
    Timeout,
}

/// RAII guard holding a disabled-echo terminal state.
///
/// Dropping the guard restores the console mode or termios state that was
/// saved when the guard was created, on every path including panics that
/// unwind through the prompt.
pub struct EchoGuard {
    state: sys::SavedMode,
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        self.state.restore();
    }
}

/// Reports whether standard input is attached to a terminal.
pub fn is_tty_input() -> bool {
    sys::is_tty_input()
}

/// Reports whether the interactive output channel is attached to a terminal.
///
/// Interactive prompt output is written to standard error, so this probe
/// checks the standard error handle, not standard output. Redirecting
/// standard output for data or JSON never by itself disables prompting.
pub fn is_tty_output() -> bool {
    sys::is_tty_output()
}

/// Saves the current input state and disables local echo until the returned
/// guard is dropped.
///
/// Returns [`TerminalError::EchoUnavailable`] when the target or stream does
/// not support echo control; callers must map that to a capability error.
pub fn disable_echo() -> Result<EchoGuard, TerminalError> {
    Ok(EchoGuard { state: sys::disable_echo()? })
}

/// Reads at most one byte from standard input, waiting up to `timeout_ms`.
///
/// The call is bounded: `0` polls without blocking. A [`ReadEvent::Timeout`]
/// means no byte became available within the bound; callers loop with their
/// own cancellation checks between events.
pub fn read_char_timeout(timeout_ms: u64) -> ReadEvent {
    sys::read_char_timeout(timeout_ms)
}

#[cfg(windows)]
#[allow(clippy::upper_case_acronyms)]
mod sys {
    use super::{ReadEvent, TerminalError};
    use std::os::raw::c_void;

    type DWORD = u32;
    type BOOL = i32;
    type HANDLE = *mut c_void;

    const STD_INPUT_HANDLE: DWORD = 0xFFFFFFF6;
    const STD_ERROR_HANDLE: DWORD = 0xFFFFFFF4;
    const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;
    const ENABLE_LINE_INPUT: DWORD = 0x0002;
    const ENABLE_ECHO_INPUT: DWORD = 0x0004;
    const WAIT_OBJECT_0: DWORD = 0;
    const WAIT_TIMEOUT: DWORD = 0x0000_0102;
    const FALSE: BOOL = 0;
    const INFINITE: DWORD = 0xFFFF_FFFF;

    unsafe extern "system" {
        fn GetStdHandle(nStdHandle: DWORD) -> HANDLE;
        fn GetConsoleMode(hConsoleHandle: HANDLE, lpMode: *mut DWORD) -> BOOL;
        fn SetConsoleMode(hConsoleHandle: HANDLE, dwMode: DWORD) -> BOOL;
        fn WaitForSingleObject(hHandle: HANDLE, dwMilliseconds: DWORD) -> DWORD;
        fn ReadFile(
            hFile: HANDLE,
            lpBuffer: *mut u8,
            nNumberOfBytesToRead: DWORD,
            lpNumberOfBytesRead: *mut DWORD,
            lpOverlapped: *mut c_void,
        ) -> BOOL;
    }

    fn std_handle(handle_num: DWORD) -> Option<HANDLE> {
        let handle = unsafe { GetStdHandle(handle_num) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE { None } else { Some(handle) }
    }

    fn console_mode(handle: HANDLE) -> Option<DWORD> {
        let mut mode: DWORD = 0;
        if unsafe { GetConsoleMode(handle, &mut mode) } == FALSE { None } else { Some(mode) }
    }

    pub struct SavedMode {
        handle: HANDLE,
        mode: DWORD,
    }

    impl SavedMode {
        pub(super) fn restore(&self) {
            unsafe {
                SetConsoleMode(self.handle, self.mode);
            }
        }
    }

    pub fn is_tty_input() -> bool {
        std_handle(STD_INPUT_HANDLE).and_then(console_mode).is_some()
    }

    pub fn is_tty_output() -> bool {
        std_handle(STD_ERROR_HANDLE).and_then(console_mode).is_some()
    }

    pub fn disable_echo() -> Result<SavedMode, TerminalError> {
        let handle = std_handle(STD_INPUT_HANDLE).ok_or(TerminalError::EchoUnavailable)?;
        let mode = console_mode(handle).ok_or(TerminalError::EchoUnavailable)?;
        // Clearing echo plus line input yields per-character reads without
        // local echoing; the saved mode is restored exactly on drop.
        let next = mode & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT);
        if unsafe { SetConsoleMode(handle, next) } == FALSE {
            return Err(TerminalError::EchoUnavailable);
        }
        Ok(SavedMode { handle, mode })
    }

    pub fn read_char_timeout(timeout_ms: u64) -> ReadEvent {
        let handle = match std_handle(STD_INPUT_HANDLE) {
            Some(handle) => handle,
            None => return ReadEvent::Eof,
        };
        let wait = timeout_ms.min((INFINITE - 1) as u64) as DWORD;
        match unsafe { WaitForSingleObject(handle, wait) } {
            WAIT_TIMEOUT => ReadEvent::Timeout,
            WAIT_OBJECT_0 => {
                let mut byte = 0u8;
                let mut read: DWORD = 0;
                let ok = unsafe { ReadFile(handle, &mut byte, 1, &mut read, std::ptr::null_mut()) };
                if ok != FALSE && read == 1 { ReadEvent::Byte(byte) } else { ReadEvent::Eof }
            }
            _ => ReadEvent::Eof,
        }
    }
}

#[cfg(all(unix, any(target_os = "linux", target_os = "macos")))]
mod sys {
    use super::{ReadEvent, TerminalError};

    type c_int = i32;

    // `nfds_t` is `unsigned long` on glibc and `unsigned int` on macOS.
    #[cfg(target_os = "linux")]
    type Nfds = usize;
    #[cfg(target_os = "macos")]
    type Nfds = u32;

    #[repr(C)]
    #[cfg(target_os = "linux")]
    struct Termios {
        c_iflag: u32,
        c_oflag: u32,
        c_cflag: u32,
        c_lflag: u32,
        c_line: u8,
        c_cc: [u8; 32],
        c_ispeed: u32,
        c_ospeed: u32,
    }

    #[repr(C)]
    #[cfg(target_os = "macos")]
    struct Termios {
        c_iflag: u64,
        c_oflag: u64,
        c_cflag: u64,
        c_lflag: u64,
        c_cc: [u8; 20],
    }

    #[cfg(target_os = "linux")]
    const ECHO: u32 = 0x0000_0008;
    #[cfg(target_os = "macos")]
    const ECHO: u64 = 0x0000_8000;
    const STDIN_FD: c_int = 0;
    const STDERR_FD: c_int = 2;
    const TCSADRAIN: c_int = 1;
    const POLLIN: i16 = 0x0001;
    const TRUE: c_int = 1;
    const OK: c_int = 0;

    #[repr(C)]
    struct PollFd {
        fd: c_int,
        events: i16,
        revents: i16,
    }

    unsafe extern "C" {
        fn isatty(fd: c_int) -> c_int;
        fn read(fd: c_int, buf: *mut u8, count: usize) -> isize;
        fn tcgetattr(fd: c_int, termios_p: *mut Termios) -> c_int;
        fn tcsetattr(fd: c_int, optional_actions: c_int, termios_p: *const Termios) -> c_int;
        fn poll(fds: *mut PollFd, nfds: Nfds, timeout: c_int) -> c_int;
    }

    pub struct SavedMode {
        termios: Termios,
    }

    impl SavedMode {
        pub(super) fn restore(&self) {
            unsafe {
                tcsetattr(STDIN_FD, TCSADRAIN, &self.termios);
            }
        }
    }

    pub fn is_tty_input() -> bool {
        unsafe { isatty(STDIN_FD) == TRUE }
    }

    pub fn is_tty_output() -> bool {
        unsafe { isatty(STDERR_FD) == TRUE }
    }

    pub fn disable_echo() -> Result<SavedMode, TerminalError> {
        let mut saved = core::mem::zeroed::<Termios>();
        if unsafe { tcgetattr(STDIN_FD, &mut saved) } != OK {
            return Err(TerminalError::EchoUnavailable);
        }
        let mut disabled = saved;
        disabled.c_lflag &= !ECHO;
        if unsafe { tcsetattr(STDIN_FD, TCSADRAIN, &disabled) } != OK {
            return Err(TerminalError::EchoUnavailable);
        }
        Ok(SavedMode { termios: saved })
    }

    pub fn read_char_timeout(timeout_ms: u64) -> ReadEvent {
        let timeout = timeout_ms.min(i32::MAX as u64 - 1) as c_int;
        let mut pfd = PollFd { fd: STDIN_FD, events: POLLIN, revents: 0 };
        let ready = unsafe { poll(&mut pfd, 1, timeout) };
        if ready == 0 {
            return ReadEvent::Timeout;
        }
        if ready < 0 {
            return ReadEvent::Eof;
        }
        let mut byte = 0u8;
        match unsafe { read(STDIN_FD, &mut byte, 1) } {
            1 => ReadEvent::Byte(byte),
            0 => ReadEvent::Eof,
            _ => ReadEvent::Eof,
        }
    }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
mod sys {
    use super::{ReadEvent, TerminalError};

    type c_int = i32;

    unsafe extern "C" {
        fn isatty(fd: c_int) -> c_int;
    }

    pub struct SavedMode;

    impl SavedMode {
        pub(super) fn restore(&self) {}
    }

    pub fn is_tty_input() -> bool {
        unsafe { isatty(0) == 1 }
    }

    pub fn is_tty_output() -> bool {
        unsafe { isatty(2) == 1 }
    }

    pub fn disable_echo() -> Result<SavedMode, TerminalError> {
        Err(TerminalError::EchoUnavailable)
    }

    pub fn read_char_timeout(_timeout_ms: u64) -> ReadEvent {
        ReadEvent::Eof
    }
}

#[cfg(not(any(windows, unix)))]
mod sys {
    use super::{ReadEvent, TerminalError};

    pub struct SavedMode;

    impl SavedMode {
        pub(super) fn restore(&self) {}
    }

    pub fn is_tty_input() -> bool {
        false
    }

    pub fn is_tty_output() -> bool {
        false
    }

    pub fn disable_echo() -> Result<SavedMode, TerminalError> {
        Err(TerminalError::EchoUnavailable)
    }

    pub fn read_char_timeout(_timeout_ms: u64) -> ReadEvent {
        ReadEvent::Eof
    }
}
