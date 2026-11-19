//! `data-binary-host` — minimal host executable wrappers per platform.
//!
//! Dispatches `(platform, native_logic, rodata_tail)` onto the container
//! encoders owned by `data-binary-x64` (`PE32+` / `ELF64`) and
//! `data-binary-mach-o` (`Mach-O`). Only the dispatch and the minimal entry
//! stub live here: container layout is never re-implemented, and no linking,
//! loading or execution is performed.
//!
//! The code originates from the former `acorn.rs` project (imported at commit
//! `beccbdf7`, MPL-2.0) and has been internalized and de-branded for Boost.

#![warn(missing_docs)]

/// Errors and the crate `Result` alias.
pub mod error;
/// Minimal host executable wrappers per platform.
pub mod host_exe;

macro_rules! err {
    ($($arg:tt)*) => {
        $crate::error::Error::new(format!($($arg)*))
    };
}

pub(crate) use err;
