//! `data-binary-dex` — Android `DEX` (`dex/035`) binary format.
//!
//! `dex::writer` encodes JVM `.class` bytes into a parseable `classes.dex` with
//! optional trailing sections; `dex::reader` provides minimal `string_ids` /
//! `type_ids` / `class_defs` reads and string scanning; `dex::merge` combines
//! multiple class sets and images. Only the `DEX` byte structure is owned here;
//! JVM `.class` encoding lives in `data-binary-jvm`, and trailing sections are
//! opaque bytes parsed by the assembly layer.
//!
//! The code originates from the former `acorn.rs` project (imported at commit
//! `6deb56a4`, MPL-2.0) and has been internalized and de-branded for Boost.

#![warn(missing_docs)]

/// Android `DEX`（`dex/035`）读写与合并。
pub mod dex;
/// 本 crate 的错误类型与 `Result` 别名。
pub mod error;

macro_rules! err {
    ($($arg:tt)*) => {
        $crate::error::Error::new(format!($($arg)*))
    };
}

pub(crate) use err;
