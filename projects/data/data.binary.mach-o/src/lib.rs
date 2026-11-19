//! `data-binary-mach-o` — Apple `Mach-O` binary format for the `data.binary` family.
//!
//! Builds a minimal 64-bit `Mach-O` (`aarch64` `MH_EXECUTE`) image, including the
//! `LC_SEGMENT_64 __TEXT` load command and `LC_MAIN`. It only owns the byte
//! structure; the `__TEXT` code bytes are supplied by an assembler layer and
//! substituted here. No language semantics are carried.
//!
//! The code originates from the former `acorn.rs` project (imported at commit
//! `6deb56a4`, MPL-2.0) and has been internalized and de-branded for Boost.

#![warn(missing_docs)]

/// 本 crate 的错误类型与 `Result` 别名。
pub mod error;
/// Apple `Mach-O` 64 位可执行镜像构建。
pub mod mach_o;

macro_rules! err {
    ($($arg:tt)*) => {
        $crate::error::Error::new(format!($($arg)*))
    };
}

pub(crate) use err;
